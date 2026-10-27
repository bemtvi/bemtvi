//! The client: one editor's state on the UI thread, with no toolkit. The host feeds it
//! what happens (input, a resize, the IO thread's wake, a timer coming due) and does
//! what it asks for ([`Effect`]s): repaint, set the pointer shape or the input method's
//! caret, show a file dialog, read the clipboard, close, pass a message on.
//!
//! The host's loop, whatever its toolkit:
//!
//! - **Wake** (the waker given to [`Client::new`], called from the IO thread): call
//!   [`Client::pump`] on the UI thread.
//! - **Timers:** sleep until [`Client::next_deadline`], then call [`Client::tick`]. The
//!   deadline moves after any call into the client, so re-read it after each.
//! - **Paint** on [`Effect::Redraw`]: call [`Client::paint`], and again every few
//!   milliseconds while it returns `true` (a scroll slide is running).
//! - After every call, drain [`Client::take_effects`] and carry them out.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use bemtvi_rpc::Rpc;
use bemtvi_view::{arm_scroll, encode_paste, ResizeCursor, ScrollAnim, View};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::render::{Renderer, ScrollFrame};
use crate::{
    caret_blink, mouse, spawn_io, GlyphOverflow, GuiConfig, InputEvent, IoParts, KeyCode,
    Modifiers, MouseButton, Point, Proxy, ScrollDelta, Session, SurfaceSize, UserEvent,
    CARET_BLINK,
};

/// How often a held-at-the-edge mouse drag re-sends itself to keep the buffer
/// auto-scrolling (≈25 lines/sec). The surface only reports a drag (a pointer move
/// while the button is held) on pointer motion, so without this repeat a
/// selection dragged to the edge and held still would stop scrolling; this paces
/// the continuous scroll the server does a line at a time per drag event.
const AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(40);

/// Whether a drag at grid `cell` (`(col, row)`, global, 0-based) sits in the edge
/// band that arms continuous auto-scroll, for a grid `size` (`(cols, rows)`) cells.
/// The top row and the bottom rows (the status line and the command row below the
/// text body) qualify, and so do the first and last columns, for a `nowrap` view
/// panning sideways; the server decides whether that actually scrolls the focused
/// window. Mirrors the TUI's `in_scroll_zone`.
fn in_scroll_zone((col, row): (u16, u16), (cols, rows): (u16, u16)) -> bool {
    row == 0 || row + 2 >= rows || col == 0 || col + 1 >= cols
}

/// How the client starts, besides its session.
#[derive(Clone, Debug)]
pub struct ClientOptions {
    /// The fonts, their size and how wide glyphs are fit. A config's `'guifont'` still
    /// wins.
    pub config: GuiConfig,
    /// A directory to open the file dialog at once the editor shows (what
    /// `bemtvi-gui somedir` does), instead of the server's listing of it. With no
    /// dialog to ask for (a daemon session, or `native_dialogs` off) it's `:e {dir}`.
    pub open_dir: Option<PathBuf>,
    /// Which config a later `:connect` session runs.
    pub config_source: bemtvi_server::ConfigSource,
    /// Whether `<CR>` over a `:e`/`:w`-family command line asks for the platform's file
    /// dialog (see [`bemtvi_core::dialog`]); never in a daemon session.
    pub native_dialogs: bool,
    /// macOS's Option policy for chords (see [`crate::encode_input`]).
    pub option_chords: bool,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            config: GuiConfig::default(),
            open_dir: None,
            config_source: bemtvi_server::ConfigSource::Local,
            native_dialogs: true,
            option_chords: cfg!(target_os = "macos"),
        }
    }
}

/// The pointer's shape over the surface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    /// The default arrow.
    #[default]
    Default,
    /// The text I-beam, over text a modeless session types into.
    Text,
    /// ↔, over a vertical split separator or dock edge.
    ResizeHorizontal,
    /// ↕, over a horizontal one.
    ResizeVertical,
}

/// A rectangle over the surface, in points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Names a request an [`Effect`] made, for its [`Client::answer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RequestId(u64);

/// The answer to a request an [`Effect`] made.
#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    /// A file dialog's file, or `None` when it was cancelled.
    Path(Option<PathBuf>),
    /// The clipboard's text, or `None` when it held none.
    Text(Option<String>),
}

/// What the client asks of its host.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Paint ([`Client::paint`]) soon. Any number of these before a paint mean one.
    Redraw,
    /// Show this pointer shape over the surface.
    Cursor(CursorShape),
    /// The caret the input method's candidate window opens by, or `None` when there's
    /// none to show (a picker owns the cursor).
    ImeArea(Option<PointRect>),
    /// Turn the platform's input methods on or off (`'imdisable'`).
    InputMethod(bool),
    /// Put the window's backdrop under the surface (an opacity below `100`) or take it
    /// away.
    Transparent(bool),
    /// Show the open dialog, at `start_dir` if given, and answer with
    /// [`Answer::Path`].
    OpenDialog {
        request: RequestId,
        title: String,
        start_dir: Option<PathBuf>,
    },
    /// Show the save dialog and answer with [`Answer::Path`].
    SaveDialog { request: RequestId, title: String },
    /// Read the clipboard's text and answer with [`Answer::Text`].
    ReadClipboard { request: RequestId },
    /// The editor quit (`:qa`, or its server went away). Once.
    Exit,
    /// A config or plugin called `btv.host.send(name, payload)`.
    Host { name: String, payload: Value },
}

