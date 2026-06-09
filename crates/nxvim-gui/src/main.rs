//! `nxvim-gui` entry point.
//!
//! Like the `nxvim` (TUI) binary, this runs an *embedded* server — a headless
//! editor on its own thread — and a UI client on the main thread, joined by an
//! in-process duplex stream over the exact same msgpack-RPC a remote client uses.
//! The only difference from `nxvim` is the client: a native winit + wgpu window
//! ([`nxvim_gui::run`]) instead of the terminal UI.

use anyhow::Result;
use nxvim_gui::GuiConfig;
use nxvim_server::{run as run_server, ServerInit};

fn main() -> Result<()> {
    // The positional file argument (like `nvim file.txt`) plus the font config
    // (`--font`/`--font-size`, overriding the `NXVIM_GUI_FONT*` environment).
    let (file, config) = parse_args();

    // A *directory* argument (`nxvim-gui somedir`) opens the system file picker at
    // that directory rather than the server's in-window netrw listing. So it is the
    // GUI client's job, not the server's: divert it from `ServerInit.file` (which
    // would open the explorer) into `open_dir`, leaving the server on `[No Name]`.
    let (file, open_dir) = match file {
        Some(f) if std::path::Path::new(&f).is_dir() => (None, Some(std::path::PathBuf::from(f))),
        other => (other, None),
    };

    // In-process, bidirectional transport between client and server.
    let (server_end, client_end) = tokio::io::duplex(1 << 16);

    // The server runs on its own thread with its own single-threaded runtime, so
    // the (non-Send) editor + Lua state live entirely on that thread.
    let (config_dir, runtimepath) = nxvim_server::default_runtime();
    let init = ServerInit {
        file,
        config_dir,
        runtimepath,
        clipboard: nxvim_server::ClipboardProvider::System,
        // The real binary uses the monotonic wall clock for mouse multi-click
        // timing; only tests inject a fake clock here.
        mouse_clock: None,
    };
    let server_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("failed to build server runtime");
        if let Err(err) = runtime.block_on(run_server(server_end, init)) {
            eprintln!("nxvim-gui: server error: {err}");
        }
    });

    // The GUI client owns the main thread (winit's event loop requirement).
    let result = nxvim_gui::run(client_end, config, open_dir);

    // When the client exits, the dropped stream signals the server to wind down.
    // Surface a server-thread panic as a non-zero exit, mirroring the TUI binary.
    if let Err(payload) = server_thread.join() {
        eprintln!(
            "nxvim-gui: server thread panicked: {}",
            panic_message(payload.as_ref())
        );
        std::process::exit(101);
    }
    result
}

/// Parse the command line into `(file, config)`: the first non-flag argument is
/// the file to open; `--font <name>` / `--font-size <pt>` (or the `=` form) set the
/// font, taking precedence over the `NXVIM_GUI_FONT` / `NXVIM_GUI_FONT_SIZE`
/// environment the config starts from. Unknown flags are ignored.
fn parse_args() -> (Option<String>, GuiConfig) {
    let mut file = None;
    let mut config = GuiConfig::from_env();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(name) = arg.strip_prefix("--font=") {
            config.set_font(name);
        } else if let Some(size) = arg.strip_prefix("--font-size=") {
            apply_font_size(&mut config, size);
        } else if arg == "--font" {
            if let Some(name) = args.next() {
                config.set_font(&name);
            }
        } else if arg == "--font-size" {
            if let Some(size) = args.next() {
                apply_font_size(&mut config, &size);
            }
        } else if !arg.starts_with('-') && file.is_none() {
            file = Some(arg);
        }
    }
    (file, config)
}

/// Apply a `--font-size` value, warning (but not failing) on a non-numeric one.
fn apply_font_size(config: &mut GuiConfig, value: &str) {
    match value.trim().parse::<f32>() {
        Ok(pt) => config.set_font_size(pt),
        Err(_) => eprintln!("nxvim-gui: ignoring non-numeric --font-size {value:?}"),
    }
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
