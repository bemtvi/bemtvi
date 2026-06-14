//! The terminal-job engine — a per-buffer [`vt100`] emulator that turns a child's
//! raw PTY output into the screen the editor mirrors.
//!
//! This is the **emulation** half of `:terminal`, and it is deliberately
//! transport-agnostic: it never touches a process or the network, only bytes. The
//! editor tick feeds it the child's output ([`EditHost::terminal_feed`]); it
//! decodes the escape sequences into a screen grid and projects that grid into the
//! terminal buffer's mirrored lines + cursor (via [`Editor::terminal_update`]). The
//! per-cell colors are read straight off the live grid at redraw (Phase 4).
//!
//! Because it is pure CPU (no PTY, no async), it compiles to wasm and is shared by
//! both builds — the native server feeds it bytes from a local PTY, the browser
//! feeds it bytes streamed from the daemon. The byte transport that *gets* those
//! bytes is the part that differs (Phase 3 native / Phase 7 web). See
//! `docs/plans/2026-06-14-terminal-in-buffer.md`.

use nxvim_core::{BufferId, Rgb, Style, TerminalOp};
use rmpv::Value;

use crate::redraw::StyleTable;
use crate::EditHost;

/// Scrollback limit, in rows — the neovim `'scrollback'` default. Both vt100's
/// internal scrollback and our captured-history cache are capped here; once a
/// terminal has scrolled this many rows the oldest fall off, matching neovim.
const SCROLLBACK_CAP: usize = 10_000;

/// The `vt100` callback sink: captures the things the screen model itself doesn't
/// store but a real terminal must act on — the child's window title (OSC), and the
/// **replies** to status/identity queries (`vt100` is a screen *model*, so it never
/// answers them; we must, or apps like fzf that send `\e[6n` stall waiting). The
/// emulator reads both back via [`vt100::Parser::callbacks_mut`] after each `process`.
#[derive(Default)]
struct TermSink {
    title: Option<String>,
    /// Bytes to write back to the child (cursor-position / device-attributes reports),
    /// drained and sent to the pty after the feed.
    replies: Vec<u8>,
}

impl vt100::Callbacks for TermSink {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).into_owned());
    }

    /// Answer the terminal queries a screen model can't — the same replies a real
    /// terminal emits automatically. Without these, inline TUIs (fzf, …) that probe
    /// the cursor position before drawing block until the next keystroke.
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let p0 = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (c, i1) {
            // Device Status Report.
            ('n', None) => match p0 {
                5 => self.replies.extend_from_slice(b"\x1b[0n"), // "terminal OK"
                6 => {
                    // Cursor Position Report — 1-based row;col.
                    let (row, col) = screen.cursor_position();
                    self.replies
                        .extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
                }
                _ => {}
            },
            // Primary Device Attributes: identify as a VT102.
            ('c', None) => self.replies.extend_from_slice(b"\x1b[?6c"),
            // Secondary Device Attributes.
            ('c', Some(b'>')) => self.replies.extend_from_slice(b"\x1b[>0;0;0c"),
            _ => {}
        }
    }
}

/// A scrollback row materialized for *browsing*: its display text and the coalesced
/// per-cell style runs. Built only while the user navigates terminal-normal mode
/// (never during live output), by reading vt100's retained scrollback — vt100 owns
/// the scrollback; this is a transient projection of it the redraw can read cheaply
/// (the highlight path takes `&self`, so it can't re-page vt100 itself).
#[derive(Clone)]
struct ScrollLine {
    text: String,
    /// `(start_col, end_col, style)` runs, already coalesced and default-filtered
    /// (same shape [`row_spans`] produces for live rows).
    spans: Vec<(u16, u16, Style)>,
}

