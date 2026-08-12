//! `btv.lsp.restart(name)` tears down and respawns a running server, re-starting it
//! from the config in force NOW — so a `btv.lsp.config` change made after the server
//! started actually takes effect (the motivating case is efm-langserver, whose
//! `languages` map is read only at spawn).
//!
//! Black-box per the project conventions: a real server over RPC, a tiny script
//! "server" that stays alive (so it is never auto-respawned) and appends its argv to
//! a log on each spawn. The log — spawn count and the args each spawn saw — is the
//! observable.

use std::path::Path;
use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{attach, command, exec_lua, spawn, temp_dir};
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

/// The argv line logged by each spawn, in order.
fn spawn_lines(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .map(|s| s.lines().map(str::trim).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Wait until the log has at least `n` lines, returning them. Panics on timeout.
async fn wait_for_lines(log: &Path, n: usize) -> Vec<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let lines = spawn_lines(log);
        if lines.len() >= n {
            return lines;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "log never reached {n} line(s); saw {:?}",
            spawn_lines(log)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn restart_respawns_the_server_with_the_latest_config() {
    let dir = temp_dir("lsp-restart");
    let log = dir.as_path().join("spawns.log");
    let script = dir.as_path().join("srv.sh");
    // Log the argv, then stay alive reading stdin (a live server is never
    // auto-respawned, so the ONLY growth in the log is our restart).
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\ncat >/dev/null\n",
            log.display()
        ),
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

    let start_v = |v: &str| {
        format!(
            "btv.lsp.start({{ name = 'srv', cmd = {{ '{}', '{}' }}, root_dir = '{}' }}, {{ filetype = 'rust' }})",
            script.display(),
            v,
            dir.as_path().display()
        )
    };

    // 1) Start the server with config "v1".
    exec_lua(&rpc, &start_v("v1")).await;
    let lines = wait_for_lines(&log, 1).await;
    assert_eq!(lines, vec!["v1"], "first spawn ran with the v1 config");

    // 2) Change the config to "v2" (a fresh start op refreshes the remembered spawn
    //    but does NOT respawn a still-running server), then restart.
    exec_lua(&rpc, &start_v("v2")).await;
    // The running server is untouched by a re-start of an ensured key.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        spawn_lines(&log),
        vec!["v1"],
        "config change alone must not respawn"
    );

    exec_lua(&rpc, "btv.lsp.restart('srv')").await;

    // 3) The restart respawns — with the NEW (v2) config.
    let lines = wait_for_lines(&log, 2).await;
    assert_eq!(
        lines,
        vec!["v1", "v2"],
        "restart respawned the server with the config in force now"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn restart_is_a_noop_when_nothing_is_running() {
    let dir = temp_dir("lsp-restart-noop");
    let (rpc, _incoming) = start(dir.as_path()).await;
    // No server named 'ghost' has ever started; restarting it must not error.
    exec_lua(&rpc, "btv.lsp.restart('ghost')").await;
    // The editor is still responsive afterward.
    let out = exec_lua(&rpc, "return 1 + 1").await;
    assert_eq!(out.as_i64(), Some(2));
}

/// `btv.lsp.stop(name)` — the stopping half, which backs `:LspStop`. Built on the same
/// script server: after a stop, a *restart* has nothing left to respawn, so the log
/// stops growing. That's the observable that distinguishes a real shutdown from
/// `btv.lsp.disable`, which only closes the gate on future starts.
#[cfg(unix)]
#[tokio::test]
async fn stop_shuts_the_server_down_so_a_restart_has_nothing_to_respawn() {
    let dir = temp_dir("lsp-stop");
    let log = dir.as_path().join("spawns.log");
    let script = dir.as_path().join("srv.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\ncat >/dev/null\n",
            log.display()
        ),
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
    exec_lua(
        &rpc,
        &format!(
            "btv.lsp.start({{ name = 'srv', cmd = {{ '{}', 'v1' }}, root_dir = '{}' }}, {{ filetype = 'rust' }})",
            script.display(),
            dir.as_path().display()
        ),
    )
    .await;
    assert_eq!(wait_for_lines(&log, 1).await, vec!["v1"]);

    // Stop it, and record how many were stopped — a caller needs that to say "no
    // server named X is running" instead of reporting a silent success.
    exec_lua(
        &rpc,
        "btv.lsp.stop('srv'):next(function(n) _G.stopped = n end)",
    )
    .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(n) = exec_lua(&rpc, "return _G.stopped").await.as_i64() {
            assert_eq!(n, 1, "exactly the one running server was stopped");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "stop never settled");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The proof it really went down: a restart re-ensures whatever is *running*, and
    // after a stop that is nothing — so the log stays at one line.
    exec_lua(&rpc, "btv.lsp.restart('srv')").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        spawn_lines(&log),
        vec!["v1"],
        "a stopped server must not be respawned by a later restart"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn stop_resolves_zero_when_nothing_is_running() {
    let dir = temp_dir("lsp-stop-noop");
    let (rpc, _incoming) = start(dir.as_path()).await;
    // The count is what lets `:LspStop ghost` say so rather than claim success.
    exec_lua(
        &rpc,
        "btv.lsp.stop('ghost'):next(function(n) _G.stopped = n end)",
    )
    .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(n) = exec_lua(&rpc, "return _G.stopped").await.as_i64() {
            assert_eq!(n, 0);
            return;
        }
        assert!(std::time::Instant::now() < deadline, "stop never settled");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
