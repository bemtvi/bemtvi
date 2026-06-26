//! The unified **read-only** regression net — one property (`modifiable()` refuses
//! edits at the chokepoints with `E21`), exercised across every non-ordinary buffer
//! kind that carries a `Buffer` marker: the directory listing (explorer / netrw), a
//! plugin-owned `nx.view`, the quickfix display, and an image preview. (A live
//! terminal is covered in `terminal.rs`; its read-only-ness now rides the same
//! `Buffer::read_only()` path.)
//!
//! The point is the **ex-command** path (`:d` / `:s` / `:put`). Normal-
//! mode edit keys on the explorer / view are *also* swallowed by input-routing today,
//! but an ex-command reaches the edit chokepoints directly — so before
//! `Buffer::read_only()` folded the explorer into `modifiable()`, a `:d` on a netrw
//! listing silently deleted a line despite the listing claiming to be inert. This
//! file is that hole's regression net (and the safety net for the Phase-2 deletion of
//! the bespoke input-routing, after which normal-mode edits rely on `modifiable()`
//! alone). See `docs/plans/2026-06-16-unify-special-buffer-kinds.md`.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    drain_to_latest_redraw, exec_lua, lines, message, start_attached, temp_dir, write_temp,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Feed `keys`, barrier, and return the latest redraw's message line.
async fn message_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> String {
    while incoming.try_recv().is_ok() {}
    rpc.request("nx_input", vec![Value::from(keys)])
        .await
        .expect("input");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    for _ in 0..200 {
        if let Some(map) = drain_to_latest_redraw(incoming, |_| true) {
            return message(&map);
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no redraw arrived for {keys:?}");
}

/// Run the whole ex-command edit battery against the focused read-only buffer and
/// assert each is refused with `E21` and leaves the content untouched. `kind` names
/// the buffer for the failure message.
async fn assert_ex_edits_refused(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    kind: &str,
) {
    let before = lines(rpc).await;
    // A yank seeds a register so `:put` has something to insert (proving it's the
    // read-only guard, not an empty register, that refuses it).
    message_after(rpc, incoming, "yy").await;
    for cmd in [":d<CR>", ":s/./X/<CR>", ":put<CR>"] {
        let msg = message_after(rpc, incoming, cmd).await;
        assert!(
            msg.contains("E21"),
            "{kind}: {cmd:?} should be refused with E21, got {msg:?}"
        );
    }
    assert_eq!(
        lines(rpc).await,
        before,
        "{kind}: ex-command edits must not change the content"
    );
}

/// The explorer (directory listing) — the kind Phase 1 actually fixes. An
/// ex-command edit reached the chokepoints uninhibited before `read_only()` folded
/// the `dir` marker into `modifiable()`.
#[tokio::test]
async fn explorer_listing_is_read_only_to_ex_command_edits() {
    let dir = temp_dir("ro_explorer");
    std::fs::write(dir.join("alpha.txt"), "a\n").expect("write alpha");
    std::fs::write(dir.join("beta.txt"), "b\n").expect("write beta");
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, &format!("nx.cmd('edit {}')", dir.to_string_lossy())).await;
    // The explorer (a Lua plugin) fills the listing asynchronously (`nx.fs.readdir`
    // settles off the tick), so poll until the entries appear before asserting.
    let mut listed = false;
    for _ in 0..100 {
        if lines(&rpc).await.iter().any(|l| l == "alpha.txt") {
            listed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(listed, "the explorer listing is open");
    assert_ex_edits_refused(&rpc, &mut incoming, "explorer").await;
}

/// A plugin-owned `nx.view` — read-only at the chokepoints via the same `read_only()`
/// path (its `view` marker). (Normal-mode inertness is covered in `nx_view.rs`.)
#[tokio::test]
async fn view_is_read_only_to_ex_command_edits() {
    let (rpc, mut incoming) = start().await;
    exec_lua(
        &rpc,
        r#"vw = nx.view.create{}
           vw:set_lines{ "alpha", "beta", "gamma" }
           vw:mount{ dock = "left", size = 20 }"#,
    )
    .await;
    assert_ex_edits_refused(&rpc, &mut incoming, "view").await;
}

/// The quickfix display buffer — its identity is an `Editor`-side registry, not a
/// `Buffer` marker, so `modifiable()` checks it alongside `read_only()`.
#[tokio::test]
async fn quickfix_display_is_read_only_to_ex_command_edits() {
    let (rpc, mut incoming) = start().await;
    exec_lua(
        &rpc,
        r#"vim.fn.setqflist({ { filename = "a.c", lnum = 1, text = "x" },
                              { filename = "b.c", lnum = 2, text = "y" } }, " ")"#,
    )
    .await;
    message_after(&rpc, &mut incoming, ":copen<CR>").await;
    assert_ex_edits_refused(&rpc, &mut incoming, "quickfix").await;
}

/// An image preview — bound to a file but never read into the rope; folded into
/// `read_only()` via its `image` marker so an ex-command can't write into it.
#[tokio::test]
async fn image_preview_is_read_only_to_ex_command_edits() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "nx.o.imagepreview = true").await;
    let path = write_temp("ro_image", "png", "PNGPLACEHOLDER\n");
    exec_lua(&rpc, &format!("nx.cmd('edit {path}')")).await;
    assert_ex_edits_refused(&rpc, &mut incoming, "image").await;
}
