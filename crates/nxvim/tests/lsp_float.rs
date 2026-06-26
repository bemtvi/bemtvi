//! Behavior tests for LSP **hover** and **signature help** floats.
//!
//! Both hover and signature help render through a **doc float** — a real,
//! non-focusable float *window* over a scratch buffer ([`Editor::open_doc_float`]),
//! so they scroll with the mouse wheel; each appears in the redraw as a `windows[]`
//! entry with `floating == true`.
//!
//! Wired exactly like `lsp_complete.rs`: the scripted mock language server
//! (`nxvim --__lsp-mock`, `nxvim_lsp::mock`) answers `textDocument/hover` and
//! `textDocument/signatureHelp`, the `$NXVIM_LSP_CMD` env hook overrides the
//! server's spawn argv, and the buffer is bound via the raw `nx._lsp_start`
//! bridge. The process-global env means these tests serialize on `serial_lock`.

use std::path::Path;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, drain_to_latest_redraw, exec_lua, feed, feed_mouse, map_get, serial_lock, spawn,
    temp_dir,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const NXVIM_BIN: &str = env!("CARGO_BIN_EXE_nxvim");

/// Write a mock LSP script (the whole JSON object) and point `$NXVIM_LSP_CMD` at
/// the binary's `--__lsp-mock` mode. The caller holds `serial_lock`.
fn arm_mock(dir: &Path, script: &str) {
    std::fs::write(dir.join("mock.json"), script).expect("write mock script");
    // SAFETY: serialized on `serial_lock`, so no other test races this env mutation.
    std::env::set_var(
        "NXVIM_LSP_CMD",
        format!("{NXVIM_BIN} --__lsp-mock {}/mock.json", dir.display()),
    );
}

/// Open a `.rs` buffer with `foo` under the cursor, attach, and bind the mock
/// server. Returns the rpc + redraw stream; the caller drives hover / signature.
async fn start(dir: &Path) -> (Rpc, UnboundedReceiver<Incoming>) {
    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, "let foo = bar()\n").expect("write test file");
    let init = ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    // Put the cursor on `foo` (column 4) so a hover request has a symbol; it stays
    // there (no motion) so the reply's cursor-staleness gate passes.
    feed(&rpc, "0fw");
    exec_lua(
        &rpc,
        "nx._lsp_start('mock', { 'placeholder' }, vim.fn.getcwd(), 'rust', \
         vim.api.nvim_get_current_buf(), nil, nil, nil)",
    )
    .await;
    (rpc, incoming)
}

/// The first floating *window* (`windows[]` with `floating == true`) in a redraw —
/// the hover doc float — or `None`. The main editor window is `floating == false`.
fn floating_window(map: &[(Value, Value)]) -> Option<Vec<(Value, Value)>> {
    let windows = map_get(map, "windows")?.as_array()?;
    windows
        .iter()
        .filter_map(Value::as_map)
        .find(|w| map_get(w, "floating").and_then(Value::as_bool) == Some(true))
        .cloned()
}

/// A float window's rendered text rows (the redraw `lines` array — plain strings).
fn window_lines(win: &[(Value, Value)]) -> Vec<String> {
    match map_get(win, "lines") {
        Some(Value::Array(rows)) => rows
            .iter()
            .map(|r| r.as_str().unwrap_or_default().to_string())
            .collect(),
        _ => Vec::new(),
    }
}

