//! Shada persistence — cross-session state survives a server restart, and the
//! per-instance stores compact rather than accumulating.
//!
//! Black-box, per the harness convention: spawn a server against a **temp** state
//! dir (so the real `~/.local/state` is never touched and the test stays
//! hermetic), drive it with `nvim_input`, quit, then **respawn** a second server
//! against the same dir and assert the first session's state was restored.
//!
//! Phase 1 covers registers; Phase 2 the global file marks `A`–`Z`; Phase 3 the
//! per-file marks (incl. the `` `" `` last-cursor) and search/ex history. See
//! `docs/plans/2026-06-11-shada-persistence.md`.

use std::path::{Path, PathBuf};

use nxvim_server::{is_store_file, RedbFileStore, ServerInit};
use nxvim_test_harness::{cursor, feed, lines, start_attached, temp_dir, write_temp};
use tokio::sync::mpsc::UnboundedReceiver;

/// A server that persists into `dir` via the native redb store.
fn init_with_store(dir: &Path, file: Option<String>) -> ServerInit {
    ServerInit {
        file,
        shada: Some(Box::new(RedbFileStore::new(dir.to_path_buf()))),
        ..Default::default()
    }
}

/// Drain the client's incoming channel until it closes. The channel closes only
/// when the server thread has fully returned from `run_server` — which happens
/// *after* the final shada flush — so awaiting this is a reliable "the store has
/// been written" barrier, with no reliance on wall-clock timing.
async fn await_server_exit(mut incoming: UnboundedReceiver<nxvim_rpc::Incoming>) {
    while incoming.recv().await.is_some() {}
}

/// Count the surviving shada store files in `dir`.
fn store_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_store_file(p))
        .collect()
}

#[tokio::test]
async fn register_survives_a_restart() {
    let dir = temp_dir("shada_registers");
    let file = write_temp("shada_registers", "txt", "hello world\n");

    // Session 1: yank "hello" into register `a`, then quit.
    {
        let (rpc, incoming) = start_attached(init_with_store(&dir, Some(file)), 80, 25).await;
        feed(&rpc, "\"ayiw");
        // Barrier: ensure the yank is processed before we quit.
        assert_eq!(lines(&rpc).await, vec!["hello world"]);
        feed(&rpc, ":qa!<CR>");
        // Wait for the server to fully exit — guarantees the store was flushed.
        await_server_exit(incoming).await;
    }

    // Session 2: a fresh server against the same store. Register `a` should hold
    // "hello" from the previous session, so `"ap` pastes it.
    {
        let (rpc, _incoming) = start_attached(init_with_store(&dir, None), 80, 25).await;
        feed(&rpc, "\"ap");
        assert_eq!(lines(&rpc).await, vec!["hello"]);
    }
}

#[tokio::test]
async fn stores_compact_instead_of_accumulating() {
    let dir = temp_dir("shada_compaction");

    // Session 1: yank into register `a`, exit. One store file is left behind.
    {
        let f = write_temp("shada_compaction", "txt", "alpha\n");
        let (rpc, incoming) = start_attached(init_with_store(&dir, Some(f)), 80, 25).await;
        feed(&rpc, "\"ayy");
        assert_eq!(lines(&rpc).await, vec!["alpha"]);
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }
    assert_eq!(
        store_files(&dir).len(),
        1,
        "one store after the first session"
    );

    // Session 2: a new instance mints its own file but absorbs + deletes session
    // 1's, so after it exits there is still exactly one file — count is bounded by
    // live instances, not total launches.
    {
        let f = write_temp("shada_compaction", "txt", "beta\n");
        let (rpc, incoming) = start_attached(init_with_store(&dir, Some(f)), 80, 25).await;
        feed(&rpc, "\"byy");
        assert_eq!(lines(&rpc).await, vec!["beta"]);
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }
    assert_eq!(
        store_files(&dir).len(),
        1,
        "still one store after the second session — the first was compacted away"
    );

    // Session 3: both registers survived the carry-forward (session 1's `a` was
    // merged into session 2's store, which also holds session 2's `b`).
    {
        let (rpc, _incoming) = start_attached(init_with_store(&dir, None), 80, 25).await;
        feed(&rpc, "\"ap\"bp");
        assert_eq!(lines(&rpc).await, vec!["", "alpha", "beta"]);
    }
}