/// What a request is for, until it's answered.
enum Pending {
    /// Run `<verb> <file>` with the picked file.
    Picked(String),
    /// Paste the clipboard.
    Paste,
}

/// The IO thread behind a client: stop it, and join it (with every server thread it
/// joins). Separate from the [`Client`] so a host can stop an editor it unmounts and
/// join it later.
pub struct IoThread {
    shutdown: Arc<tokio::sync::Notify>,
    thread: std::thread::JoinHandle<bool>,
}

impl IoThread {
    /// Tell the IO thread to drop its connection (the server then sees EOF and winds
    /// down), without waiting for it.
    pub fn stop(&self) {
        self.shutdown.notify_one();
    }

    /// Whether it ended, server threads joined: a [`IoThread::join`] now doesn't wait.
    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    /// Stop it and wait for it, and for every server thread; whether one panicked.
    pub fn join(self) -> bool {
        self.stop();
        self.thread.join().unwrap_or(true)
    }
}

/// The live connection and whether the editor quit, shared with the [`ClientHandle`]s.
struct Link {
    rpc: Rpc,
    exited: bool,
}

/// A handle on a client's editor that any thread can hold: run commands and Lua in it,
/// ask it to quit. It follows the session across a `:connect`.
#[derive(Clone)]
pub struct ClientHandle(Arc<Mutex<Link>>);

impl ClientHandle {
    /// The live session's connection, or an error once the editor quit.
    pub fn rpc(&self) -> Result<Rpc> {
        let link = self
            .0
            .lock()
            .map_err(|_| anyhow!("the editor's state is poisoned"))?;
        if link.exited {
            return Err(anyhow!("the editor has quit"));
        }
        Ok(link.rpc.clone())
    }

    /// Whether the editor quit.
    pub fn exited(&self) -> bool {
        self.0.lock().map_or(true, |link| link.exited)
    }

    /// Run an ex command in the editor, whatever mode it's in.
    pub fn command(&self, command: &str) -> Result<()> {
        self.rpc()?
            .notify("btv_command", vec![Value::from(command)]);
        Ok(())
    }

    /// Run Lua in the editor and resolve to what it returns (a chunk that raises
    /// resolves to the error). Values go into the chunk as text: the editor takes no
    /// arguments with it.
    pub fn exec_lua(
        &self,
        code: &str,
    ) -> impl std::future::Future<Output = Result<Value>> + Send + 'static {
        let rpc = self.rpc();
        let code = Value::from(code);
        async move {
            rpc?.request("nvim_exec_lua", vec![code, Value::Array(Vec::new())])
                .await
        }
    }

    /// Ask the editor to quit, as a window's close button does: `:qa`, which refuses
    /// with `E37` while a buffer is unsaved.
    pub fn request_quit(&self) -> Result<()> {
        self.command("qa")
    }

    /// Quit now, the way `:qall!` does: the exit sequence runs and the shada flush
    /// lands, and nothing is written that the user didn't write.
    pub fn quit_now(&self) -> Result<()> {
        self.command("qall!")
    }
}

