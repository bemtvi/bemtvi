//! Shada persistence — cross-session state survives a server restart.
//!
//! Black-box, per the harness convention: spawn a server against a **temp** state
//! dir (so the real `~/.local/state` is never touched and the test stays
//! hermetic), drive it with `nvim_input`, quit, then **respawn** a second server
//! against the same dir and assert the first session's state was restored.
//!
//! Phase 1 covers registers only. See
//! `docs/plans/2026-06-11-shada-persistence.md`.

use std::path::Path;

use nxvim_server::ServerInit;
use nxvim_test_harness::{feed, lines, start_attached, temp_dir, write_temp};
use tokio::sync::mpsc::UnboundedReceiver;

/// A server wired to persist into `dir`. Everything else default.
fn init_with_state(dir: &Path, file: Option<String>) -> ServerInit {
    ServerInit {
        file,
        state_dir: Some(dir.to_path_buf()),
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

#[tokio::test]
async fn register_survives_a_restart() {
    let dir = temp_dir("shada_registers");
    let file = write_temp("shada_registers", "txt", "hello world\n");

    // Session 1: yank "hello" into register `a`, then quit.
    {
        let (rpc, incoming) = start_attached(init_with_state(&dir, Some(file)), 80, 25).await;
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
        let (rpc, _incoming) = start_attached(init_with_state(&dir, None), 80, 25).await;
        feed(&rpc, "\"ap");
        assert_eq!(lines(&rpc).await, vec!["hello"]);
    }
}

#[tokio::test]
async fn no_state_dir_means_no_persistence() {
    // With `state_dir: None` (the default), a register set in one session must NOT
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
