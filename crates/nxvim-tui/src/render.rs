//! The renderer: lays the three regions out and paints each with a ratatui
//! widget, plus the headless [`paint`]/[`ScrollHarness`] test entry points.

use std::borrow::Cow;

use crossterm::cursor::SetCursorStyle;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use rmpv::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::anim::{arm_animation, lerp, Animation};
use crate::images::ImageStore;
use nxvim_view::{
    DiagSign, DiagSpan, DiagVirt, HlSpan, IncSearchSpans, InlayHint, MenuData, MenuPreview,
    PmenuData, RegionTabline, SearchSpans, Separator, StatusSegment, TabData, View, VirtChunk,
    VirtPlacement, WindowRegion, WindowView,
};

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

/// The background style for a window's content box. A float uses `NormalFloat`
/// (falling back to `Normal`) so its body matches the bordered box the renderer
/// paints around it; a tiled window uses `Normal`. `None` when the colorscheme
/// (and its fallback) leaves the group undefined — the cells keep the terminal
/// default, as before a theme is loaded.
fn window_bg(win: &WindowView, view: &View) -> Option<Style> {
    if win.floating {
        win.normal_float(view).map(rt)
    } else {
        win.normal(view).map(rt)
    }
}

/// The style painted across the cursor's screen row for `'cursorline'`. Prefers
/// the colorscheme's `CursorLine` group; with none resolved (no theme, or a
/// theme that leaves it undefined) it falls back to a subtle dark-gray
/// background so the line is still visible out of the box — matching how the
/// other chrome groups degrade to a built-in look.
fn cursorline_style(win: &WindowView, view: &View) -> Style {
    win.cursor_line_bg(view)
        .map(rt)
        .unwrap_or_else(|| Style::default().bg(Color::Indexed(236)))
}

/// The base style for status-line-tinted chrome — the per-window/global status
/// bars, the split separators, and the permanent-dock border lines. The theme's
/// `StatusLine` group when a colorscheme defines it, else reverse-video (vim's
/// `WinSeparator`/status default out of the box).
fn status_line_style(view: &View) -> Style {
    view.status_line
        .map(rt)
        .unwrap_or_else(|| Style::default().add_modifier(Modifier::REVERSED))
}

/// The style for the split separators and permanent-dock border glyphs: the
/// theme's `WinSeparator` group when the colorscheme defines it, else the
/// status-line tint (vim's out-of-the-box separator look — a colorscheme with no
/// `WinSeparator` still reuses its status bar's colours here). Keeping this off
/// [`status_line_style`] lets a theme give its separators a dimmer, distinct
/// colour (e.g. catppuccin's near-background `crust`) instead of the brighter
/// status-line background.
fn separator_style(view: &View) -> Style {
    let base = view
        .win_separator
        .map(rt)
        .unwrap_or_else(|| status_line_style(view));
    // `WinSeparator` commonly sets only a foreground (e.g. catppuccin's dim `crust`
    // glyph colour). Without a background the separator cell — which sits *between*
    // windows and is painted by no window — keeps the terminal's own background,
    // showing as a dark strip between light windows on a light colorscheme. Give it
    // the editor's `Normal` background (like vim, where a separator inherits Normal's
    // bg) so the cell tracks the theme instead of the terminal.
    if base.bg.is_none() {
        if let Some(bg) = view.normal.map(rt).and_then(|s| s.bg) {
            return base.bg(bg);
        }
    }
    base
}

/// Paint a horizontal run of `glyph` (`len` cells wide) at `(x, y)` in `style`.
fn paint_hline(frame: &mut Frame, x: u16, y: u16, len: u16, glyph: &str, style: Style) {
    if len == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Span::styled(glyph.repeat(len as usize), style)),
        Rect::new(x, y, len, 1),
    );
}

/// Paint a vertical run of `glyph` (`len` cells tall, one per row) at `(x, y)` in
/// `style`.
fn paint_vline(frame: &mut Frame, x: u16, y: u16, len: u16, glyph: &str, style: Style) {
    if len == 0 {
        return;
    }
    let rows: Vec<Line> = (0..len)
        .map(|_| Line::from(Span::styled(glyph.to_string(), style)))
        .collect();
    frame.render_widget(Paragraph::new(Text::from(rows)), Rect::new(x, y, 1, len));
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
        .draw(|frame| render(frame, view, None, 0, None))
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
        .draw(|frame| render(frame, view, None, doc_scroll, None))
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
            .draw(|frame| render(frame, &self.view, self.anim.as_ref(), 0, None))
            .expect("draw");
        terminal.backend().buffer().clone()
    }
}

