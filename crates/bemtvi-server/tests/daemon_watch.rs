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

use bemtvi_test_harness::{
    await_lines, buf_lines, exec_lua, feed, lines, poll_true, spawn_with_daemon_fs, DaemonFs,
};

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

/// Once the daemon's watch has reported that the file changed under a modified buffer, a
/// plain `:w` refuses to clobber the outside edit — exactly as the local `:w` does after a
/// change on disk — and `:w!` overrides. Off-tick the editor cannot stat the remote file
/// on the tick, but the watch already told it: the warning must not be local-only.
#[tokio::test]
async fn write_refuses_to_clobber_a_change_the_daemon_watch_reported() {
    let fake = DaemonFs::with("/virtual/clash.txt", "base\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake.clone(), "/virtual/clash.txt").await;
    await_lines(&rpc, &["base"]).await;

    exec_lua(
        &rpc,
        r#"
        vim.g.fcs_reason = ""
        vim.api.nvim_create_autocmd("FileChangedShell", {
          callback = function() vim.g.fcs_reason = vim.v.fcs_reason end,
        })
        "#,
    )
    .await;
    feed(&rpc, "ggccmine<Esc>");
    await_lines(&rpc, &["mine"]).await;

    fake.set("/virtual/clash.txt", "theirs\nlonger\n");
    assert!(
        poll_true(&rpc, "return vim.g.fcs_reason == 'conflict'").await,
        "the daemon watch reports the conflicting change"
    );

    feed(&rpc, ":w<CR>");
    // Round-trip, then give a (wrongly) dispatched write time to land on the daemon.
    assert_eq!(buf_lines(&rpc, 0).await, vec!["mine"]);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        fake.content("/virtual/clash.txt").as_deref(),
        Some("theirs\nlonger\n"),
        "`:w` must not clobber the change the daemon watch reported"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.bo.modified").await.as_bool(),
        Some(true)
    );
    feed(&rpc, ":wall<CR>");
    assert_eq!(buf_lines(&rpc, 0).await, vec!["mine"]);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        fake.content("/virtual/clash.txt").as_deref(),
        Some("theirs\nlonger\n"),
        "`:wall` skips the buffer whose file the daemon watch reported changed"
    );

    feed(&rpc, ":w!<CR>");
    for _ in 0..100 {
        if fake.content("/virtual/clash.txt").as_deref() == Some("mine\n") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        fake.content("/virtual/clash.txt").as_deref(),
        Some("mine\n"),
        "`:w!` overrides the guard"
    );
}

/// An autoread reload whose re-fetch is still in flight when the user edits the buffer
/// must not land over those edits: the late bytes are dropped and the conflict is
/// reported (`W12`), leaving the user's text — and its undo history — intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_autoreload_landing_after_an_edit_keeps_the_edit() {
    let fake = DaemonFs::with("/virtual/race.txt", "alpha\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake.clone(), "/virtual/race.txt").await;
    await_lines(&rpc, &["alpha"]).await;

    let hold = fake.hold_reads();
    fake.set("/virtual/race.txt", "outside\nchange\n");
    // The watch fired and the reload's fetch is parked on the daemon.
    hold.await_parked().await;
    feed(&rpc, "ggIx<Esc>");
    assert_eq!(lines(&rpc).await, vec!["xalpha"]);
    hold.release();

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["xalpha"],
        "the late reload must not clobber the edit made while it was in flight"
    );
    assert!(
        poll_true(&rpc, "return vim.bo.modified").await,
        "the edited buffer stays modified"
    );
}
