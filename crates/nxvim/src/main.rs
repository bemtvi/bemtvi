//! `nxvim` entry point — the single binary, in one of three roles:
//!
//! - **default** (`nxvim [file]`): an *embedded* server (a headless editor on its
//!   own thread) plus the terminal UI client on the main thread, over an
//!   in-process duplex stream.
//! - **`--daemon`** (`nxvim --daemon`): the *edit-host split*'s remote half — just
//!   the fs + process + watch + LSP host, no editor and no UI, multiplexing every
//!   leg of the daemon wire over **stdin/stdout**. The editor (core + Lua) stays
//!   *local* for a zero-round-trip keystroke path; only I/O runs here. This is what
//!   the local edit-host spawns over SSH (`ssh host nxvim --daemon`).
//! - **`--connect-daemon`** (`nxvim --connect-daemon [file]`): the *edit-host split*'s
//!   **local** half — the full editor + terminal UI (exactly the default role), but
//!   with its fs/process/watch/LSP host seams pointed at a `--daemon` child instead of
//!   the local disk. It spawns that daemon (this same binary in `--daemon` mode by
//!   default, or whatever `NXVIM_DAEMON_CMD` names — e.g. `ssh host nxvim --daemon`),
//!   wraps the child's stdio in [`nxvim_server::connect_daemon`], and injects the five
//!   resulting seams into [`ServerInit`]. Config (init.lua, plugins, runtimepath) and
//!   the keystroke path stay local; only I/O crosses the wire.
//!
//! The default and `--connect-daemon` roles both run [`nxvim_server::run`] — a full
//! local editor over an in-process duplex; `--connect-daemon` only differs by pointing
//! the host seams at a daemon. `--daemon` is the inverse: it runs
//! [`nxvim_server::run_daemon_io`] (no editor), the remote half of the boundary moved
//! *below* the editor.
//!
//! There is deliberately **no** "whole editor runs remote, thin client local" role:
//! to edit on another machine, open an SSH session and run `nxvim` there, or use the
//! edit-host split (`--connect-daemon`) to keep the keystroke path local.

use std::net::SocketAddr;
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use nxvim_server::{
    bind_quic_listener, connect_daemon, connect_quic, mint_token, run as run_server,
    run_daemon_io as run_server_daemon_io, serve_quic, DaemonClient, ServerInit,
};

/// Flag that runs this binary as the **daemon** over stdin/stdout (no UI, no
/// editor): just the fs + process + watch + LSP host the *edit-host split* drives.
/// With [`LISTEN_FLAG`] it instead binds a WebTransport/QUIC listener (the native
/// transport, Open Decision #2). The daemon runs only I/O — the editor stays local,
/// the inverse of a "whole editor runs remote" topology (which nxvim does not have).
const DAEMON_FLAG: &str = "--daemon";

/// With [`DAEMON_FLAG`], bind a QUIC listener instead of serving over stdin/stdout.
/// An optional bind address follows (`--listen 0.0.0.0:8765`); absent, it binds
/// [`DEFAULT_LISTEN_ADDR`]. This is the real native daemon transport — the stdio mode
/// is the local two-process stand-in.
const LISTEN_FLAG: &str = "--listen";

/// Default daemon bind address when `--listen` is given no explicit address: loopback
/// on a fixed port. Loopback-only is defense-in-depth (the bearer token is the actual
/// auth gate); pass an explicit `0.0.0.0:PORT` to accept off-host connections.
const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:8765";

/// URI scheme the daemon prints and `--connect-daemon` accepts to reach a QUIC
/// listener: `nxvim://HOST:PORT/TOKEN?cert=HASH` — host/port to dial, the bearer
/// TOKEN on the path, the TOFU cert HASH in the query. Presence of such an argument
/// selects the QUIC connect path over the default stdio-child split.
const CONNECT_URI_SCHEME: &str = "nxvim://";

/// Flag that runs the **local** edit-host half of the split: the full editor + UI
/// (the default role) wired to a `--daemon` child for fs/process/watch/LSP. The
/// daemon command defaults to this binary in `--daemon` mode; override it with
/// [`DAEMON_CMD_ENV`].
const CONNECT_DAEMON_FLAG: &str = "--connect-daemon";

/// Env var naming the command to spawn as the daemon for `--connect-daemon`. Run
/// through `sh -c`, so a full command line works verbatim — e.g.
/// `NXVIM_DAEMON_CMD="ssh host nxvim --daemon"`. Unset = spawn this same binary
/// (`current_exe --daemon`) for a local two-process split over stdio pipes.
const DAEMON_CMD_ENV: &str = "NXVIM_DAEMON_CMD";