/// A terminal buffer's vt100 emulator: the escape-sequence parser (which owns the
/// screen grid + its internal scrollback) plus the last projected size and view.
pub(crate) struct TermEmu {
    /// The vt100 parser + screen grid. Fed the child's PTY bytes; queried for the
    /// row text, cursor, per-cell colors, and (via its [`TermSink`]) window title +
    /// query replies. It also holds the scrollback ([`SCROLLBACK_CAP`] rows) — cheap
    /// to accumulate; we only *read* it back when the user browses.
    parser: vt100::Parser<TermSink>,
    /// The `(rows, cols)` the emulator was last sized to, so a redraw-time resize
    /// only re-sizes (and reprojects) when the window's text area actually changed.
    last_size: (u16, u16),
    /// The scrollback rows materialized for the current *browsing* projection, oldest
    /// first. Empty while output is live (the buffer mirrors only the screen, so a
    /// flood stays `O(screen)` per burst); populated from vt100 when the user enters
    /// terminal-normal to navigate history, and read by the color path.
    history: Vec<ScrollLine>,
    /// Whether the buffer currently includes the scrollback history (browsing view)
    /// rather than just the live screen. Tracks the projected view so a redraw can
    /// detect a mode flip and reproject.
    showing_history: bool,
}

impl TermEmu {
    fn new(rows: u16, cols: u16) -> Self {
        let (rows, cols) = (rows.max(1), cols.max(1));
        TermEmu {
            parser: vt100::Parser::new_with_callbacks(
                rows,
                cols,
                SCROLLBACK_CAP,
                TermSink::default(),
            ),
            last_size: (rows, cols),
            history: Vec::new(),
            showing_history: false,
        }
    }
}

impl EditHost {
    /// Create (or reset) the vt100 emulator for terminal buffer `buf`, sized
    /// `rows`×`cols`, and project its initial (blank) screen so the buffer shows the
    /// right number of rows immediately. Called when a `:terminal` spawns its PTY.
    pub fn terminal_open_emu(&mut self, buf: BufferId, rows: u16, cols: u16) {
        self.terminals.insert(buf, TermEmu::new(rows, cols));
        self.terminal_project(buf);
    }

    /// Feed `bytes` of the child's PTY output into `buf`'s emulator, then reproject
    /// the screen into the buffer. Any status/identity queries the child emitted are
    /// answered by writing the reply bytes back to it (the same path a keystroke
    /// takes), so apps that probe the terminal before drawing don't stall. A no-op if
    /// `buf` has no live emulator.
    pub fn terminal_feed(&mut self, buf: BufferId, bytes: &[u8]) {
        let replies = match self.terminals.get_mut(&buf) {
            Some(emu) => {
                emu.parser.process(bytes);
                std::mem::take(&mut emu.parser.callbacks_mut().replies)
            }
            None => return,
        };
        if !replies.is_empty() {
            self.editor.terminal_send(buf, replies);
        }
        self.terminal_project(buf);
    }

    /// Resize `buf`'s emulator to `rows`×`cols`, reprojecting on a real change.
    /// Returns whether the size changed, so the caller can also resize the PTY (the
    /// child needs the new winsize to reflow). A no-op (returns `false`) if `buf`
    /// has no live emulator or the size is unchanged.
    pub fn terminal_resize(&mut self, buf: BufferId, rows: u16, cols: u16) -> bool {
        let (rows, cols) = (rows.max(1), cols.max(1));
        let Some(emu) = self.terminals.get_mut(&buf) else {
            return false;
        };
        if emu.last_size == (rows, cols) {
            return false;
        }
        emu.parser.screen_mut().set_size(rows, cols);
        emu.last_size = (rows, cols);
        self.terminal_project(buf);
        true
    }

    /// Drop `buf`'s emulator — the terminal closed, or the buffer was wiped.
    pub fn terminal_remove(&mut self, buf: BufferId) {
        self.terminals.remove(&buf);
    }

