//! `nxvim` entry point — the single binary, in either of two roles:
//!
//! - **default** (`nxvim [file]`): an *embedded* server (a headless editor on its
//!   own thread) plus the terminal UI client on the main thread, over an
//!   in-process duplex stream.
//! - **`--server`** (`nxvim --server [file]`): just the headless server, speaking
//!   msgpack-RPC over **stdin/stdout** and no UI. This is what a remote client
//!   spawns over SSH (`ssh host nxvim --server`); the local `nxvim-gui` drives it
//!   through the ssh pipe.
//!
//! Both roles run the *same* [`nxvim_server::run`]/[`run_io`] over the same RPC —
//! the only difference is the transport (a duplex vs. this process's stdio), so
//! the editor behaves identically whether embedded, headless, or remote.

use anyhow::Result;
use nxvim_server::{run as run_server, run_io as run_server_io, ServerInit};

/// Flag that runs this binary as the headless server over stdin/stdout (no UI) —
/// the remote end of an SSH connection (`ssh host nxvim --server`).
const SERVER_FLAG: &str = "--server";

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

    // Headless server role: RPC over this process's stdin/stdout, no UI. The
    // client closing the pipe (EOF on stdin) winds the server down.
    if args.iter().any(|a| a == SERVER_FLAG) {
        return run_headless(file);
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
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(run_server_io(tokio::io::stdin(), tokio::io::stdout(), init))
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
