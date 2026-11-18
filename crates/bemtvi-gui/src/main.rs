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

// A Windows GUI-subsystem binary: launched from Explorer or a shortcut it opens only its
// window, not a console window beside it. Run from a terminal it attaches to that one
// instead ([`attach_parent_console`]); with neither, a startup error is shown in a dialog
// ([`cannot_start`]). The children it spawns stay windowless through
// `bemtvi_core::process::no_console_window`.
#![windows_subsystem = "windows"]

mod window_state;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::Result;
use bemtvi_gpu::remote::{self, ConnectTarget};
use bemtvi_gpu::{
    spawn_session, spawn_stdio_daemon_session, GlyphOverflow, GuiConfig, Session, WindowMaximize,
    WindowPosition, WindowSizing,
};
use bemtvi_mitsuami::{Editor, EditorHandle};
use bemtvi_server::ConfigSource;
use clap::Parser;
use mitsuami::core::{CurrentWindow, UiEvent};
use mitsuami::prelude::{
    effect, inject, signal, spawn_local, Column, ElementBuilder, LengthExt, NodeId, Placement,
    Signal, Size, Ui, Window, WindowFrame, WindowSize,
};
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

/// Borrow the console of the terminal that launched us, if any, so `--help`, `--version`,
/// usage errors and a startup error still print there. A GUI-subsystem process gets no
/// console of its own; with no parent console (an Explorer launch) this does nothing, and
/// [`cannot_start`] falls back to a dialog.
/// A stdout the parent already redirected (a pipe, a file) is kept, never swapped for the
/// console. On other platforms stdio is simply inherited and this is a no-op.
fn attach_parent_console() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{
            AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
        };
        // SAFETY: plain Win32 calls taking no pointers. A failed attach (no parent
        // console) changes nothing.
        unsafe {
            if GetStdHandle(STD_OUTPUT_HANDLE).is_null() {
                AttachConsole(ATTACH_PARENT_PROCESS);
            }
        }
    }
}

/// Whether a terminal shows what we print: stdin or stderr is one. Launched from the
/// desktop (Explorer, Finder, a `.desktop` file) neither is, and stderr goes to a log or
/// nowhere, so [`cannot_start`] shows its error in a dialog too.
fn has_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal() || std::io::stdin().is_terminal()
}

/// bemtvi-gui can't start: say why on stderr and, with no terminal to show it, in the
/// platform's error dialog too (`mitsuami::fatal_error`, which needs no running app), so
/// a launch from the desktop that fails says why instead of vanishing. Returns once the
/// dialog is dismissed.
fn cannot_start(message: &str) {
    eprintln!("{message}");
    if !has_terminal() {
        mitsuami::fatal_error("bemtvi can't start", message.trim_end());
    }
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

    attach_parent_console();

    // A startup error is reported here rather than returned, so it reaches a dialog when
    // there is no terminal.
    if let Err(e) = start() {
        cannot_start(&format!("Error: {e:?}"));
        std::process::exit(1);
    }
    Ok(())
}

/// Everything after the askpass check: parse the command line, build the startup
/// session and run the window until it closes.
fn start() -> Result<()> {
    // clap validates and renders usage/version for `--help`/`-h`, `--version`/`-V`, and
    // unknown options, and prints and exits itself; only a usage error with no terminal to
    // read it also goes to the dialog.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) if !e.use_stderr() || has_terminal() => e.exit(),
        Err(e) => {
            cannot_start(&e.render().to_string());
            std::process::exit(e.exit_code());
        }
    };

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

/// A first window's content size, before anything was saved.
const FIRST_SIZE: Size = Size::new(960.0, 640.0);

