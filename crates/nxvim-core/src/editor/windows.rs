//! The window layout subsystem: the split tree (`Node`/`WindowTree`), the layout
//! algebra, floating windows, and the `<C-w>`/`:split` window-management methods.
//! `Node` and the layout free functions are private to this module.

use super::*;
use crate::mode::Mode;
use crate::options::{Options, WindowOptions};
use crate::view::Separator;
use std::collections::BTreeMap;

/// A rectangle in terminal cells: top-left origin `(x, y)` plus size. The window
/// layout tree computes one of these per window; the core stays UI-free, so this
/// is a plain struct rather than a ratatui `Rect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Rect {
    pub(crate) x: usize,
    pub(crate) y: usize,
    pub(crate) width: usize,
    pub(crate) height: usize,
}

impl Rect {
    /// The cell at the rect's center, used to compare two windows' positions when
    /// picking a directional-focus or close-survivor neighbor.
    fn center(&self) -> (usize, usize) {
        (self.x + self.width / 2, self.y + self.height / 2)
    }
}

/// What a [`FloatConfig`] positions itself against (`nvim_open_win`'s `relative`):
/// the whole windows area (`editor`), another window's rect (`win`), or the
/// focused window's cursor cell (`cursor`). `relative` values nxvim does not
/// position yet (`mouse`, `tabline`, `laststatus`) are rejected loudly at the RPC
/// boundary rather than silently treated as `editor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatRelative {
    Editor,
    Win(WindowId),
    Cursor,
}

/// Which corner of a float is pinned to its `(row, col)` anchor point
/// (`nvim_open_win`'s `anchor`). `NW` is neovim's default: the top-left corner
/// sits at the anchor and the float extends down-right. An `E` anchor extends
/// left (subtracting the width), an `S` anchor extends up (subtracting the
/// height) — matching neovim's `win_float` corner math.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FloatAnchor {
    #[default]
    NW,
    NE,
    SW,
    SE,
}

impl FloatAnchor {
    /// The `nvim_open_win` `anchor` string for this corner — the inverse of the
    /// RPC/Lua parse, used to format `nvim_win_get_config` and the `vim._wins`
    /// mirror.
    pub fn as_str(self) -> &'static str {
        match self {
            FloatAnchor::NW => "NW",
            FloatAnchor::NE => "NE",
            FloatAnchor::SW => "SW",
            FloatAnchor::SE => "SE",
        }
    }

    /// Parse an `nvim_open_win` `anchor` keyword — the inverse of [`Self::as_str`]
    /// and the single source of truth shared by the RPC and Lua-effect parsers.
    /// `None` for an unrecognized keyword; each caller reports its own
    /// context-specific error (per the no-silent-fallback rule).
    pub fn from_keyword(s: &str) -> Option<Self> {
        Some(match s {
            "NW" => FloatAnchor::NW,
            "NE" => FloatAnchor::NE,
            "SW" => FloatAnchor::SW,
            "SE" => FloatAnchor::SE,
            _ => return None,
        })
    }
}

/// A float's border style (`nvim_open_win`'s `border`). The *width* of the border
/// (one cell on each side when present) is part of the geometry — a bordered
/// float's inner text area is `width - 2 × height - 2` — so it is carried from
/// Phase 1; the actual border glyphs are painted by the TUI in Phase 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BorderStyle {
    #[default]
    None,
    Single,
    Rounded,
    Double,
    Solid,
}

impl BorderStyle {
    /// The `nvim_open_win` `border` string for this style — the inverse of the
    /// RPC/Lua parse, used to format `nvim_win_get_config` and the `vim._wins`
    /// mirror.
    pub fn as_str(self) -> &'static str {
        match self {
            BorderStyle::None => "none",
            BorderStyle::Single => "single",
            BorderStyle::Rounded => "rounded",
            BorderStyle::Double => "double",
            BorderStyle::Solid => "solid",
        }
    }

    /// Parse an `nvim_open_win` `border` keyword — the inverse of [`Self::as_str`]
    /// and the single source of truth shared by the RPC and Lua-effect parsers.
    /// `None` for a style nxvim cannot render yet; each caller reports its own
    /// context-specific error (per the no-silent-fallback rule).
    pub fn from_keyword(s: &str) -> Option<Self> {
        Some(match s {
            "none" => BorderStyle::None,
            "single" => BorderStyle::Single,
            "rounded" => BorderStyle::Rounded,
            "double" => BorderStyle::Double,
            "solid" => BorderStyle::Solid,
            _ => return None,
        })
    }
}

/// A floating window's placement (`nvim_open_win`'s float `config`). Held on
/// [`Window::float`] (`None` for a tiled window); the [`WindowTree`] positions it
/// absolutely each layout from this config, on top of the tiled tree. `width`/
/// `height` are the **inner content** area (neovim's `nvim_open_win` semantics);
/// a border is drawn *outside* it, so the on-screen box is the content plus one
/// border cell per side (see [`place_float`]). `row`/`col` offset the `anchor`
/// corner from the `relative` origin.
///
/// Not `Copy`: `title` owns a `String`. Read it through `Window::float`'s
/// `Option<FloatConfig>` by reference (or `.clone()` where an owned value is
/// needed) — see [`Editor::window_float_config`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloatConfig {
    pub relative: FloatRelative,
    pub anchor: FloatAnchor,
    pub row: isize,
    pub col: isize,
    pub width: usize,
    pub height: usize,
    /// Stacking order; higher floats paint over lower ones. Neovim's default is
    /// 50. Ties break by window id (creation order).
    pub zindex: u32,
    /// Whether `<C-w>` focus commands can land on this float (`nvim_set_current_win`
    /// can always focus it). Honored by the focus cycle in Phase 4.
    pub focusable: bool,
    pub border: BorderStyle,
    /// Optional label drawn on the top border (`nvim_open_win`'s `title`), `None`
    /// for an untitled float. Only meaningful with a `border`; the TUI renders it
    /// on the border's top row.
    pub title: Option<String>,
}

impl Default for FloatConfig {
    fn default() -> Self {
        FloatConfig {
            relative: FloatRelative::Editor,
            anchor: FloatAnchor::NW,
            row: 0,
            col: 0,
            width: 1,
            height: 1,
            zindex: 50,
            focusable: true,
            border: BorderStyle::None,
            title: None,
        }
    }
}

/// A partial `nvim_win_set_config` change ([`Editor::set_window_config`]). Each
/// `Some` field overrides the target window's float placement; a `None` keeps the
/// current value (neovim's "absent keys are unchanged" merge semantics). The
/// merge happens **here** so both callers — the `nvim_win_set_config` RPC and the
/// `WindowOp::SetConfig` drain — send only the keys the caller passed.
///
/// `make_tiled` is the `relative = ""` form: convert a float back into a tiled
/// window (a split of the focused window), ignoring the placement fields. `title`
/// nests an `Option` so the caller can distinguish *unchanged* (`None`) from
/// *clear* (`Some(None)`) from *set* (`Some(Some(_))`).
#[derive(Debug, Clone, Default)]
pub struct WindowConfigSpec {
    pub make_tiled: bool,
    pub relative: Option<FloatRelative>,
    pub anchor: Option<FloatAnchor>,
    pub row: Option<isize>,
    pub col: Option<isize>,
    pub width: Option<usize>,
    pub height: Option<usize>,
    pub zindex: Option<u32>,
    pub focusable: Option<bool>,
    pub border: Option<BorderStyle>,
    pub title: Option<Option<String>>,
}

/// A window: one viewport onto a buffer. Mirrors the way [`OpenBuffer`] splits
/// state — the buffer binding plus, *while this window is not focused*, its saved
/// cursor/scroll so refocusing restores the view. While the window *is* focused
/// the live position is [`Editor::cursor`] / [`Editor::top`]; the `saved_*`
/// fields are meaningless then (they are stashed only on focus-out, the
/// window analogue of `OpenBuffer::saved_cursor`).
///
/// A window is either **tiled** (`float: None`, a `Node::Leaf` in the layout
/// tree) or **floating** (`float: Some(_)`, an id in [`WindowTree::floats`] that
/// no `Leaf` references — positioned absolutely on top of the tree). The two are
/// otherwise identical: a float still binds a buffer, has a cursor/scroll, and is
/// focusable.
pub(crate) struct Window {
    pub(crate) buffer: BufferId,
    pub(crate) saved_cursor: Cursor,
    pub(crate) saved_top: usize,
    pub(crate) saved_leftcol: usize,
    /// While this window is not focused, the byte offsets of its secondary
    /// (multi-)cursors — the per-window analogue of `saved_cursor`. The live set
    /// is the focused window's `CURSOR_NS` marks; on focus-out they are stashed
    /// here and on focus-in restored, so two windows onto the same buffer carry
    /// independent multi-cursor sets. Meaningless (and empty) while focused.
    pub(crate) saved_cursors: Vec<usize>,
    pub(crate) rect: Rect,
    /// Window-local options (the number gutter). A split inherits these from the
    /// window it was split off, mirroring vim.
    pub(crate) options: WindowOptions,
    /// `Some` for a floating window (its placement), `None` for a tiled one.
    pub(crate) float: Option<FloatConfig>,
}

