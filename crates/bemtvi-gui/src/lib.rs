//! The native (mitsuami + wgpu) GUI client.
//!
//! A thin RPC client that owns no editor state — the GUI sibling of `bemtvi-tui`.
//! It attaches to the server, sends keystrokes as vim key-notation
//! (`btv_input`), and paints the server's [`View`] (the same `redraw` model the
//! TUI consumes) onto a GPU surface as a monospace cell grid.
//!
//! **Windowing.** [mitsuami](https://mitsuami.roboport.com.br) shows the window with
//! the platform's own toolkit (AppKit, WinUI 3, GTK 4 or Kirigami); its whole content
//! is a `GpuSurface`, a native surface the wgpu renderer presents to. The surface
//! reports keys (where they are, what they typed through the platform's text input,
//! input methods' commits) and the pointer, and takes the cursor and the caret the
//! input method's candidate window opens by.
//!
//! **Threading.** mitsuami owns the main thread (its UI thread runs the platform's
//! loop), so the RPC lives on a separate IO thread running a current-thread tokio
//! runtime: it drives [`bemtvi_rpc::connect`], decodes each `redraw` into a [`View`],
//! and sends it to the UI thread as a [`UserEvent`] over a channel, which a UI-thread
//! task drains (mitsuami's executor wakes the UI thread for it). Input flows the other
//! way without a runtime — [`bemtvi_rpc::Rpc`] is `Clone + Send` and its `notify` is
//! synchronous, so the UI thread fires `btv_input` / `nvim_ui_*` directly on a cloned
//! handle.
//!
//! The focused window scrolls pixel-smoothly: a redraw's scroll gesture arms a
//! [`ScrollAnim`] the client clock interpolates, and the paint loop repaints every
//! frame while it runs. See [`render`] for the rendering scope.
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

pub use input::{encode_input, is_paste};
pub use mouse::{
    button_name, cell_at, drain_notches, horizontal_action, mouse_modifier, vertical_action,
};
// The neutral text-insertion encoder, re-exported so the Tier-1 key test can cover
// the path several typed characters take (a dead key that didn't compose, an input
// method's commit).
pub use bemtvi_view::{encode_paste, encode_text};
pub use session::{
    parse_connect_uri, spawn_session, spawn_stdio_daemon_session, spawn_workspace_session, Session,
};
// The pure inline-inlay-hint geometry (the shift math) and the segment splice, so
// the Tier-1 `inlay` test can exercise them without a GPU — like the mouse helpers.
// The block-element geometry (`▀`, `█`, …), drawn as rectangles rather than glyphs, so
// the Tier-1 `blocks` test can check it without a GPU.
pub use render::{block_element_rects, split_block_elements};
pub use render::{
    inlay_shift, run_columns, seg_bg_spans, segment_columns, splice_inlay, split_at_char,
    virt_inline_shift, with_preedit, Seg, DEFAULT_BG, DEFAULT_ERROR_MSG, DEFAULT_FG,
    DEFAULT_INCSEARCH_BG, DEFAULT_INLAY, DEFAULT_LINE_NR, DEFAULT_SEARCH_BG, DEFAULT_STATUS_BG,
    DEFAULT_VISUAL_BG, DEFAULT_WIN_SEPARATOR, DIAG_ERROR, DIAG_HINT, DIAG_INFO, DIAG_WARN,
    MULTICURSOR_ACCENT,
};
// The pure per-row syntax-coloring layer (run splitting + the no-colorscheme group
// fallback), exported so the Tier-1 `syntax` test can exercise it without a GPU.
pub use render::{
    apply_search_fg, col_to_screen, group_fallback, rect_subtract, row_segments, text_run_origin,
};
// The wide-glyph mask (replace an off-grid emoji cluster with cell-width spaces) and
// the shrink-to-fit sizing of the cluster that mask reserved room for, so the Tier-1
// `wide` test can exercise both without shaping / a GPU.
pub use render::{
    cluster_scale, is_letterform, italic_runs, mask_segments, offgrid_clusters, overflow_ceiling,
    overflow_cells, Ink,
};
// The pure caret-cell math for the command line and the picker prompt (char-offset
// wire fields → display-width cells), exported for the Tier-1 `caret` test.
pub use render::{cmdline_caret_col, query_caret_col};
// The block cursor's reverse-video pair — the opaque quad's colour and the inverted
// colour its covered glyph is re-drawn in — plus that recolor, exported for the
// Tier-1 `cursor` test.
pub use render::{apply_cursor_fg, block_cursor_colors};
// Where a floating menu box anchors — window-relative for a completion popup, but
// screen-relative for the command-line wildmenu. Exported for the Tier-1 `menu` test.
pub use render::menu_box_origin;
// The point-size → device-pixel font size the cell grid is measured at, exported so
// the Tier-1 `grid` test can shape a row at a fractional display scale and check it
// still lands on the grid.
pub use render::physical_font_size;
// The sRGB→linear color conversions feeding the quad pipeline, exported so the Tier-1
// `color` test can pin the channel order without a GPU.
pub use render::{color_to_rgba, srgb_to_color, srgb_to_color_rgba};

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use bemtvi_rpc::{connect, Incoming, Rpc};
use bemtvi_view::{arm_scroll, ResizeCursor, View};
// Re-exported for the Tier-1 band data-contract tests (`tests/scroll_band.rs`).
pub use bemtvi_view::ScrollAnim;
use mitsuami::prelude::{
    clipboard_text, node_ref, open_file, save_file, signal, sleep, spawn_local, Column, Cursor,
    ElementBuilder, GpuSurface, KeyCode, LengthExt, Modifiers, MouseButton, OpenFile, Point, Rect,
    SaveFile, ScrollDelta, Signal, Size, SurfaceHandle, SurfaceInput, SurfaceSize, SystemCursor,
    Window,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

use render::Renderer;
pub use render::ScrollFrame;

/// How often a held-at-the-edge mouse drag re-sends itself to keep the buffer
/// auto-scrolling (≈25 lines/sec). The surface only reports a drag (a pointer move
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

/// When a single-cell glyph may be drawn bigger than the one cell the editor reserved
/// for it, spilling its ink into the cell to its right.
///
/// A Nerd Font / symbol glyph is designed square — a full em wide — where a coding
/// font's cell is only ~0.6 em, so fitting it to its reserved cell (what the renderer
/// does by default; see [`crate::cluster_scale`]) shrinks it to about 60% and an icon
/// beside a filename looks undersized. But drawing it at its natural size paints over
/// whatever is in the next cell. The resolution is the neighbour: when the next cell is
/// blank there is nothing to paint over, so the glyph can have that space and render at
/// full size. This is wezterm's `allow_square_glyphs_to_overflow_width`, same three
/// modes and same default.
///
/// The column model never changes — the glyph still *occupies* one cell, so the cursor,
/// selections, and every column the server computes are untouched. Only the ink grows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GlyphOverflow {
    /// Never: every glyph is shrunk to the cells the editor reserved for it.
    Never,
    /// Only when the character to the right is a space, so the ink spills over blank
    /// background and can't collide with a neighbouring glyph. The default.
    #[default]
    WhenFollowedBySpace,
    /// Always, whatever follows — a glyph in the next cell is overpainted.
    Always,
}

