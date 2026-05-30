//! The terminal UI client.
//!
//! This is the rust-native analogue of neovim's built-in `tui/`. It is a *thin
//! client*: it owns no editor state. It attaches to the server over RPC, sends
//! keystrokes as vim key-notation (`nvim_input`), and paints whatever grid the
//! server pushes via `redraw` notifications. A future native GUI is simply a
//! different client speaking the same protocol.
//!
//! Rendering and terminal setup/teardown are delegated to
//! [ratatui](https://ratatui.rs); we keep no custom renderer. Input and redraw
//! are multiplexed with `tokio::select!`, so terminal events and server output
//! are handled concurrently and the UI stays responsive.

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use nxvim_rpc::{connect, Incoming};
use ratatui::text::{Line, Text};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};
use rmpv::Value;
use tokio::io::{AsyncRead, AsyncWrite};

/// Run the client over a connected stream until the server exits or disconnects.
///
/// ratatui's `init`/`restore` own raw mode and the alternate screen (and a panic
/// hook that restores the terminal), so the user's shell is never left broken.
pub async fn run<S>(stream: S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut terminal = ratatui::init();
    let result = event_loop(stream, &mut terminal).await;
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
            Value::from(size.height as u64),
            Value::Map(vec![]),
        ],
    )
    .await
    .ok();

    let mut grid = Grid::new(size.height as usize);
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
                        vec![Value::from(w as u64), Value::from(h as u64)],
                    );
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            message = incoming.recv() => match message {
                Some(Incoming::Notification { method, params }) => match method.as_str() {
                    "redraw" => {
                        if apply_redraw(&mut grid, &params) {
                            terminal.draw(|frame| draw(frame, &grid))?;
                        }
                    }
                    "nxvim_exit" => break,
                    _ => {}
                },
                Some(Incoming::Request { id, .. }) => rpc.respond(id, Ok(Value::Nil)),
                None => break,
            },
        }
    }

    Ok(())
}

/// The latest grid pushed by the server. The server lays out full rows, so the
/// client just needs to hold them and the cursor position.
struct Grid {
    lines: Vec<String>,
    cursor_row: u16,
    cursor_col: u16,
}

impl Grid {
    fn new(height: usize) -> Self {
        Grid {
            lines: vec![String::new(); height],
            cursor_row: 0,
            cursor_col: 0,
        }
    }

    fn resize(&mut self, height: usize) {
        self.lines.resize(height, String::new());
    }
}

/// Render the current grid with ratatui. One `Paragraph` for the whole frame,
/// plus the cursor — ratatui diffs and clears for us.
fn draw(frame: &mut Frame, grid: &Grid) {
    let text = Text::from(grid.lines.iter().cloned().map(Line::from).collect::<Vec<_>>());
    frame.render_widget(Paragraph::new(text), frame.area());
    frame.set_cursor_position((grid.cursor_col, grid.cursor_row));
}

/// Apply a batch of redraw events; returns `true` when a `flush` was seen.
fn apply_redraw(grid: &mut Grid, events: &[Value]) -> bool {
    let mut flush = false;
    for event in events {
        let Value::Array(parts) = event else { continue };
        match parts.first().and_then(Value::as_str) {
            Some("resize") => {
                let h = parts.get(2).and_then(Value::as_u64).unwrap_or(0) as usize;
                grid.resize(h);
            }
            Some("line") => {
                let row = parts.get(1).and_then(Value::as_u64).unwrap_or(0) as usize;
                let text = parts.get(2).and_then(Value::as_str).unwrap_or("");
                if row < grid.lines.len() {
                    grid.lines[row] = text.to_string();
                }
            }
            Some("cursor") => {
                grid.cursor_row = parts.get(1).and_then(Value::as_u64).unwrap_or(0) as u16;
                grid.cursor_col = parts.get(2).and_then(Value::as_u64).unwrap_or(0) as u16;
            }
            Some("flush") => flush = true,
            _ => {}
        }
    }
    flush
}

/// Translate a crossterm key event into vim key-notation.
fn encode_key(ev: KeyEvent) -> Option<String> {
    let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
    let alt = ev.modifiers.contains(KeyModifiers::ALT);

    let mut prefix = String::new();
    if ctrl {
        prefix.push_str("C-");
    }
    if alt {
        prefix.push_str("A-");
    }
    let wrap = |name: &str| format!("<{prefix}{name}>");

    let notation = match ev.code {
        KeyCode::Char(c) => {
            if !prefix.is_empty() {
                format!("<{prefix}{c}>")
            } else if c == '<' {
                "<lt>".to_string()
            } else {
                c.to_string()
            }
        }
        KeyCode::Esc => wrap("Esc"),
        KeyCode::Enter => wrap("CR"),
        KeyCode::Backspace => wrap("BS"),
        KeyCode::Tab => wrap("Tab"),
        KeyCode::Delete => wrap("Del"),
        KeyCode::Left => wrap("Left"),
        KeyCode::Right => wrap("Right"),
        KeyCode::Up => wrap("Up"),
        KeyCode::Down => wrap("Down"),
        KeyCode::Home => wrap("Home"),
        KeyCode::End => wrap("End"),
        KeyCode::PageUp => wrap("PageUp"),
        KeyCode::PageDown => wrap("PageDown"),
        _ => return None,
    };
    Some(notation)
}
