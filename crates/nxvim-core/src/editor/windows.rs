//! The window layout subsystem: the split tree (`Node`/`WindowTree`), the layout
//! algebra, floating windows, and the `<C-w>`/`:split` window-management methods.
//! `Node` and the layout free functions are private to this module.

use super::*;
use crate::mode::Mode;
use crate::options::{Options, WindowOptions};
use crate::view::{Separator, WindowRegion};
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
    /// RPC/Lua parse, used to format `nvim_win_get_config` and the `nx._wins`
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

/// Where a box sits inside its reference bounds — the high-level **alignment**
/// shared by every surface (floats, `nx.view`, pickers, the panel). A 9-grid:
/// pick a vertical band (top / middle / bottom) and a horizontal band (left /
/// center / right). This is sugar over the low-level [`FloatAnchor`] + `row`/`col`
/// offset — `place_aligned` turns `(align, margin)` into the box's top-left — so a
/// surface exposes one word (`"top-right"`) instead of an anchor-plus-offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    TopLeft,
    Top,
    TopRight,
    Left,
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

impl Align {
    /// The hyphenated keyword for this alignment — the inverse of
    /// [`Self::from_keyword`], the single source of truth shared by the RPC and
    /// Lua-effect parsers.
    pub fn as_str(self) -> &'static str {
        match self {
            Align::TopLeft => "top-left",
            Align::Top => "top",
            Align::TopRight => "top-right",
            Align::Left => "left",
            Align::Center => "center",
            Align::Right => "right",
            Align::BottomLeft => "bottom-left",
            Align::Bottom => "bottom",
            Align::BottomRight => "bottom-right",
        }
    }

    /// Parse an alignment keyword — the inverse of [`Self::as_str`]. `None` for an
    /// unrecognized word; each caller reports its own context-specific error (per
    /// the no-silent-fallback rule). `"centre"` is accepted as a spelling alias.
    pub fn from_keyword(s: &str) -> Option<Self> {
        Some(match s {
            "top-left" => Align::TopLeft,
            "top" => Align::Top,
            "top-right" => Align::TopRight,
            "left" => Align::Left,
            "center" | "centre" => Align::Center,
            "right" => Align::Right,
            "bottom-left" => Align::BottomLeft,
            "bottom" => Align::Bottom,
            "bottom-right" => Align::BottomRight,
            _ => return None,
        })
    }

    /// The vertical band: `0` = top, `1` = middle, `2` = bottom.
    fn vband(self) -> u8 {
        match self {
            Align::TopLeft | Align::Top | Align::TopRight => 0,
            Align::Left | Align::Center | Align::Right => 1,
            Align::BottomLeft | Align::Bottom | Align::BottomRight => 2,
        }
    }

    /// The horizontal band: `0` = left, `1` = center, `2` = right.
    fn hband(self) -> u8 {
        match self {
            Align::TopLeft | Align::Left | Align::BottomLeft => 0,
            Align::Top | Align::Center | Align::Bottom => 1,
            Align::TopRight | Align::Right | Align::BottomRight => 2,
        }
    }
}

/// An inset, in cells, from each edge of the reference bounds — so an aligned box
/// can sit in a corner *without touching the screen edge* (`margin = 2` on a
/// `"top-right"` float leaves a two-cell gap above and to the right). The relevant
/// edges depend on the alignment: a `top-right` box honors `top` + `right`, a
/// centered box ignores all four (the centering math already balances it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Margin {
    pub top: usize,
    pub right: usize,
    pub bottom: usize,
    pub left: usize,
}

