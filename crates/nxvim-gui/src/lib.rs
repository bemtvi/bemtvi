//! The native (winit + wgpu) GUI client.
//!
//! A thin RPC client that owns no editor state — the GUI sibling of `nxvim-tui`.
//! It attaches to the server, sends keystrokes as vim key-notation
//! (`nx_input`), and paints the server's [`View`] (the same `redraw` model the
//! TUI consumes) onto a GPU surface as a monospace cell grid.
//!
//! **Threading.** winit owns the main thread (its event loop is not async), so
//! the RPC lives on a separate IO thread running a current-thread tokio runtime:
//! it drives [`nxvim_rpc::connect`], decodes each `redraw` into a [`View`], and
//! forwards it to the event loop as a [`UserEvent`] via an
//! [`winit::event_loop::EventLoopProxy`]. Input flows the other way without a
//! runtime — [`nxvim_rpc::Rpc`] is `Clone + Send` and its `notify` is synchronous,
//! so the winit thread fires `nx_input` / `nvim_ui_*` directly on a cloned
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

mod images;
mod input;
mod mouse;
pub mod remote;
mod render;
mod session;

pub use input::{altgr_composed, encode_key, is_paste};
pub use mouse::{
    button_name, cell_at, drain_notches, horizontal_action, mouse_modifier, vertical_action,
};
// The neutral text-insertion encoder, re-exported so the Tier-1 key test can cover
// the path IME-committed text takes (composed accents, CJK) — the GUI feeds an
// `Ime::Commit` through this exactly as it does a clipboard paste.
pub use nxvim_view::encode_paste;
pub use session::{
    parse_connect_uri, spawn_session, spawn_stdio_daemon_session, spawn_workspace_session, Session,
};
// The pure inline-inlay-hint geometry (the shift math) and the segment splice, so
// the Tier-1 `inlay` test can exercise them without a GPU — like the mouse helpers.
pub use render::{inlay_shift, splice_inlay, Seg, DEFAULT_INLAY};
// The pure per-row syntax-coloring layer (run splitting + the no-colorscheme group
// fallback), exported so the Tier-1 `syntax` test can exercise it without a GPU.
pub use render::{col_to_screen, group_fallback, rect_subtract, row_segments, text_run_origin};
// The wide-glyph mask (replace an off-grid emoji cluster with cell-width spaces), so
// the Tier-1 `wide` test can exercise it without shaping / a GPU.
pub use render::mask_segments;
// The pure caret-cell math for the command line and the picker prompt (char-offset
// wire fields → display-width cells), exported for the Tier-1 `caret` test.
pub use render::{cmdline_caret_col, query_caret_col};
// The sRGB→linear color conversions feeding the quad pipeline, exported so the Tier-1
// `color` test can pin the channel order without a GPU.
pub use render::{color_to_rgba, srgb_to_color, srgb_to_color_rgba};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_view::{
    DiagSign, DiagSpan, HlSpan, InlayHint, ResizeCursor, ScrollData, Style, View, VirtChunk,
    VirtPlacement,
};
use rmpv::Value;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{CursorIcon, Window, WindowId};

use render::{Renderer, ScrollFrame};

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
    /// Ordered font families: the first is the primary, the rest a wezterm-style
    /// fallback chain tried in turn for a glyph the primary lacks. Empty uses the
    /// system monospace. Set from a comma-separated `--font` / `NXVIM_GUI_FONT`.
    pub fonts: Vec<String>,
    /// Font point size, before the display's scale factor is applied.
    pub font_size: f32,
    /// Render scale for an emoji / wide fallback glyph relative to the text cell — a
    /// color-emoji font draws smaller than its reserved cells, so this sizes it up.
    /// Set from `--emoji-scale` / `NXVIM_GUI_EMOJI_SCALE`.
    pub emoji_scale: f32,
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            fonts: Vec::new(),
            font_size: 15.0,
            emoji_scale: 1.2,
        }
    }
}

