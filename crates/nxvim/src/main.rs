//! `nxvim` entry point — the single binary, in either of two roles:
//!
//! - **default** (`nxvim [file]`): an *embedded* server (a headless editor on its
//!   own thread) plus the terminal UI client on the main thread, over an
//!   in-process duplex stream.
//! - **`--server`** (`nxvim --server [file]`): just the headless server, speaking
//!   msgpack-RPC over **stdin/stdout** and no UI. This is what a remote client
//!   spawns over SSH (`ssh host nxvim --server`); the local `nxvim-gui` drives it
//!   through the ssh pipe.
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
//! The first three roles run the *same* [`nxvim_server::run`]/[`run_io`] over the
//! same RPC — the only difference is the transport (a duplex vs. this process's
//! stdio), so the editor behaves identically whether embedded, headless, or remote.
//! `--daemon` is the inverse: it runs [`nxvim_server::run_daemon_io`] (no editor),
//! the remote half of the boundary moved *below* the editor. `--connect-daemon` is the
//! matching local half — the default role plus the daemon seams.

use std::process::Stdio;

use anyhow::Result;
use nxvim_server::{
    connect_daemon, run as run_server, run_daemon_io as run_server_daemon_io,
    run_io as run_server_io, ServerInit,
};

/// Flag that runs this binary as the headless server over stdin/stdout (no UI) —
/// the remote end of an SSH connection (`ssh host nxvim --server`).
const SERVER_FLAG: &str = "--server";

/// Flag that runs this binary as the **daemon** over stdin/stdout (no UI, no
/// editor): just the fs + process + watch + LSP host the *edit-host split* drives
/// remotely (`ssh host nxvim --daemon`). Contrast `--server`, which runs the
/// *whole* editor remotely.
const DAEMON_FLAG: &str = "--daemon";

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
    // The file to open is the first non-flag argument (the same in either role).
    let file = args.iter().find(|a| !a.starts_with('-')).cloned();

    // Daemon role: the edit-host split's remote half — fs/process/watch/LSP over
    // this process's stdin/stdout, no editor and no UI. The edit-host closing the
    // pipe (EOF on stdin) winds it down. Checked before `--server` so the two roles
    // never collide on a malformed argv.
    if args.iter().any(|a| a == DAEMON_FLAG) {
        return run_daemon();
    }

    // Headless server role: RPC over this process's stdin/stdout, no UI. The
    // client closing the pipe (EOF on stdin) winds the server down.
    if args.iter().any(|a| a == SERVER_FLAG) {
        return run_headless(file);
    }

    // Edit-host split, local half: the default editor + UI, but spawning a
    // `--daemon` child and routing fs/process/watch/LSP to it over stdio. Checked
    // before the plain embedded path so the flag selects the daemon-backed wiring.
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
        // the local binary spawns blocking `vim.system` shell-outs locally; a daemon
        // blocking bridge is injected here by the edit-host split.
        blocking_system: None,
        // The local binary spawns language servers as real local children (the
        // default); a daemon-backed LSP transport is injected here by the edit-host
        // split.
        lsp_transport: None,
        // The local binary reads the project fs directly; a daemon-backed Lua fs
        // bridge is injected here by the edit-host split.
        lua_fs: None,
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

/// Run the headless server (`--server`) over this process's stdin/stdout until
/// the client closes the pipe. No UI thread — the server owns the main thread's
/// runtime directly. `default_runtime` reads *this* host's config/runtimepath, so
/// over SSH it sources the remote machine's `init.lua`, plugins, and grammars.
fn run_headless(file: Option<String>) -> Result<()> {
    let (config_dir, runtimepath) = nxvim_server::default_runtime();
    let init = ServerInit {
        file,
        config_dir,
        runtimepath,
        clipboard: nxvim_server::ClipboardProvider::System,
        mouse_clock: None,
        host_fs: None,
        host_proc: None,
        host_fs_async: None,
        blocking_system: None,
        // The local binary spawns language servers as real local children (the
        // default); a daemon-backed LSP transport is injected here by the edit-host
        // split.
        lsp_transport: None,
        // The local binary reads the project fs directly; a daemon-backed Lua fs
        // bridge is injected here by the edit-host split.
        lua_fs: None,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(run_server_io(tokio::io::stdin(), tokio::io::stdout(), init))
}

/// Run the daemon (`--daemon`) over this process's stdin/stdout until the edit-host
/// closes the pipe. No editor, no Lua, no UI, and — unlike `run_headless` — **no
/// config sourcing**: the daemon is pure I/O. `run_daemon_io` connects once and
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

/// Run the **local** edit-host (`--connect-daemon`): the same embedded server + TUI
/// client as the default role, but with its host seams pointed at a `--daemon` child.
///
/// The server thread spawns the daemon (its stdio *is* the wire), wraps it in
/// [`connect_daemon`] — the edit-host multiplexer, one connection for all five seams —
/// and injects those seams into [`ServerInit`]. Config and the keystroke path stay
/// local (`default_runtime`); only fs/process/watch/LSP cross to the daemon. The
/// daemon's stderr is redirected to a temp log (it can't corrupt the TUI); `tail` it to
/// debug the daemon. `kill_on_drop` reaps the child when the editor quits.
fn run_with_daemon(file: Option<String>) -> Result<()> {
    let (server_end, client_end) = tokio::io::duplex(1 << 16);

    let server_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("failed to build server runtime");
        let result: Result<()> = runtime.block_on(async move {
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

            // One connection → all five host seams (the edit-host multiplexer).
            let client = connect_daemon(stdout, stdin);
            let (config_dir, runtimepath) = nxvim_server::default_runtime();
            let init = ServerInit {
                file,
                config_dir,
                runtimepath,
                clipboard: nxvim_server::ClipboardProvider::System,
                mouse_clock: None,
                // The local disk is unused for buffers in a daemon session — every
                // fs/process/LSP/Lua-fs path is routed to the daemon below.
                host_fs: None,
                host_proc: Some(Box::new(client.host_proc)),
                host_fs_async: Some(Box::new(client.host_fs)),
                blocking_system: Some(Box::new(client.blocking_system)),
                lsp_transport: Some(Box::new(client.lsp_transport)),
                lua_fs: Some(Box::new(client.lua_fs)),
            };
            // Hold the child for the whole session; dropping it (on quit) reaps the
            // daemon via `kill_on_drop`.
            let _child = child;
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
