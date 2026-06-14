//! The native (winit + wgpu) GUI client.
//!
//! A thin RPC client that owns no editor state — the GUI sibling of `nxvim-tui`.
//! It attaches to the server, sends keystrokes as vim key-notation
//! (`nvim_input`), and paints the server's [`View`] (the same `redraw` model the
//! TUI consumes) onto a GPU surface as a monospace cell grid.
//!
//! **Threading.** winit owns the main thread (its event loop is not async), so
//! the RPC lives on a separate IO thread running a current-thread tokio runtime:
//! it drives [`nxvim_rpc::connect`], decodes each `redraw` into a [`View`], and
//! forwards it to the event loop as a [`UserEvent`] via an
//! [`winit::event_loop::EventLoopProxy`]. Input flows the other way without a
//! runtime — [`nxvim_rpc::Rpc`] is `Clone + Send` and its `notify` is synchronous,
//! so the winit thread fires `nvim_input` / `nvim_ui_*` directly on a cloned
//! handle.
//!
//! The focused window scrolls pixel-smoothly: a redraw's scroll gesture arms a
//! [`ScrollAnim`] the client clock interpolates, and `about_to_wait` paces the
//! per-frame repaint. See [`render`] for the rendering scope.
//!
//! **Native file dialogs.** Pressing `<CR>` over certain `:` command lines pops a
//! system dialog instead of running the command as typed — a GUI-only affordance;
//! the server stays unaware (the dialog is the client's to show, then it issues
//! the real command):
//!
//! - The `…o` open family ([`open_dialog_verb`]) — `:eo`, `:spo`, `:vso`,
//!   `:tabeo`, `:newo`, `:vnewo` (and bare `:e`, an alias of `:eo`) — pops the
//!   **open** dialog and re-runs the base command (`:e`/`:sp`/`:vs`/`:tabe`/…)
//!   with the chosen file, preserving its edit / split / tab semantics.
//! - `:wo`, and a bare `:w` on an unnamed buffer ([`save_dialog_needed`]), pop the
//!   **save** dialog and write the buffer to the chosen path (`:w <file>`, which
//!   also binds the buffer to it). (`:wo` mirrors the `…o` open family and, unlike
//!   `:wn`, collides with no real ex-command — vim's `:wn` is `:wnext`.)

mod input;
mod mouse;
pub mod remote;
mod render;
mod session;

pub use input::{encode_key, is_paste};
pub use mouse::{
    button_name, cell_at, drain_notches, horizontal_action, mouse_modifier, panel_close_button,
    panel_content_rect, vertical_action, within,
};
pub use session::{parse_connect_uri, spawn_session, spawn_stdio_daemon_session, Session};
// The pure inline-inlay-hint geometry (the shift math) and the segment splice, so
// the Tier-1 `inlay` test can exercise them without a GPU — like the mouse helpers.
pub use render::{inlay_shift, splice_inlay, Seg, DEFAULT_INLAY};
// The pure per-row syntax-coloring layer (run splitting + the no-colorscheme group
// fallback), exported so the Tier-1 `syntax` test can exercise it without a GPU.
pub use render::{group_fallback, row_segments};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_view::{encode_paste, HlSpan, InlayHint, ScrollData, Style, View};
use rmpv::Value;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{Window, WindowId};

use render::{Renderer, ScrollFrame};

/// How long the client waits after a keystroke, with no further input, before
/// sending the server a synthetic `nxvim_input_flush` — vim's `timeoutlen`
/// (default 1000ms). The server has no input timer, so a trailing key that is a
/// live *prefix* of a mapping stays withheld in the matcher until something
/// flushes it; this idle nudge is that something (mirrors the TUI's `TIMEOUT_LEN`).
const TIMEOUT_LEN: Duration = Duration::from_millis(1000);

/// How often a held-at-the-edge mouse drag re-sends itself to keep the buffer
/// auto-scrolling (≈25 lines/sec). winit only re-reports a drag (a `CursorMoved`
/// while the button is held) on pointer motion, so without this repeat a
/// selection dragged to the edge and held still would stop scrolling; this paces
/// the continuous scroll the server does a line at a time per drag event.
const AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(40);

/// Whether a drag at grid `row` (global, 0-based) sits in the top/bottom edge
/// band that arms continuous auto-scroll, for a grid `rows` cells tall. The top
/// row and the bottom rows (the status line and the command row below the text
/// body) qualify; the server decides whether that actually scrolls the focused
/// window. Mirrors the TUI's `in_scroll_zone`.
fn in_scroll_zone(row: u16, rows: u16) -> bool {
    row == 0 || row + 2 >= rows
}

/// Client-side rendering configuration. The font family and point size are a
/// purely client concern — the server works in cells, not pixels — so they're set
/// here (CLI flags / environment), not through a server option. An unavailable
/// font name falls back to the system monospace.
#[derive(Clone, Debug)]
pub struct GuiConfig {
    /// Font family name (e.g. `"JetBrains Mono"`); `None` uses the system monospace.
    pub font: Option<String>,
    /// Font point size, before the display's scale factor is applied.
    pub font_size: f32,
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            font: None,
            font_size: 15.0,
        }
    }
}

impl GuiConfig {
    /// Overrides from the environment: `NXVIM_GUI_FONT` (family) and
    /// `NXVIM_GUI_FONT_SIZE` (points). Absent/blank/invalid values keep the default;
    /// CLI flags layered on top take precedence (see `main`).
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Some(name) = std::env::var_os("NXVIM_GUI_FONT") {
            c.set_font(&name.to_string_lossy());
        }
        if let Ok(size) = std::env::var("NXVIM_GUI_FONT_SIZE") {
            if let Ok(pt) = size.trim().parse::<f32>() {
                c.set_font_size(pt);
            }
        }
        c
    }

    /// Set the font family, ignoring a blank name (which keeps the monospace
    /// default rather than asking the font system for `""`).
    pub fn set_font(&mut self, name: &str) {
        let name = name.trim();
        if !name.is_empty() {
            self.font = Some(name.to_string());
        }
    }

    /// Set the point size, clamped to `[4, 200]` so a typo can't produce a zero,
    /// negative, or absurd cell. A non-finite or non-positive value is ignored.
    pub fn set_font_size(&mut self, pt: f32) {
        if pt.is_finite() && pt > 0.0 {
            self.font_size = pt.clamp(4.0, 200.0);
        }
    }
}

/// Events the IO thread injects into the winit event loop.
pub enum UserEvent {
    /// A decoded `redraw`: replace the view and repaint.
    Redraw(Box<View>),
    /// A `:connect` brought up a new (daemon or local) session: swap the App's live
    /// RPC handle to it, mark whether it is remote, and re-attach the UI.
    Connected {
        rpc: Box<Rpc>,
        /// Whether the new session is an edit-host (daemon) one — drives the local
        /// file-dialog suppression (see [`dialog_action`]).
        remote: bool,
    },
    /// The server asked the UI to exit (`nxvim_exit`), or the connection closed.
    Exit,
}

