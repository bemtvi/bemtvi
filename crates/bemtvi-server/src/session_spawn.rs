//! Client-side session-spawn scaffolding shared by the UI binaries (the TUI's
//! `bemtvi` and `bemtvi-gui`). Every native client builds a session the same way:
//! run the embedded server on its own thread over an in-process duplex, block the
//! caller until the daemon handshake + config resolution succeed
//! ([`spawn_session_thread`]), and — for a stdio daemon split — hold the current
//! child on the reconnecting link so each (re)dial re-spawns it
//! ([`connect_daemon_respawning`]). The pieces lived copy-pasted in both binaries
//! (with the drift risk that implies); what stays per client is only *what* to
//! spawn (the daemon command line) and *what* the session init looks like.

use std::future::Future;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context as _};

use crate::daemon::connect_daemon_reconnecting;
use crate::{DaemonClient, ReconnectHandle, ReconnectPolicy, ServerInit};

/// Env var naming the command to spawn as the stdio daemon. Run through `sh -c`,
/// so a full command line works verbatim — e.g.
/// `BEMTVI_DAEMON_CMD="ssh host bemtvi --daemon"`. Unset = each client's default
/// (the TUI re-invokes itself with `--daemon`; the GUI spawns the sibling `bemtvi`
/// binary).
pub const DAEMON_CMD_ENV: &str = "BEMTVI_DAEMON_CMD";

/// The `$BEMTVI_DAEMON_CMD` daemon command, if the env var is set: the command
/// line through `sh -c`, so a full pipeline/remote invocation works verbatim.
/// `None` when unset (the caller falls back to its own default daemon binary).
pub fn env_daemon_command() -> Option<tokio::process::Command> {
    std::env::var_os(DAEMON_CMD_ENV).map(|cmd| {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    })
}

/// Open the daemon's stderr log as a **private, symlink-safe** file under the temp
/// dir. The daemon's diagnostics can't share the terminal/stderr with the client,
/// so they go here; `tail` it to debug the daemon side. Two properties matter on a
/// shared `/tmp`:
///
/// - **No symlink clobber (CWE-377).** A fixed name (`bemtvi-daemon.log`) opened
///   with the symlink-following [`File::create`](std::fs::File::create) let
///   another local user pre-plant a symlink at that path and have the daemon's
///   stderr *truncate* one of the victim's files. The path is per-pid and opened
///   with `create_new` (`O_CREAT | O_EXCL`), which refuses to follow a symlink;
///   any leftover (only ever *our own* per-pid name) is removed first, and if the
///   create still fails — e.g. an attacker re-planted the name under `/tmp`'s
///   sticky bit — stderr is discarded rather than written through a hostile path.
/// - **Not world-readable.** Created `0600` so daemon diagnostics (paths, errors)
///   aren't exposed to other users of a shared temp dir.
pub fn daemon_log_stderr() -> Stdio {
    let path = std::env::temp_dir().join(format!("bemtvi-daemon-{}.log", std::process::id()));
    // Best-effort: clear a stale file from a prior same-pid run (only ever ours). If
    // another user owns the name under the sticky bit this fails harmlessly — the
    // `create_new` below then also fails and we fall back to discarding stderr.
    let _ = std::fs::remove_file(&path);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(&path)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null())
}

/// Connect to a stdio daemon over a **reconnectable, re-spawning** child link:
/// each (re)dial builds a fresh command via `make_command` and spawns it with its
/// stdout/stdin as the wire (`spawn_context` labels a spawn failure). The current
/// child lives on the link thread — each dial replaces the previous one in the
/// slot, reaping it via `kill_on_drop` — so a dropped link (the daemon died, or a
/// `ssh …` hop dropped on sleep) re-establishes the seams in place, keeping the
/// editor's local state. `make_command` chooses everything else about the child
/// (program, args, stderr — e.g. [`daemon_log_stderr`]); stdin/stdout piping and
/// `kill_on_drop` are forced here because the wire depends on them.
pub fn connect_daemon_respawning<F>(
    spawn_context: &'static str,
    mut make_command: F,
    policy: ReconnectPolicy,
) -> anyhow::Result<(DaemonClient, ReconnectHandle)>
where
    F: FnMut() -> anyhow::Result<tokio::process::Command> + Send + 'static,
{
    let slot: Arc<Mutex<Option<tokio::process::Child>>> = Arc::new(Mutex::new(None));
    let make = move || {
        let slot = slot.clone();
        let cmd = make_command();
        async move {
            let mut child = cmd?
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .context(spawn_context)?;
            let stdout = child.stdout.take().expect("daemon child stdout piped");
            let stdin = child.stdin.take().expect("daemon child stdin piped");
            // Replace (and so reap) the previous child, keeping this one alive for
            // the connection's lifetime on the link thread.
            *slot.lock().unwrap() = Some(child);
            Ok((stdout, stdin))
        }
    };
    connect_daemon_reconnecting(make, policy)
}

/// An opaque value kept alive on the server thread for the whole session (e.g. a
/// process guard); `Box::new(())` when there is nothing to hold.
pub type SessionGuard = Box<dyn std::any::Any + Send>;

/// Build the in-process editor↔client duplex, run the embedded server on its own
/// thread, and **block until `setup` (the daemon handshake + config resolution +
/// [`ServerInit`] assembly) succeeds** — so a connect failure is an `Err` HERE,
/// before the UI starts or a session swap commits, leaving any current session
/// intact. Returns the client transport + the server thread handle (joined by the
/// caller; a server error is its `Err`). `setup` runs inside the server thread's
/// runtime and yields the init plus a [`SessionGuard`] kept alive until the editor
/// quits.
pub fn spawn_session_thread<F, Fut>(
    setup: F,
) -> anyhow::Result<(
    tokio::io::DuplexStream,
    std::thread::JoinHandle<anyhow::Result<()>>,
)>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<(ServerInit, SessionGuard)>>,
{
    let (server_end, client_end) = tokio::io::duplex(1 << 16);
    // Carries only the handshake/setup outcome back here; the thread then owns the
    // duplex's server end + the session guard for the session's lifetime.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
    let server_thread = std::thread::spawn(move || -> anyhow::Result<()> {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = ready_tx.send(Err(anyhow!(e).context("building the server runtime")));
                return Ok(());
            }
        };
        runtime.block_on(async move {
            // Connect + resolve config/shada + assemble the ServerInit. A failure is
            // reported to the waiting caller via `ready_tx` (so it returns Err and
            // keeps the current session) and ends the thread cleanly — the error is
            // surfaced once, not again at join. `_guard` lives until the editor quits.
            let (init, _guard) = match setup().await {
                Ok(pair) => pair,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return Ok(());
                }
            };
            if ready_tx.send(Ok(())).is_err() {
                return Ok(()); // caller gave up waiting; nothing to serve
            }
            crate::run(server_end, init)
                .await
                .map_err(|err| anyhow!("editor server error: {err}"))
        })
    });

    match ready_rx.recv() {
        Ok(Ok(())) => Ok((client_end, server_thread)),
        // The thread reported a handshake/setup failure and is winding down; join it
        // and surface the error rather than returning a dead session.
        Ok(Err(e)) => {
            let _ = server_thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = server_thread.join();
            Err(anyhow!("editor server thread exited before connecting"))
        }
    }
}
