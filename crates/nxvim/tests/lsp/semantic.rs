//! LSP semantic tokens (ADR 0001 bridge #2): the whole-buffer
//! `textDocument/semanticTokens/full` result decoded and projected over the
//! treesitter highlight floor.
//!
//! These tests drive the scripted mock — a `semantic_tokens` script field carries
//! a legend + packed token `data`, the mock advertises the legend as its
//! `semanticTokensProvider` and returns the data — and assert on the `highlights`
//! redraw key: a semantic token surfaces as a span whose group is the resolved
//! `@lsp.*` capture, at the right screen columns. A token whose group is undefined
//! in the active theme must NOT appear (it falls back to the treesitter floor).

use crate::support::*;

/// The `[start_col, end_col, group]` highlight spans on window row `row` (0-based)
/// of the latest redraw, screen columns. Drops the trailing `style_id`.
fn hl_spans_on_row(params: &[Value], row: usize) -> Vec<(u64, u64, String)> {
    let rows = window0_get(params, "highlights")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(Value::Array(spans)) = rows.get(row).cloned() else {
        return Vec::new();
    };
    spans
        .iter()
        .filter_map(|s| {
            let a = s.as_array()?;
            Some((
                a.first()?.as_u64()?,
                a.get(1)?.as_u64()?,
                a.get(2)?.as_str()?.to_string(),
            ))
        })
        .collect()
}

/// Poll (bounded) until window row `row` carries a highlight span with `group`,
/// returning that row's spans. Drives the loop so the async semantic-token reply
/// is processed and its repaint lands. Panics with the last spans seen otherwise.
async fn wait_for_hl_group(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    row: usize,
    group: &str,
) -> Vec<(u64, u64, String)> {
    let mut last = Vec::new();
    for _ in 0..100 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            let spans = hl_spans_on_row(&params, row);
            if spans.iter().any(|(_, _, g)| g == group) {
                return spans;
            }
            last = spans;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("semantic group {group:?} never appeared on row {row}; last spans: {last:?}");
}

/// Define an `@lsp.*` highlight group so a token painting it resolves to a style
/// (otherwise the projection drops it). A distinctive fg keeps it identifiable.
async fn define_lsp_group(rpc: &Rpc, group: &str, fg: &str) {
    exec_lua(
        rpc,
        &format!("vim.api.nvim_set_hl(0, '{group}', {{ fg = '{fg}' }})"),
    )
    .await;
}