/// Run the GUI client until the window closes or the server disconnects, rendering with
/// `config`'s font. Must be called on the main thread (winit's requirement).
///
/// `initial` is the already-built startup [`Session`] (its server thread is running) —
/// embedded local, or a daemon session if launched with `--connect-daemon` / a
/// `nxvim://` target. After startup, `:connect [user@]host[:port][/file]` or
/// `:connect nxvim://…` swaps the window onto a **new** local server whose host seams
/// point at a daemon (see [`session::spawn_session`]): the IO thread builds it off the UI
/// thread, then the session loop retires the old server (it winds down on EOF) and
/// re-attaches the UI onto the new one. The editor always runs local — only the
/// fs/process/watch/LSP seams cross the wire.
pub fn run(initial: Session, config: GuiConfig, open_dir: Option<PathBuf>) -> Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    // Shutdown signal: when the event loop exits — the window's close button, or a
    // server-sent `nxvim_exit` — the main thread notifies the IO thread so it drops the
    // RPC connection. The IO thread (not `run`) owns the stream, so unless it closes
    // that, the server never sees EOF, never winds down, and the join below hangs
    // forever — the close-button freeze.
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let io_shutdown = shutdown.clone();

    // Hand the `Rpc` handle from the IO thread back to the main thread once the
    // (first) connection is up, so the winit side can fire input synchronously.
    let (rpc_tx, rpc_rx) = std::sync::mpsc::channel::<Rpc>();
    // `:connect <target>` from the App requests a switch to a daemon (or local) session.
    let (reconnect_tx, mut reconnect_rx) =
        tokio::sync::mpsc::unbounded_channel::<remote::ConnectTarget>();
    let initial_remote = initial.remote;

    let io = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build client IO runtime");
        runtime.block_on(async move {
            // The current session's transport + server thread, plus the retired servers
            // of earlier `:connect`s (each winds down on EOF; joined at teardown).
            let mut stream = initial.stream;
            let mut current_handle = Some(initial.handle);
            let mut next_remote = initial.remote;
            let mut retired: Vec<std::thread::JoinHandle<()>> = Vec::new();
            // A finished `:connect` build delivers its new session here, so the handshake
            // (run off the UI thread on the blocking pool) doesn't stall the current
            // session's redraws while it's in flight.
            let (built_tx, mut built_rx) =
                tokio::sync::mpsc::unbounded_channel::<Result<Session>>();
            let mut first = true;

            'session: loop {
                let (reader, writer) = tokio::io::split(stream);
                let (rpc, mut incoming) = connect(reader, writer);
                // Hand the connection to the App: the first synchronously (so `App::new`
                // has an `Rpc`); later ones as `Connected`, which swaps the live handle
                // and re-attaches. Dropping the *old* session's `rpc`/`incoming` on the
                // next iteration — together with the App swapping its clone — winds the
                // previous connection (and its server) down.
                if first {
                    let _ = rpc_tx.send(rpc.clone());
                    first = false;
                } else {
                    let event = UserEvent::Connected {
                        rpc: Box::new(rpc.clone()),
                        remote: next_remote,
                    };
                    if proxy.send_event(event).is_err() {
                        break 'session; // event loop gone
                    }
                }

                // Block until a `:connect` build succeeds (the only value-carrying
                // break), or `break 'session` on disconnect / exit / shutdown.
                let new_session: Session = loop {
                    tokio::select! {
                        message = incoming.recv() => match message {
                            Some(Incoming::Notification { method, params }) => match method.as_str() {
                                "redraw" => {
                                    let view = View::from_redraw(&params);
                                    if proxy.send_event(UserEvent::Redraw(Box::new(view))).is_err() {
                                        break 'session; // event loop gone
                                    }
                                }
                                "nxvim_exit" => {
                                    let _ = proxy.send_event(UserEvent::Exit);
                                    break 'session;
                                }
                                _ => {}
                            },
                            // Answer server-initiated requests with nil, like the TUI.
                            Some(Incoming::Request { id, .. }) => rpc.respond(id, Ok(Value::Nil)),
                            None => break 'session, // connection closed → exit UI
                        },
                        // `:connect <target>`: build the new session off the UI thread
                        // (the ssh/quic handshake — and its askpass dialog — can take
                        // seconds), so the current session keeps rendering meanwhile. The
                        // result arrives on `built_rx`.
                        target = reconnect_rx.recv() => match target {
                            Some(target) => {
                                let file = target.embedded_file();
                                let tx = built_tx.clone();
                                tokio::task::spawn_blocking(move || {
                                    let _ = tx.send(session::spawn_session(Some(target), file));
                                });
                            }
                            None => break 'session, // App (the only sender) is gone
                        },
                        // A `:connect` build finished. On success, restart the session on
                        // it; on failure, keep the current session and report why.
                        built = built_rx.recv() => match built {
                            Some(Ok(session)) => break session,
                            Some(Err(err)) => report_connect_error(&rpc, &err),
                            None => {} // unreachable: `built_tx` is held above
                        },
                        // The UI exited: stop, so dropping the runtime below closes the
                        // connection and the server winds down.
                        _ = io_shutdown.notified() => break 'session,
                    }
                };

                // Swap in the new session: retire the old server (it winds down when this
                // iteration's `rpc`/`incoming` drop on `continue`) and re-loop on the new
                // transport, which re-attaches the UI via `Connected`.
                if let Some(handle) = current_handle.take() {
                    retired.push(handle);
                }
                current_handle = Some(new_session.handle);
                next_remote = new_session.remote;
                stream = new_session.stream;
                continue 'session;
            }

            // Tell the UI to exit if it hasn't already. Dropping this scope's
            // `rpc`/`incoming` closes the current stream — the EOF that lets the current
            // server wind down. Join every server thread (current + retired), surfacing a
            // panic so a crashed server is a non-zero exit, not a silent clean quit.
            let _ = proxy.send_event(UserEvent::Exit);
            let mut panicked = false;
            for handle in current_handle.into_iter().chain(retired) {
                if handle.join().is_err() {
                    panicked = true;
                }
            }
            panicked
        })
    });

    let rpc = match rpc_rx.recv() {
        Ok(rpc) => rpc,
        Err(_) => {
            let _ = io.join();
            return Err(anyhow::anyhow!("client IO thread exited before connecting"));
        }
    };
    let mut app = App::new(rpc, config, open_dir, initial_remote, reconnect_tx);
    event_loop.run_app(&mut app)?;

    // The UI is done: stop the IO thread and wait for it, so the streams are dropped
    // (servers see EOF) and their threads are joined before returning.
    shutdown.notify_one();
    let panicked = io.join().unwrap_or(true);
    if panicked {
        eprintln!("nxvim-gui: a server thread panicked");
        std::process::exit(101);
    }
    Ok(())
}

