//! A language server that keeps crashing until the manager's circuit breaker
//! gives up must be startable again: the edit-host's lazy-start guard
//! (`lsp_ensured`) has to clear on `ServerExited`, so the next
//! `vim.lsp.start` / FileType dispatch re-`ensure`s a fresh supervisor instead
//! of being swallowed by the guard forever.
//!
//! Black-box per the project conventions: a real server over RPC, a real (tiny,
//! deliberately crashing) "language server" script whose spawn count is the
//! observable — each spawn appends one line to a log file.

use std::path::Path;
use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{attach, command, exec_lua, spawn, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start(dir: &Path) -> (Rpc, UnboundedReceiver<Incoming>) {
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// How many times the crashing server was spawned so far.
fn spawn_count(log: &Path) -> usize {
    std::fs::read_to_string(log)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

/// Wait until the spawn count stops growing (the breaker gave up), returning the
/// settled count. Panics if it never settles within the deadline.
async fn settled_spawn_count(log: &Path) -> usize {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut last = spawn_count(log);
    let mut stable_for = 0u32;
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let now = spawn_count(log);
        if now == last {
            stable_for += 1;
            // The breaker's max backoff between attempts in this window is 1.6s,
            // so 2.5s of no growth means it has given up.
            if stable_for >= 10 {
                return now;
            }
        } else {
            stable_for = 0;
            last = now;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the crashing server's spawn count never settled (breaker never gave up?)"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_given_up_server_is_respawned_by_a_later_start() {
    let dir = temp_dir("lsp-respawn");
    let log = dir.as_path().join("spawns.log");
    let script = dir.as_path().join("crashy.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho spawned >> '{}'\nexit 1\n", log.display()),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let file = dir.as_path().join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();

    let (rpc, _incoming) = start(dir.as_path()).await;
    command(&rpc, &format!("e {}", file.display())).await;

    // Bind the buffer to the (crashing) server and let the breaker run out.
    let start_lua = format!(
        "vim.lsp.start({{ name = 'crashy', cmd = {{ '{}' }}, root_dir = '{}' }})",
        script.display(),
        dir.as_path().display()
    );
    exec_lua(&rpc, &start_lua).await;
    let given_up = settled_spawn_count(&log).await;
    assert!(given_up >= 1, "the server was spawned at least once");

    // A later start for the same (name, root) — what the next FileType dispatch
    // does — must re-`ensure` the server: the guard cleared on exit, and the
    // manager's breaker gave the key up, so this spawns a fresh supervisor.
    exec_lua(&rpc, &start_lua).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while spawn_count(&log) <= given_up {
        assert!(
            std::time::Instant::now() < deadline,
            "a vim.lsp.start after the breaker gave up never respawned the server \
             (spawn count stuck at {given_up})"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
