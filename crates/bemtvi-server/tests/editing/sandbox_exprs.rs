//! `btv.filetype.detect` and `btv.indent.expr` — the two sandbox expressions that
//! answer questions the core deliberately refuses to guess at.
//!
//! Black-box: source an `init.lua`, open/edit a buffer, read the resolved
//! filetype or the resulting indentation back over RPC.

use crate::support::*;

/// Feed `keys` and return the first message any resulting frame carries.
///
/// These errors are echoed on the frame the command produced, and a later
/// barrier repaint clears the message line again — so taking the *latest*
/// redraw loses it. Poll for the latest frame that still carries one.
async fn message_from_any_frame(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> String {
    feed(rpc, keys);
    for _ in 0..50 {
        if let Some(m) = drain_to_latest_redraw(incoming, |m| !message(m).is_empty()) {
            return message(&m);
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    String::new()
}

// ===== content-based filetype ===============================================

/// Write `body` to a temp file named `name`, open it under `init_lua`, and return
/// the filetype the editor settled on.
async fn filetype_of(tag: &str, name: &str, body: &str, init_lua: &str) -> String {
    let dir = temp_dir(tag);
    let file = dir.join(name);
    std::fs::write(&file, body).expect("write fixture");
    let (rpc, _incoming) = start_with_config(&dir, init_lua).await;
    feed_sync(&rpc, &format!(":e {}<CR>", file.to_string_lossy())).await;
    // The sniffer settles on the repaint after the buffer exists.
    let _ = lines(&rpc).await;
    exec_lua(&rpc, "return btv.bo.filetype")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn a_h_file_stays_c_without_a_sniffer() {
    let ft = filetype_of("ft_h_plain", "x.h", "int main(void);\n", "").await;
    assert_eq!(ft, "c", "the built-in table resolves .h to c");
}

#[tokio::test]
async fn the_sniffer_can_call_a_h_file_cpp_from_its_content() {
    // Exactly the case `mod.rs` documents as "omitted rather than guessed".
    let ft = filetype_of(
        "ft_h_cpp",
        "x.h",
        "template <typename T> struct S {};\n",
        r#"btv.filetype.detect([[ ext == "h" and head:find("template", 1, true) and "cpp" or nil ]])"#,
    )
    .await;
    assert_eq!(ft, "cpp", "content should win over the extension table");
}

#[tokio::test]
async fn declining_leaves_the_builtin_answer_alone() {
    let ft = filetype_of(
        "ft_decline",
        "x.h",
        "int main(void);\n",
        r#"btv.filetype.detect([[ ext == "h" and head:find("template", 1, true) and "cpp" or nil ]])"#,
    )
    .await;
    assert_eq!(ft, "c", "a nil verdict must not clobber the built-in");
}

#[tokio::test]
async fn the_sniffer_sees_the_name_and_the_head() {
    let ft = filetype_of(
        "ft_names",
        "weird.xyz",
        "#!/usr/bin/env fish\n",
        r#"btv.filetype.detect([[ (name == "weird.xyz" and ext == "xyz" and head:find("fish", 1, true)) and "fish" or nil ]])"#,
    )
    .await;
    assert_eq!(ft, "fish");
}

#[tokio::test]
async fn a_failing_sniffer_reports_and_leaves_the_buffer_typed_normally() {
    let dir = temp_dir("ft_raise");
    let file = dir.join("x.h");
    std::fs::write(&file, "int main(void);\n").expect("write");
    let (rpc, mut incoming) =
        start_with_config(&dir, r#"btv.filetype.detect([[ error("boom") ]])"#).await;
    let msg = message_from_any_frame(
        &rpc,
        &mut incoming,
        &format!(":e {}<CR>", file.to_string_lossy()),
    )
    .await;
    assert!(
        msg.contains("btv.filetype.detect"),
        "expected a report, got {msg:?}"
    );
    let ft = exec_lua(&rpc, "return btv.bo.filetype").await;
    assert_eq!(
        ft.as_str().unwrap_or_default(),
        "c",
        "fell back to the table"
    );
}

#[tokio::test]
async fn a_sniffer_closure_is_rejected_at_the_lua_boundary() {
    let dir = temp_dir("ft_badarg");
    let (rpc, _incoming) = start_with_config(&dir, "").await;
    let e = exec_lua(
        &rpc,
        "local ok, e = pcall(btv.filetype.detect, function() end) return tostring(e)",
    )
    .await;
    assert!(
        e.as_str()
            .unwrap_or_default()
            .contains("expected a string of Lua source"),
        "got {e:?}"
    );
}

// ===== indentexpr ============================================================

/// Type `body` into a fresh buffer under `init_lua`, then `=` the whole thing.
async fn reindented(tag: &str, body: &str, init_lua: &str) -> Vec<String> {
    let dir = temp_dir(tag);
    let (rpc, _incoming) = start_with_config(&dir, init_lua).await;
    feed_sync(&rpc, &format!("i{body}<Esc>")).await;
    feed_sync(&rpc, "gg=G").await;
    lines(&rpc).await
}

#[tokio::test]
async fn an_indentexpr_sets_each_lines_indent() {
    // A tiny then/end rule, on a filetype with no grammar at all.
    let lines = reindented(
        "ix_basic",
        "if x then<CR>body<CR>end",
        r#"btv.o.expandtab = true
           btv.indent.expr([[
             line:match("^%s*end") and previndent - sw
               or prev:match("then%s*$") and previndent + sw
               or previndent
           ]])"#,
    )
    .await;
    assert_eq!(lines, vec!["if x then", "    body", "end"]);
}

#[tokio::test]
async fn declining_falls_through_to_the_builtin_indent() {
    // Always-nil: `=` must behave exactly as with no expression installed.
    let with = reindented("ix_nil", "a<CR>b", "btv.indent.expr([[ nil ]])").await;
    let without = reindented("ix_none", "a<CR>b", "").await;
    assert_eq!(with, without);
}

#[tokio::test]
async fn a_failing_indentexpr_reports_once_and_uninstalls() {
    let dir = temp_dir("ix_raise");
    let (rpc, mut incoming) =
        start_with_config(&dir, r#"btv.indent.expr([[ error("boom") ]])"#).await;
    feed_sync(&rpc, "ia<CR>b<CR>c<Esc>").await;
    let msg = message_from_any_frame(&rpc, &mut incoming, "gg=G").await;
    assert!(
        msg.contains("btv.indent.expr"),
        "expected a report, got {msg:?}"
    );
    // Uninstalled rather than repeating per line; the buffer survives.
    assert_eq!(lines(&rpc).await, vec!["a", "b", "c"]);
}

#[tokio::test]
async fn an_indentexpr_compile_error_is_reported_where_it_is_configured() {
    let dir = temp_dir("ix_badsyntax");
    let (rpc, mut incoming) = start_with_config(&dir, "btv.indent.expr([[ previndent + ]])").await;
    let msg = message(&redraw_after(&rpc, &mut incoming, "").await);
    assert!(
        msg.contains("btv.indent.expr") && msg.contains("invalid expression"),
        "got {msg:?}"
    );
}
