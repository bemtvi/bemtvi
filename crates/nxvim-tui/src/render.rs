//! The renderer: lays the three regions out and paints each with a ratatui
//! widget, plus the headless [`paint`]/[`ScrollHarness`] test entry points.

use crossterm::cursor::SetCursorStyle;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use rmpv::Value;
use unicode_width::UnicodeWidthChar;

use crate::anim::{arm_animation, lerp, Animation};
use crate::parse::{DiagSpan, HlSpan, IncSearchSpans, SearchSpans};
use crate::view::{PanelData, PmenuData, View};

/// Tab stop width in cells. Must match `nxvim_core::unicode::TABSTOP` so the
/// painted text lines up with the server's reported screen columns.
const TABSTOP: usize = 8;

/// Render `view` into a `width`x`height` cell grid using ratatui's test backend
/// and return the painted buffer. This drives the *same* `render` the live
/// client uses, so tests assert on exactly what a user would see.
pub fn paint(view: &View, width: u16, height: u16) -> ratatui::buffer::Buffer {
    paint_doc_scrolled(view, width, height, 0)
}

/// Like [`paint`], but with the completion doc preview scrolled down `doc_scroll`
/// lines — the hook a test uses to assert the mouse-wheel scroll offset actually
/// shifts the rendered docs. Not part of the client's runtime API.
#[doc(hidden)]
pub fn paint_doc_scrolled(
    view: &View,
    width: u16,
    height: u16,
    doc_scroll: u16,
) -> ratatui::buffer::Buffer {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal
        .draw(|frame| render(frame, view, None, doc_scroll))
        .expect("draw");
    terminal.backend().buffer().clone()
}

/// The terminal cursor shape for the current mode, matching vim/neovim's default
/// `guicursor`: insert mode gets a thin vertical bar (the "edit cursor"), replace
/// mode an underline, and every other mode the steady block. The live client
/// emits this after each redraw (the shape is a terminal-wide setting, not
/// something ratatui's per-frame cursor *position* controls); the headless tests
/// assert on it without a real terminal.
pub fn cursor_style(view: &View) -> SetCursorStyle {
    if view.is_insert() {
        SetCursorStyle::SteadyBar
    } else if view.is_replace() {
        SetCursorStyle::SteadyUnderScore
    } else {
        SetCursorStyle::SteadyBlock
    }
}

/// A headless mirror of the client's render state — the `View` and the in-flight
/// scroll `Animation` — driven by `redraw` notifications exactly as the live
/// event loop drives them (via [`arm_animation`]). Lets tests exercise the
/// scroll-animation lifecycle (which the event loop owns) without a real
/// terminal or RPC connection. Not part of the client's runtime API.
#[doc(hidden)]
#[derive(Default)]
pub struct ScrollHarness {
    view: View,
    anim: Option<Animation>,
}

#[doc(hidden)]
impl ScrollHarness {
    pub fn new() -> Self {
        ScrollHarness::default()
    }

    /// Apply a `redraw` notification's params, arming/clearing the scroll
    /// animation the same way the live event loop does.
    pub fn on_redraw(&mut self, params: &[Value]) {
        self.view.update(params);
        self.anim = arm_animation(&self.view, self.anim.take());
    }

    /// Whether a scroll animation is currently in flight.
    pub fn animating(&self) -> bool {
        self.anim.is_some()
    }

    /// Paint the current frame — mid-animation when one is in flight — into a
    /// `width`x`height` grid, via the same `render` the live client runs.
    pub fn paint(&self, width: u16, height: u16) -> ratatui::buffer::Buffer {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &self.view, self.anim.as_ref(), 0))
            .expect("draw");
        terminal.backend().buffer().clone()
    }
}

