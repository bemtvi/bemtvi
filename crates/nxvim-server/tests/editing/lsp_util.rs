use crate::support::*;

// ----- Phase 7: vim.lsp.util.* real implementations -----------------------
//
// These exercise the helpers a config calls inside on_attach / handlers, driven
// through `nvim_exec_lua`. The param builders read the real cursor/buffer (the
// Phase-6 mirror) and convert byte columns to the LSP offset encoding; the editing
// helpers (`apply_workspace_edit`, `show_document`) queue an LspOp the server
// drains into the native workspace-edit / goto paths, so `lines` / `cursor` (native
// RPC reads of the real editor) independently confirm the effect landed.

#[tokio::test]
async fn make_position_params_reflects_the_cursor_and_encoding() {
    let (rpc, _incoming) = start(None).await;
    // "é" is 2 UTF-8 bytes / 1 UTF-16 unit, so the cursor on 'c' sits at byte
    // column 4 but UTF-16 character 3 — the two must not be conflated.
    feed(&rpc, "iéabc<Esc>");
    let utf16 = exec_lua(
        &rpc,
        r#"
        local p = vim.lsp.util.make_position_params(0, "utf-16")
        return p.position.line * 1000 + p.position.character
        "#,
    )
    .await;
    assert_eq!(utf16.as_u64(), Some(3), "line 0, UTF-16 character 3");
    let utf8 = exec_lua(
        &rpc,
        r#"return vim.lsp.util.make_position_params(0, "utf-8").position.character"#,
    )
    .await;
    assert_eq!(utf8.as_u64(), Some(4), "UTF-8 column is the byte index");
}