/// One editor's client state on the UI thread. See the module docs for the host's
/// side of it.
pub struct Client {
    link: ClientHandle,
    rpc: Rpc,
    events: UnboundedReceiver<UserEvent>,
    effects: Vec<Effect>,
    /// Whether an [`Effect::Redraw`] is in `effects` already.
    redraw_queued: bool,
    renderer: Option<Renderer>,
    /// The grid a client with no renderer attached at ([`Client::attach_headless`]).
    headless_grid: Option<(u16, u16)>,
    view: View,
    /// The in-flight scroll slide, if any (keeps the paint loop going).
    scroll: Option<ScrollAnim>,
    /// Whether the editor may ask this client for a file dialog (`file_dialogs` at
    /// attach).
    native_dialogs: bool,
    option_chords: bool,
    /// Requests the host hasn't answered yet.
    pending: HashMap<RequestId, Pending>,
    next_request: u64,
    /// What the pointer shows and whether input methods are on, as last asked for, so
    /// a frame that doesn't change them asks for nothing.
    input_method_on: bool,
    transparent: bool,
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
    /// The cell the last hover over an open context menu was reported at, so a context menu
    /// highlights the row under the pointer with one `move` per cell, not per pixel.
    last_hover_cell: Option<(u16, u16)>,
    /// The resize cursor currently shown (over a draggable separator / dock edge),
    /// or `None` for the default arrow. Tracked so a hover only sets the cursor when
    /// the shape actually changes, not on every pointer move.
    resize_hint: Option<ResizeCursor>,
    /// Whether the pointer shows the text I-beam, over the text of a window a
    /// modeless session types into ([`bemtvi_view::WindowView::text_pointer`]).
    /// Tracked like [`Self::resize_hint`], so the shape is only set when it changes.
    text_hint: bool,
    /// When the modeless caret's blink last restarted ([`caret_blink`]): the caret
    /// moved, or a frame turned it on.
    blink_since: Instant,
    /// What the blink restarts on: whether the caret is up, and its place.
    blink_key: (bool, u16, u16),
    /// When the caret next flips, while there is one.
    blink_next: Instant,
    /// While the left button is held with the pointer in the top/bottom edge band,
    /// the cell to re-issue the drag at so the buffer keeps auto-scrolling without
    /// further pointer motion. `None` when not at an edge or the button is up.
    autoscroll: Option<(u16, u16)>,
    /// When the auto-scroll drag is re-sent next, while `autoscroll` is set.
    autoscroll_next: Instant,
    /// Fractional wheel remainder per axis `(horizontal, vertical)`, so a
    /// pixel-precise trackpad still scrolls one whole line at a time (see
    /// [`mouse::drain_notches`]).
    wheel_accum: (f32, f32),
    /// Last `(cols, windows_rows)` reported to the server, to suppress
    /// no-op resize notifications.
    reported: (u16, u16),
    /// When set, fire one `bemtvi_input_flush` once this instant passes with no
    /// further input — the GUI's `timeoutlen` timer. Armed by each keystroke and
    /// re-armed by the next, so it measures idle-since-last-key.
    flush_deadline: Option<Instant>,
    /// The last IME cursor rect `(x, y, w, h)` in pixels, so a frame that didn't move
    /// the caret leaves it alone.
    ime_area: Option<(f32, f32, f32, f32)>,
    /// Font config (CLI flags / environment) used as the renderer's startup default
    /// and the fallback for any field a `guifont` doesn't set.
    config: GuiConfig,
    /// The last `view.guifont` applied to the renderer, so a redraw only re-shapes
    /// when it actually changed (`:set guifont=…` / the init-time value).
    applied_guifont: String,
    /// The configured fonts last reported as not installed, so each list is reported
    /// once rather than on every re-apply (a DPI change re-applies the font).
    reported_missing: Vec<String>,
    /// The last `view.guiglyphoverflow` applied to the renderer, so a redraw only
    /// re-applies when it actually changed (`:set guiglyphoverflow=…` / the init-time
    /// value). Empty is both the initial state and the option's default — which means
    /// "keep what `--glyph-overflow` / the env set".
    applied_glyph_overflow: String,
    /// The last `(view.guiopacity, view.guihlopacity)` applied to the renderer, so a
    /// redraw only re-applies (and only re-reports a refusal) when one actually
    /// changed. `None` is both the initial state and a view no server has filled in
    /// yet: fully opaque.
    applied_opacity: (Option<u8>, Option<u8>),
    /// A directory passed on the command line (`bemtvi-gui somedir`), if any. Asked
    /// for as the native file picker (anchored there) once the editor is attached,
    /// instead of the server's in-window listing. Taken on first use so it fires
    /// exactly once (see [`Client::open_start_dir`]).
    open_dir: Option<PathBuf>,
    /// Whether this is an edit-host (daemon) session — buffers live on the daemon's
    /// fs, not the local disk. Suppresses the native open/save file dialogs, which would
    /// otherwise browse and write the wrong machine.
    /// Updated on a `:connect` swap (see [`UserEvent::Connected`]).
    remote: bool,
    /// Handed to the renderer's image store so it can request remote preview bytes over
    /// the editor RPC (a daemon session's image files aren't on local disk). The IO
    /// thread drains it and posts replies back as [`UserEvent::ImageBytes`].
    fetch_tx: tokio::sync::mpsc::UnboundedSender<crate::ImageFetch>,
    /// Handed to the renderer's image store so it can off-load preview decodes (a
    /// disk read + full-res decode must not block repaints). The IO thread drains
    /// it and posts outcomes back as [`UserEvent::ImageDecoded`].
    decode_tx: tokio::sync::mpsc::UnboundedSender<crate::DecodeReq>,
}

