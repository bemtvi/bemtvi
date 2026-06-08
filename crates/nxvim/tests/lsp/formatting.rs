//! LSP Phase 6: formatting, rename, and code actions — plus the workspace-root
//! env knob, symlink-canonicalized rename matching, and a late `vim.lsp.enable`.

use crate::support::*;

#[tokio::test]
async fn lsp_format_rewrites_the_buffer_and_is_idempotent() {
    let _guard = test_lock().lock().await;
    // The mock returns a whole-line replacement; `:LspFormat` rewrites the buffer.
    // The edit replaces line 0 incl. its newline ((0,0)..(1,0)) with the canonical
    // text, so a re-run on the already-formatted line is a no-op (idempotent).
    let record = configure_mock(
        "fmt",
        serde_json::json!({ "formatting": [text_edit(0, 0, 1, 0, "let x = 1;\n")] }),
    );
    let file = temp_file("fmt", "rs", "let x=1;\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    cmd(&rpc, "LspFormat").await;
    wait_for_lines(&rpc, &["let x = 1;"]).await;

    // Re-format: the line is already canonical, so it stays unchanged.
    cmd(&rpc, "LspFormat").await;
    for _ in 0..6 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        lines(&rpc).await,
        vec!["let x = 1;".to_string()],
        "re-formatting already-formatted text is a no-op"
    );
}

#[tokio::test]
async fn a_formatting_reply_is_dropped_after_an_intervening_edit() {
    let _guard = test_lock().lock().await;
    // Content-version guard: the formatting reply is delayed; an edit lands before
    // it does, so applying the (now stale, whole-document) edit would corrupt the
    // buffer. The reply must be dropped, leaving the user's edit intact.
    let record = configure_mock(
        "fmt-stale",
        serde_json::json!({
            "formatting": [text_edit(0, 0, 1, 0, "FORMATTED\n")],
            "reply_delay_ms": 200,
        }),
    );
    let file = temp_file("fmt-stale", "rs", "original\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Fire format (the mock sleeps before replying), then edit before it lands.
    cmd(&rpc, "LspFormat").await;
    feed(&rpc, "A!<Esc>");
    barrier(&rpc).await;
    // The request really went out (so the drop path is exercised, not a no-op),
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/formatting")).await;
    // and after the reply delay elapses the stale reply has been dropped.
    for _ in 0..10 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        lines(&rpc).await,
        vec!["original!".to_string()],
        "the late formatting reply was dropped; the user's edit stands"
    );
}