impl GlyphOverflow {
    /// Parse a mode name, however it was spelled: the `--glyph-overflow` flag, the
    /// `BEMTVI_GUI_GLYPH_OVERFLOW` environment variable, or the `'guiglyphoverflow'`
    /// option relayed off the redraw. `never`, `always`, or `when-followed-by-space`
    /// (`space` for short), case- and separator-insensitive (`WhenFollowedBySpace` and
    /// `when_followed_by_space` both parse). `None` for anything else, which the caller
    /// reports rather than silently ignoring.
    ///
    /// The core validates `:set guiglyphoverflow=…` against this same set
    /// (`bemtvi-core/src/editor/options.rs`), and the web client re-spells it in
    /// `glyphOverflowMode`; keep the three in step.
    pub fn parse(spec: &str) -> Option<Self> {
        match spec.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "never" => Some(Self::Never),
            "always" => Some(Self::Always),
            "space" | "when-followed-by-space" | "whenfollowedbyspace" => {
                Some(Self::WhenFollowedBySpace)
            }
            _ => None,
        }
    }

    /// The accepted spellings, for a CLI/env error message.
    pub const NAMES: &'static str = "never | always | when-followed-by-space";
}

/// Client-side rendering configuration. The font family and point size are a
/// purely client concern — the server works in cells, not pixels — so they're set
/// here (CLI flags / environment), not through a server option. An unavailable
/// font name falls back to the system monospace.
#[derive(Clone, Debug)]
pub struct GuiConfig {
    /// Ordered font families: the first is the primary, the rest a wezterm-style
    /// fallback chain tried in turn for a glyph the primary lacks. Empty uses the
    /// system monospace. Set from a comma-separated `--font` / `BEMTVI_GUI_FONT`.
    pub fonts: Vec<String>,
    /// Font point size, before the display's scale factor is applied.
    pub font_size: f32,
    /// *Maximum* render scale for an emoji / wide fallback glyph relative to the text
    /// cell — a color-emoji font draws smaller than its reserved cells, so this sizes it
    /// up. It is a ceiling, not a constant: a glyph whose ink is *wider* than the cells
    /// the editor reserved (a Nerd Font icon is a full em where a coding font's cell is
    /// ~0.6 em) is shrunk to fit instead, so it can't collide with the next character.
    /// Set from `--emoji-scale` / `BEMTVI_GUI_EMOJI_SCALE`.
    pub emoji_scale: f32,
    /// Whether a square single-cell glyph (a Nerd Font icon) may be drawn at its natural
    /// size, overflowing into the cell to its right, instead of being shrunk to fit its
    /// own. See [`GlyphOverflow`]. Set from `--glyph-overflow` /
    /// `BEMTVI_GUI_GLYPH_OVERFLOW`.
    pub glyph_overflow: GlyphOverflow,
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            fonts: Vec::new(),
            font_size: 15.0,
            emoji_scale: 1.2,
            glyph_overflow: GlyphOverflow::default(),
        }
    }
}