impl Client {
    /// The client for `session`: starts its IO thread and connects to it. `wake` is
    /// called from the IO thread whenever it queued something for [`Client::pump`].
    /// Returns the client and its IO thread, which the host stops (or joins) when it's
    /// done with the editor.
    pub fn new(
        session: Session,
        options: ClientOptions,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> (Client, IoThread) {
        let (tx, events) = tokio::sync::mpsc::unbounded_channel();
        let (rpc_tx, rpc_rx) = std::sync::mpsc::channel::<Rpc>();
        let (fetch_tx, fetch_rx) = tokio::sync::mpsc::unbounded_channel();
        let (decode_tx, decode_rx) = tokio::sync::mpsc::unbounded_channel();
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let remote = session.remote;
        let thread = spawn_io(IoParts {
            initial: session,
            config_source: options.config_source,
            proxy: Proxy::new(tx, wake),
            rpc_tx,
            fetch_rx,
            decode_rx,
            shutdown: shutdown.clone(),
        });
        // The first connection is up as soon as the thread runs (it's an in-process
        // duplex, or a daemon session already connected); only a thread that died first
        // fails this.
        let rpc = rpc_rx
            .recv()
            .expect("bemtvi-gpu: the client IO thread ended before it connected");
        let now = Instant::now();
        let client = Client {
            link: ClientHandle(Arc::new(Mutex::new(Link {
                rpc: rpc.clone(),
                exited: false,
            }))),
            rpc,
            events,
            effects: Vec::new(),
            redraw_queued: false,
            renderer: None,
            headless_grid: None,
            view: View::default(),
            scroll: None,
            native_dialogs: options.native_dialogs,
            option_chords: options.option_chords,
            pending: HashMap::new(),
            next_request: 0,
            input_method_on: true,
            transparent: false,
            mods: Modifiers::default(),
            scale: 1.0,
            cursor_px: (0.0, 0.0),
            mouse_down: false,
            last_drag_cell: None,
            last_hover_cell: None,
            resize_hint: None,
            text_hint: false,
            blink_since: now,
            blink_key: (false, 0, 0),
            blink_next: now + CARET_BLINK,
            autoscroll: None,
            autoscroll_next: now,
            wheel_accum: (0.0, 0.0),
            reported: (0, 0),
            flush_deadline: None,
            ime_area: None,
            config: options.config,
            applied_guifont: String::new(),
            reported_missing: Vec::new(),
            applied_glyph_overflow: String::new(),
            applied_opacity: (None, None),
            open_dir: options.open_dir,
            remote,
            fetch_tx,
            decode_tx,
        };
        (client, IoThread { shutdown, thread })
    }

    /// A handle on the editor for any thread.
    pub fn handle(&self) -> ClientHandle {
        self.link.clone()
    }

    /// The latest view the server sent.
    pub fn view(&self) -> &View {
        &self.view
    }

    /// Whether the editor quit.
    pub fn exited(&self) -> bool {
        self.link.exited()
    }

    /// Whether a renderer is drawing the editor ([`Client::attach_surface`]).
    pub fn has_renderer(&self) -> bool {
        self.renderer.is_some()
    }

    /// What the client asked for since the last call.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        self.redraw_queued = false;
        std::mem::take(&mut self.effects)
    }

    fn request_redraw(&mut self) {
        if !std::mem::replace(&mut self.redraw_queued, true) {
            self.effects.push(Effect::Redraw);
        }
    }

    fn request(&mut self, pending: Pending) -> RequestId {
        let id = RequestId(self.next_request);
        self.next_request += 1;
        self.pending.insert(id, pending);
        id
    }

    /// Render to `window` from now on, at `size`, and attach the editor at the grid it
    /// fits. A failure says why and quits the session (the server then answers with
    /// `bemtvi_exit`): with nothing to show, a blank window shouldn't stay up.
    pub fn attach_surface(
        &mut self,
        window: impl HasWindowHandle + HasDisplayHandle + Send + Sync + 'static,
        size: SurfaceSize,
    ) -> Result<()> {
        self.scale = if size.scale > 0.0 { size.scale } else { 1.0 };
        match Renderer::new(
            window,
            size,
            &self.config,
            self.fetch_tx.clone(),
            self.decode_tx.clone(),
        ) {
            Ok(r) => self.renderer = Some(r),
            Err(e) => {
                self.rpc.notify("btv_command", vec![Value::from("qall!")]);
                return Err(e.context("bemtvi-gpu: failed to init the renderer"));
            }
        }
        self.report_size(true);
        // A new surface has input methods on; turn them off if `'imdisable'` says so.
        self.update_input_method();
        // Apply any `guifont` already received before the renderer existed (a redraw
        // can beat the surface); a no-op for the default empty value.
        self.apply_guifont();
        // Same for `'guiglyphoverflow'`, which an `init.lua` will have set before the
        // first frame; empty leaves the renderer on the CLI/env mode it was built with.
        self.apply_glyph_overflow();
        // And the opacities (`'guiopacity'` / `'guihlopacity'`), set by an `init.lua` too.
        self.apply_opacity();
        self.request_redraw();
        self.open_start_dir();
        Ok(())
    }

    /// A directory given on the command line opens the native file picker there, once
    /// the editor is attached (taken so it fires once). Where `<CR>` on `:e {dir}`
    /// asks for no dialog — an edit-host session, whose files are on another machine
    /// than the dialog's, or dialogs turned off (see [`bemtvi_core::dialog`]) — it's
    /// `:e {dir}` as typed: the editor's own listing of it.
    fn open_start_dir(&mut self) {
        let Some(dir) = self.open_dir.take() else {
            return;
        };
        let dir = dir.to_string_lossy();
        if self.native_dialogs && !self.remote {
            self.pick_open_at("e", &dir);
        } else {
            let dir = bemtvi_core::ex_names::escape_file_arg(&dir);
            self.rpc
                .notify("btv_command", vec![Value::from(format!("e {dir}"))]);
        }
    }