/// Internal, debug-only flag that runs this binary as a scripted mock language
/// server (see `nxvim_lsp::mock`), used by the LSP test suite as a hermetic
/// stand-in for a real server. Never present in release builds.
#[cfg(debug_assertions)]
const LSP_MOCK_FLAG: &str = "--__lsp-mock";

fn main() -> Result<()> {
    // Mock language server mode (debug builds only): a hermetic, scripted LSP
    // server the test suite spawns instead of a real one. It never starts an
    // editor; the script path follows the flag.
    #[cfg(debug_assertions)]
    if std::env::args().nth(1).as_deref() == Some(LSP_MOCK_FLAG) {
        let script = std::env::args().nth(2).unwrap_or_default();
        nxvim_lsp::mock::run(&script);
        return Ok(());
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    // A `nxvim://…` connect URI (the QUIC daemon target) is not a file; pick the file
    // from the remaining non-flag, non-URI arguments.
    let connect_uri = args
        .iter()
        .find(|a| a.starts_with(CONNECT_URI_SCHEME))
        .cloned();
    let file = args
        .iter()
        .find(|a| !a.starts_with('-') && !a.starts_with(CONNECT_URI_SCHEME))
        .cloned();

    // Daemon role: the edit-host split's remote half — fs/process/watch/LSP, no editor
    // and no UI. With `--listen` it binds a QUIC listener (the real native transport);
    // otherwise it serves over this process's stdin/stdout (the local stand-in), wound
    // down by EOF on stdin.
    if args.iter().any(|a| a == DAEMON_FLAG) {
        if args.iter().any(|a| a == LISTEN_FLAG) {
            return run_daemon_listen(listen_addr(&args)?);
        }
        return run_daemon();
    }

    // Edit-host split, local half, over QUIC: a `nxvim://…` target (with or without the
    // `--connect-daemon` flag) connects to a `--daemon --listen` listener and routes
    // fs/process/watch/LSP to it. Checked before the stdio-child split below.
    if let Some(uri) = connect_uri {
        return run_with_daemon_quic(file, &uri);
    }

    // Edit-host split, local half, over stdio: the default editor + UI, but spawning a
    // `--daemon` child and routing fs/process/watch/LSP to it over stdio pipes.
    if args.iter().any(|a| a == CONNECT_DAEMON_FLAG) {
        return run_with_daemon(file);
    }

    // In-process, bidirectional transport between client and server.
    let (server_end, client_end) = tokio::io::duplex(1 << 16);

    // The server runs on its own thread with its own single-threaded runtime,
    // so the (non-Send) editor + Lua state live entirely on that thread.
    let (config_dir, runtimepath) = nxvim_server::default_runtime();
    let init = ServerInit {
        file,
        config_dir,
        // Persist cross-session state (registers, marks, history, …) under
        // `stdpath("state")/shada`. Editor state lives where the editor runs, so the
        // edit-host split persists locally (only fs/proc cross to the daemon).
        shada: Some(nxvim_server::default_shada()),
        runtimepath,
        // The real editor wires the host clipboard for the `"+` / `"*` registers.
        clipboard: nxvim_server::ClipboardProvider::System,
        // The real binary uses the monotonic wall clock for mouse multi-click
        // timing; only tests inject a fake clock here.
        mouse_clock: None,
        // The local binary reads/writes through the real disk (the default); a
        // daemon-backed fs is injected here by the edit-host split.
        host_fs: None,
        // The local binary spawns real local processes (the default); a
        // daemon-backed process host is injected here by the edit-host split.
        host_proc: None,
        // The local binary opens the startup file synchronously through the disk;
        // the async (daemon) fs is injected here by the edit-host split.
        host_fs_async: None,
        // The local binary spawns language servers as real local children (the
        // default); a daemon-backed LSP transport is injected here by the edit-host
        // split.
        lsp_transport: None,
        // The local binary runs `nx.fs` against the local disk (the actor's `StdLuaFs`);
        // a daemon-backed `luafs_op` seam is injected here by the edit-host split.
        fs_jobs: None,
    };
    let server_thread = std::thread::spawn(move || {
        // Test-only fault injection (debug builds only): force a server-thread
        // panic so the parent's crash handling below can be exercised end to end.
        // Compiled out of release builds entirely.
        #[cfg(debug_assertions)]
        if std::env::var_os("NXVIM_PANIC_TEST").is_some() {
            panic!("NXVIM_PANIC_TEST: injected server-thread panic");
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("failed to build server runtime");
        if let Err(err) = runtime.block_on(run_server(server_end, init)) {
            eprintln!("nxvim: server error: {err}");
        }
    });

    // The client (terminal UI) runs on the main thread.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    let result = runtime.block_on(nxvim_tui::run(client_end));

    // When the client exits, the dropped stream signals the server to wind down.
    // Report a server-thread *panic* as a non-zero exit with a diagnostic: the
    // old `let _ = join()` discarded the payload, so a server crash exited 0 and
    // looked exactly like a clean `:q`. This takes precedence over `result`,
    // since a crashed server is the more important failure to surface.
    if let Err(payload) = server_thread.join() {
        eprintln!(
            "nxvim: server thread panicked: {}",
            panic_message(payload.as_ref())
        );
        std::process::exit(101); // Rust's conventional panic exit code
    }
    result
}

/// Run the daemon (`--daemon`) over this process's stdin/stdout until the edit-host
/// closes the pipe. No editor, no Lua, no UI, and **no config sourcing**: the daemon
/// is pure I/O. `run_daemon_io` connects once and
/// multiplexes every leg of the wire (fs/process/watch/`sys_run`/LSP/`luafs`) onto
/// this one stdio stream, serving each against the real local disk and processes;
/// LSP/process discovery and the project tree arrive on the wire from the remote
/// edit-host. The server owns the main thread's runtime directly.
fn run_daemon() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(run_server_daemon_io(
        tokio::io::stdin(),
        tokio::io::stdout(),
    ))
}

/// Run the daemon as a **QUIC listener** (`--daemon --listen [addr]`): the real native
/// transport (Open Decision #2). Mints a bearer token + self-signed cert, binds `addr`,
/// prints the connect URI the edit-host needs, then accepts connections — each running
/// the full six-leg multiplexer ([`serve_quic`] → `run_daemon_io`) over one QUIC bidi
/// stream. Like `--daemon`, no editor / Lua / config: pure I/O.
fn run_daemon_listen(addr: SocketAddr) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(async move {
        let (endpoint, info) = bind_quic_listener(addr, mint_token())?;
        // The connect credentials (cert hash + token are the auth) go to stdout so a
        // human can copy the command; if the bind address is non-loopback, the user
        // substitutes the reachable host for the connecting machine.
        println!("nxvim daemon listening on {}", info.addr);
        println!(
            "  connect with: nxvim --connect-daemon '{CONNECT_URI_SCHEME}{}/{}?cert={}'",
            info.addr, info.token, info.cert_hash
        );
        serve_quic(endpoint, info.token).await
    })
}

