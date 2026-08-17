//! The compute sandbox is **stateless**, and enforced to be.
//!
//! Nothing an expression does may carry from one call to the next, or from one
//! expression to another. That is not fussiness: no call shape here is a clean
//! once-per-item traversal — `:s` re-runs the expression on every keystroke of
//! the live preview, a foldexpr sees only the rows an edit touched, the picker
//! scorer sees only the top survivors, and `foldtext` is memoized so calls are
//! skipped — so an accumulator is quietly wrong in all of them.

use crate::support::*;

/// Run one `:s` whose replacement is `expr`, and return the resulting line.
async fn subst_line(tag: &str, body: &str, expr: &str) -> String {
    let path = temp_path(tag).to_string_lossy().into_owned();
    std::fs::write(&path, body).expect("write");
    let (rpc, _i) = start(Some(path)).await;
    feed_sync(&rpc, &format!(r#":%s/x/\={expr}/g<CR>"#)).await;
    lines(&rpc).await.first().cloned().unwrap_or_default()
}

#[tokio::test]
async fn assigning_a_global_is_refused() {
    // The counter idiom, which used to work and produced nonsense (it counted the
    // live preview's evaluations, not the substitutions).
    let (rpc, mut i) = {
        let path = temp_path("ss_assign").to_string_lossy().into_owned();
        std::fs::write(&path, "x x x\n").expect("write");
        start(Some(path)).await
    };
    let msg = message_after(
        &rpc,
        &mut i,
        r#":%s/x/\=(function() c = (c or 0) + 1 return tostring(c) end)()/g<CR>"#,
    )
    .await;
    assert!(
        msg.contains("stateless"),
        "assigning a global must be refused loudly, got {msg:?}"
    );
    // Refused, not half-applied.
    assert_eq!(lines(&rpc).await, vec!["x x x"]);
}

#[tokio::test]
async fn rawset_is_absent_so_the_guard_cannot_be_bypassed() {
    // `rawset` writes past metatables; if it were exposed the read-only
    // environment would be decorative.
    let got = subst_line("ss_rawset", "x\n", "type(rawset)").await;
    assert_eq!(got, "nil");
}

#[tokio::test]
async fn the_environment_metatable_is_hidden() {
    let got = subst_line("ss_meta", "x\n", "type(getmetatable)").await;
    assert_eq!(
        got, "nil",
        "no getmetatable/setmetatable to swap the guard out"
    );
}

#[tokio::test]
async fn a_stdlib_table_cannot_be_mutated() {
    // The libraries are shared by every compiled chunk, so a writable `string`
    // would be a channel between unrelated expressions.
    let (rpc, mut i) = {
        let path = temp_path("ss_lib").to_string_lossy().into_owned();
        std::fs::write(&path, "x\n").expect("write");
        start(Some(path)).await
    };
    let msg = message_after(
        &rpc,
        &mut i,
        r#":%s/x/\=(function() string.smuggle = 1 return "ok" end)()/<CR>"#,
    )
    .await;
    assert!(
        msg.contains("stateless") || msg.contains("expression failed"),
        "mutating a stdlib table must fail, got {msg:?}"
    );
}

#[tokio::test]
async fn nothing_leaks_between_two_expressions() {
    let path = temp_path("ss_leak").to_string_lossy().into_owned();
    std::fs::write(&path, "a\nb\n").expect("write");
    let (rpc, _i) = start(Some(path)).await;
    // The first `:s` really does try to stash a value, so this fails on *both*
    // counts if the environment were writable: line 1 would have been rewritten,
    // and line 2 would read `42` instead of `nil`.
    feed_sync(
        &rpc,
        r#":1s/a/\=(function() smuggled = 42 return "wrote" end)()/<CR>"#,
    )
    .await;
    feed_sync(&rpc, r#":2s/b/\=tostring(smuggled)/<CR>"#).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["a", "nil"],
        "the write must be refused (line 1 untouched) and invisible (line 2 nil)"
    );
}

#[tokio::test]
async fn reads_of_the_allowed_stdlib_still_work() {
    // The guard must not break ordinary use: reads fall through to the allow-list.
    let got = subst_line(
        "ss_reads",
        "x\n",
        r#"string.upper("ok") .. tostring(math.max(1, 2))"#,
    )
    .await;
    assert_eq!(got, "OK2");
}