/// Lay the frame out and paint it: each window at its rect (gutter + text body +
/// its own status line), the split separators between them, then the global
/// command line and completion popup. The windows area is the frame minus the
/// command line; with one window it spans that whole area, so the output matches
/// the pre-windows single-window frame exactly.
/// When `anim` is present it animates the **focused** window's slide.
pub(crate) fn render(
    frame: &mut Frame,
    view: &View,
    anim: Option<&Animation>,
    doc_scroll: u16,
    mut images: Option<&mut ImageStore>,
) {
    // The command line is the last row. Each window draws its own status line at the
    // bottom of its rect, so there is no longer a global status row here.
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
    let dock = DockLayout::new(frame.area(), view, tabline_rows, global_status_rows);
    let (tabline_area, cmd_area) = (dock.tabline, dock.cmd);
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
        // `'padding'` insets the content box by a per-side margin; the server already
        // sized this window's rows/cursor to the inset area, so the renderer just
        // shifts the origin in and shrinks the box. The blank margin must share the
        // window background — `render_window` only paints the inset content box — so
        // paint the whole box with `Normal` first when there's a margin to fill.
        let outer = window_area(dock.content(win.region), win);
        if win.padding.horizontal() + win.padding.vertical() > 0 {
            if let Some(bg) = window_bg(win, view) {
                frame.render_widget(Block::default().style(bg), outer);
            }
        }
        let area = pad_rect(outer, win.padding);
        // Only the focused window animates a scroll slide.
        let win_anim = if win.focused { anim } else { None };
        let (text_inner, cursor_row, cursor_shift) =
            render_window(frame, area, win, view, win_anim, images.as_deref_mut());
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
                let block = float_block(bt(border), win.title.as_deref(), FloatTheme::of(view));
                let inner = block.inner(outer);
                frame.render_widget(block, outer);
                inner
            }
            None => {
                // A borderless float has no box to fill the (post-`Clear`) cells, so
                // paint the float background across it — otherwise a `'padding'`
                // margin or end-of-line gap would show the terminal default. A
                // bordered float's `float_block` already fills its inner with the
                // same `NormalFloat` background.
                if let Some(bg) = window_bg(win, view) {
                    frame.render_widget(Block::default().style(bg), outer);
                }
                outer
            }
        };
        // `'padding'` insets the content a further per-side margin inside any border.
        let inner = pad_rect(inner, win.padding);
        let (text_inner, cursor_row, cursor_shift) =
            render_window(frame, inner, win, view, None, images.as_deref_mut());
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
        // The command-line wildmenu anchors to the command-line area (frame-bottom,
        // no gutter); the `nx.picker` overlay (`editor_relative`) anchors to the
        // editor windows area (frame minus the command row) so it floats over the
        // whole editor, not the focused split; every other menu anchors to the focused
        // window's text inner. The windows area is also the base for an editor-relative
        // docs sidebar.
        (Some(menu), Some((inner, _, _))) => {
            let f = frame.area();
            let editor_area =
                Rect::new(f.x, f.y, f.width, f.height.saturating_sub(dock.cmd.height));
            let base = if menu.cmdline {
                cmd_area
            } else if menu.editor_relative {
                editor_area
            } else {
                inner
            };
            render_menu(frame, base, menu, &view.styles)
        }
        _ => None,
    };

    // The list-less content float (`nx.ui.float`; LSP hover / signature help). A
    // cursor-anchored float floats over the focused window's text area; an
    // `editor`/`bottom`-relative float (the which-key surface) anchors over the
    // whole editor's windows area (the frame minus the command row, matching the
    // server's geometry), so a split doesn't drag it into the focused pane. Drawn
    // on top, with no input focus.
    if let Some(float) = &view.content_float {
        let base = if float.editor_relative {
            let f = frame.area();
            Some(Rect::new(
                f.x,
                f.y,
                f.width,
                f.height.saturating_sub(dock.cmd.height),
            ))
        } else {
            focused_inner.map(|(inner, _, _)| inner)
        };
        if let Some(base) = base {
            render_content_float(frame, base, float, &view.styles, FloatTheme::of(view));
        }
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
        // `cmdline_cursor` so it sits mid-line after edits.
        let prompt_width = cmdline_prompt_width(view);
        // `cmdline_cursor` is a server-supplied *char* offset, but the terminal
        // cursor needs a *display column* — a wide (CJK) char occupies two cells —
        // so measure the painted width of the chars before the caret. Saturate the
        // cast and the adds so a bogus value can't overflow the column coordinate
        // (a debug-build panic, a release-build wrap) — ratatui clamps the result.
        let caret: usize = view
            .cmdline
            .chars()
            .take(view.cmdline_cursor)
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
            .sum();
        let col = cmd_area
            .x
            .saturating_add(prompt_width)
            .saturating_add(u16::try_from(caret).unwrap_or(u16::MAX));
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
        // All three terms (`cursor_screen_col`, `shift`, `leftcol`) are derived from
        // server data; saturate the arithmetic so an out-of-range value can't overflow
        // the coordinate (a debug panic / release wrap). The `.min` already bounds the
        // result to the window, and ratatui clamps the final cursor position.
        let col = inner.x.saturating_add(
            win.cursor_screen_col
                .saturating_add(shift)
                .saturating_sub(win.leftcol)
                .min(inner.width.saturating_sub(1)),
        );
        let row = inner
            .y
            .saturating_add(cursor_row.min(inner.height.saturating_sub(1)));
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
/// dock][cmd]`; each open dock reserves its content extent plus one separator
/// cell toward the main area. With no dock open every band is `0` and the layout
/// collapses to the pre-dock `[tabline][main][global status][cmd]` form, so a
/// dock-free frame is unchanged.
struct DockLayout {
    main: Rect,
    left: Rect,
    right: Rect,
    top: Rect,
    bottom: Rect,
    tabline: Rect,
    global_status: Rect,
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
    fn new(area: Rect, view: &View, tabline_rows: u16, global_status_rows: u16) -> Self {
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
            Constraint::Length(res(db)),            // bottom dock
            Constraint::Length(1),                  // command line
        ])
        .split(area);
        let (top_band, tabline, mid, global_status, bottom_band, cmd) =
            (v[0], v[1], v[2], v[3], v[4], v[5]);
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
                render_tab_cells(
                    frame,
                    area,
                    &region.title,
                    &region.tabs,
                    region.current,
                    view,
                );
            }
        }
    }

    /// Paint the border line between each open dock and the main area. Drawn with
    /// the **heavy** box-drawing glyphs (`━`/`┃`) so a permanent dock edge reads as
    /// distinct from the light (`─`/`│`) borders between ordinary window splits.
    fn render_borders(&self, frame: &mut Frame, view: &View) {
        // The permanent-dock edges use the **heavy** box-drawing glyphs (`━`/`┃`) so
        // a dock border reads as distinct from the light (`─`/`│`) split separators.
        // They share the split separators' `WinSeparator` tint.
        let style = separator_style(view);
        if self.dt > 0 {
            paint_hline(
                frame,
                self.top.x,
                self.top.y + self.dt,
                self.top.width,
                "━",
                style,
            );
        }
        if self.db > 0 {
            paint_hline(
                frame,
                self.bottom.x,
                self.bottom.y - 1,
                self.bottom.width,
                "━",
                style,
            );
        }
        if self.dl > 0 {
            paint_vline(
                frame,
                self.left.x + self.dl,
                self.mid.y,
                self.mid.height,
                "┃",
                style,
            );
        }
        if self.dr > 0 {
            paint_vline(
                frame,
                self.right.x - 1,
                self.mid.y,
                self.mid.height,
                "┃",
                style,
            );
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

/// Inset `area` by a window's `'padding'` — a per-side blank margin in cells —
/// clamped so at least a 1×1 cell survives a margin wider than the window. The
/// content (gutter/text/status) paints into the returned rect; the cells outside
/// it show whatever was painted behind (the editor background, or a float's box).
fn pad_rect(area: Rect, pad: nxvim_view::Padding) -> Rect {
    let left = pad.left.min(area.width.saturating_sub(1));
    let top = pad.top.min(area.height.saturating_sub(1));
    Rect {
        x: area.x + left,
        y: area.y + top,
        width: area.width.saturating_sub(left + pad.right).max(1),
        height: area.height.saturating_sub(top + pad.bottom).max(1),
    }
}

/// The bordered [`Block`] for a float: `BorderType` glyphs all round, with the
/// `title` (when present) on the top border, padded with a space each side so it
/// reads as a label rather than running into the corners. Left-aligned, matching
/// neovim's default `title_pos = "left"`.
fn float_block(border: BorderType, title: Option<&str>, theme: FloatTheme) -> Block<'static> {
    let mut block = Block::new().borders(Borders::ALL).border_type(border);
    // `NormalFloat` paints the whole box (border cells and the inner background it
    // shows through where content doesn't reach); `FloatBorder` then recolors the
    // border glyphs over it. Both fall back to the terminal default when unset.
    if let Some(bg) = theme.bg {
        block = block.style(bg);
    }
    if let Some(border_style) = theme.border {
        block = block.border_style(border_style);
    }
    if let Some(title) = title {
        let mut line = Line::from(format!(" {title} ")).left_aligned();
        if let Some(title_style) = theme.title {
            line = line.style(title_style);
        }
        block = block.title_top(line);
    }
    block
}

/// The resolved float chrome styles (`FloatBorder` / `NormalFloat` / `FloatTitle`),
/// each `None` when the colorscheme leaves the group undefined. Pulled once from
/// the [`View`] and threaded to every float so they theme identically.
#[derive(Clone, Copy, Default)]
struct FloatTheme {
    border: Option<Style>,
    bg: Option<Style>,
    title: Option<Style>,
}

impl FloatTheme {
    fn of(view: &View) -> Self {
        FloatTheme {
            border: view.float_border.map(rt),
            bg: view.normal_float.map(rt),
            title: view.float_title.map(rt),
        }
    }
}

/// A float's inner content rect (past its border), or the whole `area` for a
/// borderless float. Shared by the renderer and [`text_inner_rect`] so a focused
/// float's cursor/popup anchor lands on the cells the border left for content.
fn float_inner(area: Rect, border: Option<BorderType>) -> Rect {
    match border {
        Some(border) => float_block(border, None, FloatTheme::default()).inner(area),
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
    images: Option<&mut ImageStore>,
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

    // An image-preview buffer (`'imagepreview'`): paint the picture across the whole
    // text body and skip the gutter / text machinery entirely. Only the live client
    // has an `ImageStore` (graphics need a real terminal); the headless render / test
    // paths pass `None`, leaving the body blank. The buffer is empty, so there is no
    // meaningful cursor — return the body rect and row 0.
    if let Some(image) = &win.image {
        if let Some(bg) = window_bg(win, view) {
            frame.render_widget(Block::default().style(bg), text_area);
        }
        if let Some(store) = images {
            store.render(frame, text_area, image);
        }
        if let Some(status_area) = status_area {
            render_status(frame, status_area, &win.status, view);
        }
        return (text_area, 0, 0);
    }

    let height = text_area.height as usize;

    // Owned slide-band snapshots, populated only while animating (a `skip/take`
    // window of the band by screen-row offset). The static path — the overwhelmingly
    // common case — borrows straight from `win` instead of deep-cloning the whole
    // viewport (lines, per-cell highlights, selection, numbers) every repaint.
    let anim_lines: Vec<String>;
    let anim_sel: Vec<Option<(u16, u16)>>;
    let anim_secondary_sel: SearchSpans;
    let anim_hl: Vec<Vec<HlSpan>>;
    let anim_numbers: Vec<Option<usize>>;
    let anim_continuation: Vec<bool>;
    let anim_inlay: Vec<Vec<InlayHint>>;
    let anim_virt_text: Vec<Vec<VirtPlacement>>;
    let anim_virt_lines: Vec<Option<Vec<VirtChunk>>>;
    let anim_diag_virt: Vec<Option<DiagVirt>>;
    // Diagnostic underlines and signs ride the screen-row band now, sliced like the
    // other overlays, so the squiggles and signs slide with the text instead of
    // blanking out for the ~150ms slide.
    let anim_diag: Vec<Vec<DiagSpan>>;
    let anim_signs: Vec<Option<DiagSign>>;
    let anim_search: SearchSpans;
    let anim_incsearch: IncSearchSpans;
    let frame_lines: &[String];
    let frame_sel: &[Option<(u16, u16)>];
    let frame_secondary_sel: &SearchSpans;
    let frame_hl: &[Vec<HlSpan>];
    let frame_inlay: &[Vec<InlayHint>];
    // Extmark virt_text rides the slide band (sliced like `frame_inlay`), so the
    // placements slide with the line instead of flashing out and back on settle.
    let frame_virt_text: &[Vec<VirtPlacement>];
    // Extmark `virt_lines` (whole virtual rows) and diagnostic virtual text ride the
    // band too, now that it is screen-row based and interleaves the virtual rows.
    let frame_virt_lines: &[Option<Vec<VirtChunk>>];
    let frame_diag_virt: &[Option<DiagVirt>];
    // Diagnostic underlines / signs ride the band (sliced into the snapshots above),
    // assigned in the same match as the other frame fields.
    let frame_diag: &[Vec<DiagSpan>];
    let frame_signs: &[Option<DiagSign>];
    let frame_numbers: &[Option<usize>];
    // Soft-wrap continuation flags, sliced like `numbers`, so the gutter blanks the
    // wrapped rows whether the viewport is settled or mid-slide.
    let frame_continuation: &[bool];
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
                                               // The slide is a screen-row offset into the band: `off` is the viewport
                                               // top's band-row index, `cur_row` the cursor's. The band already
                                               // interleaves any `virt_lines`, so advancing whole screen rows slides
                                               // them correctly. `off` is a slice index (band `rows[0]` is the anchor).
            let off = lerp(a.from_row, a.to_row, t).round() as usize;
            let cur_row = lerp(a.from_cursor_row, a.to_cursor_row, t).round() as usize;
            anim_lines = a.lines.iter().skip(off).take(height).cloned().collect();
            // Grow/shrink the selection's moving edge in step with the slide. The
            // band carries the selection over the *maximal* extent the slide
            // touches; mid-slide, only the rows on the anchor side of the
            // interpolated cursor are highlighted, so the selection tracks the
            // scroll instead of snapping to its full extent (or vanishing) on frame
            // 0. The clip side follows the *selection orientation*, not the scroll
            // direction: anchor above ⇒ extends down ⇒ hide rows past the cursor
            // below; anchor below ⇒ extends up ⇒ hide above. `None` when no visual
            // selection is sliding (the band is all-empty anyway). The comparison is
            // in band-row space now: row `off + j` versus the cursor's band row.
            anim_sel = {
                let mut sel: Vec<Option<(u16, u16)>> =
                    a.selection.iter().skip(off).take(height).copied().collect();
                if let Some(down) = a.sel_extends_down {
                    for (j, span) in sel.iter_mut().enumerate() {
                        let band_row = off + j; // band-row index of this visible row
                        let past = if down {
                            band_row > cur_row
                        } else {
                            band_row < cur_row
                        };
                        if past {
                            *span = None;
                        }
                    }
                }
                sel
            };
            anim_secondary_sel = a
                .secondary_selection
                .iter()
                .skip(off)
                .take(height)
                .cloned()
                .collect();
            anim_numbers = a.numbers.iter().skip(off).take(height).copied().collect();
            anim_continuation = a
                .continuation
                .iter()
                .skip(off)
                .take(height)
                .copied()
                .collect();
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
            anim_virt_text = a.virt_text.iter().skip(off).take(height).cloned().collect();
            anim_virt_lines = a
                .virt_lines
                .iter()
                .skip(off)
                .take(height)
                .cloned()
                .collect();
            anim_diag_virt = a
                .diagnostics_virt
                .iter()
                .skip(off)
                .take(height)
                .cloned()
                .collect();
            anim_diag = a
                .diagnostics
                .iter()
                .skip(off)
                .take(height)
                .cloned()
                .collect();
            anim_signs = a
                .diagnostics_signs
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
            frame_secondary_sel = &anim_secondary_sel;
            frame_numbers = &anim_numbers;
            frame_continuation = &anim_continuation;
            frame_hl = &anim_hl;
            frame_inlay = &anim_inlay;
            frame_virt_text = &anim_virt_text;
            frame_virt_lines = &anim_virt_lines;
            frame_diag_virt = &anim_diag_virt;
            frame_diag = &anim_diag;
            frame_signs = &anim_signs;
            frame_search = &anim_search;
            frame_incsearch = &anim_incsearch;
            // The cursor's row within the viewport and its buffer line (for relative
            // numbers) come from the band: `cur_row - off` rows down, and the band's
            // `numbers` at the cursor row gives the 1-based buffer line.
            cursor_row = cur_row.saturating_sub(off) as u16;
            current_line = a.numbers.get(cur_row).copied().flatten().unwrap_or(1);
        }
        None => {
            frame_lines = &win.lines;
            frame_sel = &win.selection;
            frame_secondary_sel = &win.secondary_selection;
            frame_numbers = &win.numbers;
            frame_continuation = &win.continuation;
            frame_hl = &win.highlights;
            frame_inlay = &win.inlay_hints;
            frame_virt_text = &win.virt_text;
            frame_virt_lines = &win.virt_lines;
            frame_diag_virt = &win.diagnostics_virt;
            frame_diag = &win.diagnostics;
            frame_signs = &win.diagnostics_signs;
            frame_search = &win.search;
            frame_incsearch = &win.incsearch;
            cursor_row = win.cursor_row;
            current_line = win.cursor_line;
        }
    }

    // Paint the text body with the window background first (when a colorscheme is
    // loaded), so every following widget's spans patch their foreground onto it
    // and the gutter, end-of-line gaps, and `~` rows all share it. A float uses
    // `NormalFloat` so its body matches the bordered box around it; a tiled window
    // uses `Normal`. With no theme this is skipped.
    if let Some(bg) = window_bg(win, view) {
        frame.render_widget(Block::default().style(bg), text_area);
    }

    // The line-background layer (`line_hl_group` — e.g. rendered-markdown code
    // blocks in a doc float): tint each marked screen row across the whole window
    // width, the `'cursorline'` model (a full-width `Block` under the gutter, text,
    // and overlays, so those all draw on top and syntax colouring composes with the
    // tint). Painted *before* the cursorline tint so the cursor's active line still
    // wins on a row that carries both. Rows the server didn't mark add nothing.
    for &(brow, style) in &win.line_bg {
        let row = text_area.y + brow.min(text_area.height.saturating_sub(1));
        let line_area = Rect {
            x: text_area.x,
            y: row,
            width: text_area.width,
            height: 1,
        };
        frame.render_widget(Block::default().style(rt(style)), line_area);
    }

    // `'cursorline'`: tint the cursor's screen row across the whole window width
    // (sign column, gutter, and text). Painted right after the `Normal` background
    // so the gutter numbers, text spans, and overlays (selection / search /
    // diagnostics) all draw on top — only cells they don't claim show the tint.
    // The `~` filler rows below the buffer never carry the cursor, so a short
    // buffer's cursorline still lands on a real line.
    if win.cursorline {
        let row = text_area.y + cursor_row.min(text_area.height.saturating_sub(1));
        let line_area = Rect {
            x: text_area.x,
            y: row,
            width: text_area.width,
            height: 1,
        };
        frame.render_widget(
            Block::default().style(cursorline_style(win, view)),
            line_area,
        );
    }

    // The fold-marker gutter sits at the very left (vim's foldcolumn, before the
    // sign and number columns). Painted from the server's per-row `foldcolumn`
    // strings; absent when `'foldcolumn'` is `0`.
    let text_area = if win.foldcolumn_width > 0 {
        let cols =
            Layout::horizontal([Constraint::Length(win.foldcolumn_width), Constraint::Min(0)])
                .split(text_area);
        render_fold_column(frame, cols[0], &win.foldcolumn, win, view);
        cols[1]
    } else {
        text_area
    };

    // Reserve the diagnostic sign column at the far left (vim's signcolumn, left of
    // the number gutter). Its width comes from the server's resolved `signcolumn`
    // policy (`0` = no column); glyphs are painted below, once the palette is built.
    let (sign_area, gutter_area) = if win.sign_width > 0 {
        let cols = Layout::horizontal([Constraint::Length(win.sign_width), Constraint::Min(0)])
            .split(text_area);
        (Some(cols[0]), cols[1])
    } else {
        (None, text_area)
    };

    // Split a number-column gutter off the left of the remaining body when enabled.
    let text_inner = if win.number_width > 0 {
        let cols = Layout::horizontal([Constraint::Length(win.number_width), Constraint::Min(0)])
            .split(gutter_area);
        render_gutter(
            frame,
            cols[0],
            frame_numbers,
            frame_continuation,
            current_line,
            win,
            view,
        );
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
        end_of_buffer: win.end_of_buffer(view).map(rt),
    };
    // Diagnostic signs ride the band (`frame_signs`, sliced in the interpolation
    // match) just like the underlines and the other overlays, so they slide with the
    // text instead of blanking out for the slide. Painted now that the palette
    // resolves style ids.
    if let Some(sign_area) = sign_area {
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
        frame_diag_virt,
        frame_virt_text,
        frame_virt_lines,
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
        inlay_cursor_shift(frame_inlay, cursor_row, win.cursor_screen_col, win.leftcol)
            .saturating_add(virt_cursor_shift(
                frame_virt_text,
                cursor_row,
                win.cursor_screen_col,
                win.leftcol,
            ));
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
    let style = separator_style(view);
    for sep in separators {
        // Each separator is relative to its region's content origin.
        let wins_area = dock.content(sep.region);
        let x = wins_area.x + sep.x;
        let y = wins_area.y + sep.y;
        // Clamp the run to the windows area so a separator near an edge can't paint
        // past it (the guard reproduces the old zero-size-rect clamp exactly).
        if sep.vertical {
            if wins_area.right() > x {
                let len = sep.length.min(wins_area.bottom().saturating_sub(y));
                paint_vline(frame, x, y, len, "│", style);
            }
        } else if wins_area.bottom() > y {
            let len = sep.length.min(wins_area.right().saturating_sub(x));
            paint_hline(frame, x, y, len, "─", style);
        }
    }
}

/// Paint the line-number column. Each row shows, per the active options:
/// absolute numbers (`number`), distance-from-cursor (`relativenumber`), or the
/// hybrid — absolute on the cursor line, relative elsewhere — when both are on.
/// The cursor line uses the theme's `CursorLineNr`, other rows its `LineNr`;
/// with no colorscheme loaded they fall back to un-dimmed / dimmed (vim's look
/// out of the box). `~` filler rows and soft-wrap continuation rows
/// (`continuation[i]`) get a blank gutter — a wrapped line's number shows on its
/// first display row only.
fn render_gutter(
    frame: &mut Frame,
    area: Rect,
    numbers: &[Option<usize>],
    continuation: &[bool],
    current_line: usize,
    win: &WindowView,
    view: &View,
) {
    let width = area.width as usize;
    let text = Text::from(
        numbers
            .iter()
            .enumerate()
            .map(|(row, num)| {
                let is_current = *num == Some(current_line);
                // Blank the number on a soft-wrap continuation row (vim shows it on
                // the line's first row only); `numbers` still carries the line there.
                let shown = if continuation.get(row).copied().unwrap_or(false) {
                    None
                } else {
                    *num
                };
                let cell = gutter_cell(shown, current_line, win.number, win.relativenumber, width);
                let style = if is_current {
                    win.cursor_line_nr(view).map(rt).unwrap_or_default()
                } else {
                    win.line_nr(view)
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

/// Paint the fold-marker gutter reserved by [`render_window`]: each visible row's
/// `foldcolumn` string (`-`/`│` for open folds, `+` for a closed one, blanks
/// elsewhere), in the line-number palette style. The strings are already exactly
/// the column width; `pad_to_width` guards a short/over-long one.
fn render_fold_column(
    frame: &mut Frame,
    area: Rect,
    foldcolumn: &[String],
    win: &WindowView,
    view: &View,
) {
    let width = area.width as usize;
    let style = win
        .line_nr(view)
        .map(rt)
        .unwrap_or_else(|| Style::default().add_modifier(Modifier::DIM));
    let text = Text::from(
        (0..area.height as usize)
            .map(|row| {
                let cell = foldcolumn.get(row).map(String::as_str).unwrap_or("");
                Line::from(Span::styled(pad_to_width(cell, width), style))
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
    virt_text: &[Vec<VirtPlacement>],
    virt_lines: &[Option<Vec<VirtChunk>>],
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
    let empty_virt_text: Vec<VirtPlacement> = Vec::new();
    let text = Text::from(
        lines
            .iter()
            .enumerate()
            .map(|(row, l)| {
                // A virtual row (`virt_lines`) is a whole extra screen line of
                // extmark chunks interleaved above / below its buffer line — no
                // buffer text, gutter number, selection, or cursor. It also has a
                // `None` number, so check it *before* the `~`-filler test below.
                if let Some(Some(chunks)) = virt_lines.get(row) {
                    return virt_line(chunks, width, theme);
                }
                let sel = selection.get(row).copied().flatten();
                let sec_sel = secondary_selection.get(row).unwrap_or(&empty_search);
                let matches = search.get(row).unwrap_or(&empty_search);
                let cur = incsearch.get(row).copied().flatten();
                let hl = highlights.get(row).unwrap_or(&empty);
                let diag = diagnostics.get(row).unwrap_or(&empty_diag);
                let virt = diagnostics_virt.get(row).and_then(Option::as_ref);
                let vtext = virt_text.get(row).unwrap_or(&empty_virt_text);
                let inlay = inlay_hints.get(row).unwrap_or(&empty_inlay);
                // A row with no buffer line (and no virtual content) is a `~`
                // end-of-buffer filler.
                let is_filler = matches!(numbers.get(row), Some(None));
                highlight_line(
                    l, sel, sec_sel, matches, cur, hl, diag, virt, vtext, inlay, width, is_filler,
                    tabstop, leftcol, theme,
                )
            })
            .collect::<Vec<_>>(),
    );
    frame.render_widget(Paragraph::new(text), area);
}

/// Build one **virtual line** (`virt_lines`) row from its chunk run: each chunk's
/// text painted in its resolved style (`virt_chunk_style` — the palette entry the
/// server interned, else the window's normal colors), laid out from the left edge
/// of the text body and clamped to the viewport width. A virtual row carries no
/// buffer text, so there is no tab expansion, selection, search, or cursor to
/// overlay — just the chunks. (`virt_lines_leftcol` / horizontal scroll of virtual
/// rows is a later refinement; today they start at the text body's left edge.)
fn virt_line(chunks: &[VirtChunk], max_width: usize, theme: &LineTheme) -> Line<'static> {
    let mut spans: Vec<Span> = Vec::new();
    let mut painted = 0usize;
    for (text, id) in chunks {
        if painted >= max_width {
            break;
        }
        let shown = truncate_to_width(text, max_width - painted);
        if shown.is_empty() {
            continue;
        }
        painted += str_width(&shown);
        spans.push(Span::styled(shown, virt_chunk_style(*id, theme)));
    }
    Line::from(spans)
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
    vtext: &[VirtPlacement],
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
            expanded.into_owned(),
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
    // Inline `virt_text` placements splice into the stream just like inlay hints
    // (server-sorted ascending by screen column). Both push the following glyphs —
    // and the cursor (see `virt_cursor_shift`) — right.
    let inline: Vec<&VirtPlacement> = vtext.iter().filter(|p| p.pos == VIRT_POS_INLINE).collect();
    // Overlay + win_col placements draw *over* the cells at their column (no shift),
    // suppressing the real glyphs they cover. Sorted ascending by column so the
    // walk meets them in order.
    let mut overlays: Vec<&VirtPlacement> = vtext
        .iter()
        .filter(|p| p.pos == VIRT_POS_OVERLAY || p.pos == VIRT_POS_WIN_COL)
        .collect();
    overlays.sort_by_key(|p| p.col);

    let mut spans: Vec<Span> = Vec::new();
    let mut run = String::new();
    let mut run_style = Style::default();
    let mut col = 0usize;
    let mut hi = 0usize; // next hint to emit
    let mut vi = 0usize; // next inline virt_text placement to emit
    let mut oi = 0usize; // next overlay placement to emit
    let mut overlay_end = 0usize; // absolute col real glyphs are suppressed up to
    let mut inserted = 0usize; // visible hint / inline-virt cells spliced in so far
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
        while vi < inline.len() && (inline[vi].col as usize) <= col {
            emit_inline_virt(
                inline[vi],
                &mut spans,
                &mut run,
                &mut run_style,
                col,
                leftcol,
                max_width,
                &mut inserted,
                theme,
            );
            vi += 1;
        }
        while oi < overlays.len() && (overlays[oi].col as usize) <= col {
            // The cell the overlay starts on, for the `hl_mode` combine/blend merge
            // (replace ignores it). Resolved from the same walk the real glyphs use.
            let under = cell_style(
                overlays[oi].col as usize,
                sel,
                secondary_sel,
                search,
                incsearch,
                hl,
                diag,
                theme,
            );
            overlay_end = overlay_end.max(emit_overlay(
                overlays[oi],
                &mut spans,
                &mut run,
                &mut run_style,
                col,
                leftcol,
                max_width,
                inserted,
                under,
                theme,
            ));
            oi += 1;
        }
        // A glyph the overlay covers (`col < overlay_end`) is suppressed: the
        // overlay text painted it. Past the overlay, painting resumes normally.
        if col >= leftcol && col >= overlay_end {
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
    // Inline virt_text anchored at or past end-of-text.
    while vi < inline.len() {
        emit_inline_virt(
            inline[vi],
            &mut spans,
            &mut run,
            &mut run_style,
            col,
            leftcol,
            max_width,
            &mut inserted,
            theme,
        );
        vi += 1;
    }

    // Visible cells painted so far (text past the horizontal scroll, plus the
    // inline hint / virt_text cells). The eol virt text below is clamped against
    // this so it never overruns the viewport.
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

    // Overlay / win_col placements anchored at or past end-of-text: pad to the
    // placement's column, then draw its chunks (e.g. a fixed-column guide on a short
    // line). No suppression past EOL — there are no real glyphs left to cover.
    if !is_filler {
        while oi < overlays.len() {
            let p = overlays[oi];
            oi += 1;
            let target = (p.col as usize).saturating_sub(leftcol) + inserted;
            if target < painted || target >= max_width {
                continue; // behind the cursor of painted text, or off the right edge
            }
            if target > painted {
                spans.push(Span::raw(" ".repeat(target - painted)));
                painted = target;
            }
            // Past end-of-text there's no cell underneath, so `hl_mode` is moot — the
            // chunk paints in its own style (replace).
            painted += push_virt_chunks(
                &mut spans,
                &p.chunks,
                painted,
                max_width,
                theme,
                Style::default(),
                0,
            );
        }
    }

    // Extmark end-of-line virtual text. Each `eol` placement paints after a
    // one-cell gap past the text painted so far; its chunks paint in order, each in
    // its own resolved style. Truncated to the remaining viewport width; never on a
    // `~` filler row.
    if !is_filler {
        for placement in vtext {
            if placement.pos != VIRT_POS_EOL {
                continue;
            }
            if painted + 1 >= max_width {
                break;
            }
            spans.push(Span::raw(" "));
            painted += 1;
            // eol text paints in the empty space past the line — nothing underneath, so
            // replace (its own style) regardless of `hl_mode`.
            painted += push_virt_chunks(
                &mut spans,
                &placement.chunks,
                painted,
                max_width,
                theme,
                Style::default(),
                0,
            );
        }
    }

    // Right-aligned virtual text: flush every `right_align` placement's chunks to
    // the window's right edge (stacked in placement order). Skipped on a `~` filler
    // row, and clamped to start no earlier than the painted text (so it never
    // overlaps real content — it left-justifies and truncates if the row is full).
    if !is_filler {
        let ra: Vec<&(String, Option<usize>)> = vtext
            .iter()
            .filter(|p| p.pos == VIRT_POS_RIGHT_ALIGN)
            .flat_map(|p| p.chunks.iter())
            .collect();
        if !ra.is_empty() {
            let total: usize = ra.iter().map(|(t, _)| str_width(t)).sum();
            let start = max_width.saturating_sub(total).max(painted);
            if start > painted && start < max_width {
                spans.push(Span::raw(" ".repeat(start - painted)));
                painted = start;
            }
            for (text, id) in ra {
                if painted >= max_width {
                    break;
                }
                let shown = truncate_to_width(text, max_width - painted);
                if shown.is_empty() {
                    break;
                }
                painted += str_width(&shown);
                spans.push(Span::styled(shown, virt_chunk_style(*id, theme)));
            }
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

/// The `virt_text` placement `pos` tags (mirror the server's `VirtTextPos`):
/// `0`=eol (after end-of-text), `1`=inline (spliced into the row, shifting text),
/// `2`=overlay (drawn over the cells at its column, no shift), `3`=right_align
/// (flush to the window's right edge), `4`=win_col (overlaid at a fixed window
/// column).
const VIRT_POS_EOL: u8 = 0;
const VIRT_POS_INLINE: u8 = 1;
const VIRT_POS_OVERLAY: u8 = 2;
const VIRT_POS_RIGHT_ALIGN: u8 = 3;
const VIRT_POS_WIN_COL: u8 = 4;

/// The style for one extmark virtual-text chunk: the colorscheme palette entry the
/// server resolved its `hl_group` to, or the window's normal foreground when the
/// chunk carried no group (or it didn't resolve). This is the chunk's *own* style,
/// before any `hl_mode` merge with the cell underneath (see [`apply_hl_mode`]).
fn virt_chunk_style(id: Option<usize>, theme: &LineTheme) -> Style {
    id.and_then(|i| theme.palette.get(i))
        .copied()
        .unwrap_or_default()
}

/// The `hl_mode` wire codes (mirror the server's [`nxvim_core::HlMode`]):
/// `0`=replace (the default), `1`=combine, `2`=blend.
const HL_MODE_COMBINE: u8 = 1;
const HL_MODE_BLEND: u8 = 2;

/// Resolve a virtual-text chunk's final style given its `hl_mode` and the style of
/// the cell it paints **over**. Only `overlay` / `win_col` placements sit over real
/// cells — `eol` / `inline` / `right_align` have nothing underneath, so they always
/// pass `under == Style::default()` and render as `replace`.
///
/// - `replace` (the default): the chunk's own style, ignoring the cell beneath it.
/// - `combine`: the chunk's *set* attributes layered over the underlying cell, so
///   where the chunk leaves a color/attr unset the cell's shows through
///   (`Style::patch`, the same merge the cell-style walk uses).
/// - `blend`: like `combine`, but a truecolor fg/bg present on *both* sides is
///   averaged channel-wise (a non-truecolor or one-sided color falls back to the
///   `combine` pick). A coarse approximation of neovim's alpha blend — exact pixel
///   blending isn't expressible in a terminal cell — but it reads as a tint of the
///   text underneath rather than an opaque replace.
fn apply_hl_mode(chunk: Style, under: Style, mode: u8) -> Style {
    match mode {
        HL_MODE_COMBINE => under.patch(chunk),
        HL_MODE_BLEND => {
            let mut out = under.patch(chunk);
            out.fg = blend_color(under.fg, chunk.fg);
            out.bg = blend_color(under.bg, chunk.bg);
            out
        }
        _ => chunk,
    }
}

/// Average two cell colors channel-wise when **both** are truecolor; otherwise keep
/// the chunk's color when set, else the underlying one. Used by [`apply_hl_mode`]'s
/// `blend` path.
fn blend_color(under: Option<Color>, chunk: Option<Color>) -> Option<Color> {
    match (under, chunk) {
        (Some(Color::Rgb(r1, g1, b1)), Some(Color::Rgb(r2, g2, b2))) => {
            let mix = |a: u8, b: u8| ((a as u16 + b as u16) / 2) as u8;
            Some(Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2)))
        }
        (under, chunk) => chunk.or(under),
    }
}

/// Total display width of `s` in screen cells (wide chars by their width).
fn str_width(s: &str) -> usize {
    s.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
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

/// Push virtual-text chunks into `spans` starting at viewport column `painted`,
/// each truncated to the remaining viewport width and styled by its resolved
/// palette id (the window's normal color when the chunk carried no group, or it
/// didn't resolve). Returns the total display width added. Shared by the eol,
/// overlay, win_col, and right_align render paths.
///
/// `under`/`mode` carry the `hl_mode` merge (see [`apply_hl_mode`]): the overlay /
/// win_col paths pass the style of the cell beneath the placement so `combine` /
/// `blend` can tint the underlying color; eol / right_align pass
/// `Style::default()` + replace, since nothing sits underneath them.
fn push_virt_chunks(
    spans: &mut Vec<Span<'static>>,
    chunks: &[(String, Option<usize>)],
    painted: usize,
    max_width: usize,
    theme: &LineTheme,
    under: Style,
    mode: u8,
) -> usize {
    let mut added = 0usize;
    for (text, id) in chunks {
        let at = painted + added;
        if at >= max_width {
            break;
        }
        let shown = truncate_to_width(text, max_width - at);
        if shown.is_empty() {
            break;
        }
        added += str_width(&shown);
        let style = apply_hl_mode(virt_chunk_style(*id, theme), under, mode);
        spans.push(Span::styled(shown, style));
    }
    added
}

/// Draw one overlay / win_col `virt_text` placement *over* the cells at its column:
/// flush the pending text run, then push the chunks at the placement's viewport
/// position. Returns the absolute column the overlay extends to, so the caller
/// suppresses the real glyphs it covers (overlay replaces, it does not shift). A
/// placement scrolled off the left (`col < leftcol`) paints nothing and suppresses
/// nothing.
///
/// `under` is the style of the cell the overlay starts on (the caller resolves it
/// from the same cell-style walk the real glyphs use), so the placement's `hl_mode`
/// can `combine`/`blend` the chunk with the text it covers — see [`apply_hl_mode`].
/// Computed at the start column and applied to the whole placement (a short overlay
/// usually sits over a uniform run; per-cell merge is deferred, like wide-char).
#[allow(clippy::too_many_arguments)]
fn emit_overlay(
    placement: &VirtPlacement,
    spans: &mut Vec<Span<'static>>,
    run: &mut String,
    run_style: &mut Style,
    col: usize,
    leftcol: usize,
    max_width: usize,
    inserted: usize,
    under: Style,
    theme: &LineTheme,
) -> usize {
    if (placement.col as usize) < leftcol {
        return col;
    }
    if !run.is_empty() {
        spans.push(Span::styled(std::mem::take(run), *run_style));
    }
    let painted = col.saturating_sub(leftcol) + inserted;
    col + push_virt_chunks(
        spans,
        &placement.chunks,
        painted,
        max_width,
        theme,
        under,
        placement.hl_mode,
    )
}

/// Splice one inline `virt_text` placement into the row's span stream at its anchor
/// column: flush the pending text run, then push each chunk (truncated to the
/// remaining viewport width) in its own resolved style. The inline analogue of
/// [`emit_inlay_hint`], but for a multi-chunk run. A placement scrolled off the
/// left (`col < leftcol`) is skipped; `inserted` accumulates the visible cells so
/// the caller tracks the shift the splice adds to the following glyphs (and the
/// cursor, via [`virt_cursor_shift`]).
#[allow(clippy::too_many_arguments)]
fn emit_inline_virt(
    placement: &VirtPlacement,
    spans: &mut Vec<Span<'static>>,
    run: &mut String,
    run_style: &mut Style,
    col: usize,
    leftcol: usize,
    max_width: usize,
    inserted: &mut usize,
    theme: &LineTheme,
) {
    if (placement.col as usize) < leftcol {
        return; // scrolled off the left edge.
    }
    let mut flushed = false;
    for (text, id) in &placement.chunks {
        let painted = col.saturating_sub(leftcol) + *inserted;
        let shown = truncate_to_width(text, max_width.saturating_sub(painted));
        if shown.is_empty() {
            break; // no room left in the viewport.
        }
        if !flushed && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(run), *run_style));
            flushed = true;
        }
        *inserted += UnicodeWidthStr::width(shown.as_str());
        spans.push(Span::styled(shown, virt_chunk_style(*id, theme)));
    }
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
        // Saturate the width sum: a malicious/buggy server can place arbitrarily wide
        // hints, and a bare `sum()` would overflow `u16` (a debug-build panic).
        .fold(0u16, |acc, (_, text, _)| {
            acc.saturating_add(UnicodeWidthStr::width(text.as_str()) as u16)
        })
}

/// The combined width of the inline `virt_text` placements on `cursor_row` at or
/// before `cursor_col` (and inside the horizontal scroll) — how far the inline
/// splice pushes the cursor right. The `virt_text` analogue of
/// [`inlay_cursor_shift`], summing each inline placement's chunk widths.
fn virt_cursor_shift(
    virt_text: &[Vec<VirtPlacement>],
    cursor_row: u16,
    cursor_col: u16,
    leftcol: u16,
) -> u16 {
    virt_text
        .get(cursor_row as usize)
        .into_iter()
        .flatten()
        .filter(|p| p.pos == VIRT_POS_INLINE && p.col >= leftcol && p.col <= cursor_col)
        .flat_map(|p| p.chunks.iter())
        // Saturate the width sum (see `inlay_cursor_shift`): a `u16` `sum()` over
        // server-supplied chunk widths would overflow on absurd input.
        .fold(0u16, |acc, (text, _)| {
            acc.saturating_add(UnicodeWidthStr::width(text.as_str()) as u16)
        })
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
fn expand_tabs(line: &str, tabstop: usize) -> Cow<'_, str> {
    if !line.contains('\t') {
        // The overwhelmingly common (tab-free) case borrows the line untouched, so
        // a repaint allocates nothing per row here.
        return Cow::Borrowed(line);
    }
    let tabstop = tabstop.max(1);
    let mut out = String::with_capacity(line.len() + tabstop);
    let mut col = 0;
    for ch in line.chars() {
        if ch == '\t' {
            let spaces = tabstop - (col % tabstop);
            for _ in 0..spaces {
                out.push(' ');
            }
            col += spaces;
        } else {
            out.push(ch);
            col += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    Cow::Owned(out)
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
    let base = status_line_style(view);

    let spans: Vec<Span> = segments
        .iter()
        .map(|(text, style)| {
            let style = style.map_or(base, |s| {
                let merged = base.patch(rt(s));
                // The base's REVERSED is a fallback that fakes fg/bg contrast on an
                // unstyled bar (no `StatusLine` group loaded). A segment that brings
                // its own background supplies real colours, so the reverse-video must
                // not leak through and swap them — clear it (via `sub_modifier`, so it
                // also overrides the `Paragraph` base set across the row below) unless
                // the segment asked for reverse itself. Without this a themed
                // statusline (lualine et al.) renders inverted.
                if s.bg.is_some() && !s.reverse {
                    merged.remove_modifier(Modifier::REVERSED)
                } else {
                    merged
                }
            });
            Span::styled(text.as_str(), style)
        })
        .collect();

    frame.render_widget(Paragraph::new(Line::from(spans)).style(base), area);
}

/// The base style for the command-line / message row: the theme's `MsgArea`
/// group layered over `Normal`, so the row picks up the colorscheme's background
/// (vim's default — the message area tracks `Normal` unless `MsgArea` overrides
/// it). Empty when no theme is loaded, leaving the terminal default.
fn msg_area_style(view: &View) -> Style {
    let base = view.normal.map(rt).unwrap_or_default();
    match view.msg_area {
        Some(m) => base.patch(rt(m)),
        None => base,
    }
}

fn render_command(frame: &mut Frame, area: Rect, view: &View) {
    let base = msg_area_style(view);
    // Fill the whole row with the message-area background first, so the idle row
    // (and the blank tail past a short command / message) carries the theme rather
    // than the terminal default.
    frame.render_widget(Paragraph::new("").style(base), area);
    if view.command_mode {
        frame.render_widget(
            Paragraph::new(format!("{}{}", cmdline_prompt_str(view), view.cmdline)).style(base),
            area,
        );
    } else if !view.message.is_empty() {
        // An error message paints with the theme's `ErrorMsg` (red foreground when
        // the colorscheme leaves it undefined); a normal message keeps the msg-area
        // base.
        let style = if view.message_error {
            base.patch(
                view.error_msg
                    .map(rt)
                    .unwrap_or_else(|| Style::default().fg(Color::Red)),
            )
        } else {
            base
        };
        frame.render_widget(Paragraph::new(view.message.as_str()).style(style), area);
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
        frame.render_widget(Paragraph::new(Line::from(spans)).style(base), area);
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
                Some(s) => Span::styled(text.as_str(), rt(*s)),
                None => Span::raw(text.as_str()),
            })
            .collect();
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    render_tab_cells(frame, area, "", &view.tabline, view.current_tab, view);
}

/// Paint built-in tabline cells into `area`: an optional bold `title` label first
/// (the `nx.dock` dock title), then one ` {count} {name}{+} ` cell per tab (the
/// window count only when >1, a `+` when modified — vim's default), the `current`
/// cell highlighted and the strip past the last cell left blank (vim's
/// `TabLineFill`). Shared by the global (main) tabline and each dock's own
/// tabline. A no-op for an empty strip (no title and no tabs) or zero-height area.
///
/// The colors come from the theme's `TabLine` (inactive cells + bold title),
/// `TabLineSel` (the active cell) and `TabLineFill` (the bar background) groups
/// when the colorscheme defines them; otherwise the active cell falls back to
/// reverse-video and the rest to the terminal default.
fn render_tab_cells(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    tabs: &[TabData],
    current: usize,
    view: &View,
) {
    if (title.is_empty() && tabs.is_empty()) || area.height == 0 {
        return;
    }
    let inactive = view.tabline_style.map(rt).unwrap_or_default();
    let active = view
        .tabline_sel
        .map(rt)
        .unwrap_or_else(|| Style::default().add_modifier(Modifier::REVERSED));
    let fill = view.tabline_fill.map(rt).unwrap_or_default();
    let mut spans: Vec<Span> = Vec::with_capacity(tabs.len() + 1);
    if !title.is_empty() {
        spans.push(Span::styled(
            format!(" {title} "),
            inactive.add_modifier(Modifier::BOLD),
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
        let style = if i == current { active } else { inactive };
        spans.push(Span::styled(text, style));
    }
    // Paint the strip past the last cell with `TabLineFill` so the bar's background
    // matches the theme rather than the terminal default.
    frame.render_widget(Paragraph::new(Line::from(spans)).style(fill), area);
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

/// Display width (in cells — wide chars count two) of the leading prompt, used to
/// place the command cursor past it. Matches the server's `cmdline_prompt_width`
/// (`unicode::display_width`), so the wildmenu anchor and the caret agree.
fn cmdline_prompt_width(view: &View) -> u16 {
    u16::try_from(str_width(&cmdline_prompt_str(view))).unwrap_or(u16::MAX)
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
    // border sits one cell before it. The command-line wildmenu is the mirror image:
    // it omits its *bottom* border so the list sits flush against the command line
    // it floats above.
    let (borders, vborder) = if menu.cmdline {
        (Borders::LEFT | Borders::RIGHT | Borders::TOP, 1)
    } else if menu.border_top {
        (Borders::ALL, 2)
    } else {
        (Borders::LEFT | Borders::RIGHT | Borders::BOTTOM, 1)
    };
    let left_shift = u16::from(!menu.border_top);
    let area = if menu.cmdline {
        // The command-line wildmenu: a fully-bordered box floating just *above* the
        // command line. `text_area` here is the command-line area (frame-bottom, no
        // number gutter), so the box aligns to the command line, not the focused
        // window. `col` is a column within the command line (the token's start), and
        // the box grows upward from the command-line row.
        let x = text_area.x.saturating_add(menu.col);
        let width = menu
            .width
            .saturating_add(2)
            .min(text_area.width.saturating_sub(menu.col).max(1));
        // The box can use every row above the command line.
        let height = menu.height.saturating_add(vborder).min(text_area.y);
        let y = text_area.y.saturating_sub(height);
        Rect {
            x,
            y,
            width,
            height,
        }
    } else {
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
        Rect {
            x,
            y,
            width,
            height,
        }
    };
    if area.width < 3 || area.height < vborder + 1 {
        return None;
    }
    // Themed colors (nvim-cmp / telescope groups, resolved server-side): the box
    // background, the border, the selected row, and the matched characters. Each is
    // `None` when the colorscheme leaves its group undefined — the popup then keeps
    // the built-in look (no bg, plain border, reverse-video selection, bold match).
    let bg_style = menu.styles.bg.map(rt).unwrap_or_default();
    let sel_style = menu.styles.sel.map(rt);
    let match_style = menu.styles.matched.map(rt);
    let mut block = Block::new().borders(borders).style(bg_style);
    if let Some(b) = menu.styles.border.map(rt) {
        block = block.border_style(b);
    }
    // The picker box's title (`nx.picker.open{ title = … }`), centered on the top
    // border. Only a fully-bordered picker carries one (the wildmenu / completion
    // leave it `None`), so it never collides with the missing top border.
    if let Some(title) = menu.title.as_deref().filter(|t| !t.is_empty()) {
        let mut line = Line::from(format!(" {title} ")).centered();
        if let Some(ts) = menu.styles.title.map(rt) {
            line = line.style(ts);
        }
        block = block.title_top(line);
    }
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

    // Multi-select: when any row is marked, draw a 2-cell marker gutter on every
    // list row (a glyph on marked rows). Only when marks are in play, so a plain
    // picker / `nx.ui.select` / completion popup renders exactly as before.
    let any_marked = menu.marked.iter().any(|&m| m);
    // Build one list row (or a blank filler past the end of the list).
    let list_line = |r: usize| -> Line<'static> {
        let idx = start + r;
        match menu.items.get(idx) {
            Some(label) => {
                let empty = Vec::new();
                let spans = menu.match_spans.get(idx).unwrap_or(&empty);
                let marked = any_marked.then(|| menu.marked.get(idx).copied().unwrap_or(false));
                menu_row_line(
                    label,
                    spans,
                    sel == Some(idx),
                    width,
                    marked,
                    sel_style,
                    match_style,
                )
            }
            None => Line::from(" ".repeat(width)),
        }
    };
    // `> query` prompt line — the query in bold so it reads as the live input.
    let prompt_style = menu
        .styles
        .prompt
        .map(rt)
        .unwrap_or_else(|| Style::default().add_modifier(Modifier::DIM));
    let prompt_line = || -> Line<'static> {
        let query = menu.query.as_deref().unwrap_or("");
        Line::from(vec![
            Span::styled("> ", prompt_style),
            Span::styled(
                pmenu_row(query, "", width.saturating_sub(2)),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ])
    };
    // The internal division rules (prompt/list horizontal rule, preview vertical
    // rule) take the themed border style — the same `TelescopeBorder`/`FloatBorder`
    // group the box border uses (line ~2493) — so all three clients agree. Only when
    // the colorscheme leaves the group undefined do we fall back to plain DIM.
    let sep_style = menu
        .styles
        .border
        .map(rt)
        .unwrap_or_else(|| Style::default().add_modifier(Modifier::DIM));
    // A full-width horizontal rule separating the prompt from the list.
    let sep_line = || -> Line<'static> { Line::from(Span::styled("─".repeat(width), sep_style)) };

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
        // The command-line wildmenu floats *above* its input, so flip the list to
        // keep the best match (row 0) at the bottom, nearest the command cursor —
        // the mirror of a below-cursor completion popup.
        if menu.cmdline {
            lines.reverse();
        }
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), list_area);

    // The preview column: a vertical separator rule, then the windowed file.
    if let Some((sep_x, preview_area, pv)) = preview_layout {
        paint_vline(frame, sep_x, inner.y, inner.height, "│", sep_style);
        let palette: Vec<Style> = styles.iter().copied().map(rt).collect();
        render_preview(frame, preview_area, pv, &palette);
    }

    // (The completion / cmdline **docs** are no longer a `menu.docs` overlay — they
    // render as real doc-float windows through the normal window path, so nothing to
    // draw here.)

    // The terminal caret sits in the prompt (in the list column), past the `> `
    // prefix at the query's text-cursor column (clamped inside the column).
    // `query_cursor` is a *char* offset (the server's `cursor_chars()`); the caret
    // column is display cells, so measure the painted width of the chars before it
    // (a wide CJK char in the query occupies two cells).
    prompt_row.map(|row| {
        let qw: usize = menu
            .query
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(menu.query_cursor as usize)
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
            .sum();
        let caret = u16::try_from(2 + qw)
            .unwrap_or(u16::MAX)
            .min(list_area.width.saturating_sub(1));
        (list_area.x + caret, list_area.y + row as u16)
    })
}

/// Render the list-less content float (`nx.ui.float`; LSP hover / signature help):
/// a bordered box of plain content lines at the server-placed geometry (same
/// text-area-relative convention as the docs sidebar). No selection, no scrolling —
/// the server already windowed the lines to `height`. A `None` border draws the
/// content with no box.
fn render_content_float(
    frame: &mut Frame,
    text_area: Rect,
    float: &nxvim_view::ContentFloatData,
    styles: &[nxvim_view::Style],
    theme: FloatTheme,
) {
    let bordered = float.border.is_some();
    let chrome = if bordered { 2 } else { 0 };
    let x = text_area.x.saturating_add(float.col);
    let y = text_area.y.saturating_add(float.row);
    let width = (float.width.saturating_add(chrome)).min(text_area.right().saturating_sub(x));
    let height = (float.height.saturating_add(chrome)).min(text_area.bottom().saturating_sub(y));
    let area = Rect {
        x,
        y,
        width,
        height,
    };
    let min = if bordered { 3 } else { 1 };
    if area.width < min || area.height < min {
        return;
    }
    frame.render_widget(Clear, area);
    let inner = match float.border.map(bt) {
        Some(border) => {
            let block = float_block(border, float.title.as_deref(), theme);
            let inner = block.inner(area);
            frame.render_widget(block, area);
            inner
        }
        None => area,
    };
    let w = inner.width as usize;
    let lines: Vec<Line> = float
        .lines
        .iter()
        .take(inner.height as usize)
        .map(|l| content_float_line(l, w, styles))
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Build one content-float row from its chunk run: each chunk's text painted in
/// its resolved style (the palette entry the server interned, else normal colors),
/// truncated to the box width and padded out to it so the popup background fills
/// the row. A plain caller's single un-styled chunk renders as plain text.
fn content_float_line(
    chunks: &[nxvim_view::VirtChunk],
    width: usize,
    styles: &[nxvim_view::Style],
) -> Line<'static> {
    let mut spans: Vec<Span> = Vec::new();
    let mut painted = 0usize;
    for (text, id) in chunks {
        if painted >= width {
            break;
        }
        let shown = truncate_to_width(text, width - painted);
        if shown.is_empty() {
            continue;
        }
        painted += str_width(&shown);
        let style = id
            .and_then(|i| styles.get(i))
            .copied()
            .map(rt)
            .unwrap_or_default();
        spans.push(Span::styled(shown, style));
    }
    if painted < width {
        spans.push(Span::raw(" ".repeat(width - painted)));
    }
    Line::from(spans)
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
        pmenu_row(&elide_middle(&pv.title, w), "", w),
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
fn menu_row_line(
    label: &str,
    spans: &[(u16, u16)],
    selected: bool,
    width: usize,
    marked: Option<bool>,
    sel_style: Option<Style>,
    match_style: Option<Style>,
) -> Line<'static> {
    // The selected row uses the theme's selection group when defined, else
    // reverse-video. A non-selected row is transparent so the box background shows.
    let base = if selected {
        sel_style.unwrap_or_else(|| Style::default().add_modifier(Modifier::REVERSED))
    } else {
        Style::default()
    };
    // Matched chars use the theme's match group when defined (patched onto the row
    // base so a selected row keeps its selection background), else a bold underline.
    let matched = match match_style {
        Some(m) if selected => base.patch(m),
        Some(m) => m,
        None => base
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::UNDERLINED),
    };
    let mut out: Vec<Span> = Vec::new();
    let mut used = 0usize;
    // Multi-select marker gutter (2 cells), present on every row only while marks
    // are in play so columns stay aligned: a glyph on marked rows, blanks otherwise.
    if let Some(is_marked) = marked {
        let (glyph, st) = if is_marked {
            ("● ", base.add_modifier(Modifier::BOLD))
        } else {
            ("  ", base)
        };
        out.push(Span::styled(glyph.to_string(), st));
        used += 2;
    }
    // Path-priority truncation: when the row overflows, keep the file name (the
    // path tail) on screen by dropping leading directory components behind a `…`,
    // rather than the plain head-cut below that would hide the name. Rows that fit
    // — and non-path rows — fall through unchanged; `spans` are remapped to match.
    let (label, spans) = elide_keep_tail(label, spans, width.saturating_sub(used));
    let (label, spans) = (label.as_str(), spans.as_slice());
    // Coalesce runs of identically-styled chars into one span (the same walk
    // `preview_line` does) instead of a per-char span — a picker frame renders
    // dozens of rows, and per-char `Span`/`String` allocations add up.
    let mut run = String::new();
    let mut run_matched = false;
    for (i, ch) in label.chars().enumerate() {
        if used >= width {
            break;
        }
        let i = i as u16;
        let is_match = spans.iter().any(|(s, e)| i >= *s && i < *e);
        if is_match != run_matched && !run.is_empty() {
            let style = if run_matched { matched } else { base };
            out.push(Span::styled(std::mem::take(&mut run), style));
        }
        run_matched = is_match;
        run.push(ch);
        used += 1;
    }
    if !run.is_empty() {
        let style = if run_matched { matched } else { base };
        out.push(Span::styled(run, style));
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
    let dock = DockLayout::new(
        Rect::new(0, 0, width, height),
        view,
        tabline_rows,
        global_status_rows,
    );
    // A focused float's content sits inside its border; the popup anchors there.
    // The focused window may live in a dock, so offset by its region's origin.
    let area = pad_rect(
        float_inner(
            window_area(dock.content(win.region), win),
            win.border.map(bt),
        ),
        win.padding,
    );
    // The text body is the window rect minus its bottom status row — when this
    // window shows one (per `'laststatus'`); otherwise it claims the whole rect.
    let text_area = if win.status_visible {
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area)[0]
    } else {
        area
    };
    // The fold-marker gutter sits at the very left (before the sign and number
    // columns), mirroring `render_window` — without it the popup anchor drifts
    // left by the foldcolumn width whenever `'foldcolumn'` is on.
    let text_area = if win.foldcolumn_width > 0 {
        Layout::horizontal([Constraint::Length(win.foldcolumn_width), Constraint::Min(0)])
            .split(text_area)[1]
    } else {
        text_area
    };
    // Reserve the sign column first (left of the number gutter), mirroring
    // `render_window`, so the popup anchors past both gutters.
    let gutter_area = if win.sign_width > 0 {
        Layout::horizontal([Constraint::Length(win.sign_width), Constraint::Min(0)])
            .split(text_area)[1]
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
/// in [`render`], so the hit-test lands on exactly the painted cells.
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
/// Shorten `s` to at most `width` chars, keeping its head and tail and dropping the
/// middle behind a single `…` when it won't fit — so a too-long preview path shows
/// both its root and its filename instead of just the start. Char-based, matching
/// the rest of the picker's column math.
fn elide_middle(s: &str, width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= width {
        return s.to_string();
    }
    if width <= 1 {
        return chars.iter().take(width).collect();
    }
    let keep = width - 1; // one column for the `…`
    let front = keep / 2; // favour the tail (filename) with the larger half
    let back = keep - front;
    let head: String = chars[..front].iter().collect();
    let tail: String = chars[chars.len() - back..].iter().collect();
    format!("{head}…{tail}")
}

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

/// Truncate a picker row `label` to `width` columns while keeping the file name (the
/// path tail) visible. When a path overflows the row, drop whole leading directory
/// components behind a single `…` so the file name survives — instead of the plain
/// head-cut the caller would otherwise apply, which truncates the *tail* and hides
/// the very thing you scan for. `spans` (matched-char char ranges into `label`) are
/// remapped onto the returned string so highlights still land on the right chars.
///
/// A row that already fits — or a non-path row (no `/`) — is returned unchanged, so
/// only file paths get the tail-priority treatment; the caller's head-cut still
/// applies to plain labels. When even the file name alone can't fit, the tail is
/// kept (the name is truncated only because it's impossible to show whole).
fn elide_keep_tail(label: &str, spans: &[(u16, u16)], width: usize) -> (String, Vec<(u16, u16)>) {
    let chars: Vec<char> = label.chars().collect();
    let n = chars.len();
    if n <= width || width == 0 || !label.contains('/') {
        return (label.to_string(), spans.to_vec());
    }
    // Reserve one column for the leading `…`; keep at most the last `width - 1` chars.
    let drop = n - (width - 1);
    // Prefer a clean cut just after a path separator — the smallest `/`-boundary at
    // or past `drop` keeps the most directory context that still fits. None ⇒ raw cut.
    let cut = (drop..n).find(|&i| chars[i - 1] == '/').unwrap_or(drop);
    let mut out = String::with_capacity(width);
    out.push('…');
    out.extend(&chars[cut..]);
    // Remap spans: original index `i` (≥ cut) renders at display index `i - cut + 1`
    // (the `…` occupies index 0). A span wholly inside the dropped prefix vanishes.
    let shift = cut as i64 - 1;
    let remapped = spans
        .iter()
        .filter_map(|&(s, e)| {
            let ns = (s as i64).max(cut as i64) - shift;
            let ne = (e as i64).min(n as i64) - shift;
            (ns < ne).then_some((ns as u16, ne as u16))
        })
        .collect();
    (out, remapped)
}
