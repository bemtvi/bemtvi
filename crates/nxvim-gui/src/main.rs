//! `nxvim-gui` entry point.
//!
//! Two ways to run, sharing one client:
//!
//! - **Embedded** (`nxvim-gui [file]`): like the `nxvim` TUI binary, a headless
//!   server runs on its own thread, joined to the UI client over an in-process
//!   duplex stream — the same msgpack-RPC a remote client uses.
//! - **Remote** (`nxvim-gui [user@]host[:port] [file]`): the server runs on a
//!   remote host reached over SSH ([`nxvim_gui::remote`]); only this thin client
//!   is local. The editor state, Lua, LSP, and treesitter all live remote.
//!
//! Either way the client is the same native winit + wgpu window
//! ([`nxvim_gui::run`]); only the transport it's handed differs.

use anyhow::Result;
use nxvim_gui::remote::RemoteSpec;
use nxvim_gui::GuiConfig;
use nxvim_server::{run as run_server, ServerInit};

fn main() -> Result<()> {
    // The positional arguments (first is the file *or* an SSH target; for a remote
    // target the second is the remote file) plus the font config (`--font` /
    // `--font-size`, overriding the `NXVIM_GUI_FONT*` environment).
    let (positionals, config) = parse_args();

    // A first positional shaped like `[user@]host[:port]` runs the editor on a
    // remote host over SSH instead of embedding a local server. The second
    // positional is then the file to open *there*.
    if let Some(spec) = positionals.first().and_then(|a| RemoteSpec::parse(a)) {
        let spec = spec.with_file(positionals.get(1).cloned());
        // The transport is built inside the client's IO runtime (the ssh child's
        // pipes must live on the runtime that polls them), so hand `run` a
        // connector rather than a ready stream. No local server, no `open_dir`.
        return nxvim_gui::run(
            move || async move { nxvim_gui::remote::connect(&spec).await },
            config,
            None,
        );
    }

    let file = positionals.into_iter().next();

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

    // The GUI client owns the main thread (winit's event loop requirement). The
    // embedded transport is already built, so the connector just hands it over.
    let result = nxvim_gui::run(
        move || async move { anyhow::Ok(client_end) },
        config,
        open_dir,
    );

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
///
/// Positionals are returned in order: the first is the file *or* an SSH target,
/// and (for a remote target) the second is the remote file — `main` decides which.
fn parse_args() -> (Vec<String>, GuiConfig) {
    let mut positionals = Vec::new();
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
        } else if !arg.starts_with('-') {
            positionals.push(arg);
        }
    }
    (positionals, config)
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