/// Lay out the three regions and render each with its own widget. When `anim`
/// is present and unfinished, the text area shows an interpolated slice of the
/// scroll band instead of the static viewport.
pub(crate) fn render(frame: &mut Frame, view: &View, anim: Option<&Animation>, doc_scroll: u16) {
    // The panel docks below the status line, claiming `height + 1` rows (content
    // plus its title bar); `0` rows — an empty region — when no panel is open.
    // The server already shrank the text `lines` to match, so the text area
    // (`Min(1)`) lands at exactly the right height. The command line stays the
    // very last row (where `:` typing and the cursor live).
    let panel_rows = view.panel.as_ref().map_or(0, |p| p.height + 1);
    let regions = Layout::vertical([
        Constraint::Min(1),             // text area
        Constraint::Length(1),          // status line
        Constraint::Length(panel_rows), // panel (0 when none)
        Constraint::Length(1),          // command line
    ])
    .split(frame.area());
    let (text_area, status_area, panel_area, cmd_area) =
        (regions[0], regions[1], regions[2], regions[3]);

    let height = text_area.height as usize;
    let frame_lines: Vec<String>;
    let frame_sel: Vec<Option<(u16, u16)>>;
    // Search-match highlights for the static viewport. A search never starts a
    // scroll animation, so the slide band carries none — left empty while sliding.
    let empty_search: SearchSpans = Vec::new();
    let empty_incsearch: IncSearchSpans = Vec::new();
    // Syntax highlights for the painted rows — the static viewport, or, during a
    // slide, the matching slice of the over-scanned band so the scroll is colored
    // throughout instead of flashing white until it settles.
    let frame_hl: Vec<Vec<HlSpan>>;
    let frame_numbers: Vec<Option<usize>>;
    let cursor_row: u16;
    // 1-based buffer line the cursor sits on, used to compute relative numbers.
    // During a slide it tracks the interpolated cursor so the gutter stays in
    // step with the moving text.
    let current_line: usize;

    match anim {
        Some(a) => {
            // `arm_animation` never arms a zero-duration slide; guard the divide
            // anyway so progress can't become NaN/inf if that ever changes.
            let raw = if a.duration.is_zero() {
                1.0
            } else {
                (a.start.elapsed().as_secs_f32() / a.duration.as_secs_f32()).clamp(0.0, 1.0)
            };
            let t = 1.0 - (1.0 - raw).powi(3); // ease-out cubic
            let top = lerp(a.from_top, a.to_top, t).round() as usize;
            let cur = lerp(a.from_cursor, a.to_cursor, t).round() as usize;
            let off = top.saturating_sub(a.base_line);
            frame_lines = a.lines.iter().skip(off).take(height).cloned().collect();
            frame_sel = a.selection.iter().skip(off).take(height).copied().collect();
            frame_numbers = a.numbers.iter().skip(off).take(height).copied().collect();
            frame_hl = a
                .highlights
                .iter()
                .skip(off)
                .take(height)
                .cloned()
                .collect();
            cursor_row = cur.saturating_sub(top) as u16;
            current_line = cur + 1;
        }
        None => {
            frame_lines = view.lines.clone();
            frame_sel = view.selection.clone();
            frame_numbers = view.numbers.clone();
            frame_hl = view.highlights.clone();
            cursor_row = view.cursor_row;
            current_line = view.cursor_line;
        }
    }

    // Paint the whole text area with the theme's `Normal` background first (when
    // a colorscheme is loaded), so every following widget's spans patch their
    // foreground onto it and the gutter, end-of-line gaps, and `~` rows all share
    // the editor background. With no theme this is skipped — the terminal default
    // shows through, exactly as before.
    if let Some(normal) = view.normal {
        frame.render_widget(Block::default().style(normal), text_area);
    }

    // Split a number-column gutter off the left of the text area when enabled.
    // The gutter is its own widget; the text, selection, and cursor columns are
    // all measured from the text sub-area, so they stay gutter-agnostic.
    let text_inner = if view.number_width > 0 {
        let cols = Layout::horizontal([Constraint::Length(view.number_width), Constraint::Min(0)])
            .split(text_area);
        render_gutter(frame, cols[0], &frame_numbers, current_line, view);
        cols[1]
    } else {
        text_area
    };

    // Token style ids index a palette captured with the frame they belong to:
    // the in-flight animation's snapshot while sliding, else the live view's.
    let theme = LineTheme {
        palette: match anim {
            Some(a) => &a.styles,
            None => &view.styles,
        },
        visual: view.visual,
        search: view.search_style,
        incsearch: view.incsearch_style,
        end_of_buffer: view.end_of_buffer,
    };
    // The slide band carries no search spans; the static viewport uses the view's.
    let (frame_search, frame_incsearch): (&SearchSpans, &IncSearchSpans) = match anim {
        Some(_) => (&empty_search, &empty_incsearch),
        None => (&view.search, &view.incsearch),
    };
    // Diagnostics, like search, are painted on the settled viewport only — the
    // slide band carries none (squiggles reappear once the scroll lands).
    let empty_diag: Vec<Vec<DiagSpan>> = Vec::new();
    let frame_diag: &[Vec<DiagSpan>] = match anim {
        Some(_) => &empty_diag,
        None => &view.diagnostics,
    };
    render_text(
        frame,
        text_inner,
        &frame_lines,
        &frame_sel,
        frame_search,
        frame_incsearch,
        &frame_hl,
        frame_diag,
        &frame_numbers,
        &theme,
    );
    render_status(frame, status_area, view);
    render_command(frame, cmd_area, view);

    // The insert-mode completion popup floats over the text area, drawn after the
    // text so it sits on top. The editing cursor (placed below) stays visible
    // above the menu. A panel and the popup never coexist (the popup is
    // insert-mode; a focused panel grabs every key), so this never fights the
    // panel cursor handled next.
    if let Some(pmenu) = &view.pmenu {
        render_pmenu(frame, text_inner, pmenu, doc_scroll);
    }

    // A focused panel owns the cursor: draw it on the panel's current line and
    // skip the text/command cursor entirely.
    if let Some(panel) = &view.panel {
        let content = render_panel(frame, panel_area, panel);
        frame.set_cursor_position((
            content.x,
            content.y + panel.cursor_row.min(content.height.saturating_sub(1)),
        ));
        return;
    }

    if view.command_mode {
        // Offset past the leading prompt — a single prefix char (`:`/`/`/`?`) or
        // the multi-char `vim.ui.input` label; the cursor then follows
        // `cmdline_cursor` (a char offset) so it sits mid-line after edits.
        let prompt_width = cmdline_prompt_width(view);
        let col = cmd_area.x + prompt_width + view.cmdline_cursor as u16;
        frame.set_cursor_position((col, cmd_area.y));
    } else {
        // The cursor row is interpolated during a slide, but the column comes
        // straight from the destination view — correct because the scroll
        // commands move only vertically. A future horizontal scroll would need
        // to interpolate the column too.
        frame.set_cursor_position((
            text_inner.x + view.cursor_screen_col,
            text_inner.y + cursor_row,
        ));
    }
}