    /// Stop rendering: the renderer and its GPU resources go now (an unmounted editor's
    /// surface is gone). The editor keeps running.
    pub fn detach_surface(&mut self) {
        // A button held as the surface goes will never be released on it: release it at
        // the last cell it reached, so the server ends the drag (a Visual selection being
        // extended, a split being resized) instead of waiting on a release that can't come.
        if self.mouse_down {
            if let Some((col, row)) = self.last_drag_cell {
                self.send_mouse("left", "release", col, row);
            }
        }
        self.renderer = None;
        // What the old surface held or was asked for goes with it: a button held on it
        // (or a drag's auto-scroll, which would keep sending drags from `tick`), and the
        // pointer shape, input-method caret, switch and backdrop it showed. A new surface
        // starts at the platform's defaults, so the next attach asks again from those.
        self.mouse_down = false;
        self.last_drag_cell = None;
        self.last_hover_cell = None;
        self.autoscroll = None;
        self.wheel_accum = (0.0, 0.0);
        self.resize_hint = None;
        self.text_hint = false;
        self.ime_area = None;
        self.input_method_on = true;
        self.transparent = false;
    }

    /// Attach the editor at a fixed grid with nothing to draw it on: a host that
    /// renders nothing (a test driving the editor through its keys and frames). The
    /// pointer has no cells to land on then, so mouse input does nothing.
    pub fn attach_headless(&mut self, cols: u16, rows: u16) {
        self.headless_grid = Some((cols, rows));
        self.report_size(true);
        self.update_input_method();
        self.open_start_dir();
    }

