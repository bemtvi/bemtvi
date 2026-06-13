//! `nxvim-gui` entry point — the GUI binary is a **client only**, in one of two roles
//! (the local halves of the `nxvim` TUI binary's edit-host split):
//!
//! - **default** (`nxvim-gui [file]`): an *embedded* editor — a headless server on
//!   its own thread, joined to the UI client over an in-process duplex stream (the
//!   same msgpack-RPC the TUI binary uses). The client is a native winit + wgpu
//!   window ([`nxvim_gui::run`]).
//! - **`--connect-daemon`** (`nxvim-gui --connect-daemon [file]`): the *edit-host split*'s
//!   **local** half — the full editor + GUI window (exactly the default role), but with
//!   its fs/process/watch/LSP host seams pointed at a `--daemon` child. The daemon is the
//!   sibling `nxvim` binary by default (the GUI never serves as a daemon itself), or
//!   whatever `NXVIM_DAEMON_CMD` names — e.g. `ssh host nxvim --daemon`. A
//!   `nxvim://HOST:PORT/TOKEN?cert=HASH` argument selects the QUIC transport instead.
//!   Config (init.lua, plugins, runtimepath) and the keystroke path stay local; only
//!   I/O crosses the wire.
//!
//! There is deliberately **no** daemon-serving role in the GUI (run `nxvim --daemon` for
//! that) and **no** "whole editor runs remote, thin client local" role: the edit-host
//! split keeps the editor (core + Lua) local for a zero-round-trip keystroke path, moving
//! only the fs/process/watch/LSP boundary across the wire.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{anyhow, Result};
use nxvim_gui::GuiConfig;
use nxvim_server::{
    connect_daemon, connect_quic, run as run_server, ClipboardProvider, DaemonClient, ServerInit,
};

/// `--daemon` flag passed to the *daemon* binary (the sibling `nxvim`, never this one):
/// runs it as pure I/O — the fs + process + watch + LSP host the edit-host split drives.
const DAEMON_FLAG: &str = "--daemon";

/// URI scheme `--connect-daemon` accepts to reach a QUIC listener:
/// `nxvim://HOST:PORT/TOKEN?cert=HASH`. Presence of such an argument selects the QUIC
/// connect path over the default stdio-child split.
const CONNECT_URI_SCHEME: &str = "nxvim://";

/// Flag that runs the **local** edit-host half of the split: the full editor + GUI
/// window (the default role) wired to a `--daemon` child for fs/process/watch/LSP.
const CONNECT_DAEMON_FLAG: &str = "--connect-daemon";

/// Env var naming the command to spawn as the daemon for `--connect-daemon`. Run
/// through `sh -c`, so a full command line works verbatim — e.g.
/// `NXVIM_DAEMON_CMD="ssh host nxvim --daemon"`. Unset = spawn the sibling `nxvim`
/// binary (`--daemon`) for a local two-process split over stdio pipes.
const DAEMON_CMD_ENV: &str = "NXVIM_DAEMON_CMD";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // The positional arguments (the first is the file to open) plus the font config
    // (`--font` / `--font-size`, overriding the `NXVIM_GUI_FONT*` environment).
    let (positionals, config) = parse_args(&args);

    // A `nxvim://…` connect URI (the QUIC daemon target) is not a file; pick the file
    // from the remaining non-URI positionals.
    let connect_uri = args
        .iter()
        .find(|a| a.starts_with(CONNECT_URI_SCHEME))
        .cloned();
    let file = positionals
        .into_iter()
        .find(|a| !a.starts_with(CONNECT_URI_SCHEME));

    // Edit-host split, local half, over QUIC: a `nxvim://…` target connects to a
    // `--daemon --listen` listener and routes fs/process/watch/LSP to it.
    if let Some(uri) = connect_uri {
        return run_with_daemon_quic(file, config, &uri);
    }

    // Edit-host split, local half, over stdio: the default editor + window, but spawning
    // a `--daemon` child and routing fs/process/watch/LSP to it over stdio pipes.
    if args.iter().any(|a| a == CONNECT_DAEMON_FLAG) {
        return run_with_daemon(file, config);
    }

    // A *directory* argument (`nxvim-gui somedir`) opens the system file picker at that
    // directory rather than the server's in-window netrw listing. So it is the GUI
    // client's job, not the server's: divert it from `ServerInit.file` (which would
    // open the explorer) into `open_dir`, leaving the server on `[No Name]`. (Only the
    // local embedded role — a daemon session's files live on the daemon's fs, so a
    // local native picker would be meaningless there.)
    let (file, open_dir) = match file {
        Some(f) if std::path::Path::new(&f).is_dir() => (None, Some(PathBuf::from(f))),
        other => (other, None),
    };

    run_embedded(file, config, open_dir)
}