/// Paint the line-number column. Each row shows, per the active options:
/// absolute numbers (`number`), distance-from-cursor (`relativenumber`), or the
/// hybrid — absolute on the cursor line, relative elsewhere — when both are on.
/// The cursor line uses the theme's `CursorLineNr`, other rows its `LineNr`;
/// with no colorscheme loaded they fall back to un-dimmed / dimmed (vim's look
/// out of the box). `~` filler rows get a blank gutter.
fn render_gutter(
    frame: &mut Frame,
    area: Rect,
    numbers: &[Option<usize>],
    current_line: usize,
    view: &View,
) {
    let width = area.width as usize;
    let text = Text::from(
        numbers
            .iter()
            .map(|num| {
                let is_current = *num == Some(current_line);
                let cell = gutter_cell(*num, current_line, view.number, view.relativenumber, width);
                let style = if is_current {
                    view.cursor_line_nr.unwrap_or_default()
                } else {
                    view.line_nr
                        .unwrap_or_else(|| Style::default().add_modifier(Modifier::DIM))
                };
                Line::from(Span::styled(cell, style))
            })
            .collect::<Vec<_>>(),
    );
    frame.render_widget(Paragraph::new(text), area);
}

/// Build one `width`-cell gutter cell for a row whose buffer line is `num`
/// (`None` for a `~` filler). Numbers are right-aligned with a trailing space,
/// except the hybrid cursor line whose absolute number is left-aligned — vim's
/// layout.
fn gutter_cell(
    num: Option<usize>,
    current_line: usize,
    number: bool,
    relativenumber: bool,
    width: usize,
) -> String {
    let Some(n) = num else {
        return " ".repeat(width);
    };
    let is_current = n == current_line;
    if number && relativenumber && is_current {
        // Hybrid cursor line: absolute number, left-aligned.
        format!("{n:<width$}")
    } else {
        let value = if !relativenumber {
            n // number-only: absolute on every line
        } else if is_current {
            0 // relativenumber-only cursor line shows 0
        } else {
            n.abs_diff(current_line)
        };
        let field = width.saturating_sub(1); // reserve the trailing space
        format!("{value:>field$} ")
    }
}

