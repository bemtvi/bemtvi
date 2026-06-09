//! LSP inlay hints (Phases 1–2): `textDocument/inlayHint` decoded against the
//! negotiated encoding and projected as **inline** virtual text.
//!
//! These tests drive the scripted mock — an `inlay_hints` script field carries an
//! `InlayHint[]`, the mock advertises `inlayHintProvider` and returns it — and
//! assert on the `inlay_hints` redraw key: each hint surfaces as `[col, text]` at
//! the right screen column, but only once `vim.lsp.inlay_hint.enable(true)` turns
//! the feature on (it is opt-in, off by default). A Tier-2 grid test proves the
//! inline splice pushes the real glyphs right on the rendered screen.
//!
//! Phase 2 adds the read/refine surface: `client.server_capabilities.inlayHintProvider`
//! reads truthy, `vim.lsp.inlay_hint.get(filter)` reads the cached hints from Lua,
//! and a **lazy** hint (empty label + `data`) has its label filled via
//! `inlayHint/resolve` (scripted with the `inlay_resolve` mock field).

use crate::support::*;

/// Build one LSP `InlayHint` JSON object at `(line, character)` with `label` and
/// `kind` (`1`=type, `2`=parameter).
fn inlay(line: u64, character: u64, label: &str, kind: u64) -> Json {
    serde_json::json!({
        "position": { "line": line, "character": character },
        "label": label,
        "kind": kind,
    })
}

/// The `(col, text)` inline inlay hints on window row `row` (0-based) of the latest
/// redraw, screen columns. Drops the trailing `style_id`.
fn inlay_on_row(params: &[Value], row: usize) -> Vec<(u64, String)> {
    let rows = window0_get(params, "inlay_hints")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(Value::Array(hints)) = rows.get(row).cloned() else {
        return Vec::new();
    };
    hints
        .iter()
        .filter_map(|h| {
            let a = h.as_array()?;
            Some((a.first()?.as_u64()?, a.get(1)?.as_str()?.to_string()))
        })
        .collect()
}

