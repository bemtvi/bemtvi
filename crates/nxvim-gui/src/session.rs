//! Building an editor **session** for the GUI: a local `nxvim_server::run` on its own
//! thread, joined to the window over an in-process duplex, with its fs/process/watch/LSP
//! host seams pointed at the local disk (the embedded default) or a remote `--daemon`
//! (the *edit-host split*). [`spawn_session`] is the single place sessions are born — at
//! startup and again on every `:connect` — so the [`ServerInit`] wiring lives once.
//!
//! The editor (core + Lua + the keystroke path) always runs **local** over the duplex;
//! only the host seams cross the wire. So `:connect` swaps the seams (a fresh local
//! server on a daemon), not the editor transport — see [`crate::run`]'s session loop.

use std::process::Stdio;
use std::thread::JoinHandle;

use anyhow::{anyhow, Result};
use nxvim_server::{
    connect_daemon, connect_quic, run as run_server, ClipboardProvider, ConfigSource, DaemonClient,
    ServerInit,
};
use tokio::io::DuplexStream;

use crate::remote::{ssh_daemon_command, ConnectTarget};

/// Env var naming the command to spawn as the daemon for a `--connect-daemon` startup.
/// Run through `sh -c`, so a full command line works verbatim — e.g.
/// `NXVIM_DAEMON_CMD="ssh host nxvim --daemon"`. Unset = spawn the sibling `nxvim`
/// binary (`--daemon`) for a local two-process split over stdio pipes.
const DAEMON_CMD_ENV: &str = "NXVIM_DAEMON_CMD";

/// A live editor session: the GUI's in-process duplex to the local server, plus the
/// server thread's [`JoinHandle`] kept for the session's lifetime. Dropping the duplex
/// (the window or a `:connect` swap) lets the server see EOF and wind down; the handle
/// is then joined to reap the thread and surface a panic.
pub struct Session {
    /// The editor RPC transport the window drives (always an in-process duplex).
    pub stream: DuplexStream,
    /// Whether this is an edit-host (daemon) session — buffers live on the daemon's fs.
    /// Drives the GUI's local-file-dialog suppression (see [`crate::dialog_action`]).
    pub remote: bool,
    /// The server thread, joined at teardown (or after the swap that retired it).
    pub handle: JoinHandle<()>,
}

/// Spawn the local server for `target` (`None` = embedded, local disk) on its own
/// thread and return the GUI [`Session`]. **Blocking**: for a daemon target it waits for
/// the ssh/quic handshake, so a connect failure (bad host, refused auth, bad cert) is an
/// `Err` here — before any window opens or `:connect` swap happens. The embedded target
/// never fails the connect step.
///
/// `file` is the buffer to open (the CLI/`:connect` file, or a daemon target's embedded
/// `/file`). The daemon child (ssh) lives inside the server thread with `kill_on_drop`,
/// so it is reaped when the session ends.
pub fn spawn_session(
    target: Option<ConnectTarget>,
    file: Option<String>,
    config_source: ConfigSource,
) -> Result<Session> {
    let remote = target.is_some();
    spawn_server(remote, file, config_source, move || async move {
        // Build the host seams: `None` for the embedded local disk, or a daemon client
        // (ssh stdio / quic) for the edit-host split. The ssh child is the session guard.
        match target {
            None => Ok((None, no_guard())),
            Some(ConnectTarget::Ssh(spec)) => {
                // `ssh … nxvim --daemon` over stdio: the child's stdout/stdin *is* the
                // daemon wire. `connect_daemon` drives it for the five host seams.
                let mut child = ssh_daemon_command(&spec).spawn().map_err(|e| {
                    anyhow!(e).context("spawning ssh (is it installed and on PATH?)")
                })?;
                let stdout = child.stdout.take().expect("ssh stdout piped");
                let stdin = child.stdin.take().expect("ssh stdin piped");
                Ok((Some(connect_daemon(stdout, stdin)), guard(child)))
            }
            Some(ConnectTarget::Quic(uri)) => {
                let (url, cert_hash, token) = parse_connect_uri(&uri)?;
                // `connect_quic` blocks on its own link thread until the QUIC +
                // WebTransport session is up, then returns the five seams (or fails loud).
                Ok((Some(connect_quic(&url, &cert_hash, &token)?), no_guard()))
            }
        }
    })
}

/// Spawn the daemon **stdio** child for a `--connect-daemon` startup (no explicit
/// target): `$NXVIM_DAEMON_CMD` through `sh -c`, else the sibling `nxvim --daemon`. Its
/// stderr goes to a temp log (it can't share this binary's stderr cleanly); `tail` it to
/// debug the daemon side. The returned [`Session`] is a daemon session (`remote = true`).
pub fn spawn_stdio_daemon_session(
    file: Option<String>,
    config_source: ConfigSource,
) -> Result<Session> {
    spawn_server(true, file, config_source, || async {
        let mut child = spawn_stdio_daemon()?;
        let stdout = child.stdout.take().expect("daemon stdout piped");
        let stdin = child.stdin.take().expect("daemon stdin piped");
        Ok((Some(connect_daemon(stdout, stdin)), guard(child)))
    })
}