impl GuiConfig {
    /// Overrides from the environment: `BEMTVI_GUI_FONT` (family, or a comma-separated
    /// fallback list) and `BEMTVI_GUI_FONT_SIZE` (points). Absent/blank/invalid values
    /// keep the default; CLI flags layered on top take precedence (see `main`).
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Some(name) = std::env::var_os("BEMTVI_GUI_FONT") {
            c.set_font(&name.to_string_lossy());
        }
        if let Ok(size) = std::env::var("BEMTVI_GUI_FONT_SIZE") {
            if let Ok(pt) = size.trim().parse::<f32>() {
                c.set_font_size(pt);
            }
        }
        if let Ok(scale) = std::env::var("BEMTVI_GUI_EMOJI_SCALE") {
            if let Ok(s) = scale.trim().parse::<f32>() {
                c.set_emoji_scale(s);
            }
        }
        if let Ok(mode) = std::env::var("BEMTVI_GUI_GLYPH_OVERFLOW") {
            // A mode name is all-or-nothing — unlike a size, there is nothing sensible to
            // clamp a typo to — so an unrecognised one says so rather than quietly
            // rendering in a mode the user didn't ask for.
            match GlyphOverflow::parse(&mode) {
                Some(m) => c.glyph_overflow = m,
                None => eprintln!(
                    "bemtvi-gui: ignoring BEMTVI_GUI_GLYPH_OVERFLOW={mode:?} \
                     (expected {})",
                    GlyphOverflow::NAMES
                ),
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

    /// Set the emoji render scale *ceiling*, clamped to `[0.25, 4.0]` so a typo can't
    /// blow a glyph up across the screen or vanish it. A non-finite or non-positive
    /// value is ignored (keeps the current scale).
    pub fn set_emoji_scale(&mut self, scale: f32) {
        if scale.is_finite() && scale > 0.0 {
            self.emoji_scale = scale.clamp(0.25, 4.0);
        }
    }
}

// The image store (render thread) emits an `ImageFetch` when it sees a `remote`
// preview it can't read off local disk; the IO thread fulfils it with
// `bemtvi_image_read` and posts the bytes back as [`UserEvent::ImageBytes`]. The
// request type itself is the shared client model's (`bemtvi_view::images`).
pub(crate) use bemtvi_view::images::DecodeReq;
pub(crate) use bemtvi_view::images::ImageFetch;

/// A request from the App to bring up a **new** session, swapping the live one. Built off
/// the UI thread by the session loop (see [`run`]) and reported back via
/// [`UserEvent::Connected`] on success, or `:echoerr` on failure.
pub enum SessionRequest {
    /// `:connect …` — switch to an edit-host (daemon/QUIC) session.
    Connect(remote::ConnectTarget),
    /// `:workspace <dir>` — switch to a local workspace session rooted at the directory.
    Workspace(PathBuf),
}

/// Events the IO thread sends the UI thread.
pub enum UserEvent {
    /// A decoded `redraw`: replace the view and repaint.
    Redraw(Box<View>),
    /// An `bemtvi_image_read` reply: a remote preview's fetched bytes (or a read error),
    /// to hand to the image store and repaint. Carries the version the fetch was for.
    ImageBytes {
        path: String,
        version: (u64, u64),
        result: Result<Vec<u8>, String>,
    },
    /// An off-thread preview decode's outcome (or failure), to hand to the image
    /// store and repaint. Carries the version the decode was requested at.
    ImageDecoded {
        path: String,
        version: (u64, u64),
        decoded: Option<image::DynamicImage>,
    },
    /// A `:connect` brought up a new (daemon or local) session: swap the App's live
    /// RPC handle to it, mark whether it is remote, and re-attach the UI.
    Connected {
        rpc: Box<Rpc>,
        /// Whether the new session is an edit-host (daemon) one — drives the local
        /// file-dialog suppression (see [`dialog_action`]).
        remote: bool,
    },
    /// The server asked the UI to exit (`bemtvi_exit`), or the connection closed.
    Exit,
    /// A fatal signal (`kill`, SIGHUP, SIGINT) asked the process to stop. Delivered
    /// from [`bemtvi_view::signals`]' watchdog thread over the same channel, so the
    /// shutdown runs on the UI thread like any other quit.
    Shutdown,
}

/// Run the GUI client until the window closes or the server disconnects, rendering with
/// `config`'s font. Must be called on the main thread (the platform's UI toolkits'
/// requirement).
///
/// `initial` is the already-built startup [`Session`] (its server thread is running) —
/// embedded local, or a daemon session if launched with `--connect-daemon` / a
/// `bemtvi://` target. After startup, `:connect [user@]host[:port][/file]` or
/// `:connect bemtvi://…` swaps the window onto a **new** local server whose host seams
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
    config_source: bemtvi_server::ConfigSource,
) -> Result<()> {
    let (proxy, events) = tokio::sync::mpsc::unbounded_channel::<UserEvent>();

    // Be killable without losing the session. There is no terminal to hand back here
    // — a window is not a tty — so this client takes only the *graceful* half: a
    // `kill` / SIGHUP / SIGINT wakes the UI thread through the channel and the editor
    // quits itself properly (see `UserEvent::Shutdown`), with the watchdog in
    // `bemtvi_view::signals` still guaranteeing the process dies if that goes nowhere.
    {
        let proxy = proxy.clone();
        bemtvi_view::signals::install(bemtvi_view::signals::Config {
            restore_sequence: Vec::new(),
            restore_termios: false,
            on_shutdown: Box::new(move || {
                let _ = proxy.send(UserEvent::Shutdown);
            }),
        });
    }

    // Shutdown signal: when the UI's run loop ends — the window closed after a `:qa`,
    // or a server-sent `bemtvi_exit` — the main thread notifies the IO thread so it drops the
    // RPC connection. The IO thread (not `run`) owns the stream, so unless it closes
    // that, the server never sees EOF, never winds down, and the join below hangs
    // forever — the close-button freeze.
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let io_shutdown = shutdown.clone();

    // Hand the `Rpc` handle from the IO thread back to the main thread once the
    // (first) connection is up, so the UI side can fire input synchronously.
    let (rpc_tx, rpc_rx) = std::sync::mpsc::channel::<Rpc>();
    // `:connect <target>` / `:workspace <dir>` from the App request a switch to a new
    // (daemon or local-workspace) session.
    let (reconnect_tx, mut reconnect_rx) = tokio::sync::mpsc::unbounded_channel::<SessionRequest>();
    // The image store (render thread) requests remote preview bytes here; the IO thread
    // fulfils each over `bemtvi_image_read` on the *current* session and posts the bytes
    // back as `UserEvent::ImageBytes`.
    let (fetch_tx, mut fetch_rx) = tokio::sync::mpsc::unbounded_channel::<ImageFetch>();
    // Preview decodes, the same shape: the (synchronous) paint can only enqueue a
    // decode request — local previews carry just the path, remote ones their fetched
    // bytes — and the IO thread runs the disk read + full-res decode on a blocking
    // task, never on the UI thread, posting the outcome back as
    // `UserEvent::ImageDecoded`.
    let (decode_tx, mut decode_rx) = tokio::sync::mpsc::unbounded_channel::<DecodeReq>();
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
            let mut retired: Vec<std::thread::JoinHandle<anyhow::Result<()>>> = Vec::new();
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
                    if proxy.send(event).is_err() {
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
                                    if proxy.send(UserEvent::Redraw(Box::new(view))).is_err() {
                                        break 'session; // event loop gone
                                    }
                                }
                                "bemtvi_exit" => {
                                    let _ = proxy.send(UserEvent::Exit);
                                    break 'session;
                                }
                                // `btv.session.reconnect(spec)` from inside the VM (§B): a
                                // plugin initiates the reload. Parse the wire spec and build
                                // the new session off the UI thread (like `:connect`), then
                                // `built_rx` swaps onto it. A bad spec / failed build is
                                // reported and leaves the current session intact.
                                "btv_session_reconnect" => {
                                    match params.first().ok_or_else(|| {
                                        anyhow::anyhow!("btv_session_reconnect: missing spec")
                                    }).and_then(bemtvi_server::ReconnectSpec::from_value) {
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
                                "btv_connect_fallback" => {
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
                                                    "not a connect target: {url:?} (expected bemtvi://… or [user@]host[:port][/file])"
                                                ),
                                            ),
                                        },
                                        None => report_session_error(
                                            &rpc,
                                            ":connect",
                                            &anyhow::anyhow!("btv_connect_fallback: missing url"),
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
                        // over `bemtvi_image_read` on the *current* session (a spawned
                        // task, so a slow daemon read doesn't stall redraws) and post the
                        // reply back to the UI thread, which hands it to the image store.
                        // `None` (App, the only sender, is gone) just falls through —
                        // teardown is handled by the other arms.
                        fetch = fetch_rx.recv() => if let Some(ImageFetch { path, version }) = fetch {
                            let rpc = rpc.clone();
                            let proxy = proxy.clone();
                            tokio::spawn(async move {
                                let result = bemtvi_view::images::image_read_reply(
                                    rpc.request(
                                        "bemtvi_image_read",
                                        vec![Value::from(path.as_str())],
                                    )
                                    .await,
                                );
                                let _ = proxy.send(UserEvent::ImageBytes {
                                    path,
                                    version,
                                    result,
                                });
                            });
                        },
                        // A preview needs decoding: run the disk read + full-res decode
                        // on a blocking task (a slow disk or a huge image must never
                        // stall repaints) and post the outcome back as
                        // `UserEvent::ImageDecoded`.
                        decode = decode_rx.recv() => if let Some(req) = decode {
                            let proxy = proxy.clone();
                            tokio::task::spawn_blocking(move || {
                                let (path, version, decoded) =
                                    req.decode(bemtvi_view::images::MAX_EDGE);
                                let _ = proxy.send(UserEvent::ImageDecoded {
                                    path,
                                    version,
                                    decoded,
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
            let _ = proxy.send(UserEvent::Exit);
            let mut panicked = false;
            for handle in current_handle.into_iter().chain(retired) {
                match handle.join() {
                    Err(_) => panicked = true,
                    // A server error (not a panic): report it like the old in-thread
                    // eprintln did, but keep the exit clean.
                    Ok(Err(e)) => eprintln!("bemtvi-gui: server error: {e}"),
                    Ok(Ok(())) => {}
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
    let app = App::new(
        rpc,
        config,
        open_dir,
        initial_remote,
        reconnect_tx,
        fetch_tx,
        decode_tx,
    );
    // Runs until the window closes. The app id matches the installed `.desktop` file's
    // basename (its `StartupWMClass`), so Linux desktops associate the AppImage's icon
    // with the window; it's also an unpackaged Windows build's AppUserModelID.
    let ui_app = app.clone();
    mitsuami::App::new()
        .id("bemtvi")
        .name("bemtvi")
        .open(move || App::window(ui_app, events))
        .run();

    // Leak the renderer instead of dropping it. wgpu's GLES/EGL backend segfaults inside
    // `eglTerminate` when its `Instance` drops on Wayland + Mesa — an upstream wgpu/Mesa
    // teardown bug: the EGL display teardown marshals a request onto an already-invalid
    // Wayland proxy (`terminate_display` → `eglTerminate` → `wl_proxy_marshal_flags` →
    // SIGSEGV). wgpu instantiates the GL/EGL backend even when Vulkan is the chosen
    // adapter, so restricting the adapter wouldn't dodge it. `run` only ever returns as
    // the process is about to exit (see `main` — every path tail-calls it), so forgetting
    // the renderer — and with it every wgpu handle plus the surface it holds — is safe:
    // the OS reclaims the GPU/driver resources at exit anyway. Everything else in `app`
    // (the RPC handle, the channel senders) still drops normally, so the IO thread's
    // shutdown handshake below is unaffected.
    std::mem::forget(app.borrow_mut().renderer.take());

    // The UI is done: stop the IO thread and wait for it, so the streams are dropped
    // (servers see EOF) and their threads are joined before returning.
    shutdown.notify_one();
    let panicked = io.join().unwrap_or(true);
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

/// Report a session-switch failure in the GUI message line via the *current* session —
/// the new one never came up. `label` is the command that failed (`:connect` — bad host,
/// refused auth, a malformed `bemtvi://` URI; or `:workspace` — a missing / non-directory
/// path). bemtvi is its own editor and has no vimscript `:echohl`; `:echoerr` already
/// renders the text as an error message. The error chain is flattened to one line
/// (`echoerr` rejects newlines) and single quotes doubled (Vim string escaping) so a
/// hostname or path can't break the command.
fn report_session_error(rpc: &Rpc, label: &str, err: &anyhow::Error) {
    let line = format!("{label} failed: {err:#}").replace('\n', "; ");
    let escaped = line.replace('\'', "''");
    rpc.notify(
        "btv_command",
        vec![Value::from(format!("echoerr '{escaped}'"))],
    );
}

/// What the UI-thread side of the window holds: the window's own reactive state, the
/// renderer, and the latest view. Shared (`Rc<RefCell<…>>`) by the surface's handlers
/// and the UI-thread tasks — the paint loop, the IO thread's event pump and the timers
/// — which all run on mitsuami's UI thread, one at a time.
struct App {
    /// This app itself, for the tasks its methods start (timers, dialogs, paste).
    me: Weak<RefCell<App>>,
    rpc: Rpc,
    /// The editor's surface, once mitsuami made it. The renderer is built on it at its
    /// first non-empty size.
    surface: Option<SurfaceHandle>,
    renderer: Option<Renderer>,
    view: View,
    /// The in-flight scroll slide, if any (keeps the paint loop going).
    scroll: Option<ScrollAnim>,
    /// Wakes the paint loop: [`Self::request_redraw`] coalesces any number of requests
    /// into one paint.
    redraw: Rc<tokio::sync::Notify>,
    /// The window's own state, made in its reactive scope: whether it's open (closing
    /// it ends the app), the pointer's cursor over the surface (a resize arrow over a
    /// split separator), the caret the input method's candidate window opens by, and
    /// whether input methods are on (unless `'imdisable'`: [`View::imdisable`]).
    open: Option<Signal<bool>>,
    cursor: Option<Signal<Cursor>>,
    caret: Option<Signal<Option<Rect>>>,
    input_method: Option<Signal<bool>>,
    /// The modifiers held with the latest input — a wheel or button carries its own,
    /// and `send_mouse` reports them.
    mods: Modifiers,
    /// The surface's scale (pixels per point): the surface reports the pointer in
    /// points, the renderer works in pixels.
    scale: f32,
    /// Latest pointer position in physical pixels (a wheel event carries none, so
    /// pointer moves are tracked here for the next wheel).
    cursor_px: (f64, f64),
    /// Whether the left mouse button is held — a pointer move while it's set is a
    /// drag (the server extends the Visual selection from the press anchor).
    mouse_down: bool,
    /// The cell the last left-drag was reported at, so motion within one cell is
    /// coalesced to a single drag (the server works in cells, not pixels).
    last_drag_cell: Option<(u16, u16)>,
    /// The resize cursor currently shown (over a draggable separator / dock edge),
    /// or `None` for the default arrow. Tracked so a hover only sets the cursor when
    /// the shape actually changes, not on every pointer move.
    resize_hint: Option<ResizeCursor>,
    /// While the left button is held with the pointer in the top/bottom edge band,
    /// the cell to re-issue the drag at so the buffer keeps auto-scrolling without
    /// further pointer motion. `None` when not at an edge or the button is up.
    autoscroll: Option<(u16, u16)>,
    /// Whether the auto-scroll repeat task is running (it stops once `autoscroll`
    /// clears), so arming it again doesn't start a second one.
    autoscrolling: bool,
    /// Fractional wheel remainder per axis `(horizontal, vertical)`, so a
    /// pixel-precise trackpad still scrolls one whole line at a time (see
    /// [`mouse::drain_notches`]).
    wheel_accum: (f32, f32),
    /// Last `(cols, windows_rows)` reported to the server, to suppress
    /// no-op resize notifications.
    reported: (u16, u16),
    /// When set, fire one `bemtvi_input_flush` once this instant passes with no
    /// further input — the GUI's `timeoutlen` timer. Armed by each keystroke and
    /// re-armed by the next, so it measures idle-since-last-key; each arming starts a
    /// timer task, which does nothing if a later key moved the deadline.
    flush_deadline: Option<Instant>,
    /// The last IME cursor rect `(x, y, w, h)` set as the surface's caret, so a frame
    /// that didn't move the caret leaves it alone.
    ime_area: Option<(f32, f32, f32, f32)>,
    /// Font config (CLI flags / environment) used as the renderer's startup default
    /// and the fallback for any field a `guifont` doesn't set.
    config: GuiConfig,
    /// The last `view.guifont` applied to the renderer, so a redraw only re-shapes
    /// when it actually changed (`:set guifont=…` / the init-time value).
    applied_guifont: String,
    /// The last `view.guiglyphoverflow` applied to the renderer, so a redraw only
    /// re-applies when it actually changed (`:set guiglyphoverflow=…` / the init-time
    /// value). Empty is both the initial state and the option's default — which means
    /// "keep what `--glyph-overflow` / the env set".
    applied_glyph_overflow: String,
    /// A directory passed on the command line (`bemtvi-gui somedir`), if any. Popped
    /// as the native file picker (anchored there) once the renderer exists, instead of
    /// the server's in-window listing — see [`Self::init_renderer`]. Taken on first
    /// use so it fires exactly once.
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
    /// Handed to the renderer's image store so it can off-load preview decodes (a
    /// disk read + full-res decode must not block repaints). The IO thread drains
    /// it and posts outcomes back as [`UserEvent::ImageDecoded`].
    decode_tx: tokio::sync::mpsc::UnboundedSender<DecodeReq>,
}

impl App {
    fn new(
        rpc: Rpc,
        config: GuiConfig,
        open_dir: Option<PathBuf>,
        remote: bool,
        reconnect: tokio::sync::mpsc::UnboundedSender<SessionRequest>,
        fetch_tx: tokio::sync::mpsc::UnboundedSender<ImageFetch>,
        decode_tx: tokio::sync::mpsc::UnboundedSender<DecodeReq>,
    ) -> Rc<RefCell<Self>> {
        Rc::new_cyclic(|me| {
            RefCell::new(Self {
                me: me.clone(),
                rpc,
                surface: None,
                renderer: None,
                view: View::default(),
                scroll: None,
                redraw: Rc::new(tokio::sync::Notify::new()),
                open: None,
                cursor: None,
                caret: None,
                input_method: None,
                mods: Modifiers::default(),
                scale: 1.0,
                cursor_px: (0.0, 0.0),
                mouse_down: false,
                last_drag_cell: None,
                resize_hint: None,
                autoscroll: None,
                autoscrolling: false,
                wheel_accum: (0.0, 0.0),
                reported: (0, 0),
                flush_deadline: None,
                ime_area: None,
                config,
                applied_guifont: String::new(),
                applied_glyph_overflow: String::new(),
                open_dir,
                remote,
                reconnect,
                fetch_tx,
                decode_tx,
            })
        })
    }

    /// The window: titled `bemtvi`, 960×640 points, its whole content the editor's
    /// surface, focused so keys go to it from the start. Built in mitsuami's app scope,
    /// where it starts the UI-thread tasks: the pump for the IO thread's `events`, and
    /// the paint loop.
    fn window(this: Rc<RefCell<Self>>, mut events: UnboundedReceiver<UserEvent>) -> Window {
        let open = signal(true);
        let cursor = signal(Cursor::Default);
        let caret = signal(None::<Rect>);
        let input_method = signal(true);
        {
            let mut app = this.borrow_mut();
            app.open = Some(open);
            app.cursor = Some(cursor);
            app.caret = Some(caret);
            app.input_method = Some(input_method);
        }
        let app = this.clone();
        spawn_local(async move {
            while let Some(event) = events.recv().await {
                app.borrow_mut().user_event(event);
            }
        });
        spawn_local(Self::paint_loop(this.clone()));

        let closing = this.clone();
        let content = this.clone();
        Window::new("bemtvi")
            .size(Size::new(960.0, 640.0))
            .open(open)
            // Don't close on the close button directly — that would discard a modified
            // buffer silently. Route through the server's `:qa`, which quits a clean
            // editor (it then sends `bemtvi_exit`, arriving as `UserEvent::Exit`) but
            // refuses with `E37` when a buffer is unsaved, keeping the window open.
            // `btv_command` runs the ex-command regardless of the current mode (unlike
            // injecting `:qa<CR>` keys). mitsuami routes the platform's Quit (macOS's
            // app menu, the Dock, a logout) here too.
            .on_close_request(move || {
                closing
                    .borrow()
                    .rpc
                    .notify("btv_command", vec![Value::from("qa")]);
            })
            .content(move || {
                let surface = node_ref();
                surface.focus();
                let (ready, resized, input) = (content.clone(), content.clone(), content.clone());
                Column::new().size(100.pct(), 100.pct()).child(
                    GpuSurface::new()
                        .label("bemtvi")
                        .node_ref(surface)
                        .size(100.pct(), 100.pct())
                        // Text as a text field takes it: layouts, dead keys, AltGr and
                        // input methods (composed accents, CJK) all type — input methods
                        // unless a config sets `'imdisable'`.
                        .text_input(true)
                        .input_method(input_method)
                        .text_cursor(caret)
                        .cursor(cursor)
                        .on_ready(move |handle| ready.borrow_mut().surface = Some(handle))
                        .on_resize(move |size| resized.borrow_mut().resized(size))
                        .on_input(move |event| input.borrow_mut().input(&event)),
                )
            })
    }

    /// Paints whenever a repaint was asked for (any number of requests, one paint),
    /// and every 8 ms while a scroll slide runs: `present` (Fifo vsync) then paces the
    /// actual paint to the display refresh.
    async fn paint_loop(this: Rc<RefCell<Self>>) {
        let wake = this.borrow().redraw.clone();
        loop {
            wake.notified().await;
            while this.borrow_mut().paint() {
                sleep(Duration::from_millis(8)).await;
            }
        }
    }

    /// Paint one frame; whether a slide is still running. A finished slide settles
    /// first, so the final frame is the live (destination) view rather than a clamped
    /// band.
    fn paint(&mut self) -> bool {
        if self.scroll.as_ref().is_some_and(ScrollAnim::done) {
            self.scroll = None;
        }
        let frame = self.scroll.as_ref().map(ScrollFrame::of);
        if let Some(r) = self.renderer.as_mut() {
            if let Err(e) = r.render(&self.view, frame.as_ref()) {
                eprintln!("bemtvi-gui: render error: {e}");
            }
        }
        self.update_ime_area();
        self.scroll.is_some()
    }

    fn request_redraw(&self) {
        self.redraw.notify_one();
    }

    /// The surface's size in pixels, or its scale, changed. The first non-empty size
    /// builds the renderer on the surface.
    fn resized(&mut self, size: SurfaceSize) {
        if size.is_empty() {
            return;
        }
        if self.renderer.is_none() {
            self.init_renderer(size);
            return;
        }
        if size.scale > 0.0 && size.scale != self.scale {
            // The renderer's cell metrics are device pixels (`points × scale`), so a
            // DPI change — dragging the window onto a different-scale monitor — must
            // re-derive them, or the glyphs keep the old monitor's pixel size (wrong
            // physical size + a wrong grid). Re-applying the current font at the new
            // scale re-measures the cell, re-reports the grid, and repaints.
            self.scale = size.scale;
            if let Some(r) = self.renderer.as_mut() {
                r.set_scale(size.scale);
            }
            self.apply_guifont();
        }
        if let Some(r) = self.renderer.as_mut() {
            r.resize(size.width, size.height);
        }
        self.report_size(false);
        self.request_redraw();
    }

    /// Build the renderer on the surface at its first size, attach the UI, and apply
    /// what arrived before it existed.
    fn init_renderer(&mut self, size: SurfaceSize) {
        let Some(surface) = self.surface.clone() else {
            return;
        };
        self.scale = if size.scale > 0.0 { size.scale } else { 1.0 };
        match Renderer::new(
            surface,
            size,
            &self.config,
            self.fetch_tx.clone(),
            self.decode_tx.clone(),
        ) {
            Ok(r) => self.renderer = Some(r),
            Err(e) => {
                // Fail loud: without a renderer there's nothing to show, so quit the
                // session (the server answers with `bemtvi_exit`) rather than leave a
                // blank window up.
                eprintln!("bemtvi-gui: failed to init renderer: {e}");
                self.rpc.notify("btv_command", vec![Value::from("qall!")]);
                return;
            }
        }
        self.report_size(true);
        // Apply any `guifont` already received before the renderer existed (a redraw
        // can beat the surface); a no-op for the default empty value.
        self.apply_guifont();
        // Same for `'guiglyphoverflow'`, which an `init.lua` will have set before the
        // first frame; empty leaves the renderer on the CLI/env mode it was built with.
        self.apply_glyph_overflow();
        self.request_redraw();
        // A directory given on the command line opens the native file picker there
        // (taken so it fires once).
        if let Some(dir) = self.open_dir.take() {
            self.pick_open_at("e", &dir.to_string_lossy());
        }
    }

    /// Something from the IO thread.
    fn user_event(&mut self, event: UserEvent) {
        match event {
            UserEvent::Redraw(view) => {
                self.view = *view;
                self.update_input_method();
                // A changed `guifont` re-shapes the renderer (and re-reports the
                // grid) before this frame paints.
                if self.view.guifont != self.applied_guifont {
                    self.apply_guifont();
                }
                // A changed `'guiglyphoverflow'` re-sizes off-grid glyphs from this
                // frame on (placement only — nothing to re-shape).
                if self.view.guiglyphoverflow != self.applied_glyph_overflow {
                    self.apply_glyph_overflow();
                }
                // (Re)arm or clear the scroll slide from this frame's gesture.
                self.scroll = arm_scroll(&self.view, self.scroll.take());
                self.request_redraw();
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
                    self.request_redraw();
                }
            }
            // An off-paint decode finished (or failed): hand the outcome to the image
            // store — which drops it if a newer version superseded it — and repaint,
            // so the picture replaces its loading placeholder.
            UserEvent::ImageDecoded {
                path,
                version,
                decoded,
            } => {
                if let Some(r) = self.renderer.as_mut() {
                    r.deliver_image_decode(path, version, decoded);
                    self.request_redraw();
                }
            }
            // A `:connect` brought up a new server: swap to its RPC handle (dropping the
            // old one — the App's last clone, which lets the old connection wind down),
            // update remote-ness, and re-attach the UI. Clearing `reported` forces
            // `report_size` to send a fresh `btv_ui_attach`; resetting the view avoids
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
                // The reset view carries no `'guiglyphoverflow'` either: back to the
                // CLI/env mode until the new server's first redraw says otherwise.
                self.apply_glyph_overflow();
                self.request_redraw();
            }
            // Closing the last window ends mitsuami's run loop, and `run`'s teardown.
            UserEvent::Exit => {
                if let Some(open) = self.open {
                    open.set(false);
                }
            }
            // Killed. Quit the session the way `:qall!` does rather than letting the
            // process be torn down mid-tick: the exit sequence runs
            // (QuitPre/ExitPre/VimLeavePre/VimLeave, so plugins persist their state)
            // and the server's clean-exit shada flush lands. The bang is deliberate —
            // nobody is there to answer an E37 prompt — and so is the missing `w`:
            // writing files the user never asked to write would be worse than losing
            // the edits. The server answers with `bemtvi_exit`, which arrives back here
            // as `UserEvent::Exit` and closes the window; `run`'s tail then re-raises
            // the signal so the kill still looks like a kill.
            UserEvent::Shutdown => {
                self.rpc.notify("btv_command", vec![Value::from("qall!")]);
            }
        }
    }

    /// Keys, typed text and the pointer on the surface.
    fn input(&mut self, input: &SurfaceInput) {
        match input {
            SurfaceInput::PointerMoved {
                position,
                modifiers,
            } => {
                self.mods = *modifiers;
                self.cursor_px = self.pixels(*position);
                // While the left button is held, a move into a new cell is a drag.
                self.mouse_drag();
                self.update_resize_cursor();
            }
            SurfaceInput::Button {
                button,
                pressed,
                position,
                modifiers,
            } => {
                self.mods = *modifiers;
                self.cursor_px = self.pixels(*position);
                match (button, pressed) {
                    (MouseButton::Primary, true) => self.mouse_left_press(),
                    (MouseButton::Primary, false) => self.mouse_left_release(),
                    // Right / middle clicks have no client-owned overlay affordance,
                    // so they forward straight to the server at the pointer cell — the
                    // `'mousemodel'` right-click branch and middle-click paste live
                    // there. Only the press is meaningful (the server no-ops
                    // right/middle drag + release), mirroring the TUI.
                    (button, true) => {
                        if let (Some(name), Some((col, row))) =
                            (mouse::button_name(*button), self.pointer_cell())
                        {
                            self.send_mouse(name, "press", col, row);
                        }
                    }
                    _ => {}
                }
            }
            SurfaceInput::Scroll { delta, modifiers } => {
                self.mods = *modifiers;
                self.mouse_wheel(*delta);
            }
            SurfaceInput::Key {
                code,
                pressed: true,
                unmodified,
                modifiers,
                ..
            } => {
                self.mods = *modifiers;
                self.key_press(input, *code, *unmodified, *modifiers);
            }
            // Text an input method committed (composed accents, CJK, …): the user's own
            // typing arriving as one string, not a paste, so it drives insert mode
            // normally (auto-pairs and the rest) rather than going in literally.
            SurfaceInput::Text(_) => self.send_keys(input),
            // An input method taking back what keys typed, to replace it (macOS's
            // accents replace the letter a held key typed): delete it in the buffer.
            SurfaceInput::DeleteSurrounding { .. } => self.send_keys(input),
            // An input method's composition in progress: the surface is ours to draw,
            // so the platform can't show it — paint it at the cursor until it's
            // committed (a `Text`) or cancelled (an empty one).
            SurfaceInput::Preedit { text, cursor } => {
                if let Some(r) = self.renderer.as_mut() {
                    r.set_preedit(text, *cursor);
                }
                self.request_redraw();
            }
            _ => {}
        }
    }

    /// A key went down (or repeats).
    fn key_press(
        &mut self,
        input: &SurfaceInput,
        code: KeyCode,
        unmodified: Option<char>,
        mods: Modifiers,
    ) {
        // Paste from the system clipboard (Cmd+V / Ctrl+Shift+V / Shift+Insert), fed
        // through the same `encode_paste` the TUI uses for terminal bracketed paste.
        // Claimed before key encoding so the gesture's base key isn't also typed.
        if input::is_paste(code, unmodified, mods) {
            self.paste_clipboard();
            return;
        }
        // `<CR>` over a dialog-triggering `:` command line pops a native file dialog
        // and runs the real command with the chosen path, in place of the keystroke.
        // Only for a true `:` ex command (not a search or a `vim.ui.input` prompt).
        if matches!(code, KeyCode::Enter | KeyCode::NumpadEnter)
            && self.view.command_mode
            && self.view.cmdline_prefix == ':'
            && self.view.cmdline_prompt.is_empty()
            // …but NOT while a completion picker (`btv.picker`, e.g. the cmdline file
            // completer) floats over the still-open command line: there `<CR>` belongs
            // to the picker so it can descend into a directory or paste the chosen
            // path. Intercepting it here would pop the native dialog / submit instead,
            // copying every directory into the line and running the last one (the
            // GUI-only divergence from the TUI, where every key reaches the server's
            // picker untouched).
            && self.view.menu.is_none()
        {
            // `:workspace [dir]` swaps this window onto a fresh local session rooted at
            // the directory. It is a client affordance, *also* registered server-side
            // as a no-op virtual command (see [`session::CLIENT_INIT_LUA`]) so it gets
            // completion, help, and history. So request the swap here, but DON'T
            // swallow the `<CR>`: let it fall through and submit the command line
            // normally, which records it in `:` history and runs the harmless no-op
            // body. The swap then arrives async (see `UserEvent::Connected`).
            //
            // `:connect` is NOT intercepted here: it is a real prelude command (§C)
            // that routes through the VM so a connector can claim it, then swaps via a
            // `btv_session_reconnect` (provider) or `btv_connect_fallback` (direct
            // dial) notification handled in [`crate::run`].
            if let Some(dir) = remote::workspace_command(&self.view.cmdline) {
                let _ = self
                    .reconnect
                    .send(SessionRequest::Workspace(PathBuf::from(dir)));
            }
            let unnamed = self.view.focused().is_some_and(|w| w.unnamed);
            // `dialog_action` returns `None` in a remote (daemon) session, so the
            // native local-fs picker never fires for remote buffers — the command runs
            // as typed and the server/daemon handles it. Each `pick_*` swallows the
            // `<CR>` (it aborts the command line and re-issues the real command with
            // the chosen path), so `return` before the key is encoded below.
            match dialog_action(&self.view.cmdline, unnamed, self.remote) {
                Some(DialogAction::Open { base }) => {
                    self.pick_open(base);
                    return;
                }
                // Opening a *directory* (`:e somedir`): pop the picker there instead
                // of letting the server show its netrw listing. A file argument runs
                // as typed (the server opens it).
                Some(DialogAction::OpenPath { base, arg }) if Path::new(arg).is_dir() => {
                    let arg = arg.to_owned();
                    self.pick_open_at(base, &arg);
                    return;
                }
                Some(DialogAction::Save) => {
                    self.pick_save();
                    return;
                }
                _ => {}
            }
        }
        self.send_keys(input);
    }

    /// Encode a key press or committed text and feed it to the server, arming the
    /// `timeoutlen` flush (the next key re-arms it, so it measures idle-since-last-key).
    /// macOS composes Option(Alt)+key into a character (Option+c types `ç`), but
    /// `<A-c>`-style bindings (multi-cursor) want the chord there too, so on macOS
    /// Option makes chords (see [`input::encode_input`]).
    fn send_keys(&mut self, input: &SurfaceInput) {
        if let Some(notation) = input::encode_input(input, cfg!(target_os = "macos")) {
            self.rpc
                .notify("btv_input", vec![Value::from(notation.as_str())]);
            self.arm_flush_deadline();
        }
    }

    /// A point on the surface in physical pixels.
    fn pixels(&self, at: Point) -> (f64, f64) {
        let scale = f64::from(self.scale);
        (f64::from(at.x) * scale, f64::from(at.y) * scale)
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
            "btv_ui_attach"
        } else {
            "btv_ui_try_resize"
        };
        // A native window always has full key disambiguation (a key is reported by
        // where it is, so Ctrl+I is `KeyI` + control, distinct from `Tab`), so declare
        // the keyboard protocol active at attach — the server then keeps
        // `<C-i>`/`<C-m>`/`<C-[>`/`<C-h>` apart from `<Tab>`/`<CR>`/`<Esc>`/`<BS>`. It
        // also paints 24-bit color natively, so declare `truecolor` too — the server
        // then defaults in the bundled `bemtvi` colorscheme (giving `Normal` a real
        // background, not just the renderer's syntax fallback) when the config hasn't
        // chosen one. The map is read only at attach; a resize passes an empty one.
        let caps = if attach {
            Value::Map(vec![
                (Value::from("keyboard_protocol"), Value::Boolean(true)),
                (Value::from("truecolor"), Value::Boolean(true)),
            ])
        } else {
            Value::Map(vec![])
        };
        self.rpc
            .notify(method, vec![Value::from(cols), Value::from(win_rows), caps]);
    }

    /// Read the system clipboard (the platform's, through mitsuami) and feed it to the
    /// server as one `btv_input` via [`encode_paste`] — the GUI analogue of the TUI's
    /// bracketed paste (one notify, one redraw, no per-character trickle). A missing,
    /// empty, or non-text clipboard is a silent no-op (nothing to paste).
    fn paste_clipboard(&self) {
        let me = self.me.clone();
        spawn_local(async move {
            let Some(text) = clipboard_text().await else {
                return;
            };
            let notation = encode_paste(&text);
            if let (false, Some(app)) = (notation.is_empty(), me.upgrade()) {
                let mut app = app.borrow_mut();
                app.rpc
                    .notify("btv_input", vec![Value::from(notation.as_str())]);
                app.arm_flush_deadline();
            }
        });
    }

    /// Turn the platform's input methods off while `'imdisable'` is set, and back on
    /// as it's unset. A composition still showing as they go off is dropped.
    fn update_input_method(&mut self) {
        let Some(input_method) = self.input_method else {
            return;
        };
        let on = !self.view.imdisable;
        if input_method.get_untracked() == on {
            return;
        }
        input_method.set(on);
        if !on {
            if let Some(r) = self.renderer.as_mut() {
                r.set_preedit("", None);
            }
        }
    }

    /// Set the just-painted caret rect as the surface's caret, so the platform opens
    /// the IME candidate window at the caret instead of the window origin. The renderer
    /// reports the rect in physical pixels (`None` when a picker/panel owns the
    /// cursor); the surface takes points. Only a changed rect is set.
    fn update_ime_area(&mut self) {
        let area = self.renderer.as_ref().and_then(Renderer::ime_cursor_area);
        if area == self.ime_area {
            return;
        }
        self.ime_area = area;
        let scale = self.scale;
        if let Some(caret) = self.caret {
            caret.set(
                area.map(|(x, y, w, h)| Rect::new(x / scale, y / scale, w / scale, h / scale)),
            );
        }
    }

    /// Apply the relayed `view.guifont` to the renderer: parse the family and `:h`
    /// size, fall back to the CLI/env [`GuiConfig`] for any unset field, re-shape,
    /// and re-report the grid (the cell size changed). Called whenever `guifont`
    /// changes — including its first non-empty value from `init.lua` — so a
    /// `:set guifont=…` takes effect live. A no-op before the renderer exists; it's
    /// applied from [`Self::init_renderer`] once the renderer is built.
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
        self.request_redraw();
    }

    /// Apply the relayed `'guiglyphoverflow'` to the renderer — whether a square
    /// one-cell glyph may render over the cell to its right (see [`GlyphOverflow`]).
    /// Empty (the option's default) means the client keeps what `--glyph-overflow` /
    /// `BEMTVI_GUI_GLYPH_OVERFLOW` set, so a config that never mentions the option
    /// leaves the CLI in charge. An unparseable value can only come from a raw
    /// `btv.o` write (the `:set` path rejects it with E474), and says so on stderr
    /// rather than silently rendering in some other mode.
    ///
    /// Cheap — no re-shape, no grid change — so it just repaints.
    fn apply_glyph_overflow(&mut self) {
        let relayed = self.view.guiglyphoverflow.clone();
        let mode = if relayed.trim().is_empty() {
            self.config.glyph_overflow
        } else {
            GlyphOverflow::parse(&relayed).unwrap_or_else(|| {
                eprintln!(
                    "bemtvi-gui: ignoring guiglyphoverflow={relayed:?} (expected {})",
                    GlyphOverflow::NAMES
                );
                self.config.glyph_overflow
            })
        };
        self.applied_glyph_overflow = relayed;
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        renderer.set_glyph_overflow(mode);
        self.request_redraw();
    }

    /// Pop the native **open** dialog for an `…o` open command, then run `<base>
    /// <file>` with the chosen path. The caller swallows the `<CR>`.
    ///
    /// On a pick it aborts the half-typed command line (`<Esc>`) and runs the base
    /// command; on cancel it just aborts. bemtvi's `:edit`-family parser takes the
    /// whole argument tail as the filename (no backslash unescaping, unlike vim),
    /// so the path is passed **raw** — a leading `fnameescape` would wrongly bake
    /// its `\ ` escapes into the name. The dialog is the platform's own, on the window
    /// (a sheet on macOS), and doesn't block the UI thread: the command runs when it
    /// answers.
    fn pick_open(&self, base: &'static str) {
        self.pick(base, OpenFile::new().title("Open"));
    }

    /// Like [`Self::pick_open`] but opens the dialog **anchored at `dir`** — the GUI's
    /// answer to opening a *directory* (`:e somedir`, or `bemtvi-gui somedir`). Rather
    /// than the server's in-window netrw listing, the user gets the system file picker
    /// already browsing that directory, and the picked file is opened with `base`.
    /// Same contract and raw-path rationale as [`Self::pick_open`]; a relative `dir`
    /// resolves against the process cwd, exactly as the server's listing would.
    fn pick_open_at(&self, base: &'static str, dir: &str) {
        let dir = std::path::absolute(dir).unwrap_or_else(|_| PathBuf::from(dir));
        self.pick(base, OpenFile::new().title("Open").start_folder(dir));
    }

    fn pick(&self, base: &'static str, request: OpenFile) {
        let me = self.me.clone();
        spawn_local(async move {
            let picked = open_file(request)
                .await
                .and_then(|paths| paths.into_iter().next());
            if let Some(app) = me.upgrade() {
                app.borrow().run_picked(base, picked);
            }
        });
    }

    /// Pop the native **save** dialog for `:wo` / a bare `:w` on an unnamed buffer,
    /// then `:w <file>` to the chosen path (which also binds the buffer to it).
    /// Same contract and raw-path rationale as [`Self::pick_open`].
    fn pick_save(&self) {
        let me = self.me.clone();
        spawn_local(async move {
            let picked = save_file(SaveFile::new().title("Save As")).await;
            if let Some(app) = me.upgrade() {
                app.borrow().run_picked("w", picked);
            }
        });
    }

    /// Abort the half-typed command line, then run `<verb> <path>` for a picked
    /// file (nothing further on cancel). Shared by the open and save pickers.
    fn run_picked(&self, verb: &str, picked: Option<PathBuf>) {
        self.rpc.notify("btv_input", vec![Value::from("<Esc>")]);
        if let Some(path) = picked {
            let path = path.to_string_lossy();
            self.rpc
                .notify("btv_command", vec![Value::from(format!("{verb} {path}"))]);
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
        if !self.view.timeout {
            self.flush_deadline = None;
            return;
        }
        let wait = Duration::from_millis(self.view.timeoutlen);
        self.flush_deadline = Some(Instant::now() + wait);
        let me = self.me.clone();
        spawn_local(async move {
            sleep(wait).await;
            let Some(app) = me.upgrade() else {
                return;
            };
            let mut app = app.borrow_mut();
            // Idle `timeoutlen` reached (a later key moved the deadline on, and its own
            // timer fires then): nudge the server to resolve any withheld mapped prefix
            // (harmless when nothing is pending), and disarm.
            if app.flush_deadline.is_some_and(|d| Instant::now() >= d) {
                app.rpc.notify("bemtvi_input_flush", vec![]);
                app.flush_deadline = None;
            }
        });
    }

    /// Fire one `btv_input_mouse(button, action, modifier, grid=0, row, col)` —
    /// a mouse gesture at a global screen cell. The server owns the hit-test back
    /// to a window + buffer position; `grid` is always 0 (bemtvi is single-grid).
    fn send_mouse(&self, button: &str, action: &str, col: u16, row: u16) {
        self.rpc.notify(
            "btv_input_mouse",
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
        self.mouse_down = true;
        self.last_drag_cell = Some((col, row));

        self.send_mouse("left", "press", col, row);
        // A press-and-hold already in the edge band auto-scrolls without a drag.
        self.arm_autoscroll((col, row));
    }

    /// Arm or disarm continuous drag auto-scroll for a press/drag now at `cell`:
    /// armed when the cell sits in the top/bottom edge band, cleared otherwise. While
    /// it stays armed a task re-issues the drag every [`AUTOSCROLL_INTERVAL`], so the
    /// buffer keeps scrolling even though a drag is only reported on pointer motion.
    fn arm_autoscroll(&mut self, cell: (u16, u16)) {
        let rows = self.renderer.as_ref().map_or(0, |r| r.grid_size().1);
        if !in_scroll_zone(cell.1, rows) {
            self.autoscroll = None;
            return;
        }
        self.autoscroll = Some(cell);
        if std::mem::replace(&mut self.autoscrolling, true) {
            return; // the running task picks the new cell up
        }
        let me = self.me.clone();
        spawn_local(async move {
            loop {
                sleep(AUTOSCROLL_INTERVAL).await;
                let Some(app) = me.upgrade() else {
                    return;
                };
                let mut app = app.borrow_mut();
                // The server scrolls the focused window one line per drag it lands
                // past the text body and re-extends the selection.
                match app.autoscroll {
                    Some((col, row)) => app.send_mouse("left", "drag", col, row),
                    None => {
                        app.autoscrolling = false;
                        return;
                    }
                }
            }
        });
    }

    /// Update the pointer shape on hover: a resize cursor over a draggable split
    /// separator or dock edge, the default arrow elsewhere. A no-op while a button
    /// is held (a drag in progress keeps its grab cursor) and only sets the
    /// surface's cursor when the shape changes, so an ordinary move over text costs
    /// nothing.
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
        if let Some(cursor) = self.cursor {
            cursor.set(match hint {
                Some(ResizeCursor::Col) => Cursor::System(SystemCursor::ResizeEastWest),
                Some(ResizeCursor::Row) => Cursor::System(SystemCursor::ResizeNorthSouth),
                None => Cursor::Default,
            });
        }
    }

    /// A pointer move while the left button is held: extend the Visual selection
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
        if let Some((col, row)) = self.pointer_cell() {
            self.send_mouse("left", "release", col, row);
        }
    }

    /// The mouse wheel. Every notch is forwarded to the server, which hit-tests it
    /// to the window — or the overlay (the completion popup, a picker) — under the
    /// pointer; nothing scrolls client-side. A trackpad's points accumulate into
    /// whole lines.
    fn mouse_wheel(&mut self, delta: ScrollDelta) {
        let Some(r) = self.renderer.as_ref() else {
            return;
        };
        let (cell_w, cell_h) = r.cell_size();
        let (ax, ay) = match delta {
            ScrollDelta::Lines { x, y } => (x, y),
            ScrollDelta::Points { x, y } => (x * self.scale / cell_w, y * self.scale / cell_h),
        };
        let hnotch = mouse::drain_notches(ax, &mut self.wheel_accum.0);
        let vnotch = mouse::drain_notches(ay, &mut self.wheel_accum.1);
        let Some((col, row)) = self.pointer_cell() else {
            return;
        };

        // Cap the per-event repeat so a flung trackpad can't flood the server.
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