/// Report a `:connect` failure (bad host, refused auth, a malformed `nxvim://` URI) in
/// the GUI message line via the *current* session — the new one never came up. The error
/// chain is flattened to one line (`echom` rejects newlines) and single quotes doubled
/// (Vim string escaping) so a hostname or path can't break the command.
fn report_connect_error(rpc: &Rpc, err: &anyhow::Error) {
    let line = format!(":connect failed: {err:#}").replace('\n', "; ");
    let escaped = line.replace('\'', "''");
    rpc.notify(
        "nvim_command",
        vec![Value::from(format!(
            "echohl ErrorMsg|echom '{escaped}'|echohl NONE"
        ))],
    );
}

/// An in-flight scroll slide, driven by the client clock. Mirrors the TUI's
/// `Animation`, but the GUI keeps `top`/`cursor` fractional (no rounding) for
/// sub-pixel smoothness. The band (`lines`/`numbers`/`highlights`, palette
/// `styles`) is the server's gesture snapshot, anchored at `base_line`.
struct ScrollAnim {
    from_top: f32,
    to_top: f32,
    from_cursor: f32,
    to_cursor: f32,
    start: Instant,
    duration: Duration,
    base_line: usize,
    lines: Vec<String>,
    selection: Vec<Option<(u16, u16)>>,
    /// Orientation of the sliding visual selection (see
    /// [`ScrollData::sel_extends_down`]); drives the selection edge clip.
    sel_extends_down: Option<bool>,
    numbers: Vec<Option<usize>>,
    highlights: Vec<Vec<HlSpan>>,
    /// `hlsearch` / `incsearch` match spans for the band, so the search highlight
    /// slides with the text instead of vanishing until the slide settles.
    search: Vec<Vec<(u16, u16)>>,
    incsearch: Vec<Option<(u16, u16)>>,
    inlay_hints: Vec<Vec<InlayHint>>,
    styles: Vec<Style>,
}

impl ScrollAnim {
    fn new(s: &ScrollData) -> Self {
        Self {
            from_top: s.from_top,
            to_top: s.to_top,
            from_cursor: s.from_cursor,
            to_cursor: s.to_cursor,
            start: Instant::now(),
            duration: s.duration,
            base_line: s.base_line,
            lines: s.lines.clone(),
            selection: s.selection.clone(),
            sel_extends_down: s.sel_extends_down,
            numbers: s.numbers.clone(),
            highlights: s.highlights.clone(),
            search: s.search.clone(),
            incsearch: s.incsearch.clone(),
            inlay_hints: s.inlay_hints.clone(),
            styles: s.styles.clone(),
        }
    }

    fn done(&self) -> bool {
        self.start.elapsed() >= self.duration
    }

    /// The interpolated frame at the current instant (ease-out cubic, matching
    /// the TUI's feel).
    fn frame(&self) -> ScrollFrame<'_> {
        let raw = if self.duration.is_zero() {
            1.0
        } else {
            (self.start.elapsed().as_secs_f32() / self.duration.as_secs_f32()).clamp(0.0, 1.0)
        };
        let t = 1.0 - (1.0 - raw).powi(3);
        let lerp = |a: f32, b: f32| a + (b - a) * t;
        ScrollFrame {
            top: lerp(self.from_top, self.to_top),
            cursor: lerp(self.from_cursor, self.to_cursor),
            base_line: self.base_line,
            lines: &self.lines,
            selection: &self.selection,
            // The selection's moving edge tracks the interpolated cursor; the clip
            // side follows the selection's orientation (anchor above ⇒ down), not
            // the scroll direction, so it grows *and* shrinks smoothly either way.
            sel_clip: self.sel_extends_down,
            numbers: &self.numbers,
            highlights: &self.highlights,
            search: &self.search,
            incsearch: &self.incsearch,
            inlay_hints: &self.inlay_hints,
            styles: &self.styles,
        }
    }
}

/// Decide the scroll slide to run after a `redraw`, given any slide already in
/// flight — the GUI port of the TUI's `arm_animation`. A gesture (re)arms a fresh
/// slide; a scroll-less redraw that merely repaints the slide's destination (a
/// delayed highlight reply) lets it play out; any other scroll-less redraw is a
/// real change and interrupts the slide.
fn arm_scroll(view: &View, current: Option<ScrollAnim>) -> Option<ScrollAnim> {
    if let Some(s) = view.focused().and_then(|w| w.scroll.as_ref()) {
        // A zero-duration gesture has no slide; show the static destination.
        if s.duration.is_zero() {
            return None;
        }
        return Some(ScrollAnim::new(s));
    }
    current.filter(|a| repaints_destination(view, a))
}

/// Whether `view` merely repaints the destination `anim` is sliding toward (same
/// first visible line and cursor line), so a delayed redraw must not abort it.
fn repaints_destination(view: &View, anim: &ScrollAnim) -> bool {
    let dest_top = anim.to_top as usize + 1; // first visible line, 1-based
    let dest_cursor = anim.to_cursor as usize + 1; // cursor line, 1-based
    let Some(win) = view.focused() else {
        return false;
    };
    win.numbers.first().copied().flatten() == Some(dest_top) && win.cursor_line == dest_cursor
}

/// The winit application: holds the window, the renderer, and the latest view.
struct App {
    rpc: Rpc,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    view: View,
    /// The in-flight scroll slide, if any (drives the per-frame repaint loop).
    scroll: Option<ScrollAnim>,
    mods: ModifiersState,
    /// Latest pointer position in physical pixels (winit's button and wheel events
    /// don't carry it, so `CursorMoved` is tracked here for the next press/wheel).
    cursor_px: (f64, f64),
    /// Whether the left mouse button is held — winit has no "drag" event, so a
    /// `CursorMoved` while this is set synthesizes one (server extends the Visual
    /// selection from the press anchor).
    mouse_down: bool,
    /// The cell the last left-drag was reported at, so motion within one cell is
    /// coalesced to a single drag (the server works in cells, not pixels).
    last_drag_cell: Option<(u16, u16)>,
    /// While the left button is held with the pointer in the top/bottom edge band,
    /// the cell to re-issue the drag at so the buffer keeps auto-scrolling without
    /// further pointer motion. `None` when not at an edge or the button is up.
    autoscroll: Option<(u16, u16)>,
    /// When the next auto-scroll drag repeat is due (paced by [`AUTOSCROLL_INTERVAL`]);
    /// drives a `WaitUntil` in [`Self::about_to_wait`], like [`Self::flush_deadline`].
    autoscroll_deadline: Option<Instant>,
    /// Fractional wheel remainder per axis `(horizontal, vertical)`, so a
    /// pixel-precise trackpad still scrolls one whole line at a time (see
    /// [`mouse::drain_notches`]).
    wheel_accum: (f32, f32),
    /// Client-side scroll offset for the completion doc preview (a pure UI gesture
    /// — the server owns no notion of the box's pixel height). Reset to 0 whenever
    /// the previewed docs change (a new selection, or the menu closing).
    doc_scroll: u16,
    /// Last `(cols, windows_rows)` reported to the server, to suppress
    /// no-op resize notifications.
    reported: (u16, u16),
    /// When set, fire one `nxvim_input_flush` once this instant passes with no
    /// further input — the GUI's `timeoutlen` timer (see [`TIMEOUT_LEN`]). Armed by
    /// each keystroke and re-armed by the next, so it measures idle-since-last-key.
    flush_deadline: Option<Instant>,
    /// Font config (CLI flags / environment) used as the renderer's startup default
    /// and the fallback for any field a `guifont` doesn't set.
    config: GuiConfig,
    /// The last `view.guifont` applied to the renderer, so a redraw only re-shapes
    /// when it actually changed (`:set guifont=…` / the init-time value).
    applied_guifont: String,
    /// A directory passed on the command line (`nxvim-gui somedir`), if any. Popped
    /// as the native file picker (anchored there) once the window exists, instead of
    /// the server's in-window listing — see [`Self::resumed`]. Taken on first use so
    /// it fires exactly once.
    open_dir: Option<PathBuf>,
    /// Whether this is an edit-host (daemon) session — buffers live on the daemon's
    /// fs, not the local disk. Suppresses the native open/save file dialogs (see
    /// [`dialog_action`]), which would otherwise browse and write the wrong machine.
    /// Updated on a `:connect` swap (see [`UserEvent::Connected`]).
    remote: bool,
    /// Requests a `:connect <target>` switch to a daemon (or local) session. The IO
    /// thread builds the new session and feeds the swapped handle back as
    /// [`UserEvent::Connected`] (see [`run`]).
    reconnect: tokio::sync::mpsc::UnboundedSender<remote::ConnectTarget>,
}