/// A float window's `filetype` from the redraw (the buffer's effective ts type).
fn window_filetype(win: &[(Value, Value)]) -> &str {
    map_get(win, "filetype")
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// A float window's outer rect `(x, y, width, height)` from the redraw.
fn window_rect(win: &[(Value, Value)]) -> (usize, usize, usize, usize) {
    let rect = match map_get(win, "rect") {
        Some(Value::Map(m)) => m.clone(),
        other => panic!("expected a window rect, got {other:?}"),
    };
    let n = |k| map_get(&rect, k).and_then(Value::as_u64).unwrap_or(0) as usize;
    (n("x"), n("y"), n("width"), n("height"))
}

/// Retry the `trigger` Lua until a floating doc-float *window* (hover / signature)
/// appears with some line containing `want` (the async reply takes a moment to
/// land); returns its rows.
async fn await_doc_float_window(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    trigger: &str,
    want: &str,
) -> Vec<(Value, Value)> {
    for _ in 0..200 {
        exec_lua(rpc, trigger).await;
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| floating_window(m).is_some()) {
            let win = floating_window(&map).expect("a floating window");
            if window_lines(&win).iter().any(|l| l.contains(want)) {
                return win;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("the doc-float window never contained {want:?}");
}

#[tokio::test]
async fn hover_reply_opens_a_float_window() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_hover");
    arm_mock(
        &dir,
        r#"{ "hover": { "contents": { "kind": "markdown",
             "value": "`foo`: a scripted hover symbol" } } }"#,
    );
    let (rpc, mut incoming) = start(&dir).await;

    // The hover is a real float WINDOW now (so it can scroll), not the content-float
    // `float` surface — assert it appears in `windows[]` carrying the markup.
    let win = await_doc_float_window(&rpc, &mut incoming, "nx.lsp.hover()", "scripted hover").await;
    assert!(
        window_lines(&win).iter().any(|l| l.contains("foo")),
        "hover float window should carry the markup, got {:?}",
        window_lines(&win)
    );
    // LSP doc content is markdown, so the scratch buffer is typed `markdown` by
    // default (free tree-sitter highlighting).
    assert_eq!(
        window_filetype(&win),
        "markdown",
        "the hover doc-float buffer defaults to the markdown filetype"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

#[tokio::test]
async fn hover_window_scrolls_with_the_wheel_and_a_key_dismisses_it() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_hover_scroll");
    // A tall hover (more lines than the float's 20-row cap) so there is content to
    // scroll past — the whole reason a doc float is a window, not a content overlay.
    let body = (0..30)
        .map(|i| format!("hover line {i:02}"))
        .collect::<Vec<_>>()
        .join("\\n");
    arm_mock(
        &dir,
        &format!(r#"{{ "hover": {{ "contents": {{ "kind": "markdown", "value": "{body}" }} }} }}"#),
    );
    let (rpc, mut incoming) = start(&dir).await;

    // Open it and confirm the top of the content shows first.
    let win = await_doc_float_window(&rpc, &mut incoming, "nx.lsp.hover()", "hover line 00").await;
    assert_eq!(
        window_lines(&win).first().map(String::as_str),
        Some("hover line 00"),
        "the float opens at the top of the content"
    );
    let (x, y, _, _) = window_rect(&win);

    // A wheel-down notch over the float scrolls its content — the wheel flows
    // through the mouse path, NOT `input`, so it does not dismiss the popup.
    for _ in 0..3 {
        feed_mouse(&rpc, "wheel", "down", y + 1, x + 1);
    }
    nxvim_test_harness::barrier(&rpc).await;
    let scrolled = drain_to_latest_redraw(&mut incoming, |m| {
        floating_window(m)
            .map(|w| window_lines(&w).first() != Some(&"hover line 00".to_string()))
            .unwrap_or(false)
    })
    .and_then(|m| floating_window(&m))
    .expect("the wheel scrolled the hover float (still open)");
    let top = window_lines(&scrolled);
    assert_ne!(
        top.first().map(String::as_str),
        Some("hover line 00"),
        "wheel-down scrolled the content; top is no longer line 00 ({top:?})"
    );

    // The next KEY dismisses it (transient), like a content float.
    feed(&rpc, "j");
    nxvim_test_harness::barrier(&rpc).await;
    assert!(
        drain_to_latest_redraw(&mut incoming, |m| floating_window(m).is_none()).is_some(),
        "a key dismissed the hover float window"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

#[tokio::test]
async fn scrolling_the_text_dismisses_the_hover_float() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_hover_scroll_away");
    arm_mock(
        &dir,
        r#"{ "hover": { "contents": { "kind": "markdown",
             "value": "`foo`: a scripted hover symbol" } } }"#,
    );
    let (rpc, mut incoming) = start(&dir).await;
    let win = await_doc_float_window(&rpc, &mut incoming, "nx.lsp.hover()", "scripted hover").await;

    // A wheel over the TEXT (well away from the float) scrolls the view out from
    // under the hover, so it must close — not follow the cursor.
    let (_, fy, _, _) = window_rect(&win);
    feed_mouse(&rpc, "wheel", "down", fy + 6, 0);
    nxvim_test_harness::barrier(&rpc).await;
    assert!(
        drain_to_latest_redraw(&mut incoming, |m| floating_window(m).is_none()).is_some(),
        "scrolling the text dismissed the hover float"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

#[tokio::test]
async fn clicking_elsewhere_dismisses_the_hover_float() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_hover_click_away");
    arm_mock(
        &dir,
        r#"{ "hover": { "contents": { "kind": "markdown",
             "value": "`foo`: a scripted hover symbol" } } }"#,
    );
    let (rpc, mut incoming) = start(&dir).await;
    let win = await_doc_float_window(&rpc, &mut incoming, "nx.lsp.hover()", "scripted hover").await;

    // A click away from the float moves the cursor off the word, so the hover must
    // close instead of trailing the cursor to the click.
    let (_, fy, _, _) = window_rect(&win);
    feed_mouse(&rpc, "left", "press", fy + 6, 0);
    nxvim_test_harness::barrier(&rpc).await;
    assert!(
        drain_to_latest_redraw(&mut incoming, |m| floating_window(m).is_none()).is_some(),
        "clicking elsewhere dismissed the hover float"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

#[tokio::test]
async fn signature_help_reply_opens_a_float_window() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_sig");
    arm_mock(
        &dir,
        r#"{ "signature_help": { "signatures": [
             { "label": "fn foo(a: i32, b: i32)",
               "parameters": [ { "label": "a: i32" }, { "label": "b: i32" } ] } ],
             "activeSignature": 0, "activeParameter": 0 } }"#,
    );
    let (rpc, mut incoming) = start(&dir).await;

    // Signature help is the same scrollable doc-float WINDOW as the hover.
    let win = await_doc_float_window(
        &rpc,
        &mut incoming,
        "nx.lsp.signature_help()",
        "fn foo(a: i32, b: i32)",
    )
    .await;
    // The active parameter is appended in brackets (the float renders plain lines).
    assert!(
        window_lines(&win).iter().any(|l| l.contains("[a: i32]")),
        "signature float window should mark the active parameter, got {:?}",
        window_lines(&win)
    );
    // Signature help shows a code signature, so the popup inherits the invoking
    // buffer's filetype (the mock opened an `.rs` buffer → `rust`), not `markdown`.
    assert_eq!(
        window_filetype(&win),
        "rust",
        "the signature doc-float buffer inherits the invoking buffer's filetype"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

// ── Signature-help auto-trigger (opt-in, server-advertised trigger chars) ──────
//
// The mock advertises `signatureHelpProvider.triggerCharacters = ["(", ","]`, so an
// opted-in editor fires `textDocument/signatureHelp` when you type `(` while editing.
// The float is *sticky* across the keystrokes that fill the call (unlike the
// next-key-transient hover) and closes when you leave insert mode.

/// A signature-help reply script the mock answers any `signatureHelp` request with.
const SIG_SCRIPT: &str = r#"{ "signature_help": { "signatures": [
     { "label": "fn foo(a: i32, b: i32)",
       "parameters": [ { "label": "a: i32" }, { "label": "b: i32" } ] } ],
     "activeSignature": 0, "activeParameter": 0 } }"#;