/// The theme inputs `highlight_line` needs that come from the active
/// colorscheme: the frame's resolved style `palette` (spans index into it), the
/// `visual` selection style, and the `end_of_buffer` style for `~` filler rows.
/// All `None`/empty with no colorscheme, so the client keeps its built-in look.
struct LineTheme<'a> {
    palette: &'a [Style],
    visual: Option<Style>,
    /// The `Search`/`IncSearch` match styles from the colorscheme; `None` falls
    /// back to a built-in yellow highlight so matches show with no theme loaded.
    search: Option<Style>,
    incsearch: Option<Style>,
    end_of_buffer: Option<Style>,
}

#[allow(clippy::too_many_arguments)]
fn render_text(
    frame: &mut Frame,
    area: Rect,
    lines: &[String],
    selection: &[Option<(u16, u16)>],
    search: &[Vec<(u16, u16)>],
    incsearch: &[Option<(u16, u16)>],
    highlights: &[Vec<HlSpan>],
    diagnostics: &[Vec<DiagSpan>],
    numbers: &[Option<usize>],
    theme: &LineTheme,
) {
    let width = area.width as usize;
    let empty: Vec<HlSpan> = Vec::new();
    let empty_diag: Vec<DiagSpan> = Vec::new();
    let empty_search: Vec<(u16, u16)> = Vec::new();
    let text = Text::from(
        lines
            .iter()
            .enumerate()
            .map(|(row, l)| {
                let sel = selection.get(row).copied().flatten();
                let matches = search.get(row).unwrap_or(&empty_search);
                let cur = incsearch.get(row).copied().flatten();
                let hl = highlights.get(row).unwrap_or(&empty);
                let diag = diagnostics.get(row).unwrap_or(&empty_diag);
                // A row with no buffer line is a `~` end-of-buffer filler.
                let is_filler = matches!(numbers.get(row), Some(None));
                highlight_line(l, sel, matches, cur, hl, diag, width, is_filler, theme)
            })
            .collect::<Vec<_>>(),
    );
    frame.render_widget(Paragraph::new(text), area);
}

/// Build a display line, painting each screen cell with its highlight span's
/// style (the resolved palette entry, or [`group_style`] as fallback) and
/// overlaying the visual selection (`sel`, a half-open `[start, end)` span of
/// screen cells) on top — the theme's `Visual` when loaded, reverse-video
/// otherwise. `end` may run past the text to mark a selected newline or fill a
/// linewise selection, in which case the gap up to `max_width` is painted with
/// selected blanks. `~` filler rows are painted with the `EndOfBuffer` style.
///
/// Both `hl` spans and `sel` are in screen columns (the server resolved byte
/// offsets through the same tab/wide-char `virtcol` the gutter-less text area
/// uses), so they line up with the glyphs painted here. Token foregrounds patch
/// onto the `Normal` background already painted across the text area.
#[allow(clippy::too_many_arguments)]
fn highlight_line(
    line: &str,
    sel: Option<(u16, u16)>,
    search: &[(u16, u16)],
    incsearch: Option<(u16, u16)>,
    hl: &[HlSpan],
    diag: &[DiagSpan],
    max_width: usize,
    is_filler: bool,
    theme: &LineTheme,
) -> Line<'static> {
    let expanded = expand_tabs(line);

    // `~` rows carry no tokens or selection: paint the marker with the theme's
    // EndOfBuffer style (default — terminal foreground — with no colorscheme).
    if is_filler {
        return Line::from(Span::styled(
            expanded,
            theme.end_of_buffer.unwrap_or_default(),
        ));
    }

    let sel = sel.filter(|(s, e)| e > s);

    // Walk cells left to right, coalescing runs of identical style into spans.
    let mut spans: Vec<Span> = Vec::new();
    let mut run = String::new();
    let mut run_style = Style::default();
    let mut col = 0usize;
    for ch in expanded.chars() {
        let style = cell_style(col, sel, search, incsearch, hl, diag, theme);
        if style != run_style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_style = style;
        run.push(ch);
        col += UnicodeWidthChar::width(ch).unwrap_or(0);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style));
    }

    // Extend the selection past end-of-text (selected newline / linewise fill).
    if let Some((_, e)) = sel {
        let e = (e as usize).min(max_width);
        if col < e {
            let pad = " ".repeat(e - col);
            spans.push(Span::styled(pad, selection_style(Style::default(), theme)));
        }
    }
    Line::from(spans)
}

