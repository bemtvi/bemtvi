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

pub use keys::encode_key;
pub use render::{close_button, paint, ScrollHarness};
pub use view::View;

use anyhow::Result;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyEventKind, MouseButton,
    MouseEventKind,
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
    let result = event_loop(stream, &mut terminal).await;
    drop(mouse); // disable mouse capture before leaving the alternate screen
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
    let mut term_events = EventStream::new();

    loop {
        tokio::select! {
            term_event = term_events.next() => match term_event {
                Some(Ok(Event::Key(key))) => {
                    if key.kind != KeyEventKind::Release {
                        if let Some(notation) = encode_key(key) {
                            rpc.notify("nvim_input", vec![Value::from(notation.as_str())]);
                        }
                    }
                }
                Some(Ok(Event::Resize(w, h))) => {
                    rpc.notify(
                        "nvim_ui_try_resize",
                        vec![Value::from(w as u64), Value::from(text_height(h) as u64)],
                    );
                }
                Some(Ok(Event::Mouse(m))) => {
                    // A left-click on the focused panel's `[X]` closes it — the
                    // same effect as pressing `q`, which the focused panel maps
                    // to close. Guarded on a panel being open so a stray click
                    // never injects a `q` into the buffer.
                    if m.kind == MouseEventKind::Down(MouseButton::Left) {
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
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            message = incoming.recv() => match message {
                Some(Incoming::Notification { method, params }) => match method.as_str() {
                    "redraw" => {
                        view.update(&params);
                        anim = arm_animation(&view, anim.take());
                        terminal.draw(|frame| render(frame, &view, anim.as_ref()))?;
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
                terminal.draw(|frame| render(frame, &view, anim.as_ref()))?;
            },
        }
    }

    Ok(())
}

/// Text-area height = terminal height minus the chrome rows we render ourselves.
fn text_height(terminal_height: u16) -> u16 {
    terminal_height.saturating_sub(CHROME_ROWS).max(1)
}
