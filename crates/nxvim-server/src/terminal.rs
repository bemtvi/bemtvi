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

/// A terminal buffer's vt100 emulator: the escape-sequence parser (which owns the
/// screen grid + its internal scrollback) plus the materialized history mirror.
pub(crate) struct TermEmu {
    /// The vt100 parser + screen grid. Fed the child's PTY bytes; queried for the
    /// row text, cursor, per-cell colors, and (via its [`TermSink`]) window title +
    /// query replies. It also holds the scrollback (the `'scrollback'` cap, rows).
    parser: vt100::Parser<TermSink>,
    /// The `(rows, cols)` the emulator was last sized to, so a redraw-time resize
    /// only re-sizes (and reprojects) when the window's text area actually changed.
    last_size: (u16, u16),
    /// The scrollback rows' **text**, oldest first — the buffer's leading lines
    /// (before the live screen). The buffer always mirrors `history ++ screen`, so
    /// line numbers and the cursor are stable across mode changes (like neovim). Text
    /// is cheap to keep current every frame; per-cell color is *not* (see below).
    history: Vec<String>,
    /// Per-cell color runs for the history rows, index-aligned with `history` — but
    /// materialized **only while browsing** (terminal-normal). Coloring 10k+ scrolled
    /// rows on every frame of a live flood is prohibitively expensive, and the live
    /// screen (always colored, Phase 4) is what's on screen then anyway. So during
    /// live output this stays empty (history renders monochrome) and is filled lazily
    /// when the user leaves terminal-insert to read history. Empty ⇒ not materialized.
    history_styles: Vec<Vec<(u16, u16, Style)>>,
    /// vt100's scrollback length at the last projection. The history mirror is
    /// re-read from vt100 only when this changes (something scrolled); an unchanged
    /// length means only the live screen moved, so we rewrite just that region.
    last_held: usize,
    /// The `'scrollback'` cap this emulator was created with — kept so an interrupt
    /// trim can re-seed a fresh parser with the same cap (see [`EditHost::terminal_trim`]).
    scrollback: usize,
    /// Newlines fed since the last user keystroke went to the child — the flood
    /// signal a `^C` consults: a command that dumped many lines since you last typed
    /// is "actively flooding", so `^C` trims the scrollback to the recent tail. Reset
    /// to 0 on every input (so steady typing never reads as a flood).
    lines_since_input: usize,
}

impl TermEmu {
    fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        let (rows, cols) = (rows.max(1), cols.max(1));
        TermEmu {
            parser: vt100::Parser::new_with_callbacks(rows, cols, scrollback, TermSink::default()),
            last_size: (rows, cols),
            history: Vec::new(),
            history_styles: Vec::new(),
            // usize::MAX forces the first projection to read the (empty) scrollback
            // and lay out the full buffer.
            last_held: usize::MAX,
            scrollback,
            lines_since_input: 0,
        }
    }
}

/// How many recent lines an interrupt (`^C`) keeps when it trims a flooding
/// terminal's scrollback — and, reused as the flood threshold, how many lines must
/// have streamed since the last keystroke for a `^C` to trigger the trim at all.
const TERM_TRIM_KEEP: usize = 200;

impl EditHost {
    /// Create (or reset) the vt100 emulator for terminal buffer `buf`, sized
    /// `rows`×`cols`, and project its initial (blank) screen so the buffer shows the
    /// right number of rows immediately. Called when a `:terminal` spawns its PTY.
    pub fn terminal_open_emu(&mut self, buf: BufferId, rows: u16, cols: u16, scrollback: usize) {
        self.terminals
            .insert(buf, TermEmu::new(rows, cols, scrollback));
        self.terminal_project(buf);
    }