    /// Project `buf`'s emulator into the buffer's mirrored lines + cursor.
    ///
    /// While output is **live** (the user is in terminal-job mode on this buffer)
    /// the buffer mirrors only the visible screen, so each PTY burst costs
    /// `O(screen)` no matter how much has scrolled past — a flood (`rg` printing
    /// 500k matches) can never make this `O(history)`. vt100 keeps the scrollback
    /// internally (cheap). When the user **browses** (terminal-normal), the
    /// scrollback is materialized from vt100 into `history` and projected ahead of
    /// the screen, so `gg`/`G`/search/yank traverse it; the cursor offset by the
    /// history length keeps the live input position correct.
    fn terminal_project(&mut self, buf: BufferId) {
        let browsing = self.terminal_browsing(buf);
        let Some(emu) = self.terminals.get_mut(&buf) else {
            return;
        };
        let (rows, cols) = emu.parser.screen().size();

        // Materialize (or clear) the scrollback history for the current view. Reading
        // it back out of vt100 is only done here, when browsing — never on the hot
        // live-output path.
        if browsing {
            let screen = emu.parser.screen_mut();
            screen.set_scrollback(usize::MAX);
            let held = screen.scrollback();
            emu.history = read_scrollback(screen, held, 0, held, rows, cols);
            // `read_scrollback` left the view-window offset paged into history; reset
            // to the live view so the screen rows below read the live bottom.
            emu.parser.screen_mut().set_scrollback(0);
        } else {
            emu.history.clear();
        }
        emu.showing_history = browsing;

        let screen = emu.parser.screen();
        let mut lines: Vec<String> = emu.history.iter().map(|l| l.text.clone()).collect();
        lines.extend(screen.rows(0, cols));
        let (cy, cx) = screen.cursor_position();
        let cursor_row = emu.history.len() + cy as usize;
        let title = emu.parser.callbacks().title.clone();

        self.editor
            .terminal_update(buf, &lines, cursor_row, cx as usize);
        if let Some(title) = title {
            self.editor.terminal_set_title(buf, &title);
        }
    }

    /// Whether terminal buffer `buf` should show its scrollback history (browsing)
    /// rather than just the live screen. True unless it is the focused buffer in
    /// terminal-job mode — i.e. the user has left terminal-insert (`<C-\><C-n>`) to
    /// navigate, or is looking at a background terminal. While `false` the projection
    /// pins to the live bottom, keeping floods cheap.
    fn terminal_browsing(&self, buf: BufferId) -> bool {
        !(buf == self.editor.current_buffer_id() && self.editor.mode == nxvim_core::Mode::Terminal)
    }

    /// Reproject the focused terminal when its view should flip between live (pinned
    /// to the bottom) and browsing (scrollback materialized) — e.g. on `<C-\><C-n>` /
    /// `i`, which change the mode without any PTY output to trigger a projection.
    /// Called each redraw; a no-op when the view is already correct.
    pub(crate) fn sync_terminal_view(&mut self) {
        let buf = self.editor.current_buffer_id();
        let Some(emu) = self.terminals.get(&buf) else {
            return;
        };
        if emu.showing_history != self.terminal_browsing(buf) {
            self.terminal_project(buf);
        }
    }

    /// Project terminal buffer `buf`'s grid colors into a redraw `highlights`
    /// payload — the Phase 4 color path. Returns `None` when `buf` is not a live
    /// terminal, so the caller falls through to the treesitter projection.
    ///
    /// Each screen row's cells become coalesced spans `[start_col, end_col,
    /// group, style_id]` in **screen columns**, the exact shape
    /// [`highlights_for`](crate::EditHost::highlights_for) emits — so every
    /// client paints terminal color through its existing styling path with no
    /// wire change, the `style_id` indexing the shared per-frame `styles`
    /// palette. A cell column is a display column (a wide glyph and its
    /// continuation cell share one run), so the columns line up with the
    /// projected row text. Cells with the terminal's default look (no color, no
    /// attrs) emit no span, falling back to the client's base.
    pub(crate) fn terminal_highlights(
        &self,
        buf: BufferId,
        numbers: &[Option<usize>],
        styles: &mut StyleTable,
    ) -> Option<Value> {
        let emu = self.terminals.get(&buf)?;
        let screen = emu.parser.screen();
        let (rows, cols) = screen.size();
        let hist_len = emu.history.len();
        let out = numbers
            .iter()
            .map(|num| {
                // `numbers` are 1-based buffer lines. Buffer line idx maps to a
                // captured-history row (idx < hist_len) or, beyond that, a live
                // screen row (idx - hist_len) — the same split `terminal_project`
                // lays the buffer out in.
                let Some(idx) = num.map(|n| n - 1) else {
                    return Value::Array(Vec::new());
                };
                let runs = if idx < hist_len {
                    emu.history[idx].spans.clone()
                } else {
                    match u16::try_from(idx - hist_len) {
                        Ok(row) if row < rows => row_spans(screen, row, cols),
                        _ => return Value::Array(Vec::new()),
                    }
                };
                let mut spans: Vec<Value> = Vec::new();
                for (start, end, style) in runs {
                    push_span(&mut spans, start, end, style, styles);
                }
                Value::Array(spans)
            })
            .collect();
        Some(Value::Array(out))
    }
}

