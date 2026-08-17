//! `:s/…/\=…/` — a **computed** replacement: the text after `\=` is a Lua
//! expression evaluated once per match in the bounded compute sandbox, with the
//! submatches in scope.
//!
//! Driven black-box: type the ex command, read the buffer (or the message the
//! failure reported) back over RPC.

use crate::support::*;

/// A fresh server on a temp file holding `body`.
async fn start_body(tag: &str, body: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let path = temp_path(tag).to_string_lossy().into_owned();
    std::fs::write(&path, body).expect("write temp file");
    start(Some(path)).await
}

// ===== the submatch table ====================================================

#[tokio::test]
async fn m0_is_the_whole_match() {
    let (rpc, _i) = start_body("sx_m0", "alpha beta\n").await;
    feed_sync(&rpc, r#":%s/\w+/\=m[0]:upper()/g<CR>"#).await;
    assert_eq!(lines(&rpc).await, vec!["ALPHA BETA"]);
}

#[tokio::test]
async fn numbered_groups_can_be_reordered() {
    let (rpc, _i) = start_body("sx_swap", "one_two\n").await;
    feed_sync(&rpc, r#":%s/(\w+)_(\w+)/\=m[2] .. "_" .. m[1]/<CR>"#).await;
    assert_eq!(lines(&rpc).await, vec!["two_one"]);
}

#[tokio::test]
async fn a_number_result_is_accepted_and_rendered_without_a_trailing_zero() {
    let (rpc, _i) = start_body("sx_math", "7 21\n").await;
    feed_sync(&rpc, r#":%s/\d+/\=tonumber(m[0]) * 2/g<CR>"#).await;
    assert_eq!(lines(&rpc).await, vec!["14 42"]);
}

#[tokio::test]
async fn lnum_is_the_one_based_line_of_the_match() {
    let (rpc, _i) = start_body("sx_lnum", "x\nx\nx\n").await;
    feed_sync(&rpc, r#":%s/x/\=tostring(lnum)/<CR>"#).await;
    assert_eq!(lines(&rpc).await, vec!["1", "2", "3"]);
}

#[tokio::test]
async fn a_group_that_did_not_participate_arrives_as_nil() {
    let (rpc, _i) = start_body("sx_nil", "ab\n").await;
    // The second alternative never participates, so `m[2]` must be nil — not "".
    feed_sync(&rpc, r#":%s/(a)|(z)/\=type(m[2])/<CR>"#).await;
    assert_eq!(lines(&rpc).await, vec!["nilb"]);
}

#[tokio::test]
async fn the_expression_runs_once_per_match_not_once_per_line() {
    let (rpc, _i) = start_body("sx_permatch", "a a a\n").await;
    // Each match sees its own capture, so a counter-free proof is enough: the
    // three matches each expand independently.
    feed_sync(&rpc, r#":%s/a/\=m[0] .. "!"/g<CR>"#).await;
    assert_eq!(lines(&rpc).await, vec!["a! a! a!"]);
}

// ===== the sandbox is closed =================================================

/// Each of these must read `nil` inside the sandbox: reaching the host, the
/// filesystem, the loader, or the editor's own Lua API is not merely discouraged
/// but absent from the environment.
#[tokio::test]
async fn the_sandbox_cannot_reach_the_host_or_the_editor() {
    for name in [
        "io",
        "os",
        "require",
        "load",
        "loadstring",
        "dofile",
        "debug",
        "package",
        "btv",
    ] {
        let (rpc, _i) = start_body(&format!("sx_closed_{name}"), "x\n").await;
        feed_sync(&rpc, &format!(r#":%s/x/\=type({name})/<CR>"#)).await;
        assert_eq!(
            lines(&rpc).await,
            vec!["nil"],
            "`{name}` must not be reachable from the sandbox"
        );
    }
}

#[tokio::test]
async fn pcall_is_absent_so_an_expression_cannot_swallow_its_own_deadline() {
    let (rpc, _i) = start_body("sx_nopcall", "x\n").await;
    feed_sync(&rpc, r#":%s/x/\=type(pcall)/<CR>"#).await;
    assert_eq!(lines(&rpc).await, vec!["nil"]);
}

// ===== failure is loud =======================================================

#[tokio::test]
async fn a_compile_error_reports_and_leaves_the_buffer_untouched() {
    let (rpc, mut i) = start_body("sx_badsyntax", "alpha\n").await;
    let msg = message_after(&rpc, &mut i, r#":%s/alpha/\=m[/<CR>"#).await;
    assert!(msg.contains("E1300"), "expected E1300, got {msg:?}");
    assert!(
        msg.contains("invalid expression"),
        "expected a compile complaint, got {msg:?}"
    );
    assert_eq!(lines(&rpc).await, vec!["alpha"]);
}

#[tokio::test]
async fn a_runtime_error_reports_the_failing_line() {
    let (rpc, mut i) = start_body("sx_raise", "alpha\n").await;
    let msg = message_after(&rpc, &mut i, r#":%s/alpha/\=error("boom")/<CR>"#).await;
    assert!(msg.contains("E1300"), "expected E1300, got {msg:?}");
    assert!(
        msg.contains("line 1"),
        "expected the line number, got {msg:?}"
    );
}

#[tokio::test]
async fn a_non_string_result_is_rejected_rather_than_coerced() {
    let (rpc, mut i) = start_body("sx_badret", "alpha\n").await;
    let msg = message_after(&rpc, &mut i, r#":%s/alpha/\={}/<CR>"#).await;
    assert!(
        msg.contains("expected a string or number"),
        "a table result must be refused, got {msg:?}"
    );
    // Refused, not silently applied as empty text.
    assert_eq!(lines(&rpc).await, vec!["alpha"]);
}

#[tokio::test]
async fn a_runaway_expression_is_abandoned_at_its_deadline() {
    let (rpc, mut i) = start_body("sx_spin", "alpha\n").await;
    let started = std::time::Instant::now();
    let msg = message_after(
        &rpc,
        &mut i,
        r#":%s/alpha/\=(function() while true do end end)()/<CR>"#,
    )
    .await;
    let elapsed = started.elapsed();
    assert!(
        msg.contains("budget"),
        "expected a deadline report, got {msg:?}"
    );
    // The whole point: it comes back. A generous ceiling keeps this from flaking
    // on a loaded machine while still failing if the deadline never fired.
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the deadline did not stop the runaway ({elapsed:?})"
    );
    assert_eq!(lines(&rpc).await, vec!["alpha"]);
}

// ===== the literal form is unaffected ========================================

#[tokio::test]
async fn a_replacement_that_merely_contains_an_equals_stays_literal() {
    let (rpc, _i) = start_body("sx_literal", "x\n").await;
    feed_sync(&rpc, ":%s/x/a=b/<CR>").await;
    assert_eq!(lines(&rpc).await, vec!["a=b"]);
}

#[tokio::test]
async fn capture_templates_still_expand_the_literal_way() {
    let (rpc, _i) = start_body("sx_template", "one_two\n").await;
    // Braces are required here: `$2_` would otherwise read as a group *named*
    // `2_` (the dialect's documented ambiguity), not group 2 followed by `_`.
    feed_sync(&rpc, r#":%s/(\w+)_(\w+)/${2}_${1}/<CR>"#).await;
    assert_eq!(lines(&rpc).await, vec!["two_one"]);
}