#[tokio::test]
async fn rename_applies_a_workspace_edit_across_open_buffers() {
    let _guard = test_lock().lock().await;
    // Rename returns a two-file WorkspaceEdit; both open buffers change, each is
    // independently undoable, and the active buffer's cursor survives.
    let file_a = temp_file("rename-a", "rs", "foo = foo\n");
    let file_b = temp_file("rename-b", "rs", "use foo\n");
    let record = configure_mock(
        "rename",
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

    // Open B (so the rename can reach it), wait for its didOpen, then back to A.
    cmd(&rpc, &format!("e {file_b}")).await;
    wait_for_record(&rpc, &record, |r| {
        count_method(r, "textDocument/didOpen") >= 2
    })
    .await;
    let buf_b = current_buf(&rpc).await;
    set_buf(&rpc, buf_a).await;

    // Cursor at the start of A; rename foo → xyz.
    feed(&rpc, "gg0");
    cmd(&rpc, "LspRename xyz").await;
    wait_for_lines(&rpc, &["xyz = xyz"]).await;

    // The other open buffer changed too (read by handle, no switch).
    assert_eq!(
        lines_of_buf(&rpc, buf_b).await,
        vec!["use xyz".to_string()],
        "the rename reached the other open buffer"
    );
    // The active buffer's cursor survived at a valid resting cell.
    assert_eq!(cursor(&rpc).await, (1, 0), "the active cursor survived");

    // Undo on A reverts only A; B is untouched (independent undo histories).
    feed(&rpc, "u");
    assert_eq!(lines(&rpc).await, vec!["foo = foo".to_string()]);
    assert_eq!(
        lines_of_buf(&rpc, buf_b).await,
        vec!["use xyz".to_string()],
        "B is unaffected by A's undo"
    );
    // Switch to B and undo there.
    set_buf(&rpc, buf_b).await;
    feed(&rpc, "u");
    assert_eq!(lines(&rpc).await, vec!["use foo".to_string()]);
}

#[tokio::test]
async fn lsp_rename_with_no_name_prompts_prefilled_with_the_cword() {
    let _guard = test_lock().lock().await;
    // `:LspRename` with no argument now prompts (vim.ui.input) instead of erroring,
    // prefilled with the symbol under the cursor — so the prompt is editable and
    // the typed name reaches `textDocument/rename`.
    let file = temp_file("rename-prompt", "rs", "foo = foo\n");
    let record = configure_mock(
        "rename-prompt",
        serde_json::json!({
            "rename": ws_changes(&[(
                &file,
                vec![text_edit(0, 0, 0, 3, "X"), text_edit(0, 6, 0, 9, "X")],
            )])
        }),
    );
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Cursor on the `foo` symbol; `:LspRename` (no arg) opens the prompt prefilled
    // with the cword "foo". Append "bar" and submit.
    feed(&rpc, "gg0");
    cmd(&rpc, "LspRename").await;
    feed(&rpc, "bar<CR>");

    let recs = wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/rename")).await;
    let req = find(&recs, "textDocument/rename").expect("the rename request went out");
    assert_eq!(
        req["params"]["newName"].as_str(),
        Some("foobar"),
        "the prompt prefilled the cword (foo) and the appended text rode along: {req:?}"
    );
    // The server's WorkspaceEdit reply still applies.
    wait_for_lines(&rpc, &["X = X"]).await;
}

#[tokio::test]
async fn vim_lsp_buf_rename_no_arg_prompts_via_lua() {
    let _guard = test_lock().lock().await;
    // The bare-RHS form `vim.lsp.buf.rename()` (no name) prompts too — the path a
    // `vim.keymap.set('n', '<leader>rn', vim.lsp.buf.rename)` mapping takes.
    let file = temp_file("rename-lua-prompt", "rs", "foo = foo\n");
    let record = configure_mock(
        "rename-lua-prompt",
        serde_json::json!({
            "rename": ws_changes(&[(&file, vec![text_edit(0, 0, 0, 3, "Y"), text_edit(0, 6, 0, 9, "Y")])])
        }),
    );
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "gg0");
    cmd(&rpc, "lua vim.lsp.buf.rename()").await;
    // Accept the prefilled cword default unchanged.
    feed(&rpc, "<CR>");

    let recs = wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/rename")).await;
    let req = find(&recs, "textDocument/rename").expect("the rename request went out");
    assert_eq!(
        req["params"]["newName"].as_str(),
        Some("foo"),
        "the prompt defaulted to the cword: {req:?}"
    );
    wait_for_lines(&rpc, &["Y = Y"]).await;
}