/// Read scrollback rows `[from, to)` (absolute indices, oldest = 0) out of vt100's
/// view-window, returning them oldest-first with text + coalesced style runs.
/// `held` is the current scrollback length. vt100 only shows a `rows`-tall window
/// at a time, so a range wider than the screen is read in pages; at scrollback
/// offset `k` the window's row 0 is scrollback row `held - k`. The caller restores
/// the offset to the live view afterward.
fn read_scrollback(
    screen: &mut vt100::Screen,
    held: usize,
    from: usize,
    to: usize,
    rows: u16,
    cols: u16,
) -> Vec<ScrollLine> {
    let mut out = Vec::with_capacity(to.saturating_sub(from));
    let mut idx = from;
    while idx < to {
        let k = held - idx; // offset that places scrollback row `idx` at window row 0
        screen.set_scrollback(k);
        // The window shows min(k, rows) scrollback rows before the live screen; take
        // only as many as remain in the requested range.
        let take = k.min(rows as usize).min(to - idx);
        let texts: Vec<String> = screen.rows(0, cols).collect();
        for r in 0..take {
            out.push(ScrollLine {
                text: texts.get(r).cloned().unwrap_or_default(),
                spans: row_spans(screen, r as u16, cols),
            });
        }
        idx += take;
    }
    out
}

/// Coalesce one grid row's cells into `(start_col, end_col, style)` runs in screen
/// columns, dropping default-look runs (so blank cells fall back to the client's
/// base). A wide glyph's continuation column inherits its lead cell's style, so the
/// runs line up with the projected row text. Shared by the live color path and the
/// scrollback capture, reading whichever row the current view-window exposes.
fn row_spans(screen: &vt100::Screen, row: u16, cols: u16) -> Vec<(u16, u16, Style)> {
    let mut runs = Vec::new();
    let mut run: Option<(u16, Style)> = None;
    let mut carry = Style::default();
    let flush = |runs: &mut Vec<(u16, u16, Style)>, start: u16, end: u16, style: Style| {
        if style != Style::default() {
            runs.push((start, end, style));
        }
    };
    for col in 0..cols {
        let style = match screen.cell(row, col) {
            Some(cell) if cell.is_wide_continuation() => carry.clone(),
            Some(cell) => {
                carry = cell_style(cell);
                carry.clone()
            }
            None => Style::default(),
        };
        if !matches!(&run, Some((_, prev)) if *prev == style) {
            if let Some((start, prev)) = run.take() {
                flush(&mut runs, start, col, prev);
            }
            run = Some((col, style));
        }
    }
    if let Some((start, prev)) = run.take() {
        flush(&mut runs, start, cols, prev);
    }
    runs
}

/// Push one coalesced cell run as a `[start, end, group, style_id]` highlight
/// span, interning its style into the frame palette. The terminal's default look
/// (an empty [`Style`]) emits nothing, so blank cells fall back to the client's
/// base rendering instead of bloating the palette.
fn push_span(spans: &mut Vec<Value>, start: u16, end: u16, style: Style, styles: &mut StyleTable) {
    if style == Style::default() {
        return;
    }
    let id = styles.intern(style);
    spans.push(Value::Array(vec![
        Value::from(start as u64),
        Value::from(end as u64),
        Value::from("Terminal"),
        Value::from(id as u64),
    ]));
}

/// One vt100 [`Cell`](vt100::Cell)'s look as a resolved [`Style`]: its fg/bg
/// colors projected to truecolor and its on/off attributes mapped across. The
/// terminal's *default* fg/bg become `None` so the client paints them with its
/// own base colors (matching neovim, where uncolored terminal text uses
/// `Normal`); only explicitly-set cell colors carry through.
fn cell_style(cell: &vt100::Cell) -> Style {
    Style {
        fg: ansi_rgb(cell.fgcolor()),
        bg: ansi_rgb(cell.bgcolor()),
        sp: None,
        bold: cell.bold(),
        italic: cell.italic(),
        underline: cell.underline(),
        undercurl: false,
        strikethrough: false,
        reverse: cell.inverse(),
    }
}