#[tokio::test]
async fn global_mark_survives_a_restart() {
    let dir = temp_dir("shada_global_mark");
    let file = write_temp("shada_global_mark", "txt", "alpha\n  beta\ngamma\n");

    // Session 1: open the file, set global mark `A` on line 2, col 4, then quit.
    {
        let (rpc, incoming) =
            start_attached(init_with_store(&dir, Some(file.clone())), 80, 25).await;
        feed(&rpc, "ggj04l");
        assert_eq!(cursor(&rpc).await, (2, 4));
        feed(&rpc, "mA");
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: a fresh server with an *empty* buffer (no file argument). Jumping
    // to `` `A `` must lazily reopen the marked file and land at the saved spot —
    // the file was never loaded at startup, only on the jump.
    {
        let (rpc, _incoming) = start_attached(init_with_store(&dir, None), 80, 25).await;
        // Precondition: the marked file is not open yet — the buffer is empty.
        assert_eq!(lines(&rpc).await, vec![""]);
        feed(&rpc, "`A");
        assert_eq!(lines(&rpc).await, vec!["alpha", "  beta", "gamma"]);
        assert_eq!(cursor(&rpc).await, (2, 4));
    }
}

#[tokio::test]
async fn last_cursor_mark_reopens_a_file_where_it_was_left() {
    let dir = temp_dir("shada_quote_mark");
    let file = write_temp("shada_quote_mark", "txt", "alpha\nbeta\ngamma\ndelta\n");

    // Session 1: move the cursor to line 3, col 2 and quit (no edit). The current
    // buffer's live cursor becomes its `"` last-cursor mark at the exit flush.
    {
        let (rpc, incoming) =
            start_attached(init_with_store(&dir, Some(file.clone())), 80, 25).await;
        feed(&rpc, "ggjjll");
        assert_eq!(cursor(&rpc).await, (3, 2));
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: reopen the same file; the cursor starts at the top, and `` `" ``
    // returns to the saved last-cursor spot.
    {
        let (rpc, _incoming) = start_attached(init_with_store(&dir, Some(file)), 80, 25).await;
        assert_eq!(cursor(&rpc).await, (1, 0));
        feed(&rpc, "`\"");
        assert_eq!(cursor(&rpc).await, (3, 2));
    }
}

#[tokio::test]
async fn search_history_survives_a_restart() {
    let dir = temp_dir("shada_history");
    let file = write_temp("shada_history", "txt", "alpha\nbeta\ngamma\n");

    // Session 1: run a `/gamma` search (pushes the search history), then quit.
    {
        let (rpc, incoming) =
            start_attached(init_with_store(&dir, Some(file.clone())), 80, 25).await;
        feed(&rpc, "/gamma<CR>");
        assert_eq!(cursor(&rpc).await, (3, 0));
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: open `/`, recall the restored pattern with <Up>, run it — the
    // cursor lands on `gamma`, proving the history came back.
    {
        let (rpc, _incoming) = start_attached(init_with_store(&dir, Some(file)), 80, 25).await;
        assert_eq!(cursor(&rpc).await, (1, 0));
        feed(&rpc, "/<Up><CR>");
        assert_eq!(cursor(&rpc).await, (3, 0));
    }
}

#[tokio::test]
async fn no_store_means_no_persistence() {
    // With `shada: None` (the default), a register set in one session must NOT
    // bleed into the next — persistence is strictly opt-in, keeping every other
    // test hermetic.
    let file = write_temp("shada_off", "txt", "hello world\n");
    {
        let (rpc, incoming) = start_attached(
            ServerInit {
                file: Some(file),
                ..Default::default()
            },
            80,
            25,
        )
        .await;
        feed(&rpc, "\"ayiw");
        assert_eq!(lines(&rpc).await, vec!["hello world"]);
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }
    {
        let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 25).await;
        feed(&rpc, "\"ap");
        // Nothing pasted: register `a` is empty, so the buffer is unchanged.
        assert_eq!(lines(&rpc).await, vec![""]);
    }
}