/// The style of the screen cell at column `col`: its highlight span's resolved
/// palette style (or [`group_style`] fallback when the span carries no id),
/// with the selection composed on top when the cell is selected.
fn cell_style(
    col: usize,
    sel: Option<(u16, u16)>,
    search: &[(u16, u16)],
    incsearch: Option<(u16, u16)>,
    hl: &[HlSpan],
    diag: &[DiagSpan],
    theme: &LineTheme,
) -> Style {
    let mut style = Style::default();
    for (start, end, group, id) in hl {
        if col >= *start as usize && col < *end as usize {
            style = match id {
                Some(i) => theme.palette.get(*i).copied().unwrap_or_default(),
                None => group_style(group),
            };
            break; // spans don't overlap
        }
    }
    // Search-match highlights ride on top of the syntax token: every match in
    // the `Search` color, then the live incsearch match in `IncSearch` over it.
    let in_span = |span: (u16, u16)| col >= span.0 as usize && col < span.1 as usize;
    if search.iter().copied().any(in_span) {
        style = search_style(style, theme.search, Color::Yellow);
    }
    if incsearch.is_some_and(in_span) {
        style = search_style(style, theme.incsearch, Color::LightYellow);
    }
    // The visual selection sits on top of everything.
    if let Some((s, e)) = sel {
        if col >= s as usize && col < e as usize {
            style = selection_style(style, theme);
        }
    }
    // A diagnostic adds its underline last, so it survives the selection: the
    // cell keeps its syntax fg and selection bg and gains the severity's
    // underline/undercurl + `sp` color.
    for (start, end, severity, id) in diag {
        if col >= *start as usize && col < *end as usize {
            style = diagnostic_style(style, *severity, *id, theme);
            break; // server already widened/clipped diagnostic spans per row
        }
    }
    style
}

/// Compose a diagnostic underline onto `base`: the colorscheme's resolved
/// `DiagnosticUnderline*` style (its `sp` underline color + undercurl/underline
/// modifier) when loaded, else a built-in undercurl in the severity's color. The
/// cell's foreground/background are left untouched, so syntax and selection show
/// through with only the underline added.
fn diagnostic_style(base: Style, severity: u8, id: Option<usize>, theme: &LineTheme) -> Style {
    match id.and_then(|i| theme.palette.get(i)) {
        Some(style) => base.patch(*style),
        None => base
            .underline_color(severity_color(severity))
            .add_modifier(Modifier::UNDERLINED),
    }
}

/// The built-in underline color for a diagnostic severity (`1`=error … `4`=hint),
/// used when no colorscheme defines the `DiagnosticUnderline*` group. Indexed
/// ANSI colors keep it terminal-portable, matching the syntax/search fallbacks.
fn severity_color(severity: u8) -> Color {
    match severity {
        2 => Color::Yellow,   // warning
        3 => Color::Cyan,     // information
        4 => Color::DarkGray, // hint
        _ => Color::Red,      // error (and any unexpected code)
    }
}

/// Compose a search-match highlight onto `base`: the colorscheme's resolved
/// `Search`/`IncSearch` style when loaded, else a built-in `fallback`-on-black
/// highlight so matches stay visible with no theme.
fn search_style(base: Style, themed: Option<Style>, fallback: Color) -> Style {
    match themed {
        Some(style) => base.patch(style),
        None => base.bg(fallback).fg(Color::Black),
    }
}

/// Compose the visual selection onto `base`: the theme's `Visual` style (its
/// background swaps in, the cell's foreground is kept where `Visual` leaves it
/// unset) when a colorscheme is loaded, reverse-video otherwise (vim's default).
fn selection_style(base: Style, theme: &LineTheme) -> Style {
    match theme.visual {
        Some(visual) => base.patch(visual),
        None => base.add_modifier(Modifier::REVERSED),
    }
}