#[tokio::test]
async fn a_code_action_lists_in_the_panel_and_applies_on_enter() {
    let _guard = test_lock().lock().await;
    // The code-action list opens in the panel; `<CR>` on a row applies that
    // action's eager edit (and no control key leaks a literal character).
    let file = temp_file("ca", "rs", "let x=1;\n");
    let edit = ws_changes(&[(&file, vec![text_edit(0, 0, 1, 0, "let x = 1;\n")])]);
    let record = configure_mock(
        "ca",
        serde_json::json!({
            "code_action": [{ "title": "Add spaces", "edit": edit }]
        }),
    );
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    cmd(&rpc, "LspCodeAction").await;
    let (title, panel_lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP code actions");
    assert_eq!(panel_lines, vec!["Add spaces".to_string()]);

    // `<CR>` applies the chosen action and closes the panel.
    feed(&rpc, "<CR>");
    wait_for_lines(&rpc, &["let x = 1;"]).await;
    assert!(
        !rpc.request("nxvim_panel_is_open", vec![])
            .await
            .unwrap()
            .as_bool()
            .unwrap(),
        "the code-action panel closed after applying"
    );
}

#[tokio::test]
async fn a_lazy_code_action_is_resolved_before_applying() {
    let _guard = test_lock().lock().await;
    // A lazy action arrives with no `edit` (only `data`); selecting it fires
    // `codeAction/resolve`, and the resolved edit (returned with the action) is
    // what gets applied.
    let file = temp_file("ca-resolve", "rs", "let x=1;\n");
    let resolved = serde_json::json!({
        "title": "Add spaces",
        "edit": ws_changes(&[(&file, vec![text_edit(0, 0, 1, 0, "let x = 1;\n")])]),
    });
    let record = configure_mock(
        "ca-resolve",
        serde_json::json!({
            // No `edit` here — only `data`, so the client must resolve it.
            "code_action": [{ "title": "Add spaces", "data": { "id": 1 } }],
            "code_action_resolve": resolved,
        }),
    );
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    cmd(&rpc, "LspCodeAction").await;
    let (_title, panel_lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(panel_lines, vec!["Add spaces".to_string()]);

    // `<CR>` resolves then applies; the resolve round-trip really happened.
    feed(&rpc, "<CR>");
    wait_for_lines(&rpc, &["let x = 1;"]).await;
    assert!(
        has_method(&record_lines(&record), "codeAction/resolve"),
        "the lazy action was resolved before applying"
    );
}

#[tokio::test]
async fn a_code_action_command_runs_via_execute_command() {
    let _guard = test_lock().lock().await;
    // A bare-`Command` code action carries no edit; applying it dispatches a
    // `workspace/executeCommand` to the server (Phase 8) rather than the old
    // "command unsupported" echo.
    let file = temp_file("ca-cmd", "rs", "fn main() {}\n");
    let record = configure_mock(
        "ca-cmd",
        serde_json::json!({
            "code_action": [{ "title": "Run it", "command": "mock.run", "arguments": ["a"] }],
            "custom_replies": { "workspace/executeCommand": { "ok": true } }
        }),
    );
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    cmd(&rpc, "LspCodeAction").await;
    let (_title, panel_lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(panel_lines, vec!["Run it".to_string()]);

    // `<CR>` applies the command action: it goes out as workspace/executeCommand.
    feed(&rpc, "<CR>");
    let recs = wait_for_record(&rpc, &record, |r| has_method(r, "workspace/executeCommand")).await;
    let req = find(&recs, "workspace/executeCommand").expect("the command reached the server");
    assert_eq!(
        req["params"]["command"].as_str(),
        Some("mock.run"),
        "the code-action command is dispatched: {req:?}"
    );
}

#[tokio::test]
async fn a_formatting_edit_lands_at_the_right_byte_with_utf16() {
    let _guard = test_lock().lock().await;
    // The edit analogue of the cross-file `é` test: a leading 2-byte `é` and a
    // utf-16 server. The edit's range is in utf-16 units (char 1..2 = the `x`
    // after `é`); applying it must convert to byte 2..3, so `x` → `X` lands as
    // `éX=1`, not corrupting the `é`.
    let record = configure_mock(
        "fmt-utf16",
        serde_json::json!({
            "position_encoding": "utf-16",
            "formatting": [text_edit(0, 1, 0, 2, "X")],
        }),
    );
    let file = temp_file("fmt-utf16", "rs", "éx=1\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    cmd(&rpc, "LspFormat").await;
    wait_for_lines(&rpc, &["éX=1"]).await;
}

#[tokio::test]
async fn format_and_rename_never_block_the_editor() {
    let _guard = test_lock().lock().await;
    // Resilience: format/rename requests whose server offers nothing (null replies)
    // leave the editor fully editable and the buffer unchanged.
    let record = configure_mock("edit-resil", serde_json::json!({}));
    let file = temp_file("edit-resil", "rs", "fn main() {}\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    cmd(&rpc, "LspFormat").await;
    cmd(&rpc, "LspRename bar").await;
    // Both requests were genuinely sent; their null replies applied nothing.
    wait_for_record(&rpc, &record, |r| {
        has_method(r, "textDocument/formatting") && has_method(r, "textDocument/rename")
    })
    .await;
    // The editor still edits.
    feed(&rpc, "ook<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {}".to_string(), "ok".to_string()],
        "the editor stays fully editable; the null edit replies changed nothing"
    );
}

#[tokio::test]
async fn a_formatting_edit_with_an_out_of_bounds_line_does_not_crash_the_editor() {
    let _guard = test_lock().lock().await;
    // SECURITY/ROBUSTNESS: a (malicious or buggy) server returns a formatting edit
    // whose range references a line far beyond the buffer's last line. The byte
    // conversion must clamp rather than index the rope out of bounds — an
    // unclamped `Position.line` reaches `line_start(row)` → ropey's
    // `assert!(line_idx <= len_lines)` and panics the single server thread,
    // taking the whole editor down. The editor must survive and stay editable.
    let record = configure_mock(
        "fmt-oob-line",
        serde_json::json!({
            // Buffer has 1 line; the edit's range starts on line 99.
            "formatting": [text_edit(99, 0, 99, 0, "INJECTED")],
        }),
    );
    let file = temp_file("fmt-oob-line", "rs", "let x = 1;\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    cmd(&rpc, "LspFormat").await;
    // Let the reply land and be applied.
    for _ in 0..10 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The editor is still alive and editable — it did not panic on the bad line.
    // The out-of-bounds position is clamped to end-of-document (a benign no-crash
    // outcome), so the inserted text lands at the buffer end rather than indexing
    // the rope out of bounds. The key property is that the editor survives and the
    // RPC connection stays up; a follow-up edit still works.
    feed(&rpc, "Gokeep<Esc>");
    let after = lines(&rpc).await;
    assert_eq!(
        after.first().map(String::as_str),
        Some("let x = 1;"),
        "the original line is intact; the editor survived: {after:?}"
    );
    assert_eq!(
        after.last().map(String::as_str),
        Some("keep"),
        "the editor is still editable after the malformed reply: {after:?}"
    );
}

/// Restores an env var to its prior value on drop, so a test that sets a
/// process-global env var leaves it as it found it even on a panic.
struct EnvGuard(&'static str, Option<std::ffi::OsString>);
impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        EnvGuard(key, prev)
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.1 {
            Some(v) => std::env::set_var(self.0, v),
            None => std::env::remove_var(self.0),
        }
    }
}

#[tokio::test]
async fn the_workspace_root_is_configurable_via_env() {
    let _guard = test_lock().lock().await;
    // `$NXVIM_LSP_ROOT` overrides the workspace root the client sends as `rootUri`
    // (and uses as the server's working dir) — the knob for pointing the editor at
    // a real project root when testing against a live server.
    let root = std::env::temp_dir().join(format!("nxvim-lsp-root-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let record = configure_mock("ws-root", serde_json::json!({}));
    let _env = EnvGuard::set("NXVIM_LSP_ROOT", &root);
    let file = temp_file("ws-root", "rs", "fn main() {}\n");
    let (rpc, _incoming) = start(Some(file)).await;

    let recs = wait_for_record(&rpc, &record, |r| has_method(r, "initialize")).await;
    let root_uri = find(&recs, "initialize").unwrap()["params"]["rootUri"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        root_uri,
        file_uri(root.to_str().unwrap()),
        "rootUri honors NXVIM_LSP_ROOT"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn rename_matches_a_buffer_opened_through_a_symlink() {
    let _guard = test_lock().lock().await;
    // A server may canonicalize symlinks in the URI it returns (e.g. macOS
    // `/var` → `/private/var`), so it differs from the URI we sent at `didOpen`.
    // The apply must still match the open buffer — by canonicalized path. The
    // buffer is opened via a symlink; the rename is keyed by the real path.
    let real = temp_file("rename-sym-real", "rs", "foo = 1\n");
    let link = std::env::temp_dir().join(format!("nxvim-lsp-sym-{}.rs", std::process::id()));
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let record = configure_mock(
        "rename-sym",
        serde_json::json!({
            "rename": ws_changes(&[(&real, vec![text_edit(0, 0, 0, 3, "bar")])])
        }),
    );
    let (rpc, _incoming) = start(Some(link.to_str().unwrap().to_string())).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "gg0");
    cmd(&rpc, "LspRename bar").await;
    wait_for_lines(&rpc, &["bar = 1"]).await;
    let _ = std::fs::remove_file(&link);
}

/// `vim.lsp.enable` called **interactively, after the buffer is already open**
/// must start the server for that buffer — not merely arm a `FileType` autocmd
/// for *future* buffers. Opening the file fires `FileType` once; a later
/// `:lua vim.lsp.enable(...)` used to no-op because the dispatcher caught nothing.
/// Mirrors neovim, whose `enable` processes already-loaded buffers on the spot.
#[tokio::test]
async fn enable_after_open_starts_the_server_for_the_current_buffer() {
    let _guard = test_lock().lock().await;
    let record = configure_mock("enable_late", serde_json::json!({}));
    let file = temp_file("enable_late", "rs", "fn main() {}\n");

    // A config dir that *defines* the mock but does not enable it, so opening the
    // rust buffer starts nothing — the enable is the interactive step below.
    let cfg = std::env::temp_dir().join(format!("nxvim-lsp-cfg-late-{}", std::process::id()));
    std::fs::create_dir_all(&cfg).expect("create config dir");
    std::fs::write(
        cfg.join("init.lua"),
        "vim.lsp.config('mock', { cmd = { 'mock' }, filetypes = { 'rust' } })\n",
    )
    .expect("write init.lua");

    let (rpc, _incoming) = start_with_config_dir(Some(file), cfg).await;

    // Nothing enabled yet: the open rust buffer must not have started a server.
    for _ in 0..4 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        record_lines(&record).is_empty(),
        "no server should start before enable, got {:?}",
        record_lines(&record)
    );

    // Enable interactively, after the buffer's FileType already fired. The server
    // must start for the current buffer and run the handshake + didOpen.
    cmd(&rpc, "lua vim.lsp.enable('mock')").await;
    let recs = wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    let open = find(&recs, "textDocument/didOpen").expect("a didOpen after a late enable");
    assert_eq!(
        open["params"]["textDocument"]["languageId"].as_str(),
        Some("rust"),
        "the late enable opened the current rust buffer against the server"
    );
}
