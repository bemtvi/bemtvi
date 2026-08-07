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
    // Put the cursor on `foo` (column 4) so a hover request has a symbol; it stays
    // there (no motion) so the reply's cursor-staleness gate passes.
    start_with(dir, "let foo = bar()\n", "0fw").await
}

/// [`start`] over arbitrary buffer `text`, with `keys` placing the cursor — so a
/// test can drive a hover from a chosen screen row (e.g. the last one).
async fn start_with(dir: &Path, text: &str, keys: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, text).expect("write test file");
    let init = ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    feed(&rpc, keys);
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

/// The rows of a float window carrying an **overlay** `virt_text` placement, as
/// `(row, screen column, text)` — how the signature float's active-parameter
/// marker reaches a client. The wire shape per placement is
/// `[pos, col, hl_mode, [[text, style_id], …]]` with `pos == 2` for an overlay.
fn overlay_markers(win: &[(Value, Value)]) -> Vec<(usize, u64, String)> {
    let Some(rows) = map_get(win, "virt_text").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (row, placements) in rows.iter().enumerate() {
        let Some(placements) = placements.as_array() else {
            continue;
        };
        for p in placements {
            let Some(a) = p.as_array() else { continue };
            if a[0].as_u64() != Some(2) {
                continue;
            }
            let col = a[1].as_u64().unwrap_or_default();
            let text: String = a[3]
                .as_array()
                .map(|chunks| {
                    chunks
                        .iter()
                        .filter_map(|c| c.as_array()?[0].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            out.push((row, col, text));
        }
    }
    out
}

/// The resolved *style* of the first overlay `virt_text` chunk in `win` — the
/// signature float's active-parameter caret — looked up in the frame's `styles`
/// palette (the wire carries a palette index per chunk). `None` when there is no
/// overlay chunk or its group resolved to nothing.
fn overlay_marker_style(
    redraw: &[(Value, Value)],
    win: &[(Value, Value)],
) -> Option<Vec<(Value, Value)>> {
    let styles = map_get(redraw, "styles")?.as_array()?;
    let rows = map_get(win, "virt_text")?.as_array()?;
    let id = rows
        .iter()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(Value::as_array)
        .filter(|p| p[0].as_u64() == Some(2))
        .filter_map(|p| p[3].as_array()?.first()?.as_array()?[1].as_u64())
        .next()?;
    match styles.get(id as usize)? {
        Value::Map(m) => Some(m.clone()),
        _ => None,
    }
}

/// A color channel (`fg` / `bg`) of a wire style map as `0xRRGGBB`; `None` when the
/// style leaves it unset — which for `bg` is exactly "the surface below shows".
fn hl_color(style: &[(Value, Value)], key: &str) -> Option<u64> {
    style
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .and_then(|(_, v)| v.as_u64())
}

/// Paint a captured redraw through the **real client renderer** and return its
/// rows as strings — the tier-2 check that a wire-level decoration actually lands
/// on cells a user sees (mirrors `screen.rs`).
fn painted_rows(redraw: &[(Value, Value)]) -> Vec<String> {
    let mut view = nxvim_view::View::default();
    view.update(&[Value::Map(redraw.to_vec())]);
    let buf = nxvim_tui::paint(&view, 80, 24);
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
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

/// The focused *text* window's cursor cell as an absolute screen row — its rect's
/// top plus the in-window `cursor_row`. The float has to stay clear of this row.
fn cursor_screen_row(map: &[(Value, Value)]) -> usize {
    let windows = map_get(map, "windows")
        .and_then(Value::as_array)
        .expect("windows in the redraw");
    let win = windows
        .iter()
        .filter_map(Value::as_map)
        .find(|w| {
            map_get(w, "floating").and_then(Value::as_bool) != Some(true)
                && map_get(w, "focused").and_then(Value::as_bool) == Some(true)
        })
        .expect("the focused text window");
    let (_, y, _, _) = window_rect(win);
    y + map_get(win, "cursor_row")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize
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
    await_doc_float_redraw(rpc, incoming, trigger, want).await.1
}

/// [`await_doc_float_window`], also returning the whole redraw the float came in —
/// what [`painted_rows`] needs to render the frame.
async fn await_doc_float_redraw(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    trigger: &str,
    want: &str,
) -> (Vec<(Value, Value)>, Vec<(Value, Value)>) {
    for _ in 0..200 {
        exec_lua(rpc, trigger).await;
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| floating_window(m).is_some()) {
            let win = floating_window(&map).expect("a floating window");
            if window_lines(&win).iter().any(|l| l.contains(want)) {
                return (map, win);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("the doc-float window never contained {want:?}");
}

/// A doc float drops *below* the cursor's line — but near the bottom of the screen
/// there is no room below, and merely clamping the box on-screen slides it back up
/// **over** the cursor, hiding the very line being described (you cannot see what
/// you are typing while the signature popup covers the call). It must flip above the
/// cursor line instead, the way the content-float projection already does.
#[tokio::test]
async fn a_hover_at_the_bottom_of_the_screen_opens_above_the_cursor() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_hover_bottom");
    arm_mock(
        &dir,
        r#"{ "hover": { "contents": { "kind": "markdown",
             "value": "`foo`: a scripted hover symbol" } } }"#,
    );
    // More lines than the 24-row screen, so `G` scrolls the view and leaves the
    // cursor on the LAST text row — no room at all for a float below it.
    let mut text: String = (0..60).map(|_| "let x = 1\n").collect();
    text.push_str("let foo = bar()\n");
    let (rpc, mut incoming) = start_with(&dir, &text, "G0fw").await;

    let (redraw, win) =
        await_doc_float_redraw(&rpc, &mut incoming, "nx.lsp.hover()", "scripted hover").await;
    let cursor_row = cursor_screen_row(&redraw);
    let (_, y, _, h) = window_rect(&win);
    assert!(
        y + h <= cursor_row,
        "the hover float must sit entirely above the cursor row {cursor_row}, \
         got rows {y}..{}",
        y + h
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

/// A popup taller than *either* side of the cursor: it takes the roomier side and
/// shrinks into it (the content scrolls) rather than spilling over the cursor line.
#[tokio::test]
async fn a_tall_hover_shrinks_into_the_roomier_side_of_the_cursor() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_hover_tall");
    // 30 lines of hover — 20 after the popup's height cap, 22 with its border, more
    // than fits above *or* below a cursor sitting low in a 24-row screen.
    let body = format!(
        "```\\n{}\\n```",
        (0..30)
            .map(|i| format!("hover line {i:02}"))
            .collect::<Vec<_>>()
            .join("\\n")
    );
    arm_mock(
        &dir,
        &format!(r#"{{ "hover": {{ "contents": {{ "kind": "markdown", "value": "{body}" }} }} }}"#),
    );
    // Every line hovers, so `G10k` lands the cursor ~10 rows off the bottom: more
    // room above than below, but not enough for the whole popup on either side.
    let text: String = (0..60).map(|_| "let foo = bar()\n").collect();
    let (rpc, mut incoming) = start_with(&dir, &text, "G10k0fw").await;

    let (redraw, win) =
        await_doc_float_redraw(&rpc, &mut incoming, "nx.lsp.hover()", "hover line 00").await;
    let cursor_row = cursor_screen_row(&redraw);
    let (_, y, _, h) = window_rect(&win);
    assert!(
        y + h <= cursor_row,
        "a too-tall hover shrinks to stay clear of the cursor row {cursor_row}, \
         got rows {y}..{}",
        y + h
    );
    assert!(
        h > 2,
        "the shrunk float still shows content, got height {h}"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

/// The signature popup is the surface this hurts most — you are mid-call in insert
/// mode, and a popup over the cursor hides what you are typing. Same placement rule.
#[tokio::test]
async fn a_signature_float_at_the_bottom_of_the_screen_opens_above_the_cursor() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_sig_bottom");
    arm_mock(
        &dir,
        r#"{ "signature_help": { "signatures": [
             { "label": "fn foo(a: i32, b: i32)",
               "parameters": [ { "label": "a: i32" }, { "label": "b: i32" } ] } ],
             "activeSignature": 0, "activeParameter": 0 } }"#,
    );
    let mut text: String = (0..60).map(|_| "let x = 1\n").collect();
    text.push_str("let foo = bar()\n");
    let (rpc, mut incoming) = start_with(&dir, &text, "G0fw").await;

    let (redraw, win) =
        await_doc_float_redraw(&rpc, &mut incoming, "nx.lsp.signature_help()", "fn foo(").await;
    let cursor_row = cursor_screen_row(&redraw);
    let (_, y, _, h) = window_rect(&win);
    assert!(
        y + h <= cursor_row,
        "the signature float must sit entirely above the cursor row {cursor_row}, \
         got rows {y}..{}",
        y + h
    );

    std::env::remove_var("NXVIM_LSP_CMD");
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
    // LSP doc content is markdown, now RENDERED: the scratch buffer holds the stripped
    // text — the `` `foo` `` inline code shows as plain `foo` (backticks gone).
    assert!(
        window_lines(&win)
            .iter()
            .any(|l| l == "foo: a scripted hover symbol"),
        "hover markdown should render stripped, got {:?}",
        window_lines(&win)
    );
    // The rendered buffer is left untyped (styling comes from the render's extmarks),
    // so a filetype ts pass never repaints the already-stripped text as markdown.
    assert_ne!(
        window_filetype(&win),
        "markdown",
        "the rendered hover buffer is not typed markdown"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

#[tokio::test]
async fn hover_markdown_html_entities_are_decoded_to_plain_text() {
    // pyright / basedpyright encode a docstring's leading indentation as `&nbsp;`
    // HTML entities (and escape `<`/`>`/`&` as `&lt;`/`&gt;`/`&amp;`) in its markdown
    // hover. nxvim renders markdown as plain text, so those entities must be decoded
    // back to the characters they stand for — otherwise the float shows the literal
    // `&nbsp;` noise instead of the intended indentation.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_hover_entities");
    arm_mock(
        &dir,
        r#"{ "hover": { "contents": { "kind": "markdown",
             "value": "Example:\n\n&nbsp;&nbsp;&nbsp;&nbsp;foo &lt;T&gt; &amp; bar" } } }"#,
    );
    let (rpc, mut incoming) = start(&dir).await;

    let win = await_doc_float_window(&rpc, &mut incoming, "nx.lsp.hover()", "foo").await;
    let lines = window_lines(&win);
    assert!(
        !lines.iter().any(|l| l.contains("&nbsp;")
            || l.contains("&lt;")
            || l.contains("&gt;")
            || l.contains("&amp;")),
        "HTML entities should be decoded, not shown literally, got {lines:?}"
    );
    // The renderer decodes `&lt;`/`&gt;`/`&amp;` to their characters; `&nbsp;` becomes
    // a non-breaking space (whitespace, trimmed here) so the indentation is kept as a
    // paragraph rather than being swallowed as a code block.
    assert!(
        lines.iter().any(|l| l.trim_start() == "foo <T> & bar"),
        "`&lt;`/`&gt;`/`&amp;` should decode to their chars, got {lines:?}"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

#[tokio::test]
async fn hover_window_scrolls_with_the_wheel_and_a_key_dismisses_it() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_hover_scroll");
    // A tall hover (more lines than the float's 20-row cap) so there is content to
    // scroll past — the whole reason a doc float is a window, not a content overlay. A
    // **code block** so the lines stay distinct (outside a fence, markdown collapses the
    // single newlines into one wrapped paragraph).
    let body = format!(
        "```\\n{}\\n```",
        (0..30)
            .map(|i| format!("hover line {i:02}"))
            .collect::<Vec<_>>()
            .join("\\n")
    );
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

    // Signature help is the same scrollable doc-float WINDOW as the hover, with the
    // call broken one parameter per line so a long signature stays readable.
    let win =
        await_doc_float_window(&rpc, &mut incoming, "nx.lsp.signature_help()", "fn foo(").await;
    assert_eq!(
        window_lines(&win),
        vec!["fn foo(", "    a: i32,", "    b: i32,", ")"],
        "the signature splits into a leader, one line per parameter, and a trailer"
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

/// The active parameter is pointed at by a marker OVERLAID on its line's indent —
/// `activeParameter: 0` marks the first parameter's row (row 1, under the leader).
/// The marker is virtual text, not buffer text, so the popup's lines stay valid
/// code for the tree-sitter pass that colors them.
#[tokio::test]
async fn signature_help_marks_the_active_parameter_line() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_sig_marker");
    arm_mock(
        &dir,
        r#"{ "signature_help": { "signatures": [
             { "label": "fn foo(a: i32, b: i32)",
               "parameters": [ { "label": "a: i32" }, { "label": "b: i32" } ] } ],
             "activeSignature": 0, "activeParameter": 0 } }"#,
    );
    let (rpc, mut incoming) = start(&dir).await;

    let (redraw, win) =
        await_doc_float_redraw(&rpc, &mut incoming, "nx.lsp.signature_help()", "a: i32").await;
    assert_eq!(
        overlay_markers(&win),
        vec![(1, 2, "▸".to_string())],
        "the marker overlays column 2 of the FIRST parameter's row, got lines {:?}",
        window_lines(&win)
    );
    // The marker never enters the text — the row is still the bare indented parameter.
    assert_eq!(
        window_lines(&win).get(1).map(String::as_str),
        Some("    a: i32,"),
        "the marked line's buffer text carries no marker glyph"
    );
    // Tier 2: the overlay reaches actual painted cells, drawn INTO the indent so the
    // parameter text keeps its column — not shifted right by a spliced-in glyph.
    let rows = painted_rows(&redraw);
    assert!(
        rows.iter().any(|r| r.contains("  ▸ a: i32,")),
        "the marker paints on screen over the indent, got {:?}",
        rows.iter()
            .filter(|r| r.contains("i32"))
            .collect::<Vec<_>>()
    );
    assert!(
        rows.iter().any(|r| r.contains("    b: i32,")),
        "the unmarked parameter keeps the bare indent, aligned with the marked one"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

/// The marker takes its highlight group's **foreground only** — it is a caret drawn
/// into the popup's indent, not a highlight *of* the parameter's text, so the float's
/// own background has to run through it. `LspSignatureActiveParameter` is a group a
/// theme is free to express as a pure background band (catppuccin defines nothing but
/// `bg` + `bold`, because neovim paints it over the parameter text itself); painted
/// over a lone `▸` in the indent that band reads as a coloured box adrift in the popup.
#[tokio::test]
async fn signature_marker_takes_the_group_foreground_not_its_background() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_sig_marker_hl");
    arm_mock(
        &dir,
        r#"{ "signature_help": { "signatures": [
             { "label": "fn foo(a: i32, b: i32)",
               "parameters": [ { "label": "a: i32" }, { "label": "b: i32" } ] } ],
             "activeSignature": 0, "activeParameter": 0 } }"#,
    );
    let (rpc, mut incoming) = start(&dir).await;
    // A theme's take on the group: an accent *background* band, plus a foreground so
    // the assertion can tell "fg kept" from "nothing resolved".
    exec_lua(
        &rpc,
        "vim.api.nvim_set_hl(0, 'LspSignatureActiveParameter', \
         { fg = '#89b4fa', bg = '#313244', bold = true })",
    )
    .await;

    let (redraw, win) =
        await_doc_float_redraw(&rpc, &mut incoming, "nx.lsp.signature_help()", "a: i32").await;
    let style = overlay_marker_style(&redraw, &win).expect("the marker chunk resolved a style");
    assert_eq!(
        hl_color(&style, "fg"),
        Some(0x89b4fa),
        "the caret keeps the group's accent colour, got {style:?}"
    );
    assert_eq!(
        hl_color(&style, "bg"),
        None,
        "the caret drops the group's background so the float's own shows through, \
         got {style:?}"
    );
    assert_eq!(
        style
            .iter()
            .find(|(k, _)| k.as_str() == Some("bold"))
            .and_then(|(_, v)| v.as_bool()),
        Some(true),
        "the group's attributes still apply, got {style:?}"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

/// The marker follows the server's `activeParameter`: pointing at the *second*
/// parameter moves it one row down. Together with the test above (same signature,
/// different index) this pins the mapping index → row rather than a fixed row.
#[tokio::test]
async fn signature_help_marker_follows_the_active_parameter_index() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_sig_marker2");
    arm_mock(
        &dir,
        r#"{ "signature_help": { "signatures": [
             { "label": "fn foo(a: i32, b: i32)",
               "parameters": [ { "label": "a: i32" }, { "label": "b: i32" } ] } ],
             "activeSignature": 0, "activeParameter": 1 } }"#,
    );
    let (rpc, mut incoming) = start(&dir).await;

    let win =
        await_doc_float_window(&rpc, &mut incoming, "nx.lsp.signature_help()", "b: i32").await;
    assert_eq!(
        overlay_markers(&win),
        vec![(2, 2, "▸".to_string())],
        "the marker follows activeParameter=1 to the SECOND parameter's row, lines {:?}",
        window_lines(&win)
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

/// Parameters given as UTF-16 **label offsets** (LSP's other, authoritative form)
/// split exactly like string labels — the layout reads the server's spans, it does
/// not re-derive them by splitting on commas. The signature here is deliberately
/// comma-rich *inside* a parameter (`dict[str, int]`): a comma split would produce
/// four rows, the structural split produces two.
#[tokio::test]
async fn signature_help_splits_on_label_offsets_not_commas() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_sig_offsets");
    // "def load(cfg: dict[str, int], key: tuple[A, B]) -> None"
    //           ^9            ^28  ^30           ^45
    arm_mock(
        &dir,
        r#"{ "signature_help": { "signatures": [
             { "label": "def load(cfg: dict[str, int], key: tuple[A, B]) -> None",
               "parameters": [ { "label": [9, 28] }, { "label": [30, 46] } ] } ],
             "activeSignature": 0, "activeParameter": 1 } }"#,
    );
    let (rpc, mut incoming) = start(&dir).await;

    let win =
        await_doc_float_window(&rpc, &mut incoming, "nx.lsp.signature_help()", "def load(").await;
    assert_eq!(
        window_lines(&win),
        vec![
            "def load(",
            "    cfg: dict[str, int],",
            "    key: tuple[A, B],",
            ") -> None",
        ],
        "offset-labelled parameters split structurally, keeping their inner commas"
    );
    assert_eq!(
        overlay_markers(&win),
        vec![(2, 2, "▸".to_string())],
        "the marker lands on the offset-resolved second parameter"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

/// A single-parameter call has nothing to lay out vertically — three rows would
/// say what one already says — so it keeps the compact one-line form, with the
/// active parameter named in brackets as before.
#[tokio::test]
async fn single_parameter_signature_stays_on_one_line() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_sig_one");
    arm_mock(
        &dir,
        r#"{ "signature_help": { "signatures": [
             { "label": "fn only(a: i32)", "parameters": [ { "label": "a: i32" } ] } ],
             "activeSignature": 0, "activeParameter": 0 } }"#,
    );
    let (rpc, mut incoming) = start(&dir).await;

    let win =
        await_doc_float_window(&rpc, &mut incoming, "nx.lsp.signature_help()", "fn only").await;
    assert_eq!(
        window_lines(&win),
        vec!["fn only(a: i32)    [a: i32]"],
        "a one-parameter signature stays on one line"
    );
    assert!(
        overlay_markers(&win).is_empty(),
        "with nothing split there is no parameter row to point at"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

/// A server whose parameter labels cannot be located in its own signature label
/// (here: labels that are plain names, not substrings of the label) has no spans
/// to split on. Rather than guess, the float falls back to the single-line
/// rendering — and still names the active parameter.
#[tokio::test]
async fn unlocatable_parameters_fall_back_to_one_line() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_sig_fallback");
    arm_mock(
        &dir,
        r#"{ "signature_help": { "signatures": [
             { "label": "fn foo(a: i32, b: i32)",
               "parameters": [ { "label": "first" }, { "label": "second" } ] } ],
             "activeSignature": 0, "activeParameter": 1 } }"#,
    );
    let (rpc, mut incoming) = start(&dir).await;

    let win =
        await_doc_float_window(&rpc, &mut incoming, "nx.lsp.signature_help()", "fn foo").await;
    assert_eq!(
        window_lines(&win),
        vec!["fn foo(a: i32, b: i32)    [second]"],
        "unlocatable parameters degrade to the labelled single line"
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
    let win = poll_float(&rpc, &mut incoming, "a: i32")
        .await
        .expect("typing `(` auto-opens the signature float");
    assert_eq!(
        overlay_markers(&win),
        vec![(1, 2, "▸".to_string())],
        "the active parameter is marked, got {:?}",
        window_lines(&win)
    );

    // Typing an argument character keeps the float (it is sticky during the session),
    // unlike a hover which the next key dismisses.
    feed(&rpc, "x");
    assert!(
        poll_float(&rpc, &mut incoming, "a: i32").await.is_some(),
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