impl App {
    fn new(
        rpc: Rpc,
        config: GuiConfig,
        open_dir: Option<PathBuf>,
        remote: bool,
        reconnect: tokio::sync::mpsc::UnboundedSender<remote::ConnectTarget>,
    ) -> Self {
        Self {
            rpc,
            window: None,
            renderer: None,
            view: View::default(),
            scroll: None,
            mods: ModifiersState::empty(),
            cursor_px: (0.0, 0.0),
            mouse_down: false,
            last_drag_cell: None,
            autoscroll: None,
            autoscroll_deadline: None,
            wheel_accum: (0.0, 0.0),
            doc_scroll: 0,
            reported: (0, 0),
            flush_deadline: None,
            config,
            applied_guifont: String::new(),
            open_dir,
            remote,
            reconnect,
        }
    }

    /// Compute the grid, and if it changed since last time, tell the server. The
    /// reported height reserves the bottom row for the command line (like the
    /// TUI's one chrome row), so the windows area is `total_rows - 1`.
    fn report_size(&mut self, attach: bool) {
        let Some(renderer) = self.renderer.as_ref() else {
            return;
        };
        let (cols, total_rows) = renderer.grid_size();
        let win_rows = total_rows.saturating_sub(1).max(1);
        if !attach && self.reported == (cols, win_rows) {
            return;
        }
        self.reported = (cols, win_rows);
        let method = if attach {
            "nvim_ui_attach"
        } else {
            "nvim_ui_try_resize"
        };
        self.rpc.notify(
            method,
            vec![Value::from(cols), Value::from(win_rows), Value::Map(vec![])],
        );
    }

    /// Read the system clipboard and feed it to the server as one `nvim_input` via
    /// [`encode_paste`] — the GUI analogue of the TUI's bracketed paste (one
    /// notify, one redraw, no per-character trickle). A missing, empty, or
    /// non-text clipboard is a silent no-op (nothing to paste).
    fn paste_clipboard(&self) {
        let Ok(text) = arboard::Clipboard::new().and_then(|mut c| c.get_text()) else {
            return;
        };
        let notation = encode_paste(&text);
        if !notation.is_empty() {
            self.rpc
                .notify("nvim_input", vec![Value::from(notation.as_str())]);
        }
    }