/// Map a treesitter capture group to a terminal style. Keyed on the group's
/// major family (the segment before the first `.`), so `function.call`,
/// `function.builtin`, … all share `function`'s color. This is the client's
/// theme — the *only* place that decides how a group looks. Unknown groups fall
/// back to the default foreground. Indexed ANSI colors keep it terminal-portable.
pub(crate) fn group_style(group: &str) -> Style {
    let major = group.split('.').next().unwrap_or(group);
    let style = Style::default();
    match major {
        "keyword" | "conditional" | "repeat" | "include" | "exception" | "keyword_operator" => {
            style.fg(Color::Magenta)
        }
        "function" | "method" => style.fg(Color::Blue),
        "constructor" | "type" | "namespace" | "module" => style.fg(Color::Yellow),
        "string" | "character" => style.fg(Color::Green),
        "number" | "boolean" | "float" | "constant" => style.fg(Color::Cyan),
        "attribute" | "label" | "property" | "field" => style.fg(Color::Cyan),
        "comment" => style.fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        "tag" => style.fg(Color::Red),
        "operator" | "punctuation" => style.fg(Color::Gray),
        _ => style,
    }
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

    // The theme's StatusLine when loaded; reverse-video out of the box.
    let style = view
        .status_line
        .unwrap_or_else(|| Style::default().add_modifier(Modifier::REVERSED));
    frame.render_widget(Paragraph::new(line).style(style), area);
}

fn render_command(frame: &mut Frame, area: Rect, view: &View) {
    let content = if view.command_mode {
        format!("{}{}", cmdline_prompt_str(view), view.cmdline)
    } else {
        view.message.clone()
    };
    frame.render_widget(Paragraph::new(content), area);
}

/// The leading prompt shown ahead of the editable command line: the multi-char
/// `vim.ui.input` label when set, else the single prefix char (`:`/`/`/`?`).
fn cmdline_prompt_str(view: &View) -> String {
    if view.cmdline_prompt.is_empty() {
        view.cmdline_prefix.to_string()
    } else {
        view.cmdline_prompt.clone()
    }
}

/// Display width (in cells, approximated as char count) of the leading prompt,
/// used to place the command cursor past it.
fn cmdline_prompt_width(view: &View) -> u16 {
    cmdline_prompt_str(view).chars().count() as u16
}

