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
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};
use rmpv::Value;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::sleep;
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
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            message = incoming.recv() => match message {
                Some(Incoming::Notification { method, params }) => match method.as_str() {
                    "redraw" => {
                        view.update(&params);
                        // A scroll gesture arms a fresh animation; any other
                        // redraw (e.g. the result of a keypress) supersedes and
                        // clears the in-flight one — the interrupt path.
                        anim = view.scroll.as_ref().map(Animation::new);
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
    /// Per visible row, the half-open screen-column span `[start, end)` to paint
    /// as the visual selection, or `None`. Mirrors the server's `View::selection`.
    selection: Vec<Option<(u16, u16)>>,
    scroll: Option<ScrollData>,
}

/// The scroll gesture mirrored from the server's redraw, ready to animate.
/// Line/cursor positions are kept as `f32` for interpolation; `lines`/`selection`
/// are the band covering the slide, anchored at `base_line`.
#[derive(Clone)]
struct ScrollData {
    from_top: f32,
    to_top: f32,
    from_cursor: f32,
    to_cursor: f32,
    duration: Duration,
    base_line: usize,
    lines: Vec<String>,
    selection: Vec<Option<(u16, u16)>>,
}

/// An in-flight scroll animation, driven by the client's local clock.
struct Animation {
    from_top: f32,
    to_top: f32,
    from_cursor: f32,
    to_cursor: f32,
    start: Instant,
    duration: Duration,
    base_line: usize,
    lines: Vec<String>,
    selection: Vec<Option<(u16, u16)>>,
}

impl Animation {
    fn new(s: &ScrollData) -> Self {
        Animation {
            from_top: s.from_top,
            to_top: s.to_top,
            from_cursor: s.from_cursor,
            to_cursor: s.to_cursor,
            start: Instant::now(),
            duration: s.duration,
            base_line: s.base_line,
            lines: s.lines.clone(),
            selection: s.selection.clone(),
        }
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
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
        self.selection = map_get(map, "selection")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|v| match v.as_array() {
                        Some(pair) if pair.len() == 2 => Some((
                            pair[0].as_u64().unwrap_or(0) as u16,
                            pair[1].as_u64().unwrap_or(0) as u16,
                        )),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.scroll = match map_get(map, "scroll") {
            Some(Value::Map(s)) => Some(ScrollData {
                from_top: map_u64(s, "from_top") as f32,
                to_top: map_u64(s, "to_top") as f32,
                from_cursor: map_u64(s, "from_cursor") as f32,
                to_cursor: map_u64(s, "to_cursor") as f32,
                duration: Duration::from_millis(map_u64(s, "duration_ms")),
                base_line: map_u64(s, "base_line") as usize,
                lines: map_get(s, "lines")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                selection: map_get(s, "selection")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .map(|v| match v.as_array() {
                                Some(pair) if pair.len() == 2 => Some((
                                    pair[0].as_u64().unwrap_or(0) as u16,
                                    pair[1].as_u64().unwrap_or(0) as u16,
                                )),
                                _ => None,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            _ => None,
        };
    }
}

/// Lay out the three regions and render each with its own widget. When `anim`
/// is present and unfinished, the text area shows an interpolated slice of the
/// scroll band instead of the static viewport.
fn render(frame: &mut Frame, view: &View, anim: Option<&Animation>) {
    let regions = Layout::vertical([
        Constraint::Min(1),    // text area
        Constraint::Length(1), // status line
        Constraint::Length(1), // command line
    ])
    .split(frame.area());
    let (text_area, status_area, cmd_area) = (regions[0], regions[1], regions[2]);

    let height = text_area.height as usize;
    let frame_lines: Vec<String>;
    let frame_sel: Vec<Option<(u16, u16)>>;
    let cursor_row: u16;

    match anim {
        Some(a) => {
            let raw = (a.start.elapsed().as_secs_f32() / a.duration.as_secs_f32()).clamp(0.0, 1.0);
            let t = 1.0 - (1.0 - raw).powi(3); // ease-out cubic
            let top = lerp(a.from_top, a.to_top, t).round() as usize;
            let cur = lerp(a.from_cursor, a.to_cursor, t).round() as usize;
            let off = top.saturating_sub(a.base_line);
            frame_lines = a.lines.iter().skip(off).take(height).cloned().collect();
            frame_sel = a.selection.iter().skip(off).take(height).copied().collect();
            cursor_row = cur.saturating_sub(top) as u16;
        }
        None => {
            frame_lines = view.lines.clone();
            frame_sel = view.selection.clone();
            cursor_row = view.cursor_row;
        }
    }

    render_text(frame, text_area, &frame_lines, &frame_sel);
    render_status(frame, status_area, view);
    render_command(frame, cmd_area, view);

    if view.command_mode {
        let col = cmd_area.x + 1 + view.cmdline.chars().count() as u16;
        frame.set_cursor_position((col, cmd_area.y));
    } else {
        // The cursor row is interpolated during a slide, but the column comes
        // straight from the destination view — correct because the scroll
        // commands move only vertically. A future horizontal scroll would need
        // to interpolate the column too.
        frame.set_cursor_position((
            text_area.x + view.cursor_screen_col,
            text_area.y + cursor_row,
        ));
    }
}

fn render_text(frame: &mut Frame, area: Rect, lines: &[String], selection: &[Option<(u16, u16)>]) {
    let width = area.width as usize;
    let text = Text::from(
        lines
            .iter()
            .enumerate()
            .map(|(row, l)| {
                let sel = selection.get(row).copied().flatten();
                highlight_line(l, sel, width)
            })
            .collect::<Vec<_>>(),
    );
    frame.render_widget(Paragraph::new(text), area);
}

/// Build a display line, styling the screen columns in `sel` as the visual
/// selection. `sel` is a half-open `[start, end)` span of screen cells; `end`
/// may run past the text to mark a selected newline or fill a linewise
/// selection, in which case the gap up to `max_width` is painted with blanks.
fn highlight_line(line: &str, sel: Option<(u16, u16)>, max_width: usize) -> Line<'static> {
    let expanded = expand_tabs(line);
    let (start, end) = match sel {
        Some((s, e)) if e > s => (s as usize, e as usize),
        _ => return Line::from(expanded),
    };

    // Partition the (tab-expanded) cells into before / selected / after by their
    // screen column, tracking width so wide glyphs advance two cells.
    let (mut pre, mut mid, mut post) = (String::new(), String::new(), String::new());
    let mut col = 0usize;
    for ch in expanded.chars() {
        if col < start {
            pre.push(ch);
        } else if col < end {
            mid.push(ch);
        } else {
            post.push(ch);
        }
        col += UnicodeWidthChar::width(ch).unwrap_or(0);
    }
    // Extend the highlight past end-of-text (selected newline / linewise fill).
    while col < end && col < max_width {
        mid.push(' ');
        col += 1;
    }

    let hl = Style::default().add_modifier(Modifier::REVERSED);
    let mut spans = Vec::with_capacity(3);
    if !pre.is_empty() {
        spans.push(Span::raw(pre));
    }
    if !mid.is_empty() {
        spans.push(Span::styled(mid, hl));
    }
    if !post.is_empty() {
        spans.push(Span::raw(post));
    }
    Line::from(spans)
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