/// The window: titled `bemtvi`, its whole content the editor, focused so keys go to it
/// from the start. Built in mitsuami's app scope.
///
/// It opens as it was when the GUI last closed (see [`window_state`]): where, as large,
/// and maximized or not, or as the `'guiposition'`, `'guisize'` and `'guimaximize'`
/// modes saved then say (centred, a share of the screen, always maximized). The
/// editor's options then have their say as its config sets them: a mode other than the
/// one the window opened by applies at once (centred, resized, maximized or restored),
/// and the modes current when the editor quits decide how the window opens next time.
fn window(
    handle: EditorHandle,
    session: Session,
    config: GuiConfig,
    open_dir: Option<PathBuf>,
    config_source: bemtvi_server::ConfigSource,
    mut signals: UnboundedReceiver<()>,
) -> Window {
    let open = signal(true);
    let saved = window_state::load();
    let ui = inject::<Ui>().expect("bemtvi-gui: the window is built in mitsuami's app scope");
    let (size, at) = opening(&ui, saved);
    // Set by the user maximizing or restoring it too, the platform's way.
    let maximized = signal(saved.maximized);
    let maximize = Rc::new(Cell::new(WindowMaximize::default()));
    // The modes the window is by: what it opened by, then the editor's.
    let position = signal(saved.position);
    let sizing = signal(saved.sizing);
    // Its restored frame, as the platform reports it (and where it opens, if it was).
    let frame = signal(at);
    // Where the platform places windows itself (GTK, Wayland), the window has no
    // position to centre: say so once, rather than let `centered` do nothing.
    // The editor's own `'guiposition'`, once its config has run (the saved mode, which
    // the window opened by, may not be the session's).
    let relayed_position = signal(None::<WindowPosition>);
    let told = handle.clone();
    let said = Cell::new(false);
    effect(move || {
        let unplaced = frame.get().is_some_and(|f| f.position.is_none());
        let centred = relayed_position.get() == Some(WindowPosition::Centered);
        if unplaced && centred && !said.replace(true) {
            let note = told.exec_lua(
                "btv.notify_once(\"guiposition: this platform places windows itself, so the \
                 window can't be centred\", btv.log.levels.WARN)",
            );
            spawn_local(async move {
                if let Err(e) = note.await {
                    eprintln!("bemtvi-gui: {e:#}");
                }
            });
        }
    });
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
    let placement = match saved.position {
        WindowPosition::Centered => Placement::Centred,
        WindowPosition::Remember => Placement::System,
    };
    Window::new("bemtvi")
        .size(size)
        .frame(frame)
        .placement(placement)
        .maximized(maximized)
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
            let ui = inject::<Ui>().expect("bemtvi-gui: a window's content has its Ui");
            let CurrentWindow(window) =
                inject::<CurrentWindow>().expect("bemtvi-gui: a window's content has its window");
            let (on_mode, exit_mode) = (maximize.clone(), maximize.clone());
            let sized = ui.clone();
            let remembered = saved.frame;
            // A share of the screen was taken of the display a new window opens on, the
            // primary one (the window didn't exist yet). Once the platform says where
            // the window is (its first frame), and that's another display (a
            // remembered place on it), the share is of that one's work area, once.
            let fitting = ui.clone();
            let fitted = Cell::new(false);
            ui.on_event(window, move |event| {
                // Maximized, its restored frame isn't reported until it's restored:
                // the first one it is then is the one to fit.
                if !matches!(event, UiEvent::FrameChanged(_))
                    || fitted.get()
                    || maximized.get_untracked()
                {
                    return;
                }
                fitted.set(true);
                let WindowSizing::Screen { width, height } = sizing.get_untracked() else {
                    return;
                };
                let (Some(here), Some(primary)) =
                    (fitting.work_area(Some(window)), fitting.work_area(None))
                else {
                    return;
                };
                let differs = |a: f32, b: f32| (a - b).abs() > b * 0.02;
                if differs(here.width, primary.width) || differs(here.height, primary.height) {
                    let centred = position.get_untracked() == WindowPosition::Centered;
                    resize_to_share(&fitting, window, frame, share(here, width, height), centred);
                }
            });
            // Closing the last window ends mitsuami's run loop, and `run`'s teardown.
            Column::new().size(100.pct(), 100.pct()).child(
                Editor::new(session, move || {
                    // Every way the editor quits passes here (`:qa`, the close button, a
                    // kill, its server going away), so the state is saved once, as the
                    // window still is.
                    let now = window_state::Saved {
                        maximized: maximized.get_untracked(),
                        position: position.get_untracked(),
                        sizing: sizing.get_untracked(),
                        // Before the window said where it is (quit at once), the
                        // frame it opened by, or the one saved last time: kept.
                        frame: frame.get_untracked().or(remembered),
                    };
                    window_state::save(exit_mode.get(), now);
                    open.set(false);
                })
                .on_window_maximize(move |mode| {
                    on_mode.set(mode);
                    match mode {
                        WindowMaximize::Always => maximized.set(true),
                        WindowMaximize::Never => maximized.set(false),
                        WindowMaximize::Remember => {}
                    }
                })
                .on_window_position(move |mode| {
                    relayed_position.set(Some(mode));
                    if position.get_untracked() == mode {
                        return;
                    }
                    position.set(mode);
                    // A maximized window keeps the screen; centred is how it opens
                    // next time.
                    if mode == WindowPosition::Centered && !maximized.get_untracked() {
                        ui.centre_window(window);
                    }
                })
                .on_window_sizing(move |mode| {
                    if sizing.get_untracked() == mode {
                        return;
                    }
                    sizing.set(mode);
                    // A maximized window keeps the screen; the share is how it opens
                    // next time.
                    if let WindowSizing::Screen { width, height } = mode {
                        if !maximized.get_untracked() {
                            if let Some(area) = sized.work_area(Some(window)) {
                                let centred = position.get_untracked() == WindowPosition::Centered;
                                resize_to_share(
                                    &sized,
                                    window,
                                    frame,
                                    share(area, width, height),
                                    centred,
                                );
                            }
                        }
                    }
                })
                .handle(handle.clone())
                .config(config.clone())
                .open_dir(open_dir.clone())
                .config_source(config_source)
                .autofocus(true),
            )
        })
}

