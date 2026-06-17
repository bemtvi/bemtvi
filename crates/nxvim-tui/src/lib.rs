//! The terminal UI client.
//!
//! A thin RPC client that owns no editor state. It attaches to the server,
//! sends keystrokes as vim key-notation (`nvim_input`), and renders the
//! server's [`View`](nxvim_view::View) using **ratatui-native widgets, one per
//! region**: the text area, the status line, and the command line are laid out
//! with a ratatui `Layout` and drawn as separate widgets. There is no neovim UI
//! protocol and no custom cell renderer.
//!
//! The client owns the screen layout: it reserves one row for the global command
//! line (each window draws its own status line inside its rect) and tells the
//! server only how tall the *windows area* is, so scrolling stays correct. Input
//! and redraw are multiplexed with `tokio::select!`.
//!
//! The semantic-view decode/input layer is frontend-neutral and lives in the
//! [`nxvim_view`] crate (it mirrors the server's view, parses each `redraw`, and
//! holds the msgpack accessors). The TUI-specific work is split across submodules:
//! [`anim`] is the scroll-animation state machine, [`render`] paints the frame,
//! [`images`] renders `'imagepreview'` pictures via ratatui-image, and [`keys`]
//! encodes key events. This module keeps only the event loop and transport.

mod anim;
mod images;
mod keys;
mod render;

pub use keys::encode_key;
pub use render::{
    close_button, cursor_style, paint, paint_doc_scrolled, paint_with_cursor, panel_content_rect,
    pmenu_doc_geometry, pmenu_geometry, ScrollHarness,
};

use anyhow::Result;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use futures::StreamExt;
use nxvim_rpc::{connect, Incoming};
use nxvim_view::{encode_paste, View};
use rmpv::Value;
use std::io::Write;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::sleep;

use anim::{arm_animation, Animation};
use ratatui::DefaultTerminal;
use render::render;

/// Rows reserved at the bottom for the global command/message line. Each window
/// now draws its own status line inside its rect, so only the command line is
/// reserved here; the height the client reports is the windows-area height.
const CHROME_ROWS: u16 = 1;

/// How long the client waits after a keystroke, with no further input, before
/// sending the server a synthetic `nxvim_input_flush` — vim's `timeoutlen`
/// (default 1000ms). The server has no input timer (it processes `nvim_input`
/// batches synchronously), so a trailing key that is a *live prefix* of a mapping
/// stays withheld in the matcher until something flushes it. This idle flush is
/// that something: it lets an ambiguous shorter map (`j` with `jk` mapped) or a
/// replayed prefix (the second `g` of `gg` with `gh` mapped) resolve without the
/// user pressing another key — the timer-less divergence's blessed fix (design D4).
const TIMEOUT_LEN: Duration = Duration::from_millis(1000);

/// How often a held-at-the-edge mouse drag re-sends itself to keep the buffer
/// auto-scrolling (≈25 lines/sec). The terminal only reports a drag on pointer
/// motion, so without this repeat a selection dragged to the edge and held still
/// would stop scrolling; this paces the continuous scroll the server does a line
/// at a time per drag event.
const AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(40);

/// Whether a drag at windows-area `row` (global, 0-based) sits in the top/bottom
/// edge band that arms continuous auto-scroll, for a terminal `height` rows tall.
/// The top row (above a tabline, or the first text row) and the bottom windows
/// rows (the status line and below, where a drag has crossed past the text body)
/// qualify; the server decides whether that actually scrolls the focused window.
fn in_scroll_zone(row: u16, height: u16) -> bool {
    row == 0 || row + 2 >= height
}

