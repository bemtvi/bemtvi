//! `bemtvi-gui` entry point — the GUI binary is a **client only**. It opens a native
//! mitsuami + wgpu window ([`run`]) onto a local editor server (a headless
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
//! session loop in the client's IO thread (`bemtvi_gpu::spawn_io`).
//!
//! There is deliberately **no** daemon-serving role in the GUI (run `bemtvi --daemon` for
//! that) and **no** "whole editor runs remote, thin client local" role: the edit-host
//! split keeps the editor local for a zero-round-trip keystroke path, moving only the
//! fs/process/watch/LSP boundary across the wire.

use std::path::{Path, PathBuf};

use anyhow::Result;
use bemtvi_gpu::remote::{self, ConnectTarget};
use bemtvi_gpu::{spawn_session, spawn_stdio_daemon_session, GlyphOverflow, GuiConfig, Session};
use bemtvi_mitsuami::{Editor, EditorHandle};
use bemtvi_server::ConfigSource;
use clap::Parser;
use mitsuami::prelude::{signal, spawn_local, Column, ElementBuilder, LengthExt, Size, Window};
use tokio::sync::mpsc::UnboundedReceiver;

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
    about = "A modal, vim-style editor: the native (mitsuami + wgpu) GUI client.",
    long_about = "A modal, vim-style editor: the native (mitsuami + wgpu) GUI client. With no \
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

    /// Max emoji / wide-glyph size relative to the cell (a glyph too wide for its
    /// cells is shrunk below this), overriding BEMTVI_GUI_EMOJI_SCALE
    #[arg(long, value_name = "FACTOR")]
    emoji_scale: Option<f32>,

    /// When a square one-cell glyph (a Nerd Font icon) may render at full size,
    /// overflowing into the next cell instead of being shrunk to fit its own:
    /// never | always | when-followed-by-space (default). Overrides
    /// BEMTVI_GUI_GLYPH_OVERFLOW
    #[arg(long, value_name = "MODE", value_parser = parse_glyph_overflow)]
    glyph_overflow: Option<GlyphOverflow>,
}

