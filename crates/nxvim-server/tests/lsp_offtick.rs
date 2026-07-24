//! A project-wide LSP `WorkspaceEdit` (rename / code action) must reach files that
//! aren't open in a buffer **even in a daemon / web session**, where an unopened
//! file's bytes live across the wire and can only be fetched off the editor tick.
//!
//! The reachable seam without a real stdio server is `nx._lsp_apply_workspace_edit`
//! (the Lua entry behind `vim.lsp.util.apply_workspace_edit`): it hands an LSP-shape
//! `WorkspaceEdit` into the same `apply_workspace_edit` path a native rename reply
//! uses. Driving it under an **async daemon fs** (so `host_fs_offtick` is on)
//! exercises the deferred-apply path: the unopened file's replica buffer is created,
//! its fetch enqueued, the edits stashed, and applied once the bytes land.
//!
//! Faithful, not a no-op: the unopened file's path is `/virtual/...`, which the
//! edit-host's *local* disk cannot read — so the renamed content appearing in its
//! buffer can only have come across the wire from the daemon's fs.

use nxvim_test_harness::{await_lines, command, exec_lua, spawn_with_daemon_fs, DaemonFs};

/// A rename's `WorkspaceEdit` touches the open file *and* a file that was never
/// opened, in a daemon session. The unopened file's bytes are fetched over the wire,
/// then the stashed edits apply to its replica buffer — so a project-wide rename
/// reaches unopened files off-tick, not just locally.
#[tokio::test]
async fn workspace_edit_reaches_an_unopened_file_off_tick() {
    let fake = DaemonFs::with_files(&[
        ("/virtual/a.rs", "let foo = 1\n"),
        ("/virtual/b.rs", "use a::foo;\nfn g() { foo() }\n"),
    ]);
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/a.rs").await;

    // The startup file fetches over the wire first.
    assert_eq!(
        await_lines(&rpc, &["let foo = 1"]).await,
        vec!["let foo = 1"],
        "the open file should load over the daemon wire"
    );

    // Rename `foo` -> `bar`: one occurrence in the open `a.rs`, two in the unopened
    // `b.rs` (which has never been opened in a buffer).
    let edit = "nx._lsp_apply_workspace_edit({ changes = { \
        ['file:///virtual/a.rs'] = { \
          { range = { start = { line = 0, character = 4 }, ['end'] = { line = 0, character = 7 } }, newText = 'bar' } }, \
        ['file:///virtual/b.rs'] = { \
          { range = { start = { line = 0, character = 7 }, ['end'] = { line = 0, character = 10 } }, newText = 'bar' }, \
          { range = { start = { line = 1, character = 9 }, ['end'] = { line = 1, character = 12 } }, newText = 'bar' } } } })";
    exec_lua(&rpc, edit).await;

    // The open buffer is rewritten synchronously.
    assert_eq!(
        await_lines(&rpc, &["let bar = 1"]).await,
        vec!["let bar = 1"],
        "the open file should be renamed in place"
    );

    // The unopened `b.rs` was brought into a replica buffer, its bytes fetched over
    // the wire, and the stashed edits applied once they landed. Switch to it (the
    // edit created the buffer, so `:edit` reuses it) and check both occurrences.
    command(&rpc, "edit /virtual/b.rs").await;
    assert_eq!(
        await_lines(&rpc, &["use a::bar;", "fn g() { bar() }"]).await,
        vec!["use a::bar;", "fn g() { bar() }"],
        "the rename should reach both occurrences in the unopened, off-tick file"
    );
}
