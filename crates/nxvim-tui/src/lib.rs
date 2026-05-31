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

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use nxvim_rpc::{connect, Incoming};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};
use rmpv::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use unicode_width::UnicodeWidthChar;

/// Rows reserved at the bottom for the status line and command line.
const CHROME_ROWS: u16 = 2;
/// Tab stop width in cells. Must match `nxvim_core::unicode::TABSTOP` so the
/// painted text lines up with the server's reported screen columns.
const TABSTOP: usize = 8;

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
            Value::from(text_height(size.height) as u64),
            Value::Map(vec![]),
        ],
    )
    .await
    .ok();

    let mut view = View::default();
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
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            message = incoming.recv() => match message {
                Some(Incoming::Notification { method, params }) => match method.as_str() {
                    "redraw" => {
                        view.update(&params);
                        terminal.draw(|frame| render(frame, &view))?;
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

/// Text-area height = terminal height minus the chrome rows we render ourselves.
fn text_height(terminal_height: u16) -> u16 {
    terminal_height.saturating_sub(CHROME_ROWS).max(1)
}

/// The server's view, mirrored client-side for rendering.
#[derive(Default)]
struct View {
    lines: Vec<String>,
    cursor_row: u16,
    cursor_col: u16,
    cursor_screen_col: u16,
    mode_label: String,
    command_mode: bool,
    cmdline: String,
    message: String,
    file_name: String,
    modified: bool,
    cursor_line: usize,
}

impl View {
    fn update(&mut self, params: &[Value]) {
        let Some(Value::Map(map)) = params.first() else {
            return;
        };
        self.lines = map_get(map, "lines")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        self.cursor_row = map_u64(map, "cursor_row") as u16;
        self.cursor_col = map_u64(map, "cursor_col") as u16;
        self.cursor_screen_col = map_u64(map, "cursor_screen_col") as u16;
        self.mode_label = map_str(map, "mode_label");
        self.command_mode = map_get(map, "command_mode")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.cmdline = map_str(map, "cmdline");
        self.message = map_str(map, "message");
        self.file_name = map_str(map, "file_name");
        self.modified = map_get(map, "modified")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.cursor_line = map_u64(map, "cursor_line") as usize;
    }
}

/// Lay out the three regions and render each with its own widget.
fn render(frame: &mut Frame, view: &View) {
    let regions = Layout::vertical([
        Constraint::Min(1),    // text area
        Constraint::Length(1), // status line
        Constraint::Length(1), // command line
    ])
    .split(frame.area());
    let (text_area, status_area, cmd_area) = (regions[0], regions[1], regions[2]);

    render_text(frame, text_area, view);
    render_status(frame, status_area, view);
    render_command(frame, cmd_area, view);

    if view.command_mode {
        let col = cmd_area.x + 1 + view.cmdline.chars().count() as u16;
        frame.set_cursor_position((col, cmd_area.y));
    } else {
        frame.set_cursor_position((
            text_area.x + view.cursor_screen_col,
            text_area.y + view.cursor_row,
        ));
    }
}

fn render_text(frame: &mut Frame, area: Rect, view: &View) {
    let text = Text::from(
        view.lines
            .iter()
            .map(|l| Line::from(expand_tabs(l)))
            .collect::<Vec<_>>(),
    );
    frame.render_widget(Paragraph::new(text), area);
}

/// Expand tabs to spaces at `TABSTOP`, tracking display width so wide characters
/// before a tab advance the column correctly. No-op for tab-free lines; the
/// result never contains a `\t`.
///
/// Per-`char` `UnicodeWidthChar` width matches the server's per-grapheme
/// `unicode::virtcol` (`UnicodeWidthStr`) because str width is just the sum of
/// its chars' widths — so the cursor's `cursor_screen_col` lines up with the
/// glyphs painted here.
fn expand_tabs(line: &str) -> String {
    if !line.contains('\t') {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len() + TABSTOP);
    let mut col = 0;
    for ch in line.chars() {
        if ch == '\t' {
            let spaces = TABSTOP - (col % TABSTOP);
            out.push_str(&" ".repeat(spaces));
            col += spaces;
        } else {
            out.push(ch);
            col += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    out
}

fn render_status(frame: &mut Frame, area: Rect, view: &View) {
    let modified = if view.modified { " [+]" } else { "" };
    let left = format!(" {}  {}{}", view.mode_label, view.file_name, modified);
    let right = format!("{},{} ", view.cursor_line, view.cursor_col + 1);

    let width = area.width as usize;
    let pad = width.saturating_sub(left.chars().count() + right.chars().count());
    let line = format!("{left}{}{right}", " ".repeat(pad));

    frame.render_widget(
        Paragraph::new(line).style(Style::default().add_modifier(Modifier::REVERSED)),
        area,
    );
}

fn render_command(frame: &mut Frame, area: Rect, view: &View) {
    let content = if view.command_mode {
        format!(":{}", view.cmdline)
    } else {
        view.message.clone()
    };
    frame.render_widget(Paragraph::new(content), area);
}

fn map_get<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

fn map_u64(map: &[(Value, Value)], key: &str) -> u64 {
    map_get(map, key).and_then(Value::as_u64).unwrap_or(0)
}

fn map_str(map: &[(Value, Value)], key: &str) -> String {
    map_get(map, key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
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
