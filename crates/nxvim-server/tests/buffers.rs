//! Behavior tests for multiple open buffers, driven the way a real client
//! drives the editor (black-box RPC, exactly like `editing.rs`).
//!
//! Phase 2 covers the switch *mechanism*: `:e` opening/reusing buffers, the
//! alternate buffer (`<C-^>`), and each buffer keeping its own content, cursor
//! position, and undo history across switches. Phase 3 adds the list surface
//! (`:ls`, `:b`, `:bnext`/`:bprev`, `:bd`, `:wall`) and the buffer RPC API.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{buf_lines, command, cursor, feed, field, lines, start_attached};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server on its own thread and return a connected client.
async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// The current status `message`, read off the latest `redraw`. Sends a barrier
/// request so the redraw for the preceding action is already queued, then drains
/// to the most recent one.
async fn message(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> String {
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let mut msg = String::new();
    while let Ok(inc) = incoming.try_recv() {
        if let Incoming::Notification { method, params } = inc {
            if method == "redraw" {
                if let Some(Value::Map(map)) = params.into_iter().next() {
                    msg = map
                        .iter()
                        .find(|(k, _)| k.as_str() == Some("message"))
                        .and_then(|(_, v)| v.as_str())
                        .unwrap_or("")
                        .to_string();
                }
            }
        }
    }
    msg
}

/// Whether the focused window's buffer is unnamed (`windows[0].unnamed`), read off
/// the latest `redraw` like [`message`]. This is the explicit flag a GUI uses to
/// route a bare `:w` to its save dialog, rather than matching the display name.
async fn unnamed(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> bool {
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let mut flag = false;
    while let Ok(inc) = incoming.try_recv() {
        if let Incoming::Notification { method, params } = inc {
            if method == "redraw" {
                if let Some(Value::Map(map)) = params.into_iter().next() {
                    if let Some(v) = field(&map, "unnamed") {
                        flag = v.as_bool().unwrap_or(false);
                    }
                }
            }
        }
    }
    flag
}

/// A uniquely-named temp file path with the given contents. `.txt` so no
/// treesitter grammar is involved (keeps these tests free of the syntax worker).
fn temp_file(tag: &str, contents: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("nxvim_buf_{tag}_{}_{n}.txt", std::process::id()));
    std::fs::write(&path, contents).unwrap();
    path
}

fn name(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ----- buffer RPC API helpers -------------------------------------------------

async fn list_bufs(rpc: &Rpc) -> Vec<u64> {
    match rpc
        .request("nvim_list_bufs", vec![])
        .await
        .expect("list_bufs")
    {
        Value::Array(a) => a.iter().filter_map(Value::as_u64).collect(),
        _ => Vec::new(),
    }
}

async fn current_buf(rpc: &Rpc) -> u64 {
    rpc.request("nvim_get_current_buf", vec![])
        .await
        .expect("get_current_buf")
        .as_u64()
        .expect("u64")
}

async fn set_current_buf(rpc: &Rpc, id: u64) {
    rpc.request("nvim_set_current_buf", vec![Value::from(id)])
        .await
        .expect("set_current_buf");
}

async fn create_buf(rpc: &Rpc) -> u64 {
    rpc.request("nvim_create_buf", vec![])
        .await
        .expect("create_buf")
        .as_u64()
        .expect("u64")
}

async fn buf_name(rpc: &Rpc, handle: u64) -> String {
    rpc.request("nvim_buf_get_name", vec![Value::from(handle)])
        .await
        .expect("buf_get_name")
        .as_str()
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn editing_a_nonexistent_file_does_not_storm_the_file_watch() {
    // Regression: `:e <missing>` opens a new-file buffer whose path has nothing on
    // disk behind it. The server used to arm a native file watch on that absent path;
    // kqueue/inotify can't watch a path that doesn't exist, so the arm failed — and the
    // arm-failure handler dropped the watch state then *immediately re-armed*, which
    // failed again, an unbounded arm→fail→re-arm storm that repainted on every cycle
    // and froze the UI. A new-file buffer (nothing to watch) must arm no watch at all,
    // so the channel goes quiet once the open settles.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let missing = std::env::temp_dir().join(format!(
        "nxvim_missing_{}_{}.txt",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    assert!(!missing.exists(), "the test path must not exist on disk");
    let real = temp_file("storm", "hello\nworld\n");
    let (rpc, mut incoming) = start().await;

    // Open a real file first (arms its watch), then — the way a user does, via *typed*
    // keystrokes (`nx_input`), not an `nx_command` RPC — edit a missing file.
    feed(&rpc, &format!(":e {}<CR>", name(&real)));
    let _ = lines(&rpc).await;
    feed(&rpc, &format!(":e {}<CR>", name(&missing)));
    // Barrier: `lines` round-trips a request so the open has been applied. The open
    // itself still works — a new-file buffer, current, named, one empty line.
    assert_eq!(lines(&rpc).await, vec![""]);
    let cur = current_buf(&rpc).await;
    assert_eq!(buf_name(&rpc, cur).await, name(&missing));

    // Drain whatever the open queued, then watch a quiet window. With the storm the
    // watch keeps repainting forever, so the channel never goes quiet; fixed, the
    // count is ~0.
    while incoming.try_recv().is_ok() {}
    let mut redraws = 0usize;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        while let Ok(inc) = incoming.try_recv() {
            if matches!(&inc, Incoming::Notification { method, .. } if method == "redraw") {
                redraws += 1;
            }
        }
    }
    std::fs::remove_file(&real).ok();
    assert!(
        redraws < 20,
        "`:e <missing>` must not storm the file watch; saw {redraws} redraws in 1s"
    );
}

#[tokio::test]
async fn unnamed_flag_tracks_whether_the_buffer_has_a_file() {
    // The redraw carries an explicit `unnamed` flag per window — the signal the GUI
    // uses to send a bare `:w` to its save dialog. A fresh buffer is unnamed;
    // opening a real file clears it; writing to a new path (save-as) names it.
    let a = temp_file("a", "a1\na2\n");
    let (rpc, mut incoming) = start().await;
    assert!(
        unnamed(&rpc, &mut incoming).await,
        "a fresh [No Name] buffer reports unnamed"
    );

    command(&rpc, &format!("e {}", name(&a))).await;
    assert!(
        !unnamed(&rpc, &mut incoming).await,
        "after :e <file> the buffer is named"
    );

    std::fs::remove_file(&a).ok();
}

#[tokio::test]
async fn edit_reuses_the_throwaway_buffer_then_opens_new_ones() {
    let a = temp_file("a", "a1\na2\na3\n");
    let b = temp_file("b", "b1\nb2\nb3\n");
    let (rpc, _incoming) = start().await;

    // First `:e` reuses the initial empty [No Name] buffer in place.
    command(&rpc, &format!("e {}", name(&a))).await;
    assert_eq!(lines(&rpc).await, vec!["a1", "a2", "a3"]);

    // A second file opens a new buffer and switches to it.
    command(&rpc, &format!("e {}", name(&b))).await;
    assert_eq!(lines(&rpc).await, vec!["b1", "b2", "b3"]);

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn reediting_an_open_file_switches_back_and_restores_the_cursor() {
    let a = temp_file("a", "a1\na2\na3\n");
    let b = temp_file("b", "b1\nb2\nb3\n");
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await;
    feed(&rpc, "jl"); // cursor to line 2, col 1
    assert_eq!(cursor(&rpc).await, (2, 1));

    command(&rpc, &format!("e {}", name(&b))).await;
    assert_eq!(lines(&rpc).await, vec!["b1", "b2", "b3"]);
    assert_eq!(cursor(&rpc).await, (1, 0));

    // Re-editing `a` finds the existing buffer and switches back — no duplicate,
    // and the cursor is where we left it.
    command(&rpc, &format!("e {}", name(&a))).await;
    assert_eq!(lines(&rpc).await, vec!["a1", "a2", "a3"]);
    assert_eq!(cursor(&rpc).await, (2, 1));

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn reediting_a_redundant_spelling_of_the_path_reuses_the_buffer() {
    // The buffer match normalizes paths lexically (no filesystem access), so a
    // different spelling of the same file — here with a redundant `/./` — finds
    // the open buffer instead of opening a duplicate.
    let a = temp_file("a", "a1\na2\n");
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await;
    assert_eq!(list_bufs(&rpc).await, vec![1]);

    // `/tmp/dir/file` -> `/tmp/dir/./file`: same file, different spelling.
    let redundant = a.parent().unwrap().join(".").join(a.file_name().unwrap());
    command(&rpc, &format!("e {}", name(&redundant))).await;
    assert_eq!(lines(&rpc).await, vec!["a1", "a2"]);
    assert_eq!(
        list_bufs(&rpc).await,
        vec![1],
        "the redundant spelling reused buffer 1 rather than opening a duplicate"
    );

    std::fs::remove_file(&a).ok();
}

#[tokio::test]
async fn ctrl_caret_toggles_the_alternate_buffer() {
    let a = temp_file("a", "a1\na2\n");
    let b = temp_file("b", "b1\nb2\n");
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await;
    command(&rpc, &format!("e {}", name(&b))).await;
    assert_eq!(lines(&rpc).await, vec!["b1", "b2"]);

    feed(&rpc, "<C-^>"); // -> alternate (a)
    assert_eq!(lines(&rpc).await, vec!["a1", "a2"]);

    feed(&rpc, "<C-^>"); // -> back to b
    assert_eq!(lines(&rpc).await, vec!["b1", "b2"]);

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn ctrl_caret_without_an_alternate_reports_e23() {
    let (rpc, mut incoming) = start().await;
    feed(&rpc, "<C-^>");
    assert_eq!(message(&rpc, &mut incoming).await, "E23: No alternate file");
}

#[tokio::test]
async fn undo_history_is_independent_per_buffer() {
    let a = temp_file("a", "a1\na2\n");
    let b = temp_file("b", "b1\nb2\n");
    let (rpc, mut incoming) = start().await;

    // Edit buffer a (open a new line), leaving it modified.
    command(&rpc, &format!("e {}", name(&a))).await;
    feed(&rpc, "oINSERTED<Esc>");
    assert_eq!(lines(&rpc).await, vec!["a1", "INSERTED", "a2"]);

    // Switch to b (a stays open, modified). Undo in b touches nothing in b and
    // reports the empty-history message — proving b has its own stack.
    command(&rpc, &format!("e {}", name(&b))).await;
    feed(&rpc, "u");
    assert_eq!(
        message(&rpc, &mut incoming).await,
        "Already at oldest change"
    );
    assert_eq!(lines(&rpc).await, vec!["b1", "b2"]);

    // Back to a: its undo stack is intact, so `u` removes the inserted line.
    command(&rpc, &format!("e {}", name(&a))).await;
    feed(&rpc, "u");
    assert_eq!(lines(&rpc).await, vec!["a1", "a2"]);

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn reediting_the_same_file_honors_the_modified_guard() {
    let a = temp_file("a", "a1\na2\n");
    let (rpc, mut incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await;
    feed(&rpc, "oDIRTY<Esc>");
    assert_eq!(lines(&rpc).await, vec!["a1", "DIRTY", "a2"]);

    // `:e a` on the same, modified file refuses without `!`.
    command(&rpc, &format!("e {}", name(&a))).await;
    assert_eq!(
        message(&rpc, &mut incoming).await,
        "E37: No write since last change (add ! to override)"
    );
    assert_eq!(lines(&rpc).await, vec!["a1", "DIRTY", "a2"]);

    // `:e!` reloads from disk, discarding the change.
    command(&rpc, &format!("e! {}", name(&a))).await;
    assert_eq!(lines(&rpc).await, vec!["a1", "a2"]);

    std::fs::remove_file(&a).ok();
}

#[tokio::test]
async fn undoing_every_edit_clears_the_modified_flag() {
    // Editing and then undoing back to the on-disk text must leave the buffer
    // unmodified — the modified flag tracks divergence from disk, not whether any
    // edit ever happened. Observed through the `:e` modified guard: a clean buffer
    // reloads silently, a dirty one refuses with E37.
    let a = temp_file("a", "a1\na2\n");
    let (rpc, mut incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await;
    feed(&rpc, "oDIRTY<Esc>");
    assert_eq!(lines(&rpc).await, vec!["a1", "DIRTY", "a2"]);

    // Undo the edit: text is back to disk, and so is the modified flag.
    feed(&rpc, "u");
    assert_eq!(lines(&rpc).await, vec!["a1", "a2"]);

    // `:e a` on the now-clean buffer reloads without complaint (no E37).
    command(&rpc, &format!("e {}", name(&a))).await;
    assert_eq!(message(&rpc, &mut incoming).await, "");
    assert_eq!(lines(&rpc).await, vec!["a1", "a2"]);

    std::fs::remove_file(&a).ok();
}

#[tokio::test]
async fn redoing_back_past_a_save_marks_modified_again() {
    // The clean point follows the file, not the original text: after saving, the
    // saved state is the clean one. Undoing away from it is modified; redoing back
    // to it is clean again.
    let a = temp_file("a", "a1\na2\n");
    let (rpc, mut incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await;
    feed(&rpc, "oDIRTY<Esc>");
    command(&rpc, "w").await; // save: this state is now the clean one
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "a1\nDIRTY\na2\n");

    // Undo away from the saved state -> modified again. `:e a` refuses.
    feed(&rpc, "u");
    assert_eq!(lines(&rpc).await, vec!["a1", "a2"]);
    command(&rpc, &format!("e {}", name(&a))).await;
    assert_eq!(
        message(&rpc, &mut incoming).await,
        "E37: No write since last change (add ! to override)"
    );

    // Redo back to the saved state -> clean again. `:e a` reloads silently.
    feed(&rpc, "<C-r>");
    assert_eq!(lines(&rpc).await, vec!["a1", "DIRTY", "a2"]);
    command(&rpc, &format!("e {}", name(&a))).await;
    assert_eq!(message(&rpc, &mut incoming).await, "");

    std::fs::remove_file(&a).ok();
}

#[tokio::test]
async fn enew_opens_an_empty_buffer_and_keeps_the_old_one() {
    let a = temp_file("a", "a1\na2\n");
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await;
    command(&rpc, "enew").await;
    assert_eq!(lines(&rpc).await, vec![""]);

    // The previous file is the alternate, reachable with <C-^>.
    feed(&rpc, "<C-^>");
    assert_eq!(lines(&rpc).await, vec!["a1", "a2"]);

    std::fs::remove_file(&a).ok();
}

#[tokio::test]
async fn ls_lists_open_buffers_with_flags() {
    let a = temp_file("a", "a1\n");
    let b = temp_file("b", "b1\n");
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await; // buffer 1, becomes alternate
    command(&rpc, &format!("e {}", name(&b))).await; // buffer 2, current

    // Widen the viewport so each buffer's listing — a line that embeds the long
    // absolute temp path — fits on a single display row. At the default 80
    // columns those paths word-wrap, splitting one buffer's entry across several
    // rows; that breaks the one-row-per-buffer correspondence this test asserts
    // (row count, per-row flags, selected row) without saying anything about the
    // `:ls` behavior under test.
    rpc.request(
        "nx_ui_try_resize",
        vec![Value::from(400u64), Value::from(24u64)],
    )
    .await
    .expect("resize");

    command(&rpc, "ls").await;
    let rows = lines(&rpc).await;

    // `:ls` opens the read-only `[Buffers]` listing: one row per buffer; current is
    // `%a`, the alternate is `#h`.
    assert_eq!(rows.len(), 2, "listing was: {rows:?}");
    assert!(
        rows[0].contains("#h") && rows[0].contains(&name(&a)),
        "{rows:?}"
    );
    assert!(
        rows[1].contains("%a") && rows[1].contains(&name(&b)),
        "{rows:?}"
    );
    // The listing opens with the current buffer (row index 1, `%a` — line 2) selected.
    assert_eq!(
        cursor(&rpc).await.0,
        2,
        "current buffer's row starts under the cursor"
    );

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn buffer_command_switches_by_number_and_name() {
    let a = temp_file("a", "a1\na2\n");
    let b = temp_file("b", "b1\nb2\n");
    let c = temp_file("c", "c1\nc2\n");
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await; // 1
    command(&rpc, &format!("e {}", name(&b))).await; // 2
    command(&rpc, &format!("e {}", name(&c))).await; // 3
    assert_eq!(list_bufs(&rpc).await, vec![1, 2, 3]);

    command(&rpc, "b 1").await;
    assert_eq!(lines(&rpc).await, vec!["a1", "a2"]);

    // Switch by file-name substring (the full path is unique).
    command(&rpc, &format!("b {}", name(&b))).await;
    assert_eq!(lines(&rpc).await, vec!["b1", "b2"]);

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
    std::fs::remove_file(&c).ok();
}

#[tokio::test]
async fn bnext_and_bprev_wrap_around() {
    let a = temp_file("a", "a\n");
    let b = temp_file("b", "b\n");
    let c = temp_file("c", "c\n");
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await; // 1
    command(&rpc, &format!("e {}", name(&b))).await; // 2
    command(&rpc, &format!("e {}", name(&c))).await; // 3 (current)

    command(&rpc, "bnext").await; // 3 -> wraps to 1
    assert_eq!(lines(&rpc).await, vec!["a"]);
    command(&rpc, "bnext").await; // -> 2
    assert_eq!(lines(&rpc).await, vec!["b"]);
    command(&rpc, "bprev").await; // -> 1
    assert_eq!(lines(&rpc).await, vec!["a"]);
    command(&rpc, "bprev").await; // 1 -> wraps to 3
    assert_eq!(lines(&rpc).await, vec!["c"]);

    command(&rpc, "bfirst").await;
    assert_eq!(lines(&rpc).await, vec!["a"]);
    command(&rpc, "blast").await;
    assert_eq!(lines(&rpc).await, vec!["c"]);

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
    std::fs::remove_file(&c).ok();
}

#[tokio::test]
async fn bdelete_blocks_modified_then_falls_back_to_alternate() {
    let a = temp_file("a", "a1\na2\n");
    let b = temp_file("b", "b1\nb2\n");
    let (rpc, mut incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await; // 1 (alternate)
    command(&rpc, &format!("e {}", name(&b))).await; // 2 (current)
    feed(&rpc, "oDIRTY<Esc>"); // modify b

    // `:bd` refuses the modified current buffer without `!`.
    command(&rpc, "bd").await;
    assert!(
        message(&rpc, &mut incoming).await.starts_with("E89"),
        "expected E89"
    );
    assert_eq!(list_bufs(&rpc).await, vec![1, 2]);

    // `:bd!` deletes it and falls back to the alternate (a).
    command(&rpc, "bd!").await;
    assert_eq!(list_bufs(&rpc).await, vec![1]);
    assert_eq!(lines(&rpc).await, vec!["a1", "a2"]);

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn bdelete_last_buffer_leaves_a_fresh_no_name() {
    let (rpc, _incoming) = start().await; // single empty [No Name] buffer (1)
    command(&rpc, "bd").await;

    // A new empty buffer takes its place — never zero buffers.
    let bufs = list_bufs(&rpc).await;
    assert_eq!(bufs.len(), 1);
    assert_ne!(bufs[0], 1, "the deleted id is not reused");
    assert_eq!(lines(&rpc).await, vec![""]);
    assert_eq!(buf_name(&rpc, 0).await, "");
}

/// Deleting a buffer that is *also* shown in another window (a split) must rebind
/// that other window, not leave it dangling on the freed id — which used to crash
/// the editor on the next redraw.
#[tokio::test]
async fn bdelete_rebinds_other_windows_showing_the_buffer() {
    let (rpc, _incoming) = start().await;
    feed(&rpc, "iAAA<Esc>"); // buffer 1 = AAA
    feed(&rpc, "<C-w>s"); // split; both windows show buffer 1
    command(&rpc, "enew").await; // current window → buffer 2 (empty)
    feed(&rpc, "iBBB<Esc>"); // buffer 2 = BBB

    // Buffer 1 still shows in the OTHER split window; deleting it must not crash.
    command(&rpc, "bd! 1").await;
    assert_eq!(list_bufs(&rpc).await, vec![2], "buffer 1 is gone");

    // The lingering window survived and was rebound to a valid buffer.
    feed(&rpc, "<C-w>w");
    assert_eq!(
        lines(&rpc).await,
        vec!["BBB"],
        "the other window was rebound"
    );
}

#[tokio::test]
async fn buffer_rpc_api_lists_reads_switches_and_creates() {
    let a = temp_file("a", "a1\na2\n");
    let b = temp_file("b", "b1\nb2\n");
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await; // 1
    command(&rpc, &format!("e {}", name(&b))).await; // 2 (current)

    assert_eq!(list_bufs(&rpc).await, vec![1, 2]);
    assert_eq!(current_buf(&rpc).await, 2);
    assert_eq!(buf_name(&rpc, 1).await, name(&a));
    // Read a non-current buffer by handle.
    assert_eq!(buf_lines(&rpc, 1).await, vec!["a1", "a2"]);

    set_current_buf(&rpc, 1).await;
    assert_eq!(current_buf(&rpc).await, 1);
    assert_eq!(lines(&rpc).await, vec!["a1", "a2"]);

    // create_buf adds a buffer without switching to it.
    let new = create_buf(&rpc).await;
    assert_eq!(new, 3);
    assert_eq!(current_buf(&rpc).await, 1);
    assert_eq!(list_bufs(&rpc).await, vec![1, 2, 3]);

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

/// `:w` must never clobber the original file when it cannot complete the save
/// atomically. A read-only directory holding a writable file is the test seam:
/// the old non-atomic write (`std::fs::write`, `O_TRUNC`) opens and truncates
/// the file *in place* — that needs write permission on the file, not the dir —
/// so it would overwrite the original. An atomic save instead creates a temp
/// entry in the dir (which the read-only dir forbids) and renames it into place,
/// so it fails loudly and leaves the original byte-for-byte intact.
#[cfg(unix)]
#[tokio::test]
async fn a_save_that_cannot_be_made_atomic_leaves_the_original_intact() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("nxvim_atomic_ro_{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("keep.txt");
    std::fs::write(&path, "original\n").unwrap();

    let (rpc, _incoming) = start().await;
    command(&rpc, &format!("e {}", name(&path))).await;
    feed(&rpc, "oCLOBBER<Esc>");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    command(&rpc, "w").await;

    // Restore dir perms first so cleanup runs regardless of the assertion.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let on_disk = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(
        on_disk, "original\n",
        "a save into an unwritable dir clobbered the original file"
    );
}

/// An atomic save writes a fresh temp file and renames it over the target, so it
/// must carry the existing file's permissions onto the replacement — otherwise a
/// `:w` would silently downgrade a `0600` secret to the default `0644`.
#[cfg(unix)]
#[tokio::test]
async fn write_preserves_the_existing_file_mode() {
    use std::os::unix::fs::PermissionsExt;

    let path = temp_file("mode", "secret\n");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let (rpc, _incoming) = start().await;
    command(&rpc, &format!("e {}", name(&path))).await;
    feed(&rpc, "oMORE<Esc>");
    command(&rpc, "w").await;

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "atomic save downgraded the file mode");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "secret\nMORE\n");

    std::fs::remove_file(&path).ok();
}

/// Saving a buffer bound to a *symlink* must write through to the link's target
/// and keep the link itself — the atomic rename must resolve the symlink first,
/// not replace it with a regular file.
#[cfg(unix)]
#[tokio::test]
async fn write_through_a_symlink_keeps_the_link_and_updates_the_target() {
    let real = temp_file("symlink_real", "original\n");
    let link =
        std::env::temp_dir().join(format!("nxvim_buf_symlink_link_{}.txt", std::process::id()));
    std::fs::remove_file(&link).ok();
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let (rpc, _incoming) = start().await;
    command(&rpc, &format!("e {}", name(&link))).await;
    feed(&rpc, "oNEW<Esc>");
    command(&rpc, "w").await;

    // The link is still a symlink (not replaced by a regular file)...
    let meta = std::fs::symlink_metadata(&link).unwrap();
    assert!(
        meta.file_type().is_symlink(),
        "save replaced the symlink with a regular file"
    );
    // ...and the edit landed on the real file it points to.
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "original\nNEW\n");

    std::fs::remove_file(&link).ok();
    std::fs::remove_file(&real).ok();
}

#[tokio::test]
async fn wall_writes_every_modified_buffer() {
    let a = temp_file("a", "a1\n");
    let b = temp_file("b", "b1\n");
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await;
    feed(&rpc, "oAAA<Esc>");
    command(&rpc, &format!("e {}", name(&b))).await;
    feed(&rpc, "oBBB<Esc>");

    command(&rpc, "wall").await;

    // Both files are persisted to disk with their edits.
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "a1\nAAA\n");
    assert_eq!(std::fs::read_to_string(&b).unwrap(), "b1\nBBB\n");

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn wall_warns_and_switches_to_a_buffer_changed_on_disk() {
    let a = temp_file("a", "a1\n"); // buffer 1
    let b = temp_file("b", "b1\n"); // buffer 2
    let (rpc, mut incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await;
    feed(&rpc, "oAAA<Esc>");
    command(&rpc, &format!("e {}", name(&b))).await;
    feed(&rpc, "oBBB<Esc>");

    // Someone rewrites buffer 1's file on disk behind our back.
    std::fs::write(&a, "EXTERNAL\n").unwrap();

    // `:wall` writes the buffers it safely can (b), but stops at the first one
    // that changed on disk: it switches to that buffer and warns instead of
    // clobbering it.
    command(&rpc, "wall").await;
    assert!(
        message(&rpc, &mut incoming)
            .await
            .contains("changed on disk"),
        "expected a clobber warning"
    );
    assert_eq!(current_buf(&rpc).await, 1, "must switch to the conflict");
    assert_eq!(
        std::fs::read_to_string(&a).unwrap(),
        "EXTERNAL\n",
        "the conflicting file must be left untouched"
    );
    assert_eq!(
        std::fs::read_to_string(&b).unwrap(),
        "b1\nBBB\n",
        "the non-conflicting buffer is still saved"
    );

    // `:wall!` forces every buffer through, clobbering the external change.
    command(&rpc, "wall!").await;
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "a1\nAAA\n");

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn quit_warns_and_shows_a_modified_buffer_instead_of_losing_it() {
    // `:q` quits the editor only when nothing is unsaved; if a buffer is
    // modified it switches the window to that buffer and warns (E37), rather
    // than quitting and dropping the change.
    let a = temp_file("a", "a1\na2\n");
    let b = temp_file("b", "b1\nb2\n");
    let (rpc, mut incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await; // buffer 1
    feed(&rpc, "oAAA<Esc>"); // a modified
    command(&rpc, &format!("e {}", name(&b))).await; // buffer 2 (current, clean)

    // `:q` from the clean buffer b: a is still modified, so it must not quit.
    // It surfaces a (switching to it) and warns.
    command(&rpc, "q").await;
    let msg = message(&rpc, &mut incoming).await;
    assert!(msg.starts_with("E37"), "expected E37, got {msg:?}");
    assert_eq!(
        lines(&rpc).await,
        vec!["a1", "AAA", "a2"],
        "`:q` should switch to and show the modified buffer"
    );
    assert_eq!(
        list_bufs(&rpc).await,
        vec![1, 2],
        "nothing should be closed"
    );

    // Now showing the modified buffer a; `:q` again still warns (a is current).
    command(&rpc, "q").await;
    assert!(message(&rpc, &mut incoming).await.starts_with("E37"));

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn ls_enter_jumps_to_the_selected_buffer() {
    let a = temp_file("a", "a1\n");
    let b = temp_file("b", "b1\n");
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await; // buffer 1
    command(&rpc, &format!("e {}", name(&b))).await; // buffer 2 (current)

    // `:ls` opens the `[Buffers]` listing with the current buffer (buffer 2, row
    // index 1) under the cursor; its rows are id-sorted, so `k` moves up to buffer 1
    // (a). `<CR>` is a buffer-local map that parses the leading buffer number off the
    // cursor line and switches to that buffer.
    command(&rpc, "ls").await;
    feed(&rpc, "k<CR>");

    assert_eq!(
        current_buf(&rpc).await,
        1,
        "selected buffer becomes current"
    );
    assert_eq!(lines(&rpc).await, vec!["a1"]);

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

// ----- filename expansion in ex-command arguments (`%`, `#`, modifiers) --------

#[tokio::test]
async fn percent_expands_to_the_current_file_in_ex_commands() {
    // `%` in a file-taking ex-command stands for the current file's name. `:e %`
    // therefore re-edits the file in place — it must not open a *new* buffer named
    // literally `%`.
    let a = temp_file("pct", "x1\nx2\n");
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await;
    assert_eq!(list_bufs(&rpc).await, vec![1]);

    command(&rpc, "e %").await;
    assert_eq!(lines(&rpc).await, vec!["x1", "x2"]);
    assert_eq!(
        list_bufs(&rpc).await,
        vec![1],
        "`:e %` re-edits the current file rather than opening a buffer named '%'"
    );

    std::fs::remove_file(&a).ok();
}

#[tokio::test]
async fn percent_modifiers_expand_in_a_write_target() {
    // `%:r` is the current file with its extension stripped; `:w %:r.bak` writes a
    // sibling `<base>.bak`. This exercises `%` plus a `:r` modifier plus trailing
    // text concatenation in one argument.
    let a = temp_file("wmod", "hello\nworld\n");
    let bak = a.with_extension("bak");
    std::fs::remove_file(&bak).ok();
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await;
    command(&rpc, "w %:r.bak").await;

    let written =
        std::fs::read_to_string(&bak).expect("`:w %:r.bak` should write the derived file");
    assert_eq!(written, "hello\nworld\n");

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&bak).ok();
}

#[tokio::test]
async fn hash_expands_to_the_alternate_file() {
    // `#` is the alternate file. After editing `a` then `b`, the alternate is `a`,
    // so `:e #` jumps back to it (the `<C-^>` target, spelled as a filename arg).
    let a = temp_file("halt", "a1\na2\n");
    let b = temp_file("halt", "b1\nb2\n");
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await;
    command(&rpc, &format!("e {}", name(&b))).await;

    command(&rpc, "e #").await;
    assert_eq!(lines(&rpc).await, vec!["a1", "a2"]);

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}
