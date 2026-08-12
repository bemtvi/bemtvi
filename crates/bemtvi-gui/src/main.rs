//! `bemtvi-gui` entry point — the GUI binary is a **client only**. It opens a native
//! winit + wgpu window ([`bemtvi_gui::run`]) onto a local editor server (a headless
//! server on its own thread, joined over an in-process duplex — the same msgpack-RPC the
//! TUI binary uses). The editor (core + Lua + the keystroke path) always runs local; a
//! session can point its fs/process/watch/LSP host seams at a remote `--daemon` (the
//! *edit-host split*), either at startup or live via `:connect`.
//!
//! Startup roles (all build the *same* local editor — only the host seams differ):
//! - **default** (`bemtvi-gui [file]`): embedded, the local disk.
//! - **`--connect-daemon`** (`bemtvi-gui --connect-daemon [file]`): an edit-host session
//!   over a `--daemon` child spawned on stdio — the sibling `bemtvi` binary by default, or
//!   whatever `BEMTVI_DAEMON_CMD` names (e.g. `ssh host bemtvi --daemon`).
//! - **`bemtvi://HOST:PORT/TOKEN?cert=HASH`**: an edit-host session over a QUIC
//!   (WebTransport) `--daemon --listen` listener.
//!
//! After startup, `:connect [user@]host[:port][/file]` (ssh stdio, with native askpass)
//! and `:connect bemtvi://…` (QUIC) switch a running window to a daemon session — see the
//! session loop in [`bemtvi_gui::run`].
//!
//! There is deliberately **no** daemon-serving role in the GUI (run `bemtvi --daemon` for
//! that) and **no** "whole editor runs remote, thin client local" role: the edit-host
//! split keeps the editor local for a zero-round-trip keystroke path, moving only the
//! fs/process/watch/LSP boundary across the wire.

use std::path::{Path, PathBuf};

use anyhow::Result;
use bemtvi_gui::remote::{self, ConnectTarget};
use bemtvi_gui::{spawn_session, spawn_stdio_daemon_session, GuiConfig};
use bemtvi_server::ConfigSource;
use clap::Parser;

/// bemtvi-gui's command line. clap derives the parser, validates flags, errors on unknown
/// options, and generates `--help`/`--version` — the same real parsing the `bemtvi` (TUI)
/// binary uses, replacing the old hand-rolled scan.
///
/// The GUI is a **client only**: there is no `--daemon`/`--listen` serving role and no
/// `--test-plugin`/`--lua` headless role (run `bemtvi` for those). What it does share with
/// the TUI is the connect surface — the positional [`Cli::targets`] (a FILE and/or a
/// `bemtvi://…` connect URI) and `--connect-daemon` — plus its own font/render options.
#[derive(Parser)]
#[command(
    name = "bemtvi-gui",
    version,
    about = "A modal, vim-style editor: the native (winit + wgpu) GUI client.",
    long_about = "A modal, vim-style editor: the native (winit + wgpu) GUI client. With no \
        role flag, bemtvi-gui opens the given file (or an empty buffer) in a window onto a \
        local editor server. A directory argument opens the system file picker there."
)]
struct Cli {
    /// File to open, and/or a bemtvi://… daemon connect URI
    #[arg(value_name = "TARGET")]
    targets: Vec<String>,

    /// Run the editor locally but route fs/process/watch/LSP to a --daemon child
    #[arg(long)]
    connect_daemon: bool,

    /// In a daemon session (at startup or a later :connect), run the daemon's config +
    /// plugins instead of the local config (default: local config)
    #[arg(long)]
    remote_config: bool,

    /// Font family (comma-separated fallback list), overriding BEMTVI_GUI_FONT
    #[arg(long, value_name = "NAME")]
    font: Option<String>,

    /// Font size in points, overriding BEMTVI_GUI_FONT_SIZE
    #[arg(long, value_name = "PT")]
    font_size: Option<f32>,

    /// Emoji / wide-glyph size relative to the cell, overriding BEMTVI_GUI_EMOJI_SCALE
    #[arg(long, value_name = "FACTOR")]
    emoji_scale: Option<f32>,
}

fn main() -> Result<()> {
    // If ssh re-invoked this binary as its `SSH_ASKPASS` helper (for a `:connect`
    // host-key/password prompt), pop the native dialog and exit — never start the editor.
    // Checked before clap, so the askpass argv (a prompt string) is never parsed as flags.
    if let Some(result) = remote::run_askpass_if_invoked() {
        return result;
    }

    // clap parses, validates, and exits with usage/version itself for `--help`/`-h`,
    // `--version`/`-V`, and unknown options.
    let cli = Cli::parse();

    // The render config starts from the `BEMTVI_GUI_FONT*` environment; the typed CLI
    // options (already validated by clap) override it.
    let mut config = GuiConfig::from_env();
    if let Some(name) = &cli.font {
        // `set_font` accepts a comma-separated fallback list, tried in order for a glyph
        // the primary lacks (`--font "JetBrains Mono,Noto Color Emoji"`).
        config.set_font(name);
    }
    if let Some(pt) = cli.font_size {
        config.set_font_size(pt);
    }
    if let Some(scale) = cli.emoji_scale {
        config.set_emoji_scale(scale);
    }

    // Which config (+ shada) a daemon session runs: the local machine's by default, or
    // the daemon's with `--remote-config`. Unlike the TUI, the GUI can `:connect` after a
    // local start, so the flag is session-wide (it also applies to later `:connect`s) and
    // is never rejected for a local startup.
    let config_source = if cli.remote_config {
        ConfigSource::Remote
    } else {
        ConfigSource::Local
    };

    // A `bemtvi://…` connect URI (the QUIC daemon target) is not a file; the first other
    // positional is the file to open.
    let connect_uri = cli
        .targets
        .iter()
        .find(|a| remote::is_connect_uri(a))
        .cloned();
    let file = cli.targets.into_iter().find(|a| !remote::is_connect_uri(a));

    // Edit-host split, local half, over QUIC: a `bemtvi://…` target connects to a
    // `--daemon --listen` listener and routes fs/process/watch/LSP to it.
    if let Some(uri) = connect_uri {
        let session = spawn_session(Some(ConnectTarget::Quic(uri)), file, config_source)?;
        return bemtvi_gui::run(session, config, None, config_source);
    }

    // Edit-host split, local half, over stdio: spawn a `--daemon` child (sibling `bemtvi`
    // or `BEMTVI_DAEMON_CMD`) and route fs/process/watch/LSP to it over stdio pipes.
    if cli.connect_daemon {
        let session = spawn_stdio_daemon_session(file, config_source)?;
        return bemtvi_gui::run(session, config, None, config_source);
    }

    // Default role: embedded server on the local disk. A *directory* argument
    // (`bemtvi-gui somedir`) opens the system file picker at that directory rather than
    // the server's in-window netrw listing — so it is the GUI client's job: divert it
    // from `ServerInit.file` (which would open the explorer) into `open_dir`, leaving the
    // server on `[No Name]`. (Only the local embedded role — a daemon session's files
    // live on the daemon's fs, so a local native picker would be meaningless there.)
    let (file, open_dir) = match file {
        Some(f) if Path::new(&f).is_dir() => (None, Some(PathBuf::from(f))),
        other => (other, None),
    };
    let session = spawn_session(None, file, config_source)?;
    bemtvi_gui::run(session, config, open_dir, config_source)
}
