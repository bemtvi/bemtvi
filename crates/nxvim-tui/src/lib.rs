//! The terminal UI client.
//!
//! A thin RPC client that owns no editor state. It attaches to the server,
//! sends keystrokes as vim key-notation (`nvim_input`), and renders the
//! server's [`View`](nxvim view map) using **ratatui-native widgets, one per
//! region**: the text area, the status line, and the command line are laid out
//! with a ratatui `Layout` and drawn as separate widgets. There is no neovim UI
//! protocol and no custom cell renderer.
//!
//! The client owns the screen layout: it reserves two rows (status + command)
//! and tells the server only how tall the *text area* is, so scrolling stays
//! correct. Input and redraw are multiplexed with `tokio::select!`.
//!
//! The work is split across submodules: [`view`] mirrors the server's view and
//! parses each `redraw`, [`parse`] holds the msgpack accessors, [`anim`] is the
//! scroll-animation state machine, [`render`] paints the frame, and [`keys`]
//! encodes key events. This module keeps only the event loop and transport.

mod anim;
mod keys;
mod parse;
mod render;
mod view;

pub use keys::{encode_key, encode_paste};
pub use render::{
    close_button, cursor_style, paint, paint_doc_scrolled, pmenu_doc_geometry, ScrollHarness,
};
pub use view::View;

use anyhow::Result;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyEventKind, MouseButton, MouseEventKind,
};
use futures::StreamExt;
use nxvim_rpc::{connect, Incoming};
use rmpv::Value;
use std::io::Write;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::sleep;

use anim::{arm_animation, Animation};
use ratatui::DefaultTerminal;
use render::render;

/// Rows reserved at the bottom for the status line and command line.
const CHROME_ROWS: u16 = 2;

/// How long the client waits after a keystroke, with no further input, before
/// sending the server a synthetic `nxvim_input_flush` — vim's `timeoutlen`
/// (default 1000ms). The server has no input timer (it processes `nvim_input`
/// batches synchronously), so a trailing key that is a *live prefix* of a mapping
/// stays withheld in the matcher until something flushes it. This idle flush is
/// that something: it lets an ambiguous shorter map (`j` with `jk` mapped) or a
/// replayed prefix (the second `g` of `gg` with `gh` mapped) resolve without the
/// user pressing another key — the timer-less divergence's blessed fix (design D4).
const TIMEOUT_LEN: Duration = Duration::from_millis(1000);

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
                }
                Some(Ok(Event::Mouse(m))) => match m.kind {
                    // A left-click on the focused panel's `[X]` closes it — the
                    // same effect as pressing `q`, which the focused panel maps
                    // to close. Guarded on a panel being open so a stray click
                    // never injects a `q` into the buffer.
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(panel) = view.panel.as_ref() {
                            let size = terminal.size().unwrap_or_default();
                            if let Some((row, cols)) =
                                close_button(size.width, size.height, panel.height)
                            {
                                if m.row == row && cols.contains(&m.column) {
                                    rpc.notify("nvim_input", vec![Value::from("q")]);
                                }
                            }
                        }
                    }
                    // The mouse wheel over the completion doc preview scrolls it —
                    // a purely client-side gesture (the box height is the client's
                    // to know), so it never touches the buffer or the server. Three
                    // lines per notch, clamped so it can't overscroll the docs.
                    MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                        let size = terminal.size().unwrap_or_default();
                        if let Some((bx, by, bw, bh, max_scroll)) =
                            pmenu_doc_geometry(size.width, size.height, &view)
                        {
                            let over_box = m.column >= bx
                                && m.column < bx + bw
                                && m.row >= by
                                && m.row < by + bh;
                            if over_box {
                                const STEP: u16 = 3;
                                doc_scroll = if m.kind == MouseEventKind::ScrollDown {
                                    (doc_scroll + STEP).min(max_scroll)
                                } else {
                                    doc_scroll.saturating_sub(STEP)
                                };
                                terminal
                                    .draw(|frame| render(frame, &view, anim.as_ref(), doc_scroll))?;
                            }
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
                        terminal.draw(|frame| render(frame, &view, anim.as_ref(), doc_scroll))?;
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
                terminal.draw(|frame| render(frame, &view, anim.as_ref(), doc_scroll))?;
            },
            // `timeoutlen` idle flush: a keystroke armed the timer and nothing
            // followed within `TIMEOUT_LEN`, so nudge the server to resolve any key
            // it withheld as a live prefix (design D4). Harmless when nothing is
            // pending. Disarmed so it fires at most once per idle gap.
            _ = sleep(TIMEOUT_LEN), if flush_armed => {
                rpc.notify("nxvim_input_flush", vec![]);
                flush_armed = false;
            },
        }
    }

    Ok(())
}

/// Text-area height = terminal height minus the chrome rows we render ourselves.
fn text_height(terminal_height: u16) -> u16 {
    terminal_height.saturating_sub(CHROME_ROWS).max(1)
}
