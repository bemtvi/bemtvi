//! Black-box tests for picker / LSP navigation reusing the open buffer instead of
//! opening a duplicate or erroring on a modified buffer.
//!
//! The LSP hands back **absolute** paths, but a file is commonly opened with a
//! **cwd-relative** name (`:e src/foo.rs`). Confirming a symbol / reference must
//! still land in the *same* buffer — and when that buffer is the one you're
//! editing (and modified), it must just move the cursor, not reload (E37) or
//! strand a second buffer for the same file.
//!
//! These mutate the **process** working directory (so a relative open and an
//! absolute jump resolve to one file), which is process-global — so each test
//! holds the process-wide [`serial_lock`] for its whole body and restores the cwd
//! on the way out (via [`CwdGuard`]), and this lives in its **own** test binary so
//! it can't perturb the cwd-reading tests in other suites.

use std::path::{Path, PathBuf};

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, command, cursor, drain_to_latest_redraw, exec_lua, feed, lines, map_get, message,
    serial_lock, spawn, temp_dir,
};
use tokio::sync::mpsc::UnboundedReceiver;

/// Restore the process cwd to what it was when constructed — so a test that
/// changes the cwd (or panics mid-way) doesn't leak it to the next one.
struct CwdGuard(PathBuf);
impl CwdGuard {
    fn capture() -> Self {
        CwdGuard(std::env::current_dir().expect("cwd"))
    }
}
impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

async fn start(dir: &Path) -> (Rpc, UnboundedReceiver<Incoming>) {
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// The current buffer's handle.
async fn cur_buf(rpc: &Rpc) -> u64 {
    exec_lua(rpc, "return nx.buf.current()")
        .await
        .as_u64()
        .expect("current bufnr")
}

/// How many buffers are open — a duplicate-on-jump bug shows up as this growing.
async fn buf_count(rpc: &Rpc) -> u64 {
    exec_lua(rpc, "return #vim.api.nvim_list_bufs()")
        .await
        .as_u64()
        .expect("buffer count")
}

/// The latest message the editor echoed (`""` if none) — used to assert E37 never
/// fired. The barrier's repaint preserves persistent message state, so the latest
/// redraw is authoritative.
async fn latest_message(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> String {
    nxvim_test_harness::barrier(rpc).await;
    drain_to_latest_redraw(incoming, |m| map_get(m, "message").is_some())
        .map(|m| message(&m))
        .unwrap_or_default()
}

/// Bug 1: a symbol/reference jump carries the LSP's **absolute** path, but the
/// file was opened with a cwd-relative name. Confirming it must reuse that buffer
/// (cwd-aware), not open a second buffer for the same file.
#[tokio::test]
async fn picker_jump_reuses_a_relatively_opened_buffer() {
    let _guard = serial_lock().lock().await;
    let _cwd = CwdGuard::capture();
    // Canonicalize the temp dir up front: after `:cd` the process cwd reads back
    // symlink-resolved (macOS temp dirs live under `/var` → `/private/var`), and the
    // editor's cwd-aware buffer dedup is filesystem-free — it never canonicalizes an
    // absolute path. A real LSP hands back already-canonical paths, so to model that
    // (and not the symlink artifact) the ABSOLUTE path we feed must be in the same
    // canonical form the editor's cwd will be.
    let dir = std::fs::canonicalize(temp_dir("lsp_nav_relative")).unwrap();
    let file = dir.join("target.txt");
    std::fs::write(&file, "one\ntwo\nthree\nfour\n").unwrap();
    let abs = file.to_string_lossy().into_owned();

    let (rpc, mut incoming) = start(&dir).await;
    // Anchor the working dir to the temp dir, then open the file *relatively* —
    // exactly how a project file gets opened.
    command(&rpc, &format!("cd {}", dir.display())).await;
    command(&rpc, "edit target.txt").await;
    assert_eq!(lines(&rpc).await, vec!["one", "two", "three", "four"]);
    let before_buf = cur_buf(&rpc).await;
    let before_count = buf_count(&rpc).await;

    // Modify the buffer so a reload would be observable (and a `:edit` of the same
    // file would E37).
    feed(&rpc, "ggIX<Esc>");
    nxvim_test_harness::barrier(&rpc).await;
    assert_eq!(lines(&rpc).await[0], "Xone", "the edit landed");

    // Confirm a located item with the file's ABSOLUTE path (as the LSP would).
    exec_lua(
        &rpc,
        &format!("nx.picker.edit({{ path = '{abs}', row = 3, col = 2 }})"),
    )
    .await;
    nxvim_test_harness::barrier(&rpc).await;

    assert_eq!(
        cur_buf(&rpc).await,
        before_buf,
        "the jump reused the relatively-opened buffer (not a duplicate)"
    );
    assert_eq!(
        buf_count(&rpc).await,
        before_count,
        "no second buffer was opened for the same file"
    );
    // `cursor()` reports row 1-based, col 0-based — so a 1-based `col = 2` lands at 1.
    assert_eq!(
        cursor(&rpc).await,
        (3, 1),
        "the cursor jumped to the item's row/col"
    );
    assert_eq!(
        lines(&rpc).await[0],
        "Xone",
        "the buffer kept its unsaved edit — a jump does not reload"
    );
    assert!(
        !latest_message(&rpc, &mut incoming).await.contains("E37"),
        "navigating must not raise the no-write-since-last-change guard"
    );
}

/// Bug 2: confirming a location *in the current, modified buffer* (path matches
/// exactly) must move the cursor, never reload — the old `:edit` path raised E37.
#[tokio::test]
async fn picker_jump_into_the_current_modified_buffer_moves_the_cursor() {
    let _guard = serial_lock().lock().await;
    let _cwd = CwdGuard::capture();
    let dir = temp_dir("lsp_nav_modified");
    let file = dir.join("buf.txt");
    std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\nepsilon\n").unwrap();
    let abs = file.to_string_lossy().into_owned();

    let (rpc, mut incoming) = start(&dir).await;
    // Open by absolute path so the picker item's path matches exactly — the case
    // that used to hit the `:edit` modified guard.
    command(&rpc, &format!("edit {abs}")).await;
    let before_buf = cur_buf(&rpc).await;
    let before_count = buf_count(&rpc).await;

    feed(&rpc, "ggIZ<Esc>");
    nxvim_test_harness::barrier(&rpc).await;
    assert_eq!(lines(&rpc).await[0], "Zalpha", "the edit landed");

    exec_lua(
        &rpc,
        &format!("nx.picker.edit({{ path = '{abs}', row = 5, col = 1 }})"),
    )
    .await;
    nxvim_test_harness::barrier(&rpc).await;

    assert_eq!(cur_buf(&rpc).await, before_buf, "stayed in the same buffer");
    assert_eq!(buf_count(&rpc).await, before_count, "no extra buffer");
    assert_eq!(
        cursor(&rpc).await,
        (5, 0),
        "the cursor moved to the location"
    );
    assert_eq!(
        lines(&rpc).await[0],
        "Zalpha",
        "the unsaved edit survived — no reload"
    );
    assert!(
        !latest_message(&rpc, &mut incoming).await.contains("E37"),
        "jumping into the modified buffer must not raise E37"
    );
}
