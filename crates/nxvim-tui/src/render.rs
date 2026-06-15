//! The renderer: lays the three regions out and paints each with a ratatui
//! widget, plus the headless [`paint`]/[`ScrollHarness`] test entry points.

use crossterm::cursor::SetCursorStyle;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use rmpv::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::anim::{arm_animation, lerp, Animation};
use nxvim_view::{
    DiagSign, DiagSpan, DiagVirt, HlSpan, IncSearchSpans, InlayHint, MenuData, MenuPreview,
    PanelData, PmenuData, RegionTabline, SearchSpans, Separator, StatusSegment, TabData, View,
    WindowRegion, WindowView,
};

/// Width in cells of the diagnostic sign column when reserved (vim's fixed
/// 2-cell `signcolumn`, independent of the number gutter's width).
const SIGN_WIDTH: u16 = 2;

/// Convert a neutral [`nxvim_view::Style`] into the ratatui [`Style`] the renderer
/// paints. `fg`/`bg` become truecolor, `sp` the underline color, and each flag its
/// modifier. ratatui has no undercurl modifier, so `undercurl` aliases to
/// `UNDERLINED` (the `sp` underline color still distinguishes it). Absent fields
/// stay unset so the style patches cleanly onto whatever it is painted over.
fn rt(s: nxvim_view::Style) -> Style {
    let mut style = Style::default();
    if let Some(c) = s.fg {
        style = style.fg(rgb(c));
    }
    if let Some(c) = s.bg {
        style = style.bg(rgb(c));
    }
    if let Some(c) = s.sp {
        style = style.underline_color(rgb(c));
    }
    if s.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if s.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if s.underline || s.undercurl {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if s.strikethrough {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    if s.reverse {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

/// Unpack a `0xRRGGBB` color integer into a truecolor [`Color::Rgb`]. The top
/// byte is ignored (the wire never sets it).
fn rgb(c: u32) -> Color {
    let [_, r, g, b] = c.to_be_bytes();
    Color::Rgb(r, g, b)
}

/// Map a neutral float [`nxvim_view::Border`] to the ratatui [`BorderType`] used
/// to draw it. `Solid` (neovim's space border) renders as the nearest line style.
fn bt(b: nxvim_view::Border) -> BorderType {
    match b {
        nxvim_view::Border::Single => BorderType::Plain,
        nxvim_view::Border::Rounded => BorderType::Rounded,
        nxvim_view::Border::Double => BorderType::Double,
        nxvim_view::Border::Solid => BorderType::QuadrantInside,
    }
}

/// Render `view` into a `width`x`height` cell grid using ratatui's test backend
/// and return the painted buffer. This drives the *same* `render` the live
/// client uses, so tests assert on exactly what a user would see.
pub fn paint(view: &View, width: u16, height: u16) -> ratatui::buffer::Buffer {
    paint_doc_scrolled(view, width, height, 0)
}

/// Like [`paint`], but also returns the terminal cursor position the frame
/// placed (`None` when hidden). The hook a test uses to assert where the cursor
/// landed — e.g. inside a focused float's inner area. Not part of the runtime API.
#[doc(hidden)]
pub fn paint_with_cursor(
    view: &View,
    width: u16,
    height: u16,
) -> (ratatui::buffer::Buffer, Option<(u16, u16)>) {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal
        .draw(|frame| render(frame, view, None, 0))
        .expect("draw");
    let cursor = terminal.get_cursor_position().ok().map(|p| (p.x, p.y));
    (terminal.backend().buffer().clone(), cursor)
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

/// Lay the frame out and paint it: each window at its rect (gutter + text body +
/// its own status line), the split separators between them, then the global
/// command line, completion popup, and panel. The windows area is the frame
/// minus the global bottom panel and command line; with one window it spans that
/// whole area, so the output matches the pre-windows single-window frame exactly.
/// When `anim` is present it animates the **focused** window's slide.
pub(crate) fn render(frame: &mut Frame, view: &View, anim: Option<&Animation>, doc_scroll: u16) {
    // The panel docks below all windows, claiming `height + 1` rows (content plus
    // its title bar); `0` when none is open. The command line is the last row.
    // Each window draws its own status line at the bottom of its rect, so there
    // is no longer a global status row here.
    let panel_rows = view.panel.as_ref().map_or(0, |p| p.height + 1);
    // The tabline claims one row at the very top when ≥2 tabs are open (matching
    // the server's windows-area shrink); `0` otherwise, so single-tab frames are
    // unchanged.
    let tabline_rows = u16::from(!view.tabline.is_empty());
    // The global status line (`laststatus=3`) claims one row docked below all
    // windows (matching the server's windows-area shrink); `0` for per-window modes.
    let global_status_rows = u16::from(!view.global_status.is_empty());
    // The permanent dock bands. Each open dock reserves its content extent plus one
    // separator cell toward the main area; all are `0` (and the layout collapses to
    // the pre-dock form) when no dock is open.
    let dock = DockLayout::new(
        frame.area(),
        view,
        tabline_rows,
        global_status_rows,
        panel_rows,
    );
    let (tabline_area, panel_area, cmd_area) = (dock.tabline, dock.panel, dock.cmd);
    let global_status_area = dock.global_status;

    if tabline_rows > 0 {
        render_tabline(frame, tabline_area, view);
    }
    // Each open dock paints its own tabline into its band's first row.
    dock.render_dock_tablines(frame, view);

    // Paint each window into its region (main area or a dock); capture the focused
    // one's text-inner rect and (possibly interpolated) cursor row for the terminal
    // cursor and the popup anchor. Tiled windows paint first; floats overlay them.
    let mut focused_inner: Option<(Rect, u16, u16)> = None;
    for win in view.windows.iter().filter(|w| !w.floating) {
        let area = window_area(dock.content(win.region), win);
        // Only the focused window animates a scroll slide.
        let win_anim = if win.focused { anim } else { None };
        let (text_inner, cursor_row, cursor_shift) =
            render_window(frame, area, win, view, win_anim);
        if win.focused {
            focused_inner = Some((text_inner, cursor_row, cursor_shift));
        }
    }

    // Per-tree split borders (each offset by its region origin), then the dock/main
    // border lines.
    render_separators(frame, &dock, &view.separators, view);
    dock.render_borders(frame, view);

    // Floats, on top, in list order (the server already sorts them by zindex). A
    // float is opaque (`Clear`) over what it covers, draws its border + title, and
    // paints its own gutter/text/status inside. A float never scroll-animates.
    for win in view.windows.iter().filter(|w| w.floating) {
        let outer = window_area(dock.content(win.region), win);
        frame.render_widget(Clear, outer);
        let inner = match win.border {
            Some(border) => {
                let block = float_block(bt(border), win.title.as_deref());
                let inner = block.inner(outer);
                frame.render_widget(block, outer);
                inner
            }
            None => outer,
        };
        let (text_inner, cursor_row, cursor_shift) = render_window(frame, inner, win, view, None);
        if win.focused {
            focused_inner = Some((text_inner, cursor_row, cursor_shift));
        }
    }

    // The single global status line (`laststatus=3`), docked below all windows.
    if global_status_rows > 0 {
        render_status(frame, global_status_area, &view.global_status, view);
    }

    render_command(frame, cmd_area, view);

    // The insert-mode completion popup floats over the focused window's text area,
    // drawn after the windows so it sits on top.
    if let (Some(pmenu), Some((inner, _, _))) = (&view.pmenu, focused_inner) {
        render_pmenu(frame, inner, pmenu, doc_scroll);
    }

    // The floating selectable-list menu (`nx.ui.select`) floats the same way,
    // over the focused window's text area, with input focus. A picker returns its
    // prompt caret position so we can draw the terminal cursor there.
    let menu_cursor = match (&view.menu, focused_inner) {
        (Some(menu), Some((inner, _, _))) => render_menu(frame, inner, menu, &view.styles),
        _ => None,
    };

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

    // An open picker owns the cursor: draw it in the prompt, not the text window
    // behind the float.
    if let Some(pos) = menu_cursor {
        frame.set_cursor_position(pos);
        return;
    }

    if view.command_mode {
        // Offset past the leading prompt — a single prefix char (`:`/`/`/`?`) or
        // the multi-char `vim.ui.input` label; the cursor then follows
        // `cmdline_cursor` (a char offset) so it sits mid-line after edits.
        let prompt_width = cmdline_prompt_width(view);
        let col = cmd_area.x + prompt_width + view.cmdline_cursor as u16;
        frame.set_cursor_position((col, cmd_area.y));
    } else if let (Some((inner, cursor_row, shift)), Some(win)) = (focused_inner, view.focused()) {
        // The cursor row is interpolated during a slide, but the column comes
        // straight from the focused window — correct because the scroll commands
        // move only vertically. The horizontal scroll offset (`leftcol`) shifts the
        // painted text left, so the cursor follows by the same amount; the core
        // keeps the cursor on screen, so this normally lands inside the text area.
        // The `.min` is a safety clamp (a stale frame, a degenerate width) so a
        // cursor can never be drawn past a window's right edge — escaping a narrow
        // float or vsplit onto whatever sits beside it. (Rows are already bounded
        // by the window's text height.)
        // `shift` is how far the inline inlay hints on the cursor's row push it
        // right (computed by `render_window` from the same band/window hints it
        // painted, so the cursor tracks the splice during the slide and once settled).
        let col = inner.x
            + (win.cursor_screen_col + shift)
                .saturating_sub(win.leftcol)
                .min(inner.width.saturating_sub(1));
        let row = inner.y + cursor_row.min(inner.height.saturating_sub(1));
        frame.set_cursor_position((col, row));
        // A block cursor (normal / visual) envelops the full display width of the
        // grapheme it sits on — a wide CJK/emoji glyph, or a `^X` / `<xx>`
        // control-char token. The terminal's one hardware cursor covers `col`; its
        // trailing cells are painted as a clean reverse-video block so the whole
        // token reads as covered. The cells are reset to the *default* fg/bg before
        // reversing — otherwise a `<xx>` token's `SpecialKey` foreground would
        // reverse into a coloured background instead of matching the cursor's plain
        // block (a wide glyph has no such fg, which is why it already looked right).
        // The thin bar (insert) / underline (replace) shapes don't envelop.
        if !view.is_insert() && !view.is_replace() {
            for extra in 1..win.cursor_width {
                let x = col + extra;
                if x >= inner.right() {
                    break;
                }
                if let Some(cell) = frame.buffer_mut().cell_mut((x, row)) {
                    cell.set_style(Style::reset().add_modifier(Modifier::REVERSED));
                }
            }
        }
    }
}

/// The absolute screen rectangles for each render region this frame: the main
/// area, the four permanent docks, and the chrome rows. The whole frame stacks
/// vertically as `[top dock][tabline][left|main|right][global status][bottom
/// dock][panel][cmd]`; each open dock reserves its content extent plus one
/// separator cell toward the main area. With no dock open every band is `0` and
/// the layout collapses to the pre-dock `[tabline][main][global status][panel]
/// [cmd]` form, so a dock-free frame is unchanged.
struct DockLayout {
    main: Rect,
    left: Rect,
    right: Rect,
    top: Rect,
    bottom: Rect,
    tabline: Rect,
    global_status: Rect,
    panel: Rect,
    cmd: Rect,
    /// The `[left|main|right]` band, for full-height left/right dock borders.
    mid: Rect,
    /// Each dock's own tabline row (its band's first row), `None` when that dock
    /// draws no tabline. The dock's content rect above already excludes this row.
    tl_left: Option<Rect>,
    tl_right: Option<Rect>,
    tl_top: Option<Rect>,
    tl_bottom: Option<Rect>,
    /// Per-side content extents (cells), `0` when closed.
    dl: u16,
    dr: u16,
    dt: u16,
    db: u16,
}

impl DockLayout {
    fn new(
        area: Rect,
        view: &View,
        tabline_rows: u16,
        global_status_rows: u16,
        panel_rows: u16,
    ) -> Self {
        // A dock reserves `content + 1` cells (the `+1` its separator), `0` closed.
        let res = |n: u16| if n > 0 { n + 1 } else { 0 };
        let (dl, dr, dt, db) = (
            view.dock_left,
            view.dock_right,
            view.dock_top,
            view.dock_bottom,
        );
        let v = Layout::vertical([
            Constraint::Length(res(dt)),            // top dock (above the tabline)
            Constraint::Length(tabline_rows),       // tabline
            Constraint::Min(1),                     // mid: left | main | right
            Constraint::Length(global_status_rows), // global status (laststatus=3)
            Constraint::Length(res(db)),            // bottom dock (above the panel)
            Constraint::Length(panel_rows),         // read-only panel
            Constraint::Length(1),                  // command line
        ])
        .split(area);
        let (top_band, tabline, mid, global_status, bottom_band, panel, cmd) =
            (v[0], v[1], v[2], v[3], v[4], v[5], v[6]);
        let h = Layout::horizontal([
            Constraint::Length(res(dl)), // left dock
            Constraint::Min(1),          // main
            Constraint::Length(res(dr)), // right dock
        ])
        .split(mid);
        let (left_band, main, right_band) = (h[0], h[1], h[2]);
        // Content rects exclude the one separator cell facing the main area: the
        // top dock's separator is its bottom row, the left dock's its right column,
        // and the bottom/right docks' separators face the main area too (so their
        // content starts one cell in).
        let top = Rect::new(top_band.x, top_band.y, top_band.width, dt);
        let bottom = Rect::new(
            bottom_band.x,
            bottom_band.y + res(db) - db,
            bottom_band.width,
            db,
        );
        let left = Rect::new(left_band.x, left_band.y, dl, left_band.height);
        let right = Rect::new(
            right_band.x + res(dr) - dr,
            right_band.y,
            dr,
            right_band.height,
        );
        // Each dock reserves the first row of its content rect for its own tabline
        // (when that dock's region tabline is non-empty). The stored content rects
        // stay uncarved (so the dock-edge borders keep their band-relative
        // positions); `content()` shifts a window down past its tabline, mirroring
        // the row the core relayout removed from the dock tree. `tl` is the tabline
        // row itself (the content rect's first row), `None` when no tabline shows.
        let tl = |content: Rect, visible: bool| -> Option<Rect> {
            (visible && content.height > 1)
                .then(|| Rect::new(content.x, content.y, content.width, 1))
        };
        let rt = &view.region_tablines;
        let tl_top = tl(top, !rt.top.tabs.is_empty());
        let tl_bottom = tl(bottom, !rt.bottom.tabs.is_empty());
        let tl_left = tl(left, !rt.left.tabs.is_empty());
        let tl_right = tl(right, !rt.right.tabs.is_empty());
        DockLayout {
            main,
            left,
            right,
            top,
            bottom,
            tabline,
            global_status,
            panel,
            cmd,
            mid,
            tl_left,
            tl_right,
            tl_top,
            tl_bottom,
            dl,
            dr,
            dt,
            db,
        }
    }

    /// The content rect a window of `region` is offset against — shifted down past
    /// the region's own tabline row when it has one (docks only; the main tabline
    /// is the global top row handled separately).
    fn content(&self, region: WindowRegion) -> Rect {
        let (rect, tabline) = match region {
            WindowRegion::Main => (self.main, None),
            WindowRegion::DockLeft => (self.left, self.tl_left),
            WindowRegion::DockRight => (self.right, self.tl_right),
            WindowRegion::DockTop => (self.top, self.tl_top),
            WindowRegion::DockBottom => (self.bottom, self.tl_bottom),
        };
        match tabline {
            Some(_) => Rect::new(
                rect.x,
                rect.y + 1,
                rect.width,
                rect.height.saturating_sub(1),
            ),
            None => rect,
        }
    }

    /// Paint each open dock's own tabline into its reserved band row (the main
    /// tabline is the global top row, drawn separately). A dock with no tabline
    /// this frame has `None` here and paints nothing.
    fn render_dock_tablines(&self, frame: &mut Frame, view: &View) {
        let rt = &view.region_tablines;
        let docks: [(Option<Rect>, &RegionTabline); 4] = [
            (self.tl_left, &rt.left),
            (self.tl_right, &rt.right),
            (self.tl_top, &rt.top),
            (self.tl_bottom, &rt.bottom),
        ];
        for (area, region) in docks {
            if let Some(area) = area {
                render_tab_cells(frame, area, &region.title, &region.tabs, region.current);
            }
        }
    }

    /// Paint the border line between each open dock and the main area. Drawn with
    /// the **heavy** box-drawing glyphs (`━`/`┃`) so a permanent dock edge reads as
    /// distinct from the light (`─`/`│`) borders between ordinary window splits.
    fn render_borders(&self, frame: &mut Frame, view: &View) {
        let style = view
            .status_line
            .map(rt)
            .unwrap_or_else(|| Style::default().add_modifier(Modifier::REVERSED));
        let hline = |frame: &mut Frame, x: u16, y: u16, len: u16| {
            frame.render_widget(
                Paragraph::new(Span::styled("━".repeat(len as usize), style)),
                Rect::new(x, y, len, 1),
            );
        };
        let vline = |frame: &mut Frame, x: u16, y: u16, len: u16| {
            let rows: Vec<Line> = (0..len)
                .map(|_| Line::from(Span::styled("┃", style)))
                .collect();
            frame.render_widget(Paragraph::new(Text::from(rows)), Rect::new(x, y, 1, len));
        };
        if self.dt > 0 {
            hline(frame, self.top.x, self.top.y + self.dt, self.top.width);
        }
        if self.db > 0 {
            hline(frame, self.bottom.x, self.bottom.y - 1, self.bottom.width);
        }
        if self.dl > 0 {
            vline(frame, self.left.x + self.dl, self.mid.y, self.mid.height);
        }
        if self.dr > 0 {
            vline(frame, self.right.x - 1, self.mid.y, self.mid.height);
        }
    }
}

/// A window's absolute rect: its wire rect offset by its region's content origin,
/// or the whole region for a legacy flat redraw (no rect → single window).
fn window_area(wins_area: Rect, win: &WindowView) -> Rect {
    match win.rect {
        Some(r) => Rect {
            x: wins_area.x + r.x,
            y: wins_area.y + r.y,
            width: r.width.min(wins_area.width.saturating_sub(r.x)),
            height: r.height.min(wins_area.height.saturating_sub(r.y)),
        },
        None => wins_area,
    }
}

/// The bordered [`Block`] for a float: `BorderType` glyphs all round, with the
/// `title` (when present) on the top border, padded with a space each side so it
/// reads as a label rather than running into the corners. Left-aligned, matching
/// neovim's default `title_pos = "left"`.
fn float_block(border: BorderType, title: Option<&str>) -> Block<'static> {
    let mut block = Block::new().borders(Borders::ALL).border_type(border);
    if let Some(title) = title {
        block = block.title_top(Line::from(format!(" {title} ")).left_aligned());
    }
    block
}

/// A float's inner content rect (past its border), or the whole `area` for a
/// borderless float. Shared by the renderer and [`text_inner_rect`] so a focused
/// float's cursor/popup anchor lands on the cells the border left for content.
fn float_inner(area: Rect, border: Option<BorderType>) -> Rect {
    match border {
        Some(border) => float_block(border, None).inner(area),
        None => area,
    }
}

/// Paint one window into `area`: the theme background, the number gutter, the
/// text body (or an interpolated slide when `anim` is set), and a status line on
/// the bottom row. Returns the text-inner rect (past the gutter) and the
/// (possibly interpolated) cursor row, for placing the terminal cursor.
fn render_window(
    frame: &mut Frame,
    area: Rect,
    win: &WindowView,
    view: &View,
    anim: Option<&Animation>,
) -> (Rect, u16, u16) {
    // The status line is the window's bottom row (when this window shows one, per
    // `'laststatus'`); the text body is the rest. With no per-window status row the
    // text body claims the whole rect — the server already sized `lines` to fill it.
    let (text_area, status_area) = if win.status_visible {
        let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
        (rows[0], Some(rows[1]))
    } else {
        (area, None)
    };
    let height = text_area.height as usize;

    // Empty fallbacks for the overlays a slide band does not carry (diagnostics,
    // signs, secondary selections). Search *is* carried (see `anim_search` below),
    // so `hlsearch` keeps highlighting while the view slides.
    let empty_search: SearchSpans = Vec::new();
    let empty_diag: Vec<Vec<DiagSpan>> = Vec::new();
    let empty_virt: Vec<Option<DiagVirt>> = Vec::new();
    let empty_signs: Vec<Option<DiagSign>> = Vec::new();

    // Owned slide-band snapshots, populated only while animating (a `skip/take`
    // window of the full buffer). The static path — the overwhelmingly common
    // case — borrows straight from `win` instead of deep-cloning the whole
    // viewport (lines, per-cell highlights, selection, numbers) every repaint.
    let anim_lines: Vec<String>;
    let anim_sel: Vec<Option<(u16, u16)>>;
    let anim_hl: Vec<Vec<HlSpan>>;
    let anim_numbers: Vec<Option<usize>>;
    let anim_inlay: Vec<Vec<InlayHint>>;
    let anim_search: SearchSpans;
    let anim_incsearch: IncSearchSpans;
    let frame_lines: &[String];
    let frame_sel: &[Option<(u16, u16)>];
    let frame_hl: &[Vec<HlSpan>];
    let frame_inlay: &[Vec<InlayHint>];
    let frame_numbers: &[Option<usize>];
    // Search spans ride the slide band (the static viewport uses the win's), so
    // `hlsearch`/`incsearch` keep highlighting the moving text instead of blinking
    // off until the slide settles. Assigned inside the match below (where the band
    // snapshots are initialized), like `frame_hl`.
    let frame_search: &SearchSpans;
    let frame_incsearch: &IncSearchSpans;
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
            anim_lines = a.lines.iter().skip(off).take(height).cloned().collect();
            // Grow/shrink the selection's moving edge in step with the slide. The
            // band carries the selection over the *maximal* extent the slide
            // touches; mid-slide, only the rows on the anchor side of the
            // interpolated cursor are highlighted, so the selection tracks the
            // scroll instead of snapping to its full extent (or vanishing) on frame
            // 0. The clip side follows the *selection orientation*, not the scroll
            // direction: anchor above ⇒ extends down ⇒ hide rows past the cursor
            // below; anchor below ⇒ extends up ⇒ hide above. `None` when no visual
            // selection is sliding (the band is all-empty anyway).
            anim_sel = {
                let mut sel: Vec<Option<(u16, u16)>> =
                    a.selection.iter().skip(off).take(height).copied().collect();
                if let Some(down) = a.sel_extends_down {
                    for (j, span) in sel.iter_mut().enumerate() {
                        let line = top + j; // 0-based buffer line of this visible row
                        let past = if down { line > cur } else { line < cur };
                        if past {
                            *span = None;
                        }
                    }
                }
                sel
            };
            anim_numbers = a.numbers.iter().skip(off).take(height).copied().collect();
            anim_hl = a
                .highlights
                .iter()
                .skip(off)
                .take(height)
                .cloned()
                .collect();
            anim_inlay = a
                .inlay_hints
                .iter()
                .skip(off)
                .take(height)
                .cloned()
                .collect();
            // Search matches ride the slide too, sliced to the visible window like
            // the highlights, so `hlsearch` stays lit on the moving text.
            anim_search = a.search.iter().skip(off).take(height).cloned().collect();
            anim_incsearch = a.incsearch.iter().skip(off).take(height).copied().collect();
            frame_lines = &anim_lines;
            frame_sel = &anim_sel;
            frame_numbers = &anim_numbers;
            frame_hl = &anim_hl;
            frame_inlay = &anim_inlay;
            frame_search = &anim_search;
            frame_incsearch = &anim_incsearch;
            cursor_row = cur.saturating_sub(top) as u16;
            current_line = cur + 1;
        }
        None => {
            frame_lines = &win.lines;
            frame_sel = &win.selection;
            frame_numbers = &win.numbers;
            frame_hl = &win.highlights;
            frame_inlay = &win.inlay_hints;
            frame_search = &win.search;
            frame_incsearch = &win.incsearch;
            cursor_row = win.cursor_row;
            current_line = win.cursor_line;
        }
    }

    // Paint the text body with the theme's `Normal` background first (when a
    // colorscheme is loaded), so every following widget's spans patch their
    // foreground onto it and the gutter, end-of-line gaps, and `~` rows all share
    // the editor background. With no theme this is skipped.
    if let Some(normal) = view.normal.map(rt) {
        frame.render_widget(Block::default().style(normal), text_area);
    }

    // Reserve a 2-cell diagnostic sign column at the far left (vim's signcolumn,
    // left of the number gutter) when this window's buffer has diagnostics and
    // signs are on. Its glyphs are painted below, once the style palette is built.
    let (sign_area, gutter_area) = if win.sign_column {
        let cols = Layout::horizontal([Constraint::Length(SIGN_WIDTH), Constraint::Min(0)])
            .split(text_area);
        (Some(cols[0]), cols[1])
    } else {
        (None, text_area)
    };

    // Split a number-column gutter off the left of the remaining body when enabled.
    let text_inner = if win.number_width > 0 {
        let cols = Layout::horizontal([Constraint::Length(win.number_width), Constraint::Min(0)])
            .split(gutter_area);
        render_gutter(frame, cols[0], frame_numbers, current_line, win, view);
        cols[1]
    } else {
        gutter_area
    };

    // Token style ids index a palette captured with the frame they belong to:
    // the in-flight animation's snapshot while sliding, else the live view's. The
    // neutral palette is converted to ratatui styles once here so `cell_style` can
    // compose them with `.patch`/`.fg`/… as it walks each row's cells.
    let palette: Vec<Style> = match anim {
        Some(a) => &a.styles,
        None => &view.styles,
    }
    .iter()
    .copied()
    .map(rt)
    .collect();
    let theme = LineTheme {
        palette: &palette,
        visual: view.visual.map(rt),
        search: view.search_style.map(rt),
        incsearch: view.incsearch_style.map(rt),
        end_of_buffer: view.end_of_buffer.map(rt),
    };
    // Secondary multi-cursor selections paint on the settled viewport only.
    let frame_secondary_sel: &SearchSpans = match anim {
        Some(_) => &empty_search,
        None => &win.secondary_selection,
    };
    // Diagnostics, like search, are painted on the settled viewport only.
    let frame_diag: &[Vec<DiagSpan>] = match anim {
        Some(_) => &empty_diag,
        None => &win.diagnostics,
    };
    let frame_virt: &[Option<DiagVirt>] = match anim {
        Some(_) => &empty_virt,
        None => &win.diagnostics_virt,
    };
    // Signs, like diagnostics, are painted on the settled viewport only — a slide
    // band carries none (the reserved column stays blank while animating, so the
    // text below it doesn't jump). Painted now that the palette resolves style ids.
    if let Some(sign_area) = sign_area {
        let frame_signs: &[Option<DiagSign>] = match anim {
            Some(_) => &empty_signs,
            None => &win.diagnostics_signs,
        };
        render_sign_column(frame, sign_area, frame_signs, &palette);
    }
    render_text(
        frame,
        text_inner,
        frame_lines,
        frame_sel,
        frame_secondary_sel,
        frame_search,
        frame_incsearch,
        frame_hl,
        frame_diag,
        frame_virt,
        frame_inlay,
        frame_numbers,
        win.tabstop.max(1) as usize,
        win.leftcol as usize,
        &theme,
    );
    // Paint the secondary multi-cursors over the settled text. They're a
    // static-state decoration (like search / diagnostics), so skip them mid-slide
    // — the interpolated positions wouldn't line up with the projected ones.
    if anim.is_none() {
        render_secondary_cursors(frame, text_inner, win, view);
        // In placement mode, recolor the active (primary) cursor cell so it reads
        // as "dropping cursors", distinct from the reverse-video placed ones.
        if win.focused && view.is_multicursor() {
            paint_cursor_cell(
                frame,
                text_inner,
                win.cursor_screen_col,
                win.cursor_row,
                win.leftcol,
                Style::default()
                    .bg(MULTICURSOR_ACCENT)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            );
        }
    }
    if let Some(status_area) = status_area {
        render_status(frame, status_area, &win.status, view);
    }
    // How far the inline inlay hints on the cursor's row push the cursor right —
    // computed from `frame_inlay`, so it's the band's hints mid-slide and the
    // window's once settled (both indexed by the cursor's screen row). Returned so
    // the caller places the focused window's cursor past the splice in either case.
    let cursor_shift =
        inlay_cursor_shift(frame_inlay, cursor_row, win.cursor_screen_col, win.leftcol);
    (text_inner, cursor_row, cursor_shift)
}

/// Paint each secondary multi-cursor as a styled cell over the already-rendered
/// text. The terminal has only one real cursor (the primary, placed via
/// `set_cursor_position`); the extra cursors are shown this way. Their look tracks
/// the same mode-driven cursor *shape* the primary uses (`cursor_style`): a block
/// cursor (normal/visual) paints reverse-video, while the bar (insert) and
/// underline (replace) shapes — neither paintable in a single cell — both show as
/// an underline (tinted with the multi-cursor accent so it doesn't read as one of
/// the text's own underlines), so a mode change propagates to every cursor.
/// Positions off the horizontal scroll or past the text edges are dropped,
/// matching the primary cursor's clamp.
fn render_secondary_cursors(frame: &mut Frame, text_inner: Rect, win: &WindowView, view: &View) {
    // The block shape (normal/visual) paints reverse-video; the bar (insert) and
    // underline (replace) shapes — neither paintable in a cell — both show as an
    // underline, tinted with the multi-cursor accent so it reads as a cursor rather
    // than blending into the text's own (default-colored) underlines.
    let patch = if view.is_insert() || view.is_replace() {
        Style::default()
            .add_modifier(Modifier::UNDERLINED)
            .underline_color(MULTICURSOR_ACCENT)
    } else {
        Style::default().add_modifier(Modifier::REVERSED)
    };
    for &(row, col) in &win.secondary_cursors {
        paint_cursor_cell(frame, text_inner, col, row, win.leftcol, patch);
    }
}

/// The multi-cursor accent (a warm amber): the active cursor's background in
/// MULTICURSOR placement mode, and the secondary cursors' underline color in
/// insert/replace mode, so every multi-cursor decoration reads as one family.
const MULTICURSOR_ACCENT: Color = Color::Rgb(229, 192, 123);

/// Patch the cell at window-relative `(row, screen_col)` with `patch`, applying
/// the same `leftcol` horizontal shift the text gets and dropping anything off
/// the scroll or past the text edges (matching the primary cursor's clamp).
fn paint_cursor_cell(
    frame: &mut Frame,
    text_inner: Rect,
    screen_col: u16,
    row: u16,
    leftcol: u16,
    patch: Style,
) {
    let Some(rel_col) = screen_col.checked_sub(leftcol) else {
        return;
    };
    let x = text_inner.x + rel_col;
    let y = text_inner.y + row;
    if x >= text_inner.right() || y >= text_inner.bottom() {
        return;
    }
    if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
        let style = cell.style().patch(patch);
        cell.set_style(style);
    }
}

/// Draw the split separators between windows: a vertical `│` or horizontal `─`
/// run, anchored in the windows area. The theme's `StatusLine` style tints them
/// (reverse-video out of the box), matching vim's `WinSeparator` default of
/// reusing the status-line look. None to draw with a single window.
fn render_separators(frame: &mut Frame, dock: &DockLayout, separators: &[Separator], view: &View) {
    let style = view
        .status_line
        .map(rt)
        .unwrap_or_else(|| Style::default().add_modifier(Modifier::REVERSED));
    for sep in separators {
        // Each separator is relative to its region's content origin.
        let wins_area = dock.content(sep.region);
        let x = wins_area.x + sep.x;
        let y = wins_area.y + sep.y;
        let (w, h, lines): (u16, u16, Vec<Line>) = if sep.vertical {
            // A column of `│`, one cell per row.
            let rows = (0..sep.length)
                .map(|_| Line::from(Span::styled("│", style)))
                .collect();
            (1, sep.length, rows)
        } else {
            // A single row of `─`.
            let row = vec![Line::from(Span::styled(
                "─".repeat(sep.length as usize),
                style,
            ))];
            (sep.length, 1, row)
        };
        let rect = Rect::new(
            x,
            y,
            w.min(wins_area.right().saturating_sub(x)),
            h.min(wins_area.bottom().saturating_sub(y)),
        );
        frame.render_widget(Paragraph::new(Text::from(lines)), rect);
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
    win: &WindowView,
    view: &View,
) {
    let width = area.width as usize;
    let text = Text::from(
        numbers
            .iter()
            .map(|num| {
                let is_current = *num == Some(current_line);
                let cell = gutter_cell(*num, current_line, win.number, win.relativenumber, width);
                let style = if is_current {
                    view.cursor_line_nr.map(rt).unwrap_or_default()
                } else {
                    view.line_nr
                        .map(rt)
                        .unwrap_or_else(|| Style::default().add_modifier(Modifier::DIM))
                };
                Line::from(Span::styled(cell, style))
            })
            .collect::<Vec<_>>(),
    );
    frame.render_widget(Paragraph::new(text), area);
}

/// Paint the diagnostic sign column reserved by [`render_window`]: each row with a
/// sign shows its severity glyph (padded to the column width) in the resolved
/// `DiagnosticSign*` palette style, or a built-in severity foreground when the
/// colorscheme defines none; rows with no sign are blank. `signs` is empty while a
/// scroll animates, so the reserved column paints blank and the text below it
/// stays put.
fn render_sign_column(
    frame: &mut Frame,
    area: Rect,
    signs: &[Option<DiagSign>],
    palette: &[Style],
) {
    let width = area.width as usize;
    let text = Text::from(
        (0..area.height as usize)
            .map(|row| match signs.get(row).and_then(Option::as_ref) {
                Some((glyph, severity, id)) => {
                    let style = id
                        .and_then(|i| palette.get(i).copied())
                        .unwrap_or_else(|| Style::default().fg(severity_color(*severity)));
                    Line::from(Span::styled(pad_to_width(glyph, width), style))
                }
                None => Line::from(" ".repeat(width)),
            })
            .collect::<Vec<_>>(),
    );
    frame.render_widget(Paragraph::new(text), area);
}

/// A sign glyph fitted to exactly `width` cells: truncated if too wide, then
/// right-padded with spaces (so a 1-cell `E` fills the 2-cell column as `E `).
fn pad_to_width(s: &str, width: usize) -> String {
    let mut out = truncate_to_width(s, width);
    let painted: usize = out
        .chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum();
    out.push_str(&" ".repeat(width.saturating_sub(painted)));
    out
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
    secondary_selection: &[Vec<(u16, u16)>],
    search: &[Vec<(u16, u16)>],
    incsearch: &[Option<(u16, u16)>],
    highlights: &[Vec<HlSpan>],
    diagnostics: &[Vec<DiagSpan>],
    diagnostics_virt: &[Option<DiagVirt>],
    inlay_hints: &[Vec<InlayHint>],
    numbers: &[Option<usize>],
    tabstop: usize,
    leftcol: usize,
    theme: &LineTheme,
) {
    let width = area.width as usize;
    let empty: Vec<HlSpan> = Vec::new();
    let empty_diag: Vec<DiagSpan> = Vec::new();
    let empty_search: Vec<(u16, u16)> = Vec::new();
    let empty_inlay: Vec<InlayHint> = Vec::new();
    let text = Text::from(
        lines
            .iter()
            .enumerate()
            .map(|(row, l)| {
                let sel = selection.get(row).copied().flatten();
                let sec_sel = secondary_selection.get(row).unwrap_or(&empty_search);
                let matches = search.get(row).unwrap_or(&empty_search);
                let cur = incsearch.get(row).copied().flatten();
                let hl = highlights.get(row).unwrap_or(&empty);
                let diag = diagnostics.get(row).unwrap_or(&empty_diag);
                let virt = diagnostics_virt.get(row).and_then(Option::as_ref);
                let inlay = inlay_hints.get(row).unwrap_or(&empty_inlay);
                // A row with no buffer line is a `~` end-of-buffer filler.
                let is_filler = matches!(numbers.get(row), Some(None));
                highlight_line(
                    l, sel, sec_sel, matches, cur, hl, diag, virt, inlay, width, is_filler,
                    tabstop, leftcol, theme,
                )
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
///
/// `leftcol` is the horizontal scroll offset: the first `leftcol` screen columns
/// are not painted, so the row begins at the cell whose absolute screen column is
/// `leftcol`. Styles are still keyed on the **absolute** column, so every span
/// lines up; a wide char or tab straddling the `leftcol` boundary is dropped (its
/// start column is hidden), leaving the boundary cell blank.
#[allow(clippy::too_many_arguments)]
fn highlight_line(
    line: &str,
    sel: Option<(u16, u16)>,
    secondary_sel: &[(u16, u16)],
    search: &[(u16, u16)],
    incsearch: Option<(u16, u16)>,
    hl: &[HlSpan],
    diag: &[DiagSpan],
    virt: Option<&DiagVirt>,
    inlay: &[InlayHint],
    max_width: usize,
    is_filler: bool,
    tabstop: usize,
    leftcol: usize,
    theme: &LineTheme,
) -> Line<'static> {
    let expanded = expand_tabs(line, tabstop);

    // `~` rows carry no tokens or selection: paint the marker with the theme's
    // EndOfBuffer style (default — terminal foreground — with no colorscheme). The
    // `~` sits at column 0, so the horizontal scroll never moves it.
    if is_filler {
        return Line::from(Span::styled(
            expanded,
            theme.end_of_buffer.unwrap_or_default(),
        ));
    }

    let sel = sel.filter(|(s, e)| e > s);

    // Walk cells left to right, coalescing runs of identical style into spans.
    // Cells left of `leftcol` are skipped (scrolled off); the rest paint starting
    // at the left edge, keyed on their absolute column so spans still align.
    //
    // Inlay hints (already sorted by column) are spliced into the stream at their
    // anchor column, *before* the real glyph there — pushing the following glyphs
    // right. The overlay styles stay keyed on the original absolute `col`, so the
    // selection / search / highlight / diagnostic spans remain correct per glyph
    // (the style travels with the glyph, not its painted position); only the
    // cursor, painted separately, needs the matching shift (see `inlay_shift`).
    let mut spans: Vec<Span> = Vec::new();
    let mut run = String::new();
    let mut run_style = Style::default();
    let mut col = 0usize;
    let mut hi = 0usize; // next hint to emit
    let mut inserted = 0usize; // visible hint cells spliced in so far
    for ch in expanded.chars() {
        while hi < inlay.len() && (inlay[hi].0 as usize) <= col {
            emit_inlay_hint(
                &inlay[hi],
                &mut spans,
                &mut run,
                &mut run_style,
                col,
                leftcol,
                max_width,
                &mut inserted,
                theme,
            );
            hi += 1;
        }
        if col >= leftcol {
            let style = cell_style(col, sel, secondary_sel, search, incsearch, hl, diag, theme);
            if style != run_style && !run.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut run), run_style));
            }
            run_style = style;
            run.push(ch);
        }
        col += UnicodeWidthChar::width(ch).unwrap_or(0);
    }
    if !run.is_empty() {
        spans.push(Span::styled(std::mem::take(&mut run), run_style));
    }
    // Hints anchored at or past end-of-text (e.g. an end-of-line type annotation).
    while hi < inlay.len() {
        emit_inlay_hint(
            &inlay[hi],
            &mut spans,
            &mut run,
            &mut run_style,
            col,
            leftcol,
            max_width,
            &mut inserted,
            theme,
        );
        hi += 1;
    }

    // Visible cells painted so far (text past the horizontal scroll, plus the
    // inline hint cells). The virt text below is clamped against this so it never
    // overruns the viewport.
    let mut painted = col.saturating_sub(leftcol) + inserted;

    // Extend the selection past end-of-text (selected newline / linewise fill),
    // within the scrolled viewport `[leftcol, leftcol + max_width)`. The pad count
    // is a difference of absolute columns, which equals the painted-cell count. The
    // primary's `sel` and every secondary selection on this row can each run past
    // the text; pad out to the furthest of them so a linewise fill reaches the edge.
    let pad_end = sel
        .map(|(_, e)| e)
        .into_iter()
        .chain(secondary_sel.iter().map(|(_, e)| *e))
        .max();
    if let Some(e) = pad_end {
        let start = col.max(leftcol);
        let e = (e as usize).min(leftcol + max_width);
        if start < e {
            let pad = " ".repeat(e - start);
            spans.push(Span::styled(pad, selection_style(Style::default(), theme)));
            painted += e - start;
        }
    }

    // Inline diagnostic virtual text after a one-cell gap, truncated to whatever
    // viewport width is left (never on a `~` filler row). The server already
    // prefixed the text; the client only positions and colors it.
    if let Some((text, severity, id)) = virt {
        if !is_filler && painted + 1 < max_width {
            let avail = max_width - painted - 1;
            let shown = truncate_to_width(text, avail);
            if !shown.is_empty() {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(shown, virt_text_style(*severity, *id, theme)));
            }
        }
    }
    Line::from(spans)
}