/// A session guard: an opaque value kept alive on the server thread for the whole
/// session — the daemon child (so `kill_on_drop` reaps it on quit), or nothing.
type Guard = Box<dyn std::any::Any + Send>;

/// The empty guard for a local or quic session (quic owns its link thread internally).
fn no_guard() -> Guard {
    Box::new(())
}

/// Box a value (the daemon child) as a session [`Guard`].
fn guard(value: impl std::any::Any + Send) -> Guard {
    Box::new(value)
}

/// The shared session-spawn scaffolding: run the editor server on its own thread with
/// host seams from `connect`, joined to the window over an in-process duplex whose client
/// end is returned. **Blocking** on the daemon handshake: `connect` runs first inside the
/// server runtime, and its outcome is relayed back so a failed connect is an `Err` here
/// (before any window/swap) rather than a dead session. `connect` yields the optional
/// [`DaemonClient`] (`None` = local disk) plus a [`Guard`] kept alive for the session.
fn spawn_server<F, Fut>(
    remote: bool,
    file: Option<String>,
    config_source: ConfigSource,
    connect: F,
) -> Result<Session>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(Option<DaemonClient>, Guard)>>,
{
    let (server_end, client_end) = tokio::io::duplex(1 << 16);
    // `ready` carries only the handshake outcome back here; the server thread then owns
    // the duplex's `server_end` and the daemon child for the session's lifetime.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
    let handle = std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = ready_tx.send(Err(anyhow!(e).context("building the server runtime")));
                return;
            }
        };
        runtime.block_on(async move {
            let (client, _guard) = match connect().await {
                Ok(parts) => parts,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            // Build the server config — for a daemon session this fetches + materializes
            // the remote's config/plugins, so a failure here is a setup failure the caller
            // must see (before we report the handshake up).
            let init = match server_init(file, client, config_source).await {
                Ok(init) => init,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            // The handshake is up and config is staged: unblock the caller.
            if ready_tx.send(Ok(())).is_err() {
                return; // caller gave up waiting; nothing to serve
            }
            // `_guard` (the daemon child, or nothing) lives until the editor quits.
            if let Err(e) = run_server(server_end, init).await {
                eprintln!("nxvim-gui: server error: {e}");
            }
        });
    });

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Session {
            stream: client_end,
            remote,
            handle,
        }),
        // The server thread reported a handshake/setup failure and exited; join it (it's
        // already done) and surface the error instead of returning a dead session.
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e)
        }
        Err(_) => {
            let _ = handle.join();
            Err(anyhow!("nxvim-gui: server thread exited before connecting"))
        }
    }
}

/// Spawn the stdio daemon child (see [`spawn_stdio_daemon_session`]). Must run inside a
/// tokio runtime (the child's pipes bind to it).
fn spawn_stdio_daemon() -> Result<tokio::process::Child> {
    use tokio::process::Command;
    let log_path = std::env::temp_dir().join("nxvim-daemon.log");
    let stderr = std::fs::File::create(&log_path)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());
    let mut cmd = if let Some(cmd) = std::env::var_os(DAEMON_CMD_ENV) {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    } else {
        let nxvim = std::env::current_exe()?.with_file_name(nxvim_bin_name());
        if !nxvim.exists() {
            return Err(anyhow!(
                "no sibling `nxvim` daemon binary at {} — install nxvim alongside \
                 nxvim-gui, or set {DAEMON_CMD_ENV} (e.g. \"ssh host nxvim --daemon\")",
                nxvim.display()
            ));
        }
        let mut c = Command::new(nxvim);
        c.arg("--daemon");
        c
    };
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow!(e).context("spawning the daemon"))
}

/// The sibling daemon binary's file name (`nxvim.exe` on Windows, `nxvim` elsewhere).
fn nxvim_bin_name() -> &'static str {
    if cfg!(windows) {
        "nxvim.exe"
    } else {
        "nxvim"
    }
}