    /// Apply the relayed `view.guifont` to the renderer: parse the family and `:h`
    /// size, fall back to the CLI/env [`GuiConfig`] for any unset field, re-shape,
    /// and re-report the grid (the cell size changed). Called whenever `guifont`
    /// changes — including its first non-empty value from `init.lua` — so a
    /// `:set guifont=…` takes effect live. A no-op before the renderer exists; it's
    /// applied from `resumed` once the renderer is built.
    fn apply_guifont(&mut self) {
        let (family, size) = parse_guifont(&self.view.guifont);
        let font = family.or_else(|| self.config.font.clone());
        let size = size.unwrap_or(self.config.font_size);
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        renderer.set_font(font.as_deref(), size);
        self.applied_guifont = self.view.guifont.clone();
        // The cell size changed, so the grid in cells did too — re-report it (the
        // server re-lays out for the new dimensions) and repaint.
        self.report_size(false);
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Pop the native **open** dialog for an `…o` open command, then run `<base>
    /// <file>` with the chosen path. Returns `true` once it has handled the `<CR>`
    /// (so the caller swallows the key).
    ///
    /// On a pick it aborts the half-typed command line (`<Esc>`) and runs the base
    /// command; on cancel it just aborts. nxvim's `:edit`-family parser takes the
    /// whole argument tail as the filename (no backslash unescaping, unlike vim),
    /// so the path is passed **raw** — a leading `fnameescape` would wrongly bake
    /// its `\ ` escapes into the name. The dialog is modal on the main thread
    /// (winit's requirement; on Linux rfd blocks on the portal call via pollster,
    /// safe here since the winit thread is outside the tokio runtime), which
    /// briefly blocks the event loop — fine for a file picker.
    fn pick_open(&self, base: &str) -> bool {
        let picked = rfd::FileDialog::new().set_title("Open").pick_file();
        self.run_picked(base, picked);
        true
    }

    /// Like [`Self::pick_open`] but opens the dialog **anchored at `dir`** — the GUI's
    /// answer to opening a *directory* (`:e somedir`, or `nxvim-gui somedir`). Rather
    /// than the server's in-window netrw listing, the user gets the system file picker
    /// already browsing that directory, and the picked file is opened with `base`.
    /// Same key-swallowing and raw-path contract as [`Self::pick_open`]; a relative
    /// `dir` resolves against the process cwd, exactly as the server's listing would.
    fn pick_open_at(&self, base: &str, dir: &str) -> bool {
        let picked = rfd::FileDialog::new()
            .set_title("Open")
            .set_directory(dir)
            .pick_file();
        self.run_picked(base, picked);
        true
    }

    /// Pop the native **save** dialog for `:wo` / a bare `:w` on an unnamed buffer,
    /// then `:w <file>` to the chosen path (which also binds the buffer to it).
    /// Same key-swallowing contract and raw-path rationale as [`Self::pick_open`].
    fn pick_save(&self) -> bool {
        let picked = rfd::FileDialog::new().set_title("Save As").save_file();
        self.run_picked("w", picked);
        true
    }

    /// Abort the half-typed command line, then run `<verb> <path>` for a picked
    /// file (nothing further on cancel). Shared by the open and save pickers.
    fn run_picked(&self, verb: &str, picked: Option<PathBuf>) {
        self.rpc.notify("nvim_input", vec![Value::from("<Esc>")]);
        if let Some(path) = picked {
            let path = path.to_string_lossy();
            self.rpc
                .notify("nvim_command", vec![Value::from(format!("{verb} {path}"))]);
        }
    }

    /// The absolute screen cell the pointer is currently over, or `None` before
    /// the renderer exists.
    fn pointer_cell(&self) -> Option<(u16, u16)> {
        let r = self.renderer.as_ref()?;
        Some(r.cell_at(self.cursor_px.0, self.cursor_px.1))
    }

    /// Fire one `nvim_input_mouse(button, action, modifier, grid=0, row, col)` —
    /// a mouse gesture at a global screen cell. The server owns the hit-test back
    /// to a window + buffer position; `grid` is always 0 (nxvim is single-grid).
    fn send_mouse(&self, button: &str, action: &str, col: u16, row: u16) {
        self.rpc.notify(
            "nvim_input_mouse",
            vec![
                Value::from(button),
                Value::from(action),
                Value::from(mouse::mouse_modifier(self.mods)),
                Value::from(0u64),
                Value::from(row as u64),
                Value::from(col as u64),
            ],
        );
    }

    /// Left button pressed. A client-owned overlay claims the click when the
    /// pointer is over it — the panel's `[X]` / content (close, select, activate)
    /// or a completion row (select / accept) — exactly like the TUI; otherwise it
    /// is a text-area press the server turns into focus-follows-click + a Visual
    /// anchor. Arms drag tracking either way (a stray drag the server no-ops).
    fn mouse_left_press(&mut self) {
        let Some((col, row)) = self.pointer_cell() else {
            return;
        };
        let Some(r) = self.renderer.as_ref() else {
            return;
        };
        let (cols, total_rows) = r.grid_size();
        self.mouse_down = true;
        self.last_drag_cell = Some((col, row));

        // 1. The bottom panel (`:messages`/`:ls`) swallows every click while open:
        // its `[X]` (or `q`) closes it, a content row selects that entry, and the
        // already-selected entry activates (`<CR>`) — the TUI's select-then-confirm.
        if let Some(panel) = self.view.panel.as_ref() {
            if let Some((brow, bcols)) = mouse::panel_close_button(cols, total_rows, panel.height) {
                if row == brow && bcols.contains(&col) {
                    self.rpc.notify("nvim_input", vec![Value::from("q")]);
                    return;
                }
            }
            if let Some((cx, cy, cw, ch)) =
                mouse::panel_content_rect(cols, total_rows, panel.height)
            {
                if mouse::within(col, row, cx, cy, cw, ch) {
                    let prow = row - cy; // row within the content area
                    if (prow as usize) < panel.lines.len() {
                        let sel_end = panel.cursor_row + panel.cursor_span.max(1);
                        if prow >= panel.cursor_row && prow < sel_end {
                            self.rpc.notify("nvim_input", vec![Value::from("<CR>")]);
                        } else {
                            self.rpc
                                .notify("nxvim_panel_click", vec![Value::from(prow as u64)]);
                        }
                    }
                }
            }
            return;
        }

        // 2. The completion popup: a click on a row selects it, and clicking the
        // already-selected row accepts it (<C-n> then <C-y>). A click off the popup
        // is swallowed (no text-area fallthrough), matching the TUI.
        if let Some(hit) = render::pmenu_hit(&self.view, cols) {
            let (ix, iy, iw, ih) = hit.item;
            if mouse::within(col, row, ix, iy, iw, ih) {
                if let Some(pmenu) = self.view.pmenu.as_ref() {
                    let idx = hit.start + (row - iy) as usize;
                    if idx < pmenu.items.len() {
                        if pmenu.selected == Some(idx) {
                            self.rpc.notify("nxvim_complete_accept", vec![]);
                        } else {
                            self.rpc
                                .notify("nxvim_complete_select", vec![Value::from(idx as u64)]);
                        }
                    }
                }
            }
            return;
        }

        // 3. No overlay: a text-area press, forwarded to the server (single-grid).
        self.send_mouse("left", "press", col, row);
        // A press-and-hold already in the edge band auto-scrolls without a drag.
        self.arm_autoscroll((col, row));
    }

    /// Arm or disarm continuous drag auto-scroll for a press/drag now at `cell`:
    /// armed (with a fresh repeat deadline) when the cell sits in the top/bottom
    /// edge band, cleared otherwise. [`Self::about_to_wait`] then re-issues the
    /// drag every [`AUTOSCROLL_INTERVAL`] while it stays armed, so the buffer keeps
    /// scrolling even though winit reports a drag only on actual pointer motion.
    fn arm_autoscroll(&mut self, cell: (u16, u16)) {
        let rows = self.renderer.as_ref().map_or(0, |r| r.grid_size().1);
        if in_scroll_zone(cell.1, rows) {
            self.autoscroll = Some(cell);
            self.autoscroll_deadline = Some(Instant::now() + AUTOSCROLL_INTERVAL);
        } else {
            self.autoscroll = None;
            self.autoscroll_deadline = None;
        }
    }

    /// A `CursorMoved` while the left button is held: extend the Visual selection
    /// by reporting a drag, but only when the pointer crosses into a new cell (the
    /// server works in cells, so within-cell motion is noise).
    fn mouse_drag(&mut self) {
        if !self.mouse_down {
            return;
        }
        let Some(cell) = self.pointer_cell() else {
            return;
        };
        if self.last_drag_cell == Some(cell) {
            return;
        }
        self.last_drag_cell = Some(cell);
        self.send_mouse("left", "drag", cell.0, cell.1);
        // (Re)arm continuous auto-scroll when the drag is parked in the edge band;
        // disarm once it moves back into the body.
        self.arm_autoscroll(cell);
    }

    /// Left button released: finalize the Visual selection (the server keeps it)
    /// and end drag tracking. Forwarded unconditionally like the TUI — the server
    /// no-ops it unless a text press set an anchor.
    fn mouse_left_release(&mut self) {
        self.mouse_down = false;
        self.last_drag_cell = None;
        self.autoscroll = None; // release ends the drag, so stop scrolling
        self.autoscroll_deadline = None;
        if let Some((col, row)) = self.pointer_cell() {
            self.send_mouse("left", "release", col, row);
        }
    }

    /// The mouse wheel. A client-owned overlay under the pointer claims *vertical*
    /// notches — the completion doc preview scrolls its docs client-side, the popup
    /// list moves its selection, the message panel moves its cursor — mirroring the
    /// TUI. Everything else (all horizontal notches, and any vertical notch not over
    /// an overlay) is a text-area scroll the server hit-tests to the window under
    /// the pointer. A trackpad's fractional pixels accumulate into whole lines.
    fn mouse_wheel(&mut self, delta: MouseScrollDelta) {
        let Some(r) = self.renderer.as_ref() else {
            return;
        };
        let (cell_w, cell_h) = r.cell_size();
        let (ax, ay) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (x, y),
            MouseScrollDelta::PixelDelta(p) => (p.x as f32 / cell_w, p.y as f32 / cell_h),
        };
        let hnotch = mouse::drain_notches(ax, &mut self.wheel_accum.0);
        let vnotch = mouse::drain_notches(ay, &mut self.wheel_accum.1);
        let Some((col, row)) = self.pointer_cell() else {
            return;
        };
        let (cols, total_rows) = r.grid_size();

        // Cap the per-event repeat so a flung trackpad can't flood the server.
        const MAX_STEPS: i32 = 10;
        if let Some(action) = mouse::vertical_action(vnotch) {
            let down = vnotch < 0;
            let steps = vnotch.unsigned_abs().min(MAX_STEPS as u32);
            if !self.wheel_vertical_overlay(col, row, cols, total_rows, down, steps) {
                for _ in 0..steps {
                    self.send_mouse("wheel", action, col, row);
                }
            }
        }
        // Horizontal notches never hit an overlay — always a text-area scroll.
        if let Some(action) = mouse::horizontal_action(hnotch) {
            let steps = hnotch.unsigned_abs().min(MAX_STEPS as u32);
            for _ in 0..steps {
                self.send_mouse("wheel", action, col, row);
            }
        }
    }