/// The default role: an embedded server on its own thread joined to the GUI client on
/// the main thread over an in-process duplex — every host seam served against the local
/// disk and processes.
fn run_embedded(file: Option<String>, config: GuiConfig, open_dir: Option<PathBuf>) -> Result<()> {
    // In-process, bidirectional transport between client and server.
    let (server_end, client_end) = tokio::io::duplex(1 << 16);

    // The server runs on its own thread with its own single-threaded runtime, so
    // the (non-Send) editor + Lua state live entirely on that thread.
    let (config_dir, runtimepath) = nxvim_server::default_runtime();
    let init = ServerInit {
        file,
        config_dir,
        // Persist cross-session state under `stdpath("state")/shada` (local; the
        // edit-host split persists locally — only fs/proc cross to the daemon).
        shada: Some(nxvim_server::default_shada()),
        runtimepath,
        clipboard: ClipboardProvider::System,
        // The real binary uses the monotonic wall clock for mouse multi-click
        // timing; only tests inject a fake clock here.
        mouse_clock: None,
        // The local GUI reads/writes through the real disk (the default); a
        // daemon-backed fs is injected here by the edit-host split.
        host_fs: None,
        // The local GUI spawns real local processes (the default); a daemon-backed
        // process host is injected here by the edit-host split.
        host_proc: None,
        // The local GUI opens the startup file synchronously through the disk; the
        // async (daemon) fs is injected here by the edit-host split.
        host_fs_async: None,
        // The local GUI runs blocking `vim.system` shell-outs locally; a daemon
        // blocking bridge is injected here by the edit-host split.
        blocking_system: None,
        // The local GUI spawns language servers as real local children; a daemon-backed
        // LSP transport is injected here by the edit-host split.
        lsp_transport: None,
        // The local GUI reads the project fs directly; a daemon-backed Lua fs bridge
        // is injected here by the edit-host split.
        lua_fs: None,
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

/// Build the [`tokio::process::Command`] for the daemon child. With [`DAEMON_CMD_ENV`]
/// set, run that command line through `sh -c` (so `ssh host nxvim --daemon` works
/// verbatim). Unset, spawn the sibling `nxvim` binary (co-located in the same bin dir)
/// in `--daemon` mode — the GUI is a client only and never serves as a daemon itself, so
/// it fails loud if no sibling `nxvim` is found rather than spawning a useless window.
fn daemon_command() -> Result<tokio::process::Command> {
    use tokio::process::Command;
    if let Some(cmd) = std::env::var_os(DAEMON_CMD_ENV) {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        return Ok(c);
    }
    let nxvim = std::env::current_exe()?.with_file_name(nxvim_bin_name());
    if !nxvim.exists() {
        return Err(anyhow!(
            "no sibling `nxvim` daemon binary at {} — install nxvim alongside nxvim-gui, \
             or set {DAEMON_CMD_ENV} (e.g. \"ssh host nxvim --daemon\")",
            nxvim.display()
        ));
    }
    let mut c = Command::new(nxvim);
    c.arg(DAEMON_FLAG);
    Ok(c)
}

/// The sibling daemon binary's file name (`nxvim.exe` on Windows, `nxvim` elsewhere).
fn nxvim_bin_name() -> &'static str {
    if cfg!(windows) {
        "nxvim.exe"
    } else {
        "nxvim"
    }
}

/// Run the **local** edit-host over a stdio-piped `--daemon` child (`--connect-daemon`,
/// no `nxvim://` target): the local two-process split. The server thread spawns the
/// daemon (its stdio *is* the wire), wraps it in [`connect_daemon`], and runs the editor
/// against those seams. The daemon's stderr is redirected to a temp log (so it can't
/// interleave with this binary's own stderr); `tail` it to debug the daemon. `kill_on_drop`
/// reaps the child on quit.
fn run_with_daemon(file: Option<String>, config: GuiConfig) -> Result<()> {
    run_edit_host_session(file, config, || {
        // The daemon's stderr is sent to a log the user can tail to diagnose the
        // daemon side, rather than interleaving with this process's stderr.
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
/// (a `nxvim://HOST:PORT/TOKEN?cert=HASH` target). Same editor + window as the stdio
/// split; only the transport differs — [`connect_quic`] pins the daemon's cert TOFU and
/// presents the bearer token, then returns the same five seams. The QUIC endpoint +
/// connection are owned by `connect_quic`'s link thread, so there is no child to hold.
fn run_with_daemon_quic(file: Option<String>, config: GuiConfig, uri: &str) -> Result<()> {
    let (url, cert_hash, token) = parse_connect_uri(uri)?;
    run_edit_host_session(file, config, move || {
        let client = connect_quic(&url, &cert_hash, &token)?;
        Ok((client, Box::new(())))
    })
}

/// The shared edit-host runtime: build the in-process editor↔GUI duplex, run the embedded
/// server (with its host seams pointed at whatever `connect` returns) on its own thread,
/// and drive the GUI window on the main thread. `connect` runs inside the server runtime
/// and yields the [`DaemonClient`] plus a guard kept alive for the whole session (the
/// stdio child, so `kill_on_drop` reaps it on quit; `()` for QUIC). Config and the
/// keystroke path stay local; only fs/process/watch/LSP cross to the daemon.
fn run_edit_host_session<F>(file: Option<String>, config: GuiConfig, connect: F) -> Result<()>
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
                clipboard: ClipboardProvider::System,
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
            // `_guard` (the stdio child, or `()` for QUIC) lives until the editor quits.
            run_server(server_end, init).await
        });
        if let Err(err) = result {
            eprintln!("nxvim-gui: edit-host error: {err}");
        }
    });

    // The GUI client owns the main thread, exactly as the default role. The transport
    // is already built; the connector hands it over. A daemon session opens the startup
    // file through the daemon's fs, so there is no local file picker (`open_dir` is None).
    let result = nxvim_gui::run(move || async move { anyhow::Ok(client_end) }, config, None);

    if let Err(payload) = server_thread.join() {
        eprintln!(
            "nxvim-gui: server thread panicked: {}",
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

/// Parse the command line into `(positionals, config)`: non-flag arguments (the
/// first is the file to open) in order, plus the font config — `--font <name>` /
/// `--font-size <pt>` (or the `=` form) set the font, taking precedence over the
/// `NXVIM_GUI_FONT` / `NXVIM_GUI_FONT_SIZE` environment the config starts from.
/// Unknown flags (e.g. `--connect-daemon`) are ignored here — they are handled by `main`.
fn parse_args(args: &[String]) -> (Vec<String>, GuiConfig) {
    let mut positionals = Vec::new();
    let mut config = GuiConfig::from_env();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(name) = arg.strip_prefix("--font=") {
            config.set_font(name);
        } else if let Some(size) = arg.strip_prefix("--font-size=") {
            apply_font_size(&mut config, size);
        } else if arg == "--font" {
            if let Some(name) = iter.next() {
                config.set_font(name);
            }
        } else if arg == "--font-size" {
            if let Some(size) = iter.next() {
                apply_font_size(&mut config, size);
            }
        } else if !arg.starts_with('-') {
            positionals.push(arg.clone());
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