/// The [`ServerInit`] for a GUI session. The editor + Lua + shada always run locally
/// (the keystroke path stays local); what varies is the host seams and where config
/// comes from. `client = None` is the embedded local disk (config from local disk).
/// `Some(daemon)` is the edit-host split: fs/process/watch/LSP/Lua-fs route to the
/// daemon, and the config is resolved from `config_source` — `Remote` **fetches** the
/// daemon's config + plugins (materialized locally), `Local` runs this machine's config;
/// either way the daemon's cwd / tree-sitter parser set are mirrored — matching the TUI.
/// A failed resolve is loud (the caller surfaces it as a handshake error) rather than a
/// silent fall back to local config.
async fn server_init(
    file: Option<String>,
    client: Option<DaemonClient>,
    config_source: ConfigSource,
) -> Result<ServerInit> {
    let mut ts_autoinstall = Vec::new();
    // The daemon's cwd seeds `DirState` so `:pwd`/`:cd`/`getcwd` operate on the remote
    // dir in a daemon session (`docs/plans/2026-06-23-remote-cwd.md`); `None` for a local
    // session, which seeds from the local process cwd.
    let mut remote_cwd = None;
    // The on-daemon shada sync target for a `Remote`-config session (Approach A); `None`
    // for a local-shada session.
    let mut remote_shada = None;
    let (
        config_dir,
        runtimepath,
        shada_store,
        host_fs,
        host_proc,
        host_fs_async,
        lsp_transport,
        fs_jobs,
    ) = match client {
        None => {
            let (config_dir, runtimepath) = nxvim_server::default_runtime();
            (
                config_dir,
                runtimepath,
                nxvim_server::default_shada(),
                None,
                None,
                None,
                None,
                None,
            )
        }
        Some(c) => {
            // `Remote` materializes the daemon's config + plugins locally; `Local`
            // (the native default) runs this machine's own config and fetches only the
            // daemon's cwd / parser set. A failed resolve is loud (the caller surfaces
            // it as a handshake error), not a silent fall back to local config.
            let resolved = c.config.resolve(config_source).await.map_err(|e| {
                anyhow!("could not resolve the session config from the daemon: {e}")
            })?;
            ts_autoinstall = resolved.ts_autoinstall;
            remote_cwd = resolved.remote_cwd;
            // Shada follows the config source: `Remote` keeps it on the daemon
            // (download now over `c.host_fs`, before it is moved below; sync back after
            // each flush), `Local` uses the local store. A download error falls back to
            // local rather than clobbering the daemon's copy.
            let (store, rs) = nxvim_server::resolve_session_shada(
                &c.host_fs,
                config_source,
                resolved.state_dir.as_deref(),
                // The GUI has no `--shada-namespace` yet (v1: TUI only).
                None,
                nxvim_server::default_shada(),
            )
            .await;
            remote_shada = rs;
            (
                // A daemon session leaves the synchronous `host_fs` unused — every fs
                // read goes through the async daemon seam below — so it stays `None`.
                resolved.config_dir,
                resolved.runtimepath,
                store,
                None,
                Some(Box::new(c.host_proc) as _),
                Some(Box::new(c.host_fs) as _),
                Some(Box::new(c.lsp_transport) as _),
                Some(c.fs_jobs),
            )
        }
    };
    Ok(ServerInit {
        file,
        config_dir,
        // Editor + Lua run locally; shada is local for a `Local`-config session and on the
        // daemon for a `Remote`-config one (Approach A — synced over the fs seam).
        shada: Some(shada_store),
        remote_shada,
        // The GUI front end does not yet pass `--shada-namespace`, so it uses the global
        // store and never captures/restores the editor session (v1: TUI only).
        workspace_session: false,
        restore_session: false,
        session_save_layout: false,
        // The GUI front end is TUI-parity minus sessions (v1) — no workspace identity.
        shada_namespace: None,
        workspace_dir: None,
        runtimepath,
        clipboard: ClipboardProvider::System,
        mouse_clock: None,
        host_fs,
        host_proc,
        host_fs_async,
        lsp_transport,
        fs_jobs,
        // The GUI is the interactive editor — offer the built-in default recommended
        // set on a fresh setup (a config's own recommend{} still overrides it), and
        // enable command-line completion by default (a config's setup{} still wins).
        offer_default_recommended: true,
        cmdline_complete_default: true,
        ts_autoinstall,
        remote_cwd,
    })
}

/// Parse a `nxvim://HOST:PORT/TOKEN?cert=HASH` connect URI into the pieces
/// [`connect_quic`] needs: the `https://HOST:PORT` dial URL (WebTransport requires the
/// `https` scheme), the bearer `TOKEN` (the path), and the TOFU cert `HASH` (the `cert`
/// query). Fails loud on a malformed URI rather than dialing a half-specified target.
pub fn parse_connect_uri(uri: &str) -> Result<(String, String, String)> {
    let rest = uri
        .strip_prefix("nxvim://")
        .ok_or_else(|| anyhow!("daemon connect URI must start with nxvim:// : {uri:?}"))?;
    let (authority, after) = rest
        .split_once('/')
        .ok_or_else(|| anyhow!("daemon connect URI is missing the /TOKEN path: {uri:?}"))?;
    if authority.is_empty() {
        return Err(anyhow!("daemon connect URI is missing HOST:PORT: {uri:?}"));
    }
    let (token, query) = after
        .split_once('?')
        .ok_or_else(|| anyhow!("daemon connect URI is missing the ?cert=HASH query: {uri:?}"))?;
    if token.is_empty() {
        return Err(anyhow!("daemon connect URI has an empty TOKEN: {uri:?}"));
    }
    let cert_hash = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("cert="))
        .filter(|h| !h.is_empty())
        .ok_or_else(|| anyhow!("daemon connect URI is missing cert=HASH: {uri:?}"))?;
    Ok((
        format!("https://{authority}"),
        cert_hash.to_owned(),
        token.to_owned(),
    ))
}