    /// Route `steps` vertical wheel notches to whichever overlay the pointer is
    /// over — the completion doc preview (client-side scroll), the popup list
    /// (move the selection), or the message panel (move its cursor). Returns
    /// `true` when an overlay claimed them; `false` to fall through to a text scroll.
    fn wheel_vertical_overlay(
        &mut self,
        col: u16,
        row: u16,
        cols: u16,
        total_rows: u16,
        down: bool,
        steps: u32,
    ) -> bool {
        // The completion popup's doc preview and item list.
        if let Some(hit) = render::pmenu_hit(&self.view, cols) {
            if let Some((dx, dy, dw, dh, max_scroll)) = hit.doc {
                if mouse::within(col, row, dx, dy, dw, dh) {
                    const STEP: u16 = 3;
                    for _ in 0..steps {
                        self.doc_scroll = if down {
                            (self.doc_scroll + STEP).min(max_scroll)
                        } else {
                            self.doc_scroll.saturating_sub(STEP)
                        };
                    }
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                    return true;
                }
            }
            let (ix, iy, iw, ih) = hit.item;
            if mouse::within(col, row, ix, iy, iw, ih) {
                if let Some(pmenu) = self.view.pmenu.as_ref() {
                    let n = pmenu.items.len();
                    if n > 0 {
                        // Move the selection one item per notch, non-wrapping (like a
                        // scrollbar); an unselected list lands on the first item.
                        let mut sel = pmenu.selected;
                        for _ in 0..steps {
                            sel = Some(match sel {
                                Some(i) if down => (i + 1).min(n - 1),
                                Some(i) => i.saturating_sub(1),
                                None => 0,
                            });
                        }
                        if let Some(idx) = sel {
                            self.rpc
                                .notify("nxvim_complete_select", vec![Value::from(idx as u64)]);
                        }
                    }
                }
                return true;
            }
        }
        // The bottom panel: the server owns its (word-wrapped) cursor, so feed the
        // navigation keys it already handles.
        if let Some(panel) = self.view.panel.as_ref() {
            if let Some((cx, cy, cw, ch)) =
                mouse::panel_content_rect(cols, total_rows, panel.height)
            {
                if mouse::within(col, row, cx, cy, cw, ch) {
                    let key = if down { "<Down>" } else { "<Up>" };
                    for _ in 0..steps {
                        self.rpc.notify("nvim_input", vec![Value::from(key)]);
                    }
                    return true;
                }
            }
        }
        false
    }
}

/// The base ex verb to run after the native **open** dialog, if `cmdline` is one
/// of the `…o` open commands — `:eo`→`e`, `:spo`→`sp`, `:vso`→`vs`, `:tabeo`→`tabe`,
/// `:newo`→`new`, `:vnewo`→`vnew` — or bare `:e`/`:edit`, an alias of `:eo`. `None`
/// for anything else (including bare `:sp`/`:vs`/`:tabe`, which keep their usual
/// no-argument behavior). Pure, so it is unit-tested in `tests/keys.rs`.
pub fn open_dialog_verb(cmdline: &str) -> Option<&'static str> {
    match cmdline.trim() {
        // Bare `:e`/`:edit` aliases `:eo`; `:eo` is the explicit form.
        "e" | "ed" | "edi" | "edit" | "eo" => Some("e"),
        "spo" => Some("sp"),
        "vso" => Some("vs"),
        "tabeo" => Some("tabe"),
        "newo" => Some("new"),
        "vnewo" => Some("vnew"),
        _ => None,
    }
}

/// If `cmdline` is a file-**opening** ex command carrying a path argument, the
/// `(base_verb, arg)` to act on: the canonical verb to re-run and the raw path tail.
/// Only the commands the server routes through `ex_edit` — where a *directory*
/// argument opens the in-window netrw listing — are matched: `:e`/`:edit`,
/// `:sp`/`:split`, `:vs`/`:vsplit`, and `:tabe`/`:tabedit`/`:tabnew`. Everything
/// else is left alone, so a directory argument to `:cd`, `:lcd`, `:w`, `:grep`,
/// `:set`, … is *not* mistaken for an open (the caller would otherwise wrongly pop a
/// file picker for `:cd somedir`). `None` for a non-open command or one with no
/// argument (the bare forms are [`open_dialog_verb`]'s job). The caller decides
/// whether `arg` is actually a directory; this is pure, so it is unit-tested in
/// `tests/keys.rs`.
pub fn open_path_command(cmdline: &str) -> Option<(&'static str, &str)> {
    let (cmd, arg) = cmdline.trim_start().split_once(char::is_whitespace)?;
    let arg = arg.trim();
    if arg.is_empty() {
        return None; // no argument → a bare command, not an open-with-path
    }
    // A trailing `!` (`:e! dir`) is a force flag, irrelevant to listing a directory.
    let base = match cmd.strip_suffix('!').unwrap_or(cmd) {
        "e" | "edit" => "e",
        "sp" | "spl" | "split" => "sp",
        "vs" | "vsp" | "vspl" | "vsplit" => "vs",
        "tabe" | "tabed" | "tabedit" | "tabnew" => "tabe",
        _ => return None,
    };
    Some((base, arg))
}

/// Whether `<CR>` over `cmdline` should pop the native **save** dialog: `:wo`
/// (save to a new file) always, or a bare `:w`/`:write` when the focused buffer is
/// `unnamed` (so a plain `:w` has no file to write to). `None`/false leaves the
/// command to run as typed. `:wo` is chosen to mirror the `…o` open family and
/// because it shadows no real ex-command (vim's `:wn` is `:wnext`). Pure, so it
/// is unit-tested in `tests/keys.rs`.
pub fn save_dialog_needed(cmdline: &str, unnamed: bool) -> bool {
    matches!(cmdline.trim(), "wo")
        || (unnamed && matches!(cmdline.trim(), "w" | "wr" | "wri" | "writ" | "write"))
}