/// clap value parser for `--glyph-overflow`, so a misspelt mode is a usage error
/// naming the alternatives rather than a silently-ignored flag.
fn parse_glyph_overflow(spec: &str) -> Result<GlyphOverflow, String> {
    GlyphOverflow::parse(spec)
        .ok_or_else(|| format!("expected {}, got `{spec}`", GlyphOverflow::NAMES))
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
    if let Some(mode) = cli.glyph_overflow {
        config.glyph_overflow = mode;
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
        return run(session, config, None, config_source);
    }

    // Edit-host split, local half, over stdio: spawn a `--daemon` child (sibling `bemtvi`
    // or `BEMTVI_DAEMON_CMD`) and route fs/process/watch/LSP to it over stdio pipes.
    if cli.connect_daemon {
        let session = spawn_stdio_daemon_session(file, config_source)?;
        return run(session, config, None, config_source);
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
    run(session, config, open_dir, config_source)
}

/// Run the GUI client until the window closes or the server disconnects, rendering with
/// `config`'s font. Must be called on the main thread (the platform's UI toolkits'
/// requirement).
///
/// `initial` is the already-built startup [`Session`] (its server thread is running) —
/// embedded local, or a daemon session if launched with `--connect-daemon` / a
/// `bemtvi://` target. After startup, `:connect [user@]host[:port][/file]` or
/// `:connect bemtvi://…` swaps the window onto a **new** local server whose host seams
/// point at a daemon (see [`spawn_session`]), and `:workspace [dir]` swaps it
/// onto a fresh **local** session rooted at a directory (its own shada namespace + saved
/// layout — see `bemtvi_gpu::spawn_workspace_session`). Either way the IO thread builds the
/// new session off the UI thread, then the session loop retires the old server (it winds
/// down on EOF) and re-attaches the UI onto the new one. The editor always runs local —
/// only the fs/process/watch/LSP seams cross the wire.
///
/// The window's content is the same [`Editor`] a mitsuami app mounts in its own
/// layout; what's here is what belongs to an app that owns the process: the window, the
/// signals, and the exit status.
fn run(
    initial: Session,
    config: GuiConfig,
    open_dir: Option<PathBuf>,
    config_source: bemtvi_server::ConfigSource,
) -> Result<()> {
    // Be killable without losing the session. There is no terminal to hand back here
    // — a window is not a tty — so this client takes only the *graceful* half: a
    // `kill` / SIGHUP / SIGINT wakes the UI thread through the channel and the editor
    // quits itself properly (see [`EditorHandle::quit_now`]), with the watchdog
    // in `bemtvi_view::signals` still guaranteeing the process dies if that goes nowhere.
    let (signal_tx, signal_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    bemtvi_view::signals::install(bemtvi_view::signals::Config {
        restore_sequence: Vec::new(),
        restore_termios: false,
        on_shutdown: Box::new(move || {
            let _ = signal_tx.send(());
        }),
    });

    let handle = EditorHandle::new();
    let ui_handle = handle.clone();
    // Runs until the window closes. The app id matches the installed `.desktop` file's
    // basename (its `StartupWMClass`), so Linux desktops associate the AppImage's icon
    // with the window; it's also an unpackaged Windows build's AppUserModelID.
    mitsuami::App::new()
        .id("bemtvi")
        .name("bemtvi")
        .open(move || {
            window(
                ui_handle,
                initial,
                config,
                open_dir,
                config_source,
                signal_rx,
            )
        })
        .run();

    // The UI is done: stop the IO thread and wait for it, so the streams are dropped
    // (servers see EOF) and their threads are joined before returning.
    let panicked = handle.close();
    if panicked {
        eprintln!("bemtvi-gui: a server thread panicked");
        std::process::exit(101);
    }
    // If we are only here because the process was killed, the shutdown is now
    // complete — the editor ran its exit sequence and the server flushed its shada.
    // Die of that signal so whoever sent it (a shell, a session manager logging the
    // user out) sees the kill it asked for, not a clean exit 0.
    bemtvi_view::signals::exit_as_signal_if_killed();
    Ok(())
}

/// The window: titled `bemtvi`, 960×640 points, its whole content the editor, focused so
/// keys go to it from the start. Built in mitsuami's app scope.
fn window(
    handle: EditorHandle,
    session: Session,
    config: GuiConfig,
    open_dir: Option<PathBuf>,
    config_source: bemtvi_server::ConfigSource,
    mut signals: UnboundedReceiver<()>,
) -> Window {
    let open = signal(true);
    // Killed: quit the session the way `:qall!` does (see `EditorHandle::quit_now`).
    let killed = handle.clone();
    spawn_local(async move {
        // An editor that already quit is already on its way out.
        if signals.recv().await.is_some() {
            let _ = killed.quit_now();
        }
    });
    let closing = handle.clone();
    // `content` may be asked for again; the session is only there the first time.
    let session = std::cell::Cell::new(Some(session));
    Window::new("bemtvi")
        .size(Size::new(960.0, 640.0))
        .open(open)
        // Don't close on the close button directly — that would discard a modified
        // buffer silently. Route through the server's `:qa`, which quits a clean
        // editor (it then sends `bemtvi_exit`, which closes the window through the
        // editor's `on_exit`) but refuses with `E37` when a buffer is unsaved, keeping
        // the window open. mitsuami routes the platform's Quit (macOS's app menu, the
        // Dock, a logout) here too.
        // An editor that already quit has nothing to refuse with: just close.
        .on_close_request(move || {
            if closing.request_quit().is_err() {
                open.set(false);
            }
        })
        .content(move || {
            let session = session
                .take()
                .expect("bemtvi-gui: the window's content is built once");
            // Closing the last window ends mitsuami's run loop, and `run`'s teardown.
            Column::new().size(100.pct(), 100.pct()).child(
                Editor::new(session, move || open.set(false))
                    .handle(handle.clone())
                    .config(config.clone())
                    .open_dir(open_dir.clone())
                    .config_source(config_source)
                    .autofocus(true),
            )
        })
}