/// The bind address for `--daemon --listen`: the argument right after `--listen`
/// (`--listen 0.0.0.0:8765`) or its `--listen=ADDR` form, else [`DEFAULT_LISTEN_ADDR`].
fn listen_addr(args: &[String]) -> Result<SocketAddr> {
    let raw = args.iter().position(|a| a == LISTEN_FLAG).and_then(|i| {
        // The next argument, unless it's another flag (then `--listen` took no address).
        args.get(i + 1).filter(|a| !a.starts_with('-')).cloned()
    });
    let raw = raw
        .or_else(|| {
            args.iter().find_map(|a| {
                a.strip_prefix(&format!("{LISTEN_FLAG}="))
                    .map(str::to_owned)
            })
        })
        .unwrap_or_else(|| DEFAULT_LISTEN_ADDR.to_owned());
    raw.parse()
        .with_context(|| format!("invalid --listen address {raw:?}"))
}

/// Build the [`tokio::process::Command`] for the daemon child. Defaults to *this*
/// binary in `--daemon` mode (a local two-process split over stdio pipes); if
/// [`DAEMON_CMD_ENV`] is set, run that command line through `sh -c` instead — so a
/// remote daemon (`ssh host nxvim --daemon`) is just an env var, no code change.
fn daemon_command() -> Result<tokio::process::Command> {
    use tokio::process::Command;
    if let Some(cmd) = std::env::var_os(DAEMON_CMD_ENV) {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        Ok(c)
    } else {
        let exe = std::env::current_exe()?;
        let mut c = Command::new(exe);
        c.arg(DAEMON_FLAG);
        Ok(c)
    }
}