/// Truncate `s` to at most `max` screen cells (wide chars counted by their
/// display width), dropping any trailing char that would straddle the boundary.
fn truncate_to_width(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > max {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
}

/// The style for a diagnostic's inline virtual text: the colorscheme's resolved
/// `DiagnosticVirtualText*` palette entry when the server interned one, else a
/// built-in severity foreground.
fn virt_text_style(severity: u8, id: Option<usize>, theme: &LineTheme) -> Style {
    if let Some(style) = id.and_then(|i| theme.palette.get(i)) {
        return *style;
    }
    Style::default().fg(severity_color(severity))
}

/// Splice one inlay hint into the row's span stream at its anchor column: flush
/// any pending text run, then push the hint's text (truncated to whatever viewport
/// width is left), styled by its resolved `LspInlayHint` palette entry or a dim
/// fallback. A hint scrolled off the left (`hcol < leftcol`) or with no room left
/// is skipped. `inserted` accumulates the visible hint cells so the caller tracks
/// the shift the splice adds to the following glyphs.
#[allow(clippy::too_many_arguments)]
fn emit_inlay_hint(
    hint: &InlayHint,
    spans: &mut Vec<Span<'static>>,
    run: &mut String,
    run_style: &mut Style,
    col: usize,
    leftcol: usize,
    max_width: usize,
    inserted: &mut usize,
    theme: &LineTheme,
) {
    let (hcol, text, id) = hint;
    if (*hcol as usize) < leftcol {
        return; // scrolled off the left edge.
    }
    let painted = col.saturating_sub(leftcol) + *inserted;
    let shown = truncate_to_width(text, max_width.saturating_sub(painted));
    if shown.is_empty() {
        return;
    }
    if !run.is_empty() {
        spans.push(Span::styled(std::mem::take(run), *run_style));
    }
    *inserted += UnicodeWidthStr::width(shown.as_str());
    spans.push(Span::styled(shown, inlay_hint_style(*id, theme)));
}

/// The combined width of the inline inlay hints on `cursor_row` that sit at or
/// before `cursor_col` (and inside the horizontal scroll) — how far the inline
/// splice pushes the cursor to the right. A hint exactly at the cursor column is
/// inserted before the cursor's glyph, so it counts. Hints scrolled off the left
/// (`hcol < leftcol`) don't add visible cells (a best-effort approximation under
/// horizontal scroll).
fn inlay_cursor_shift(
    inlay_hints: &[Vec<InlayHint>],
    cursor_row: u16,
    cursor_col: u16,
    leftcol: u16,
) -> u16 {
    inlay_hints
        .get(cursor_row as usize)
        .into_iter()
        .flatten()
        .filter(|(hcol, _, _)| *hcol >= leftcol && *hcol <= cursor_col)
        .map(|(_, text, _)| UnicodeWidthStr::width(text.as_str()) as u16)
        .sum()
}

/// The style for an inlay hint: the colorscheme's resolved `LspInlayHint` palette
/// entry when the server interned one, else a built-in dim foreground (neovim
/// links `LspInlayHint` to a comment-like dimming by default).
fn inlay_hint_style(id: Option<usize>, theme: &LineTheme) -> Style {
    if let Some(style) = id.and_then(|i| theme.palette.get(i)) {
        return *style;
    }
    Style::default().fg(Color::DarkGray)
}

/// The style of the screen cell at column `col`: its highlight span's resolved
/// palette style (or [`group_style`] fallback when the span carries no id),
/// with the selection composed on top when the cell is selected.
#[allow(clippy::too_many_arguments)]
fn cell_style(
    col: usize,
    sel: Option<(u16, u16)>,
    secondary_sel: &[(u16, u16)],
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
    // The visual selection sits on top of everything — the primary's `sel`, plus
    // any secondary multi-cursor selection covering this cell (same `Visual` style).
    if sel.is_some_and(in_span) || secondary_sel.iter().copied().any(in_span) {
        style = selection_style(style, theme);
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
        // The `^X` / `<xx>` overlay on an unprintable control char, when no
        // colorscheme defines `SpecialKey`: a standout foreground so the token
        // reads clearly as "this isn't ordinary text" (vim's `SpecialKey` look).
        "SpecialKey" => style.fg(Color::LightMagenta).add_modifier(Modifier::BOLD),
        _ => style,
    }
}

/// Expand tabs to spaces at `tabstop` (the buffer's, mirrored from the server),
/// tracking display width so wide characters before a tab advance the column
/// correctly. No-op for tab-free lines; the result never contains a `\t`.
///
/// Per-`char` `UnicodeWidthChar` width matches the server's per-grapheme
/// `unicode::virtcol` (`UnicodeWidthStr`) because str width is just the sum of
/// its chars' widths — so the cursor's `cursor_screen_col` lines up with the
/// glyphs painted here.
fn expand_tabs(line: &str, tabstop: usize) -> String {
    if !line.contains('\t') {
        return line.to_string();
    }
    let tabstop = tabstop.max(1);
    let mut out = String::with_capacity(line.len() + tabstop);
    let mut col = 0;
    for ch in line.chars() {
        if ch == '\t' {
            let spaces = tabstop - (col % tabstop);
            out.push_str(&" ".repeat(spaces));
            col += spaces;
        } else {
            out.push(ch);
            col += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    out
}

/// Paint a status line from the `segments` the server's `%`-format engine
/// projected (text + resolved style). The base look is the theme's `StatusLine`
/// when loaded, else reverse-video; each segment's own style patches onto that
/// base (so a `%#Group#` that sets only a foreground keeps the status background).
/// Segments span the painted width — the engine's `%=`/`%<` pass already padded
/// or truncated them to fit — and the base style fills any remainder. An empty
/// `segments` (an older server) leaves the bare base look across the row. Shared
/// by the per-window status row and the global status line (`laststatus=3`).
fn render_status(frame: &mut Frame, area: Rect, segments: &[StatusSegment], view: &View) {
    let base = view
        .status_line
        .map(rt)
        .unwrap_or_else(|| Style::default().add_modifier(Modifier::REVERSED));

    let spans: Vec<Span> = segments
        .iter()
        .map(|(text, style)| {
            let style = style.map_or(base, |s| base.patch(rt(s)));
            Span::styled(text.clone(), style)
        })
        .collect();

    frame.render_widget(Paragraph::new(Line::from(spans)).style(base), area);
}

fn render_command(frame: &mut Frame, area: Rect, view: &View) {
    if view.command_mode {
        frame.render_widget(
            Paragraph::new(format!("{}{}", cmdline_prompt_str(view), view.cmdline)),
            area,
        );
    } else if !view.message.is_empty() {
        frame.render_widget(Paragraph::new(view.message.clone()), area);
    } else if !view.hidden_docks.is_empty() {
        // The idle command row advertises collapsed (hidden) docks as `▸{label}`
        // chips — the only on-screen hint a hidden dock still exists. A click on one
        // re-shows that dock (see the core `hidden_chip_at` hit-test). Geometry must
        // match that hit-test: chips from col 0, each `▸{label}`, space-separated.
        let chip_style = view.status_line.map(rt).unwrap_or_default();
        let mut spans: Vec<Span> = Vec::new();
        for (i, label) in view.hidden_docks.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(format!("▸{label}"), chip_style));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

/// Render the tabline across the top row. With a custom `'tabline'` set, the
/// server has already rendered it into styled segments ([`View::tabline_segments`])
/// — paint those verbatim. Otherwise fall back to the built-in cells: one per tab,
/// each ` {count} {name}{+} ` (the window count only when >1, a `+` when the tab's
/// buffer is modified, matching vim's default tabline), the active cell
/// reverse-video and the strip past the last cell left blank (vim's `TabLineFill`).
fn render_tabline(frame: &mut Frame, area: Rect, view: &View) {
    if !view.tabline_segments.is_empty() {
        let spans: Vec<Span> = view
            .tabline_segments
            .iter()
            .map(|(text, style)| match style {
                Some(s) => Span::styled(text.clone(), rt(*s)),
                None => Span::raw(text.clone()),
            })
            .collect();
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    render_tab_cells(frame, area, "", &view.tabline, view.current_tab);
}

/// Paint built-in tabline cells into `area`: an optional bold `title` label first
/// (the `nx.dock` dock title), then one ` {count} {name}{+} ` cell per tab (the
/// window count only when >1, a `+` when modified — vim's default), the `current`
/// cell reverse-video and the strip past the last cell left blank (vim's
/// `TabLineFill`). Shared by the global (main) tabline and each dock's own
/// tabline. A no-op for an empty strip (no title and no tabs) or zero-height area.
fn render_tab_cells(frame: &mut Frame, area: Rect, title: &str, tabs: &[TabData], current: usize) {
    if (title.is_empty() && tabs.is_empty()) || area.height == 0 {
        return;
    }
    let mut spans: Vec<Span> = Vec::with_capacity(tabs.len() + 1);
    if !title.is_empty() {
        spans.push(Span::styled(
            format!(" {title} "),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }
    for (i, tab) in tabs.iter().enumerate() {
        let count = if tab.window_count > 1 {
            format!("{} ", tab.window_count)
        } else {
            String::new()
        };
        let modified = if tab.modified { "+" } else { "" };
        let text = format!(" {count}{}{modified} ", tab.label);
        let style = if i == current {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        spans.push(Span::styled(text, style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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
    let start = pmenu_start(pmenu.selected, rows);
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

/// Draw the floating selectable-list menu (`nx.ui.select`) as a bordered overlay
/// over the text area, anchored at `(menu.col, menu.row)` in `text_area` cells —
/// the same shape as the completion popup, but each row is a plain label and the
/// highlighted row is reverse-video. The box is `Clear`ed first so the text
/// beneath doesn't bleed through, and the list scrolls to keep the selection
/// visible when there are more items than rows.
fn render_menu(
    frame: &mut Frame,
    text_area: Rect,
    menu: &MenuData,
    styles: &[nxvim_view::Style],
) -> Option<(u16, u16)> {
    // The completion popup omits its top border (it sits flush against the line
    // below the cursor), so it costs one fewer row than a fully bordered box. It
    // also shifts one cell left so the *left* border doesn't push the list off the
    // word: `menu.col` is the content anchor (the word start), and the box's left
    // border sits one cell before it.
    let (borders, vborder) = if menu.border_top {
        (Borders::ALL, 2)
    } else {
        (Borders::LEFT | Borders::RIGHT | Borders::BOTTOM, 1)
    };
    let left_shift = u16::from(!menu.border_top);
    let x = text_area
        .x
        .saturating_add(menu.col)
        .saturating_sub(left_shift);
    let y = text_area.y.saturating_add(menu.row);
    let width = menu
        .width
        .saturating_add(2)
        .min(text_area.right().saturating_sub(x));
    let height = menu
        .height
        .saturating_add(vborder)
        .min(text_area.bottom().saturating_sub(y));
    let area = Rect {
        x,
        y,
        width,
        height,
    };
    if area.width < 3 || area.height < vborder + 1 {
        return None;
    }
    let block = Block::new().borders(borders);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let inner_h = inner.height as usize;
    let box_w = inner.width as usize;
    // Split the box into a list column (left) + a 1-col vertical separator + a
    // preview column (right) when the picker carries a preview pane; otherwise the
    // list fills the box. The prompt + results live in the list column; the preview
    // spans the full inner height on the right.
    let (width, list_area, preview_layout) = match &menu.preview {
        Some(pv) => {
            let preview_w = (pv.width as usize).min(box_w.saturating_sub(2)).max(1);
            let list_w = box_w.saturating_sub(preview_w + 1).max(1);
            let list_area = Rect {
                x: inner.x,
                y: inner.y,
                width: list_w as u16,
                height: inner.height,
            };
            let sep_x = inner.x + list_w as u16;
            let preview_area = Rect {
                x: sep_x + 1,
                y: inner.y,
                width: preview_w as u16,
                height: inner.height,
            };
            (list_w, list_area, Some((sep_x, preview_area, pv)))
        }
        None => (box_w, inner, None),
    };
    // A picker carries a prompt row and a separator row (the `chrome`); the list
    // fills the rest. A promptless `nx.ui.select` has neither — the list is the
    // whole box. The prompt sits above the list by default, or below it when the
    // source/open asked for it (telescope-style).
    let has_prompt = menu.query.is_some();
    let chrome = usize::from(has_prompt) * 2;
    let list_rows = inner_h.saturating_sub(chrome);
    // A noselect completion popup highlights no row and scrolls from the top.
    let sel = menu.selected_active.then_some(menu.selected);
    let start = pmenu_start(sel, list_rows);

    // Build one list row (or a blank filler past the end of the list).
    let list_line = |r: usize| -> Line<'static> {
        let idx = start + r;
        match menu.items.get(idx) {
            Some(label) => {
                let empty = Vec::new();
                let spans = menu.match_spans.get(idx).unwrap_or(&empty);
                menu_row_line(label, spans, sel == Some(idx), width)
            }
            None => Line::from(" ".repeat(width)),
        }
    };
    // `> query` prompt line — the query in bold so it reads as the live input.
    let prompt_line = || -> Line<'static> {
        let query = menu.query.as_deref().unwrap_or("");
        Line::from(vec![
            Span::styled("> ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(
                pmenu_row(query, "", width.saturating_sub(2)),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ])
    };
    // A full-width horizontal rule separating the prompt from the list.
    let sep_line = || -> Line<'static> {
        Line::from(Span::styled(
            "─".repeat(width),
            Style::default().add_modifier(Modifier::DIM),
        ))
    };

    let mut lines: Vec<Line> = Vec::with_capacity(inner_h);
    // The prompt row's offset within `inner` (for the caret); `None` when promptless.
    let mut prompt_row = None;
    if has_prompt && menu.prompt_bottom {
        for r in 0..list_rows {
            lines.push(list_line(r));
        }
        lines.push(sep_line());
        prompt_row = Some(lines.len());
        lines.push(prompt_line());
    } else if has_prompt {
        prompt_row = Some(0);
        lines.push(prompt_line());
        lines.push(sep_line());
        for r in 0..list_rows {
            lines.push(list_line(r));
        }
    } else {
        for r in 0..list_rows {
            lines.push(list_line(r));
        }
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), list_area);

    // The preview column: a vertical separator rule, then the windowed file.
    if let Some((sep_x, preview_area, pv)) = preview_layout {
        let sep: Vec<Line> = (0..inner.height)
            .map(|_| {
                Line::from(Span::styled(
                    "│",
                    Style::default().add_modifier(Modifier::DIM),
                ))
            })
            .collect();
        frame.render_widget(
            Paragraph::new(Text::from(sep)),
            Rect {
                x: sep_x,
                y: inner.y,
                width: 1,
                height: inner.height,
            },
        );
        let palette: Vec<Style> = styles.iter().copied().map(rt).collect();
        render_preview(frame, preview_area, pv, &palette);
    }

    // The terminal caret sits in the prompt (in the list column), past the `> `
    // prefix at the query's text-cursor column (clamped inside the column).
    prompt_row.map(|row| {
        let caret = (2 + menu.query_cursor).min(list_area.width.saturating_sub(1));
        (list_area.x + caret, list_area.y + row as u16)
    })
}

/// Render the picker preview column: a dim title row (the file path) then the
/// windowed file lines, syntax-coloured from the server's tree-sitter `highlights`
/// (Phase 3b), with the match line (`loc`) reverse-highlighted so the grep /
/// reference hit stands out. `palette` is the frame's resolved style table (span
/// `style_id`s index it); a span with no id falls back to [`group_style`].
fn render_preview(frame: &mut Frame, area: Rect, pv: &MenuPreview, palette: &[Style]) {
    let w = area.width as usize;
    let cap = area.height as usize;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(cap);
    lines.push(Line::from(Span::styled(
        pmenu_row(&pv.title, "", w),
        Style::default()
            .add_modifier(Modifier::DIM)
            .add_modifier(Modifier::BOLD),
    )));
    let empty = Vec::new();
    for (i, text) in pv.lines.iter().enumerate() {
        if lines.len() >= cap {
            break;
        }
        let is_loc = pv.loc.is_some_and(|(r, _)| r as usize == i);
        let hl = pv.highlights.get(i).unwrap_or(&empty);
        lines.push(preview_line(text, hl, palette, is_loc, w));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// One preview line: each char coloured by the tree-sitter span covering it (char
/// columns, no tab expansion — matching the server's char-based spans), padded to
/// `width`. The `loc` match line is reverse-video over the syntax colours.
fn preview_line(
    text: &str,
    hl: &[HlSpan],
    palette: &[Style],
    loc: bool,
    width: usize,
) -> Line<'static> {
    let base = if loc {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    let mut spans: Vec<Span> = Vec::new();
    let mut run = String::new();
    let mut run_style = base;
    let mut used = 0usize;
    for (ci, ch) in text.chars().enumerate() {
        if used >= width {
            break;
        }
        let token = hl
            .iter()
            .find(|(s, e, _, _)| ci >= *s as usize && ci < *e as usize);
        let mut style = match token {
            Some((_, _, group, id)) => match id {
                Some(i) => palette.get(*i).copied().unwrap_or_default(),
                None => group_style(group),
            },
            None => Style::default(),
        };
        if loc {
            style = style.add_modifier(Modifier::REVERSED);
        }
        if style != run_style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_style = style;
        run.push(ch);
        used += 1;
    }
    if !run.is_empty() {
        spans.push(Span::styled(std::mem::take(&mut run), run_style));
    }
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), base));
    }
    Line::from(spans)
}

/// Build one menu row: the `label` padded to `width`, reverse-video when
/// `selected`, with the matched-character `spans` (half-open **char** ranges)
/// bold+underlined so the fuzzy match stands out. Char-indexed to match the
/// server's char-based spans.
fn menu_row_line(label: &str, spans: &[(u16, u16)], selected: bool, width: usize) -> Line<'static> {
    let base = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    let matched = base
        .add_modifier(Modifier::BOLD)
        .add_modifier(Modifier::UNDERLINED);
    let mut out: Vec<Span> = Vec::new();
    let mut used = 0usize;
    for (i, ch) in label.chars().enumerate() {
        if used >= width {
            break;
        }
        let i = i as u16;
        let is_match = spans.iter().any(|(s, e)| i >= *s && i < *e);
        out.push(Span::styled(
            ch.to_string(),
            if is_match { matched } else { base },
        ));
        used += 1;
    }
    if used < width {
        out.push(Span::styled(" ".repeat(width - used), base));
    }
    Line::from(out)
}

/// First visible item index for a popup whose inner content area is `rows` tall
/// with `selected` highlighted: scroll the list to keep the selection in view,
/// else start at the top. The single source of truth shared by [`render_pmenu`]
/// and [`pmenu_geometry`] so a click maps to the same row the renderer drew.
fn pmenu_start(selected: Option<usize>, rows: usize) -> usize {
    match selected {
        Some(s) if s >= rows => s + 1 - rows,
        _ => 0,
    }
}

/// The focused window's text-area inner rect (past its number gutter) for a
/// `width`×`height` terminal showing `view` — the cell space the popup and its
/// doc box anchor in. Mirrors the window layout in [`render`]/[`render_window`];
/// shared by the popup and doc-box geometry so hit-tests land on exactly the
/// cells the renderer draws. An empty rect before the first redraw (no window).
fn text_inner_rect(width: u16, height: u16, view: &View) -> Rect {
    let Some(win) = view.focused() else {
        return Rect::new(0, 0, 0, 0);
    };
    let tabline_rows = u16::from(!view.tabline.is_empty());
    let global_status_rows = u16::from(!view.global_status.is_empty());
    let panel_rows = view.panel.as_ref().map_or(0, |p| p.height + 1);
    let dock = DockLayout::new(
        Rect::new(0, 0, width, height),
        view,
        tabline_rows,
        global_status_rows,
        panel_rows,
    );
    // A focused float's content sits inside its border; the popup anchors there.
    // The focused window may live in a dock, so offset by its region's origin.
    let area = float_inner(
        window_area(dock.content(win.region), win),
        win.border.map(bt),
    );
    // The text body is the window rect minus its bottom status row — when this
    // window shows one (per `'laststatus'`); otherwise it claims the whole rect.
    let text_area = if win.status_visible {
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area)[0]
    } else {
        area
    };
    // Reserve the sign column first (left of the number gutter), mirroring
    // `render_window`, so the popup anchors past both gutters.
    let gutter_area = if win.sign_column {
        Layout::horizontal([Constraint::Length(SIGN_WIDTH), Constraint::Min(0)]).split(text_area)[1]
    } else {
        text_area
    };
    if win.number_width > 0 {
        Layout::horizontal([Constraint::Length(win.number_width), Constraint::Min(0)])
            .split(gutter_area)[1]
    } else {
        gutter_area
    }
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
    let text_inner = text_inner_rect(width, height, view);
    let popup = popup_rect(text_inner, pmenu)?;
    let (area, max_scroll) = doc_rect(text_inner, popup, &pmenu.doc)?;
    Some((area.x, area.y, area.width, area.height, max_scroll))
}

/// Headless geometry of the completion popup's inner item area for a
/// `width`×`height` terminal showing `view`: `(x, y, width, height, start)` in
/// screen cells, where `start` is the first visible item index — or `None` when
/// no popup is drawn. The event loop uses it to hit-test the mouse: the wheel
/// moves the selection while it is over the box, and a left-click on row `r`
/// chooses item `start + (r - y)`. Mirrors the layout in [`render_pmenu`] (same
/// reason as [`pmenu_doc_geometry`]).
#[doc(hidden)]
pub fn pmenu_geometry(width: u16, height: u16, view: &View) -> Option<(u16, u16, u16, u16, usize)> {
    let pmenu = view.pmenu.as_ref()?;
    let text_inner = text_inner_rect(width, height, view);
    let area = popup_rect(text_inner, pmenu)?;
    let inner = Block::new().borders(Borders::ALL).inner(area);
    let start = pmenu_start(pmenu.selected, inner.height as usize);
    Some((inner.x, inner.y, inner.width, inner.height, start))
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

/// Screen rect of the panel's content area — the rows below its top-border bar —
/// on a `width`×`height` terminal showing a panel of content height
/// `panel_height`: `(x, y, width, height)` in cells, or `None` when there's no
/// room. The content sits one row below the border ([`close_button`]'s row), so
/// `y = border_row + 1`. Pure (no rendering), so the event loop can hit-test the
/// mouse against the same rows [`render_panel`] draws.
#[doc(hidden)]
pub fn panel_content_rect(
    width: u16,
    height: u16,
    panel_height: u16,
) -> Option<(u16, u16, u16, u16)> {
    let border_row = height.checked_sub(panel_height + 2)?;
    Some((0, border_row + 1, width, panel_height))
}