/// A node in the window layout tree: either a single window (`Leaf`) or a
/// `Split` dividing its area among `children` by `sizes` (relative weights —
/// equal for an even split, leftover cells handed to the first children).
enum Node {
    Leaf(WindowId),
    Split {
        dir: SplitDir,
        children: Vec<Node>,
        sizes: Vec<usize>,
    },
}

impl Node {
    /// Append every leaf window id under this node in layout (left-to-right,
    /// top-to-bottom) order.
    fn collect_leaves(&self, out: &mut Vec<WindowId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { children, .. } => {
                for c in children {
                    c.collect_leaves(out);
                }
            }
        }
    }
}

/// The window layout: every open window keyed by id, the `root` of the split
/// tree arranging them, the `current` (focused) window, and the separators the
/// last [`WindowTree::layout`] produced. Window ids are minted by [`Editor`]
/// (globally unique across tabs), not here. Mirrors [`BufferStore`]; with one
/// window the tree is a single `Leaf`, there are no separators, and `current`
/// always resolves.
pub(crate) struct WindowTree {
    windows: BTreeMap<WindowId, Window>,
    root: Node,
    pub(crate) current: WindowId,
    /// Borders between splits, recomputed on every [`WindowTree::layout`]. Empty
    /// with a single window.
    pub(crate) separators: Vec<Separator>,
    /// Floating window ids — those in `windows` with `float.is_some()`, which no
    /// `Leaf` in `root` references. Kept sorted by `(zindex, id)` so iterating it
    /// yields bottom-to-top paint order. Empty until the first `nvim_open_win`
    /// float.
    pub(crate) floats: Vec<WindowId>,
}

impl WindowTree {
    /// A tree seeded with a single window (id 1) bound to `buffer` and focused,
    /// the window analogue of [`BufferStore::with_one`]. The first tab uses this;
    /// later tabs ([`Editor::new_tab`]) use [`WindowTree::with_window`] with an
    /// [`Editor`]-minted id so window handles stay unique across tabs.
    pub(crate) fn with_one(buffer: BufferId) -> Self {
        WindowTree::with_window(WindowId(1), buffer, WindowOptions::default())
    }

    /// A tree seeded with a single window of a specific (caller-minted) `id`,
    /// bound to `buffer` with `options`. Used to back a freshly created tab page,
    /// whose window id comes from [`Editor::alloc_window_id`] so it never collides
    /// with a window in another tab.
    pub(crate) fn with_window(id: WindowId, buffer: BufferId, options: WindowOptions) -> Self {
        let mut windows = BTreeMap::new();
        windows.insert(
            id,
            Window {
                buffer,
                saved_cursor: Cursor::default(),
                saved_top: 0,
                saved_leftcol: 0,
                saved_cursors: Vec::new(),
                rect: Rect::default(),
                options,
                float: None,
            },
        );
        WindowTree {
            windows,
            root: Node::Leaf(id),
            current: id,
            separators: Vec::new(),
            floats: Vec::new(),
        }
    }

    pub(crate) fn get(&self, id: WindowId) -> &Window {
        self.windows
            .get(&id)
            .expect("current window id is always valid")
    }

    pub(crate) fn get_mut(&mut self, id: WindowId) -> &mut Window {
        self.windows
            .get_mut(&id)
            .expect("current window id is always valid")
    }

    /// The focused window.
    pub(crate) fn cur(&self) -> &Window {
        self.get(self.current)
    }

    /// The focused window, mutably.
    pub(crate) fn cur_mut(&mut self) -> &mut Window {
        let id = self.current;
        self.get_mut(id)
    }

    /// All window ids in layout order (the `nvim_list_wins` order).
    pub(crate) fn leaves(&self) -> Vec<WindowId> {
        let mut out = Vec::new();
        self.root.collect_leaves(&mut out);
        out
    }

    /// How many windows are open. Always ≥ 1.
    pub(crate) fn count(&self) -> usize {
        self.windows.len()
    }

    /// How many *ordinary* (tiled, layout-tree) windows are open — floats
    /// excluded. Always ≥ 1: the editor can never be left holding only floating
    /// windows, so this is the count that gates closing the "last" window and
    /// quitting the editor.
    pub(crate) fn tiled_count(&self) -> usize {
        self.leaves().len()
    }

    /// Re-sort the [float](Self::floats) list bottom-to-top by `(zindex, id)`, so
    /// iterating it is paint order and id breaks a zindex tie by creation order.
    /// Called after a float is added or its zindex changes.
    pub(crate) fn sort_floats(&mut self) {
        let mut keyed: Vec<(u32, u64, WindowId)> = self
            .floats
            .iter()
            .map(|&id| {
                let z = self
                    .windows
                    .get(&id)
                    .and_then(|w| w.float.as_ref())
                    .map_or(0, |f| f.zindex);
                (z, id.0, id)
            })
            .collect();
        keyed.sort_by_key(|&(z, n, _)| (z, n));
        self.floats = keyed.into_iter().map(|(_, _, id)| id).collect();
    }

    /// Assign each window its `rect` by dividing `total` across the tree, and
    /// recompute the [`Separator`]s between splits. A single leaf takes the whole
    /// area; a `Split` subtracts one cell per inter-child border, distributes the
    /// rest by `sizes`, and recurses. After the tiled pass, [floats](Self::floats)
    /// are positioned absolutely on top (`cursor_off` is the focused window's
    /// cursor cell offset from its own rect top-left, for `relative="cursor"`).
    pub(crate) fn layout(&mut self, total: Rect, cursor_off: (usize, usize)) {
        let mut rects: Vec<(WindowId, Rect)> = Vec::new();
        let mut seps: Vec<Separator> = Vec::new();
        layout_node(&mut self.root, total, &mut rects, &mut seps);
        for (id, rect) in rects {
            if let Some(w) = self.windows.get_mut(&id) {
                w.rect = rect;
            }
        }
        self.separators = seps;
        self.position_floats(total, cursor_off);
    }

    /// Position every floating window absolutely on top of the freshly-laid tiled
    /// tree. Runs *after* the tiled pass so a `relative="win"`/`"cursor"` float
    /// reads up-to-date tiled rects. The cursor's absolute cell is the focused
    /// window's rect origin plus `cursor_off`. A no-op when there are no floats,
    /// so a session without floats lays out exactly as before.
    pub(crate) fn position_floats(&mut self, total: Rect, cursor_off: (usize, usize)) {
        if self.floats.is_empty() {
            return;
        }
        // The focused window's rect origin plus the cursor offset. During a
        // focused-window close `current` is briefly the removed window; fall back
        // to the bare offset (the caller re-lays once the survivor is entered).
        let cursor_cell = match self.windows.get(&self.current) {
            Some(w) => (w.rect.x + cursor_off.0, w.rect.y + cursor_off.1),
            None => cursor_off,
        };
        let placements: Vec<(WindowId, Rect)> = self
            .floats
            .iter()
            .filter_map(|&id| {
                let cfg = self.windows.get(&id)?.float.clone()?;
                let origin = match cfg.relative {
                    FloatRelative::Editor => total,
                    FloatRelative::Win(wid) => {
                        self.windows.get(&wid).map(|w| w.rect).unwrap_or(total)
                    }
                    FloatRelative::Cursor => Rect {
                        x: cursor_cell.0,
                        y: cursor_cell.1,
                        width: 0,
                        height: 0,
                    },
                };
                Some((id, place_float(origin, total, &cfg)))
            })
            .collect();
        for (id, rect) in placements {
            if let Some(w) = self.windows.get_mut(&id) {
                w.rect = rect;
            }
        }
    }
}

