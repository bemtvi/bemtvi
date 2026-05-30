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

fn main() -> Result<()> {
    // Positional file argument, like `nvim file.txt`.
    let file = std::env::args().nth(1);

    // In-process, bidirectional transport between client and server.
    let (server_end, client_end) = tokio::io::duplex(1 << 16);

    // The server runs on its own thread with its own single-threaded runtime,
    // so the (non-Send) editor + Lua state live entirely on that thread.
    let init = ServerInit { file };
    let server_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
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
    let _ = server_thread.join();
    result
}