/// Project a vt100 [`Color`](vt100::Color) to truecolor. `Default` is `None` (the
/// client's base color); `Rgb` passes through; an indexed color resolves through
/// the standard xterm 256-color palette — the 16 ANSI colors, the 6×6×6 color
/// cube (16–231), and the 24-step grayscale ramp (232–255).
fn ansi_rgb(color: vt100::Color) -> Option<Rgb> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Rgb(r, g, b) => Some(Rgb { r, g, b }),
        vt100::Color::Idx(i) => Some(match i {
            // The 16 base ANSI colors, xterm's canonical values.
            0..=15 => {
                const ANSI16: [(u8, u8, u8); 16] = [
                    (0, 0, 0),
                    (205, 0, 0),
                    (0, 205, 0),
                    (205, 205, 0),
                    (0, 0, 238),
                    (205, 0, 205),
                    (0, 205, 205),
                    (229, 229, 229),
                    (127, 127, 127),
                    (255, 0, 0),
                    (0, 255, 0),
                    (255, 255, 0),
                    (92, 92, 255),
                    (255, 0, 255),
                    (0, 255, 255),
                    (255, 255, 255),
                ];
                let (r, g, b) = ANSI16[i as usize];
                Rgb { r, g, b }
            }
            // The 6×6×6 cube: each axis steps 0, 95, 135, 175, 215, 255.
            16..=231 => {
                const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
                let n = i - 16;
                Rgb {
                    r: LEVELS[(n / 36) as usize],
                    g: LEVELS[((n / 6) % 6) as usize],
                    b: LEVELS[(n % 6) as usize],
                }
            }
            // The grayscale ramp: 8 + 10·k for k in 0..24 (24, 34, … 238).
            232..=255 => {
                let v = 8 + 10 * (i - 232);
                Rgb { r: v, g: v, b: v }
            }
        }),
    }
}

/// Drain core's terminal ops (`Editor::take_pending_terminal`) and route them to the
/// transport — the bridge between the pure-core queue and the PTY. Native spawns /
/// writes / kills a real local PTY; the wasm build (until Phase 7 wires the daemon
/// leg) has no transport, so it fails the open *loud* rather than silently dropping
/// it. Called from the settle path each tick.
#[cfg(feature = "native")]
impl EditHost {
    pub(crate) fn dispatch_terminal_ops(&mut self, ops: Vec<TerminalOp>) {
        for op in ops {
            match op {
                TerminalOp::Open {
                    buf,
                    argv,
                    cwd,
                    rows,
                    cols,
                } => {
                    // Build the emulator first so the very next redraw projects the
                    // (blank) screen at the right size, then spawn the PTY behind it.
                    self.terminal_open_emu(buf, rows, cols);
                    // `portable-pty` defaults a `None` cwd to `$HOME`; the shell should
                    // instead open in the editor's working directory, so resolve it here
                    // (the server owns process I/O — core stays pure).
                    let cwd = cwd.or_else(|| {
                        std::env::current_dir()
                            .ok()
                            .map(|p| p.to_string_lossy().into_owned())
                    });
                    self.fx.terminal_command(native::TermCommand::Open {
                        buf,
                        argv,
                        cwd,
                        rows,
                        cols,
                    });
                }
                TerminalOp::Send { buf, bytes } => {
                    self.fx
                        .terminal_command(native::TermCommand::Write { buf, bytes });
                }
                TerminalOp::Kill { buf } => {
                    self.terminal_remove(buf);
                    self.fx.terminal_command(native::TermCommand::Kill { buf });
                }
            }
        }
    }

    /// Keep the current terminal's PTY winsize matching its window text area. Called
    /// each redraw: when the focused window shows a terminal and its text rect
    /// changed (a UI resize, a `<C-w>` resize), reflow the emulator and resize the
    /// child's PTY so it re-lays-out. (Terminals in unfocused splits are resized when
    /// next focused — a follow-up.)
    pub(crate) fn sync_terminal_sizes(&mut self) {
        let buf = self.editor.current_buffer_id();
        if !self.terminals.contains_key(&buf) {
            return;
        }
        let (rows, cols) = self.editor.current_text_area();
        if self.terminal_resize(buf, rows, cols) {
            self.fx
                .terminal_command(native::TermCommand::Resize { buf, rows, cols });
        }
    }
}

