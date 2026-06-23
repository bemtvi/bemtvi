//! `nxvim-gui` entry point — the GUI binary is a **client only**. It opens a native
//! winit + wgpu window ([`nxvim_gui::run`]) onto a local editor server (a headless
//! server on its own thread, joined over an in-process duplex — the same msgpack-RPC the
//! TUI binary uses). The editor (core + Lua + the keystroke path) always runs local; a
//! session can point its fs/process/watch/LSP host seams at a remote `--daemon` (the
//! *edit-host split*), either at startup or live via `:connect`.
//!
//! Startup roles (all build the *same* local editor — only the host seams differ):
//! - **default** (`nxvim-gui [file]`): embedded, the local disk.
//! - **`--connect-daemon`** (`nxvim-gui --connect-daemon [file]`): an edit-host session
//!   over a `--daemon` child spawned on stdio — the sibling `nxvim` binary by default, or
//!   whatever `NXVIM_DAEMON_CMD` names (e.g. `ssh host nxvim --daemon`).
//! - **`nxvim://HOST:PORT/TOKEN?cert=HASH`**: an edit-host session over a QUIC
//!   (WebTransport) `--daemon --listen` listener.
//!
//! After startup, `:connect [user@]host[:port][/file]` (ssh stdio, with native askpass)
//! and `:connect nxvim://…` (QUIC) switch a running window to a daemon session — see the
//! session loop in [`nxvim_gui::run`].
//!
//! There is deliberately **no** daemon-serving role in the GUI (run `nxvim --daemon` for
//! that) and **no** "whole editor runs remote, thin client local" role: the edit-host
//! split keeps the editor local for a zero-round-trip keystroke path, moving only the
//! fs/process/watch/LSP boundary across the wire.

use std::path::{Path, PathBuf};

use anyhow::Result;
use nxvim_gui::remote::{self, ConnectTarget};
use nxvim_gui::{spawn_session, spawn_stdio_daemon_session, GuiConfig};

/// Flag that runs the **local** edit-host half of the split: the full editor + GUI
/// window, with its fs/process/watch/LSP host seams pointed at a `--daemon` child spawned
/// on stdio (the sibling `nxvim` binary, or `NXVIM_DAEMON_CMD`).
const CONNECT_DAEMON_FLAG: &str = "--connect-daemon";

fn main() -> Result<()> {
    // If ssh re-invoked this binary as its `SSH_ASKPASS` helper (for a `:connect`
    // host-key/password prompt), pop the native dialog and exit — never start the editor.
    if let Some(result) = remote::run_askpass_if_invoked() {
        return result;
    }

    let args: Vec<String> = std::env::args().skip(1).collect();

    // The positional arguments (the first is the file to open) plus the font config
    // (`--font` / `--font-size`, overriding the `NXVIM_GUI_FONT*` environment).
    let (positionals, config) = parse_args(&args);

    // A `nxvim://…` connect URI (the QUIC daemon target) is not a file; pick the file
    // from the remaining non-URI positionals.
    let connect_uri = args.iter().find(|a| remote::is_connect_uri(a)).cloned();
    let file = positionals.into_iter().find(|a| !remote::is_connect_uri(a));

    // Edit-host split, local half, over QUIC: a `nxvim://…` target connects to a
    // `--daemon --listen` listener and routes fs/process/watch/LSP to it.
    if let Some(uri) = connect_uri {
        let session = spawn_session(Some(ConnectTarget::Quic(uri)), file)?;
        return nxvim_gui::run(session, config, None);
    }

    // Edit-host split, local half, over stdio: spawn a `--daemon` child (sibling `nxvim`
    // or `NXVIM_DAEMON_CMD`) and route fs/process/watch/LSP to it over stdio pipes.
    if args.iter().any(|a| a == CONNECT_DAEMON_FLAG) {
        let session = spawn_stdio_daemon_session(file)?;
        return nxvim_gui::run(session, config, None);
    }

    // Default role: embedded server on the local disk. A *directory* argument
    // (`nxvim-gui somedir`) opens the system file picker at that directory rather than
    // the server's in-window netrw listing — so it is the GUI client's job: divert it
    // from `ServerInit.file` (which would open the explorer) into `open_dir`, leaving the
    // server on `[No Name]`. (Only the local embedded role — a daemon session's files
    // live on the daemon's fs, so a local native picker would be meaningless there.)
    let (file, open_dir) = match file {
        Some(f) if Path::new(&f).is_dir() => (None, Some(PathBuf::from(f))),
        other => (other, None),
    };
    let session = spawn_session(None, file)?;
    nxvim_gui::run(session, config, open_dir)
}

/// Parse the command line into `(positionals, config)`: non-flag arguments (the first is
/// the file to open) in order, plus the font config — `--font <name>` / `--font-size
/// <pt>` (or the `=` form) set the font, taking precedence over the `NXVIM_GUI_FONT` /
/// `NXVIM_GUI_FONT_SIZE` environment the config starts from. `--font` accepts a
/// comma-separated fallback list (`--font "JetBrains Mono,Noto Color Emoji"`), tried in
/// order for a glyph the primary lacks. Unknown flags (e.g. `--connect-daemon`) are
/// ignored here — they are handled by `main`.
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