    /// Feed `bytes` of the child's PTY output into `buf`'s emulator and answer any
    /// status/identity queries (so apps probing the terminal don't stall). Does **not**
    /// project — the caller batches feeds and projects once per repaint
    /// ([`EditHost::terminal_project`]), so a flood costs one projection per frame, not
    /// one per PTY chunk. A no-op if `buf` has no live emulator.
    pub fn terminal_feed(&mut self, buf: BufferId, bytes: &[u8]) {
        let replies = match self.terminals.get_mut(&buf) {
            Some(emu) => {
                emu.parser.process(bytes);
                // Track output volume since the last keystroke (the `^C` flood signal).
                emu.lines_since_input += bytes.iter().filter(|&&b| b == b'\n').count();
                std::mem::take(&mut emu.parser.callbacks_mut().replies)
            }
            None => return,
        };
        if !replies.is_empty() {
            self.editor.terminal_send(buf, replies);
        }
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
        // A reflow can rewrite the scrollback; force a full re-read on the next project.
        emu.last_held = usize::MAX;
        self.terminal_project(buf);
        true
    }

    /// Drop `buf`'s emulator — the terminal closed, or the buffer was wiped.
    pub fn terminal_remove(&mut self, buf: BufferId) {
        self.terminals.remove(&buf);
    }

    /// React to a keystroke about to be sent to `buf`'s child. A bare `^C` (the
    /// single byte `0x03`) on a terminal that has been *actively flooding* — at least
    /// [`TERM_TRIM_KEEP`] lines streamed since the last keystroke — trims the
    /// scrollback to the recent tail with a marker ([`terminal_trim`]), so cancelling
    /// a runaway command leaves a readable buffer instead of thousands of lines. A
    /// `^C` at an idle prompt (nothing flooded) is left alone, so normal shell use
    /// keeps its full scrollback. Either way the flood counter resets — the next
    /// command's output is measured from here.
    ///
    /// Returns whether it trimmed — the daemon/edit-host uses that as the signal to also
    /// discard the child's in-flight backlog (the browser leg can't otherwise stop a flood
    /// promptly; see the Worker's `discardingTerms`).
    ///
    /// [`terminal_trim`]: EditHost::terminal_trim
    pub(crate) fn terminal_on_input(&mut self, buf: BufferId, bytes: &[u8]) -> bool {
        let flooding = match self.terminals.get(&buf) {
            Some(emu) => emu.lines_since_input >= TERM_TRIM_KEEP,
            None => return false,
        };
        let trimmed = bytes == [0x03] && flooding;
        if trimmed {
            self.terminal_trim(buf);
        }
        if let Some(emu) = self.terminals.get_mut(&buf) {
            emu.lines_since_input = 0;
        }
        trimmed
    }

    /// Trim `buf`'s scrollback to the last [`TERM_TRIM_KEEP`] lines, prepending a
    /// marker that records how many earlier lines were dropped. Used by `^C` to bail
    /// out of a flood ([`terminal_on_input`]). Implemented by re-seeding a fresh vt100
    /// parser with the marker + kept tail as plain text: vt100 exposes no scrollback
    /// truncation, and a re-seed naturally lays the tail back out (the last screenful
    /// on the live screen, the rest in scrollback). The child is untouched — its next
    /// output (the shell prompt after the interrupt) simply appends. Color on the kept
    /// tail is dropped (re-fed as plain text), which matches the monochrome look
    /// scrolled-off history already has during a live flood.
    ///
    /// [`terminal_on_input`]: EditHost::terminal_on_input
    pub(crate) fn terminal_trim(&mut self, buf: BufferId) {
        let Some(emu) = self.terminals.get(&buf) else {
            return;
        };
        let (rows, cols) = emu.parser.screen().size();
        let scrollback = emu.scrollback;
        // The full current mirror: scrollback history followed by the live screen.
        let mut all: Vec<String> = emu.history.clone();
        all.extend(emu.parser.screen().rows(0, cols));
        if all.len() <= TERM_TRIM_KEEP {
            return; // nothing meaningful to drop
        }
        let dropped = all.len() - TERM_TRIM_KEEP;
        let marker = format!("──── ^C: {dropped} earlier lines trimmed ────");

        // Re-seed: marker line, then each kept line on its own (`\r\n` so the parser
        // scrolls them exactly as the child's output would have).
        let mut parser =
            vt100::Parser::new_with_callbacks(rows, cols, scrollback, TermSink::default());
        let mut seed: Vec<u8> = marker.into_bytes();
        for line in &all[dropped..] {
            seed.extend_from_slice(b"\r\n");
            seed.extend_from_slice(line.as_bytes());
        }
        parser.process(&seed);

        let emu = self
            .terminals
            .get_mut(&buf)
            .expect("emulator present above");
        emu.parser = parser;
        emu.history.clear();
        emu.history_styles.clear();
        emu.last_held = usize::MAX;
        emu.lines_since_input = 0;
        self.terminal_project(buf);
    }