/// Poll until the LSP client has finished `initialize` (mirrored into
/// `nx.lsp._clients`), so the server's advertised trigger chars have reached core.
async fn wait_for_lsp(rpc: &Rpc) {
    for _ in 0..200 {
        if exec_lua(rpc, "return next(nx.lsp._clients) ~= nil").await == Value::Boolean(true) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("the mock LSP never initialized");
}

/// Poll the redraw stream for a floating doc-float window containing `want`. `None`
/// once the bounded retries are exhausted with no such float.
async fn poll_float(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    want: &str,
) -> Option<Vec<(Value, Value)>> {
    for _ in 0..200 {
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| floating_window(m).is_some()) {
            let win = floating_window(&map).expect("a floating window");
            if window_lines(&win).iter().any(|l| l.contains(want)) {
                return Some(win);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    None
}

/// Whether any floating window is currently in the latest redraw (bounded poll).
async fn any_float(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> bool {
    for _ in 0..8 {
        nxvim_test_harness::barrier(rpc).await;
        if drain_to_latest_redraw(incoming, |m| floating_window(m).is_some()).is_some() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    }
    false
}

#[tokio::test]
async fn autotrigger_floats_signature_on_open_paren_and_stays_while_typing() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_sig_auto");
    arm_mock(&dir, SIG_SCRIPT);
    let (rpc, mut incoming) = start(&dir).await;

    // Opt in, then wait until the server's trigger chars (`(` / `,`) have reached core.
    exec_lua(&rpc, "nx.lsp.signature_help_autotrigger(true)").await;
    wait_for_lsp(&rpc).await;

    // Type a call on a fresh line: the `(` auto-fires signature help.
    feed(&rpc, "ofoo(");
    let win = poll_float(&rpc, &mut incoming, "fn foo(a: i32, b: i32)")
        .await
        .expect("typing `(` auto-opens the signature float");
    assert!(
        window_lines(&win).iter().any(|l| l.contains("[a: i32]")),
        "the active parameter is marked, got {:?}",
        window_lines(&win)
    );

    // Typing an argument character keeps the float (it is sticky during the session),
    // unlike a hover which the next key dismisses.
    feed(&rpc, "x");
    assert!(
        poll_float(&rpc, &mut incoming, "fn foo(a: i32, b: i32)")
            .await
            .is_some(),
        "the signature float survives typing into the call"
    );

    // Leaving insert mode ends the session and closes the float.
    feed(&rpc, "<Esc>");
    assert!(
        !any_float(&rpc, &mut incoming).await,
        "leaving insert mode closes the signature float"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

#[tokio::test]
async fn autotrigger_is_off_by_default() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_sig_auto_off");
    arm_mock(&dir, SIG_SCRIPT);
    let (rpc, mut incoming) = start(&dir).await;
    wait_for_lsp(&rpc).await;

    // No opt-in: typing `(` must NOT float anything — signature help stays manual.
    feed(&rpc, "ofoo(");
    assert!(
        !any_float(&rpc, &mut incoming).await,
        "without opting in, typing `(` does not auto-open signature help"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

#[tokio::test]
async fn empty_hover_echoes_instead_of_an_empty_float() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_empty");
    // No `hover` field ⇒ the mock returns null ⇒ a brief message, no float.
    arm_mock(&dir, r#"{ }"#);
    let (rpc, mut incoming) = start(&dir).await;

    // Drive hover until the server has attached and answered (its empty reply
    // echoes "No hover information"); the float must never open along the way. The
    // transient "No language server attached" startup message is skipped past.
    let mut saw_empty_hover = false;
    let mut last_message = String::new();
    for _ in 0..200 {
        exec_lua(&rpc, "nx.lsp.hover()").await;
        nxvim_test_harness::barrier(&rpc).await;
        if let Some(map) = drain_to_latest_redraw(&mut incoming, |_| true) {
            assert!(
                !matches!(map_get(&map, "float"), Some(Value::Map(_))),
                "an empty hover must not open a float"
            );
            if let Some(m) = map_get(&map, "message").and_then(Value::as_str) {
                if !m.is_empty() {
                    last_message = m.to_string();
                }
                if m.contains("hover information") {
                    saw_empty_hover = true;
                    break;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        saw_empty_hover,
        "expected the empty-hover message, last saw {last_message:?}"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}