/// Resolve a float's outer `rect` from its `origin` (the rect its `relative`
/// names) and the `bounds` (the windows area). Applies the `row`/`col` offset
/// from the origin's top-left, shifts so the `anchor` corner lands there (an `E`
/// anchor subtracts the width, an `S` anchor the height — neovim's corner math),
/// then clamps the box to stay fully on-screen.
///
/// `cfg.width`/`cfg.height` are the **inner content** area (neovim's
/// `nvim_open_win` semantics); the border, when present, is drawn *outside* it, so
/// the outer box this rect describes is the content plus one border cell per side.
/// `nvim_win_get_config` keeps reporting the inner dimensions (they are what
/// `FloatConfig` stores) — only the placement grows by the border here. A float
/// larger than `bounds` pins to the top-left rather than shrinking.
fn place_float(origin: Rect, bounds: Rect, cfg: &FloatConfig) -> Rect {
    let border = if cfg.border != BorderStyle::None {
        2
    } else {
        0
    };
    let w = cfg.width.max(1) + border;
    let h = cfg.height.max(1) + border;
    let mut x = origin.x as isize + cfg.col;
    let mut y = origin.y as isize + cfg.row;
    if matches!(cfg.anchor, FloatAnchor::NE | FloatAnchor::SE) {
        x -= w as isize;
    }
    if matches!(cfg.anchor, FloatAnchor::SW | FloatAnchor::SE) {
        y -= h as isize;
    }
    let lo_x = bounds.x as isize;
    let lo_y = bounds.y as isize;
    let hi_x = ((bounds.x + bounds.width).saturating_sub(w) as isize).max(lo_x);
    let hi_y = ((bounds.y + bounds.height).saturating_sub(h) as isize).max(lo_y);
    let x = x.clamp(lo_x, hi_x).max(0) as usize;
    let y = y.clamp(lo_y, hi_y).max(0) as usize;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

/// Split `avail` cells among children weighted by `sizes`, handing any rounding
/// leftover to the first children (vim's behavior). When `sizes` already sum to
/// `avail` this is the identity (so an absolute `:resize` lands exactly). Every
/// child then gets **at least one** cell when there are enough to go around (no
/// zero-extent windows from a lopsided weight like `<C-w>_`'s); only when `avail`
/// is smaller than the child count do trailing children get nothing. Returns one
/// cell count per size; the sum equals `avail`.
fn distribute(avail: usize, sizes: &[usize]) -> Vec<usize> {
    let n = sizes.len();
    if n == 0 {
        return Vec::new();
    }
    let total: usize = sizes.iter().sum::<usize>().max(1);
    let mut out: Vec<usize> = sizes.iter().map(|s| avail * s / total).collect();
    let mut leftover = avail.saturating_sub(out.iter().sum());
    let mut i = 0;
    while leftover > 0 {
        out[i % n] += 1;
        i += 1;
        leftover -= 1;
    }
    // Repair any zero-extent child (a lopsided weight floored it to nothing) by
    // stealing a cell from the current largest, while there's room for one each.
    if avail >= n {
        while let Some(z) = out.iter().position(|&c| c == 0) {
            let max = out
                .iter()
                .enumerate()
                .max_by_key(|(_, &c)| c)
                .map(|(i, _)| i)
                .unwrap_or(0);
            out[max] -= 1;
            out[z] += 1;
        }
    }
    out
}

/// Recursively assign rects to the leaves under `node` within `rect`, collecting
/// the border [`Separator`]s between split children. A `Horizontal` split stacks
/// children top-to-bottom with a `─` row between each (dividing height); a
/// `Vertical` split lays them left-to-right with a `│` column between each
/// (dividing width). One cell is reserved per inter-child border.
///
/// As a side effect each split's `sizes` are rewritten to the cell extents it
/// just assigned, so after a layout `sizes` is always in **cells**: the resize
/// commands (`<C-w>+`/`-`/`<`/`>`/`=`) do plain cell arithmetic on it, and a
/// terminal resize re-runs layout with those cells as weights — `distribute`
/// rescales them proportionally, preserving each split's relative shares.
fn layout_node(
    node: &mut Node,
    rect: Rect,
    rects: &mut Vec<(WindowId, Rect)>,
    seps: &mut Vec<Separator>,
) {
    match node {
        Node::Leaf(id) => rects.push((*id, rect)),
        Node::Split {
            dir,
            children,
            sizes,
        } => {
            let n = children.len();
            let borders = n.saturating_sub(1);
            match dir {
                SplitDir::Horizontal => {
                    let avail = rect.height.saturating_sub(borders);
                    let heights = distribute(avail, sizes);
                    *sizes = heights.clone();
                    let mut y = rect.y;
                    for (i, (child, h)) in children.iter_mut().zip(heights).enumerate() {
                        let child_rect = Rect {
                            x: rect.x,
                            y,
                            width: rect.width,
                            height: h,
                        };
                        layout_node(child, child_rect, rects, seps);
                        y += h;
                        if i + 1 < n {
                            seps.push(Separator {
                                vertical: false,
                                x: rect.x,
                                y,
                                length: rect.width,
                            });
                            y += 1;
                        }
                    }
                }
                SplitDir::Vertical => {
                    let avail = rect.width.saturating_sub(borders);
                    let widths = distribute(avail, sizes);
                    *sizes = widths.clone();
                    let mut x = rect.x;
                    for (i, (child, w)) in children.iter_mut().zip(widths).enumerate() {
                        let child_rect = Rect {
                            x,
                            y: rect.y,
                            width: w,
                            height: rect.height,
                        };
                        layout_node(child, child_rect, rects, seps);
                        x += w;
                        if i + 1 < n {
                            seps.push(Separator {
                                vertical: true,
                                x,
                                y: rect.y,
                                length: rect.height,
                            });
                            x += 1;
                        }
                    }
                }
            }
        }
    }
}

/// Replace the `target` leaf in the tree with a `Split` of two leaves — the new
/// window first (the top/left, which takes focus) and the old window second.
/// Returns `false` if `target` is not in the tree. The sizes start equal, so the
/// split is even (vim's default); manual resize is a later phase.
fn split_leaf(node: &mut Node, target: WindowId, dir: SplitDir, new_id: WindowId) -> bool {
    match node {
        Node::Leaf(id) if *id == target => {
            *node = Node::Split {
                dir,
                children: vec![Node::Leaf(new_id), Node::Leaf(target)],
                sizes: vec![1, 1],
            };
            true
        }
        Node::Leaf(_) => false,
        Node::Split { children, .. } => children
            .iter_mut()
            .any(|c| split_leaf(c, target, dir, new_id)),
    }
}

/// Remove the `target` leaf from the tree, collapsing a `Split` that is left
/// with a single child into that child. Returns `false` if `target` is not
/// found. Only ever called when more than one window is open, so the root is a
/// `Split` and the last window is never removed.
fn remove_leaf(node: &mut Node, target: WindowId) -> bool {
    let Node::Split {
        children, sizes, ..
    } = node
    else {
        return false;
    };
    let removed = if let Some(pos) = children
        .iter()
        .position(|c| matches!(c, Node::Leaf(id) if *id == target))
    {
        children.remove(pos);
        sizes.remove(pos);
        true
    } else {
        children.iter_mut().any(|c| remove_leaf(c, target))
    };
    if removed && children.len() == 1 {
        *node = children.remove(0);
    }
    removed
}

/// The smallest extent (cells) a window may be shrunk to along a split axis, so
/// resizing a neighbor never collapses it: one text row plus its status line for
/// a horizontal split, one column for a vertical one. A leaf at this size still
/// renders (its text height floors to 1).
fn min_extent(dir: SplitDir) -> usize {
    match dir {
        SplitDir::Horizontal => 2,
        SplitDir::Vertical => 1,
    }
}

/// Grow `sizes[i]` by `delta` cells (shrink it when `delta` is negative),
/// taking the opposite amount from a neighbor so the split's total extent is
/// unchanged — exactly vim's "steal from the window below/right" resize. The
/// neighbor is the next sibling, or the previous one when `i` is last. Both the
/// resized child and its neighbor stay at or above [`min_extent`].
fn adjust_sizes(sizes: &mut [usize], i: usize, delta: isize, dir: SplitDir) {
    let n = sizes.len();
    if n < 2 || delta == 0 {
        return;
    }
    let j = if i + 1 < n { i + 1 } else { i - 1 };
    let min = min_extent(dir);
    if delta > 0 {
        // Grow `i` by as much as the neighbor can spare above the minimum.
        let give = (delta as usize).min(sizes[j].saturating_sub(min));
        sizes[i] += give;
        sizes[j] -= give;
    } else {
        // Shrink `i` no further than its own minimum; the neighbor takes it.
        let take = ((-delta) as usize).min(sizes[i].saturating_sub(min));
        sizes[i] -= take;
        sizes[j] += take;
    }
}

/// Find leaf `target` under `node` and, at the **nearest** ancestor split whose
/// orientation is `axis`, resize the child on the path toward it by `delta`
/// cells (via [`adjust_sizes`]). Returns whether `target` is in this subtree;
/// `done` guards so only that nearest split is touched. A no-op if there is no
/// ancestor split of that orientation (e.g. resizing height with only vertical
/// splits above).
fn resize_toward(
    node: &mut Node,
    target: WindowId,
    axis: SplitDir,
    delta: isize,
    done: &mut bool,
) -> bool {
    match node {
        Node::Leaf(id) => *id == target,
        Node::Split {
            dir,
            children,
            sizes,
        } => {
            let mut on_path = None;
            for (i, child) in children.iter_mut().enumerate() {
                if resize_toward(child, target, axis, delta, done) {
                    on_path = Some(i);
                    break;
                }
            }
            let Some(i) = on_path else {
                return false;
            };
            if !*done && *dir == axis {
                adjust_sizes(sizes, i, delta, axis);
                *done = true;
            }
            true
        }
    }
}

/// Maximize leaf `target`'s extent along `axis`: at **every** ancestor split of
/// that orientation, give the child on the path the lion's share and the rest a
/// single weight, so the window fills the dimension across nested splits (vim's
/// `<C-w>_` / `<C-w>|`). Sizes are weights here; the next layout renormalizes
/// them to cells, clamping siblings to one cell each. Returns whether `target`
/// is in this subtree.
fn maximize_toward(node: &mut Node, target: WindowId, axis: SplitDir) -> bool {
    match node {
        Node::Leaf(id) => *id == target,
        Node::Split {
            dir,
            children,
            sizes,
        } => {
            let mut on_path = None;
            for (i, child) in children.iter_mut().enumerate() {
                if maximize_toward(child, target, axis) {
                    on_path = Some(i);
                    break;
                }
            }
            let Some(i) = on_path else {
                return false;
            };
            if *dir == axis {
                for s in sizes.iter_mut() {
                    *s = 1;
                }
                // A weight large enough to dominate any realistic terminal, so
                // `distribute` hands this child everything but the siblings'
                // reserved one cell.
                sizes[i] = 10_000;
            }
            true
        }
    }
}

/// Reset every split's `sizes` to equal weights (vim's `<C-w>=`); the next
/// layout renders even shares.
fn equalize_node(node: &mut Node) {
    if let Node::Split {
        children, sizes, ..
    } = node
    {
        for s in sizes.iter_mut() {
            *s = 1;
        }
        for child in children.iter_mut() {
            equalize_node(child);
        }
    }
}

/// Per-window data for the [`View`] projection: the window's buffer, the view
/// position to render it at (live for the focused window, stashed otherwise),
/// its rect, whether it holds focus, and — for a float — the overlay chrome
/// (`floating`/`border`/`title`) the client paints it with. Tiled windows carry
/// `floating: false`, `border: None`, `title: None`.
pub(crate) struct WindowLayout {
    pub(crate) buffer: BufferId,
    pub(crate) cursor: Cursor,
    pub(crate) top: usize,
    /// First visible screen column (horizontal scroll offset). `0` unless a long
    /// line under `nowrap` has scrolled the viewport right.
    pub(crate) leftcol: usize,
    pub(crate) rect: Rect,
    pub(crate) focused: bool,
    /// This window's window-local options (the number gutter), so the projection
    /// renders each window's own gutter rather than a single global one.
    pub(crate) options: WindowOptions,
    /// Whether this window is a float (drawn on top of the tiled layout at its
    /// absolute `rect`). The client paints floats in a second, on-top pass.
    pub(crate) floating: bool,
    /// The float's border style (`None` for a tiled window or a borderless
    /// float). A bordered float's content area is its `rect` inset by one cell.
    pub(crate) border: BorderStyle,
    /// The float's title, drawn on the top border. `None` when untitled.
    pub(crate) title: Option<String>,
}

/// One tab page's label data for the [`View`] tabline: the file name of the
/// tab's focused window's buffer (`[No Name]` when unset), whether that buffer is
/// modified, and how many windows the tab holds. Mirrors vim's default tabline
/// label (`{win_count}{+ if modified} {name}`); the client formats the cells.
pub(crate) struct TabLabel {
    pub(crate) name: String,
    pub(crate) modified: bool,
    pub(crate) window_count: usize,
}

/// A recorded scroll gesture (`<C-d>` / `<C-u>` / `<C-f>` / `<C-b>`) that moved
/// the viewport, handed to the client so it can animate the slide. Lines/columns
/// are absolute buffer lines; `duration_ms` is a suggested pacing the client may
/// clamp or ignore.
#[derive(Clone, Copy)]
pub(crate) struct PendingScroll {
    pub from_top: usize,
    pub to_top: usize,
    pub from_cursor: usize,
    pub to_cursor: usize,
    pub duration_ms: u64,
}

impl Editor {
    /// The focused window's id (the `nvim_get_current_win` target).
    pub fn current_window_id(&self) -> WindowId {
        self.windows.current
    }

    /// All window ids (the `nvim_list_wins` order): the tiled windows in layout
    /// order first, then the floats bottom-to-top by `(zindex, id)`.
    pub fn window_ids(&self) -> Vec<WindowId> {
        let mut ids = self.windows.leaves();
        ids.extend(self.windows.floats.iter().copied());
        ids
    }

    /// Number of open windows (always ≥ 1).
    pub fn window_count(&self) -> usize {
        self.windows.count()
    }

    /// The id the next [`Editor::open_split_window`] / split will mint, without
    /// allocating it. The Lua bridge pushes this into its mirror so
    /// `nvim_open_win` can return the new handle synchronously (the real window
    /// is created when the queued op drains).
    pub fn next_window_id(&self) -> WindowId {
        WindowId(self.next_win_id)
    }

    /// Mint a fresh, globally-unique window id (the single source of window
    /// handles; every split, float, and new-tab window draws from here).
    pub(crate) fn alloc_window_id(&mut self) -> WindowId {
        let id = WindowId(self.next_win_id);
        self.next_win_id += 1;
        id
    }

    /// Make window `id` the focused one (`nvim_set_current_win`). A no-op if `id`
    /// is not open or already current.
    pub fn set_current_window(&mut self, id: WindowId) {
        self.focus_window(id);
    }

    /// The buffer window `id` shows (`nvim_win_get_buf`), or `None` if there is
    /// no such window.
    pub fn window_buffer(&self, id: WindowId) -> Option<BufferId> {
        self.windows.windows.get(&id).map(|w| w.buffer)
    }

    /// The first `(tab index, window id)` whose tiled window shows `buf`,
    /// scanning tabs in tabline order and windows in layout order. The active
    /// tab reads its live tree; inactive tabs read their stashed one. Backs
    /// `:drop` / `:tab drop`'s "already open somewhere?" check. Floats are
    /// excluded — `:drop` targets editable windows, as in vim.
    pub(crate) fn window_showing(&self, buf: BufferId) -> Option<(usize, WindowId)> {
        for (idx, tab) in self.tabs.iter().enumerate() {
            let tree = if idx == self.current_tab {
                &self.windows
            } else {
                tab.tree
                    .as_ref()
                    .expect("an inactive tab always holds its stashed layout")
            };
            for win in tree.leaves() {
                if tree.windows.get(&win).map(|w| w.buffer) == Some(buf) {
                    return Some((idx, win));
                }
            }
        }
        None
    }

    /// Rebind window `id` to show buffer `buf` *without* changing focus
    /// (`nvim_win_set_buf`). The focused window swaps its live buffer (like `:b`);
    /// an inactive window updates its binding and clamps its stashed cursor to the
    /// new buffer. A no-op if either handle is unknown.
    pub fn set_window_buffer(&mut self, id: WindowId, buf: BufferId) {
        if !self.windows.windows.contains_key(&id) || !self.buffers.map.contains_key(&buf) {
            return;
        }
        if id == self.windows.current {
            self.switch_buffer(buf);
            return;
        }
        let lines = self.buffers.get(buf).buffer.line_count();
        let w = self.windows.get_mut(id);
        w.buffer = buf;
        if w.saved_cursor.line >= lines {
            w.saved_cursor.line = lines.saturating_sub(1);
            w.saved_cursor.col = 0;
        }
    }

    /// Window `id`'s window-local options (the number gutter), or `None` for an
    /// unknown id. The server snapshots these into the `vim.wo` mirror.
    pub fn window_options(&self, id: WindowId) -> Option<WindowOptions> {
        self.windows.windows.get(&id).map(|w| w.options)
    }

    /// Set a boolean window-local option on window `id` (`vim.wo` /
    /// `nvim_win_set_option`). Recognizes `number` / `relativenumber`; a no-op for
    /// any other name or an unknown id. `0` is resolved to the focused window by
    /// the caller.
    pub fn set_window_option_bool(&mut self, id: WindowId, name: &str, value: bool) {
        let Some(w) = self.windows.windows.get_mut(&id) else {
            return;
        };
        match name {
            "number" => w.options.number = value,
            "relativenumber" => w.options.relativenumber = value,
            _ => {}
        }
    }

    /// Set a boolean global option from outside the editor (the Lua `vim.o`
    /// bridge), the global analogue of [`Editor::set_window_option_bool`]. The
    /// wired global options are all the search booleans; an unknown name is a
    /// no-op (the Lua side only forwards the canonical wired set). This is the same
    /// state the `:set` ex path writes — the two routes share one home.
    pub fn set_global_option_bool(&mut self, name: &str, value: bool) {
        match name {
            "ignorecase" => self.options.ignorecase = value,
            "smartcase" => self.options.smartcase = value,
            "wrapscan" => self.options.wrapscan = value,
            "hlsearch" => self.options.hlsearch = value,
            "incsearch" => self.options.incsearch = value,
            "autoread" => self.options.autoread = value,
            _ => {}
        }
    }

    /// Set a numeric global option from outside the editor, the numeric analogue
    /// of [`Editor::set_global_option_bool`] and the shared home for both the `:set
    /// {opt}=…` ex path ([`Editor::apply_set_num`]) and the Lua `vim.o` bridge — so
    /// the two routes validate, echo, and relayout identically. The wired numeric
    /// globals are `showtabline` (0/1/2) and `laststatus` (0/1/2/3): an
    /// out-of-range value is rejected loudly (vim's `E487` below the range, `E474`
    /// above it), and a valid change re-lays the windows area, since it grows or
    /// shrinks the reserved tabline / global-statusline row. An unknown name is a
    /// no-op (the Lua side forwards only the canonical wired set).
    pub fn set_global_option_num(&mut self, name: &str, value: i64) {
        // `mousetime` is an unbounded non-negative millisecond count — it doesn't
        // share `showtabline`/`laststatus`'s small-range, relayout-on-change shape,
        // so handle it before the bounded block.
        if name == "mousetime" {
            if value < 0 {
                self.echo(format!("E487: Argument must be positive: {name}={value}"));
                return;
            }
            self.options.mousetime = value as usize;
            return;
        }
        let max = match name {
            "showtabline" => 2,
            "laststatus" => 3,
            _ => return,
        };
        if value < 0 {
            self.echo(format!("E487: Argument must be positive: {name}={value}"));
            return;
        }
        if value > max {
            self.echo(format!("E474: Invalid argument: {name}={value}"));
            return;
        }
        match name {
            "showtabline" => self.options.showtabline = value as u8,
            "laststatus" => self.options.laststatus = value as u8,
            _ => unreachable!("validated above"),
        }
        self.relayout();
        self.ensure_visible();
    }

    /// Set a string global option from outside the editor (the Lua `vim.o`
    /// bridge), the string analogue of [`Editor::set_global_option_bool`]. The
    /// wired string globals are `statusline`, `tabline`, `guifont`, the mouse
    /// strings (`mouse`/`mousemodel`/`mousescroll`), and `regexsyntax`; an unknown
    /// name is a no-op (the Lua side forwards only the canonical wired set). This is
    /// the same state the `:set statusline=…` / `:set guifont=…` ex path writes —
    /// the two routes share one home. (The `:set` path additionally *validates*
    /// `regexsyntax`; a raw `vim.o` write of a bad value reads back as `pcre`.)
    pub fn set_global_option_str(&mut self, name: &str, value: &str) {
        match name {
            "statusline" => self.options.statusline = value.to_string(),
            "tabline" => self.options.tabline = value.to_string(),
            "guifont" => self.options.guifont = value.to_string(),
            "mouse" => self.options.mouse = value.to_string(),
            "mousemodel" => self.options.mousemodel = value.to_string(),
            "mousescroll" => self.options.mousescroll = value.to_string(),
            "regexsyntax" => self.options.regexsyntax = value.to_string(),
            _ => {}
        }
    }

    /// The editor's global options, for the server to mirror to Lua (`vim.o`).
    pub fn global_options(&self) -> Options {
        self.options.clone()
    }

    /// Window `id`'s cursor as `(0-based line, byte col)` — the live cursor for
    /// the focused window, the stashed one otherwise (`nvim_win_get_cursor`).
    /// `None` if there is no such window.
    pub fn window_cursor(&self, id: WindowId) -> Option<(usize, usize)> {
        let w = self.windows.windows.get(&id)?;
        let c = if id == self.windows.current {
            self.cursor
        } else {
            w.saved_cursor
        };
        Some((c.line, c.col))
    }

    /// Move window `id`'s cursor to `(0-based line, byte col)` (`nvim_win_set_cursor`).
    /// The focused window moves its live cursor (clamped, view kept visible); an
    /// inactive window updates its stashed position (clamped to its buffer's line
    /// count; the column is re-clamped when the window is next focused). A no-op
    /// for an unknown id.
    pub fn set_window_cursor(&mut self, id: WindowId, line: usize, col: usize) {
        if !self.windows.windows.contains_key(&id) {
            return;
        }
        if id == self.windows.current {
            self.cursor.line = line;
            self.cursor.col = col;
            self.clamp_cursor();
            self.desired_col = self.cursor_virtcol();
            self.ensure_visible();
            return;
        }
        let buf = self.windows.get(id).buffer;
        let lines = self.buffers.get(buf).buffer.line_count();
        let w = self.windows.get_mut(id);
        w.saved_cursor.line = line.min(lines.saturating_sub(1));
        w.saved_cursor.col = col;
    }

    /// Window `id`'s scroll offset as `(top, leftcol)` — the first visible buffer
    /// line (0-based) and the first visible screen column. The focused window
    /// reports its live offset; an inactive window its stashed one. `None` for an
    /// unknown id. Backs `vim.fn.winsaveview`'s `topline`/`leftcol`.
    pub fn window_scroll(&self, id: WindowId) -> Option<(usize, usize)> {
        let w = self.windows.windows.get(&id)?;
        Some(if id == self.windows.current {
            (self.top, self.leftcol)
        } else {
            (w.saved_top, w.saved_leftcol)
        })
    }

    /// Window `id`'s text offset — the number-gutter width, the columns before the
    /// first text cell. `None` for an unknown id. Feeds the server's screen-column
    /// math for `vim.fn.screencol`.
    pub fn window_textoff(&self, id: WindowId) -> Option<usize> {
        let w = self.windows.windows.get(&id)?;
        let lines = self.buffers.get(w.buffer).buffer.line_count();
        Some(self.number_width_for(w.options, lines))
    }

    /// Scroll window `id` so its first visible line is `top` (0-based), clamped to
    /// the buffer's last line. The focused window moves its live viewport; an
    /// inactive window updates its stashed `top` (applied when next focused). A
    /// no-op for an unknown id. Backs `vim.fn.winrestview`'s `topline`.
    pub fn set_window_topline(&mut self, id: WindowId, top: usize) {
        let Some(w) = self.windows.windows.get(&id) else {
            return;
        };
        let last = self
            .buffers
            .get(w.buffer)
            .buffer
            .line_count()
            .saturating_sub(1);
        let top = top.min(last);
        if id == self.windows.current {
            self.top = top;
        } else {
            self.windows.get_mut(id).saved_top = top;
        }
    }

    /// Window `id`'s rect as `(x, y, width, height)` in windows-area cells, or
    /// `None` if there is no such window. `height` includes the status-line row;
    /// the API width/height the server returns derive from this.
    pub fn window_rect(&self, id: WindowId) -> Option<(usize, usize, usize, usize)> {
        self.windows
            .windows
            .get(&id)
            .map(|w| (w.rect.x, w.rect.y, w.rect.width, w.rect.height))
    }

    /// Window `id`'s **content** size as `(width, height)` — what
    /// `nvim_win_get_width` / `nvim_win_get_height` report. The width includes the
    /// number gutter (as neovim's does) but excludes a bordered float's side
    /// columns; the height excludes a bordered float's border rows and the status
    /// row when one is shown. Mirrors the [`crate::view::window_view`] /
    /// [`Editor::text_height`] content math so the API agrees with what is drawn.
    pub fn window_content_size(&self, id: WindowId) -> Option<(usize, usize)> {
        let w = self.windows.windows.get(&id)?;
        let inset = matches!(&w.float, Some(cfg) if cfg.border != BorderStyle::None) as usize;
        let status = usize::from(self.window_statusline_visible(w.float.is_some()));
        let width = w.rect.width.saturating_sub(2 * inset);
        let height = w
            .rect
            .height
            .saturating_sub(2 * inset)
            .saturating_sub(status);
        Some((width, height))
    }

    /// Set window `id`'s width to `width` columns (`nvim_win_set_width`), stealing
    /// from / yielding to a sibling. A no-op with one window or an unknown id.
    pub fn set_window_width(&mut self, id: WindowId, width: usize) {
        let Some((_, _, w, _)) = self.window_rect(id) else {
            return;
        };
        self.resize_window_id(id, SplitDir::Vertical, width as isize - w as isize);
    }

    /// Set window `id`'s text height to `height` rows (`nvim_win_set_height`). The
    /// rect carries the status line, so the target rect height is `height + 1`. A
    /// no-op with one window or an unknown id.
    pub fn set_window_height(&mut self, id: WindowId, height: usize) {
        let Some((_, _, _, h)) = self.window_rect(id) else {
            return;
        };
        self.resize_window_id(id, SplitDir::Horizontal, (height + 1) as isize - h as isize);
    }

    /// Close window `id` (`nvim_win_close`). Returns `false` if it is the last
    /// window (which can't be closed). `force` is accepted for API compatibility;
    /// closing a window in nxvim never unloads its buffer (the buffer stays in the
    /// store), so there is nothing to force.
    pub fn close_window_by_id(&mut self, id: WindowId, _force: bool) -> bool {
        self.remove_window(id)
    }

    /// `nvim_open_win` (split form) — split the focused window and bind the new,
    /// now-focused window to `buf`. Returns the new window's id. `vertical`
    /// chooses a `:vsplit` over a `:split`.
    pub fn open_split_window(&mut self, buf: BufferId, vertical: bool) -> WindowId {
        let dir = if vertical {
            SplitDir::Vertical
        } else {
            SplitDir::Horizontal
        };
        self.split(dir);
        self.switch_buffer(buf);
        self.windows.current
    }

    /// `nvim_open_win` (float form) — create a **floating** window bound to `buf`,
    /// positioned by `config`, and (when `enter`) focus it. Unlike
    /// [`Editor::open_split_window`] this does **not** touch the layout tree: the
    /// new window is added to the float list and positioned absolutely on top, so
    /// the tiled windows keep their rects. Returns the new window's id.
    pub fn open_float_window(
        &mut self,
        buf: BufferId,
        config: FloatConfig,
        enter: bool,
    ) -> WindowId {
        // A float defaults to a clean gutter — no line-number column — so popup
        // content (diagnostics, hover, completion docs, plugin UIs) fills the
        // window width instead of being squeezed/truncated by an inherited gutter.
        // This matches how floats read in neovim; a caller that wants numbers in a
        // floating editor re-enables them with `nvim_win_set_option(win, "number")`.
        // The horizontal-scroll settings still come from the focused window.
        let mut options = self.windows.get(self.windows.current).options;
        options.number = false;
        options.relativenumber = false;
        let id = self.alloc_window_id();
        self.windows.windows.insert(
            id,
            Window {
                buffer: buf,
                saved_cursor: Cursor::default(),
                saved_top: 0,
                saved_leftcol: 0,
                saved_cursors: Vec::new(),
                rect: Rect::default(),
                options,
                float: Some(config),
            },
        );
        self.windows.floats.push(id);
        self.windows.sort_floats();
        if enter {
            // Focus the float like any window switch (stashes the outgoing view,
            // binds the float's buffer). `relayout` then positions it.
            self.focus_window(id);
        }
        self.relayout();
        if enter {
            self.ensure_visible();
        }
        id
    }

    /// The float placement of window `id` (`nvim_win_get_config`), or `None` if
    /// `id` is a tiled window or not open. The server formats it into neovim's
    /// config map (`{ relative = "" }` for a tiled window).
    pub fn window_float_config(&self, id: WindowId) -> Option<FloatConfig> {
        self.windows.windows.get(&id).and_then(|w| w.float.clone())
    }

    /// `nvim_win_set_config(win, config)` — reconfigure window `id` from a partial
    /// [`WindowConfigSpec`]. Three behaviors, selected by the spec and the
    /// window's current kind:
    ///
    /// - **move / resize / restyle a float:** merge the `Some` fields over the
    ///   window's current [`FloatConfig`] and re-lay; absent fields are unchanged.
    /// - **tiled → float** (`spec.relative` is `Some`, window is currently tiled):
    ///   the window leaves the layout tree (its split collapses, a neighbor fills
    ///   the freed area) and joins the float list with the merged config. Refused
    ///   for the **last** tiled window (neovim won't float the only normal window).
    /// - **float → tiled** (`spec.make_tiled`, the `relative = ""` form): the float
    ///   re-tiles as a horizontal split of the focused window.
    ///
    /// A no-op for an unknown id, or `make_tiled` on an already-tiled window.
    pub fn set_window_config(&mut self, id: WindowId, spec: WindowConfigSpec) {
        if !self.windows.windows.contains_key(&id) {
            return;
        }
        // `relative = ""` re-tiles a float; on a tiled window it is a no-op.
        if spec.make_tiled {
            self.convert_float_to_tiled(id);
            return;
        }
        let was_tiled = self.windows.get(id).float.is_none();
        if was_tiled && self.windows.leaves().len() <= 1 {
            // The only tiled window can't become a float — there must always be a
            // normal window left for the tiled layout to fill.
            self.echo("nvim_win_set_config: cannot make the last window floating");
            return;
        }
        // Base config: the window's live placement, or a default seeded from its
        // current tiled rect (so a tiled → float conversion that omits width/height
        // keeps its on-screen size).
        let mut cfg = match self.windows.get(id).float.clone() {
            Some(c) => c,
            None => {
                let (_, _, w, h) = self.window_rect(id).unwrap_or((0, 0, 1, 1));
                FloatConfig {
                    width: w.max(1),
                    height: h.max(1),
                    ..FloatConfig::default()
                }
            }
        };
        if let Some(v) = spec.relative {
            cfg.relative = v;
        }
        if let Some(v) = spec.anchor {
            cfg.anchor = v;
        }
        if let Some(v) = spec.row {
            cfg.row = v;
        }
        if let Some(v) = spec.col {
            cfg.col = v;
        }
        if let Some(v) = spec.width {
            cfg.width = v.max(1);
        }
        if let Some(v) = spec.height {
            cfg.height = v.max(1);
        }
        if let Some(v) = spec.zindex {
            cfg.zindex = v;
        }
        if let Some(v) = spec.focusable {
            cfg.focusable = v;
        }
        if let Some(v) = spec.border {
            cfg.border = v;
        }
        if let Some(v) = spec.title {
            cfg.title = v;
        }
        if was_tiled {
            // Detach the window from the tiled tree (a sibling expands) but keep
            // the window itself; it becomes a float.
            remove_leaf(&mut self.windows.root, id);
            self.windows.floats.push(id);
        }
        self.windows.get_mut(id).float = Some(cfg);
        self.windows.sort_floats();
        self.relayout();
        if self.windows.current == id {
            self.ensure_visible();
        }
    }

    /// Convert float `id` back into a tiled window (the `relative = ""` form of
    /// `nvim_win_set_config`): drop it from the float list, clear its
    /// [`FloatConfig`], and re-insert it into the layout tree as a horizontal
    /// split of the focused window (neovim's "make it a normal window" lands it in
    /// the current layout). A no-op if `id` is already tiled.
    fn convert_float_to_tiled(&mut self, id: WindowId) {
        if self.windows.get(id).float.is_none() {
            return;
        }
        self.windows.floats.retain(|f| *f != id);
        self.windows.get_mut(id).float = None;
        // Split a tiled neighbor to make room. When the float itself is focused,
        // split the first tiled leaf (there is always at least one, since a float
        // is never the only window); otherwise split the focused window.
        let target = if self.windows.current == id {
            self.windows
                .leaves()
                .into_iter()
                .next()
                .expect("a tiled window always exists alongside a float")
        } else {
            self.windows.current
        };
        split_leaf(&mut self.windows.root, target, SplitDir::Horizontal, id);
        self.relayout();
        self.ensure_visible();
    }

    /// Per-window data the [`View`] projection needs: each window's buffer, its
    /// view position (the *live* `cursor`/`top` for the focused window, the
    /// stashed `saved_*` for the rest), its computed rect, and whether it holds
    /// focus. In layout order.
    pub(crate) fn window_layouts(&self) -> Vec<WindowLayout> {
        let cur = self.windows.current;
        let layout_of = |id: WindowId| {
            let w = self.windows.get(id);
            let focused = id == cur;
            let (floating, border, title) = match &w.float {
                Some(cfg) => (true, cfg.border, cfg.title.clone()),
                None => (false, BorderStyle::None, None),
            };
            WindowLayout {
                buffer: w.buffer,
                cursor: if focused { self.cursor } else { w.saved_cursor },
                top: if focused { self.top } else { w.saved_top },
                leftcol: if focused {
                    self.leftcol
                } else {
                    w.saved_leftcol
                },
                rect: w.rect,
                focused,
                options: w.options,
                floating,
                border,
                title,
            }
        };
        // Tiled windows in tree order first, then the floats bottom-to-top by
        // `(zindex, id)` — the same order `window_ids`/`nvim_list_wins` uses, so
        // the client paints floats on top in z-order.
        self.windows
            .leaves()
            .into_iter()
            .chain(self.windows.floats.iter().copied())
            .map(layout_of)
            .collect()
    }

    /// The split borders the last layout produced (empty with one window).
    pub(crate) fn separators(&self) -> &[Separator] {
        &self.windows.separators
    }

    /// Split the focused window in `dir`: the new window is a clone of the
    /// current (same buffer, copied cursor/scroll) and takes focus, landing in
    /// the new top/left window as vim's `:split` / `:vsplit` do. Both windows
    /// then show the same view position; the old one keeps it via its stash.
    pub(crate) fn split(&mut self, dir: SplitDir) {
        let cursor = self.cursor;
        let top = self.top;
        let leftcol = self.leftcol;
        let cur = self.windows.current;
        // Stash the live position into the outgoing window so the old (bottom /
        // right) sibling keeps its view; seed the new window from the same spot.
        // Clone the live secondary multi-cursors into the outgoing window's stash
        // so refocusing it restores its own copy; the live `CURSOR_NS` marks stay
        // in place for the new (focused) clone, which keeps the same view.
        let secondaries = self.secondary_cursor_bytes();
        {
            let w = self.windows.get_mut(cur);
            w.saved_cursor = cursor;
            w.saved_top = top;
            w.saved_leftcol = leftcol;
            w.saved_cursors = secondaries;
        }
        let buffer = self.windows.get(cur).buffer;
        // A split inherits the source window's window-local options, as vim does.
        let options = self.windows.get(cur).options;
        let new_id = self.alloc_window_id();
        self.windows.windows.insert(
            new_id,
            Window {
                buffer,
                saved_cursor: cursor,
                saved_top: top,
                saved_leftcol: leftcol,
                saved_cursors: Vec::new(),
                rect: Rect::default(),
                options,
                float: None,
            },
        );
        split_leaf(&mut self.windows.root, cur, dir, new_id);
        self.windows.current = new_id;
        // The new window shows the same buffer at the same position, so the live
        // `cursor`/`top` already describe it — only the viewport shrank.
        self.relayout();
        self.ensure_visible();
    }

    /// `<C-w>c` / `:close` — close the focused window and expand a neighbor to
    /// fill the freed area. On the last *tiled* window it closes any open floats
    /// instead (the editor can't be left showing only floats); with none open it
    /// refuses (vim's `E444`). The quit-when-last semantics belong to `:q`.
    pub(crate) fn close_window(&mut self) {
        let cur = self.windows.current;
        if !self.remove_window(cur) {
            self.echo("E444: Cannot close last window");
        }
    }

    /// Close every floating window, dropping each from the window map *and* the
    /// float list. Used where the editor would otherwise be left holding only
    /// floats — which can't stand alone — so neovim closes the floats instead of
    /// the ordinary window: `:only`, and `:close` / `<C-w>c` on the last tiled
    /// window. Clearing the list (not just the map) is essential: a leftover id
    /// there panics the next redraw, which looks every float up by id.
    fn close_all_floats(&mut self) {
        for id in std::mem::take(&mut self.windows.floats) {
            self.windows.windows.remove(&id);
        }
    }

    /// Remove window `id` (focused or not) and collapse its parent split. When
    /// the closed window held focus, the spatially nearest survivor takes it (and
    /// its view position is restored); otherwise focus is untouched. Floats
    /// anchored to the closing window (`relative="win"`) close with it. Closing
    /// the *last tiled* window instead closes any open floats and keeps this
    /// window (neovim's behavior — the editor can't be left showing only floats).
    /// Returns `false` — without touching the layout — only when `id` is the
    /// genuine last window (the last tiled one with no floats to close) or not
    /// open. Shared by `<C-w>c`/`:close` and the `nvim_win_close` API.
    pub(crate) fn remove_window(&mut self, id: WindowId) -> bool {
        if !self.windows.windows.contains_key(&id) {
            return false;
        }
        // The last tiled window can't be closed — a float never substitutes for it
        // (the tiled layout must always have a normal window to fill). But the
        // editor also can't be left showing only floats, so vim closes the floats
        // instead (keeping this window); with none open it is the genuine last
        // window. Gating on the *total* count instead would let an unfocused float
        // fool this into closing the last tiled window, stranding focus on a
        // deleted id.
        let is_float = self.windows.get(id).float.is_some();
        if !is_float && self.windows.tiled_count() <= 1 {
            if self.windows.floats.is_empty() {
                return false;
            }
            self.close_all_floats();
            self.relayout();
            self.ensure_visible();
            return true;
        }
        let closing_rect = self.windows.get(id).rect;
        // `id` plus every float transitively anchored to it (`relative="win"`):
        // neovim closes a float when its parent window goes away.
        let mut victims = vec![id];
        let mut i = 0;
        while i < victims.len() {
            let parent = victims[i];
            let children: Vec<WindowId> = self
                .windows
                .floats
                .iter()
                .copied()
                .filter(|f| !victims.contains(f))
                .filter(|f| {
                    matches!(
                        self.windows.get(*f).float.as_ref().map(|c| c.relative),
                        Some(FloatRelative::Win(p)) if p == parent
                    )
                })
                .collect();
            victims.extend(children);
            i += 1;
        }
        let was_focused = victims.contains(&self.windows.current);
        // A float is not in the layout tree — drop it from the float list; a
        // tiled window collapses its parent split.
        for &v in &victims {
            if self.windows.get(v).float.is_some() {
                self.windows.floats.retain(|f| *f != v);
            } else {
                remove_leaf(&mut self.windows.root, v);
            }
            self.windows.windows.remove(&v);
        }
        if was_focused {
            // Pick the spatially nearest survivor (using the pre-relayout rects)
            // as the new focus, then re-lay and restore its view.
            let new_cur = self.nearest_window(closing_rect);
            self.relayout();
            self.enter_window(new_cur);
            // `current` was the removed window during the relayout above, so any
            // cursor-relative floats were placed against a fallback origin —
            // re-lay now that the survivor is focused.
            if !self.windows.floats.is_empty() {
                self.relayout();
            }
        } else {
            self.relayout();
            self.ensure_visible();
        }
        true
    }

    /// `<C-w>o` / `:only` — drop every other tiled window *and every float*
    /// (neovim's `:only` closes floats too); the focused window expands to the
    /// whole area. A no-op when it is already the only window. The kept window
    /// stays focused, so the live view position is untouched. Refused from a
    /// focused float (only a float would remain — neovim's E5601).
    pub(crate) fn only_window(&mut self) {
        if self.windows.get(self.windows.current).float.is_some() {
            self.echo("E5601: Cannot close window, only floating window would remain");
            return;
        }
        if self.windows.leaves().len() <= 1 && self.windows.floats.is_empty() {
            return;
        }
        let keep = self.windows.current;
        // Close the floats (clearing the list, not just the map — a stale id there
        // panics the next redraw), then drop the other tiled windows.
        self.close_all_floats();
        self.windows.windows.retain(|id, _| *id == keep);
        self.windows.root = Node::Leaf(keep);
        self.relayout();
        self.ensure_visible();
    }

    /// `<C-w>=` / `:wincmd =` — reset every split to even shares and re-lay.
    pub(crate) fn equalize_windows(&mut self) {
        equalize_node(&mut self.windows.root);
        self.relayout();
        self.ensure_visible();
    }

    /// `<C-w>+`/`-` (height) and `<C-w>>`/`<` (width), and `:resize`/`:vertical
    /// resize` — grow or shrink the focused window by `delta` cells along `axis`,
    /// stealing from (or yielding to) a sibling. A no-op with a single window or
    /// no ancestor split of that orientation. `sizes` are in cells after the last
    /// layout, so this is plain cell arithmetic; the re-layout repaints it.
    pub(crate) fn resize_window(&mut self, axis: SplitDir, delta: isize) {
        let cur = self.windows.current;
        self.resize_window_id(cur, axis, delta);
    }

    /// Resize window `id` (focused or not) by `delta` cells along `axis`. The
    /// id-targeting core of [`Editor::resize_window`], shared with the
    /// `nvim_win_set_width`/`set_height` API and the mouse separator drag. A no-op
    /// with one window, a zero delta, or an unknown id.
    pub(crate) fn resize_window_id(&mut self, id: WindowId, axis: SplitDir, delta: isize) {
        if delta == 0 || self.windows.count() <= 1 || !self.windows.windows.contains_key(&id) {
            return;
        }
        let mut done = false;
        resize_toward(&mut self.windows.root, id, axis, delta, &mut done);
        self.relayout();
        self.ensure_visible();
    }

    /// `<C-w>_` (height) / `<C-w>|` (width) — maximize the focused window along
    /// `axis`, shrinking the others to the minimum. A no-op with one window.
    pub(crate) fn maximize_window(&mut self, axis: SplitDir) {
        if self.windows.count() <= 1 {
            return;
        }
        let cur = self.windows.current;
        maximize_toward(&mut self.windows.root, cur, axis);
        self.relayout();
        self.ensure_visible();
    }

    /// The surviving window whose rect center is closest to `from` — the
    /// new-focus pick after a close. Survivors are read from the (post-removal)
    /// tree; rects are still the pre-relayout values, which is exactly what
    /// "nearest to where the closed window was" wants.
    fn nearest_window(&self, from: Rect) -> WindowId {
        let (fx, fy) = from.center();
        self.windows
            .leaves()
            .into_iter()
            .min_by_key(|id| {
                let (cx, cy) = self.windows.get(*id).rect.center();
                fx.abs_diff(cx) + fy.abs_diff(cy)
            })
            .expect("closing a non-last window always leaves a survivor")
    }

    /// Move focus to the nearest window in `dir` (vim's `<C-w>h/j/k/l`). Only
    /// windows wholly on that side qualify; the closest by center wins. A no-op
    /// if there is none that way.
    pub(crate) fn focus_dir(&mut self, dir: WinDir) {
        let cur = self.windows.current;
        let from = self.windows.get(cur).rect;
        let (fx, fy) = from.center();
        let target = self
            .windows
            .leaves()
            .into_iter()
            .filter(|id| *id != cur)
            .filter(|id| {
                let r = self.windows.get(*id).rect;
                match dir {
                    WinDir::Left => r.x + r.width <= from.x,
                    WinDir::Right => r.x >= from.x + from.width,
                    WinDir::Up => r.y + r.height <= from.y,
                    WinDir::Down => r.y >= from.y + from.height,
                }
            })
            .min_by_key(|id| {
                let (cx, cy) = self.windows.get(*id).rect.center();
                fx.abs_diff(cx) + fy.abs_diff(cy)
            });
        if let Some(id) = target {
            self.focus_window(id);
        }
    }

    /// Cyclic window focus (vim's `<C-w>w` forward / `<C-w>W` backward), wrapping
    /// around the layout order. The cycle spans the tiled windows (in tree order)
    /// and any *focusable* floats (in z-order, appended after); non-focusable
    /// floats are skipped, as neovim does. (`nvim_set_current_win` can still focus
    /// a non-focusable float explicitly — only the `<C-w>` cycle excludes it.)
    pub(crate) fn focus_cycle(&mut self, forward: bool) {
        let mut order = self.windows.leaves();
        order.extend(self.windows.floats.iter().copied().filter(|f| {
            self.windows
                .get(*f)
                .float
                .as_ref()
                .is_some_and(|c| c.focusable)
        }));
        let n = order.len();
        if n <= 1 {
            return;
        }
        let cur = self.windows.current;
        let i = order.iter().position(|id| *id == cur).unwrap_or(0);
        let next = if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        self.focus_window(order[next]);
    }

    /// The window analogue of [`Editor::switch_buffer`]: stash the focused
    /// window's live view position, then make `id` current and restore its view.
    /// A no-op if `id` is already focused or not a live window.
    fn focus_window(&mut self, id: WindowId) {
        if id == self.windows.current || !self.windows.windows.contains_key(&id) {
            return;
        }
        // Stash the outgoing window's secondary multi-cursors before reading its
        // primary position — finalizing placement (see `stash_secondary_cursors`)
        // may snap the primary onto a placed cursor.
        self.stash_secondary_cursors();
        let cursor = self.cursor;
        let top = self.top;
        let leftcol = self.leftcol;
        let out = self.windows.current;
        {
            let w = self.windows.get_mut(out);
            w.saved_cursor = cursor;
            w.saved_top = top;
            w.saved_leftcol = leftcol;
        }
        self.enter_window(id);
    }

    /// Stash the focused window's live secondary multi-cursor set into its
    /// [`Window`] (the multi-cursor analogue of `saved_cursor`) and clear the live
    /// `CURSOR_NS`/`ANCHOR_NS` marks, so the next focused window starts from a
    /// clean slate. Finalizes any in-progress placement first, so the stashed set
    /// is exactly the placed cursors. Must run while the focused window's buffer is
    /// still current — before [`Editor::enter_window`] swaps it.
    pub(crate) fn stash_secondary_cursors(&mut self) {
        if self.mode == Mode::MultiCursor {
            self.finish_multicursor();
        }
        let positions = self.secondary_cursor_bytes();
        let cur = self.windows.current;
        self.windows.get_mut(cur).saved_cursors = positions;
        self.clear_secondary_cursors();
    }

    /// Make `id` the current window, restoring the buffer it shows and its saved
    /// view position, landing in normal mode with transient state cleared. The
    /// window analogue of [`Editor::enter_buffer`]: it does *not* stash the
    /// outgoing window (the caller did, or it is being closed). The restored
    /// cursor is clamped to the buffer's current line count (it may have shrunk
    /// while this window was inactive).
    pub(crate) fn enter_window(&mut self, id: WindowId) {
        self.windows.current = id;
        let w = self.windows.get_mut(id);
        let (buffer, cursor, top, leftcol) =
            (w.buffer, w.saved_cursor, w.saved_top, w.saved_leftcol);
        let saved_cursors = std::mem::take(&mut w.saved_cursors);
        self.set_cur_buffer(buffer);
        self.cursor = cursor;
        self.top = top;
        self.leftcol = leftcol;
        self.mode = Mode::Normal;
        self.reset_pending();
        self.scroll_from = None;
        self.pending_scroll = None;
        self.message.clear();
        // Restore this window's secondary multi-cursors onto its buffer (clearing
        // any leftover live marks first — a window close or tab switch can enter
        // here without the previous focus having stashed its own set).
        self.restore_secondary_cursors(saved_cursors);
        self.clamp_cursor();
        self.ensure_visible();
    }

    /// Resize the *text viewport*. The client owns the screen layout and tells
    /// us only how tall the text area is (status/command lines are the client's
    /// own regions), so the whole height here is editable rows.
    pub fn resize(&mut self, width: usize, height: usize) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.relayout();
        self.ensure_visible();
    }

    /// Whether the tabline is drawn right now, per `showtabline`: never at `0`,
    /// only with more than one tab at `1` (the default), always at `2`. The single
    /// gate both [`Editor::tabline_rows`] (the reserved row) and
    /// [`Editor::tab_labels`] (the projected labels) consult, so they never
    /// disagree.
    pub(crate) fn tabline_visible(&self) -> bool {
        match self.options.showtabline {
            0 => false,
            2 => true,
            _ => self.tabs.len() > 1,
        }
    }

    /// Rows the tabline reserves at the top of the reported area: one when the
    /// tabline is shown ([`Editor::tabline_visible`]), zero otherwise. The client
    /// paints the tabline into this row and offsets the windows area below it —
    /// the top-of-frame analogue of the bottom panel.
    pub(crate) fn tabline_rows(&self) -> usize {
        usize::from(self.tabline_visible())
    }

    /// Whether a window paints its own per-window status row. A **float** never
    /// does by default (matching neovim — see the body), regardless of
    /// `laststatus`. A **tiled** window follows `laststatus`: never at `0`, only
    /// with ≥2 tiled windows at `1`, always at `2` (the default), and never at `3`
    /// (a single global status line replaces the per-window ones). The single gate
    /// the view projection ([`crate::view`]) and the scroll math
    /// ([`Editor::text_height`]) consult so the reserved text row, the cursor
    /// scrolling, and the client's paint never disagree.
    pub(crate) fn window_statusline_visible(&self, floating: bool) -> bool {
        if floating {
            // A float carries no status line by default, matching neovim: its
            // `last_status` only walks the tiled frame tree, so a float's
            // status height stays 0 and its full inner height is content. (A
            // per-window opt-in could grow here later; nxvim has none yet.)
            return false;
        }
        match self.options.laststatus {
            0 | 3 => false,
            1 => self.windows.tiled_count() > 1,
            _ => true,
        }
    }

    /// Whether the single **global** status line is shown — only at
    /// `laststatus=3`. It docks one row at the bottom of the windows area (the
    /// bottom-of-frame analogue of the tabline) and shows the *current* window's
    /// status; per-window status rows are then hidden
    /// ([`Editor::window_statusline_visible`]).
    pub(crate) fn global_statusline_visible(&self) -> bool {
        self.options.laststatus == 3
    }

    /// Rows the global status line reserves at the bottom of the windows area: one
    /// at `laststatus=3` ([`Editor::global_statusline_visible`]), zero otherwise.
    fn global_statusline_rows(&self) -> usize {
        usize::from(self.global_statusline_visible())
    }

    /// Re-divide the current terminal area across the window tree. The windows
    /// area excludes the global bottom panel (it docks below all windows), the top
    /// tabline row (shown only with ≥2 tabs), and the global status-line row (only
    /// at `laststatus=3`); the command/message line is the client's own row,
    /// already excluded from the height the client reports. Re-run on resize and
    /// whenever the panel, tabline, or global status line appears/disappears
    /// (which grows/shrinks the windows area).
    pub(crate) fn relayout(&mut self) {
        let height = self
            .height
            .saturating_sub(self.panel_rows())
            .saturating_sub(self.tabline_rows())
            .saturating_sub(self.global_statusline_rows());
        // The focused window's cursor cell, as an offset from its own rect's
        // top-left — what a `relative="cursor"` float anchors to. Guard against a
        // transient invalid `current` (mid-close, before the surviving window is
        // entered): `cursor_virtcol` reads the current window's buffer, which is
        // gone for that instant. Only floats consume this, so (0, 0) is harmless.
        let cursor_off = if self.windows.windows.contains_key(&self.windows.current) {
            (
                self.cursor_virtcol(),
                self.cursor.line.saturating_sub(self.top),
            )
        } else {
            (0, 0)
        };
        self.windows.layout(
            Rect {
                x: 0,
                y: 0,
                width: self.width,
                height,
            },
            cursor_off,
        );
    }
}