    /// The surface's size in pixels, or its scale, changed.
    pub fn resize(&mut self, size: SurfaceSize) {
        if size.is_empty() || self.renderer.is_none() {
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

    /// Handle what the IO thread queued (the host's waker said there is some).
    pub fn pump(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.user_event(event);
        }
    }

    /// When the client next needs [`Client::tick`]: the `timeoutlen` flush, a drag's
    /// auto-scroll repeat, the caret's next blink. `None` when nothing is due.
    pub fn next_deadline(&self) -> Option<Instant> {
        let blink = self.view.caret.then_some(self.blink_next);
        let autoscroll = self.autoscroll.map(|_| self.autoscroll_next);
        [self.flush_deadline, autoscroll, blink]
            .into_iter()
            .flatten()
            .min()
    }

    /// Do what came due by `now`.
    pub fn tick(&mut self, now: Instant) {
        // Idle `timeoutlen` reached: nudge the server to resolve any withheld mapped
        // prefix (harmless when nothing is pending), and disarm.
        if self.flush_deadline.is_some_and(|d| now >= d) {
            self.rpc.notify("bemtvi_input_flush", vec![]);
            self.flush_deadline = None;
        }
        // The server scrolls the focused window one line per drag it lands past the
        // text body and re-extends the selection.
        if let Some((col, row)) = self.autoscroll {
            if now >= self.autoscroll_next {
                self.send_mouse("left", "drag", col, row);
                self.autoscroll_next = now + AUTOSCROLL_INTERVAL;
            }
        }
        // The caret flips: repaint, and wait for the next flip.
        if self.view.caret && now >= self.blink_next {
            let left = caret_blink(now.saturating_duration_since(self.blink_since)).1;
            self.blink_next = now + left;
            self.request_redraw();
        }
    }

    /// Paint one frame; whether a slide is still running (paint again in a few
    /// milliseconds). A finished slide settles first, so the final frame is the live
    /// (destination) view rather than a clamped band.
    pub fn paint(&mut self) -> bool {
        if self.scroll.as_ref().is_some_and(ScrollAnim::done) {
            self.scroll = None;
        }
        let frame = self.scroll.as_ref().map(ScrollFrame::of);
        let hidden = self.view.caret && caret_blink(self.blink_since.elapsed()).0;
        if let Some(r) = self.renderer.as_mut() {
            r.set_caret_hidden(hidden);
            if let Err(e) = r.render(&self.view, frame.as_ref()) {
                eprintln!("bemtvi-gpu: render error: {e}");
            }
        }
        self.update_ime_area();
        self.scroll.is_some()
    }

    /// The answer to a request an [`Effect`] made. One the client doesn't know (already
    /// answered) is ignored.
    pub fn answer(&mut self, request: RequestId, answer: Answer) {
        match (self.pending.remove(&request), answer) {
            (Some(Pending::Picked(verb)), Answer::Path(picked)) => self.run_picked(&verb, picked),
            (Some(Pending::Paste), Answer::Text(text)) => self.paste(text.as_deref()),
            (Some(_), answer) => {
                eprintln!("bemtvi-gpu: {answer:?} doesn't answer request {request:?}")
            }
            (None, _) => {}
        }
    }

    /// Restart the caret's blink when this frame moved it, turned it on, or put it
    /// in another window, so a caret is never hidden right after it moves.
    fn restart_blink(&mut self) {
        let key = match self.view.focused() {
            Some(w) => (self.view.caret, w.cursor_row, w.cursor_screen_col),
            None => (self.view.caret, 0, 0),
        };
        if key != self.blink_key {
            self.blink_key = key;
            self.blink_since = Instant::now();
            self.blink_next = self.blink_since + CARET_BLINK;
        }
    }

    /// Something from the IO thread.
    fn user_event(&mut self, event: UserEvent) {
        match event {
            UserEvent::Redraw(view) => {
                self.view = *view;
                self.update_input_method();
                self.restart_blink();
                // A frame can change what is under a still pointer (a read-only
                // window took focus, `'modeless'` turned on).
                self.update_pointer();
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
                // A changed opacity repaints the backgrounds from this frame on.
                if (self.view.guiopacity, self.view.guihlopacity) != self.applied_opacity {
                    self.apply_opacity();
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
            // old one — the client's last clone, which lets the old connection wind
            // down), update remote-ness, and re-attach the UI. Clearing `reported` forces
            // `report_size` to send a fresh `btv_ui_attach`; resetting the view avoids
            // painting the old server's buffer until the new one's first redraw arrives.
            UserEvent::Connected { rpc, remote } => {
                self.rpc = *rpc;
                if let Ok(mut link) = self.link.0.lock() {
                    link.rpc = self.rpc.clone();
                }
                self.remote = remote;
                // A file dialog still open was asked for by the old session's `:e`/`:w`:
                // its pick (a path on this machine) must not run in the new session, whose
                // files may be on another, and whose command line it never touched. Its
                // answer then finds no request and is ignored. A clipboard paste is not
                // tied to the session, so it still lands.
                self.pending
                    .retain(|_, pending| matches!(pending, Pending::Paste));
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
                // Nor the opacities: opaque until the new server relays its values.
                self.apply_opacity();
                self.request_redraw();
            }
            // The editor quit (the IO thread says so on `bemtvi_exit`, and again as it
            // ends): the host hears of it once.
            UserEvent::Exit => {
                let first = self
                    .link
                    .0
                    .lock()
                    .map(|mut link| !std::mem::replace(&mut link.exited, true))
                    .unwrap_or(false);
                if first {
                    self.effects.push(Effect::Exit);
                }
            }
            UserEvent::Host { name, payload } => self.effects.push(Effect::Host { name, payload }),
            UserEvent::FileDialog {
                save,
                base,
                start_dir,
            } => self.file_dialog(save, base, start_dir),
        }
    }

    /// Keys, typed text and the pointer on the surface.
    pub fn input(&mut self, input: &InputEvent) {
        match input {
            InputEvent::PointerMoved {
                position,
                modifiers,
            } => {
                self.mods = *modifiers;
                self.cursor_px = self.pixels(*position);
                // While the left button is held, a move into a new cell is a drag.
                self.mouse_drag();
                self.mouse_hover();
                self.update_pointer();
            }
            InputEvent::Button {
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
                            (crate::button_name(*button), self.pointer_cell())
                        {
                            self.send_mouse(name, "press", col, row);
                        }
                    }
                    _ => {}
                }
            }
            InputEvent::Scroll { delta, modifiers } => {
                self.mods = *modifiers;
                self.mouse_wheel(*delta);
            }
            InputEvent::Key {
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
            InputEvent::Text(_) => self.send_keys(input),
            // An input method taking back what keys typed, to replace it (macOS's
            // accents replace the letter a held key typed): delete it in the buffer.
            InputEvent::DeleteSurrounding { .. } => self.send_keys(input),
            // An input method's composition in progress: the surface is ours to draw,
            // so the platform can't show it — paint it at the cursor until it's
            // committed (a `Text`) or cancelled (an empty one).
            InputEvent::Preedit { text, cursor } => {
                // A composition starting while a modeless wheel has the caret off
                // screen: it goes in at the caret, so bring the caret back into view.
                let hidden = self.view.focused().is_some_and(|w| w.cursor_hidden);
                if hidden && !text.is_empty() {
                    self.rpc.notify(
                        "nvim_exec_lua",
                        vec![
                            Value::from("btv.modeless.actions.reveal_caret()"),
                            Value::Array(vec![]),
                        ],
                    );
                }
                if let Some(r) = self.renderer.as_mut() {
                    r.set_preedit(text, *cursor);
                }
                self.request_redraw();
            }
            InputEvent::Key { pressed: false, .. } => {}
        }
    }

    /// A key went down (or repeats).
    fn key_press(
        &mut self,
        input: &InputEvent,
        code: KeyCode,
        unmodified: Option<char>,
        mods: Modifiers,
    ) {
        // Paste from the system clipboard (Cmd+V / Ctrl+Shift+V / Shift+Insert), fed
        // through the same `encode_paste` the TUI uses for terminal bracketed paste.
        // Claimed before key encoding so the gesture's base key isn't also typed.
        if crate::is_paste(code, unmodified, mods) {
            let request = self.request(Pending::Paste);
            self.effects.push(Effect::ReadClipboard { request });
            return;
        }
        self.send_keys(input);
    }

    /// Encode a key press or committed text and feed it to the server, arming the
    /// `timeoutlen` flush (the next key re-arms it, so it measures idle-since-last-key).
    fn send_keys(&mut self, input: &InputEvent) {
        if let Some(notation) = crate::encode_input(input, self.option_chords) {
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
        let (cols, total_rows) = match (&self.renderer, self.headless_grid) {
            (Some(renderer), _) => renderer.grid_size(),
            (None, Some(grid)) => grid,
            (None, None) => return,
        };
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
        // chosen one. `client` names it a GUI, which `'modeless'` is gated on. The map
        // is read only at attach; a resize passes an empty one.
        let caps = if attach {
            Value::Map(vec![
                (Value::from("keyboard_protocol"), Value::Boolean(true)),
                (Value::from("truecolor"), Value::Boolean(true)),
                (Value::from("client"), Value::from("gui")),
                // The OS, so a key layout can follow its conventions (Command on macOS).
                (Value::from("platform"), Value::from(std::env::consts::OS)),
                // Whether a submitted `:e`, `:eo`, `:wo`, … asks this client for a native
                // file dialog (see `bemtvi_core::dialog`); the editor also refuses it for
                // a session whose files are on another machine.
                (
                    Value::from("file_dialogs"),
                    Value::Boolean(self.native_dialogs && !self.remote),
                ),
            ])
        } else {
            Value::Map(vec![])
        };
        self.rpc
            .notify(method, vec![Value::from(cols), Value::from(win_rows), caps]);
    }

    /// Feed the clipboard's text to the server as one `btv_input` via [`encode_paste`]
    /// — the GUI analogue of the TUI's bracketed paste (one notify, one redraw, no
    /// per-character trickle). A missing, empty, or non-text clipboard is a silent no-op
    /// (nothing to paste).
    fn paste(&mut self, text: Option<&str>) {
        let notation = encode_paste(text.unwrap_or_default());
        if !notation.is_empty() {
            self.rpc
                .notify("btv_input", vec![Value::from(notation.as_str())]);
            self.arm_flush_deadline();
        }
    }

    /// Turn the platform's input methods off while `'imdisable'` is set, and back on
    /// as it's unset. A composition still showing as they go off is dropped.
    fn update_input_method(&mut self) {
        let on = !self.view.imdisable;
        if self.input_method_on == on {
            return;
        }
        self.input_method_on = on;
        self.effects.push(Effect::InputMethod(on));
        if !on {
            if let Some(r) = self.renderer.as_mut() {
                r.set_preedit("", None);
            }
        }
    }

    /// Ask for the just-painted caret rect as the surface's caret, so the platform opens
    /// the IME candidate window at the caret instead of the window origin. The renderer
    /// reports the rect in physical pixels (`None` when a picker/panel owns the
    /// cursor); the host takes points. Only a changed rect is asked for.
    fn update_ime_area(&mut self) {
        let area = self.renderer.as_ref().and_then(Renderer::ime_cursor_area);
        if area == self.ime_area {
            return;
        }
        self.ime_area = area;
        let scale = self.scale;
        self.effects
            .push(Effect::ImeArea(area.map(|(x, y, w, h)| PointRect {
                x: x / scale,
                y: y / scale,
                width: w / scale,
                height: h / scale,
            })));
    }

    /// Apply the relayed `view.guifont` to the renderer: parse the family and `:h`
    /// size, fall back to the CLI/env [`GuiConfig`] for any unset field, re-shape,
    /// and re-report the grid (the cell size changed). Called whenever `guifont`
    /// changes — including its first non-empty value from `init.lua` — so a
    /// `:set guifont=…` takes effect live. A no-op before the renderer exists; it's
    /// applied from [`Self::attach_surface`] once the renderer is built.
    fn apply_guifont(&mut self) {
        // The server parsed `'guifont'`; its fonts win, and with none set the CLI/env
        // list applies.
        let spec = &self.view.guifont_spec;
        let fonts = if spec.fonts.is_empty() {
            self.config.fonts.clone()
        } else {
            spec.fonts.clone()
        };
        let size = spec.size.unwrap_or(self.config.font_size);
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        renderer.set_font(&fonts, size);
        // A font in the list that isn't installed is skipped; say so, on the message
        // line, instead of drawing the next one without a word.
        let missing = renderer.missing_fonts().to_vec();
        if !missing.is_empty() && missing != self.reported_missing {
            let message = format!("guifont: not installed: {}", missing.join(", "));
            crate::io::notify_editor(&self.rpc, &message, "WARN");
        }
        self.reported_missing = missing;
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
                    "bemtvi-gpu: ignoring guiglyphoverflow={relayed:?} (expected {})",
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

    /// Apply the relayed `'guiopacity'` and `'guihlopacity'` to the renderer — how
    /// opaque the `Normal` background, and every other background, paint over the
    /// window's backdrop. `None` (no server value yet) is fully opaque. A surface that
    /// can't show its backdrop refuses anything below `100`, and that's reported in the
    /// message line as an error (as [`crate::report_session_error`] does), not left
    /// looking set while the window stays solid. Taken, a value below `100` asks for the
    /// backdrop under the surface (and `100` takes it away again: macOS's vibrancy
    /// isn't free while nothing shows it). A no-op before the renderer exists;
    /// [`Self::attach_surface`] applies it.
    ///
    /// Cheap — the next frame just paints differently — so it only repaints.
    fn apply_opacity(&mut self) {
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let (normal, hl) = (self.view.guiopacity, self.view.guihlopacity);
        self.applied_opacity = (normal, hl);
        let (normal, hl) = (normal.unwrap_or(100), hl.unwrap_or(100));
        let results = [renderer.set_opacity(normal), renderer.set_hl_opacity(hl)];
        let transparent = results.iter().all(Result::is_ok) && (normal < 100 || hl < 100);
        if transparent != self.transparent {
            self.transparent = transparent;
            self.effects.push(Effect::Transparent(transparent));
        }
        for e in results.into_iter().filter_map(Result::err) {
            crate::io::notify_editor(&self.rpc, &e, "ERROR");
        }
        self.request_redraw();
    }

    /// The editor asked for a file dialog: a submitted `:e`, `:eo`, `:wo`, … (see
    /// [`bemtvi_core::dialog`]), the open one in `start_dir` if it names one. Ask the host
    /// for it; its answer runs `<base> <file>` ([`Self::run_picked`]). The editor only
    /// asks when this client said it shows them (`file_dialogs` at attach) and the files
    /// are on this machine, so one asked of a client that doesn't is a fault, said so.
    fn file_dialog(&mut self, save: bool, base: String, start_dir: Option<PathBuf>) {
        if !self.native_dialogs || self.remote {
            crate::io::notify_editor(
                &self.rpc,
                "a file dialog was asked of a client that shows none here",
                "ERROR",
            );
            return;
        }
        let request = self.request(Pending::Picked(base));
        self.effects.push(if save {
            Effect::SaveDialog {
                request,
                title: "Save As".to_owned(),
            }
        } else {
            Effect::OpenDialog {
                request,
                title: "Open".to_owned(),
                start_dir,
            }
        });
    }

    /// Like the editor's open dialog, for `open_dir` at attach: the dialog anchored at
    /// `dir`, the picked file opened with `base`. A relative `dir` resolves against the
    /// process cwd.
    fn pick_open_at(&mut self, base: &str, dir: &str) {
        let dir = std::path::absolute(dir).unwrap_or_else(|_| PathBuf::from(dir));
        let request = self.request(Pending::Picked(base.to_owned()));
        self.effects.push(Effect::OpenDialog {
            request,
            title: "Open".to_owned(),
            start_dir: Some(dir),
        });
    }

    /// Run `<verb> <path>` for a picked file (nothing on cancel). The editor already ran
    /// the line that asked, so there is no half-typed command line left to abort.
    fn run_picked(&self, verb: &str, picked: Option<PathBuf>) {
        if let Some(path) = picked {
            let path = bemtvi_core::ex_names::escape_file_arg(&path.to_string_lossy());
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
    /// so it always measures idle-since-the-last-key with the current config. A
    /// `timeoutlen` past what an `Instant` holds (macOS's tops out at ~584 years) is
    /// never reached, like `notimeout`, rather than overflowing.
    fn arm_flush_deadline(&mut self) {
        self.flush_deadline = self
            .view
            .timeout
            .then(|| Instant::now().checked_add(Duration::from_millis(self.view.timeoutlen)))
            .flatten();
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
                Value::from(crate::mouse_modifier(self.mods)),
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
    /// armed when the cell sits in the screen's edge band, cleared otherwise. While
    /// it stays armed the drag is re-issued every [`AUTOSCROLL_INTERVAL`] (from
    /// [`Client::tick`]), so the buffer keeps scrolling even though a drag is only
    /// reported on pointer motion.
    fn arm_autoscroll(&mut self, cell: (u16, u16)) {
        let size = self.renderer.as_ref().map_or((0, 0), |r| r.grid_size());
        if !in_scroll_zone(cell, size) {
            self.autoscroll = None;
            return;
        }
        // Already repeating: the next repeat picks the new cell up.
        if self.autoscroll.replace(cell).is_none() {
            self.autoscroll_next = Instant::now() + AUTOSCROLL_INTERVAL;
        }
    }

    /// Update the pointer shape on hover: a resize cursor over a draggable split
    /// separator or dock edge, the text I-beam over text a modeless session types
    /// into, the default arrow elsewhere. A no-op while a button is held (a drag in
    /// progress keeps its grab cursor) and only asks for a shape when it changes, so
    /// an ordinary move over text costs nothing.
    fn update_pointer(&mut self) {
        if self.mouse_down {
            return;
        }
        let Some(r) = self.renderer.as_ref() else {
            return;
        };
        let (px, py) = self.cursor_px;
        let hint = r.resize_cursor_at(&self.view, px, py);
        let text = hint.is_none() && r.text_pointer_at(&self.view, px, py);
        if hint == self.resize_hint && text == self.text_hint {
            return;
        }
        self.resize_hint = hint;
        self.text_hint = text;
        self.effects.push(Effect::Cursor(match hint {
            Some(ResizeCursor::Col) => CursorShape::ResizeHorizontal,
            Some(ResizeCursor::Row) => CursorShape::ResizeVertical,
            None if text => CursorShape::Text,
            None => CursorShape::Default,
        }));
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

    /// The pointer moved with no button held: over an open context menu at the
    /// pointer, report each new cell as a `move` so the server can highlight the row
    /// under it. Nothing is sent while no such menu is open, so moving over the text
    /// or a picker costs no traffic.
    fn mouse_hover(&mut self) {
        let open = self.view.menu.as_ref().is_some_and(|m| m.pointer);
        if self.mouse_down || !open {
            self.last_hover_cell = None;
            return;
        }
        let Some(cell) = self.pointer_cell() else {
            return;
        };
        if self.last_hover_cell == Some(cell) {
            return;
        }
        self.last_hover_cell = Some(cell);
        self.send_mouse("move", "move", cell.0, cell.1);
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