/// What `<CR>` over a `:` command line should make the GUI do — the native-dialog
/// affordance, decided in one place (see [`dialog_action`]).
#[derive(Debug, PartialEq, Eq)]
pub enum DialogAction<'a> {
    /// Pop the **open** dialog, then run `<base> <picked>` (the `…o` family / bare `:e`).
    Open { base: &'static str },
    /// `cmdline` opens `arg` with `base`; if `arg` is a local **directory** the caller
    /// pops the open dialog anchored there, else the command runs as typed (`:e somedir`).
    OpenPath { base: &'static str, arg: &'a str },
    /// Pop the **save** dialog, then `:w <picked>` (`:wo` / a bare `:w` on an unnamed buffer).
    Save,
}

/// Decide what `<CR>` over a `:` command line should do in the GUI: pop a native file
/// dialog, or nothing (run the command as typed). Folds the three pure predicates
/// ([`open_dialog_verb`], [`open_path_command`], [`save_dialog_needed`]) into one
/// decision, in their established priority order.
///
/// Returns `None` in a **remote (daemon) session** (`remote == true`): the buffers live
/// on the *daemon's* fs, so a local native dialog would browse and write the wrong
/// machine — the command must run as typed and let the server handle it (the in-window
/// netrw listing for `:e <dir>`, `E32` for a nameless `:w`, …). The `OpenPath`
/// directory-vs-file test is the caller's (it touches the local fs), keeping this pure
/// and unit-tested in `tests/keys.rs`.
pub fn dialog_action(cmdline: &str, unnamed: bool, remote: bool) -> Option<DialogAction<'_>> {
    if remote {
        return None;
    }
    if let Some(base) = open_dialog_verb(cmdline) {
        Some(DialogAction::Open { base })
    } else if let Some((base, arg)) = open_path_command(cmdline) {
        Some(DialogAction::OpenPath { base, arg })
    } else if save_dialog_needed(cmdline, unnamed) {
        Some(DialogAction::Save)
    } else {
        None
    }
}