/// No terminal transport in the serverless browser build yet — Phase 7 wires the
/// daemon PTY over WebTransport. Until then an open fails *loud* (no silent stub).
#[cfg(not(feature = "native"))]
impl EditHost {
    pub(crate) fn dispatch_terminal_ops(&mut self, ops: Vec<TerminalOp>) {
        for op in ops {
            if let TerminalOp::Open { buf, .. } = op {
                self.editor.terminal_closed(buf, -1);
                self.editor
                    .echo("E: :terminal requires a daemon connection in this build".to_string());
            }
            // Send / Kill for a terminal that never opened are no-ops.
        }
    }
}

/// The native PTY transport: a `Send` actor (modeled on
/// [`EventLoop`](crate::evloop::EventLoop)) that owns the real local pseudo-terminals
/// and streams their output back to the editor thread. The editor tick fires
/// fire-and-forget [`TermCommand`]s at it; their output / exit return as
/// [`TermEvent`]s on the run loop's `select!`. The vt100 emulation lives in
/// [`EditHost`] (above), so this layer only moves bytes.
#[cfg(feature = "native")]
pub(crate) mod native {
    use std::collections::HashMap;
    use std::io::{Read, Write};

    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

    use nxvim_core::BufferId;

    /// A command from the editor tick to the terminal actor. Fire-and-forget, like
    /// [`LoopCommand`](crate::evloop::LoopCommand): the editor never awaits a reply.
    pub enum TermCommand {
        /// Spawn a PTY child for `buf`, sized `rows`×`cols`, running `argv` in `cwd`
        /// (empty `argv` ⇒ the default shell, `cwd` `None` ⇒ inherit).
        Open {
            buf: BufferId,
            argv: Vec<String>,
            cwd: Option<String>,
            rows: u16,
            cols: u16,
        },
        /// Write input bytes to `buf`'s PTY (a forwarded keystroke / paste).
        Write { buf: BufferId, bytes: Vec<u8> },
        /// Resize `buf`'s PTY so the child re-lays-out (window resize).
        Resize { buf: BufferId, rows: u16, cols: u16 },
        /// Kill `buf`'s child and forget the session.
        Kill { buf: BufferId },
    }

    /// An event from the terminal actor back to the editor thread, delivered to the
    /// run loop's `select!`. The matching [`EditHost`](crate::EditHost) handler feeds
    /// the bytes to the buffer's emulator / records the exit on the one server thread.
    #[derive(Debug)]
    pub enum TermEvent {
        /// `buf`'s child wrote output (raw PTY bytes — fed to the vt100 emulator).
        Data { buf: BufferId, bytes: Vec<u8> },
        /// `buf`'s child exited with `code` (`-1` on a spawn failure or a kill).
        Exit { buf: BufferId, code: i32 },
    }

    /// Handle the server holds to drive the terminal actor. Cheap to construct; the
    /// actor task is spawned lazily on the first [`send`](TerminalManager::send), so a
    /// session that never opens a terminal spawns nothing (the [`EventLoop`] pattern).
    ///
    /// [`EventLoop`]: crate::evloop::EventLoop
    pub struct TerminalManager {
        cmd_tx: UnboundedSender<TermCommand>,
        start: Option<(UnboundedReceiver<TermCommand>, UnboundedSender<TermEvent>)>,
        started: bool,
    }

    impl TerminalManager {
        /// Create the manager and the receiver the run loop selects on. No task is
        /// spawned until the first [`send`](Self::send).
        pub fn new() -> (TerminalManager, UnboundedReceiver<TermEvent>) {
            let (cmd_tx, cmd_rx) = unbounded_channel();
            let (event_tx, event_rx) = unbounded_channel();
            let mgr = TerminalManager {
                cmd_tx,
                start: Some((cmd_rx, event_tx)),
                started: false,
            };
            (mgr, event_rx)
        }