    /// Project `buf`'s emulator into the buffer: the scrollback history followed by
    /// the live screen, with the cursor at the live input position (offset by the
    /// history length). The buffer always mirrors the *full* `history ++ screen`, so
    /// the cursor and line numbers stay stable across `<C-\><C-n>` / `i` — there is no
    /// live-vs-browse buffer flip.
    ///
    /// Cost is kept bounded so a flood never stalls: the history mirror is re-read
    /// from vt100 **only when the scrollback length changed** (something scrolled),
    /// and even then we splice — the leading history lines are unchanged, so we rewrite
    /// only from the first changed row. A refresh where nothing scrolled (steady
    /// typing) rewrites just the live-screen region. Called once per repaint, not per
    /// PTY chunk.
    pub(crate) fn terminal_project(&mut self, buf: BufferId) {
        let Some(emu) = self.terminals.get_mut(&buf) else {
            return;
        };
        let (rows, cols) = emu.parser.screen().size();

        // Re-read the scrollback mirror only when it actually scrolled since the last
        // frame. The length alone can't tell: once the scrollback saturates at the cap
        // it stays `cap` forever while its *contents* keep shifting, so we also compare
        // the newest scrolled row (the row just above the live screen) against the last
        // one we captured — that changes on every scroll, saturated or not.
        let held = {
            let screen = emu.parser.screen_mut();
            screen.set_scrollback(usize::MAX);
            let held = screen.scrollback();
            screen.set_scrollback(0);
            held
        };
        let newest = if held == 0 {
            None
        } else {
            let screen = emu.parser.screen_mut();
            screen.set_scrollback(1); // window row 0 = the newest scrolled row
            let row = screen.rows(0, cols).next();
            screen.set_scrollback(0);
            row
        };
        let scrolled =
            held != emu.last_held || newest.as_deref() != emu.history.last().map(String::as_str);
        if scrolled {
            emu.history = read_scrollback_text(emu.parser.screen_mut(), held, rows, cols);
            emu.parser.screen_mut().set_scrollback(0); // restore the live view
            emu.last_held = held;
            // History changed; any browse-time color cache is stale. It is re-filled
            // lazily by `sync_terminal_styles` on the next browsing frame.
            emu.history_styles.clear();
        }

        let screen = emu.parser.screen();
        let hist_len = emu.history.len();
        // When the scrollback changed, rebuild the whole buffer (history may have
        // evicted from the front); otherwise rewrite only the live-screen region.
        let (replace_from, tail): (usize, Vec<String>) = if scrolled {
            let mut lines = emu.history.clone();
            lines.extend(screen.rows(0, cols));
            (0, lines)
        } else {
            (hist_len, screen.rows(0, cols).collect())
        };
        let (cy, cx) = screen.cursor_position();
        let cursor_row = hist_len + cy as usize;
        let title = emu.parser.callbacks().title.clone();

        self.editor
            .terminal_update(buf, replace_from, &tail, cursor_row, cx as usize);
        if let Some(title) = title {
            self.editor.terminal_set_title(buf, &title);
        }
    }