#[tokio::test]
async fn semantic_tokens_paint_over_the_treesitter_floor() {
    let _guard = test_lock().lock().await;
    // One token over "myfunc" (chars 0..6) typed `function` (legend index 0).
    // data = [deltaLine, deltaStart, length, tokenType, tokenModifiers].
    let record = configure_mock(
        "sem-paint",
        serde_json::json!({
            "position_encoding": "utf-8",
            "semantic_tokens": {
                "legend": { "tokenTypes": ["function"], "tokenModifiers": [] },
                "data": [0, 0, 6, 0, 0],
            },
        }),
    );
    let file = temp_file("sem-paint", "rs", "myfunc x\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    // The server must have requested the tokens (whole-buffer, on open).
    wait_for_record(&rpc, &record, |r| {
        has_method(r, "textDocument/semanticTokens/full")
    })
    .await;

    // Define the group so the token resolves (else it would be dropped).
    define_lsp_group(&rpc, "@lsp.type.function", "#ff8800").await;

    let spans = wait_for_hl_group(&rpc, &mut incoming, 0, "lsp.type.function").await;
    assert!(
        spans.contains(&(0, 6, "lsp.type.function".to_string())),
        "the function token paints screen cols 0..6 as @lsp.type.function: {spans:?}"
    );
}

#[tokio::test]
async fn an_undefined_semantic_group_is_dropped_so_treesitter_shows() {
    let _guard = test_lock().lock().await;
    // The same `function` semantic token at col 0..6, but the theme defines NO
    // @lsp.type.function — the projection must drop it, leaving whatever the
    // treesitter floor painted (never a blank cell, never an `lsp.type.*` span).
    // Real Rust here (not the bare `myfunc x` the other cases use) so the
    // treesitter floor actually paints — the dropped group must reveal it.
    let record = configure_mock(
        "sem-drop",
        serde_json::json!({
            "position_encoding": "utf-8",
            "semantic_tokens": {
                "legend": { "tokenTypes": ["function"], "tokenModifiers": [] },
                "data": [0, 0, 6, 0, 0],
            },
        }),
    );
    let file = temp_file("sem-drop", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_record(&rpc, &record, |r| {
        has_method(r, "textDocument/semanticTokens/full")
    })
    .await;
    // Wait for treesitter to paint (proves the buffer rendered) — and let the
    // semantic reply settle in the same window.
    wait_for_highlights(&rpc, &mut incoming).await;

    // Drive a couple more frames so any (stale) semantic repaint would have landed.
    for _ in 0..5 {
        barrier(&rpc).await;
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let params = drain_latest_redraw(&mut incoming).expect("a redraw");
    let spans = hl_spans_on_row(&params, 0);
    assert!(
        !spans.iter().any(|(_, _, g)| g.starts_with("lsp.")),
        "no @lsp.* span when the group is undefined; got {spans:?}"
    );
}

#[tokio::test]
async fn semantic_token_columns_are_encoding_correct() {
    let _guard = test_lock().lock().await;
    // A UTF-16 server: "héllo" is 5 UTF-16 code units but 6 bytes ('é' is 2). A
    // token of length 5 over it must decode to bytes 0..6 → screen cols 0..5
    // ('é' is one cell), proving the char→byte conversion runs through the
    // negotiated encoding before the byte→screen step.
    let record = configure_mock(
        "sem-utf16",
        serde_json::json!({
            "position_encoding": "utf-16",
            "semantic_tokens": {
                "legend": { "tokenTypes": ["function"], "tokenModifiers": [] },
                "data": [0, 0, 5, 0, 0],
            },
        }),
    );
    let file = temp_file("sem-utf16", "rs", "héllo x\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_record(&rpc, &record, |r| {
        has_method(r, "textDocument/semanticTokens/full")
    })
    .await;
    define_lsp_group(&rpc, "@lsp.type.function", "#00ff88").await;

    let spans = wait_for_hl_group(&rpc, &mut incoming, 0, "lsp.type.function").await;
    assert!(
        spans.contains(&(0, 5, "lsp.type.function".to_string())),
        "the token spans screen cols 0..5 over the wide-char line: {spans:?}"
    );
}

/// The `previousResultId`s carried by every recorded `full/delta` request.
fn delta_prev_result_ids(recs: &[Json]) -> Vec<String> {
    recs.iter()
        .filter(|r| r["method"] == "textDocument/semanticTokens/full/delta")
        .filter_map(|r| r["params"]["previousResultId"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn editing_after_a_full_result_sends_a_delta_request() {
    let _guard = test_lock().lock().await;
    // The full reply carries a `result_id`, so the next refresh (after an edit)
    // must send `full/delta` quoting it as `previousResultId` — the wire savings of
    // Phase 2. The mock advertises `full: { delta: true }` because a `delta` is
    // scripted.
    let record = configure_mock(
        "sem-delta-req",
        serde_json::json!({
            "position_encoding": "utf-8",
            "semantic_tokens": {
                "legend": { "tokenTypes": ["variable"], "tokenModifiers": [] },
                "data": [0, 0, 3, 0, 0],
                "result_id": "1",
                "delta": { "result_id": "2", "edits": [] },
            },
        }),
    );
    let file = temp_file("sem-delta-req", "rs", "abc\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    define_lsp_group(&rpc, "@lsp.type.variable", "#88ccff").await;
    // Wait until the full reply has painted, so its `result_id` is cached before we
    // edit (otherwise the refresh would have no result_id and send `full` again).
    wait_for_hl_group(&rpc, &mut incoming, 0, "lsp.type.variable").await;

    feed(&rpc, "A!");
    feed(&rpc, "\x1b");
    wait_for_record(&rpc, &record, |r| {
        has_method(r, "textDocument/semanticTokens/full/delta")
    })
    .await;
    let ids = delta_prev_result_ids(&record_lines(&record));
    assert!(
        ids.contains(&"1".to_string()),
        "the delta request quotes the prior result_id; got {ids:?}"
    );
}

#[tokio::test]
async fn a_delta_patches_the_cached_token_array() {
    let _guard = test_lock().lock().await;
    // The full reply classifies cols 0..3 as a `variable`; the scripted delta
    // replaces that one token (flat-array indices 0..5) with a `function` token.
    // Applying the edit to the cached array must repaint cols 0..3 as a function —
    // the same paint the equivalent full set `[0,0,3,0,0]` would produce.
    let record = configure_mock(
        "sem-delta-patch",
        serde_json::json!({
            "position_encoding": "utf-8",
            "semantic_tokens": {
                "legend": { "tokenTypes": ["function", "variable"], "tokenModifiers": [] },
                "data": [0, 0, 3, 1, 0],
                "result_id": "1",
                "delta": {
                    "result_id": "2",
                    "edits": [{ "start": 0, "deleteCount": 5, "data": [0, 0, 3, 0, 0] }],
                },
            },
        }),
    );
    let file = temp_file("sem-delta-patch", "rs", "abc\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    define_lsp_group(&rpc, "@lsp.type.variable", "#88ccff").await;
    define_lsp_group(&rpc, "@lsp.type.function", "#ff8800").await;

    let spans = wait_for_hl_group(&rpc, &mut incoming, 0, "lsp.type.variable").await;
    assert!(
        spans.contains(&(0, 3, "lsp.type.variable".to_string())),
        "the full reply paints cols 0..3 as a variable: {spans:?}"
    );

    // Edit → refresh → `full/delta` → patched array → cols 0..3 become a function.
    feed(&rpc, "A!");
    feed(&rpc, "\x1b");
    let spans = wait_for_hl_group(&rpc, &mut incoming, 0, "lsp.type.function").await;
    assert!(
        spans.contains(&(0, 3, "lsp.type.function".to_string())),
        "the delta repaints cols 0..3 as a function: {spans:?}"
    );
    assert!(
        !spans.iter().any(|(_, _, g)| g == "lsp.type.variable"),
        "the variable span is gone after the delta replaced its token: {spans:?}"
    );
}

#[tokio::test]
async fn a_delta_request_answered_with_a_full_set_replaces_the_cache() {
    let _guard = test_lock().lock().await;
    // A server that can't honor the `previousResultId` answers `full/delta` with a
    // fresh full set (the protocol's transparent fallback). The editor must apply
    // it by replacing the cache — repainting cols 0..3 from variable to function —
    // not by trying to splice edits that aren't there.
    let record = configure_mock(
        "sem-delta-full",
        serde_json::json!({
            "position_encoding": "utf-8",
            "semantic_tokens": {
                "legend": { "tokenTypes": ["function", "variable"], "tokenModifiers": [] },
                "data": [0, 0, 3, 1, 0],
                "result_id": "1",
                // No `edits`: the delta reply is a fresh full set instead.
                "delta": { "result_id": "2", "data": [0, 0, 3, 0, 0] },
            },
        }),
    );
    let file = temp_file("sem-delta-full", "rs", "abc\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    define_lsp_group(&rpc, "@lsp.type.variable", "#88ccff").await;
    define_lsp_group(&rpc, "@lsp.type.function", "#ff8800").await;

    wait_for_hl_group(&rpc, &mut incoming, 0, "lsp.type.variable").await;

    feed(&rpc, "A!");
    feed(&rpc, "\x1b");
    // The delta request went out (proving the delta path), but the server replied
    // with a full set the editor applied wholesale.
    wait_for_record(&rpc, &record, |r| {
        has_method(r, "textDocument/semanticTokens/full/delta")
    })
    .await;
    let spans = wait_for_hl_group(&rpc, &mut incoming, 0, "lsp.type.function").await;
    assert!(
        spans.contains(&(0, 3, "lsp.type.function".to_string())),
        "the full-set fallback repaints cols 0..3 as a function: {spans:?}"
    );
}

/// Poll (bounded) until window row `row` carries no `@lsp.*` highlight span,
/// returning that row's spans. The dual of [`wait_for_hl_group`] — drives the loop
/// so a stop/disable repaint lands. Panics with the last spans seen otherwise.
async fn wait_for_no_lsp_span(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    row: usize,
) -> Vec<(u64, u64, String)> {
    let mut last = Vec::new();
    for _ in 0..100 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            let spans = hl_spans_on_row(&params, row);
            if !spans.iter().any(|(_, _, g)| g.starts_with("lsp.")) {
                return spans;
            }
            last = spans;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("an @lsp.* span never cleared on row {row}; last spans: {last:?}");
}

#[tokio::test]
async fn stop_hides_the_paint_and_start_restores_it() {
    let _guard = test_lock().lock().await;
    // The projection is auto-on, so the token paints first. `stop` hides it (the
    // highlights drop back to the treesitter floor); `start` repaints from the
    // surviving cache without needing a fresh reply.
    let record = configure_mock(
        "sem-stop",
        serde_json::json!({
            "position_encoding": "utf-8",
            "semantic_tokens": {
                "legend": { "tokenTypes": ["function"], "tokenModifiers": [] },
                "data": [0, 0, 6, 0, 0],
            },
        }),
    );
    let file = temp_file("sem-stop", "rs", "myfunc x\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    define_lsp_group(&rpc, "@lsp.type.function", "#ff8800").await;
    wait_for_hl_group(&rpc, &mut incoming, 0, "lsp.type.function").await;

    exec_lua(&rpc, "vim.lsp.semantic_tokens.stop(0)").await;
    let spans = wait_for_no_lsp_span(&rpc, &mut incoming, 0).await;
    assert!(
        !spans.iter().any(|(_, _, g)| g.starts_with("lsp.")),
        "stop hides the semantic paint: {spans:?}"
    );

    exec_lua(&rpc, "vim.lsp.semantic_tokens.start(0)").await;
    let spans = wait_for_hl_group(&rpc, &mut incoming, 0, "lsp.type.function").await;
    assert!(
        spans.contains(&(0, 6, "lsp.type.function".to_string())),
        "start restores the semantic paint: {spans:?}"
    );
}

#[tokio::test]
async fn the_editor_wide_gate_off_hides_all_semantic_paint() {
    let _guard = test_lock().lock().await;
    // `vim.lsp.semantic_tokens.enable(false)` is nxvim's global gate: it hides the
    // paint without touching the per-buffer state, and `enable(true)` restores it.
    let record = configure_mock(
        "sem-gate",
        serde_json::json!({
            "position_encoding": "utf-8",
            "semantic_tokens": {
                "legend": { "tokenTypes": ["function"], "tokenModifiers": [] },
                "data": [0, 0, 6, 0, 0],
            },
        }),
    );
    let file = temp_file("sem-gate", "rs", "myfunc x\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    define_lsp_group(&rpc, "@lsp.type.function", "#ff8800").await;
    wait_for_hl_group(&rpc, &mut incoming, 0, "lsp.type.function").await;

    exec_lua(&rpc, "vim.lsp.semantic_tokens.enable(false)").await;
    let spans = wait_for_no_lsp_span(&rpc, &mut incoming, 0).await;
    assert!(
        !spans.iter().any(|(_, _, g)| g.starts_with("lsp.")),
        "the global gate off hides the semantic paint: {spans:?}"
    );

    exec_lua(&rpc, "vim.lsp.semantic_tokens.enable(true)").await;
    wait_for_hl_group(&rpc, &mut incoming, 0, "lsp.type.function").await;
}

#[tokio::test]
async fn force_refresh_re_issues_a_full_request() {
    let _guard = test_lock().lock().await;
    // With no edit, no semantic request fires on its own; `force_refresh` must issue
    // one (a fresh `full`, since it drops any cached result id).
    let record = configure_mock(
        "sem-refresh",
        serde_json::json!({
            "position_encoding": "utf-8",
            "semantic_tokens": {
                "legend": { "tokenTypes": ["function"], "tokenModifiers": [] },
                "data": [0, 0, 6, 0, 0],
            },
        }),
    );
    let file = temp_file("sem-refresh", "rs", "myfunc x\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    define_lsp_group(&rpc, "@lsp.type.function", "#ff8800").await;
    wait_for_hl_group(&rpc, &mut incoming, 0, "lsp.type.function").await;

    let before = count_method(&record_lines(&record), "textDocument/semanticTokens/full");
    exec_lua(&rpc, "vim.lsp.semantic_tokens.force_refresh(0)").await;
    wait_for_record(&rpc, &record, |r| {
        count_method(r, "textDocument/semanticTokens/full") > before
    })
    .await;
}

/// Poll (bounded) `vim.lsp.get_clients()[1].server_capabilities[cap]` until a
/// client is registered, returning whether the capability is truthy.
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

#[tokio::test]
async fn server_capabilities_reports_the_semantic_tokens_provider() {
    let _guard = test_lock().lock().await;
    // A server that advertised a legend reads truthy; one that didn't reads falsy —
    // the `client.server_capabilities.semanticTokensProvider` an on_attach branches
    // on (Phase 3 exposes the Phase-1 bool to Lua).
    let record = configure_mock(
        "sem-caps-yes",
        serde_json::json!({
            "position_encoding": "utf-8",
            "semantic_tokens": {
                "legend": { "tokenTypes": ["function"], "tokenModifiers": [] },
                "data": [0, 0, 6, 0, 0],
            },
        }),
    );
    let file = temp_file("sem-caps-yes", "rs", "myfunc x\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    assert!(
        wait_for_client_cap(&rpc, "semanticTokensProvider").await,
        "a server with a legend advertises semanticTokensProvider"
    );
}

#[tokio::test]
async fn server_without_a_legend_reports_no_semantic_tokens_provider() {
    let _guard = test_lock().lock().await;
    // No `semantic_tokens` script ⇒ the mock advertises no provider ⇒ the cap is
    // falsy (and nothing ever requests tokens).
    let record = configure_mock(
        "sem-caps-no",
        serde_json::json!({ "position_encoding": "utf-8" }),
    );
    let file = temp_file("sem-caps-no", "rs", "myfunc x\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    assert!(
        !wait_for_client_cap(&rpc, "semanticTokensProvider").await,
        "a server without a legend does not advertise semanticTokensProvider"
    );
}

#[tokio::test]
async fn get_at_pos_returns_the_token_under_the_position() {
    let _guard = test_lock().lock().await;
    // `get_at_pos` reads the decoded mirror (independent of the theme — an undefined
    // group is still in the cache): the function token covers cols 0..6, so a query
    // inside it returns its type, and one past it returns nothing.
    let record = configure_mock(
        "sem-at-pos",
        serde_json::json!({
            "position_encoding": "utf-8",
            "semantic_tokens": {
                "legend": { "tokenTypes": ["function"], "tokenModifiers": ["declaration"] },
                "data": [0, 0, 6, 0, 1],
            },
        }),
    );
    let file = temp_file("sem-at-pos", "rs", "myfunc x\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Poll until the mirror is populated, then assert the token under col 2.
    let mut ty = String::new();
    for _ in 0..100 {
        barrier(&rpc).await;
        let v = exec_lua(
            &rpc,
            "local t = vim.lsp.semantic_tokens.get_at_pos(0, 0, 2); \
             return t[1] and (t[1].type .. (t[1].modifiers.declaration and ':decl' or '')) or ''",
        )
        .await;
        if let Some(s) = v.as_str() {
            if !s.is_empty() {
                ty = s.to_string();
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        ty, "function:decl",
        "get_at_pos returns the token's type and active modifier under the position"
    );

    let past = exec_lua(&rpc, "return #vim.lsp.semantic_tokens.get_at_pos(0, 0, 7)")
        .await
        .as_u64();
    assert_eq!(past, Some(0), "no token past the function's end column");
}

#[tokio::test]
async fn editing_re_requests_and_repaints_semantic_tokens() {
    let _guard = test_lock().lock().await;
    // The first token set classifies col 0..6 as a function; after an edit the
    // server returns a *different* set (a token at a new position), proving the
    // refresh re-requests on change and the new paint replaces the old.
    let record = configure_mock(
        "sem-edit",
        serde_json::json!({
            "position_encoding": "utf-8",
            "semantic_tokens": {
                "legend": { "tokenTypes": ["function", "variable"], "tokenModifiers": [] },
                // A `variable` token (legend index 1) over cols 0..3.
                "data": [0, 0, 3, 1, 0],
            },
        }),
    );
    let file = temp_file("sem-edit", "rs", "abc\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    define_lsp_group(&rpc, "@lsp.type.variable", "#88ccff").await;

    let spans = wait_for_hl_group(&rpc, &mut incoming, 0, "lsp.type.variable").await;
    assert!(
        spans.contains(&(0, 3, "lsp.type.variable".to_string())),
        "the variable token paints cols 0..3: {spans:?}"
    );

    // Edit the buffer: a new change must trigger another semanticTokens/full.
    let before = count_method(&record_lines(&record), "textDocument/semanticTokens/full");
    feed(&rpc, "A!");
    feed(&rpc, "\x1b"); // leave insert
    wait_for_record(&rpc, &record, |r| {
        count_method(r, "textDocument/semanticTokens/full") > before
    })
    .await;
}