impl GuiConfig {
    /// Overrides from the environment: `NXVIM_GUI_FONT` (family, or a comma-separated
    /// fallback list) and `NXVIM_GUI_FONT_SIZE` (points). Absent/blank/invalid values
    /// keep the default; CLI flags layered on top take precedence (see `main`).
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
        if let Ok(scale) = std::env::var("NXVIM_GUI_EMOJI_SCALE") {
            if let Ok(s) = scale.trim().parse::<f32>() {
                c.set_emoji_scale(s);
            }
        }
        c
    }

    /// Set the font family list from a comma-separated spec (`"JetBrains Mono,Noto
    /// Color Emoji"`), like a `guifont` value: each family is trimmed and its
    /// backslash-escaped spaces unescaped, and blanks are dropped. An all-blank spec
    /// leaves the list untouched (keeping the monospace default rather than asking
    /// the font system for `""`).
    pub fn set_font(&mut self, spec: &str) {
        let fonts = parse_font_list(spec);
        if !fonts.is_empty() {
            self.fonts = fonts;
        }
    }

    /// Set the point size, clamped to `[4, 200]` so a typo can't produce a zero,
    /// negative, or absurd cell. A non-finite or non-positive value is ignored.
    pub fn set_font_size(&mut self, pt: f32) {
        if pt.is_finite() && pt > 0.0 {
            self.font_size = pt.clamp(4.0, 200.0);
        }
    }

    /// Set the emoji render scale, clamped to `[0.25, 4.0]` so a typo can't blow a
    /// glyph up across the screen or vanish it. A non-finite or non-positive value is
    /// ignored (keeps the current scale).
    pub fn set_emoji_scale(&mut self, scale: f32) {
        if scale.is_finite() && scale > 0.0 {
            self.emoji_scale = scale.clamp(0.25, 4.0);
        }
    }
}

/// A request to fetch a remote (daemon-session) image preview's bytes over the editor
/// RPC. The image store (render thread) emits one when it sees a `remote` preview it
/// can't read off local disk; the IO thread fulfils it with `nxvim_image_read` and
/// posts the bytes back as [`UserEvent::ImageBytes`].
pub(crate) struct ImageFetch {
    pub path: String,
    /// The preview's on-disk version `(size, mtime_ms)`, echoed back so a stale reply
    /// for a superseded version is dropped rather than replacing newer bytes.
    pub version: (u64, u64),
}

/// A request from the App to bring up a **new** session, swapping the live one. Built off
/// the UI thread by the session loop (see [`run`]) and reported back via
/// [`UserEvent::Connected`] on success, or `:echoerr` on failure.
pub enum SessionRequest {
    /// `:connect …` — switch to an edit-host (daemon/QUIC) session.
    Connect(remote::ConnectTarget),
    /// `:workspace <dir>` — switch to a local workspace session rooted at the directory.
    Workspace(PathBuf),
}