/// Parse a vim / Neovide `guifont` value into `(family, point size)`. The family is
/// the first of the comma-separated fonts (the font system handles fallback on its
/// own); a backslash-escaped space (`Fira\ Code`, the `:set` form) is unescaped. A
/// `:h<n>` field sets the size; other `:` options (`:w`, `:b`, `:i`, `:#e-…`) are
/// accepted but ignored. Either component is `None` when absent, so the caller can
/// fall back to its configured default. Pure, so it's unit-tested in `tests/keys.rs`.
pub fn parse_guifont(guifont: &str) -> (Option<String>, Option<f32>) {
    let mut parts = guifont.split(':');
    let family = parts
        .next()
        .unwrap_or("")
        .split(',')
        .next()
        .unwrap_or("")
        .replace("\\ ", " ");
    let family = family.trim();
    let family = (!family.is_empty()).then(|| family.to_string());

    let size = parts.find_map(|opt| {
        let pt = opt.strip_prefix('h')?.trim().parse::<f32>().ok()?;
        (pt.is_finite() && pt > 0.0).then_some(pt)
    });
    (family, size)
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return; // already initialized (e.g. resumed after suspend)
        }
        let attrs = Window::default_attributes()
            .with_title("nxvim")
            .with_inner_size(winit::dpi::LogicalSize::new(960.0, 640.0));
        // On Wayland/X11 the window's app_id / WM_CLASS must match the
        // installed .desktop file's basename (its `StartupWMClass`) for the
        // desktop environment to associate our icon — packaged in the AppImage
        // as `assets/nxvim.desktop` — with the window. Without it the AppImage
        // ships an icon the compositor never attaches to the running window.
        #[cfg(all(
            unix,
            not(any(target_os = "macos", target_os = "ios", target_os = "android"))
        ))]
        let attrs = {
            use winit::platform::wayland::WindowAttributesExtWayland as _;
            attrs.with_name("nxvim", "nxvim")
        };
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("nxvim-gui: failed to create window: {e}");
                event_loop.exit();
                return;
            }
        };
        match Renderer::new(window.clone(), &self.config) {
            Ok(r) => self.renderer = Some(r),
            Err(e) => {
                eprintln!("nxvim-gui: failed to init renderer: {e}");
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);
        self.report_size(true);
        // Apply any `guifont` already received before the renderer existed (a
        // redraw can beat `resumed`); a no-op for the default empty value.
        self.apply_guifont();
        // A directory given on the command line opens the native file picker there
        // (taken so it fires once — `resumed` early-returns on later wake-ups).
        if let Some(dir) = self.open_dir.take() {
            self.pick_open_at("e", &dir.to_string_lossy());
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Redraw(view) => {
                // The previewed completion docs changing (a new selection, or the
                // menu closing) resets the client-side doc scroll to the top.
                let prev_doc = self.view.pmenu.as_ref().map(|p| p.doc.clone());
                self.view = *view;
                if self.view.pmenu.as_ref().map(|p| &p.doc) != prev_doc.as_ref() {
                    self.doc_scroll = 0;
                }
                // A changed `guifont` re-shapes the renderer (and re-reports the
                // grid) before this frame paints.
                if self.view.guifont != self.applied_guifont {
                    self.apply_guifont();
                }
                // (Re)arm or clear the scroll slide from this frame's gesture.
                self.scroll = arm_scroll(&self.view, self.scroll.take());
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            // A `:connect` brought up a new server: swap to its RPC handle (dropping the
            // old one — the App's last clone, which lets the old connection wind down),
            // update remote-ness, and re-attach the UI. Clearing `reported` forces
            // `report_size` to send a fresh `nvim_ui_attach`; resetting the view avoids
            // painting the old server's buffer until the new one's first redraw arrives.
            UserEvent::Connected { rpc, remote } => {
                self.rpc = *rpc;
                self.remote = remote;
                self.view = View::default();
                self.scroll = None;
                self.reported = (0, 0);
                self.report_size(true);
                self.apply_guifont();
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            UserEvent::Exit => event_loop.exit(),
        }
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            // Don't exit on the close button directly — that would discard a
            // modified buffer silently. Route through the server's `:qa`, which
            // quits a clean editor (it then sends `nxvim_exit`, arriving as
            // `UserEvent::Exit`) but refuses with `E37` when a buffer is unsaved,
            // keeping the window open. `nvim_command` runs the ex-command
            // regardless of the current mode (unlike injecting `:qa<CR>` keys).
            WindowEvent::CloseRequested => {
                self.rpc.notify("nvim_command", vec![Value::from("qa")]);
            }
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width, size.height);
                }
                self.report_size(false);
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                // The renderer measures cells in device pixels; a DPI change is
                // surfaced as a following `Resized`, which re-reports the grid.
            }
            WindowEvent::ModifiersChanged(mods) => self.mods = mods.state(),
            // Track the pointer (winit's button/wheel events carry no position) and,
            // while the left button is held, synthesize a drag when the cell changes.
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_px = (position.x, position.y);
                self.mouse_drag();
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => match state {
                ElementState::Pressed => self.mouse_left_press(),
                ElementState::Released => self.mouse_left_release(),
            },
            // Right / middle clicks have no client-owned overlay affordance, so
            // they forward straight to the server at the pointer cell — the
            // `'mousemodel'` right-click branch and middle-click paste live there.
            // Only the press is meaningful (the server no-ops right/middle drag +
            // release), mirroring the TUI.
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button,
                ..
            } => {
                if let (Some(name), Some((col, row))) =
                    (mouse::button_name(button), self.pointer_cell())
                {
                    self.send_mouse(name, "press", col, row);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => self.mouse_wheel(delta),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                // Paste from the system clipboard (Cmd+V / Ctrl+Shift+V /
                // Shift+Insert), fed through the same `encode_paste` the TUI uses
                // for terminal bracketed paste. Claimed before key encoding so the
                // gesture's base key isn't also typed.
                if input::is_paste(&event.logical_key, self.mods) {
                    self.paste_clipboard();
                    self.flush_deadline = Some(Instant::now() + TIMEOUT_LEN);
                    return;
                }
                // `<CR>` over a dialog-triggering `:` command line pops a native
                // file dialog and runs the real command with the chosen path, in
                // place of the keystroke. Only for a true `:` ex command (not a
                // search or a `vim.ui.input` prompt). A `pick_*` returns `true` when
                // it handled the key (so it is swallowed); otherwise the keystroke
                // is encoded as usual.
                if matches!(event.logical_key, Key::Named(NamedKey::Enter))
                    && self.view.command_mode
                    && self.view.cmdline_prefix == ':'
                    && self.view.cmdline_prompt.is_empty()
                {
                    // `:connect [user@]host[:port][/file]` / `:connect nxvim://…`
                    // switches this window to an edit-host (daemon) session. It's a
                    // client affordance (the server knows nothing of `:connect`), so
                    // handle it here: dismiss the command line on the current server and
                    // ask the IO thread to bring the new session up (see
                    // `UserEvent::Connected`).
                    if let Some(target) = remote::connect_command(&self.view.cmdline) {
                        self.rpc.notify("nvim_input", vec![Value::from("<Esc>")]);
                        let _ = self.reconnect.send(target);
                        return;
                    }
                    let unnamed = self.view.focused().is_some_and(|w| w.unnamed);
                    // `dialog_action` returns `None` in a remote (daemon) session, so
                    // the native local-fs picker never fires for remote buffers — the
                    // command runs as typed and the server/daemon handles it.
                    // Each `pick_*` swallows the `<CR>` (it aborts the command line and
                    // re-issues the real command with the chosen path), so `return`
                    // before the key is encoded below.
                    match dialog_action(&self.view.cmdline, unnamed, self.remote) {
                        Some(DialogAction::Open { base }) => {
                            self.pick_open(base);
                            return;
                        }
                        // Opening a *directory* (`:e somedir`): pop the picker there
                        // instead of letting the server show its netrw listing. A file
                        // argument runs as typed (the server opens it).
                        Some(DialogAction::OpenPath { base, arg }) if Path::new(arg).is_dir() => {
                            self.pick_open_at(base, arg);
                            return;
                        }
                        Some(DialogAction::Save) => {
                            self.pick_save();
                            return;
                        }
                        _ => {}
                    }
                }
                // macOS composes Option(Alt)+key into a character — Option+c yields
                // `logical_key` "ç", not "c" — so a `<A-c>`-style binding (e.g.
                // multi-cursor) would otherwise arrive as `<A-ç>` and never match.
                // For a ctrl/alt combo take `key_without_modifiers`, the un-composed
                // base key (it also drops Shift, fine for a chord like `<A-c>`); plain
                // typing keeps `logical_key`, where the platform has folded Shift into
                // the character so `A` stays `A`.
                let key = if self.mods.control_key() || self.mods.alt_key() {
                    event.key_without_modifiers()
                } else {
                    event.logical_key.clone()
                };
                if let Some(notation) = input::encode_key(&key, self.mods) {
                    self.rpc
                        .notify("nvim_input", vec![Value::from(notation.as_str())]);
                    // Arm the `timeoutlen` flush; the next key re-arms it, so it
                    // measures idle-since-last-key (see `about_to_wait`).
                    self.flush_deadline = Some(Instant::now() + TIMEOUT_LEN);
                }
            }
            WindowEvent::RedrawRequested => {
                // Settle a finished slide before painting, so the final frame is
                // the live (destination) view rather than a clamped band.
                if self.scroll.as_ref().is_some_and(ScrollAnim::done) {
                    self.scroll = None;
                }
                let frame = self.scroll.as_ref().map(ScrollAnim::frame);
                let doc_scroll = self.doc_scroll;
                if let Some(r) = self.renderer.as_mut() {
                    if let Err(e) = r.render(&self.view, frame.as_ref(), doc_scroll) {
                        eprintln!("nxvim-gui: render error: {e}");
                    }
                }
            }
            _ => {}
        }
    }

    /// Between event batches, drive the slide and the `timeoutlen` flush. While a
    /// slide is active, wake on a short timer and request a frame; `present` (Fifo
    /// vsync) then paces the actual paint to the display refresh. `WaitUntil` (not
    /// `Poll`) means we don't busy-spin if the OS withholds `RedrawRequested` (e.g.
    /// an occluded or off-screen window) — and the time-based `done` check here is
    /// the backstop that clears the slide even if no frame ever paints. When no
    /// slide is running but a flush is armed, wake at the flush deadline instead.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Idle `timeoutlen` reached: nudge the server to resolve any withheld
        // mapped prefix (harmless when nothing is pending), and disarm.
        if self.flush_deadline.is_some_and(|d| Instant::now() >= d) {
            self.rpc.notify("nxvim_input_flush", vec![]);
            self.flush_deadline = None;
        }
        // Continuous mouse drag-scroll: re-issue the held drag at its edge cell each
        // interval so the buffer keeps scrolling while the pointer is held still.
        // The server scrolls the focused window one line per drag it lands past the
        // text body and re-extends the selection.
        if let (Some((col, row)), Some(deadline)) = (self.autoscroll, self.autoscroll_deadline) {
            if Instant::now() >= deadline {
                self.send_mouse("left", "drag", col, row);
                self.autoscroll_deadline = Some(Instant::now() + AUTOSCROLL_INTERVAL);
            }
        }
        if self.scroll.as_ref().is_some_and(ScrollAnim::done) {
            self.scroll = None;
            if let Some(w) = self.window.as_ref() {
                w.request_redraw(); // settle to the live view
            }
        }
        // The soonest pending timer wakeup (auto-scroll repeat, then the flush).
        let timer = match (self.autoscroll_deadline, self.flush_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if self.scroll.is_some() {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(8),
            ));
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        } else if let Some(deadline) = timer {
            // No slide, but a timer is pending: sleep exactly until it's due.
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}