#[tokio::test]
async fn byte_to_position_char_handles_surrogate_pairs() {
    let (rpc, _incoming) = start(None).await;
    // A 4-byte char (😀) is a surrogate pair — 2 code units — under UTF-16, but a
    // single codepoint under UTF-32. Drive the helper on a Lua literal directly.
    let got = exec_lua(
        &rpc,
        r#"
        local s = "😀"
        return vim._byte_to_position_char(s, #s, "utf-16") * 10
             + vim._byte_to_position_char(s, #s, "utf-32")
        "#,
    )
    .await;
    assert_eq!(got.as_u64(), Some(2 * 10 + 1));
}

#[tokio::test]
async fn make_given_range_params_converts_marks_to_an_exclusive_range() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // Mark positions are { row (1-based), col (0-based byte) }; the end is made
    // exclusive (+1 char), matching neovim.
    let out = exec_lua(
        &rpc,
        r#"
        local r = vim.lsp.util.make_given_range_params({1, 0}, {1, 4}, 0, "utf-8").range
        return r.start.line * 1000 + r.start.character * 100 + r["end"].character
        "#,
    )
    .await;
    // Packed line*1000 + start_char*100 + end_char: start {line 0, char 0};
    // end char = 4 + 1 = 5 (exclusive). -> 5.
    assert_eq!(out.as_u64(), Some(5));
}

#[tokio::test]
async fn locations_to_items_builds_sorted_loclist_items() {
    let path = temp_path("loclist");
    std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();
    let (rpc, _incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    // Two locations, given out of order; the items come back sorted by position,
    // and the `text` is read from the open buffer backing the URI.
    let out = exec_lua(
        &rpc,
        r#"
        local uri = vim.uri_from_bufnr(0)
        local items = vim.lsp.util.locations_to_items({
          { uri = uri, range = { start = { line = 2, character = 0 }, ["end"] = { line = 2, character = 0 } } },
          { uri = uri, range = { start = { line = 0, character = 0 }, ["end"] = { line = 0, character = 0 } } },
        }, "utf-8")
        return items[1].lnum * 1000 + items[2].lnum * 10
             + (items[1].text == "alpha" and items[2].text == "gamma" and 1 or 0)
        "#,
    )
    .await;
    // Packed item1.lnum*1000 + item2.lnum*10 + texts_matched: sorted, item 1 ->
    // line 1 ("alpha"), item 2 -> line 3 ("gamma"). -> 1031.
    assert_eq!(out.as_u64(), Some(1031));
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn get_effective_tabstop_prefers_shiftwidth_then_tabstop() {
    let (rpc, _incoming) = start(None).await;
    // Defaults: shiftwidth=0 ("follow tabstop") + tabstop=4 -> 4.
    let dflt = exec_lua(&rpc, r#"return vim.lsp.util.get_effective_tabstop(0)"#).await;
    assert_eq!(dflt.as_u64(), Some(4));
    // A non-zero shiftwidth is preferred, even when tabstop differs.
    let sw = exec_lua(
        &rpc,
        r#"vim.bo.tabstop = 8; vim.bo.shiftwidth = 2; return vim.lsp.util.get_effective_tabstop(0)"#,
    )
    .await;
    assert_eq!(sw.as_u64(), Some(2));
    // shiftwidth=0 is the "follow tabstop" sentinel -> fall through to tabstop.
    let ts = exec_lua(
        &rpc,
        r#"vim.bo.shiftwidth = 0; return vim.lsp.util.get_effective_tabstop(0)"#,
    )
    .await;
    assert_eq!(ts.as_u64(), Some(8));
}

#[tokio::test]
async fn open_floating_preview_shows_content_in_the_panel() {
    let (rpc, mut incoming) = start(None).await;
    let map = latest_after(
        &rpc,
        &mut incoming,
        r#":lua vim.lsp.util.open_floating_preview({"preview one", "preview two"}, "markdown", {title = "Docs"})<CR>"#,
    )
    .await;
    assert_eq!(panel_title(&map), "Docs");
    let lines = panel_lines(&map);
    assert!(
        lines.contains(&"preview one".to_string()) && lines.contains(&"preview two".to_string()),
        "panel content was: {lines:?}"
    );
}

#[tokio::test]
async fn apply_workspace_edit_edits_the_open_buffer() {
    let path = temp_path("wsedit");
    std::fs::write(&path, "hello world\n").unwrap();
    let (rpc, _incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    // Replace "world" (chars 6..11 on line 0) with "neovim" via a WorkspaceEdit,
    // routed through the native apply path. The buffer has no attached server, so
    // its URI resolves by canonicalized path and the encoding is UTF-8 (char == byte).
    exec_lua(
        &rpc,
        r#"
        local uri = vim.uri_from_bufnr(0)
        vim.lsp.util.apply_workspace_edit({
          changes = {
            [uri] = {
              { range = { start = { line = 0, character = 6 },
                          ["end"] = { line = 0, character = 11 } },
                newText = "neovim" },
            },
          },
        })
        "#,
    )
    .await;
    assert_eq!(lines(&rpc).await, vec!["hello neovim"]);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn show_document_jumps_the_cursor_to_the_location() {
    let path = temp_path("showdoc");
    std::fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();
    let (rpc, _incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    exec_lua(
        &rpc,
        r#"
        local uri = vim.uri_from_bufnr(0)
        vim.lsp.util.show_document(
          { uri = uri, range = { start = { line = 2, character = 0 },
                                 ["end"] = { line = 2, character = 0 } } },
          "utf-8")
        "#,
    )
    .await;
    // Jumped to line 3 (1-based), column 0.
    assert_eq!(cursor(&rpc).await, (3, 0));
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn show_document_external_location_raises() {
    let (rpc, _incoming) = start(None).await;
    // An `external = true` location has no nxvim surface, so it must fail loud
    // rather than silently no-op (the no-silent-stubs rule).
    let ok = exec_lua(
        &rpc,
        r#"return pcall(vim.lsp.util.show_document, { uri = "https://example.com", external = true })"#,
    )
    .await;
    assert_eq!(
        ok.as_bool(),
        Some(false),
        "external show_document must raise"
    );
}

// ---- Phase 0: ex range parsing -------------------------------------------
//
// A bare range (no command) resolves and moves the cursor to the *last*
// address, landing on its first non-blank — vim's behavior for `:5<CR>`,
// `:1,5<CR>`, `:%<CR>`. These exercise the range parser without `:s` yet.