/// RAII guard for terminal mouse capture: enables mouse reporting on creation
/// and **disables it on drop**, including when the event loop unwinds on a
/// panic. ratatui's panic hook restores raw mode and the alternate screen but
/// does *not* touch mouse mode, so without a drop guard a panic would leave the
/// terminal reporting mouse events — spraying the user's shell with escape
/// codes on every click and move. The guard fires on the normal return path and
/// the panic path alike. Generic over the writer so it can be tested against an
/// in-memory sink; production uses `std::io::stdout()`.
pub struct MouseCapture<W: Write> {
    out: W,
}

impl<W: Write> MouseCapture<W> {
    /// Enable mouse capture on `out`; the returned guard disables it on drop.
    pub fn enable(mut out: W) -> Self {
        let _ = crossterm::execute!(out, EnableMouseCapture);
        Self { out }
    }
}

impl<W: Write> Drop for MouseCapture<W> {
    fn drop(&mut self) {
        let _ = crossterm::execute!(self.out, DisableMouseCapture);
    }
}

/// RAII guard that restores the terminal's **default cursor shape on drop**. The
/// client swaps the cursor to a thin bar in insert mode (see
/// [`cursor_style`](render::cursor_style)); this guarantees the user's configured
/// cursor comes back when the client leaves — on the normal return path and on a
/// panic-unwind alike. Like [`MouseCapture`], it exists because ratatui's panic
/// hook restores raw mode and the alternate screen but *not* the cursor shape, so
/// without it a panic in insert mode would leave the user's shell with a bar
/// cursor. Generic over the writer for testing; production uses `std::io::stdout()`.
pub struct CursorStyleGuard<W: Write> {
    out: W,
}

impl<W: Write> CursorStyleGuard<W> {
    /// Take ownership of `out`; the returned guard resets the cursor on drop.
    pub fn new(out: W) -> Self {
        Self { out }
    }
}

impl<W: Write> Drop for CursorStyleGuard<W> {
    fn drop(&mut self) {
        let _ = crossterm::execute!(self.out, SetCursorStyle::DefaultUserShape);
    }
}

/// RAII guard for terminal bracketed paste: turns the mode on at creation and
/// **off on drop**, including on a panic unwind. With it on, the terminal hands
/// a paste to the client as a single [`Event::Paste`] carrying the whole text,
/// so the client forwards one `nvim_input` and the server does one redraw —
/// instead of the per-character storm an unbracketed paste produces, which
/// makes pasted text trickle in one char at a time. Drops cleanly on the panic
/// path too, so a crash never leaves the terminal in bracketed-paste mode.
/// Generic over the writer for the same in-memory testability as
/// [`MouseCapture`]; production uses `std::io::stdout()`.
pub struct BracketedPaste<W: Write> {
    out: W,
}

impl<W: Write> BracketedPaste<W> {
    /// Enable bracketed paste on `out`; the returned guard disables it on drop.
    pub fn enable(mut out: W) -> Self {
        let _ = crossterm::execute!(out, EnableBracketedPaste);
        Self { out }
    }
}

impl<W: Write> Drop for BracketedPaste<W> {
    fn drop(&mut self) {
        let _ = crossterm::execute!(self.out, DisableBracketedPaste);
    }
}

/// Run the client over a connected stream until the server exits or disconnects.
///
/// ratatui's `init`/`restore` own raw mode and the alternate screen (and a panic
/// hook that restores the terminal), so the user's shell is never left broken.
/// Mouse capture is ours to manage — a [`MouseCapture`] guard disables it on
/// drop so even a panic in the event loop can't leave mouse reporting on.
pub async fn run<S>(stream: S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut terminal = ratatui::init();
    // Clear the screen once on entry (neovim emits `ESC[H ESC[J` here). ratatui
    // renders by diffing against its *own* previous buffer, which assumes the
    // alternate screen is blank when we arrive — true in most terminals, but not
    // guaranteed when nxvim runs *inside* another terminal emulator (e.g. our own
    // `:term`, or tmux) that doesn't blank the alt screen on entry. Without this,
    // cells the first frame leaves blank are never painted, so stale content shows
    // through as "render leftover". The explicit clear makes the baseline real.
    let _ = terminal.clear();
    // Capture mouse events so the panel's `[X]` is clickable.
    let mouse = MouseCapture::enable(std::io::stdout());
    // Restore the user's cursor shape on the way out — the loop switches it to a
    // bar in insert mode and must not leak that into their shell.
    let cursor = CursorStyleGuard::new(std::io::stdout());
    // Receive a paste as one event instead of one keystroke per character.
    let paste = BracketedPaste::enable(std::io::stdout());
    let result = event_loop(stream, &mut terminal).await;
    // Restore terminal modes before leaving the alternate screen.
    drop(cursor); // reset cursor shape
    drop(paste); // disable bracketed paste
    drop(mouse); // disable mouse capture
    ratatui::restore();
    result
}

