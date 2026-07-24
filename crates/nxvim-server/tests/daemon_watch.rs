//! The daemon wire protocol, filesystem half — the **watch leg** (`HostWatch`, the
//! edit-host split, `docs/plans/2026-06-09-edit-host-and-browser-lua.md`).
//!
//! Companion to `daemon_save.rs` (the off-tick *write*) and `daemon_fs.rs` (the
//! off-tick *read*). Here the daemon **owns change detection**: it watches the files
//! the edit-host opened and pushes a `fs_changed` notification when one drifts, which
//! the edit-host turns into a `FileChangedShell` reconcile off the editor tick — the
//! remote analogue of the local per-buffer file watch.
//!
//! Both tests use a `/virtual/...` path the edit-host's *local* disk can't hold, so the
//! reload bytes can only have crossed the wire (the same faithfulness argument
//! `daemon_save` makes for the write):
//!
//! - an external change to an unmodified buffer **autoreloads** over the wire (the
//!   daemon detected it, pushed it, and the edit-host re-fetched the new bytes), and
//! - a `FileChangedShell` handler fires on the edit-host with `v:fcs_reason` set and
//!   its `v:fcs_choice = "reload"` drives the off-tick re-fetch.

use nxvim_test_harness::{await_lines, exec_lua, spawn_with_daemon_fs, DaemonFs};

/// An external change to an unmodified buffer **autoreloads over the wire**: the daemon
/// notices the file drifted from its baseline, pushes `fs_changed`, and the edit-host
/// re-fetches the new bytes (`'autoread'` is on by default). The new content is a
/// `/virtual/...` path the edit-host's local disk can't hold, so it can only have
/// crossed the wire — and there is **no** `:checktime`, proving the daemon's watch
/// drove it on its own.
#[tokio::test]
async fn an_external_change_autoreloads_over_the_daemon_watch() {
    let fake = DaemonFs::with("/virtual/note.txt", "alpha\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake.clone(), "/virtual/note.txt").await;
    await_lines(&rpc, &["alpha"]).await;

    // Someone else rewrites the remote file (a different length, so the daemon's
    // size-based stat sees the change). No `:checktime` — the daemon's watch detects it.
    fake.set("/virtual/note.txt", "alpha\nbeta\ngamma\n");

    await_lines(&rpc, &["alpha", "beta", "gamma"]).await;
}

/// The `FileChangedShell` round-trip works **over the wire**: with `'noautoread'`, the
/// edit-host fires `FileChangedShell` (with `v:fcs_reason` set) for a daemon-pushed
/// change, and the handler's `v:fcs_choice = "reload"` drives the off-tick re-fetch.
#[tokio::test]
async fn file_changed_shell_handler_reloads_over_the_daemon_watch() {
    let fake = DaemonFs::with("/virtual/doc.txt", "first\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake.clone(), "/virtual/doc.txt").await;
    await_lines(&rpc, &["first"]).await;

    // 'noautoread' so the change won't silently reload — it must route through the
    // handler. Register a handler that records the reason and redirects to a reload.
    exec_lua(
        &rpc,
        r#"
        vim.o.autoread = false
        vim.g.fcs_reason = ""
        vim.api.nvim_create_autocmd("FileChangedShell", {
          callback = function()
            vim.g.fcs_reason = vim.v.fcs_reason
            vim.v.fcs_choice = "reload"
          end,
        })
        "#,
    )
    .await;

    fake.set("/virtual/doc.txt", "second\nthird\n");

    // The handler's "reload" choice re-fetches the new bytes over the wire...
    await_lines(&rpc, &["second", "third"]).await;
    // ...and it saw v:fcs_reason = "changed" (unmodified buffer, file present).
    assert_eq!(
        exec_lua(&rpc, "return vim.g.fcs_reason").await.as_str(),
        Some("changed"),
        "the FileChangedShell handler must see v:fcs_reason over the wire"
    );
}