/// Events the IO thread injects into the winit event loop.
pub enum UserEvent {
    /// A decoded `redraw`: replace the view and repaint.
    Redraw(Box<View>),
    /// An `nxvim_image_read` reply: a remote preview's fetched bytes (or a read error),
    /// to hand to the image store and repaint. Carries the version the fetch was for.
    ImageBytes {
        path: String,
        version: (u64, u64),
        result: Result<Vec<u8>, String>,
    },
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
/// point at a daemon (see [`session::spawn_session`]), and `:workspace [dir]` swaps it
/// onto a fresh **local** session rooted at a directory (its own shada namespace + saved
/// layout — see [`session::spawn_workspace_session`]). Either way the IO thread builds the
/// new session off the UI thread, then the session loop retires the old server (it winds
/// down on EOF) and re-attaches the UI onto the new one. The editor always runs local —
/// only the fs/process/watch/LSP seams cross the wire.
pub fn run(
    initial: Session,
    config: GuiConfig,
    open_dir: Option<PathBuf>,
    config_source: nxvim_server::ConfigSource,
) -> Result<()> {
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
    // `:connect <target>` / `:workspace <dir>` from the App request a switch to a new
    // (daemon or local-workspace) session.
    let (reconnect_tx, mut reconnect_rx) = tokio::sync::mpsc::unbounded_channel::<SessionRequest>();
    // The image store (render thread) requests remote preview bytes here; the IO thread
    // fulfils each over `nxvim_image_read` on the *current* session and posts the bytes
    // back as `UserEvent::ImageBytes`.
    let (fetch_tx, mut fetch_rx) = tokio::sync::mpsc::unbounded_channel::<ImageFetch>();
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
            // Each carries the command label (`:connect` / `:workspace`) so a build
            // failure is reported against the command the user actually typed.
            let (built_tx, mut built_rx) =
                tokio::sync::mpsc::unbounded_channel::<(&'static str, Result<Session>)>();
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
                                // `nx.session.reconnect(spec)` from inside the VM (§B): a
                                // plugin initiates the reload. Parse the wire spec and build
                                // the new session off the UI thread (like `:connect`), then
                                // `built_rx` swaps onto it. A bad spec / failed build is
                                // reported and leaves the current session intact.
                                "nx_session_reconnect" => {
                                    match params.first().ok_or_else(|| {
                                        anyhow::anyhow!("nx_session_reconnect: missing spec")
                                    }).and_then(nxvim_server::ReconnectSpec::from_value) {
                                        Ok(spec) => {
                                            let tx = built_tx.clone();
                                            tokio::task::spawn_blocking(move || {
                                                let built = session::spawn_session_from_spec(spec);
                                                let _ = tx.send((":reconnect", built));
                                            });
                                        }
                                        Err(err) => report_session_error(&rpc, ":reconnect", &err),
                                    }
                                }
                                // `:connect <url>` with no matching connect-provider (§C): the
                                // VM hands the raw URL back for the GUI's built-in direct dial.
                                // Parse it into an ssh/quic target and build off the UI thread
                                // (like `SessionRequest::Connect` — the ssh handshake and its
                                // askpass dialog can take seconds); the swap arrives on
                                // `built_rx`. An unparseable URL is reported, not swallowed.
                                "nx_connect_fallback" => {
                                    match params.first().and_then(Value::as_str) {
                                        Some(url) => match remote::connect_target(url) {
                                            Some(target) => {
                                                let tx = built_tx.clone();
                                                tokio::task::spawn_blocking(move || {
                                                    let file = target.embedded_file();
                                                    let built = session::spawn_session(
                                                        Some(target),
                                                        file,
                                                        config_source,
                                                    );
                                                    let _ = tx.send((":connect", built));
                                                });
                                            }
                                            None => report_session_error(
                                                &rpc,
                                                ":connect",
                                                &anyhow::anyhow!(
                                                    "not a connect target: {url:?} (expected nxvim://… or [user@]host[:port][/file])"
                                                ),
                                            ),
                                        },
                                        None => report_session_error(
                                            &rpc,
                                            ":connect",
                                            &anyhow::anyhow!("nx_connect_fallback: missing url"),
                                        ),
                                    }
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
                            Some(req) => {
                                let tx = built_tx.clone();
                                // A live `:connect`/`:workspace` build runs off the UI
                                // thread (the ssh/quic handshake — and its askpass dialog —
                                // can take seconds), so the current session keeps rendering
                                // meanwhile. A `:connect` inherits the session-wide config
                                // source chosen at startup (`--remote-config`), so every
                                // reconnect is consistent with the first.
                                tokio::task::spawn_blocking(move || {
                                    let built = match req {
                                        SessionRequest::Connect(target) => {
                                            let file = target.embedded_file();
                                            (
                                                ":connect",
                                                session::spawn_session(
                                                    Some(target),
                                                    file,
                                                    config_source,
                                                ),
                                            )
                                        }
                                        SessionRequest::Workspace(dir) => (
                                            ":workspace",
                                            session::spawn_workspace_session(dir, config_source),
                                        ),
                                    };
                                    let _ = tx.send(built);
                                });
                            }
                            None => break 'session, // App (the only sender) is gone
                        },
                        // A `:connect`/`:workspace` build finished. On success, restart the
                        // session on it; on failure, keep the current session and report why.
                        built = built_rx.recv() => match built {
                            Some((_, Ok(session))) => break session,
                            Some((label, Err(err))) => report_session_error(&rpc, label, &err),
                            None => {} // unreachable: `built_tx` is held above
                        },
                        // The render thread needs a remote preview's bytes. Fetch them
                        // over `nxvim_image_read` on the *current* session (a spawned
                        // task, so a slow daemon read doesn't stall redraws) and post the
                        // reply back to the UI thread, which hands it to the image store.
                        // `None` (App, the only sender, is gone) just falls through —
                        // teardown is handled by the other arms.
                        fetch = fetch_rx.recv() => if let Some(ImageFetch { path, version }) = fetch {
                            let rpc = rpc.clone();
                            let proxy = proxy.clone();
                            tokio::spawn(async move {
                                let result = match rpc
                                    .request("nxvim_image_read", vec![Value::from(path.as_str())])
                                    .await
                                {
                                    Ok(Value::Binary(bytes)) => Ok(bytes),
                                    Ok(other) => {
                                        Err(format!("nxvim_image_read: unexpected reply {other:?}"))
                                    }
                                    Err(e) => Err(e.to_string()),
                                };
                                let _ = proxy.send_event(UserEvent::ImageBytes {
                                    path,
                                    version,
                                    result,
                                });
                            });
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
    let mut app = App::new(
        rpc,
        config,
        open_dir,
        initial_remote,
        reconnect_tx,
        fetch_tx,
    );
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

/// Report a session-switch failure in the GUI message line via the *current* session —
/// the new one never came up. `label` is the command that failed (`:connect` — bad host,
/// refused auth, a malformed `nxvim://` URI; or `:workspace` — a missing / non-directory
/// path). nxvim is its own editor and has no vimscript `:echohl`; `:echoerr` already
/// renders the text as an error message. The error chain is flattened to one line
/// (`echoerr` rejects newlines) and single quotes doubled (Vim string escaping) so a
/// hostname or path can't break the command.
fn report_session_error(rpc: &Rpc, label: &str, err: &anyhow::Error) {
    let line = format!("{label} failed: {err:#}").replace('\n', "; ");
    let escaped = line.replace('\'', "''");
    rpc.notify(
        "nx_command",
        vec![Value::from(format!("echoerr '{escaped}'"))],
    );
}

/// An in-flight scroll slide, driven by the client clock. Mirrors the TUI's
/// `Animation`, but the GUI keeps the offset fractional (no rounding) for
/// sub-pixel smoothness. The band is **screen-row based**: `lines`/the overlay
/// arrays are the over-scanned screen rows the slide reveals, and the slide is a
/// screen-row offset (`from_row` → `to_row`) into them, so interleaved
/// `virt_lines` slide with the text instead of snapping.
struct ScrollAnim {
    from_row: f32,
    to_row: f32,
    from_cursor_row: f32,
    to_cursor_row: f32,
    start: Instant,
    duration: Duration,
    lines: Vec<String>,
    selection: Vec<Option<(u16, u16)>>,
    /// Per band row, the secondary multi-cursors' selection spans, so they slide too.
    secondary_selection: Vec<Vec<(u16, u16)>>,
    /// Orientation of the sliding visual selection (see
    /// [`ScrollData::sel_extends_down`]); drives the selection edge clip.
    sel_extends_down: Option<bool>,
    numbers: Vec<Option<usize>>,
    /// Per band row, `true` on a soft-wrap continuation row, so the gutter blanks
    /// the wrapped rows while the slide animates.
    continuation: Vec<bool>,
    highlights: Vec<Vec<HlSpan>>,
    /// `hlsearch` / `incsearch` match spans for the band, so the search highlight
    /// slides with the text instead of vanishing until the slide settles.
    search: Vec<Vec<(u16, u16)>>,
    incsearch: Vec<Option<(u16, u16)>>,
    inlay_hints: Vec<Vec<InlayHint>>,
    /// Extmark `virt_text` placements for the band, so they slide with the line
    /// instead of flashing out and back when the slide settles.
    virt_text: Vec<Vec<VirtPlacement>>,
    /// Extmark `virt_lines` content per band row, so the interleaved virtual rows
    /// slide with the text instead of only appearing once the slide settles.
    virt_lines: Vec<Option<Vec<VirtChunk>>>,
    /// Diagnostic underline spans / sign-column glyphs per band row, so the
    /// squiggles and signs slide with the text instead of blanking for the slide.
    diagnostics: Vec<Vec<DiagSpan>>,
    diagnostics_signs: Vec<Option<DiagSign>>,
    styles: Vec<Style>,
}

impl ScrollAnim {
    fn new(s: &ScrollData) -> Self {
        Self {
            from_row: s.from_row,
            to_row: s.to_row,
            from_cursor_row: s.from_cursor_row,
            to_cursor_row: s.to_cursor_row,
            start: Instant::now(),
            duration: s.duration,
            lines: s.lines.clone(),
            selection: s.selection.clone(),
            secondary_selection: s.secondary_selection.clone(),
            sel_extends_down: s.sel_extends_down,
            numbers: s.numbers.clone(),
            continuation: s.continuation.clone(),
            highlights: s.highlights.clone(),
            search: s.search.clone(),
            incsearch: s.incsearch.clone(),
            inlay_hints: s.inlay_hints.clone(),
            virt_text: s.virt_text.clone(),
            virt_lines: s.virt_lines.clone(),
            diagnostics: s.diagnostics.clone(),
            diagnostics_signs: s.diagnostics_signs.clone(),
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
            row_off: lerp(self.from_row, self.to_row),
            cursor_row: lerp(self.from_cursor_row, self.to_cursor_row),
            lines: &self.lines,
            selection: &self.selection,
            secondary_selection: &self.secondary_selection,
            // The selection's moving edge tracks the interpolated cursor; the clip
            // side follows the selection's orientation (anchor above ⇒ down), not
            // the scroll direction, so it grows *and* shrinks smoothly either way.
            sel_clip: self.sel_extends_down,
            numbers: &self.numbers,
            continuation: &self.continuation,
            highlights: &self.highlights,
            search: &self.search,
            incsearch: &self.incsearch,
            inlay_hints: &self.inlay_hints,
            virt_text: &self.virt_text,
            virt_lines: &self.virt_lines,
            diagnostics: &self.diagnostics,
            diagnostics_signs: &self.diagnostics_signs,
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
    // The destination viewport top / cursor buffer lines are read off the band at
    // its settle offsets: `numbers` carries each band row's 1-based buffer line.
    let dest_top = anim
        .numbers
        .get(anim.to_row.round() as usize)
        .copied()
        .flatten();
    let dest_cursor = anim
        .numbers
        .get(anim.to_cursor_row.round() as usize)
        .copied()
        .flatten();
    let Some(win) = view.focused() else {
        return false;
    };
    win.numbers.first().copied().flatten() == dest_top && Some(win.cursor_line) == dest_cursor
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
    /// The resize cursor currently shown (over a draggable separator / dock edge),
    /// or `None` for the default arrow. Tracked so a hover only calls winit's
    /// `set_cursor` when the shape actually changes, not on every pointer move.
    resize_hint: Option<ResizeCursor>,
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
    /// Last `(cols, windows_rows)` reported to the server, to suppress
    /// no-op resize notifications.
    reported: (u16, u16),
    /// When set, fire one `nxvim_input_flush` once this instant passes with no
    /// further input — the GUI's `timeoutlen` timer (see [`TIMEOUT_LEN`]). Armed by
    /// each keystroke and re-armed by the next, so it measures idle-since-last-key.
    flush_deadline: Option<Instant>,
    /// The last IME cursor rect `(x, y, w, h)` pushed to the window, so a frame that
    /// didn't move the caret skips the redundant platform call.
    ime_area: Option<(f32, f32, f32, f32)>,
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
    /// Requests a `:connect <target>` / `:workspace <dir>` switch to a new session. The IO
    /// thread builds the new session and feeds the swapped handle back as
    /// [`UserEvent::Connected`] (see [`run`]).
    reconnect: tokio::sync::mpsc::UnboundedSender<SessionRequest>,
    /// Handed to the renderer's image store so it can request remote preview bytes over
    /// the editor RPC (a daemon session's image files aren't on local disk). The IO
    /// thread drains it and posts replies back as [`UserEvent::ImageBytes`].
    fetch_tx: tokio::sync::mpsc::UnboundedSender<ImageFetch>,
}

impl App {
    fn new(
        rpc: Rpc,
        config: GuiConfig,
        open_dir: Option<PathBuf>,
        remote: bool,
        reconnect: tokio::sync::mpsc::UnboundedSender<SessionRequest>,
        fetch_tx: tokio::sync::mpsc::UnboundedSender<ImageFetch>,
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
            resize_hint: None,
            autoscroll: None,
            autoscroll_deadline: None,
            wheel_accum: (0.0, 0.0),
            reported: (0, 0),
            flush_deadline: None,
            ime_area: None,
            config,
            applied_guifont: String::new(),
            open_dir,
            remote,
            reconnect,
            fetch_tx,
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
            "nx_ui_attach"
        } else {
            "nx_ui_try_resize"
        };
        // A native window always has full key disambiguation (winit reports Ctrl+I as
        // `Character("i")` + control, distinct from `Tab`), so declare the keyboard
        // protocol active at attach — the server then keeps `<C-i>`/`<C-m>`/`<C-[>`/
        // `<C-h>` apart from `<Tab>`/`<CR>`/`<Esc>`/`<BS>`. The map is read only at
        // attach; a resize passes an empty one.
        let caps = if attach {
            Value::Map(vec![(
                Value::from("keyboard_protocol"),
                Value::Boolean(true),
            )])
        } else {
            Value::Map(vec![])
        };
        self.rpc
            .notify(method, vec![Value::from(cols), Value::from(win_rows), caps]);
    }

    /// Read the system clipboard and feed it to the server as one `nx_input` via
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
                .notify("nx_input", vec![Value::from(notation.as_str())]);
        }
    }

    /// Push the just-painted caret rect to the window as the IME cursor area, so the
    /// platform opens the IME candidate window at the caret instead of the window
    /// origin. The renderer reports the rect in physical pixels (`None` when a
    /// picker/panel owns the cursor); only a changed rect is forwarded, since the
    /// platform call is a no-op-but-not-free per frame.
    fn update_ime_area(&mut self) {
        let area = self.renderer.as_ref().and_then(Renderer::ime_cursor_area);
        if area == self.ime_area {
            return;
        }
        self.ime_area = area;
        if let (Some(w), Some((x, y, cw, ch))) = (self.window.as_ref(), area) {
            w.set_ime_cursor_area(
                winit::dpi::PhysicalPosition::new(x, y),
                winit::dpi::PhysicalSize::new(cw, ch),
            );
        }
    }

    /// Apply the relayed `view.guifont` to the renderer: parse the family and `:h`
    /// size, fall back to the CLI/env [`GuiConfig`] for any unset field, re-shape,
    /// and re-report the grid (the cell size changed). Called whenever `guifont`
    /// changes — including its first non-empty value from `init.lua` — so a
    /// `:set guifont=…` takes effect live. A no-op before the renderer exists; it's
    /// applied from `resumed` once the renderer is built.
    fn apply_guifont(&mut self) {
        let (families, size) = parse_guifont(&self.view.guifont);
        // The `guifont` families win; with none set, fall back to the CLI/env list.
        let fonts = if families.is_empty() {
            self.config.fonts.clone()
        } else {
            families
        };
        let size = size.unwrap_or(self.config.font_size);
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        renderer.set_font(&fonts, size);
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
        self.rpc.notify("nx_input", vec![Value::from("<Esc>")]);
        if let Some(path) = picked {
            let path = path.to_string_lossy();
            self.rpc
                .notify("nx_command", vec![Value::from(format!("{verb} {path}"))]);
        }
    }

    /// The absolute screen cell the pointer is currently over, or `None` before
    /// the renderer exists.
    fn pointer_cell(&self) -> Option<(u16, u16)> {
        let r = self.renderer.as_ref()?;
        Some(r.cell_at(self.cursor_px.0, self.cursor_px.1))
    }

    /// Arm the `timeoutlen` idle flush from the relayed `'timeout'`/`'timeoutlen'`:
    /// schedule a flush `timeoutlen` ms out, or disarm entirely under `notimeout`
    /// (the which-key "wait forever" behavior). Called after every keystroke / paste,
    /// so it always measures idle-since-the-last-key with the current config.
    fn arm_flush_deadline(&mut self) {
        self.flush_deadline = self
            .view
            .timeout
            .then(|| Instant::now() + Duration::from_millis(self.view.timeoutlen));
    }

    /// Fire one `nx_input_mouse(button, action, modifier, grid=0, row, col)` —
    /// a mouse gesture at a global screen cell. The server owns the hit-test back
    /// to a window + buffer position; `grid` is always 0 (nxvim is single-grid).
    fn send_mouse(&self, button: &str, action: &str, col: u16, row: u16) {
        self.rpc.notify(
            "nx_input_mouse",
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

    /// Left button pressed: forward the global cell to the server, which owns the
    /// hit-test back to a window + buffer position (focus-follows-click + a Visual
    /// anchor) or an overlay (the completion popup, a picker, …) under the pointer.
    /// Arms drag tracking either way (a stray drag the server no-ops).
    fn mouse_left_press(&mut self) {
        let Some((col, row)) = self.pointer_cell() else {
            return;
        };
        if self.renderer.is_none() {
            return;
        }
        self.mouse_down = true;
        self.last_drag_cell = Some((col, row));

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

    /// Update the pointer shape on hover: a resize cursor over a draggable split
    /// separator or dock edge, the default arrow elsewhere. A no-op while a button
    /// is held (a drag in progress keeps its grab cursor) and only touches winit
    /// when the shape changes, so an ordinary move over text costs nothing.
    fn update_resize_cursor(&mut self) {
        if self.mouse_down {
            return;
        }
        let hint = self
            .renderer
            .as_ref()
            .and_then(|r| r.resize_cursor_at(&self.view, self.cursor_px.0, self.cursor_px.1));
        if hint == self.resize_hint {
            return;
        }
        self.resize_hint = hint;
        if let Some(window) = self.window.as_ref() {
            let icon = match hint {
                Some(ResizeCursor::Col) => CursorIcon::EwResize,
                Some(ResizeCursor::Row) => CursorIcon::NsResize,
                None => CursorIcon::Default,
            };
            window.set_cursor(icon);
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

        // Cap the per-event repeat so a flung trackpad can't flood the server. Every
        // notch is forwarded to the server, which hit-tests it to the window — or the
        // overlay (the completion popup, a picker) — under the pointer.
        const MAX_STEPS: i32 = 10;
        if let Some(action) = mouse::vertical_action(vnotch) {
            let steps = vnotch.unsigned_abs().min(MAX_STEPS as u32);
            for _ in 0..steps {
                self.send_mouse("wheel", action, col, row);
            }
        }
        if let Some(action) = mouse::horizontal_action(hnotch) {
            let steps = hnotch.unsigned_abs().min(MAX_STEPS as u32);
            for _ in 0..steps {
                self.send_mouse("wheel", action, col, row);
            }
        }
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

/// Split a comma-separated font-family list (`"JetBrains Mono,Noto Color Emoji"`)
/// into individual families: each is trimmed and its backslash-escaped spaces
/// (`Fira\ Code`, the `:set guifont` form) unescaped, and blank entries are dropped.
fn parse_font_list(spec: &str) -> Vec<String> {
    spec.split(',')
        .map(|f| f.replace("\\ ", " ").trim().to_string())
        .filter(|f| !f.is_empty())
        .collect()
}

/// Parse a vim / Neovide `guifont` value into `(families, point size)`. The families
/// are the comma-separated list, tried in order as a wezterm-style fallback chain (a
/// backslash-escaped space, the `:set` form, is unescaped). A `:h<n>` field sets the
/// size; other `:` options (`:w`, `:b`, `:i`, `:#e-…`) are accepted but ignored. The
/// family list is empty / the size `None` when absent, so the caller can fall back to
/// its configured default. Pure, so it's unit-tested in `tests/keys.rs`.
pub fn parse_guifont(guifont: &str) -> (Vec<String>, Option<f32>) {
    let mut parts = guifont.split(':');
    let families = parse_font_list(parts.next().unwrap_or(""));

    let size = parts.find_map(|opt| {
        let pt = opt.strip_prefix('h')?.trim().parse::<f32>().ok()?;
        (pt.is_finite() && pt > 0.0).then_some(pt)
    });
    (families, size)
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
        match Renderer::new(window.clone(), &self.config, self.fetch_tx.clone()) {
            Ok(r) => self.renderer = Some(r),
            Err(e) => {
                eprintln!("nxvim-gui: failed to init renderer: {e}");
                event_loop.exit();
                return;
            }
        }
        // Enable IME so composed/non-ASCII text input works: dead-key accent
        // sequences (Option+e e → "é"), AltGr characters, and full IME composition
        // (CJK, …) reach us as `WindowEvent::Ime(Ime::Commit(..))`. On macOS this is
        // mandatory — without it dead-key sequences are never combined and the
        // composed character is dropped, so non-ASCII typing simply doesn't work.
        // Plain ASCII keys still arrive as `KeyboardInput` (winit only suppresses it
        // mid-preedit), so this doesn't double-input ordinary keys.
        window.set_ime_allowed(true);
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
                self.view = *view;
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
            // A remote preview's bytes arrived (or the read failed): hand them to the
            // image store and repaint, so the picture replaces its loading placeholder.
            UserEvent::ImageBytes {
                path,
                version,
                result,
            } => {
                if let Some(r) = self.renderer.as_mut() {
                    r.deliver_image(path, version, result);
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                }
            }
            // A `:connect` brought up a new server: swap to its RPC handle (dropping the
            // old one — the App's last clone, which lets the old connection wind down),
            // update remote-ness, and re-attach the UI. Clearing `reported` forces
            // `report_size` to send a fresh `nx_ui_attach`; resetting the view avoids
            // painting the old server's buffer until the new one's first redraw arrives.
            UserEvent::Connected { rpc, remote } => {
                self.rpc = *rpc;
                self.remote = remote;
                self.view = View::default();
                self.scroll = None;
                // The new session's files are unrelated to the old one's paths; drop
                // the cached textures and any fetched remote bytes so a stale image
                // can't bleed across the swap.
                if let Some(r) = self.renderer.as_mut() {
                    r.clear_images();
                }
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
            // keeping the window open. `nx_command` runs the ex-command
            // regardless of the current mode (unlike injecting `:qa<CR>` keys).
            WindowEvent::CloseRequested => {
                self.rpc.notify("nx_command", vec![Value::from("qa")]);
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
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // The renderer's cell metrics are device pixels (`points × scale`),
                // so a DPI change — dragging the window onto a different-scale
                // monitor — must re-derive them; the following `Resized` only
                // reconfigures the surface, so without this the glyphs keep the old
                // monitor's pixel size (wrong physical size + a wrong grid).
                // Re-applying the current font at the new scale re-measures the
                // cell, re-reports the grid, and repaints.
                if let Some(r) = self.renderer.as_mut() {
                    r.set_scale(scale_factor as f32);
                    self.apply_guifont();
                }
            }
            WindowEvent::ModifiersChanged(mods) => self.mods = mods.state(),
            // Track the pointer (winit's button/wheel events carry no position) and,
            // while the left button is held, synthesize a drag when the cell changes.
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_px = (position.x, position.y);
                self.mouse_drag();
                self.update_resize_cursor();
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
            // Composed text from the IME (dead-key accents, AltGr characters, CJK
            // composition, …) lands here as one commit rather than as `KeyboardInput`
            // — see `set_ime_allowed` in `resumed`. Feed it through the same
            // `encode_paste` the clipboard path uses (one notify, literal insertion,
            // `<` escaped) so the committed characters reach the buffer exactly as
            // typed. The empty `Preedit` winit sends right before each commit, and any
            // in-progress preedit, are ignored: macOS draws its own candidate window,
            // and the final text is what the commit carries.
            WindowEvent::Ime(Ime::Commit(text)) => {
                let notation = encode_paste(&text);
                if !notation.is_empty() {
                    self.rpc
                        .notify("nx_input", vec![Value::from(notation.as_str())]);
                    self.arm_flush_deadline();
                }
            }
            // `is_synthetic` presses are winit's focus-gain enumeration of keys
            // already held (X11/Windows) — state bookkeeping, not typing. Feeding
            // them through would inject a spurious keystroke every time the window
            // regains focus while a key is down (e.g. the `w` of an Alt-Tab still
            // held). Real key events arrive with `is_synthetic == false`.
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } if event.state == ElementState::Pressed => {
                // Paste from the system clipboard (Cmd+V / Ctrl+Shift+V /
                // Shift+Insert), fed through the same `encode_paste` the TUI uses
                // for terminal bracketed paste. Claimed before key encoding so the
                // gesture's base key isn't also typed.
                if input::is_paste(&event.logical_key, self.mods) {
                    self.paste_clipboard();
                    self.arm_flush_deadline();
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
                    // …but NOT while a completion picker (`nx.picker`, e.g. the
                    // cmdline file completer) floats over the still-open command
                    // line: there `<CR>` belongs to the picker so it can descend
                    // into a directory or paste the chosen path. Intercepting it
                    // here would pop the native dialog / submit instead, copying
                    // every directory into the line and running the last one (the
                    // GUI-only divergence from the TUI, where every key reaches the
                    // server's picker untouched).
                    && self.view.menu.is_none()
                {
                    // `:workspace [dir]` swaps this window onto a fresh local session rooted
                    // at the directory. It is a client affordance, *also* registered
                    // server-side as a no-op virtual command (see [`session::CLIENT_INIT_LUA`])
                    // so it gets completion, help, and history. So request the swap here, but
                    // DON'T swallow the `<CR>`: let it fall through and submit the command line
                    // normally, which records it in `:` history and runs the harmless no-op
                    // body. The swap then arrives async (see `UserEvent::Connected`).
                    //
                    // `:connect` is NOT intercepted here: it is a real prelude command (§C)
                    // that routes through the VM so a connector can claim it, then swaps via a
                    // `nx_session_reconnect` (provider) or `nx_connect_fallback` (direct dial)
                    // notification handled in [`crate::run`].
                    if let Some(dir) = remote::workspace_command(&self.view.cmdline) {
                        let _ = self
                            .reconnect
                            .send(SessionRequest::Workspace(PathBuf::from(dir)));
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
                //
                // Exception: Windows reports **AltGr** as Ctrl+Alt, so `AltGr+E` (€ on
                // European layouts) arrives as Ctrl+Alt+`Character("€")`. That's
                // *typing*, not a chord — sending `<C-A-e>` would swallow the € — so
                // when the layout composed the logical key into a different character
                // than the base ([`altgr_composed`]), pass the composed character
                // through with no modifier prefix. Skipped on macOS, where Option-only
                // composition is deliberately mapped to `<A-…>` chords (above).
                let altgr = !cfg!(target_os = "macos")
                    && input::altgr_composed(
                        &event.logical_key,
                        &event.key_without_modifiers(),
                        self.mods,
                    );
                let (key, mods) = if altgr {
                    (event.logical_key.clone(), ModifiersState::empty())
                } else if self.mods.control_key() || self.mods.alt_key() {
                    (event.key_without_modifiers(), self.mods)
                } else {
                    (event.logical_key.clone(), self.mods)
                };
                if let Some(notation) = input::encode_key(&key, mods) {
                    self.rpc
                        .notify("nx_input", vec![Value::from(notation.as_str())]);
                    // Arm the `timeoutlen` flush; the next key re-arms it, so it
                    // measures idle-since-last-key (see `about_to_wait`).
                    self.arm_flush_deadline();
                }
            }
            WindowEvent::RedrawRequested => {
                // Settle a finished slide before painting, so the final frame is
                // the live (destination) view rather than a clamped band.
                if self.scroll.as_ref().is_some_and(ScrollAnim::done) {
                    self.scroll = None;
                }
                let frame = self.scroll.as_ref().map(ScrollAnim::frame);
                if let Some(r) = self.renderer.as_mut() {
                    if let Err(e) = r.render(&self.view, frame.as_ref(), 0) {
                        eprintln!("nxvim-gui: render error: {e}");
                    }
                }
                self.update_ime_area();
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
