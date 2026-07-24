//! The daemon wire protocol, filesystem half — **the remote explorer over the wire**
//! (edit-host split, Phase 3g of `docs/plans/2026-06-09-edit-host-and-browser-lua.md`).
//!
//! Companion to `daemon_fs.rs` (initial open), `daemon_save.rs` (save), and
//! `daemon_edit.rs` (`:edit`). Here a real editor whose async fs is a
//! [`RemoteHostFs`](nxvim_server::RemoteHostFs) talking to a
//! [`serve_fs_daemon`](nxvim_server::serve_fs_daemon) over an in-process duplex opens a
//! **remote directory** — listed over the wire, off the editor tick, through the same
//! `HostFsAsync` + replica path a file open uses. Until this slice a remote directory
//! came back as a loud `fs_read` error ("remote directory open not yet supported"); now
//! it lists, navigates, and opens entries — all without touching the edit-host's local
//! disk:
//!
//! - `nxvim /virtual/proj` (startup) lists the remote directory's entries.
//! - `:edit /virtual/proj` lists it at runtime.
//! - `<CR>` on a sub-directory descends into it over the wire; `-` goes back up.
//! - `<CR>` on a file entry opens that remote file's bytes over the wire.
//!
//! The `/virtual/...` paths can't exist on the edit-host's local disk, so any listing or
//! content can *only* have crossed the wire (the faithfulness argument the other daemon
//! suites make).

use nxvim_test_harness::{await_lines, feed, spawn_with_daemon_fs, DaemonFs};

/// A fake daemon fs pre-populated with `/virtual/proj` (a directory holding a `src/`
/// sub-directory and two files), `/virtual/proj/src` (holding `main.rs`), and the two
/// readable files. The standard fixture for these tests.
fn fixture() -> DaemonFs {
    let fs = DaemonFs::default();
    fs.dir(
        "/virtual/proj",
        &[(true, "src"), (false, "README.md"), (false, "notes.txt")],
    )
    .dir("/virtual/proj/src", &[(false, "main.rs")])
    .set("/virtual/proj/README.md", "# Readme\n")
    .set("/virtual/proj/src/main.rs", "fn main() {}\n");
    fs
}
/// name. The `/virtual/...` path can't be a local directory, so the listing crossed the
/// wire.
#[tokio::test]
async fn startup_lists_a_remote_directory() {
    let (rpc, _incoming) = spawn_with_daemon_fs(fixture(), "/virtual/proj").await;
    assert_eq!(
        await_lines(&rpc, &["../", "src/", "notes.txt", "README.md"]).await,
        vec!["../", "src/", "notes.txt", "README.md"],
        "the startup directory lists its remote entries (dirs first, then files by name)"
    );
}

/// `:edit /virtual/proj` lists a remote directory at runtime (the same off-tick path as
/// the startup open).
#[tokio::test]
async fn edit_lists_a_remote_directory() {
    let fake = fixture();
    fake.set("/virtual/note.txt", "alpha\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/note.txt").await;
    await_lines(&rpc, &["alpha"]).await;

    feed(&rpc, ":edit /virtual/proj<CR>");
    assert_eq!(
        await_lines(&rpc, &["../", "src/", "notes.txt", "README.md"]).await,
        vec!["../", "src/", "notes.txt", "README.md"],
        "`:edit <remote-dir>` lists the remote directory over the wire"
    );
}

/// `<CR>` on a sub-directory descends into it over the wire (re-listing in place); `-`
/// goes back up to the parent — both remote `read_dir`s.
#[tokio::test]
async fn enter_descends_and_dash_goes_up_over_the_wire() {
    let (rpc, _incoming) = spawn_with_daemon_fs(fixture(), "/virtual/proj").await;
    await_lines(&rpc, &["../", "src/", "notes.txt", "README.md"]).await;

    // Row 1 is `src/`; `<CR>` lists that sub-directory over the wire.
    feed(&rpc, "j<CR>");
    assert_eq!(
        await_lines(&rpc, &["../", "main.rs"]).await,
        vec!["../", "main.rs"],
        "`<CR>` on `src/` descends into the remote sub-directory"
    );

    // `-` lists the parent again (another remote read_dir).
    feed(&rpc, "-");
    assert_eq!(
        await_lines(&rpc, &["../", "src/", "notes.txt", "README.md"]).await,
        vec!["../", "src/", "notes.txt", "README.md"],
        "`-` lists the remote parent directory again"
    );
}

/// `<CR>` on a file entry opens that remote file's bytes over the wire (and destroys the
/// listing, as netrw does).
#[tokio::test]
async fn enter_on_a_file_opens_it_over_the_wire() {
    let (rpc, _incoming) = spawn_with_daemon_fs(fixture(), "/virtual/proj").await;
    await_lines(&rpc, &["../", "src/", "notes.txt", "README.md"]).await;

    // Descend into `src/` (row 1), then open `main.rs` (row 1 of that listing).
    feed(&rpc, "j<CR>");
    await_lines(&rpc, &["../", "main.rs"]).await;
    feed(&rpc, "j<CR>");
    assert_eq!(
        await_lines(&rpc, &["fn main() {}"]).await,
        vec!["fn main() {}"],
        "`<CR>` on a file row opens the remote file's bytes over the wire"
    );
}

/// `:tabnew /virtual/proj/src` opens the remote *directory* as the explorer in a **new
/// tab** — the unified off-tick open kernel (Phase 3h) composes with the remote-explorer
/// listing (Phase 3g): a directory routed through `:tabnew` lists over the wire just as
/// `:edit` does, in its own tab.
#[tokio::test]
async fn tabnew_lists_a_remote_directory_in_a_new_tab() {
    let (rpc, _incoming) = spawn_with_daemon_fs(fixture(), "/virtual/proj").await;
    await_lines(&rpc, &["../", "src/", "notes.txt", "README.md"]).await;

    feed(&rpc, ":tabnew /virtual/proj/src<CR>");
    assert_eq!(
        await_lines(&rpc, &["../", "main.rs"]).await,
        vec!["../", "main.rs"],
        "`:tabnew <remote-dir>` lists the directory as the explorer in a new tab"
    );
}
