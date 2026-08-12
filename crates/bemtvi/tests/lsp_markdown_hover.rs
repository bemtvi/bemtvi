//! Behavior test for **rendered** markdown in the LSP hover float. A scripted mock
//! server returns a markdown hover (`# heading`, `**bold**`, a fenced code block);
//! the hover float must show the *stripped* text (no `**`, `#`, or ` ``` `) and
//! carry `@markup.*` highlight spans over it — i.e. `EditHost::show_hover` routes
//! through `Editor::open_markdown_float`, not the old verbatim `open_doc_float`.
//!
//! Wired like `lsp_config.rs`: the mock (`bemtvi --__lsp-mock`) stands in via
//! `$BEMTVI_LSP_CMD` (process-global env ⇒ `serial_lock`), driven by a `rust`-filetype
//! buffer.

use std::path::Path;
use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    attach, drain_to_latest_redraw, exec_lua, feed, feed_mouse, map_get, serial_lock, spawn,
    temp_dir,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const BEMTVI_BIN: &str = env!("CARGO_BIN_EXE_bemtvi");

fn arm_mock(dir: &Path, script: &str) {
    std::fs::write(dir.join("mock.json"), script).expect("write mock script");
    // SAFETY: serialized on `serial_lock`, so no other test races this env mutation.
    std::env::set_var(
        "BEMTVI_LSP_CMD",
        format!("{BEMTVI_BIN} --__lsp-mock {}/mock.json", dir.display()),
    );
}

async fn open_rust(dir: &Path) -> (Rpc, UnboundedReceiver<Incoming>) {
    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, "let foo = bar()\n").expect("write test file");
    let init = ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    feed(&rpc, "0fw"); // cursor on `foo` so hover has a symbol
    (rpc, incoming)
}

/// The first floating window in a redraw — the hover doc float — or `None`.
fn floating_window(map: &[(Value, Value)]) -> Option<Vec<(Value, Value)>> {
    let windows = map_get(map, "windows")?.as_array()?;
    windows
        .iter()
        .filter_map(Value::as_map)
        .find(|w| map_get(w, "floating").and_then(Value::as_bool) == Some(true))
        .cloned()
}

/// A float window's rendered text rows.
fn window_lines(win: &[(Value, Value)]) -> Vec<String> {
    match map_get(win, "lines") {
        Some(Value::Array(rows)) => rows
            .iter()
            .map(|r| r.as_str().unwrap_or_default().to_string())
            .collect(),
        _ => Vec::new(),
    }
}

/// Every highlight group name on the float window's `highlights` rows (each span is
/// `[start, end, group, style_id]`). Proves which `@markup.*` groups were painted,
/// independent of whether the test colorscheme resolves them to a style.
fn window_groups(win: &[(Value, Value)]) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(Value::Array(rows)) = map_get(win, "highlights") {
        for row in rows {
            if let Value::Array(spans) = row {
                for span in spans {
                    if let Some(g) = span
                        .as_array()
                        .and_then(|s| s.get(2))
                        .and_then(Value::as_str)
                    {
                        out.push(g.to_string());
                    }
                }
            }
        }
    }
    out
}

/// Retry `btv.lsp.hover()` until the float window carries a line containing `want`,
/// returning that window's `(lines, groups)`.
async fn await_hover(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    want: &str,
) -> (Vec<String>, Vec<String>) {
    for _ in 0..200 {
        exec_lua(rpc, "btv.lsp.hover()").await;
        bemtvi_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| floating_window(m).is_some()) {
            let win = floating_window(&map).unwrap();
            let lines = window_lines(&win);
            if lines.iter().any(|l| l.contains(want)) {
                return (lines, window_groups(&win));
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the hover float never contained {want:?}");
}

#[tokio::test]
async fn hover_markdown_is_rendered_stripped_and_styled() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_md_hover");
    // A markdown hover with a heading, bold text, and a fenced code block.
    arm_mock(
        &dir,
        r##"{ "hover": { "contents": { "kind": "markdown",
             "value": "# Title\n\nUses **bold** text.\n\n```rust\nlet x = 1;\n```" } } }"##,
    );
    let (rpc, mut incoming) = open_rust(&dir).await;

    exec_lua(
        &rpc,
        r#"
        btv.lsp.config("mock", { cmd = { "placeholder" }, filetypes = { "rust" } })
        btv.lsp.enable("mock")
        "#,
    )
    .await;

    let (lines, groups) = await_hover(&rpc, &mut incoming, "let x = 1;").await;

    // The markup syntax is gone: the heading dropped its `#`, bold dropped its `**`,
    // and the fence lines dropped their backticks.
    assert!(
        lines.iter().any(|l| l.trim() == "Title"),
        "heading should render without '#', got {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("Uses bold text.")),
        "bold should render without '**', got {lines:?}"
    );
    assert!(
        lines
            .iter()
            .all(|l| !l.contains("**") && !l.contains("```") && !l.trim_start().starts_with('#')),
        "no raw markdown markers should remain, got {lines:?}"
    );

    // Styling was applied: the heading and the bold word carry `@markup.*` spans.
    assert!(
        groups.iter().any(|g| g == "@markup.heading.1"),
        "heading should be tagged @markup.heading.1, got groups {groups:?}"
    );
    assert!(
        groups.iter().any(|g| g == "@markup.strong"),
        "bold should be tagged @markup.strong, got groups {groups:?}"
    );

    std::env::remove_var("BEMTVI_LSP_CMD");
}