async fn event_loop<S>(stream: S, terminal: &mut DefaultTerminal) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let size = terminal.size()?;
    let (reader, writer) = tokio::io::split(stream);
    let (rpc, mut incoming) = connect(reader, writer);

    rpc.request(
        "nvim_ui_attach",
        vec![
            Value::from(size.width as u64),
            Value::from(text_height(size.height) as u64),
            Value::Map(vec![]),
        ],
    )
    .await
    .ok();

    // Remote (daemon-session) image previews: the file lives on the daemon, so the
    // store fetches its bytes over `nxvim_image_read` instead of reading local disk.
    // `img_fetch_*` carries a request out of the (synchronous) paint into the loop,
    // which issues the RPC on a spawned task; `img_bytes_*` carries the reply back.
    let (img_fetch_tx, mut img_fetch_rx) =
        tokio::sync::mpsc::unbounded_channel::<images::ImageFetch>();
    let (img_bytes_tx, mut img_bytes_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, (u64, u64), Result<Vec<u8>, String>)>();

    // The image renderer for `'imagepreview'`: detect the terminal's graphics
    // protocol now (it queries over stdio), *before* the `EventStream` below starts
    // reading input, so the two don't race for the terminal's replies.
    let mut image_store = images::ImageStore::new(img_fetch_tx);

    let mut view = View::default();
    let mut anim: Option<Animation> = None;
    // Client-side scroll offset for the completion doc preview (a pure UI gesture —
    // the server owns no notion of the box's pixel height). Reset to 0 whenever the
    // previewed docs change (a new selection, or the menu closing).
    let mut doc_scroll: u16 = 0;
    let mut term_events = EventStream::new();
    // The cursor shape last sent to the terminal, so we re-emit the escape only
    // when the mode actually changes the shape rather than on every redraw.
    let mut cursor_shape: Option<SetCursorStyle> = None;
    // Armed by each keystroke; when the `TIMEOUT_LEN` branch wins (no further input
    // arrived), we send one `nxvim_input_flush` and disarm. The `sleep` is recreated
    // each loop pass, so any event — including the next key — restarts the countdown,
    // which is exactly `timeoutlen`'s reset-on-input semantics.
    let mut flush_armed = false;
    // Mouse drag-scroll: while the left button is held with the pointer parked in
    // the top/bottom edge band, re-send that drag on a timer so the buffer keeps
    // auto-scrolling without further pointer motion (the terminal only reports a
    // drag when the pointer actually moves). `Some(cell)` holds the cell to repeat;
    // cleared on release or when the pointer leaves the edge band.
    let mut autoscroll: Option<(u16, u16)> = None;

    loop {
        tokio::select! {
            term_event = term_events.next() => match term_event {
                Some(Ok(Event::Key(key))) => {
                    if key.kind != KeyEventKind::Release {
                        if let Some(notation) = encode_key(key) {
                            rpc.notify("nvim_input", vec![Value::from(notation.as_str())]);
                            flush_armed = true;
                        }
                    }
                }
                Some(Ok(Event::Paste(text))) => {
                    // Bracketed paste: the whole clipboard arrives as one event,
                    // so forward it as a single `nvim_input` (one redraw) rather
                    // than the per-character trickle of an unbracketed paste.
                    let notation = encode_paste(&text);
                    if !notation.is_empty() {
                        rpc.notify("nvim_input", vec![Value::from(notation.as_str())]);
                        flush_armed = true;
                    }
                }
                Some(Ok(Event::Resize(w, h))) => {
                    rpc.notify(
                        "nvim_ui_try_resize",
                        vec![Value::from(w as u64), Value::from(text_height(h) as u64)],
                    );
                    // A resize moves the chrome (the gutter width, the status row, the
                    // command line all shift), so cells that held chrome at the old size
                    // may be blank at the new one. ratatui resets its diff baseline on
                    // resize but emits no screen clear, so — on a host that doesn't clear
                    // its grid on resize — those old cells would linger. Clear so the next
                    // frame repaints over a known-blank screen (neovim clears here too).
                    let _ = terminal.clear();
                }
                Some(Ok(Event::Mouse(m))) => match m.kind {
                    // A left-click on the focused panel's `[X]` closes it — the
                    // same effect as pressing `q`, which the focused panel maps
                    // to close. Guarded on a panel being open so a stray click
                    // never injects a `q` into the buffer.
                    MouseEventKind::Down(MouseButton::Left) => {
                        let size = terminal.size().unwrap_or_default();
                        if let Some(panel) = view.panel.as_ref() {
                            let close = close_button(size.width, size.height, panel.height);
                            let on_close = close
                                .as_ref()
                                .is_some_and(|(row, cols)| m.row == *row && cols.contains(&m.column));
                            if on_close {
                                // The `[X]` on the border bar closes the panel.
                                rpc.notify("nvim_input", vec![Value::from("q")]);
                            } else if let Some((cx, cy, cw, ch)) =
                                panel_content_rect(size.width, size.height, panel.height)
                            {
                                // A click on a content row selects that entry; a
                                // click on the already-selected entry activates it
                                // (`<CR>`) — the panel's select-then-confirm, like
                                // the completion popup. Blank rows past the content
                                // are ignored.
                                if within(m.column, m.row, cx, cy, cw, ch) {
                                    let row = (m.row - cy) as usize;
                                    if row < panel.lines.len() {
                                        let sel_end =
                                            panel.cursor_row + panel.cursor_span.max(1);
                                        let on_selected = (m.row - cy) >= panel.cursor_row
                                            && (m.row - cy) < sel_end;
                                        if on_selected {
                                            rpc.notify("nvim_input", vec![Value::from("<CR>")]);
                                        } else {
                                            rpc.notify(
                                                "nxvim_panel_click",
                                                vec![Value::from(row as u64)],
                                            );
                                        }
                                    }
                                }
                            }
                        } else if let Some((px, py, pw, ph, start)) =
                            pmenu_geometry(size.width, size.height, &view)
                        {
                            // A click on a completion row chooses that item: the
                            // first click on a row selects it (highlight + docs),
                            // and clicking the already-selected row accepts it —
                            // the same select-then-confirm as <C-n> then <C-y>.
                            if within(m.column, m.row, px, py, pw, ph) {
                                if let Some(pmenu) = view.pmenu.as_ref() {
                                    let idx = start + (m.row - py) as usize;
                                    if idx < pmenu.items.len() {
                                        if pmenu.selected == Some(idx) {
                                            rpc.notify("nxvim_complete_accept", vec![]);
                                        } else {
                                            rpc.notify(
                                                "nxvim_complete_select",
                                                vec![Value::from(idx as u64)],
                                            );
                                        }
                                    }
                                }
                            }
                        } else {
                            // No client-owned overlay (panel / completion popup) is
                            // open, so this is a text-area click: forward the global
                            // cell to the server, which owns the hit-test back to a
                            // window + buffer position (focus-follows-click + cursor
                            // placement). `grid` is 0 — nxvim is single-grid.
                            rpc.notify(
                                "nvim_input_mouse",
                                vec![
                                    Value::from("left"),
                                    Value::from("press"),
                                    Value::from(mouse_modifier(m.modifiers)),
                                    Value::from(0u64),
                                    Value::from(m.row as u64),
                                    Value::from(m.column as u64),
                                ],
                            );
                            // Arm edge auto-scroll if the press already landed in the
                            // edge band (a press-and-hold there scrolls without a drag).
                            autoscroll = in_scroll_zone(m.row, size.height)
                                .then_some((m.row, m.column));
                        }
                    }
                    // Drag and release of the left button drive a text-area
                    // selection: the server extends Visual from the press anchor on
                    // drag, and keeps it on release. Forwarded unconditionally — the
                    // server no-ops them unless a text press set an anchor, so a
                    // stray drag over chrome does nothing.
                    MouseEventKind::Drag(MouseButton::Left) => {
                        rpc.notify(
                            "nvim_input_mouse",
                            vec![
                                Value::from("left"),
                                Value::from("drag"),
                                Value::from(mouse_modifier(m.modifiers)),
                                Value::from(0u64),
                                Value::from(m.row as u64),
                                Value::from(m.column as u64),
                            ],
                        );
                        // (Re)arm continuous auto-scroll when the drag is parked in
                        // the edge band; disarm once it moves back into the body.
                        let size = terminal.size().unwrap_or_default();
                        autoscroll =
                            in_scroll_zone(m.row, size.height).then_some((m.row, m.column));
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        autoscroll = None; // release ends the drag, so stop scrolling
                        rpc.notify(
                            "nvim_input_mouse",
                            vec![
                                Value::from("left"),
                                Value::from("release"),
                                Value::from(mouse_modifier(m.modifiers)),
                                Value::from(0u64),
                                Value::from(m.row as u64),
                                Value::from(m.column as u64),
                            ],
                        );
                    }
                    // Right / middle press: no client-owned overlay claims them, so
                    // forward straight to the server, which owns the gesture — the
                    // `'mousemodel'` right-click branch (extend / popup-setpos) and
                    // middle-click paste of the `"*` register. Only the press is
                    // meaningful (the server no-ops right/middle drag + release).
                    MouseEventKind::Down(button @ (MouseButton::Right | MouseButton::Middle)) => {
                        let name = if button == MouseButton::Right {
                            "right"
                        } else {
                            "middle"
                        };
                        rpc.notify(
                            "nvim_input_mouse",
                            vec![
                                Value::from(name),
                                Value::from("press"),
                                Value::from(mouse_modifier(m.modifiers)),
                                Value::from(0u64),
                                Value::from(m.row as u64),
                                Value::from(m.column as u64),
                            ],
                        );
                    }
                    // The mouse wheel. A client-owned overlay claims a *vertical*
                    // notch when the pointer is over it: the completion doc preview
                    // scrolls its docs client-side (the box height is the client's to
                    // know), three lines per notch, clamped so it can't overscroll;
                    // the popup list moves its selection one item per notch (server
                    // state, non-wrapping like a scrollbar); the message panel moves
                    // its cursor one entry. Anything else — every horizontal notch,
                    // and any notch not over an overlay — is a text-area scroll
                    // forwarded to the server, which owns the hit-test back to the
                    // window under the pointer (grid 0 — nxvim is single-grid).
                    MouseEventKind::ScrollDown
                    | MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollLeft
                    | MouseEventKind::ScrollRight => {
                        let size = terminal.size().unwrap_or_default();
                        let vertical = matches!(
                            m.kind,
                            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
                        );
                        let down = m.kind == MouseEventKind::ScrollDown;
                        let doc = pmenu_doc_geometry(size.width, size.height, &view);
                        let over_doc = vertical
                            && doc.is_some_and(|(bx, by, bw, bh, _)| {
                                within(m.column, m.row, bx, by, bw, bh)
                            });
                        let over_pmenu = vertical
                            && pmenu_geometry(size.width, size.height, &view).is_some_and(
                                |(px, py, pw, ph, _)| within(m.column, m.row, px, py, pw, ph),
                            );
                        let over_panel = vertical
                            && view.panel.as_ref().is_some_and(|panel| {
                                panel_content_rect(size.width, size.height, panel.height)
                                    .is_some_and(|(cx, cy, cw, ch)| {
                                        within(m.column, m.row, cx, cy, cw, ch)
                                    })
                            });
                        if over_doc {
                            if let Some((.., max_scroll)) = doc {
                                const STEP: u16 = 3;
                                doc_scroll = if down {
                                    (doc_scroll + STEP).min(max_scroll)
                                } else {
                                    doc_scroll.saturating_sub(STEP)
                                };
                                terminal
                                    .draw(|frame| render(frame, &view, anim.as_ref(), doc_scroll, Some(&mut image_store)))?;
                            }
                        } else if over_pmenu {
                            if let Some(pmenu) = view.pmenu.as_ref() {
                                let n = pmenu.items.len();
                                if n > 0 {
                                    let next = match pmenu.selected {
                                        Some(i) if down => (i + 1).min(n - 1),
                                        Some(i) => i.saturating_sub(1),
                                        None => 0,
                                    };
                                    rpc.notify(
                                        "nxvim_complete_select",
                                        vec![Value::from(next as u64)],
                                    );
                                }
                            }
                        } else if over_panel {
                            // The server owns the panel's (logical, word-wrapped)
                            // cursor, so this just feeds the keys it already handles.
                            let key = if down { "<Down>" } else { "<Up>" };
                            rpc.notify("nvim_input", vec![Value::from(key)]);
                        } else {
                            let action = match m.kind {
                                MouseEventKind::ScrollDown => "down",
                                MouseEventKind::ScrollUp => "up",
                                MouseEventKind::ScrollRight => "right",
                                _ => "left",
                            };
                            rpc.notify(
                                "nvim_input_mouse",
                                vec![
                                    Value::from("wheel"),
                                    Value::from(action),
                                    Value::from(mouse_modifier(m.modifiers)),
                                    Value::from(0u64),
                                    Value::from(m.row as u64),
                                    Value::from(m.column as u64),
                                ],
                            );
                        }
                    }
                    _ => {}
                },
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            message = incoming.recv() => match message {
                Some(Incoming::Notification { method, params }) => match method.as_str() {
                    "redraw" => {
                        let prev_doc = view.pmenu.as_ref().map(|p| p.doc.clone());
                        view.update(&params);
                        // The previewed docs changed (new selection / menu gone):
                        // start the new doc from the top.
                        if view.pmenu.as_ref().map(|p| &p.doc) != prev_doc.as_ref() {
                            doc_scroll = 0;
                        }
                        anim = arm_animation(&view, anim.take());
                        terminal.draw(|frame| render(frame, &view, anim.as_ref(), doc_scroll, Some(&mut image_store)))?;
                        // Match the cursor shape to the mode (a thin bar in insert
                        // mode). Emitted only on change so it doesn't flicker.
                        let want = cursor_style(&view);
                        if cursor_shape != Some(want) {
                            let _ = crossterm::execute!(std::io::stdout(), want);
                            cursor_shape = Some(want);
                        }
                    }
                    "nxvim_exit" => break,
                    _ => {}
                },
                Some(Incoming::Request { id, .. }) => rpc.respond(id, Ok(Value::Nil)),
                None => break,
            },
            // Animation frame tick (~60fps). Disabled when nothing is animating,
            // so the future is never even created in the idle case.
            _ = sleep(Duration::from_millis(16)), if anim.is_some() => {
                if anim.as_ref().is_some_and(|a| a.start.elapsed() >= a.duration) {
                    anim = None; // settle: render the destination view below
                }
                terminal.draw(|frame| render(frame, &view, anim.as_ref(), doc_scroll, Some(&mut image_store)))?;
            },
            // `timeoutlen` idle flush: a keystroke armed the timer and nothing
            // followed within `TIMEOUT_LEN`, so nudge the server to resolve any key
            // it withheld as a live prefix (design D4). Harmless when nothing is
            // pending. Disarmed so it fires at most once per idle gap.
            _ = sleep(TIMEOUT_LEN), if flush_armed => {
                rpc.notify("nxvim_input_flush", vec![]);
                flush_armed = false;
            },
            // Continuous mouse drag-scroll: the button is held with the pointer in
            // the edge band, so re-issue the drag at its last cell. The server
            // scrolls the focused window one line per drag it lands outside the
            // text body and re-extends the selection; held still, this paces it.
            _ = sleep(AUTOSCROLL_INTERVAL), if autoscroll.is_some() => {
                if let Some((row, col)) = autoscroll {
                    rpc.notify(
                        "nvim_input_mouse",
                        vec![
                            Value::from("left"),
                            Value::from("drag"),
                            Value::from(""),
                            Value::from(0u64),
                            Value::from(row as u64),
                            Value::from(col as u64),
                        ],
                    );
                }
            },
            // A remote preview needs its bytes: fetch them over `nxvim_image_read` on a
            // spawned task (so a slow daemon read never stalls input/redraws) and send
            // the reply back on `img_bytes_*`. `None` (the store dropped) just falls
            // through. The closure-side paint can only enqueue, not await, hence this.
            fetch = img_fetch_rx.recv() => if let Some(images::ImageFetch { path, version }) = fetch {
                let rpc = rpc.clone();
                let tx = img_bytes_tx.clone();
                tokio::spawn(async move {
                    let result = match rpc
                        .request("nxvim_image_read", vec![Value::from(path.as_str())])
                        .await
                    {
                        Ok(Value::Binary(bytes)) => Ok(bytes),
                        Ok(other) => Err(format!("nxvim_image_read: unexpected reply {other:?}")),
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send((path, version, result));
                });
            },
            // A remote preview's bytes arrived (or the read failed): hand them to the
            // store and repaint, so the picture replaces its loading placeholder.
            bytes = img_bytes_rx.recv() => if let Some((path, version, result)) = bytes {
                image_store.deliver(path, version, result);
                terminal.draw(|frame| render(frame, &view, anim.as_ref(), doc_scroll, Some(&mut image_store)))?;
            },
        }
    }

    Ok(())
}

/// Text-area height = terminal height minus the chrome rows we render ourselves.
fn text_height(terminal_height: u16) -> u16 {
    terminal_height.saturating_sub(CHROME_ROWS).max(1)
}

/// Whether `(col, row)` falls inside the `w`×`h` rect anchored at `(x, y)` —
/// the mouse hit-test shared by the completion popup and its doc preview box.
fn within(col: u16, row: u16, x: u16, y: u16, w: u16, h: u16) -> bool {
    col >= x && col < x + w && row >= y && row < y + h
}

/// The `nvim_input_mouse` modifier string for a crossterm mouse event's
/// modifiers — e.g. Shift+Ctrl → `"CS"`. The server's parser accepts the chars in
/// any order with the `-` separator optional, so concatenation is enough. Drives
/// shift-click (extend the selection) and the future Ctrl/Alt gestures.
fn mouse_modifier(mods: KeyModifiers) -> String {
    let mut s = String::new();
    if mods.contains(KeyModifiers::CONTROL) {
        s.push('C');
    }
    if mods.contains(KeyModifiers::SHIFT) {
        s.push('S');
    }
    if mods.contains(KeyModifiers::ALT) {
        s.push('A');
    }
    s
}