/// How large the window opens, and the frame that places it, if it's to open where it
/// was: as large as it was (`remember`; [`FIRST_SIZE`] the first time) or a share of the
/// screen a new window opens on (mitsuami's `WindowSize::Screen`), and where it was
/// unless it's to be centred (or the platform couldn't say where it was). A frame needs
/// its size in points, so there the share is taken here, of the same screen.
fn opening(ui: &Ui, saved: window_state::Saved) -> (WindowSize, Option<WindowFrame>) {
    let size = match saved.sizing {
        WindowSizing::Screen { width, height } => WindowSize::Screen { width, height },
        WindowSizing::Remember => saved.frame.map_or(FIRST_SIZE, |f| f.size).into(),
    };
    let placed = match saved.position {
        WindowPosition::Remember => saved.frame.filter(|f| f.position.is_some()),
        WindowPosition::Centered => None,
    };
    let frame = placed.map(|f| WindowFrame {
        position: f.position,
        size: match size {
            WindowSize::Screen { width, height } => ui
                .work_area(None)
                .map_or(FIRST_SIZE, |area| share(area, width, height)),
            _ => f.size,
        },
    });
    (size, frame)
}

/// Gives the window a share of its screen now. Centred, it's resized and centred again
/// (which keeps it on the screen). Otherwise as its frame (where it is, at that size),
/// so the platform keeps it on the screen as it restores a frame, rather than letting
/// it grow from its top left corner past the screen's edges; before the platform has
/// said where it is, as a size. (Not the frame and then centring: a frame set in an
/// event handler is sent after the handler, so after the centring.)
fn resize_to_share(
    ui: &Ui,
    window: NodeId,
    frame: Signal<Option<WindowFrame>>,
    size: Size,
    centred: bool,
) {
    match frame.get_untracked() {
        Some(now) if !centred => frame.set(Some(WindowFrame { size, ..now })),
        _ => {
            ui.set_window_size(window, size);
            if centred {
                ui.centre_window(window);
            }
        }
    }
}

/// `width` × `height` (fractions) of a work area, in whole points.
fn share(area: Size, width: f32, height: f32) -> Size {
    Size::new((area.width * width).round(), (area.height * height).round())
}