/// A window map's `rect` field.
fn win_rect(win: &[(Value, Value)], key: &str) -> u64 {
    match map_get(win, "rect") {
        Some(Value::Map(r)) => map_get(r, key).and_then(Value::as_u64).unwrap_or(0),
        _ => 0,
    }
}

/// The hover float **wraps** (a long paragraph — one reflowed line, since markdown
/// collapses its soft breaks — reads fully instead of truncating), and so it does NOT
/// scroll horizontally: a horizontal wheel over it is a no-op (`leftcol` stays 0).
#[tokio::test]
async fn hover_wraps_and_does_not_scroll_horizontally() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_hover_wrap");
    // A hover whose body is one long paragraph, far wider than the ~80-col float.
    let long = "word ".repeat(40);
    arm_mock(
        &dir,
        &format!(
            r##"{{ "hover": {{ "contents": {{ "kind": "markdown", "value": "{long}" }} }} }}"##
        ),
    );
    let (rpc, mut incoming) = open_rust(&dir).await;
    exec_lua(
        &rpc,
        r#"
        btv.lsp.config("mock", { cmd = { "placeholder" }, filetypes = { "rust" } })
        btv.lsp.enable("mock")
        "#,
    )
    .await;

    let mut win = None;
    for _ in 0..200 {
        exec_lua(&rpc, "btv.lsp.hover()").await;
        bemtvi_test_harness::barrier(&rpc).await;
        if let Some(map) = drain_to_latest_redraw(&mut incoming, |m| floating_window(m).is_some()) {
            let w = floating_window(&map).unwrap();
            if window_lines(&w).iter().any(|l| l.contains("word")) {
                win = Some(w);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let win = win.expect("the hover float appeared");
    assert_eq!(
        map_get(&win, "leftcol").and_then(Value::as_u64),
        Some(0),
        "starts unscrolled"
    );
    // The float is sized to the WRAPPED rows: the 200-col paragraph is one buffer line
    // that wraps to several rows within the ~80-col float, so the window is taller than
    // the 3 (1 body + border) a raw-line-count height would give — not a clipped 1-row body.
    assert!(
        window_lines(&win).len() > 1 && win_rect(&win, "height") >= 4,
        "the hover float sizes to the wrapped paragraph, got {} rows / height {}",
        window_lines(&win).len(),
        win_rect(&win, "height")
    );
    let (rx, ry) = (win_rect(&win, "x") as usize, win_rect(&win, "y") as usize);

    // A horizontal wheel over the wrapped hover float must not scroll it sideways.
    for _ in 0..3 {
        feed_mouse(&rpc, "wheel", "right", ry + 1, rx + 1);
    }
    bemtvi_test_harness::barrier(&rpc).await;
    let map = drain_to_latest_redraw(&mut incoming, |m| floating_window(m).is_some())
        .expect("a redraw with the hover float");
    let win = floating_window(&map).expect("hover float still open");
    assert_eq!(
        map_get(&win, "leftcol").and_then(Value::as_u64),
        Some(0),
        "a wrapped hover float never scrolls horizontally"
    );

    std::env::remove_var("BEMTVI_LSP_CMD");
}
