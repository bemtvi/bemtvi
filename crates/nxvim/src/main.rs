//! `nxvim` entry point.
//!
//! Like `nvim`, the default invocation runs an *embedded* server: a headless
//! editor on its own thread, and the terminal UI client on the main thread.
//! They communicate over an in-process duplex stream using the exact same
//! msgpack-RPC the editor would use for an external client — so the embedded
//! and remote cases share one code path, and the server can never be blocked by
//! UI rendering (it runs on a separate OS thread).

use anyhow::Result;
use nxvim_server::{run as run_server, ServerInit};

/// Internal flag that re-invokes this binary as the treesitter syntax worker
/// (see `nxvim-ts`). Hidden from users; spawned only by the server.
const TS_WORKER_FLAG: &str = "--__ts-worker";

fn main() -> Result<()> {
    // Worker mode: a separate, crash-isolated process that does all tree-sitter
    // parsing and streams highlight spans back over stdio. It never starts an
    // editor; if it dies, the server respawns it. Match the flag only as the
    // *first* argument — exactly how the server spawns the worker — so a file
    // literally named `--__ts-worker` (or the flag anywhere past argv[1]) opens
    // the editor instead of silently turning it into a worker.
    if std::env::args().nth(1).as_deref() == Some(TS_WORKER_FLAG) {
        return run_ts_worker();
    }

    // Positional file argument, like `nvim file.txt`.
    let file = std::env::args().nth(1);

    // In-process, bidirectional transport between client and server.
    let (server_end, client_end) = tokio::io::duplex(1 << 16);

    // The server runs on its own thread with its own single-threaded runtime,
    // so the (non-Send) editor + Lua state live entirely on that thread.
    let (config_dir, runtimepath) = nxvim_server::default_runtime();
    let init = ServerInit {
        file,
        config_dir,
        runtimepath,
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

/// Run the treesitter worker over this process's stdio until the parent closes
/// the pipe. Its own single-threaded runtime, with I/O enabled for the stdio
/// pipes.
fn run_ts_worker() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(nxvim_ts::run_worker(
        tokio::io::stdin(),
        tokio::io::stdout(),
    ));
    Ok(())
}