/// Render the bottom panel: a `─ Title ───────[X]─` top-border bar, then the
/// content rows with the focused (cursor) line highlighted across the full
/// width. The `[X]` at the right of the bar is the click-to-close button (see
/// [`close_button`]). Returns the inner content [`Rect`] so the caller can
/// place the editing cursor on the panel's current line.
fn render_panel(frame: &mut Frame, area: Rect, panel: &PanelData) -> Rect {
    let block = Block::new()
        .borders(Borders::TOP)
        .title_top(Line::from(format!(" {} ", panel.title)).left_aligned())
        .title_top(Line::from("[X]").right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width as usize;
    // The selected entry may span several rows when it word-wrapped; highlight the
    // whole span so a wrapped entry still reads as one focused line.
    let cursor_end = panel.cursor_row.saturating_add(panel.cursor_span.max(1));
    let rows: Vec<Line> = (0..inner.height)
        .map(|row| {
            let text = panel
                .lines
                .get(row as usize)
                .map(String::as_str)
                .unwrap_or("");
            if row >= panel.cursor_row && row < cursor_end {
                // Fill the cursor line to the full width so the highlight reads
                // as a selected row, not just selected text.
                let filled = format!("{text:<width$}");
                Line::from(Span::styled(
                    filled,
                    Style::default().add_modifier(Modifier::REVERSED),
                ))
            } else {
                Line::from(text.to_string())
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(Text::from(rows)), inner);
    inner
}

/// Draw the completion popup as a bordered overlay over the text area: a box
/// anchored at `(pmenu.col, pmenu.row)` in `text_area` cells — so its left edge
/// lines up under the completion word, past the number gutter — with each item on
/// its own row and the selected row reverse-highlighted. The box is `Clear`ed
/// first so the text beneath doesn't bleed through; the list scrolls to keep the
/// selected item visible when there are more items than rows. The box is clamped
/// to `text_area` so it never paints outside the editable region.
fn render_pmenu(frame: &mut Frame, text_area: Rect, pmenu: &PmenuData, doc_scroll: u16) {
    let Some(area) = popup_rect(text_area, pmenu) else {
        return;
    };
    let block = Block::new().borders(Borders::ALL);
    let inner = block.inner(area);
    // Clear the cells first so the overlay is opaque, then the border, then rows.
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let rows = inner.height as usize;
    let width = inner.width as usize;
    // Scroll so the selected item stays in view; show from the top otherwise.
    let start = match pmenu.selected {
        Some(s) if s >= rows => s + 1 - rows,
        _ => 0,
    };
    let lines: Vec<Line> = (0..inner.height)
        .map(|r| {
            let idx = start + r as usize;
            let Some((label, _kind, detail)) = pmenu.items.get(idx) else {
                return Line::from(" ".repeat(width));
            };
            let style = if Some(idx) == pmenu.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Line::from(Span::styled(pmenu_row(label, detail, width), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);

    // The documentation preview floats beside the popup (its own bordered box).
    render_pmenu_doc(frame, text_area, area, &pmenu.doc, doc_scroll);
}

/// The popup box rect (border included), anchored under the completion word and
/// clamped to `text_area`. `None` when it can't fit a border plus one content
/// cell each way. Shared by the renderer and the doc-box geometry.
fn popup_rect(text_area: Rect, pmenu: &PmenuData) -> Option<Rect> {
    let x = text_area.x.saturating_add(pmenu.col);
    let y = text_area.y.saturating_add(pmenu.row);
    // Content size plus a one-cell border all round, clamped to the text area.
    let width = pmenu
        .width
        .saturating_add(2)
        .min(text_area.right().saturating_sub(x));
    let height = pmenu
        .height
        .saturating_add(2)
        .min(text_area.bottom().saturating_sub(y));
    let area = Rect {
        x,
        y,
        width,
        height,
    };
    (area.width >= 3 && area.height >= 3).then_some(area)
}

/// The documentation preview box's rect (border included) and its maximum scroll
/// offset (wrapped content height minus the visible inner rows, `0` when it all
/// fits) — laid out beside the `popup` within `text_area`: to its right when
/// there's room, else to its left (vim's `completeopt=popup` shape), top-aligned
/// with the popup. `None` when there are no docs or no room either side. Pure, so
/// the renderer and the wheel hit-test share one layout.
fn doc_rect(text_area: Rect, popup: Rect, doc: &[String]) -> Option<(Rect, u16)> {
    if doc.is_empty() {
        return None;
    }
    // Cap the preview so a long doc block doesn't swallow the screen.
    const MAX_W: u16 = 50;
    const MAX_H: u16 = 12;
    let natural_w = doc.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
    let want_box_w = natural_w.clamp(1, MAX_W).saturating_add(2);

    // Prefer the right of the popup; fall back to its left. Each side needs the
    // border plus at least one content cell (3 cells) to be worth drawing.
    let room_right = text_area.right().saturating_sub(popup.right());
    let room_left = popup.x.saturating_sub(text_area.x);
    let (x, box_w) = if room_right >= 3 {
        (popup.right(), want_box_w.min(room_right))
    } else if room_left >= 3 {
        let w = want_box_w.min(room_left);
        (popup.x.saturating_sub(w), w)
    } else {
        return None; // no room either side
    };

    // Height from the wrapped line count, clamped to the cap and the room below.
    let content_w = box_w.saturating_sub(2).max(1);
    let wrapped = doc
        .iter()
        .map(|l| (l.chars().count() as u16).max(1).div_ceil(content_w))
        .sum::<u16>();
    let content_h = wrapped.clamp(1, MAX_H);
    let box_h = content_h
        .saturating_add(2)
        .min(text_area.bottom().saturating_sub(popup.y));
    if box_w < 3 || box_h < 3 {
        return None;
    }
    let area = Rect {
        x,
        y: popup.y,
        width: box_w,
        height: box_h,
    };
    // Visible content rows = inner height; anything past that is reachable only by
    // scrolling.
    let max_scroll = wrapped.saturating_sub(area.height.saturating_sub(2));
    Some((area, max_scroll))
}

/// Draw the selected item's documentation in a bordered preview box beside the
/// popup, scrolled down `doc_scroll` lines (clamped so it can't scroll past the
/// end). Content is wrapped to the box width and clipped to the box; nothing is
/// drawn when there are no docs or no room beside the popup. Markdown is shown as
/// plain lines (the server's markup distiller yields lines, not styled spans).
fn render_pmenu_doc(
    frame: &mut Frame,
    text_area: Rect,
    popup: Rect,
    doc: &[String],
    doc_scroll: u16,
) {
    let Some((area, max_scroll)) = doc_rect(text_area, popup, doc) else {
        return;
    };
    let scroll = doc_scroll.min(max_scroll);
    let block = Block::new().borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let text = Text::from(
        doc.iter()
            .map(|l| Line::from(l.as_str()))
            .collect::<Vec<_>>(),
    );
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        inner,
    );
}

/// Headless geometry of the completion doc preview box for a `width`x`height`
/// terminal showing `view`: `(x, y, width, height, max_scroll)` in screen cells,
/// or `None` when no preview is drawn. `max_scroll` is the largest `doc_scroll`
/// that still reveals new content. The event loop uses it to hit-test the mouse
/// wheel and clamp the scroll; a test uses it to find the box. Mirrors the layout
/// in [`render`] (same reason as [`close_button`]).
#[doc(hidden)]
pub fn pmenu_doc_geometry(
    width: u16,
    height: u16,
    view: &View,
) -> Option<(u16, u16, u16, u16, u16)> {
    let pmenu = view.pmenu.as_ref()?;
    // Recompute the same regions `render` lays out, so the box lands identically.
    let panel_rows = view.panel.as_ref().map_or(0, |p| p.height + 1);
    let regions = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(panel_rows),
        Constraint::Length(1),
    ])
    .split(Rect::new(0, 0, width, height));
    let text_area = regions[0];
    let text_inner = if view.number_width > 0 {
        Layout::horizontal([Constraint::Length(view.number_width), Constraint::Min(0)])
            .split(text_area)[1]
    } else {
        text_area
    };
    let popup = popup_rect(text_inner, pmenu)?;
    let (area, max_scroll) = doc_rect(text_inner, popup, &pmenu.doc)?;
    Some((area.x, area.y, area.width, area.height, max_scroll))
}

/// One popup row padded to `width` cells: the `label` left-aligned, and the
/// `detail` (a type/source hint) right-aligned when it fits after a one-cell gap.
/// A too-long label is truncated. Char-count widths are exact for the ASCII
/// identifiers completion labels usually are.
fn pmenu_row(label: &str, detail: &str, width: usize) -> String {
    let label: String = label.chars().take(width).collect();
    let label_w = label.chars().count();
    let detail_w = detail.chars().count();
    if !detail.is_empty() && label_w + 1 + detail_w <= width {
        let pad = width - label_w - detail_w;
        format!("{label}{}{detail}", " ".repeat(pad))
    } else {
        format!("{label:<width$}")
    }
}

/// Screen position of the panel's `[X]` close button on a `width`x`height`
/// terminal showing a panel of content height `panel_height`: its top-border
/// row and the 3-cell column range the `[X]` occupies. `None` when the terminal
/// has no room to lay the panel out. Pure (no rendering) so the event loop's
/// click hit-test and a test can share one definition.
///
/// Layout mirrors [`render`]: from the bottom up, one command row, then the
/// panel's `panel_height + 1` rows (the panel sits below the status line) — so
/// the border row is `height - 1 - (panel_height + 1)`.
///
/// `#[doc(hidden)] pub` so a Tier-1 test can pin this geometry against the
/// painted `[X]`; not part of the client's runtime API.
#[doc(hidden)]
pub fn close_button(
    width: u16,
    height: u16,
    panel_height: u16,
) -> Option<(u16, std::ops::Range<u16>)> {
    let row = height.checked_sub(panel_height + 2)?;
    let start = width.checked_sub(3)?;
    Some((row, start..width))
}
