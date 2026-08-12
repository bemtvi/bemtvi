//! The daemon wire protocol, filesystem half (edit-host split, Phase 3 of
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md`).
//!
//! Proves the **initial buffer** is fetched over a real wire, off the editor tick:
//! an editor given a [`RemoteHostFs`](bemtvi_server::RemoteHostFs) as its async fs
//! starts empty, the server requests the startup file from a
//! [`serve_fs_daemon`](bemtvi_server::serve_fs_daemon) over an in-process
//! `tokio::io::duplex`, and the bytes load into a replica buffer. The duplex stands
//! in for the eventual ssh stdio to `bemtvi --daemon`.
//!
//! Faithful, not a no-op: the path is `/virtual/...`, which the edit-host's *local*
//! disk cannot read — so the content appearing in the buffer can only have come
//! across the wire from the daemon's fs (the same argument `host_fs.rs` makes for the
//! sync seam). The `attach` handshake completes before the file loads, evidence the
//! fetch did not block startup. A second test proves a not-yet-existing path opens as
//! an empty new-file buffer (not an error), and its name is bound for a later `:w`.
//!
//! Black-box like the rest: a real server over the in-process RPC pipe, asserting on
//! `nvim_buf_get_lines` / the buffer name.

use std::time::Duration;

use bemtvi_test_harness::{
    await_lines, buf_lines, command, exec_lua, map_get, spawn_with_daemon_fs, wait_redraw,
    window0_field, DaemonFs,
};
use rmpv::Value;

/// The startup file's bytes are fetched over the wire and loaded into the first
/// buffer — content from a `/virtual/...` path the edit-host's local disk can't read,
/// so it can only have crossed the daemon wire.
#[tokio::test]
async fn initial_buffer_is_fetched_over_the_daemon_wire() {
    let fake = DaemonFs::with("/virtual/note.txt", "fetched\nover\nthe\nwire\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/note.txt").await;

    assert_eq!(
        await_lines(&rpc, &["fetched", "over", "the", "wire"]).await,
        vec!["fetched", "over", "the", "wire"],
        "the buffer must hold the bytes the daemon served over the wire"
    );
    // The buffer is named for the remote path, the way an opened file is.
    assert_eq!(
        exec_lua(&rpc, "return vim.api.nvim_buf_get_name(0)")
            .await
            .as_str(),
        Some("/virtual/note.txt"),
        "the replica buffer must carry the remote path as its name"
    );
}

/// A not-yet-existing remote path opens as an empty new-file buffer (not an error),
/// with its name bound — the `:e newfile` case, so a first `:w` would create it.
#[tokio::test]
async fn missing_path_opens_a_new_file_buffer() {
    let fake = DaemonFs::default(); // serves nothing → the path is "new"
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/fresh.txt").await;

    // Wait for the name to bind (the off-tick open), then assert the buffer is empty.
    for _ in 0..100 {
        let name = exec_lua(&rpc, "return vim.api.nvim_buf_get_name(0)")
            .await
            .as_str()
            .map(str::to_string);
        if name.as_deref() == Some("/virtual/fresh.txt") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        exec_lua(&rpc, "return vim.api.nvim_buf_get_name(0)")
            .await
            .as_str(),
        Some("/virtual/fresh.txt"),
        "a missing remote file still binds its name (a new-file buffer)"
    );
    assert_eq!(
        buf_lines(&rpc, 0).await,
        vec![""],
        "a new-file buffer is empty, not an error or stale content"
    );
}

/// In a daemon session an image preview's bytes live on the remote host, so the
/// redraw marks the marker `remote = true` and the native client fetches the bytes
/// over `bemtvi_image_read` — content from a `/virtual/...` path the edit-host's local
/// disk can't read, so it can only have crossed the daemon wire. (The editor — and so
/// the marker's `path` — runs local; only the bytes are remote.)
#[tokio::test]
async fn image_preview_is_remote_and_bytes_fetch_over_the_wire() {
    let fake = DaemonFs::with("/virtual/note.txt", "plain\n");
    fake.set_bytes("/virtual/pic.png", b"PNGBYTES\n");
    // Open a plain buffer at startup, enable previews, then edit the remote image.
    let (rpc, mut incoming) = spawn_with_daemon_fs(fake, "/virtual/note.txt").await;
    exec_lua(&rpc, "btv.o.imagepreview = true").await;
    command(&rpc, "edit /virtual/pic.png").await;

    let frame = wait_redraw(&mut incoming, |m| {
        matches!(window0_field(m, "image"), Some(Value::Map(_)))
    })
    .await;
    let Some(Value::Map(img)) = window0_field(&frame, "image") else {
        panic!("the redraw window carries an image marker");
    };
    assert_eq!(
        map_get(img, "path").and_then(Value::as_str),
        Some("/virtual/pic.png"),
        "the marker carries the remote image path"
    );
    assert_eq!(
        map_get(img, "remote").and_then(Value::as_bool),
        Some(true),
        "a daemon session marks the image preview remote"
    );

    let reply = rpc
        .request("bemtvi_image_read", vec![Value::from("/virtual/pic.png")])
        .await
        .expect("bemtvi_image_read responds");
    assert_eq!(
        reply,
        Value::Binary(b"PNGBYTES\n".to_vec()),
        "bemtvi_image_read returns the bytes the daemon served over the wire"
    );
}
