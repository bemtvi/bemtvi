//! Building an editor **session** for the GUI: a local `nxvim_server::run` on its own
//! thread, joined to the window over an in-process duplex, with its fs/process/watch/LSP
//! host seams pointed at the local disk (the embedded default) or a remote `--daemon`
//! (the *edit-host split*). [`spawn_session`] is the single place sessions are born — at
//! startup and again on every `:connect` — so the [`ServerInit`] wiring lives once.
//!
//! The editor (core + Lua + the keystroke path) always runs **local** over the duplex;
//! only the host seams cross the wire. So `:connect` swaps the seams (a fresh local
//! server on a daemon), not the editor transport — see [`crate::run`]'s session loop.

use std::path::PathBuf;
use std::thread::JoinHandle;

use anyhow::{anyhow, Result};
use nxvim_server::{
    connect_daemon_respawning, connect_quic_reconnecting, daemon_log_stderr, env_daemon_command,
    spawn_session_thread, ClipboardProvider, ConfigSource, DaemonClient, ReconnectHandle,
    ReconnectPolicy, ReconnectSpec, ReconnectTransport, ServerInit, SessionGuard, DAEMON_CMD_ENV,
};
use tokio::io::DuplexStream;

use crate::remote::{ssh_daemon_command, ConnectTarget};

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
    /// The server thread, joined at teardown (or after the swap that retired it); its
    /// `Err` is a server error the joiner reports.
    pub handle: JoinHandle<Result<()>>,
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
    spawn_server(remote, file, None, config_source, move || async move {
        // Build the host seams: `None` for the embedded local disk, or a daemon client
        // (ssh stdio / quic) for the edit-host split.
        match target {
            None => Ok((None, None, no_guard())),
            Some(ConnectTarget::Ssh(spec)) => {
                // `ssh … nxvim --daemon` over stdio: the child's stdout/stdin *is* the
                // daemon wire. Use the RE-SPAWNING link so a dropped ssh hop (laptop
                // sleep, network blip) re-runs `ssh …` and rebinds the seams in place —
                // the editor keeps its local buffers/undo.
                let (client, handle) = connect_daemon_respawning(
                    "spawning ssh (is it installed and on PATH?)",
                    move || Ok(ssh_daemon_command(&spec)),
                    ReconnectPolicy::default(),
                )?;
                Ok((Some(client), Some(handle), no_guard()))
            }
            Some(ConnectTarget::Quic(uri)) => {
                let (url, cert_hash, token) = parse_connect_uri(&uri)?;
                // `connect_quic_reconnecting` blocks on its own link thread until the QUIC +
                // WebTransport session is up, then returns the seams + a reconnect handle (or
                // fails loud). On a drop (sleep/wake, a network blip) the supervisor re-dials a
                // fresh QUIC connection under the seams, like the ssh path.
                let (client, handle) = connect_quic_reconnecting(
                    &url,
                    &cert_hash,
                    &token,
                    ReconnectPolicy::default(),
                )?;
                Ok((Some(client), Some(handle), no_guard()))
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
    spawn_server(true, file, None, config_source, || async {
        // Reconnecting like the ssh path: re-spawn the daemon command on each (re)dial,
        // holding the current child on the link thread.
        let (client, handle) = connect_daemon_respawning(
            "spawning the daemon",
            stdio_daemon_command,
            ReconnectPolicy::default(),
        )?;
        Ok((Some(client), Some(handle), no_guard()))
    })
}

/// Build the session a `nx.session.reconnect(spec)` requested (§B): the wire spec's
/// transport chooses the seam builder, and `spec.config_source` — not the session-wide
/// choice — drives config resolution, so a connector can ask for the daemon's config or
/// the local one per swap. The system-plugin tier (§A) is re-seeded by `server_init`
/// regardless. **Blocking** on the handshake, like every other builder, so a failed
/// provision/spawn is an `Err` here and the *current* session is left intact.
pub fn spawn_session_from_spec(spec: ReconnectSpec) -> Result<Session> {
    spec.reject_keep_buffers()?;
    let config_source = spec.config_source;
    match spec.transport {
        ReconnectTransport::Spawn { command } => {
            spawn_server(true, None, None, config_source, move || async move {
                // Reconnecting, like `spawn_stdio_daemon_session`: re-launch the command on
                // each (re)dial, holding the current child on the link thread. The daemon's
                // stderr goes to a private log (it can't share the GUI's terminal).
                let (client, handle) = connect_daemon_respawning(
                    "spawning the daemon",
                    move || {
                        let mut c = command.to_command();
                        c.stderr(daemon_log_stderr());
                        Ok(c)
                    },
                    ReconnectPolicy::default(),
                )?;
                Ok((Some(client), Some(handle), no_guard()))
            })
        }
        ReconnectTransport::Quic { addr } => {
            spawn_session(Some(ConnectTarget::Quic(addr)), None, config_source)
        }
    }
}

/// Spawn a local **workspace** session rooted at `dir`: an embedded (local-disk) session
/// that derives a per-directory shada namespace, cds into the directory at startup
/// (always, like the TUI's `--workspace`), and saves/restores the window/split/dock/buffer layout across
/// launches — the GUI equivalent of the TUI's `nxvim --workspace <dir>`. **Blocking**
/// (builds the server thread + config like every session). `dir` must be an existing
/// directory: a non-directory / missing path is a loud `Err` (no silent fall-through),
/// surfaced to the user via `:echoerr` just like a failed `:connect`.
pub fn spawn_workspace_session(dir: PathBuf, config_source: ConfigSource) -> Result<Session> {
    // A `:workspace` is a *directory* session; resolve to an absolute path now (so its
    // derived shada namespace + `nx.workspace` root are stable) and reject a non-directory
    // or missing path loudly, matching the TUI's `resolve_workspace_dir`. A leading `~` is
    // expanded against `$HOME` first — the user typed it in the command line, so (unlike a
    // shell argument) it is still literal and `canonicalize` would otherwise fail on it.
    let dir = expand_tilde(dir);
    let abs =
        std::fs::canonicalize(&dir).map_err(|e| anyhow!("workspace {}: {e}", dir.display()))?;
    if !abs.is_dir() {
        return Err(anyhow!(
            "workspace requires a directory, but {} is not one",
            dir.display()
        ));
    }
    // A workspace session is always local (embedded disk); only its shada identity and
    // session capture differ from the default embedded session.
    spawn_server(false, None, Some(abs), config_source, || async {
        Ok((None, None, no_guard()))
    })
}

/// Expand a leading `~` / `~/` in a `:workspace` path against `$HOME`; anything else is
/// returned verbatim (a relative path canonicalizes against the GUI process's cwd). Mirrors
/// the server's `:cd` argument expansion so `:workspace ~/code` works as typed.
fn expand_tilde(dir: PathBuf) -> PathBuf {
    let Some(s) = dir.to_str() else { return dir };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if s == "~" {
        if let Some(home) = home {
            return home;
        }
    } else if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = home {
            return home.join(rest);
        }
    }
    dir
}

/// The empty [`SessionGuard`] for a local / daemon / quic session (the daemon child, if
/// any, lives on its own link thread — inside [`connect_daemon_respawning`] — not the
/// server thread; the guard stays for any future server-thread-scoped resource).
fn no_guard() -> SessionGuard {
    Box::new(())
}

/// The shared session-spawn scaffolding, on nxvim-server's [`spawn_session_thread`]:
/// run the editor server on its own thread with host seams from `connect`, joined to the
/// window over an in-process duplex. **Blocking** on the daemon handshake, so a failed
/// connect (or config resolve) is an `Err` here — before any window/swap — rather than a
/// dead session. `connect` yields the optional [`DaemonClient`] (`None` = local disk)
/// plus a [`SessionGuard`] kept alive for the session.
fn spawn_server<F, Fut>(
    remote: bool,
    file: Option<String>,
    workspace: Option<PathBuf>,
    config_source: ConfigSource,
    connect: F,
) -> Result<Session>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<
        Output = Result<(Option<DaemonClient>, Option<ReconnectHandle>, SessionGuard)>,
    >,
{
    let (stream, handle) = spawn_session_thread(move || async move {
        let (client, daemon_link, guard) = connect().await?;
        // Build the server config — for a daemon session this fetches + materializes
        // the remote's config/plugins, so a failure here is a setup failure the caller
        // must see (it is relayed as the handshake outcome).
        let init = server_init(file, client, daemon_link, workspace, config_source).await?;
        Ok((init, guard))
    })?;
    Ok(Session {
        stream,
        remote,
        handle,
    })
}

/// Build the stdio daemon command for a `--connect-daemon` startup: `$NXVIM_DAEMON_CMD`
/// through `sh -c`, else the sibling `nxvim --daemon`. (A `nx.session.reconnect` spawn
/// transport uses [`SpawnCommand::to_command`] instead.) The daemon's stderr goes to a
/// private log the user can `tail` to diagnose the daemon side; it can't share the
/// GUI's terminal (see [`daemon_log_stderr`] for why the path is per-pid and
/// symlink-safe rather than a fixed `/tmp` name).
fn stdio_daemon_command() -> Result<tokio::process::Command> {
    let mut cmd = match env_daemon_command() {
        Some(c) => c,
        None => {
            let nxvim = std::env::current_exe()?.with_file_name(nxvim_bin_name());
            if !nxvim.exists() {
                return Err(anyhow!(
                    "no sibling `nxvim` daemon binary at {} — install nxvim alongside \
                     nxvim-gui, or set {DAEMON_CMD_ENV} (e.g. \"ssh host nxvim --daemon\")",
                    nxvim.display()
                ));
            }
            let mut c = tokio::process::Command::new(nxvim);
            c.arg("--daemon");
            c
        }
    };
    cmd.stderr(daemon_log_stderr());
    Ok(cmd)
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
///
/// `workspace` is `Some(dir)` only for a local `:workspace <dir>` session (always with
/// `client = None`): it derives a per-directory shada namespace, forces session
/// capture/restore, and seeds the root surfaced to `nx.workspace` — the GUI counterpart of
/// the TUI's `--workspace` wiring. `None` for every other session (plain local or daemon).
async fn server_init(
    file: Option<String>,
    client: Option<DaemonClient>,
    daemon_link: Option<ReconnectHandle>,
    workspace: Option<PathBuf>,
    config_source: ConfigSource,
) -> Result<ServerInit> {
    // A `:workspace` session (local only) derives its shada namespace from the directory
    // and exposes the root to `nx.workspace`; an explicit namespace isn't offered from the
    // GUI, so the directory-derived one is the only source. `None`/`false` for every other
    // session (a daemon session never carries a workspace here).
    let (shada_namespace, workspace_dir) = match &workspace {
        Some(dir) => (
            Some(nxvim_server::workspace_namespace(dir)),
            Some(dir.to_string_lossy().into_owned()),
        ),
        None => (None, None),
    };
    let mut ts_autoinstall = Vec::new();
    // The daemon's cwd seeds `DirState` so `:pwd`/`:cd`/`getcwd` operate on the remote
    // dir in a daemon session (`docs/plans/2026-06-23-remote-cwd.md`); `None` for a local
    // session, which seeds from the local process cwd.
    let mut remote_cwd = None;
    // The daemon's home, the base a leading `~` in a file argument expands against in a
    // daemon session; `None` for a local session (expands against the local `$HOME`).
    let mut remote_home = None;
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
        host_term,
        fs_jobs,
        http_jobs,
        git_jobs,
    ) = match client {
        None => {
            let (config_dir, runtimepath) = nxvim_server::default_runtime();
            // A `:workspace` session scopes shada to a per-directory `ns/<NS>` store; a
            // plain local session's primary store IS the global one.
            let store = match shada_namespace.as_deref() {
                Some(ns) => nxvim_server::workspace_shada(Some(ns)),
                None => nxvim_server::default_shada(),
            };
            (
                config_dir,
                runtimepath,
                store,
                None,
                None,
                None,
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
            remote_home = resolved.remote_home;
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
                // `:terminal` opens its PTY on the daemon (where the files are), not locally.
                Some(c.host_term),
                Some(c.fs_jobs),
                Some(c.http),
                Some(c.git_jobs),
            )
        }
    };
    Ok(ServerInit {
        file,
        config_dir,
        // Editor + Lua run locally; shada is local for a `Local`-config session and on the
        // daemon for a `Remote`-config one (Approach A — synced over the fs seam).
        shada: Some(shada_store),
        // A `:workspace` session routes history to the global store too (parity with the
        // TUI's `--workspace`); a plain/daemon session's primary store is already global.
        global_shada: shada_namespace
            .as_ref()
            .map(|_| nxvim_server::default_shada()),
        remote_shada,
        // A `:workspace` session captures + restores the editor session (open files +
        // exact layout) and auto-opts into layout capture, exactly like `--workspace`; a
        // plain local or daemon session does neither.
        workspace_session: workspace.is_some(),
        restore_session: workspace.is_some(),
        session_save_layout: workspace.is_some(),
        // Seed the namespace + root that `nx.shada.namespace()` / `nx.workspace` report
        // (both `None` unless this is a `:workspace` session).
        shada_namespace,
        workspace_dir,
        // A GUI `:workspace` always cds into the workspace root at boot (no `--workspace-no-cwd`
        // equivalent), exactly like the TUI's default `--workspace DIR`.
        workspace_cwd: workspace.is_some(),
        runtimepath,
        clipboard: ClipboardProvider::System,
        mouse_clock: None,
        host_fs,
        host_proc,
        host_fs_async,
        lsp_transport,
        host_term,
        fs_jobs,
        http_jobs,
        git_jobs,
        // The GUI is the interactive editor — offer the built-in default recommended
        // set on a fresh setup (a config's own recommend{} still overrides it), and
        // enable command-line completion by default (a config's setup{} still wins).
        offer_default_recommended: true,
        // Seed the client-owned system-plugin tier into every session (local and daemon
        // alike) — loaded before init.lua, so a connector persists across a session swap.
        system_plugins: nxvim_server::discover_system_plugins(),
        cmdline_complete_default: true,
        // Interactive: `print` shows on the message line, not stdout.
        lua_stdio: false,
        ts_autoinstall,
        remote_cwd,
        remote_home,
        // Register the GUI's client-intercepted commands as no-op virtual commands so
        // they get name completion, help, cmdline history, and (for `:workspace`)
        // directory-path completion. The actual session swap is still done client-side
        // (see [`crate::run`]); the server-side body does nothing.
        client_init_lua: Some(CLIENT_INIT_LUA.to_string()),
        // The reconnecting daemon link (ssh/stdio) — its status flows into the editor + Lua
        // (`nx.daemon.status()`) and `:reconnect`/`:disconnect` drive it. `None` for a local
        // session, or a one-shot QUIC connect.
        daemon_link,
    })
}

/// Lua run at session startup (via [`ServerInit::client_init_lua`]) to register the GUI's
/// `:workspace` as a no-op user command — so the command line completes its name, shows its
/// help, saves it to history, and offers directory completion for its argument. The window
/// intercepts it on `<CR>` to perform the real session swap before this no-op body ever runs
/// (and still lets the keystroke through so the command is recorded in history).
///
/// `:connect` is NOT here: it is a real prelude command (`nx.connect`, §C) that routes
/// through the VM so a connector's resolver can claim it. When no provider matches, the
/// server pushes a `nx_connect_fallback` notification and the GUI dials the URL directly
/// (see [`crate::run`]) — the same ssh/quic path, now triggered by the notification instead
/// of a client-side `<CR>` intercept.
const CLIENT_INIT_LUA: &str = r#"
nx.user_command.create("workspace", function() end, {
  desc = "Open a directory as a workspace: :workspace [dir]",
  complete = "dir",
})
"#;

// `parse_connect_uri` is nxvim-server's (shared with the TUI binary); re-exported so
// `nxvim_gui::parse_connect_uri` keeps working for the black-box tests.
pub use nxvim_server::parse_connect_uri;