    /// Materialize the focused terminal's history color while **browsing** (the buffer
    /// is a terminal but the user has left terminal-job mode to navigate), and drop it
    /// while output is live. Called each redraw. Reading per-cell color out of vt100's
    /// scrollback is `O(retained rows)`, so we do it only here — never on the live
    /// flood path — and only once per scrollback state (the cache is index-aligned with
    /// `history` and rebuilt only when it has gone stale, i.e. its length no longer
    /// matches). The result is colored scrollback when you read it, with no cost while
    /// it streams.
    pub(crate) fn sync_terminal_styles(&mut self) {
        let buf = self.editor.current_buffer_id();
        let browsing = self.editor.mode != nxvim_core::Mode::Terminal;
        let Some(emu) = self.terminals.get_mut(&buf) else {
            return;
        };
        if !browsing {
            emu.history_styles.clear(); // live: history is monochrome
            return;
        }
        if emu.history_styles.len() == emu.history.len() {
            return; // already materialized for the current scrollback
        }
        let (rows, cols) = emu.parser.screen().size();
        let held = {
            let screen = emu.parser.screen_mut();
            screen.set_scrollback(usize::MAX);
            screen.scrollback()
        };
        emu.history_styles = read_scrollback_styles(emu.parser.screen_mut(), held, rows, cols);
        emu.parser.screen_mut().set_scrollback(0); // restore the live view
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
                // `numbers` are 1-based buffer lines. A history row (idx < hist_len)
                // uses its browse-time color cache (empty while output is live, so
                // monochrome then); a live screen row (idx - hist_len) reads its colors
                // straight off the vt100 grid.
                let Some(idx) = num.map(|n| n - 1) else {
                    return Value::Array(Vec::new());
                };
                let runs = if idx < hist_len {
                    match emu.history_styles.get(idx) {
                        Some(runs) => runs.clone(),
                        None => return Value::Array(Vec::new()),
                    }
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

/// Read vt100's entire retained scrollback as row **text**, oldest first. vt100 only
/// exposes scrollback through a moving `rows`-tall view-window (`set_scrollback`
/// shifts the offset; `rows` reads that window), so we page through it; at offset `k`
/// the window's row 0 is scrollback row `held - k`. Text only — per-cell color is
/// deliberately not captured (too expensive over a large scrollback). The caller
/// restores the offset to the live view afterward.
fn read_scrollback_text(
    screen: &mut vt100::Screen,
    held: usize,
    rows: u16,
    cols: u16,
) -> Vec<String> {
    let mut out = Vec::with_capacity(held);
    let mut idx = 0;
    while idx < held {
        let k = held - idx;
        screen.set_scrollback(k);
        let take = k.min(rows as usize).min(held - idx);
        out.extend(screen.rows(0, cols).take(take));
        idx += take;
    }
    out
}

/// Read vt100's retained scrollback as per-row coalesced color runs, oldest first —
/// the browse-time twin of [`read_scrollback_text`]. Paged the same way through the
/// view-window; reads each row's cells (`O(retained rows × cols)`), which is why the
/// caller only invokes it while browsing, never on the live flood path. The caller
/// restores the offset to the live view afterward.
fn read_scrollback_styles(
    screen: &mut vt100::Screen,
    held: usize,
    rows: u16,
    cols: u16,
) -> Vec<Vec<(u16, u16, Style)>> {
    let mut out = Vec::with_capacity(held);
    let mut idx = 0;
    while idx < held {
        let k = held - idx;
        screen.set_scrollback(k);
        let take = k.min(rows as usize).min(held - idx);
        for r in 0..take {
            out.push(row_spans(screen, r as u16, cols));
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
                    scrollback,
                } => {
                    // Build the emulator first so the very next redraw projects the
                    // (blank) screen at the right size, then spawn the PTY behind it.
                    self.terminal_open_emu(buf, rows, cols, scrollback);
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
                    // A `^C` on a flooding terminal trims the scrollback (+ resets the
                    // flood counter); the keystroke still reaches the child to interrupt it.
                    // (Native drains its local PTY fast, so no backlog discard is needed.)
                    let _ = self.terminal_on_input(buf, &bytes);
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

/// The wasm terminal transport: the browser owns the vt100 emulation (the shared
/// [`EditHost`] above) but has no PTY, so the real child runs on the daemon over
/// WebTransport (Phase 7). Each op routes through the [`HostEffects`](crate::edithost::HostEffects)
/// terminal seam (`term_open`/`term_write`/`term_resize`/`term_kill`), the worker forwards
/// it to the daemon, and the child's output streams back inbound (`term_data` →
/// [`EditHost::terminal_feed`]). A serverless OPFS session (no daemon) has no PTY host, so
/// an open fails *loud* — never a silent stub.
#[cfg(not(feature = "native"))]
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
                    scrollback,
                } => {
                    if !self.fx.has_remote_proc() {
                        // No daemon ⇒ no PTY host. Fail loud rather than open a dead
                        // terminal whose input goes nowhere.
                        self.editor.terminal_closed(buf, -1);
                        self.editor.echo(
                            "E: :terminal requires a daemon connection in this build".to_string(),
                        );
                        continue;
                    }
                    // Build the emulator first (the browser projects the grid locally), then
                    // ask the daemon to spawn the PTY behind it. The vt100 scrollback is a
                    // local concern; the daemon's PTY doesn't need it.
                    self.terminal_open_emu(buf, rows, cols, scrollback);
                    self.fx.term_open(buf.0, argv, cwd, rows, cols);
                }
                TerminalOp::Send { buf, bytes } => {
                    // A `^C` on a flooding terminal trims the scrollback (+ resets the flood
                    // counter); the keystroke still crosses the wire to interrupt it. A trim
                    // also signals the Worker to discard the child's in-flight backlog so the
                    // cancel takes hold promptly (the browser's QUIC window holds seconds of it).
                    if self.terminal_on_input(buf, &bytes) {
                        self.fx.term_interrupted(buf.0);
                    }
                    self.fx.term_write(buf.0, bytes);
                }
                TerminalOp::Kill { buf } => {
                    self.terminal_remove(buf);
                    self.fx.term_kill(buf.0);
                }
            }
        }
    }

    /// Keep the focused terminal's daemon PTY winsize matching its window text area —
    /// the wasm twin of the native [`sync_terminal_sizes`](EditHost::sync_terminal_sizes).
    /// Called each redraw: on a real change, reflow the local emulator and forward a
    /// `term_resize` to the daemon so the child reflows too.
    pub(crate) fn sync_terminal_sizes(&mut self) {
        let buf = self.editor.current_buffer_id();
        if !self.terminals.contains_key(&buf) {
            return;
        }
        let (rows, cols) = self.editor.current_text_area();
        if self.terminal_resize(buf, rows, cols) {
            self.fx.term_resize(buf.0, rows, cols);
        }
    }

    /// Inbound: project every live terminal once and settle + repaint — the wasm twin of
    /// the native [`on_term_events`](EditHost::on_term_events) post-drain projection. The
    /// FFI feeds each queued `term_data` push (cheap vt100 parse) via
    /// [`terminal_feed`](EditHost::terminal_feed), then calls this **once** after the batch,
    /// so a flood costs one projection per push-drain (one repaint), never one per chunk —
    /// the same "project once per repaint" rule the native leg follows.
    pub fn terminal_flush(&mut self) {
        let bufs: Vec<BufferId> = self.terminals.keys().copied().collect();
        for buf in bufs {
            self.terminal_project(buf);
        }
        self.settle_events(true);
    }

    /// Inbound: a daemon `term_exit` push — the child exited with `code`. Record it (leave
    /// terminal mode, append the `[Process exited]` notice), drop the emulator, then settle
    /// + repaint. The wasm twin of the native `on_term_event` Exit arm.
    pub fn terminal_exit(&mut self, buf: BufferId, code: i32) {
        self.editor.terminal_closed(buf, code);
        self.terminal_remove(buf);
        self.settle_events(true);
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
    use tokio::sync::mpsc::{
        channel, unbounded_channel, Receiver, Sender, UnboundedReceiver, UnboundedSender,
    };

    use nxvim_core::BufferId;

    /// Bounded capacity of the PTY-reader → consumer event channel. This is the
    /// backpressure window for a terminal child's output: when it fills (the
    /// consumer — the run loop locally, or the daemon's wire forwarder — is behind),
    /// the reader thread *blocks* on `blocking_send`, so it stops draining the PTY,
    /// the PTY buffer fills, and the OS blocks the child's `write()`. That throttles
    /// the child to the display's drain rate (like a real terminal), so a flood never
    /// queues without bound and a `^C` stops it promptly. Small for a tight window.
    const TERM_EVENT_CAP: usize = 4;

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
        start: Option<(UnboundedReceiver<TermCommand>, Sender<TermEvent>)>,
        started: bool,
    }

    impl TerminalManager {
        /// Create the manager and the receiver the run loop selects on. No task is
        /// spawned until the first [`send`](Self::send). The event channel is
        /// *bounded* ([`TERM_EVENT_CAP`]) so a child that outruns the consumer is
        /// throttled at the PTY rather than queuing output without limit.
        pub fn new() -> (TerminalManager, Receiver<TermEvent>) {
            let (cmd_tx, cmd_rx) = unbounded_channel();
            let (event_tx, event_rx) = channel(TERM_EVENT_CAP);
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
        event_tx: Sender<TermEvent>,
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
                            let _ = event_tx
                                .send(TermEvent::Data {
                                    buf,
                                    bytes: format!("nxvim: {e}\r\n").into_bytes(),
                                })
                                .await;
                            let _ = event_tx.send(TermEvent::Exit { buf, code: -1 }).await;
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

    /// Spawn one PTY child for `buf` and wire its I/O: a single OS thread streams the
    /// child's output as [`TermEvent::Data`] and, once the output is fully drained,
    /// reaps the child and reports its exit as [`TermEvent::Exit`].
    ///
    /// Reading and waiting share one thread *on purpose*: the editor's exit handler
    /// drops the buffer's emulator ([`terminal_remove`](EditHost::terminal_remove)), so
    /// any `Data` it processes *after* an `Exit` is fed to a gone emulator and lost. Two
    /// independent threads (one read, one wait) gave no ordering guarantee on the shared
    /// channel — a fast-exiting child (`printf 'x\n'`) could enqueue `Exit` before the
    /// reader enqueued its final `Data`, dropping the output. Sending `Exit` only after
    /// the read loop hits EOF/EIO makes `Data`-before-`Exit` ordering deterministic: the
    /// kernel returns the child's buffered output before signalling end-of-stream on the
    /// master (Linux `EIO`-after-drain, macOS `EOF`-after-drain), so by the time the loop
    /// breaks every byte has already been sent. The thread ends when the child exits or
    /// the event channel closes (server shutdown).
    fn open_pty(
        buf: BufferId,
        argv: Vec<String>,
        cwd: Option<String>,
        rows: u16,
        cols: u16,
        event_tx: &Sender<TermEvent>,
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

        let event_tx = event_tx.clone();
        let mut child = child;
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // `blocking_send` is the backpressure: it parks this thread
                        // when the bounded event channel is full (the consumer is
                        // behind), so we stop reading the PTY and the child blocks on
                        // its next `write()` instead of flooding an unbounded queue.
                        if event_tx
                            .blocking_send(TermEvent::Data {
                                buf,
                                bytes: chunk[..n].to_vec(),
                            })
                            .is_err()
                        {
                            // The editor side is gone (shutdown) — stop, but still reap
                            // the child below so it isn't left a zombie.
                            break;
                        }
                    }
                }
            }
            // Output fully drained (EOF/EIO on the master ⇒ the child has closed its end);
            // reap it for the real exit code and report the exit *after* every `Data`.
            let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
            let _ = event_tx.blocking_send(TermEvent::Exit { buf, code });
        });

        Ok(Session {
            writer,
            master: pair.master,
            killer,
        })
    }
}