/// The top-left cell of a `w`×`h` box placed by `align` inside the bounds
/// `(x, y, width, height)`, inset by `margin`. The single placement routine shared
/// by [`place_float`] (the float layer) and the server's picker/menu projection
/// (which works in plain ints, not the crate-private [`Rect`]), so every surface
/// aligns identically. Left/top bands anchor to the low edge plus the margin;
/// right/bottom bands anchor to the high edge minus the box minus the margin; the
/// center band splits the leftover evenly (margin-independent). The result is
/// clamped so the box stays within the bounds.
pub fn place_aligned(
    bounds: (usize, usize, usize, usize),
    w: usize,
    h: usize,
    align: Align,
    margin: Margin,
) -> (usize, usize) {
    let (bx, by, bw, bh) = bounds;
    let pos = |band: u8, lo: usize, span: usize, size: usize, near: usize, far: usize| -> usize {
        // All coordinates/dimensions are non-negative, so the placement is computed
        // entirely in saturating `usize`: `bounds`, `w`/`h`, the bands and margins
        // all derive from wire `u64`s cast to `usize` with no clamp at the dispatch
        // layer, so a hostile or buggy geometry can make any `lo + span` etc.
        // overflow. `saturating_add`/`saturating_sub` change the result only in that
        // overflow case (which would otherwise panic in debug / wrap in release);
        // for every in-range geometry the value is identical to the prior signed
        // (`isize`) form. A band offset that would underflow below `lo` saturates to
        // `0`, which the trailing clamp pins back up to `lo` — the same outcome the
        // signed `.clamp(lo, hi).max(0)` produced for a negative intermediate.
        let p = match band {
            0 => lo.saturating_add(near),
            2 => lo
                .saturating_add(span)
                .saturating_sub(size)
                .saturating_sub(far),
            _ => lo.saturating_add(span.saturating_sub(size) / 2),
        };
        // Clamp the box fully inside the bounds (a box larger than the span pins to
        // the low edge rather than spilling off-screen).
        let hi = lo.saturating_add(span).saturating_sub(size).max(lo);
        p.clamp(lo, hi)
    };
    let x = pos(align.hband(), bx, bw, w, margin.left, margin.right);
    let y = pos(align.vband(), by, bh, h, margin.top, margin.bottom);
    (x, y)
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
    /// RPC/Lua parse, used to format `nvim_win_get_config` and the `nx._wins`
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
///
/// `width`/`height` are [`Extent`]s (cells *or* a viewport fraction), resolved
/// against the editor area in [`place_float`] **every layout**, so a fractional
/// float reflows on resize. `align` is the high-level placement: `Some` ⇒ the box
/// is positioned by [`place_aligned`] (`anchor`/`row`/`col` ignored), `None` ⇒ the
/// low-level `nvim_open_win` `anchor` + `row`/`col` offset from the `relative`
/// origin. `margin` insets an aligned box from the edges.
///
/// Not `Eq` (an [`Extent::Frac`] holds an `f32`); compared by `PartialEq` only —
/// the same shape as [`crate::view::MenuView`].
#[derive(Debug, Clone, PartialEq)]
pub struct FloatConfig {
    pub relative: FloatRelative,
    pub anchor: FloatAnchor,
    pub row: isize,
    pub col: isize,
    pub width: Extent,
    pub height: Extent,
    /// High-level alignment within the `relative` bounds; `Some` supersedes
    /// `anchor`/`row`/`col`. The unified geometry surface sets this from the
    /// `align` keyword; `nvim_open_win`'s anchor/offset form leaves it `None`.
    pub align: Option<Align>,
    /// Edge inset (cells) for an aligned box; ignored when `align` is `None`.
    pub margin: Margin,
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
            width: Extent::Cells(1),
            height: Extent::Cells(1),
            align: None,
            margin: Margin::default(),
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
    pub width: Option<Extent>,
    pub height: Option<Extent>,
    /// High-level alignment update: `Some(Some(a))` sets it, `Some(None)` clears
    /// it back to the `anchor`/`row`/`col` form, `None` leaves it unchanged.
    pub align: Option<Option<Align>>,
    pub margin: Option<Margin>,
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
    /// This window's jump list — the positions jumped *from*, walked with
    /// `<C-o>`/`<C-i>`. Per-window like vim's; a split inherits a copy of its
    /// parent's. See [`crate::editor::JumpEntry`] and `editor/jumps.rs`.
    pub(crate) jumps: Vec<JumpEntry>,
    /// The navigation pointer into [`Window::jumps`]: `jumps.len()` means "at the
    /// present, not navigating"; `<C-o>` walks it toward 0, `<C-i>` back up.
    pub(crate) jump_idx: usize,
    /// A non-Normal mode to **resume** the next time this window is focused via the
    /// `<C-w><C-w>` dock chord (see [`Editor::dock_chord_intercept`]). Set when the
    /// chord crosses *out* of a window that was in insert / visual / terminal mode,
    /// and consumed by [`Editor::enter_window`] — so popping over to a dock and back
    /// lands you in the same mode you left. `None` for the common Normal case; any
    /// focus-in consumes it, so it can never go stale.
    pub(crate) resume: Option<ResumeState>,
    /// This window's **location list** stack — the per-window analogue of the global
    /// quickfix list, populated by the `:l*` commands (`:lvimgrep`/`:lgrep`/
    /// `setloclist`/…). `None` until the window first gets a loclist. Unlike vim,
    /// which shares one loclist by reference among windows split off each other,
    /// nxvim's loclists are strictly per-window: a split inherits a *clone* of its
    /// parent's (a documented divergence — see [`Editor::split`]), so the two then
    /// diverge independently. See [`crate::editor::quickfix`].
    pub(crate) loclist: Option<crate::editor::quickfix::QfStack>,
    /// The display buffer for this window's location-list window (`:lopen`), created
    /// lazily on first open — the per-window twin of [`Editor::qf_bufnr`]. A loclist
    /// *display* window shows this buffer; the window holding it here is the loclist
    /// *owner* (`<CR>`/`:ll` jump into this window). Never inherited on split (each
    /// `:lopen` mints its own), so a display buffer maps back to exactly one owner.
    pub(crate) loclist_bufnr: Option<BufferId>,
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

    /// Project the (private) split tree into a [`LayoutNode`] — the shape, with window
    /// ids at the leaves — so the persistence layer can capture the EXACT layout without
    /// `Node` itself crossing the module boundary.
    fn to_layout(&self) -> LayoutNode {
        match self {
            Node::Leaf(id) => LayoutNode::Leaf(*id),
            Node::Split {
                dir,
                children,
                sizes,
            } => LayoutNode::Split {
                vertical: matches!(dir, SplitDir::Vertical),
                sizes: sizes.clone(),
                children: children.iter().map(Node::to_layout).collect(),
            },
        }
    }

    /// Rebuild a split tree from a [`LayoutNode`] skeleton (restore). The leaf ids must
    /// already be minted and present in the accompanying window map.
    fn from_layout(layout: &LayoutNode) -> Node {
        match layout {
            LayoutNode::Leaf(id) => Node::Leaf(*id),
            LayoutNode::Split {
                vertical,
                sizes,
                children,
            } => Node::Split {
                dir: if *vertical {
                    SplitDir::Vertical
                } else {
                    SplitDir::Horizontal
                },
                sizes: sizes.clone(),
                children: children.iter().map(Node::from_layout).collect(),
            },
        }
    }
}

/// A boundary-crossing snapshot of the split tree's *shape*: the [`Node`] structure with
/// window ids at the leaves and the split direction flattened to a bool. The window
/// model keeps [`Node`] private; this is what `persist.rs` captures and rebuilds so a
/// session restores the EXACT nesting and (proportional) sizes, not an approximation.
#[derive(Debug, Clone)]
pub(crate) enum LayoutNode {
    Leaf(WindowId),
    Split {
        vertical: bool,
        sizes: Vec<usize>,
        children: Vec<LayoutNode>,
    },
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
                jumps: Vec::new(),
                jump_idx: 0,
                resume: None,
                loclist: None,
                loclist_bufnr: None,
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

    /// The split tree's shape as a [`LayoutNode`] (window ids at the leaves) — the
    /// capture half of session save/restore.
    pub(crate) fn layout_node(&self) -> LayoutNode {
        self.root.to_layout()
    }

    /// A tiled window with the given view state and otherwise-default fields — the
    /// restore-time twin of [`with_window`](Self::with_window)'s seed window.
    pub(crate) fn tiled_window(
        buffer: BufferId,
        saved_cursor: Cursor,
        saved_top: usize,
        saved_leftcol: usize,
    ) -> Window {
        Window {
            buffer,
            saved_cursor,
            saved_top,
            saved_leftcol,
            saved_cursors: Vec::new(),
            rect: Rect::default(),
            options: WindowOptions::default(),
            float: None,
            jumps: Vec::new(),
            jump_idx: 0,
            resume: None,
            loclist: None,
            loclist_bufnr: None,
        }
    }

    /// Assemble a tree from a prebuilt window map + a [`LayoutNode`] skeleton (restore).
    /// `current` must be one of the leaf ids in `root`. Separators are recomputed by the
    /// next [`relayout`](Self::relayout); there are no floats in a restored layout.
    pub(crate) fn from_layout(
        windows: BTreeMap<WindowId, Window>,
        root: LayoutNode,
        current: WindowId,
    ) -> Self {
        WindowTree {
            windows,
            root: Node::from_layout(&root),
            current,
            separators: Vec::new(),
            floats: Vec::new(),
        }
    }

    pub(crate) fn get(&self, id: WindowId) -> &Window {
        self.windows
            .get(&id)
            .expect("current window id is always valid")
    }

    /// Fallible [`get`](Self::get) for an id that may not be open (a window handle
    /// from outside, e.g. `vim.fn.getjumplist(winid)`).
    pub(crate) fn try_get(&self, id: WindowId) -> Option<&Window> {
        self.windows.get(&id)
    }

    pub(crate) fn get_mut(&mut self, id: WindowId) -> &mut Window {
        self.windows
            .get_mut(&id)
            .expect("current window id is always valid")
    }

    /// Fallible [`get_mut`](Self::get_mut) for an id that may not be open — the
    /// mutable twin of [`try_get`](Self::try_get) (e.g. populating a location list
    /// on a window identified by a possibly-stale handle).
    pub(crate) fn try_get_mut(&mut self, id: WindowId) -> Option<&mut Window> {
        self.windows.get_mut(&id)
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

    /// Every window in this tree (tiled and floating), mutably — used by the
    /// jumplist line-adjustment to shift `<C-o>` targets in all windows that show
    /// an edited buffer, not just the focused one.
    pub(crate) fn all_windows_mut(&mut self) -> impl Iterator<Item = &mut Window> {
        self.windows.values_mut()
    }

    /// Every window in this tree (tiled and floating), read-only — used to find
    /// which windows show a buffer about to be deleted (see
    /// [`Editor::rebind_windows_off_buffer`]).
    pub(crate) fn all_windows(&self) -> impl Iterator<Item = &Window> {
        self.windows.values()
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
/// They are [`Extent`]s resolved against `bounds` (the editor area, the "viewport")
/// **here, every layout** — so a fractional float reflows on resize. Resolving
/// against `bounds` rather than `origin` is deliberate: `origin` is zero-size for
/// `relative = cursor`, which would collapse every fractional cursor float to one
/// cell.
///
/// Placement has two modes. When `cfg.align` is `Some`, the box is positioned by
/// [`place_aligned`] within `bounds`, inset by `cfg.margin` (the high-level unified
/// geometry; `anchor`/`row`/`col` are ignored). Otherwise the low-level
/// `nvim_open_win` form: apply the `row`/`col` offset from the origin's top-left,
/// shift so the `anchor` corner lands there (an `E` anchor subtracts the width, an
/// `S` anchor the height), then clamp on-screen.
///
/// `nvim_win_get_config` reports the resolved inner cells off the laid-out rect
/// (see `float_mirror` / `win_config_value`), not the raw `Extent`. A float larger
/// than `bounds` pins to the top-left rather than shrinking.
fn place_float(origin: Rect, bounds: Rect, cfg: &FloatConfig) -> Rect {
    let border = if cfg.border != BorderStyle::None {
        2
    } else {
        0
    };
    let w = cfg.width.resolve(bounds.width).max(1) + border;
    let h = cfg.height.resolve(bounds.height).max(1) + border;
    let (x, y) = if let Some(align) = cfg.align {
        place_aligned(
            (bounds.x, bounds.y, bounds.width, bounds.height),
            w,
            h,
            align,
            cfg.margin,
        )
    } else {
        // `cfg.col`/`cfg.row` are wire-derived (`nvim_open_win`) and unbounded, so a
        // hostile near-`isize::MAX` offset would overflow a raw `isize` add (panic in
        // debug, wrap in release). Saturate: the trailing `clamp(lo, hi).max(0)` pins
        // the result on-screen regardless, so in-range geometry is unaffected.
        let mut x = (origin.x as isize).saturating_add(cfg.col);
        let mut y = (origin.y as isize).saturating_add(cfg.row);
        if matches!(cfg.anchor, FloatAnchor::NE | FloatAnchor::SE) {
            x = x.saturating_sub(w as isize);
        }
        if matches!(cfg.anchor, FloatAnchor::SW | FloatAnchor::SE) {
            y = y.saturating_sub(h as isize);
        }
        let lo_x = bounds.x as isize;
        let lo_y = bounds.y as isize;
        let hi_x = ((bounds.x + bounds.width).saturating_sub(w) as isize).max(lo_x);
        let hi_y = ((bounds.y + bounds.height).saturating_sub(h) as isize).max(lo_y);
        (
            x.clamp(lo_x, hi_x).max(0) as usize,
            y.clamp(lo_y, hi_y).max(0) as usize,
        )
    };
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
                                region: WindowRegion::Main,
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
                                region: WindowRegion::Main,
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
    /// This window's stable id (the handle `nvim_list_wins` /
    /// `nvim_get_current_win` report), so the projection can key per-window state
    /// (the `nx.statusline` custom-segment cache) by window.
    pub(crate) id: WindowId,
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
    /// Which screen region (main area or a dock) this window belongs to. Its
    /// `rect` is relative to that region's own origin.
    pub(crate) region: WindowRegion,
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
        let mut ids = Vec::new();
        for layer in self.open_layers() {
            if let Some(t) = self.layer_tree(layer) {
                ids.extend(t.leaves());
                ids.extend(t.floats.iter().copied());
            }
        }
        ids
    }

    /// Number of open windows across the main tree and every open dock (always ≥ 1).
    pub fn window_count(&self) -> usize {
        self.open_layers()
            .into_iter()
            .filter_map(|l| self.layer_tree(l))
            .map(|t| t.count())
            .sum()
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
    /// is not open or already current. When `id` lives in a different layer (a
    /// dock, or the main tree while a dock is focused), cross to that layer first.
    pub fn set_current_window(&mut self, id: WindowId) {
        if self.windows.try_get(id).is_some() {
            self.focus_window(id);
            return;
        }
        if let Some((layer, _)) = self.tree_of_window(id) {
            self.switch_layer(layer);
            self.focus_window(id);
        }
    }

    /// The buffer window `id` shows (`nvim_win_get_buf`), or `None` if there is
    /// no such window.
    pub fn window_buffer(&self, id: WindowId) -> Option<BufferId> {
        self.tree_of_window(id).map(|(_, t)| t.get(id).buffer)
    }

    /// The first `(tab index, window id)` whose tiled window shows `buf`,
    /// scanning tabs in tabline order and windows in layout order. The active
    /// tab reads its live tree; inactive tabs read their stashed one. Backs
    /// `:drop` / `:tab drop`'s "already open somewhere?" check. Floats are
    /// excluded — `:drop` targets editable windows, as in vim.
    pub(crate) fn window_showing(&self, buf: BufferId) -> Option<(usize, WindowId)> {
        for (idx, tab) in self.main_tabs.tabs.iter().enumerate() {
            let tree = if idx == self.main_tabs.current {
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

    /// The `(tab index, window id)` a jump to `buf` should reuse per `'switchbuf'`,
    /// or `None` to open in the current window. `usetab` reuses a window showing
    /// `buf` in **any** tab (switching to it); `useopen` reuses one only in the
    /// **current** tab; neither flag (or an empty `'switchbuf'`) means no reuse.
    /// Built on [`Self::window_showing`] (which scans every tab). Backs the
    /// `'switchbuf'` handling in [`Editor::open_path_switchbuf`] /
    /// [`Editor::switch_to_buffer_switchbuf`].
    pub(crate) fn switchbuf_window(&self, buf: BufferId) -> Option<(usize, WindowId)> {
        let swb = &self.options.switchbuf;
        let usetab = swb.split(',').any(|s| s.trim() == "usetab");
        let useopen = swb.split(',').any(|s| s.trim() == "useopen");
        if !usetab && !useopen {
            return None;
        }
        let found = self.window_showing(buf)?;
        // useopen alone is scoped to the current tab; usetab considers every tab.
        if usetab || found.0 == self.main_tabs.current {
            Some(found)
        } else {
            None
        }
    }

    /// Rebind window `id` to show buffer `buf` *without* changing focus
    /// (`nvim_win_set_buf`). The focused window swaps its live buffer (like `:b`);
    /// an inactive window updates its binding and clamps its stashed cursor to the
    /// new buffer. A no-op if either handle is unknown.
    pub fn set_window_buffer(&mut self, id: WindowId, buf: BufferId) {
        if self.tree_of_window(id).is_none() || !self.buffers.map.contains_key(&buf) {
            return;
        }
        if id == self.windows.current {
            self.switch_buffer(buf);
            return;
        }
        let lines = self.buffers.get(buf).buffer.line_count();
        // The buffer now lives in this window's layer — record its home so the
        // per-layer buffer list (`:ls`, the close-fallback) stays scoped correctly.
        let layer = self
            .tree_of_window(id)
            .map(|(l, _)| l)
            .expect("checked above");
        self.set_buffer_layer(buf, layer);
        let w = self
            .tree_of_window_mut(id)
            .expect("checked above")
            .get_mut(id);
        w.buffer = buf;
        if w.saved_cursor.line >= lines {
            w.saved_cursor.line = lines.saturating_sub(1);
            w.saved_cursor.col = 0;
        }
    }

    /// Window `id`'s window-local options (the number gutter), or `None` for an
    /// unknown id. The server snapshots these into the `vim.wo` mirror.
    pub fn window_options(&self, id: WindowId) -> Option<WindowOptions> {
        self.tree_of_window(id)
            .map(|(_, t)| t.get(id).options.clone())
    }

    /// Set a boolean window-local option on window `id` (`vim.wo` /
    /// `nvim_win_set_option`). Recognizes `number` / `relativenumber` /
    /// `cursorline` / `wrap` / `scrollanim`; a no-op for any other name or an unknown
    /// id. `0` is resolved to the focused window by the caller.
    pub fn set_window_option_bool(&mut self, id: WindowId, name: &str, value: bool) {
        let Some(t) = self.tree_of_window_mut(id) else {
            return;
        };
        let w = t.get_mut(id);
        match name {
            "number" => w.options.number = value,
            "relativenumber" => w.options.relativenumber = value,
            "cursorline" => w.options.cursorline = value,
            "wrap" => w.options.wrap = value,
            // A per-window override of the global `'scrollanim'` (see
            // [`WindowOptions::scrollanim`]); `Some(value)` shadows the global until unset.
            "scrollanim" => w.options.scrollanim = Some(value),
            _ => {}
        }
    }

    /// Set a numeric window-local option on window `id` (`vim.wo` /
    /// `nvim_win_set_option`), the numeric analogue of
    /// [`Editor::set_window_option_bool`]. Recognizes `numberwidth` (clamped to a
    /// `1` minimum, like the `:set` path); a no-op for any other name or unknown id.
    pub fn set_window_option_num(&mut self, id: WindowId, name: &str, value: i64) {
        let Some(t) = self.tree_of_window_mut(id) else {
            return;
        };
        let w = t.get_mut(id);
        if name == "numberwidth" {
            w.options.numberwidth = value.max(1) as usize;
        } else if name == "padding" {
            // `vim.wo.padding = 2` (a bare number) sets a uniform margin; the
            // string forms (`"1 2"`, …) come through `set_window_option_str`. A
            // negative value clamps to zero (no margin).
            w.options.padding = crate::options::Padding::uniform(value.max(0) as usize);
            // Re-clamp the viewport once the `&mut w` borrow ends — the text area
            // grew or shrank.
            self.ensure_visible();
        }
    }

    /// Set a string window-local option on window `id` (`vim.wo` /
    /// `nvim_win_set_option`), the string analogue of
    /// [`Editor::set_window_option_bool`]. Recognizes `signcolumn` and `fillchars`
    /// (an invalid value is ignored, matching the no-op-on-bad-input contract of the
    /// other bridge setters); a no-op for any other name or unknown id.
    pub fn set_window_option_str(&mut self, id: WindowId, name: &str, value: &str) {
        let Some(t) = self.tree_of_window_mut(id) else {
            return;
        };
        let opts = &mut t.get_mut(id).options;
        // An invalid value is ignored, matching the no-op-on-bad-input contract of
        // the other bridge setters (`:set` is the loud-error path).
        let mut geometry_changed = false;
        if name == "signcolumn" {
            if let Some(scl) = crate::options::SignColumn::parse(value) {
                opts.signcolumn = scl;
            }
        } else if name == "fillchars" && crate::options::parse_fillchars(value).is_some() {
            opts.fillchars = value.to_string();
        } else if name == "winhighlight" {
            // The per-window highlight remap (`'winhighlight'` / `'winhl'`). Stored
            // raw and parsed to a `WinHl` at projection (like `fillchars`); malformed
            // pairs are dropped there, matching this bridge's lenient contract (`:set`
            // is the loud path). A window-local value overrides the dock's, if any —
            // see `Editor::effective_winhighlight`.
            opts.winhighlight = value.to_string();
        } else if name == "padding" {
            // An invalid spec is ignored (the no-op-on-bad-input bridge contract;
            // `:set padding=` is the loud-error path).
            if let Some(pad) = crate::options::parse_padding(value) {
                opts.padding = pad;
                geometry_changed = true;
            }
        }
        // Re-clamp the viewport once the `&mut opts` borrow ends: `padding` grew or
        // shrank the text area.
        if geometry_changed {
            self.ensure_visible();
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
            "imagepreview" => self.options.imagepreview = value,
            "timeout" => self.options.timeout = value,
            "scrollanim" => self.options.scrollanim = value,
            "qfdock" => self.options.qfdock = value,
            "bdclosetab" => self.options.bdclosetab = value,
            "relative_splits" => self.options.relative_splits = value,
            "relative_docks" => self.options.relative_docks = value,
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
        if name == "mousetime"
            || name == "timeoutlen"
            || name == "scrollanimduration"
            || name == "scrollback"
        {
            if value < 0 {
                self.echo(format!("E487: Argument must be positive: {name}={value}"));
                return;
            }
            match name {
                "mousetime" => self.options.mousetime = value as usize,
                "timeoutlen" => self.options.timeoutlen = value as usize,
                "scrollanimduration" => self.options.scrollanimduration = value as usize,
                "scrollback" => self.options.scrollback = value as usize,
                _ => unreachable!("guarded above"),
            }
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
    /// strings (`mouse`/`mousemodel`/`mousescroll`), `regexsyntax`, and
    /// `fileencodings`; an unknown
    /// name is a no-op (the Lua side forwards only the canonical wired set). This is
    /// the same state the `:set statusline=…` / `:set guifont=…` ex path writes —
    /// the two routes share one home. (The `:set` path additionally *validates*
    /// `regexsyntax`; a raw `vim.o` write of a bad value reads back as `pcre`.)
    ///
    /// Returns whether `name` was a wired string global, so the `:set` path can fail
    /// loud (E518) on an unhandled name instead of silently no-op'ing; the Lua bridge
    /// ignores the result (it forwards only the canonical wired set).
    pub fn set_global_option_str(&mut self, name: &str, value: &str) -> bool {
        match name {
            "statusline" => self.options.statusline = value.to_string(),
            "tabline" => self.options.tabline = value.to_string(),
            "guifont" => self.options.guifont = value.to_string(),
            "mouse" => self.options.mouse = value.to_string(),
            "mousemodel" => self.options.mousemodel = value.to_string(),
            "mousescroll" => self.options.mousescroll = value.to_string(),
            "regexsyntax" => self.options.regexsyntax = value.to_string(),
            "fileencodings" => self.options.fileencodings = value.to_string(),
            "errorformat" => self.options.errorformat = value.to_string(),
            "switchbuf" => self.options.switchbuf = value.to_string(),
            "makeprg" => self.options.makeprg = value.to_string(),
            "grepprg" => self.options.grepprg = value.to_string(),
            "grepformat" => self.options.grepformat = value.to_string(),
            _ => return false,
        }
        true
    }

    /// The editor's global options, for the server to mirror to Lua (`vim.o`).
    pub fn global_options(&self) -> Options {
        self.options.clone()
    }

    /// Whether `'timeout'` is on — a cheap bool read (no [`Options`] clone) for the
    /// hot idle-flush path, which consults it on every flush to decide whether a
    /// withheld mapped prefix should resolve on idle (`timeout`) or wait forever for
    /// the next key (`notimeout`).
    pub fn timeout_enabled(&self) -> bool {
        self.options.timeout
    }

    /// The `'timeoutlen'` wait in ms, for the wasm host to arm its idle-flush
    /// deadline (the native clients read the relayed value off the `redraw`).
    pub fn timeoutlen_ms(&self) -> u64 {
        self.options.timeoutlen as u64
    }

    /// Window `id`'s cursor as `(0-based line, byte col)` — the live cursor for
    /// the focused window, the stashed one otherwise (`nvim_win_get_cursor`).
    /// `None` if there is no such window.
    pub fn window_cursor(&self, id: WindowId) -> Option<(usize, usize)> {
        let (_, t) = self.tree_of_window(id)?;
        let c = if id == self.windows.current {
            self.cursor
        } else {
            t.get(id).saved_cursor
        };
        Some((c.line, c.col))
    }

    /// Move window `id`'s cursor to `(0-based line, byte col)` (`nvim_win_set_cursor`).
    /// The focused window moves its live cursor (clamped, view kept visible); an
    /// inactive window updates its stashed position (clamped to its buffer's line
    /// count; the column is re-clamped when the window is next focused). A no-op
    /// for an unknown id.
    pub fn set_window_cursor(&mut self, id: WindowId, line: usize, col: usize) {
        let Some((_, t)) = self.tree_of_window(id) else {
            return;
        };
        if id == self.windows.current {
            self.cursor.line = line;
            self.cursor.col = col;
            self.clamp_cursor();
            self.desired_col = self.cursor_virtcol();
            self.ensure_visible();
            return;
        }
        let buf = t.get(id).buffer;
        let lines = self.buffers.get(buf).buffer.line_count();
        let w = self
            .tree_of_window_mut(id)
            .expect("checked above")
            .get_mut(id);
        w.saved_cursor.line = line.min(lines.saturating_sub(1));
        w.saved_cursor.col = col;
    }

    /// Window `id`'s scroll offset as `(top, leftcol)` — the first visible buffer
    /// line (0-based) and the first visible screen column. The focused window
    /// reports its live offset; an inactive window its stashed one. `None` for an
    /// unknown id. Backs `vim.fn.winsaveview`'s `topline`/`leftcol`.
    pub fn window_scroll(&self, id: WindowId) -> Option<(usize, usize)> {
        let (_, t) = self.tree_of_window(id)?;
        Some(if id == self.windows.current {
            (self.top, self.leftcol)
        } else {
            let w = t.get(id);
            (w.saved_top, w.saved_leftcol)
        })
    }

    /// Window `id`'s text offset — the gutter columns before the first text cell
    /// (the number gutter plus the sign-column floor core can know about). `None`
    /// for an unknown id. Feeds the server's screen-column math for
    /// `vim.fn.screencol`.
    pub fn window_textoff(&self, id: WindowId) -> Option<usize> {
        let (_, t) = self.tree_of_window(id)?;
        let w = t.get(id);
        let lines = self.buffers.get(w.buffer).buffer.line_count();
        Some(self.number_width_for(&w.options, lines) + w.options.signcolumn.floor_cells())
    }

    /// Scroll window `id` so its first visible line is `top` (0-based), clamped to
    /// the buffer's last line. The focused window moves its live viewport; an
    /// inactive window updates its stashed `top` (applied when next focused). A
    /// no-op for an unknown id. Backs `vim.fn.winrestview`'s `topline`.
    pub fn set_window_topline(&mut self, id: WindowId, top: usize) {
        let Some((_, t)) = self.tree_of_window(id) else {
            return;
        };
        let buf = t.get(id).buffer;
        let last = self.buffers.get(buf).buffer.line_count().saturating_sub(1);
        let top = top.min(last);
        if id == self.windows.current {
            self.top = top;
        } else {
            self.tree_of_window_mut(id)
                .expect("checked above")
                .get_mut(id)
                .saved_top = top;
        }
    }

    /// Horizontally scroll window `id` so its first visible screen column is
    /// `leftcol` (0-based). The focused window moves its live viewport; an inactive
    /// window updates its stashed `leftcol` (applied when next focused). A no-op for
    /// an unknown id. Backs `vim.fn.winrestview`'s `leftcol` / `nx.win.set_leftcol`.
    /// Only meaningful with `'nowrap'`; the next cursor move may re-derive `leftcol`
    /// to keep the cursor visible, as in vim.
    pub fn set_window_leftcol(&mut self, id: WindowId, leftcol: usize) {
        let Some((_, _)) = self.tree_of_window(id) else {
            return;
        };
        if id == self.windows.current {
            self.leftcol = leftcol;
        } else {
            self.tree_of_window_mut(id)
                .expect("checked above")
                .get_mut(id)
                .saved_leftcol = leftcol;
        }
    }

    /// Window `id`'s rect as `(x, y, width, height)` in windows-area cells, or
    /// `None` if there is no such window. `height` includes the status-line row;
    /// the API width/height the server returns derive from this.
    pub fn window_rect(&self, id: WindowId) -> Option<(usize, usize, usize, usize)> {
        self.tree_of_window(id).map(|(_, t)| {
            let w = t.get(id);
            (w.rect.x, w.rect.y, w.rect.width, w.rect.height)
        })
    }

    /// Window `id`'s viewport top (first visible 0-based buffer line): the live
    /// `self.top` for the focused window, the stashed `saved_top` otherwise. `0`
    /// for an unknown window.
    pub fn window_top(&self, id: WindowId) -> usize {
        if id == self.windows.current {
            self.top
        } else {
            self.tree_of_window(id)
                .map_or(0, |(_, t)| t.get(id).saved_top)
        }
    }

    /// Window `id`'s **content** size as `(width, height)` — what
    /// `nvim_win_get_width` / `nvim_win_get_height` report. The width includes the
    /// number gutter (as neovim's does) but excludes a bordered float's side
    /// columns; the height excludes a bordered float's border rows and the status
    /// row when one is shown. Mirrors the [`crate::view::window_view`] /
    /// [`Editor::text_height`] content math so the API agrees with what is drawn.
    pub fn window_content_size(&self, id: WindowId) -> Option<(usize, usize)> {
        let (layer, t) = self.tree_of_window(id)?;
        let w = t.get(id);
        let inset = matches!(&w.float, Some(cfg) if cfg.border != BorderStyle::None) as usize;
        let status =
            usize::from(self.window_statusline_visible(region_of_layer(layer), w.float.is_some()));
        let width = w.rect.width.saturating_sub(2 * inset);
        let height = w
            .rect
            .height
            .saturating_sub(2 * inset)
            .saturating_sub(status);
        Some((width, height))
    }

    /// Window `id`'s **padded** text area — its [content
    /// size](Editor::window_content_size) further inset by `'padding'` (horizontal
    /// from the width, vertical from the height). This is the area the client paints
    /// the gutter/text/status into and the space the hit-test, viewport-decoration,
    /// and scroll math reason about — it matches the `width`/`height`
    /// [`view::window_view`](crate::view) projects. `window_content_size` itself stays
    /// padding-free, so the `nvim_win_*` size getters and a float's reported config
    /// round-trip unchanged. `None` for an unknown window.
    pub fn window_text_area(&self, id: WindowId) -> Option<(usize, usize)> {
        let (w, h) = self.window_content_size(id)?;
        let pad = self.window_options(id)?.padding;
        Some((
            w.saturating_sub(pad.horizontal()),
            h.saturating_sub(pad.vertical()),
        ))
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
        let mut options = self.windows.get(self.windows.current).options.clone();
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
                jumps: Vec::new(),
                jump_idx: 0,
                resume: None,
                loclist: None,
                loclist_bufnr: None,
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
                    width: Extent::Cells(w.max(1).min(u16::MAX as usize) as u16),
                    height: Extent::Cells(h.max(1).min(u16::MAX as usize) as u16),
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
            cfg.width = v;
        }
        if let Some(v) = spec.height {
            cfg.height = v;
        }
        if let Some(v) = spec.align {
            cfg.align = v;
        }
        if let Some(v) = spec.margin {
            cfg.margin = v;
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
        // The globally focused window is `self.windows.current` *in the focused
        // layer's tree*; a parked tree's `current` is not focused (its cursor is
        // not drawn), so it renders its stashed view.
        let cur = self.windows.current;
        let mut out = Vec::new();
        for layer in self.open_layers() {
            let Some(tree) = self.layer_tree(layer) else {
                continue;
            };
            let region = region_of_layer(layer);
            let focused_layer = layer == self.focused_layer;
            // Tiled windows in tree order first, then floats bottom-to-top by
            // `(zindex, id)` — the same order `window_ids`/`nvim_list_wins` uses.
            for id in tree.leaves().into_iter().chain(tree.floats.iter().copied()) {
                let w = tree.get(id);
                let focused = focused_layer && id == cur;
                let (floating, border, title) = match &w.float {
                    Some(cfg) => (true, cfg.border, cfg.title.clone()),
                    None => (false, BorderStyle::None, None),
                };
                out.push(WindowLayout {
                    id,
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
                    options: w.options.clone(),
                    floating,
                    border,
                    title,
                    region,
                });
            }
        }
        out
    }

    /// The split borders for every open layer (the focused tree plus every parked
    /// dock / main tree), each tagged with its [`WindowRegion`] so the client can
    /// offset it by the region origin. Empty with one window and no dock.
    pub(crate) fn all_separators(&self) -> Vec<Separator> {
        let mut out = Vec::new();
        for layer in self.open_layers() {
            let Some(tree) = self.layer_tree(layer) else {
                continue;
            };
            let region = region_of_layer(layer);
            out.extend(tree.separators.iter().map(|s| Separator { region, ..*s }));
        }
        out
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
        let options = self.windows.get(cur).options.clone();
        // …and a copy of its jump list, so `<C-o>` history carries into the split
        // (vim copies the jumplist to the new window).
        let jumps = self.windows.get(cur).jumps.clone();
        let jump_idx = self.windows.get(cur).jump_idx;
        // The split inherits a *clone* of the parent's location list (nxvim's
        // per-window, non-shared model — vim shares it by reference). It does not
        // inherit the loclist *display* buffer: a fresh `:lopen` mints its own, so
        // every loclist display buffer maps back to exactly one owner window.
        let loclist = self.windows.get(cur).loclist.clone();
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
                jumps,
                jump_idx,
                resume: None,
                loclist,
                loclist_bufnr: None,
            },
        );
        split_leaf(&mut self.windows.root, cur, dir, new_id);
        self.windows.current = new_id;
        // The new window shows the same buffer at the same position, so the live
        // `cursor`/`top` already describe it — only the viewport shrank.
        self.relayout();
        self.ensure_visible();
    }

    /// Open a window showing `buf` as a full-width split at the very bottom of the
    /// tiled layout — vim's `botright` placement — `height` rows tall, and focus
    /// it. Unlike [`Editor::split`] (which splits the *focused* window, new sibling
    /// on top, 50/50) this wraps the whole layout so the new window spans the full
    /// width below everything, regardless of any vertical splits. Backs `:copen`.
    /// Returns the new window's id.
    pub(crate) fn open_bottom_window(&mut self, buf: BufferId, height: usize) -> WindowId {
        let options = self.windows.get(self.windows.current).options.clone();
        let new_id = self.alloc_window_id();
        self.windows.windows.insert(
            new_id,
            Window {
                buffer: buf,
                saved_cursor: Cursor::default(),
                saved_top: 0,
                saved_leftcol: 0,
                saved_cursors: Vec::new(),
                rect: Rect::default(),
                options,
                float: None,
                jumps: Vec::new(),
                jump_idx: 0,
                resume: None,
                loclist: None,
                loclist_bufnr: None,
            },
        );
        // Wrap the entire tiled layout: [existing, new] stacked vertically, the new
        // window last (bottom). A placeholder briefly stands in for the root while
        // it is moved into the split's first child.
        let old_root = std::mem::replace(&mut self.windows.root, Node::Leaf(new_id));
        self.windows.root = Node::Split {
            dir: SplitDir::Horizontal,
            children: vec![old_root, Node::Leaf(new_id)],
            sizes: vec![1, 1],
        };
        // Lay out so rects exist, focus (stashing the old window's view, seeding the
        // new one's), then shrink to the requested height and lay out again.
        self.relayout();
        self.focus_window(new_id);
        self.set_window_height(new_id, height);
        self.relayout();
        self.ensure_visible();
        new_id
    }

    /// `<C-w>c` / `:close` — close the focused window and expand a neighbor to
    /// fill the freed area. On the last *tiled* window it closes any open floats
    /// instead (the editor can't be left showing only floats); with none open it
    /// refuses (vim's `E444`). The quit-when-last semantics belong to `:q`.
    pub(crate) fn close_window(&mut self) {
        // Closing the panel window (`:q` / `:close` / `<C-w>c` / `<C-w>q` all land here)
        // is a panel dismissal: route to `close_panel` so it clears the focus lock,
        // collapses the overlay, and restores focus — a bare `remove_window` would leave
        // `Editor::panel` dangling at a closed window id.
        if self.panel_window() == Some(self.windows.current) {
            self.close_panel();
            return;
        }
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
        // If `id` owns a location list shown in a loclist window, that loclist
        // window must close with it (vim's behavior — a `:lopen` window belongs to
        // its owner). Captured here, discarded on the success path below.
        let orphan_loclist = self.windows.get(id).loclist_bufnr;
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
        if let Some(buf) = orphan_loclist {
            self.discard_loclist_display(buf);
        }
        true
    }

    /// Close the location-list window showing `buf` (an orphaned loclist display
    /// buffer whose owner window just closed) and drop the now-unshown scratch
    /// buffer. The display window carries no `loclist_bufnr` of its own, so the
    /// inner [`Editor::remove_window`] won't recurse back here.
    fn discard_loclist_display(&mut self, buf: BufferId) {
        if let Some(w) = self
            .window_ids()
            .into_iter()
            .find(|&w| self.windows.get(w).buffer == buf)
        {
            self.remove_window(w);
        }
        // Delete the scratch buffer only once nothing shows it (closing the last
        // window is refused, in which case it stays visible and must keep its buffer).
        if self.buffers.map.contains_key(&buf)
            && !self.windows.all_windows().any(|win| win.buffer == buf)
        {
            self.delete_buffer(buf, true);
        }
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

    /// Resize window `id` (focused or not, in any layer) by `delta` cells along
    /// `axis`. The id-targeting core of [`Editor::resize_window`], shared with the
    /// `nvim_win_set_width`/`set_height` API and the mouse separator drag. Resizes
    /// within `id`'s own tree — a dock's split resizes inside that dock without
    /// crossing focus. A no-op when that tree has one window, a zero delta, or an
    /// unknown id.
    pub(crate) fn resize_window_id(&mut self, id: WindowId, axis: SplitDir, delta: isize) {
        if delta == 0 {
            return;
        }
        let mut done = false;
        {
            let Some(tree) = self.tree_of_window_mut(id) else {
                return;
            };
            if tree.count() <= 1 || !tree.windows.contains_key(&id) {
                return;
            }
            resize_toward(&mut tree.root, id, axis, delta, &mut done);
        }
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
        if let Some(id) = self.window_in_dir(dir) {
            self.focus_window(id);
        }
    }

    /// The nearest tiled window to the focused one in `dir` — the directional pick
    /// shared by `<C-w>h/j/k/l` focus ([`Editor::focus_dir`]) and `<C-w>H/J/K/L`
    /// buffer-swap ([`Editor::swap_window_dir`]): the candidate wholly on that side
    /// whose center is closest. `None` when there is no window on that side.
    fn window_in_dir(&self, dir: WinDir) -> Option<WindowId> {
        let cur = self.windows.current;
        let from = self.windows.get(cur).rect;
        let (fx, fy) = from.center();
        self.windows
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
            })
    }

    /// `<C-w>H/J/K/L` — swap the focused window's buffer **and its view** (cursor,
    /// scroll, secondary multi-cursors) with the nearest window in `dir`, then focus
    /// that window so the buffer appears to have moved there (vim's `<C-w>H` feel).
    /// Window positions, sizes, options and jumplists stay put — only the buffer +
    /// view trade places. A no-op when there is no window on that side.
    pub(crate) fn swap_window_dir(&mut self, dir: WinDir) {
        let Some(target) = self.window_in_dir(dir) else {
            return;
        };
        let cur = self.windows.current;
        // Stash the focused window's live view into its [`Window`] (mirrors
        // `focus_window`), so both windows hold their full view in the tree before
        // the swap; secondary cursors are stashed and their live marks cleared.
        self.stash_secondary_cursors();
        let (cursor, top, leftcol) = (self.cursor, self.top, self.leftcol);
        {
            let w = self.windows.get_mut(cur);
            w.saved_cursor = cursor;
            w.saved_top = top;
            w.saved_leftcol = leftcol;
        }
        self.swap_window_view(cur, target);
        // Focus follows the buffer: enter the target, loading the just-swapped-in
        // view as live.
        self.enter_window(target);
    }

    /// Swap the buffer and view payload (cursor, scroll, secondary cursors) between
    /// windows `a` and `b`, leaving each window's position, size, options and
    /// jumplist in place. Both must be windows of the focused tree.
    fn swap_window_view(&mut self, a: WindowId, b: WindowId) {
        let wa = self.windows.get_mut(a);
        let pa = (
            wa.buffer,
            wa.saved_cursor,
            wa.saved_top,
            wa.saved_leftcol,
            std::mem::take(&mut wa.saved_cursors),
        );
        let wb = self.windows.get_mut(b);
        let pb = (
            wb.buffer,
            wb.saved_cursor,
            wb.saved_top,
            wb.saved_leftcol,
            std::mem::take(&mut wb.saved_cursors),
        );
        wb.buffer = pa.0;
        wb.saved_cursor = pa.1;
        wb.saved_top = pa.2;
        wb.saved_leftcol = pa.3;
        wb.saved_cursors = pa.4;
        let wa = self.windows.get_mut(a);
        wa.buffer = pb.0;
        wa.saved_cursor = pb.1;
        wa.saved_top = pb.2;
        wa.saved_leftcol = pb.3;
        wa.saved_cursors = pb.4;
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
        // The hard focus lock: while a focus-locked overlay is up — the bottom panel, or a
        // grabbing `nx.view` float — focus is pinned to its window. Every focus change
        // funnels through here (`<C-w>w`/`W` cycle, `<C-w>hjkl` directional,
        // `set_current_window` / `nvim_set_current_win`, mouse focus), so this one guard
        // makes them all inert. Opening is unaffected (the lock field is still `None` when
        // the overlay's window is first focused); the dismiss path clears the field before
        // restoring focus, so dismissal is permitted.
        if self.focus_lock_window().is_some_and(|w| w != id) {
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
        // Consume any parked mode unconditionally (so it can never go stale); it is
        // only *resumed* when the dock chord asked for it via `restore_mode_on_enter`
        // — an ordinary focus change lands in Normal as always.
        let resume = w.resume.take();
        let restore = std::mem::take(&mut self.restore_mode_on_enter);
        self.set_cur_buffer(buffer);
        self.cursor = cursor;
        self.top = top;
        self.leftcol = leftcol;
        match resume {
            // Re-enter the mode this window was left in (insert/visual/terminal),
            // before `clamp_cursor` below so an insert append-column survives.
            Some(r) if restore => self.reestablish_mode(r),
            _ => self.mode = Mode::Normal,
        }
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

    /// Re-enter a [`ResumeState`] mode parked on a window by the dock chord — the
    /// counterpart of [`Editor::dock_chord_intercept`]'s leave path. Insert/Replace
    /// open a fresh, correctly-grouped undo session at the saved resume column;
    /// Visual restores its anchor; Terminal simply re-arms job-mode forwarding (the
    /// buffer is still a live terminal). Runs inside [`Editor::enter_window`], with
    /// the target buffer already current and the cursor already at its saved spot.
    fn reestablish_mode(&mut self, r: ResumeState) {
        match r.mode {
            Mode::Insert | Mode::Replace => {
                self.mode = r.mode;
                self.cursor.col = r.col.min(self.line_len());
                // A fresh insert session: snapshot for undo and start the `".`
                // accumulator empty, so resumed typing groups and repeats cleanly.
                self.push_undo();
                self.snapshot_taken = true;
                self.insert_text.clear();
            }
            Mode::Visual | Mode::VisualLine => {
                self.mode = r.mode;
                self.visual_anchor = r.visual_anchor;
            }
            Mode::Terminal => self.mode = Mode::Terminal,
            // Defensive: only the non-Normal modes above are ever parked.
            _ => self.mode = Mode::Normal,
        }
    }

    /// Resize the *text viewport*. The client owns the screen layout and tells
    /// us only how tall the text area is (status/command lines are the client's
    /// own regions), so the whole height here is editable rows.
    pub fn resize(&mut self, width: usize, height: usize) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.relayout();
        self.ensure_visible();
        // A resize changes every window's visible height (and may clamp `top`), so
        // re-detect viewports for the `nx.decor` signal (`editor/decor.rs`).
        self.recompute_decor_dirty();
    }

    /// Whether the tabline is drawn right now, per `showtabline`: never at `0`,
    /// only with more than one tab at `1` (the default), always at `2`. The single
    /// gate both [`Editor::tabline_rows`] (the reserved row) and
    /// [`Editor::tab_labels`] (the projected labels) consult, so they never
    /// disagree.
    pub(crate) fn tabline_visible(&self) -> bool {
        self.tabline_visible_for(Layer::Main)
    }

    /// Whether `layer`'s own tabline is drawn right now, per `showtabline` applied
    /// to *that layer*'s tab count: never at `0`, only with more than one tab in
    /// the layer at `1` (the default), always (for an open layer) at `2`. Each
    /// region (main + each open dock) gates independently — a single-tab dock shows
    /// no tabline at the default, a 2-tab dock does. `false` for a closed dock.
    pub(crate) fn tabline_visible_for(&self, layer: Layer) -> bool {
        let Some(stack) = self.stack(layer) else {
            return false;
        };
        // A dock may override `showtabline` and/or carry a title (which forces its
        // strip on, unless the override is `0`); the main layer always follows the
        // global option.
        let (showtabline, has_title) = match layer {
            Layer::Dock(s) => {
                let opt = &self.dock_options[s.idx()];
                (
                    opt.showtabline.unwrap_or(self.options.showtabline),
                    !opt.title.is_empty(),
                )
            }
            Layer::Main => (self.options.showtabline, false),
        };
        match showtabline {
            0 => false,
            2 => true,
            _ => has_title || stack.tabs.len() > 1,
        }
    }

    /// Rows the tabline reserves at the top of the reported area: one when the
    /// tabline is shown ([`Editor::tabline_visible`]), zero otherwise. The client
    /// paints the tabline into this row and offsets the windows area below it —
    /// the top-of-frame analogue of the bottom panel.
    pub(crate) fn tabline_rows(&self) -> usize {
        usize::from(self.tabline_visible())
    }

    /// Rows `layer`'s own tabline reserves at the top of *its* region band: one when
    /// that layer's tabline is shown ([`Editor::tabline_visible_for`]), zero
    /// otherwise. A dock's tabline is the first row of its band (the tree lays out
    /// below it); the client paints it there. (Main's tabline is the global top row,
    /// counted by [`Editor::tabline_rows`] instead.)
    pub(crate) fn tabline_rows_for(&self, layer: Layer) -> usize {
        usize::from(self.tabline_visible_for(layer))
    }

    /// Whether a window paints its own per-window status row. A **float** never
    /// does by default (matching neovim — see the body), regardless of
    /// `laststatus`. A **tiled** window follows `laststatus`: never at `0`, only
    /// with ≥2 tiled windows at `1`, always at `2` (the default), and never at `3`
    /// (a single global status line replaces the per-window ones). The single gate
    /// the view projection ([`crate::view`]) and the scroll math
    /// ([`Editor::text_height`]) consult so the reserved text row, the cursor
    /// scrolling, and the client's paint never disagree.
    pub(crate) fn window_statusline_visible(&self, region: WindowRegion, floating: bool) -> bool {
        if floating {
            // A float carries no status line by default, matching neovim: its
            // `last_status` only walks the tiled frame tree, so a float's
            // status height stays 0 and its full inner height is content. (A
            // per-window opt-in could grow here later; nxvim has none yet.)
            return false;
        }
        // The dock's `'laststatus'` override (if set) wins for its windows, else the
        // global value decides — the per-region sibling of `tabline_visible_for`.
        let layer = layer_of_region(region);
        let laststatus = match layer {
            Layer::Dock(s) => self.dock_options[s.idx()]
                .laststatus
                .unwrap_or(self.options.laststatus),
            Layer::Main => self.options.laststatus,
        };
        match laststatus {
            0 | 3 => false,
            // `1`: only with ≥2 tiled windows *in this region's own layer* (a dock
            // counts its own tree, not the main area's).
            1 => self.layer_tree(layer).is_some_and(|t| t.tiled_count() > 1),
            _ => true,
        }
    }

    /// The [`WindowRegion`] of the focused window's layer — the region a focused-only
    /// caller (the scroll/cursor math) passes to [`window_statusline_visible`].
    pub(crate) fn focused_region(&self) -> WindowRegion {
        region_of_layer(self.focused_layer)
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
    pub(crate) fn global_statusline_rows(&self) -> usize {
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
        let bands = self.dock_bands();
        // The focused window's cursor cell, as an offset from its own rect's
        // top-left — what a `relative="cursor"` float anchors to. Guard against a
        // transient invalid `current` (mid-close, before the surviving window is
        // entered): `cursor_virtcol` reads the current window's buffer, which is
        // gone for that instant. Only floats consume this, so (0, 0) is harmless.
        // It is meaningful only for the *focused* tree (it reads the live cursor);
        // parked dock/main trees lay out with (0, 0).
        let cursor_off = if self.windows.windows.contains_key(&self.windows.current) {
            (
                self.cursor_virtcol(),
                self.cursor.line.saturating_sub(self.top),
            )
        } else {
            (0, 0)
        };
        let chrome = self.tabline_rows() + self.global_statusline_rows();
        // The middle band (left dock | main | right docks) height, and the main
        // tree's width — what's left after the docks and the global chrome.
        let mid_h = self
            .height
            .saturating_sub(bands.reserved_top())
            .saturating_sub(bands.reserved_bottom())
            .saturating_sub(chrome)
            .max(1);
        let main_w = self
            .width
            .saturating_sub(bands.reserved_left())
            .saturating_sub(bands.reserved_right())
            .max(1);
        // Lay out every open layer's tree at origin (0, 0) in its own region size;
        // each client maps the region to its absolute screen origin (the dock
        // bands it receives in the `View`). With no dock open this is exactly the
        // pre-dock layout: one main tree filling the full windows area.
        //
        // Each **dock** reserves the first row of its band for its own tabline (the
        // client paints it there and offsets the tree down a row); the tree gets the
        // remaining rows. Main has no per-region tabline row here — its tabline is
        // the global top row already excluded via `chrome`.
        let full_w = self.width;
        for layer in self.open_layers() {
            // Rows this dock's own tabline eats off the top of its band (0 for main).
            let dock_tab = match layer {
                Layer::Main => 0,
                dock => self.tabline_rows_for(dock),
            };
            let rect = match layer {
                Layer::Main => Rect {
                    x: 0,
                    y: 0,
                    width: main_w,
                    height: mid_h,
                },
                Layer::Dock(DockSide::Left) => Rect {
                    x: 0,
                    y: 0,
                    width: bands.left,
                    height: mid_h.saturating_sub(dock_tab).max(1),
                },
                Layer::Dock(DockSide::Right) => Rect {
                    x: 0,
                    y: 0,
                    width: bands.right,
                    height: mid_h.saturating_sub(dock_tab).max(1),
                },
                Layer::Dock(DockSide::Top) => Rect {
                    x: 0,
                    y: 0,
                    width: full_w,
                    height: bands.top.saturating_sub(dock_tab).max(1),
                },
                Layer::Dock(DockSide::Bottom) => Rect {
                    x: 0,
                    y: 0,
                    width: full_w,
                    height: bands.bottom.saturating_sub(dock_tab).max(1),
                },
            };
            let off = if layer == self.focused_layer {
                cursor_off
            } else {
                (0, 0)
            };
            if let Some(t) = self.layer_tree_mut(layer) {
                t.layout(rect, off);
            }
        }
        self.apply_panel_margin();
    }

    /// Shrink the open panel's window rect by its requested `margin` (a gap from the
    /// editor edges). The panel is an ordinary tiled bottom window — the layout has
    /// no inset concept — so this is a one-off post-layout adjustment of just that
    /// one window's rect; the vacated cells fall through to whatever is beneath,
    /// reading as a floating strip with a gap. `top` is ignored (the panel's top
    /// edge is set by its height, not an inset). A no-op when no panel is open or
    /// the margin is zero (the built-in listings).
    fn apply_panel_margin(&mut self) {
        let Some(p) = self.panel else { return };
        let m = p.margin;
        if m.left == 0 && m.right == 0 && m.bottom == 0 {
            return;
        }
        if let Some(w) = self.windows.windows.get_mut(&p.window) {
            let r = w.rect;
            w.rect = Rect {
                x: r.x + m.left,
                y: r.y,
                width: r.width.saturating_sub(m.left + m.right).max(1),
                height: r.height.saturating_sub(m.bottom).max(1),
            };
        }
    }

    /// The clamped per-side dock **content** extents (columns for left/right, rows
    /// for top/bottom; `0` where the dock is closed), shared by [`relayout`] and
    /// the [`crate::view::View`] projection so the core and every client agree on
    /// the geometry. Each open dock also reserves one separator cell toward the
    /// main area (see [`DockBands::reserved_left`] etc.). Sizes are clamped down so
    /// the main area always keeps at least one column and one row.
    pub(crate) fn dock_bands(&self) -> DockBands {
        let raw = |side: DockSide| {
            if self.dock_is_open(side) {
                self.dock_sizes[side.idx()].max(1)
            } else {
                0
            }
        };
        let reserved = |n: usize| if n > 0 { n + 1 } else { 0 };
        // Horizontal: left + right reservations must leave ≥1 column for main.
        let (mut left, mut right) = (raw(DockSide::Left), raw(DockSide::Right));
        while reserved(left) + reserved(right) >= self.width && (left > 0 || right > 0) {
            // The loop guard keeps at least one of the two positive, so the larger
            // side is always > 0 here.
            if right >= left {
                right = right.saturating_sub(1);
            } else {
                left = left.saturating_sub(1);
            }
        }
        // Vertical: top + bottom reservations plus the global chrome must leave ≥1
        // row for main.
        let chrome = self.tabline_rows() + self.global_statusline_rows();
        let (mut top, mut bottom) = (raw(DockSide::Top), raw(DockSide::Bottom));
        while reserved(top) + reserved(bottom) + chrome >= self.height && (top > 0 || bottom > 0) {
            if bottom >= top {
                bottom = bottom.saturating_sub(1);
            } else {
                top = top.saturating_sub(1);
            }
        }
        DockBands {
            left,
            right,
            top,
            bottom,
        }
    }
}

/// Map a window [`Layer`] to its render [`WindowRegion`].
fn region_of_layer(layer: Layer) -> WindowRegion {
    match layer {
        Layer::Main => WindowRegion::Main,
        Layer::Dock(DockSide::Left) => WindowRegion::DockLeft,
        Layer::Dock(DockSide::Right) => WindowRegion::DockRight,
        Layer::Dock(DockSide::Top) => WindowRegion::DockTop,
        Layer::Dock(DockSide::Bottom) => WindowRegion::DockBottom,
    }
}

/// The [`Layer`] a [`WindowRegion`] belongs to — the inverse of
/// [`region_of_layer`], so a projection that carries a region can look up the
/// region's dock-scoped options.
fn layer_of_region(region: WindowRegion) -> Layer {
    match region {
        WindowRegion::Main => Layer::Main,
        WindowRegion::DockLeft => Layer::Dock(DockSide::Left),
        WindowRegion::DockRight => Layer::Dock(DockSide::Right),
        WindowRegion::DockTop => Layer::Dock(DockSide::Top),
        WindowRegion::DockBottom => Layer::Dock(DockSide::Bottom),
    }
}

/// The per-side dock **content** extents (columns for left/right, rows for top/
/// bottom; `0` = closed), as computed by [`Editor::dock_bands`]. Each open dock
/// additionally reserves one separator cell between it and the main area — the
/// `reserved_*` accessors fold that in, and are what shrinks the main region and
/// what clients offset by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DockBands {
    pub left: usize,
    pub right: usize,
    pub top: usize,
    pub bottom: usize,
}

impl DockBands {
    fn reserved(n: usize) -> usize {
        if n > 0 {
            n + 1
        } else {
            0
        }
    }
    /// Columns the left dock reserves (content + one separator cell), or 0.
    pub fn reserved_left(&self) -> usize {
        Self::reserved(self.left)
    }
    /// Columns the right dock reserves (content + one separator cell), or 0.
    pub fn reserved_right(&self) -> usize {
        Self::reserved(self.right)
    }
    /// Rows the top dock reserves (content + one separator cell), or 0.
    pub fn reserved_top(&self) -> usize {
        Self::reserved(self.top)
    }
    /// Rows the bottom dock reserves (content + one separator cell), or 0.
    pub fn reserved_bottom(&self) -> usize {
        Self::reserved(self.bottom)
    }
}