/// Run the **local** edit-host over a stdio-piped `--daemon` child (`--connect-daemon`,
/// no `nxvim://` target): the local two-process split. The server thread spawns the
/// daemon (its stdio *is* the wire), wraps it in [`connect_daemon`], and runs the editor
/// against those seams. The daemon's stderr is redirected to a temp log (it can't corrupt
/// the TUI); `tail` it to debug the daemon. `kill_on_drop` reaps the child on quit.
fn run_with_daemon(file: Option<String>) -> Result<()> {
    run_edit_host_session(file, || {
        // The daemon's stderr can't share the terminal with the TUI; send it to a
        // log the user can tail to diagnose the daemon side.
        let log_path = std::env::temp_dir().join("nxvim-daemon.log");
        let stderr = std::fs::File::create(&log_path)
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::null());
        let mut child = daemon_command()?
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child.stdout.take().expect("daemon stdout piped");
        let stdin = child.stdin.take().expect("daemon stdin piped");

        // One connection → all five host seams (the edit-host multiplexer). Hold the
        // child for the whole session; dropping it (on quit) reaps the daemon via
        // `kill_on_drop`.
        let client = connect_daemon(stdout, stdin);
        Ok((client, Box::new(child)))
    })
}

/// Run the **local** edit-host over a QUIC connection to a `--daemon --listen` listener
/// (a `nxvim://HOST:PORT/TOKEN?cert=HASH` target). Same editor + TUI as the stdio split;
/// only the transport differs — [`connect_quic`] pins the daemon's cert TOFU and presents
/// the bearer token, then returns the same five seams. The QUIC endpoint + connection are
/// owned by `connect_quic`'s link thread, so there is no child to hold.
fn run_with_daemon_quic(file: Option<String>, uri: &str) -> Result<()> {
    let (url, cert_hash, token) = parse_connect_uri(uri)?;
    run_edit_host_session(file, move || {
        let client = connect_quic(&url, &cert_hash, &token)?;
        Ok((client, Box::new(())))
    })
}

/// The shared edit-host runtime: build the in-process editor↔TUI duplex, run the embedded
/// server (with its host seams pointed at whatever `connect` returns) on its own thread,
/// and drive the terminal UI on the main thread. `connect` runs inside the server runtime
/// and yields the [`DaemonClient`] plus a guard kept alive for the whole session (the
/// stdio child, so `kill_on_drop` reaps it on quit; `()` for QUIC). Config and the
/// keystroke path stay local; only fs/process/watch/LSP cross to the daemon.
fn run_edit_host_session<F>(file: Option<String>, connect: F) -> Result<()>
where
    F: FnOnce() -> Result<(DaemonClient, Box<dyn std::any::Any + Send>)> + Send + 'static,
{
    let (server_end, client_end) = tokio::io::duplex(1 << 16);

    let server_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("failed to build server runtime");
        let result: Result<()> = runtime.block_on(async move {
            let (client, _guard) = connect()?;
            let (config_dir, runtimepath) = nxvim_server::default_runtime();
            let init = ServerInit {
                file,
                config_dir,
                // Editor + Lua run locally in a daemon session, so shada is local
                // too (only fs/proc/LSP cross to the daemon).
                shada: Some(nxvim_server::default_shada()),
                runtimepath,
                clipboard: nxvim_server::ClipboardProvider::System,
                mouse_clock: None,
                // The local disk is unused for buffers in a daemon session — every
                // fs/process/LSP/Lua-fs path is routed to the daemon below.
                host_fs: None,
                host_proc: Some(Box::new(client.host_proc)),
                host_fs_async: Some(Box::new(client.host_fs)),
                lsp_transport: Some(Box::new(client.lsp_transport)),
                fs_jobs: Some(client.fs_jobs),
            };
            // `_guard` (the stdio child, or `()` for QUIC) lives until the editor quits.
            run_server(server_end, init).await
        });
        if let Err(err) = result {
            eprintln!("nxvim: edit-host error: {err}");
        }
    });

    // The client (terminal UI) runs on the main thread, exactly as the default role.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    let result = runtime.block_on(nxvim_tui::run(client_end));

    if let Err(payload) = server_thread.join() {
        eprintln!(
            "nxvim: server thread panicked: {}",
            panic_message(payload.as_ref())
        );
        std::process::exit(101);
    }
    result
}

/// Parse a `nxvim://HOST:PORT/TOKEN?cert=HASH` connect URI into the pieces
/// [`connect_quic`] needs: the `https://HOST:PORT` dial URL (WebTransport requires the
/// `https` scheme), the bearer `TOKEN` (the path), and the TOFU cert `HASH` (the `cert`
/// query). Fails loud on a malformed URI rather than dialing a half-specified target.
fn parse_connect_uri(uri: &str) -> Result<(String, String, String)> {
    let rest = uri.strip_prefix(CONNECT_URI_SCHEME).ok_or_else(|| {
        anyhow!("daemon connect URI must start with {CONNECT_URI_SCHEME}: {uri:?}")
    })?;
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

/// Best-effort human-readable text from a thread panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "<non-string panic payload>"
    }
}