/// The per-band-row inline inlay hints from a redraw carrying a scroll gesture
/// (`scroll.inlay_hints`), or `None` if this redraw has no scroll band. Each entry
/// is the row's `(col, text)` hints, mirroring [`inlay_on_row`] over the band.
fn scroll_band_inlay(params: &[Value]) -> Option<Vec<Vec<(u64, String)>>> {
    let Value::Map(s) = window0_get(params, "scroll")? else {
        return None;
    };
    let rows = s
        .iter()
        .find(|(k, _)| k.as_str() == Some("inlay_hints"))
        .and_then(|(_, v)| v.as_array())?;
    Some(
        rows.iter()
            .map(|row| {
                row.as_array()
                    .map(|hints| {
                        hints
                            .iter()
                            .filter_map(|h| {
                                let a = h.as_array()?;
                                Some((a.first()?.as_u64()?, a.get(1)?.as_str()?.to_string()))
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect(),
    )
}

/// How many `textDocument/inlayHint` requests the mock has recorded.
fn inlay_request_count(recs: &[Json]) -> usize {
    recs.iter()
        .filter(|r| r["method"] == "textDocument/inlayHint")
        .count()
}

/// Enable inlay hints on the current buffer (off by default).
async fn enable_inlay(rpc: &Rpc) {
    exec_lua(rpc, "vim.lsp.inlay_hint.enable(true)").await;
}

/// Poll (bounded) until window row `row` carries an inlay hint whose text is
/// `text`, returning the redraw params that frame. Drives the loop so the async
/// inlay-hint reply is processed and its repaint lands. Panics otherwise.
async fn wait_for_inlay(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    row: usize,
    text: &str,
) -> Vec<Value> {
    let mut last = Vec::new();
    for _ in 0..100 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            let hints = inlay_on_row(&params, row);
            if hints.iter().any(|(_, t)| t == text) {
                return params;
            }
            last = hints;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("inlay hint {text:?} never appeared on row {row}; last hints: {last:?}");
}

#[tokio::test]
async fn inlay_hints_paint_when_enabled() {
    let _guard = test_lock().lock().await;
    // A type hint ": i32" anchored after `x` (char 5) on line 0.
    let record = configure_mock(
        "inlay-on",
        serde_json::json!({
            "position_encoding": "utf-8",
            "inlay_hints": [inlay(0, 5, ": i32", 1)],
        }),
    );
    let file = temp_file("inlay-on", "rs", "let x = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    enable_inlay(&rpc).await;
    // Enabling must fire a whole-buffer inlay-hint request.
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/inlayHint")).await;

    let params = wait_for_inlay(&rpc, &mut incoming, 0, ": i32").await;
    assert!(
        inlay_on_row(&params, 0).contains(&(5, ": i32".to_string())),
        "the hint paints at screen col 5: {:?}",
        inlay_on_row(&params, 0)
    );
}

#[tokio::test]
async fn inlay_hints_are_off_by_default() {
    let _guard = test_lock().lock().await;
    // Same scripted hint, but no `enable`: the buffer must request nothing and
    // paint nothing (inlay hints are opt-in, unlike semantic tokens).
    let record = configure_mock(
        "inlay-off",
        serde_json::json!({
            "position_encoding": "utf-8",
            "inlay_hints": [inlay(0, 5, ": i32", 1)],
        }),
    );
    let file = temp_file("inlay-off", "rs", "let x = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Drive a few frames so any (erroneous) request/paint would have landed.
    for _ in 0..5 {
        feed(&rpc, "0");
        barrier(&rpc).await;
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let params = drain_latest_redraw(&mut incoming).expect("a redraw");
    assert!(
        inlay_on_row(&params, 0).is_empty(),
        "no inlay hints without enable: {:?}",
        inlay_on_row(&params, 0)
    );
    assert!(
        !has_method(&record_lines(&record), "textDocument/inlayHint"),
        "no inlayHint request is sent while the feature is disabled"
    );
}

#[tokio::test]
async fn inlay_hint_columns_are_encoding_correct() {
    let _guard = test_lock().lock().await;
    // A UTF-16 server: "héllo" is 5 UTF-16 code units but 6 bytes ('é' is 2). A
    // hint anchored at char 5 (end of the word) must decode to byte 6 → screen
    // col 5 ('é' is one cell), proving the char→byte conversion runs through the
    // negotiated encoding before the byte→screen step (the diagnostics/semantic
    // encoding guard, for inlay hints).
    let record = configure_mock(
        "inlay-utf16",
        serde_json::json!({
            "position_encoding": "utf-16",
            "inlay_hints": [inlay(0, 5, ": T", 1)],
        }),
    );
    let file = temp_file("inlay-utf16", "rs", "héllo\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    enable_inlay(&rpc).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/inlayHint")).await;

    let params = wait_for_inlay(&rpc, &mut incoming, 0, ": T").await;
    assert!(
        inlay_on_row(&params, 0).contains(&(5, ": T".to_string())),
        "the hint anchors at screen col 5 over the wide-char line: {:?}",
        inlay_on_row(&params, 0)
    );
}

#[tokio::test]
async fn editing_re_requests_inlay_hints() {
    let _guard = test_lock().lock().await;
    // After the first reply paints, an edit's `didChange` flush must re-request the
    // whole inlay-hint set (the same on-change refresh semantic tokens get).
    let record = configure_mock(
        "inlay-edit",
        serde_json::json!({
            "position_encoding": "utf-8",
            "inlay_hints": [inlay(0, 5, ": i32", 1)],
        }),
    );
    let file = temp_file("inlay-edit", "rs", "let x = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    enable_inlay(&rpc).await;
    wait_for_inlay(&rpc, &mut incoming, 0, ": i32").await;

    // Append a character (and leave insert) so a `didChange` goes out.
    feed(&rpc, "A!");
    feed(&rpc, "\x1b");
    wait_for_record(&rpc, &record, |r| inlay_request_count(r) >= 2).await;
}

#[tokio::test]
async fn an_inlay_hint_shifts_the_text_right_on_the_grid() {
    let _guard = test_lock().lock().await;
    // Tier 2: the real client paint. A hint ":i32" anchored at char 5 of
    // "let x = 1" is spliced inline, pushing the trailing " = 1" right — the
    // painted row reads "let x:i32 = 1", proving the inline insertion (not an
    // end-of-line append) reaches the rendered grid.
    let record = configure_mock(
        "inlay-grid",
        serde_json::json!({
            "position_encoding": "utf-8",
            "inlay_hints": [inlay(0, 5, ":i32", 1)],
        }),
    );
    let file = temp_file("inlay-grid", "rs", "let x = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    enable_inlay(&rpc).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/inlayHint")).await;
    let params = wait_for_inlay(&rpc, &mut incoming, 0, ":i32").await;

    let buf = paint(&View::from_redraw(&params), COLS, ROWS);
    // The text body starts past the number gutter (no sign column — inlay hints
    // reserve none). The hint splices between "let x" and " = 1".
    let row: String = (GUTTER..GUTTER + 13)
        .map(|x| buf.cell((x, 0)).map(|c| c.symbol()).unwrap_or(""))
        .collect();
    assert_eq!(
        row, "let x:i32 = 1",
        "the inline hint shifts the trailing text right on the grid"
    );
}

#[tokio::test]
async fn inlay_hints_ride_the_scroll_band() {
    let _guard = test_lock().lock().await;
    // Regression: inlay hints used to vanish *during* a scroll slide — the band
    // carried none, so they only reappeared once the slide settled. The band must
    // now carry them (keyed on the band's lines like the syntax highlights), so they
    // slide with the text. A hint on line 15 sits inside the <C-d> band (base_line
    // 0, ~36 rows).
    let record = configure_mock(
        "inlay-scroll",
        serde_json::json!({
            "position_encoding": "utf-8",
            "inlay_hints": [inlay(15, 5, ": i32", 1)],
        }),
    );
    // A buffer taller than the viewport, so <C-d> produces a scroll gesture.
    let content: String = (0..60).map(|i| format!("let v{i} = {i}\n")).collect();
    let file = temp_file("inlay-scroll", "rs", &content);
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    enable_inlay(&rpc).await;
    // The hint on line 15 is requested, cached, and painted in the settled view.
    wait_for_inlay(&rpc, &mut incoming, 15, ": i32").await;

    // Half-page scroll: the redraw's scroll band must carry the hint on its
    // line-15 row, so the slide shows it instead of dropping it until it settles.
    feed(&rpc, "<C-d>");
    let mut band = None;
    for _ in 0..40 {
        barrier(&rpc).await;
        tokio::task::yield_now().await;
        while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
            if method == "redraw" {
                if let Some(b) = scroll_band_inlay(&params) {
                    band = Some(b);
                }
            }
        }
        if band.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let band = band.expect("a redraw carrying a scroll gesture");
    assert!(
        band.get(15)
            .is_some_and(|row| row.contains(&(5, ": i32".to_string()))),
        "the scroll band carries the inlay hint on band row 15: {:?}",
        band.get(15)
    );
}

// ---- Phase 2: caps / get / resolve --------------------------------------------

/// Build a **lazy** LSP `InlayHint` at `(line, character)`: an empty label plus a
/// `data` blob the server keys its `inlayHint/resolve` on. Distilled to a
/// placeholder the editor resolves rather than paints.
fn lazy_inlay(line: u64, character: u64, data: Json) -> Json {
    serde_json::json!({
        "position": { "line": line, "character": character },
        "label": "",
        "kind": 1,
        "data": data,
    })
}

/// Poll (bounded) `vim.lsp.get_clients()[1].server_capabilities[cap]` until a
/// client is registered, returning whether the capability is truthy (the
/// semantic-tokens cap helper, for inlay hints).
async fn wait_for_client_cap(rpc: &Rpc, cap: &str) -> bool {
    let code = format!(
        "local c = vim.lsp.get_clients()[1]; \
         if not c then return nil end; \
         return c.server_capabilities.{cap} and true or false"
    );
    for _ in 0..100 {
        barrier(rpc).await;
        if let Some(b) = exec_lua(rpc, &code).await.as_bool() {
            return b;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no client registered to read server_capabilities.{cap}");
}

/// Poll (bounded) `vim.lsp.inlay_hint.get(filter)` until it returns at least one
/// hint, yielding the first hint's `(byte col, label)`. Drives the loop so the
/// async inlay reply (and any resolve) lands. `filter_lua` is a Lua table literal.
async fn wait_for_get(rpc: &Rpc, filter_lua: &str) -> (u64, String) {
    let code = format!(
        "local hs = vim.lsp.inlay_hint.get({filter_lua}); \
         if #hs == 0 then return nil end; \
         local h = hs[1].inlay_hint; \
         return {{ h.position.character, h.label }}"
    );
    for _ in 0..100 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(arr) = exec_lua(rpc, &code)
            .await
            .as_array()
            .filter(|a| a.len() == 2)
        {
            let col = arr[0].as_u64().expect("col");
            let label = arr[1].as_str().expect("label").to_string();
            return (col, label);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("vim.lsp.inlay_hint.get({filter_lua}) never returned a hint");
}

#[tokio::test]
async fn inlay_hint_provider_capability_is_truthy() {
    let _guard = test_lock().lock().await;
    // A server scripting `inlay_hints` advertises `inlayHintProvider`, readable as
    // `client.server_capabilities.inlayHintProvider` (the cap an on_attach branches
    // on to bind inlay keymaps). A server without it reads falsy.
    let record = configure_mock(
        "inlay-caps-yes",
        serde_json::json!({
            "position_encoding": "utf-8",
            "inlay_hints": [inlay(0, 5, ": i32", 1)],
        }),
    );
    let file = temp_file("inlay-caps-yes", "rs", "let x = 1\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    assert!(
        wait_for_client_cap(&rpc, "inlayHintProvider").await,
        "a server scripting inlay hints advertises inlayHintProvider"
    );
}

#[tokio::test]
async fn server_without_inlay_hints_reports_no_provider() {
    let _guard = test_lock().lock().await;
    // No `inlay_hints` script ⇒ the mock advertises no provider ⇒ the cap is falsy.
    let record = configure_mock(
        "inlay-caps-no",
        serde_json::json!({ "position_encoding": "utf-8" }),
    );
    let file = temp_file("inlay-caps-no", "rs", "let x = 1\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    assert!(
        !wait_for_client_cap(&rpc, "inlayHintProvider").await,
        "a server with no inlay hints does not advertise inlayHintProvider"
    );
}

#[tokio::test]
async fn get_returns_cached_inlay_hints() {
    let _guard = test_lock().lock().await;
    // The read half of the surface: after enabling + a reply, `inlay_hint.get`
    // returns the cached hint; a `range` filter narrows by position; a disabled
    // buffer returns nothing.
    let record = configure_mock(
        "inlay-get",
        serde_json::json!({
            "position_encoding": "utf-8",
            "inlay_hints": [inlay(0, 5, ": i32", 1)],
        }),
    );
    let file = temp_file("inlay-get", "rs", "let x = 1\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Disabled (the default): get returns an empty list.
    let empty = exec_lua(&rpc, "return #vim.lsp.inlay_hint.get({ bufnr = 0 })").await;
    assert_eq!(
        empty.as_u64(),
        Some(0),
        "get is empty while hints are disabled"
    );

    enable_inlay(&rpc).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/inlayHint")).await;
    let (col, label) = wait_for_get(&rpc, "{ bufnr = 0 }").await;
    assert_eq!(
        (col, label.as_str()),
        (5, ": i32"),
        "get returns the cached hint"
    );

    // A range that excludes column 5 on line 0 filters the hint out.
    let outside = exec_lua(
        &rpc,
        "return #vim.lsp.inlay_hint.get({ bufnr = 0, range = { \
            start = { line = 0, character = 0 }, \
            ['end'] = { line = 0, character = 3 } } })",
    )
    .await;
    assert_eq!(
        outside.as_u64(),
        Some(0),
        "a range before the hint excludes it"
    );

    // A range that covers column 5 keeps it.
    let inside = exec_lua(
        &rpc,
        "return #vim.lsp.inlay_hint.get({ bufnr = 0, range = { \
            start = { line = 0, character = 0 }, \
            ['end'] = { line = 0, character = 9 } } })",
    )
    .await;
    assert_eq!(
        inside.as_u64(),
        Some(1),
        "a range covering the hint keeps it"
    );
}

#[tokio::test]
async fn inlay_hints_appear_after_workspace_refresh() {
    let _guard = test_lock().lock().await;
    // The lua_ls / gopls shape: the first `textDocument/inlayHint` returns empty
    // (analysis not ready), then the server sends `workspace/inlayHint/refresh`. The
    // editor MUST honor that by re-querying — the second request returns the hints,
    // which then paint. Without refresh handling the cache stays empty forever (the
    // real bug behind "enabled but nothing shows").
    let record = configure_mock(
        "inlay-refresh",
        serde_json::json!({
            "position_encoding": "utf-8",
            "inlay_hints": [inlay(0, 5, ": i32", 1)],
            "inlay_refresh": true,
        }),
    );
    let file = temp_file("inlay-refresh", "rs", "let x = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    enable_inlay(&rpc).await;

    // The refresh-driven re-query paints the hint (the first reply was empty).
    let params = wait_for_inlay(&rpc, &mut incoming, 0, ": i32").await;
    assert!(
        inlay_on_row(&params, 0).contains(&(5, ": i32".to_string())),
        "the hint paints after the refresh re-query: {:?}",
        inlay_on_row(&params, 0)
    );
    // Prove a second request actually went out (the refresh re-queried).
    assert!(
        inlay_request_count(&record_lines(&record)) >= 2,
        "the refresh must trigger a re-request (saw {} inlayHint requests)",
        inlay_request_count(&record_lines(&record))
    );
}

#[tokio::test]
async fn a_lazy_inlay_hint_label_resolves() {
    let _guard = test_lock().lock().await;
    // A lazy hint (empty label + `data`) paints nothing on its own; the editor
    // sends `inlayHint/resolve` and the scripted reply fills the label, which then
    // paints inline and shows up via `get`.
    let record = configure_mock(
        "inlay-resolve",
        serde_json::json!({
            "position_encoding": "utf-8",
            "inlay_hints": [lazy_inlay(0, 5, serde_json::json!({ "id": 7 }))],
            // The resolved hint: same anchor, label now filled in.
            "inlay_resolve": {
                "position": { "line": 0, "character": 5 },
                "label": ": i32",
                "kind": 1,
                "data": { "id": 7 },
            },
        }),
    );
    let file = temp_file("inlay-resolve", "rs", "let x = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    enable_inlay(&rpc).await;
    // The whole-buffer request, then the per-hint resolve, must both go out.
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/inlayHint")).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "inlayHint/resolve")).await;

    // The resolved label paints inline (the placeholder painted nothing before).
    let params = wait_for_inlay(&rpc, &mut incoming, 0, ": i32").await;
    assert!(
        inlay_on_row(&params, 0).contains(&(5, ": i32".to_string())),
        "the resolved hint paints at screen col 5: {:?}",
        inlay_on_row(&params, 0)
    );
    // And it is readable through `get`.
    let (col, label) = wait_for_get(&rpc, "{ bufnr = 0 }").await;
    assert_eq!(
        (col, label.as_str()),
        (5, ": i32"),
        "the resolved hint reads back via get"
    );
}
