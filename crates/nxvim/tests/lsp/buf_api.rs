//! LSP Phase 7b Slice 1: `vim.lsp.buf.*`.
//!
//! The Lua entry points route through the same native `request_lsp*` paths the
//! built-in keymaps/ex-commands use. These tests prove that route end-to-end by
//! driving each feature through a *Lua-set* keymap (a non-default key, so the
//! trigger is unambiguously `vim.lsp.buf.*` and not nxvim's native `gd`/`K`
//! defaults) — the `on_attach`-style call site real configs use.

use crate::support::*;

#[tokio::test]
async fn vim_lsp_buf_definition_jumps_via_a_lua_set_keymap() {
    let _guard = test_lock().lock().await;
    // A user maps `<Space>d` to `vim.lsp.buf.definition` (the on_attach pattern).
    // Pressing it enqueues an `LspOp::BufRequest` the server applies on the same
    // tick, issuing the request and jumping the cursor to the reply's location.
    let file = temp_file("buf-def", "rs", "fn target() {}\nfn main() { target() }\n");
    let record = configure_mock(
        "buf-def",
        serde_json::json!({ "definition": location(&file, 0, 3) }),
    );
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    cmd(
        &rpc,
        "lua vim.keymap.set('n', '<Space>d', vim.lsp.buf.definition)",
    )
    .await;

    // From the call site (line 1), the Lua-set key jumps to the definition.
    feed(&rpc, "j d");
    wait_for_cursor(&rpc, (1, 3)).await;
    assert!(
        has_method(&record_lines(&record), "textDocument/definition"),
        "vim.lsp.buf.definition should send a textDocument/definition request"
    );
}

#[tokio::test]
async fn vim_lsp_buf_references_opens_the_panel_via_lua() {
    let _guard = test_lock().lock().await;
    // `vim.lsp.buf.references` routes to the references list — always a panel,
    // navigable with `<CR>`, exactly like the native `gr`.
    let file = temp_file("buf-ref", "rs", "let x = 1\nlet y = x\nlet z = x\n");
    let record = configure_mock(
        "buf-ref",
        serde_json::json!({ "references": [location(&file, 1, 8), location(&file, 2, 8)] }),
    );
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    cmd(
        &rpc,
        "lua vim.keymap.set('n', '<Space>r', vim.lsp.buf.references)",
    )
    .await;

    feed(&rpc, " r");
    let (title, panel_lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP references");
    assert_eq!(
        panel_lines.len(),
        2,
        "one row per reference: {panel_lines:?}"
    );
    assert!(
        has_method(&record_lines(&record), "textDocument/references"),
        "vim.lsp.buf.references should send a textDocument/references request"
    );

    // `<CR>` on the first row jumps to it: 1-based line 2, byte col 8.
    feed(&rpc, "<CR>");
    wait_for_cursor(&rpc, (2, 8)).await;
}

#[tokio::test]
async fn vim_lsp_buf_hover_shows_text_via_lua() {
    let _guard = test_lock().lock().await;
    // `vim.lsp.buf.hover` opens the same panel `K` does, with the markup rendered
    // as plain lines.
    let file = temp_file("buf-hover", "rs", "fn target() {}\n");
    let record = configure_mock(
        "buf-hover",
        serde_json::json!({
            "hover": {
                "contents": { "kind": "markdown", "value": "fn target()\n" }
            }
        }),
    );
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    cmd(
        &rpc,
        "lua vim.keymap.set('n', '<Space>h', vim.lsp.buf.hover)",
    )
    .await;

    feed(&rpc, " h");
    let (title, panel_lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP hover");
    assert_eq!(panel_lines, vec!["fn target()".to_string()]);
    assert!(
        has_method(&record_lines(&record), "textDocument/hover"),
        "vim.lsp.buf.hover should send a textDocument/hover request"
    );
}

#[tokio::test]
async fn vim_lsp_buf_rename_applies_a_cross_buffer_edit_via_lua() {
    let _guard = test_lock().lock().await;
    // `vim.lsp.buf.rename('xyz')` carries the new name (nxvim requires it — no
    // prompt UI) and applies the returned WorkspaceEdit across every open buffer,
    // the same path `:LspRename` drives.
    let file_a = temp_file("buf-rename-a", "rs", "foo = foo\n");
    let file_b = temp_file("buf-rename-b", "rs", "use foo\n");
    let record = configure_mock(
        "buf-rename",
        serde_json::json!({
            "rename": ws_changes(&[
                (&file_a, vec![text_edit(0, 0, 0, 3, "xyz"), text_edit(0, 6, 0, 9, "xyz")]),
                (&file_b, vec![text_edit(0, 4, 0, 7, "xyz")]),
            ])
        }),
    );
    let (rpc, _incoming) = start(Some(file_a)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    let buf_a = current_buf(&rpc).await;

    // Open B so the rename can reach it, then return to A.
    cmd(&rpc, &format!("e {file_b}")).await;
    wait_for_record(&rpc, &record, |r| {
        count_method(r, "textDocument/didOpen") >= 2
    })
    .await;
    let buf_b = current_buf(&rpc).await;
    set_buf(&rpc, buf_a).await;

    // Cursor at the start of A; rename foo → xyz through the Lua entry point.
    feed(&rpc, "gg0");
    cmd(&rpc, "lua vim.lsp.buf.rename('xyz')").await;
    wait_for_lines(&rpc, &["xyz = xyz"]).await;
    assert_eq!(
        lines_of_buf(&rpc, buf_b).await,
        vec!["use xyz".to_string()],
        "vim.lsp.buf.rename reached the other open buffer"
    );
    assert!(
        has_method(&record_lines(&record), "textDocument/rename"),
        "vim.lsp.buf.rename should send a textDocument/rename request"
    );
}