        fn ensure_started(&mut self) {
            if self.started {
                return;
            }
            if let Some((cmd_rx, event_tx)) = self.start.take() {
                tokio::spawn(run_terminal_actor(cmd_rx, event_tx));
                self.started = true;
            }
        }

        /// Fire-and-forget a command at the actor, starting it on first use.
        pub fn send(&mut self, cmd: TermCommand) {
            self.ensure_started();
            let _ = self.cmd_tx.send(cmd);
        }
    }

    /// A live PTY: the master's writer (input), the master itself (resize), and a
    /// killer cloned off the child (the child itself is moved into its wait thread).
    struct Session {
        writer: Box<dyn Write + Send>,
        master: Box<dyn portable_pty::MasterPty + Send>,
        killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    }

    /// The actor's run loop: own the live PTYs, service commands, and let each PTY's
    /// reader / waiter threads send output / exit back. Ends when the server drops the
    /// command sender (shutdown).
    async fn run_terminal_actor(
        mut cmd_rx: UnboundedReceiver<TermCommand>,
        event_tx: UnboundedSender<TermEvent>,
    ) {
        let mut sessions: HashMap<BufferId, Session> = HashMap::new();
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                TermCommand::Open {
                    buf,
                    argv,
                    cwd,
                    rows,
                    cols,
                } => {
                    match open_pty(buf, argv, cwd, rows, cols, &event_tx) {
                        Ok(session) => {
                            // Re-opening an id replaces any prior session (its child
                            // is dropped → killed).
                            sessions.insert(buf, session);
                        }
                        Err(e) => {
                            // Surface the failure in the buffer, then end the job —
                            // never a silent drop.
                            let _ = event_tx.send(TermEvent::Data {
                                buf,
                                bytes: format!("nxvim: {e}\r\n").into_bytes(),
                            });
                            let _ = event_tx.send(TermEvent::Exit { buf, code: -1 });
                        }
                    }
                }
                TermCommand::Write { buf, bytes } => {
                    if let Some(s) = sessions.get_mut(&buf) {
                        let _ = s.writer.write_all(&bytes);
                        let _ = s.writer.flush();
                    }
                }
                TermCommand::Resize { buf, rows, cols } => {
                    if let Some(s) = sessions.get(&buf) {
                        let _ = s.master.resize(PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                }
                TermCommand::Kill { buf } => {
                    if let Some(mut s) = sessions.remove(&buf) {
                        let _ = s.killer.kill();
                    }
                }
            }
        }
    }

    /// Spawn one PTY child for `buf` and wire its I/O: a reader thread streams output
    /// as [`TermEvent::Data`], a waiter thread reports the exit as [`TermEvent::Exit`].
    /// Both are plain OS threads (portable-pty's reader/wait are blocking); they end
    /// when the child exits (EOF on the master) or the event channel closes.
    fn open_pty(
        buf: BufferId,
        argv: Vec<String>,
        cwd: Option<String>,
        rows: u16,
        cols: u16,
        event_tx: &UnboundedSender<TermEvent>,
    ) -> anyhow::Result<Session> {
        let pair = native_pty_system().openpty(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut builder = match argv.split_first() {
            Some((program, args)) => {
                let mut b = CommandBuilder::new(program);
                b.args(args);
                b
            }
            None => CommandBuilder::new_default_prog(),
        };
        if let Some(dir) = cwd {
            builder.cwd(dir);
        }
        let child = pair.slave.spawn_command(builder)?;
        // Drop the slave handle so the child is the only writer to the pty — once it
        // exits, the master read returns EOF and the reader thread ends.
        drop(pair.slave);
        let killer = child.clone_killer();
        let writer = pair.master.take_writer()?;
        let mut reader = pair.master.try_clone_reader()?;

        let data_tx = event_tx.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if data_tx
                            .send(TermEvent::Data {
                                buf,
                                bytes: chunk[..n].to_vec(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });

        let exit_tx = event_tx.clone();
        let mut child = child;
        std::thread::spawn(move || {
            let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
            let _ = exit_tx.send(TermEvent::Exit { buf, code });
        });

        Ok(Session {
            writer,
            master: pair.master,
            killer,
        })
    }
}
