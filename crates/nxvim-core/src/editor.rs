//! The editor state machine: turns keys and ex-commands into buffer mutations.
//!
//! This is the rust-native analogue of neovim's `normal.c` / `ops.c` /
//! `edit.c` / `ex_docmd.c`. It is fully synchronous and owns no I/O beyond
//! reading/writing files through [`Buffer`]. The async server feeds it input
//! and reads back state; it never blocks.

use std::cmp::{max, min};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::buffer::{Buffer, EditBatch};
use crate::highlight::Highlights;
use crate::input::{Key, KeyCode};
use crate::mode::Mode;
use crate::options::{resolve_set, NumOp, Options, SetCmd, SetOp, WindowOptions};
use crate::search::SearchRegex;
use crate::unicode;
use crate::view::{PanelView, Separator, View};

/// Default content height of the bottom panel, in rows (vim's quickfix
/// default). The projection clamps it down so the text window keeps a row.
const PANEL_HEIGHT: usize = 10;

/// Cap on the retained `:messages` history. Older entries are dropped once the
/// log exceeds this, so a long session can't grow it without bound (vim likewise
/// caps `:messages`).
const MAX_MESSAGES: usize = 1000;

/// Lexically normalize a path — collapse `.`, `..`, and redundant separators —
/// **without touching the filesystem** (no symlink resolution, no existence
/// check), so the pure core stays free of blocking I/O. Enough to treat `./a`
/// and `a` as the same buffer for [`Editor::find_buffer_by_path`].
fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // The root's parent is the root; drop a leading/over-popped `..`
                // only past a real segment, else keep it (relative-path prefix).
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A cursor position within the current buffer (0-indexed line and column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    pub line: usize,
    pub col: usize,
}

/// Stable identifier for an open buffer. Monotonic and 1-based (buffer 1 is the
/// first file, or the initial `[No Name]`); an id is never reused once assigned,
/// matching vim's buffer numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BufferId(pub u64);

/// Stable identifier for a window — a viewport onto a buffer. Like [`BufferId`]
/// it is monotonic, 1-based, and never reused once assigned, matching neovim's
/// window handles. The first window is allocated at startup, bound to buffer 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(pub u64);

#[derive(Debug, Clone, Default)]
struct Register {
    text: String,
    linewise: bool,
}

/// A search match as whole-buffer byte offsets, `(start, end)` (end exclusive).
type MatchRange = (usize, usize);

/// Per visible row, the screen-column spans of every search match on that row
/// (the `Search`/`hlsearch` highlight). Empty inner vec for rows with no match.
pub(crate) type SearchSpans = Vec<Vec<(usize, usize)>>;
/// Per visible row, the single span the live `incsearch` preview rests on (the
/// `IncSearch` highlight), or `None`.
pub(crate) type IncSearchSpans = Vec<Option<(usize, usize)>>;

/// Which direction a `/` (forward) or `?` (backward) search runs in. Stored with
/// the last search so `n` repeats it and `N` inverts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchDir {
    Forward,
    Backward,
}

impl SearchDir {
    fn opposite(self) -> SearchDir {
        match self {
            SearchDir::Forward => SearchDir::Backward,
            SearchDir::Backward => SearchDir::Forward,
        }
    }

    /// The command-line prompt character (`/` forward, `?` backward).
    fn prefix(self) -> char {
        match self {
            SearchDir::Forward => '/',
            SearchDir::Backward => '?',
        }
    }
}

/// A search offset — the `e`/`s`/`b`/line suffix vim allows after the pattern
/// (`/pat/e`, `/pat/s-2`, `/pat/+3`). It repositions the cursor relative to the
/// match and, used as an operator motion, sets the motion's inclusiveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchOffset {
    /// No offset: land on the match start (exclusive motion).
    None,
    /// `s`/`b` start offset: `n` characters from the match start (exclusive).
    Start(isize),
    /// `e` end offset: `n` characters from the match's last char (inclusive).
    End(isize),
    /// A bare `[+-]n` line offset: `n` lines from the match's line (linewise).
    Line(isize),
}

impl SearchOffset {
    /// How a search resolves as an operator motion: `e` includes the match end,
    /// a line offset goes linewise, everything else stops short of the match.
    fn motion_kind(self) -> MotionKind {
        match self {
            SearchOffset::End(_) => MotionKind::Inclusive,
            SearchOffset::Line(_) => MotionKind::Linewise,
            _ => MotionKind::Exclusive,
        }
    }
}

/// Split a submitted search line into its pattern and trailing offset on the
/// **last unescaped** separator `sep` (`/` for a forward search, `?` for
/// backward), per vim's `/pat/e`, `/pat/+2` syntax. A `\`-escaped separator stays
/// in the pattern; with no separator the whole line is the pattern.
fn split_search_offset(line: &str, sep: char) -> (String, SearchOffset) {
    let chars: Vec<char> = line.chars().collect();
    let mut at = None;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2; // skip the escaped char
            continue;
        }
        if chars[i] == sep {
            at = Some(i);
        }
        i += 1;
    }
    match at {
        Some(p) => (
            chars[..p].iter().collect(),
            parse_offset(&chars[p + 1..].iter().collect::<String>()),
        ),
        None => (line.to_string(), SearchOffset::None),
    }
}

/// Parse the text after a search separator into a [`SearchOffset`]: `e`/`s`/`b`
/// (optionally `+n`/`-n`/`n`) are character offsets; a bare `[+-]n` is a line
/// offset; anything else is no offset.
fn parse_offset(s: &str) -> SearchOffset {
    let s = s.trim();
    let mut it = s.chars();
    match it.next() {
        Some('e') => SearchOffset::End(parse_signed(it.as_str()).unwrap_or(0)),
        Some('s') | Some('b') => SearchOffset::Start(parse_signed(it.as_str()).unwrap_or(0)),
        Some(c) if c == '+' || c == '-' || c.is_ascii_digit() => {
            parse_signed(s).map_or(SearchOffset::None, SearchOffset::Line)
        }
        _ => SearchOffset::None,
    }
}

/// Parse an optionally-signed magnitude. A lone `+`/`-` is `±1` (vim's `e+` means
/// `e+1`); an empty string is `None`.
fn parse_signed(s: &str) -> Option<isize> {
    let s = s.trim();
    let (sign, digits) = match s.strip_prefix('-') {
        Some(d) => (-1, d),
        None => (1, s.strip_prefix('+').unwrap_or(s)),
    };
    if digits.is_empty() {
        return (s == "+" || s == "-").then_some(sign);
    }
    digits.parse::<isize>().ok().map(|n| sign * n)
}

/// What the command line is editing: an `:` ex command, a `/`,`?` search, or a
/// scripted text prompt (`vim.ui.input`). One [`Mode::Command`] serves all three;
/// the kind decides the prompt label and what `<CR>` does. Set on entry, read on
/// submit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CmdlineKind {
    Ex,
    Search(SearchDir),
    /// A `vim.ui.input` prompt: `<CR>` hands the typed line to the waiting Lua
    /// callback (and `<Esc>` hands it `nil`), via [`Editor::prompt_results`]. The
    /// label is held in [`Editor::cmdline_prompt`].
    Prompt,
    /// A `vim.fn.confirm` button dialog: a single keypress matching a button
    /// accelerator (or `<CR>` → default, `<Esc>` → 0) resolves it, delivering the
    /// chosen 1-based index as a string through [`Editor::prompt_results`] (the
    /// same channel `Prompt` uses; the Lua side reads it back as a number). The
    /// rendered message + buttons are held in [`Editor::cmdline_prompt`].
    Confirm,
}

#[derive(Clone)]
struct Snapshot {
    text: ropey::Rope,
    cursor: Cursor,
    /// Undo-sequence number of the state this snapshot captures (see
    /// [`OpenBuffer::cur_seq`]), so undo/redo can tell when it has landed back
    /// on the last-saved state and clear `modified`.
    seq: u64,
}

/// A buffer as the editor holds it: the text [`Buffer`] plus the state vim keeps
/// with the buffer rather than the window — undo/redo history and, while the
/// buffer is not current, the last cursor/scroll position so switching back
/// restores the view.
struct OpenBuffer {
    buffer: Buffer,
    undo_stack: Vec<Snapshot>,
    redo_stack: Vec<Snapshot>,
    /// Monotonic id of the current text state. A fresh number is minted for each
    /// edit; undo/redo carry it on their snapshots. Compared against `saved_seq`
    /// to decide whether the buffer matches what's on disk. (Neovim's
    /// `b_u_seq_cur`.)
    cur_seq: u64,
    /// `cur_seq` as of the last write, or `None` once an edit has diverged from
    /// disk past the point any retained snapshot can return to. The buffer is
    /// `modified` exactly when `Some(cur_seq) != saved_seq`. (Neovim's
    /// `b_u_save_nr`.)
    saved_seq: Option<u64>,
    /// Source of the next `cur_seq`; only ever increments.
    next_seq: u64,
    /// Window position saved when this buffer stops being current; restored on
    /// switch-back. Meaningless while the buffer *is* current — the live position
    /// is then [`Editor::cursor`] / `Editor::top` / `Editor::leftcol`.
    saved_cursor: Cursor,
    saved_top: usize,
    saved_leftcol: usize,
}

impl OpenBuffer {
    fn new(buffer: Buffer) -> Self {
        OpenBuffer {
            buffer,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            // A freshly loaded buffer matches disk: state 0 is the saved state.
            cur_seq: 0,
            saved_seq: Some(0),
            next_seq: 1,
            saved_cursor: Cursor::default(),
            saved_top: 0,
            saved_leftcol: 0,
        }
    }
}

/// The set of open buffers, keyed by [`BufferId`]. A `BTreeMap` keeps iteration
/// id-sorted, which the buffer-list commands (`:ls`, `:bnext`) will rely on.
struct BufferStore {
    map: BTreeMap<BufferId, OpenBuffer>,
    /// Next id to hand out; only ever increases, so ids are never reused.
    next_id: u64,
}

impl BufferStore {
    /// A store seeded with a single buffer at id 1, returned alongside that id.
    fn with_one(buffer: Buffer) -> (Self, BufferId) {
        let id = BufferId(1);
        let mut map = BTreeMap::new();
        map.insert(id, OpenBuffer::new(buffer));
        (BufferStore { map, next_id: 2 }, id)
    }

    /// Add a buffer under a fresh id and return it.
    fn insert(&mut self, buffer: Buffer) -> BufferId {
        let id = BufferId(self.next_id);
        self.next_id += 1;
        self.map.insert(id, OpenBuffer::new(buffer));
        id
    }

    fn get(&self, id: BufferId) -> &OpenBuffer {
        self.map
            .get(&id)
            .expect("current buffer id is always valid")
    }

    fn get_mut(&mut self, id: BufferId) -> &mut OpenBuffer {
        self.map
            .get_mut(&id)
            .expect("current buffer id is always valid")
    }
}

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

/// A floating window's placement (`nvim_open_win`'s float `config`). Held on
/// [`Window::float`] (`None` for a tiled window); the [`WindowTree`] positions it
/// absolutely each layout from this config, on top of the tiled tree. `width`/
/// `height` are the **outer** box (border included); `row`/`col` offset the
/// `anchor` corner from the `relative` origin.
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
struct Window {
    buffer: BufferId,
    saved_cursor: Cursor,
    saved_top: usize,
    saved_leftcol: usize,
    rect: Rect,
    /// Window-local options (the number gutter). A split inherits these from the
    /// window it was split off, mirroring vim.
    options: WindowOptions,
    /// `Some` for a floating window (its placement), `None` for a tiled one.
    float: Option<FloatConfig>,
}

/// The orientation of a split: `Horizontal` stacks children top-to-bottom
/// (`:split` / `<C-w>s`), `Vertical` places them left-to-right (`:vsplit` /
/// `<C-w>v`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplitDir {
    Horizontal,
    Vertical,
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
/// tree arranging them, the `current` (focused) window, the next id to hand out,
/// and the separators the last [`WindowTree::layout`] produced. Mirrors
/// [`BufferStore`]; with one window the tree is a single `Leaf`, there are no
/// separators, and `current` always resolves.
struct WindowTree {
    windows: BTreeMap<WindowId, Window>,
    root: Node,
    current: WindowId,
    next_id: u64,
    /// Borders between splits, recomputed on every [`WindowTree::layout`]. Empty
    /// with a single window.
    separators: Vec<Separator>,
    /// Floating window ids — those in `windows` with `float.is_some()`, which no
    /// `Leaf` in `root` references. Kept sorted by `(zindex, id)` so iterating it
    /// yields bottom-to-top paint order. Empty until the first `nvim_open_win`
    /// float.
    floats: Vec<WindowId>,
}

impl WindowTree {
    /// A tree seeded with a single window (id 1) bound to `buffer` and focused,
    /// the window analogue of [`BufferStore::with_one`].
    fn with_one(buffer: BufferId) -> Self {
        let id = WindowId(1);
        let mut windows = BTreeMap::new();
        windows.insert(
            id,
            Window {
                buffer,
                saved_cursor: Cursor::default(),
                saved_top: 0,
                saved_leftcol: 0,
                rect: Rect::default(),
                options: WindowOptions::default(),
                float: None,
            },
        );
        WindowTree {
            windows,
            root: Node::Leaf(id),
            current: id,
            next_id: 2,
            separators: Vec::new(),
            floats: Vec::new(),
        }
    }

    fn get(&self, id: WindowId) -> &Window {
        self.windows
            .get(&id)
            .expect("current window id is always valid")
    }

    fn get_mut(&mut self, id: WindowId) -> &mut Window {
        self.windows
            .get_mut(&id)
            .expect("current window id is always valid")
    }

    /// The focused window.
    fn cur(&self) -> &Window {
        self.get(self.current)
    }

    /// The focused window, mutably.
    fn cur_mut(&mut self) -> &mut Window {
        let id = self.current;
        self.get_mut(id)
    }

    /// All window ids in layout order (the `nvim_list_wins` order).
    fn leaves(&self) -> Vec<WindowId> {
        let mut out = Vec::new();
        self.root.collect_leaves(&mut out);
        out
    }

    /// How many windows are open. Always ≥ 1.
    fn count(&self) -> usize {
        self.windows.len()
    }

    /// Mint a fresh, never-reused window id.
    fn alloc_id(&mut self) -> WindowId {
        let id = WindowId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Re-sort the [float](Self::floats) list bottom-to-top by `(zindex, id)`, so
    /// iterating it is paint order and id breaks a zindex tie by creation order.
    /// Called after a float is added or its zindex changes.
    fn sort_floats(&mut self) {
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
    fn layout(&mut self, total: Rect, cursor_off: (usize, usize)) {
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
    fn position_floats(&mut self, total: Rect, cursor_off: (usize, usize)) {
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
/// then clamps the box to stay fully on-screen. `width`/`height` are the outer
/// box; a float larger than `bounds` pins to the top-left rather than shrinking.
fn place_float(origin: Rect, bounds: Rect, cfg: &FloatConfig) -> Rect {
    let w = cfg.width.max(1);
    let h = cfg.height.max(1);
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

/// A directional window-focus move (`<C-w>h/j/k/l`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WinDir {
    Left,
    Down,
    Up,
    Right,
}

/// A `<C-w>` window command: the second key after the `<C-w>` prefix, resolved by
/// [`parse_step`] and applied in [`Editor::execute`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowCmd {
    /// `<C-w>s` (horizontal) / `<C-w>v` (vertical) — split the focused window.
    Split(SplitDir),
    /// `<C-w>h/j/k/l` — move focus to the window in that direction.
    FocusDir(WinDir),
    /// `<C-w>w` (forward) / `<C-w>W` (backward) — cyclic focus.
    FocusCycle(bool),
    /// `<C-w>c` — close the focused window (refuses the last one).
    Close,
    /// `<C-w>o` — keep only the focused window.
    Only,
    /// `<C-w>q` — `:q`: close the focused window, or quit if it is the last.
    Quit,
    /// `<C-w>=` — equalize every window's size.
    Equalize,
    /// `<C-w>+` (grow) / `<C-w>-` (shrink) — change the focused window's height
    /// by the count (default 1).
    ResizeHeight(bool),
    /// `<C-w>>` (grow) / `<C-w><` (shrink) — change the focused window's width by
    /// the count (default 1).
    ResizeWidth(bool),
    /// `<C-w>_` — maximize the focused window's height.
    MaxHeight,
    /// `<C-w>|` — maximize the focused window's width.
    MaxWidth,
}

/// Resolve the key *after* `<C-w>` into a [`WindowCmd`], or `None` for a key that
/// is not a window command.
fn window_command(key: Key) -> Option<WindowCmd> {
    // A bare `<C-w><C-w>` is the same as `<C-w>w` (cyclic focus) in vim.
    if key.ctrl {
        return match key.code {
            KeyCode::Char('w') => Some(WindowCmd::FocusCycle(true)),
            _ => None,
        };
    }
    Some(match key.as_char()? {
        's' => WindowCmd::Split(SplitDir::Horizontal),
        'v' => WindowCmd::Split(SplitDir::Vertical),
        'w' => WindowCmd::FocusCycle(true),
        'W' => WindowCmd::FocusCycle(false),
        'h' => WindowCmd::FocusDir(WinDir::Left),
        'j' => WindowCmd::FocusDir(WinDir::Down),
        'k' => WindowCmd::FocusDir(WinDir::Up),
        'l' => WindowCmd::FocusDir(WinDir::Right),
        'c' => WindowCmd::Close,
        'o' => WindowCmd::Only,
        'q' => WindowCmd::Quit,
        '=' => WindowCmd::Equalize,
        '+' => WindowCmd::ResizeHeight(true),
        '-' => WindowCmd::ResizeHeight(false),
        '>' => WindowCmd::ResizeWidth(true),
        '<' => WindowCmd::ResizeWidth(false),
        '_' => WindowCmd::MaxHeight,
        '|' => WindowCmd::MaxWidth,
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy)]
enum MotionKind {
    Exclusive,
    Inclusive,
    Linewise,
}

/// How a motion places the cursor when used as plain movement (not as an
/// operator's range). This is what drives vim's `curswant` column memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveAxis {
    /// Horizontal move: the resulting column becomes the new desired column.
    Horizontal,
    /// `$`/`End`: stick to end-of-line until a horizontal move clears it.
    EndOfLine,
    /// `gg`/`G`/etc.: jump to a line's first non-blank; resets desired column.
    LineAnchor,
    /// `j`/`k`: change line but keep the remembered desired column.
    VerticalKeep,
}

struct MotionResult {
    target: usize,
    kind: MotionKind,
    axis: MoveAxis,
}

impl MotionResult {
    /// A horizontal in-line motion (`h`/`l`/`0`/`^`/`w`/`b`/`e`/search-operator),
    /// with the caller-chosen exclusive/inclusive `kind`.
    fn horizontal(target: usize, kind: MotionKind) -> Self {
        Self {
            target,
            kind,
            axis: MoveAxis::Horizontal,
        }
    }

    /// The common exclusive horizontal motion (`h`, `l`, `0`, `^`, `w`, `b`).
    fn exclusive(target: usize) -> Self {
        Self::horizontal(target, MotionKind::Exclusive)
    }

    /// An inclusive horizontal motion (`e`, and `cw` acting like `ce`).
    fn inclusive(target: usize) -> Self {
        Self::horizontal(target, MotionKind::Inclusive)
    }

    /// A linewise motion to the start of `target`'s line, with the given `axis`
    /// (`VerticalKeep` for `j`/`k`, `LineAnchor` for `gg`/`G`/doubled operators).
    fn linewise(target: usize, axis: MoveAxis) -> Self {
        Self {
            target,
            kind: MotionKind::Linewise,
            axis,
        }
    }
}

// ===== Normal / visual command grammar =====================================
//
// The normal/visual key sequence is parsed in two clean halves. [`parse_step`]
// (pure: no buffer, no `&mut`) is the *grammar* — it decides whether a key
// extends, completes, or aborts a command, and emits a typed [`ResolvedCommand`]
// describing what to do. [`Editor::execute`] is the *effect* — it applies that
// command to the buffer through the existing helpers. The typed motion / object
// / find enums below are the contract between the two: a new built-in is a new
// variant the compiler forces into both the parse arm and the effect arm, so the
// two can never silently drift (this is what lets the keymap matcher reuse
// `parse_step` as a read-only command oracle without mirroring the executor).

/// A normal/visual cursor motion. The motion *alphabet* (which keys are motions)
/// lives in [`classify_motion`]; where each motion *lands* lives in
/// [`Editor::resolve_motion`]. Note `w`/`W`, `b`/`B`, `e`/`E` collapse to one
/// variant each — nxvim does not yet implement `WORD` motions, so the big-word
/// keys behave identically to their small-word counterparts (preserved here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Motion {
    Left,                         // h, <Left>, <BS>
    Right,                        // l, <Right>, <Space>
    LineStart,                    // 0, <Home>
    FirstNonBlank,                // ^
    LineEnd,                      // $, <End>
    Down,                         // j, <Down>
    Up,                           // k, <Up>
    GotoLine,                     // G  (count = target line, default last)
    GotoTop,                      // gg (count = target line, default first)
    Word,                         // w / W
    BackWord,                     // b / B
    EndWord,                      // e / E
    Find(FindKind, char),         // f/t/F/T {char}
    FindRepeat { reverse: bool }, // ; (same) / , (reversed)
}

/// The four find-char motions. `f`/`t` search forward, `F`/`T` backward; `t`/`T`
/// stop one grapheme short of the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FindKind {
    Find,     // f
    Till,     // t
    FindBack, // F
    TillBack, // T
}

impl FindKind {
    fn from_key(c: char) -> Option<FindKind> {
        Some(match c {
            'f' => FindKind::Find,
            't' => FindKind::Till,
            'F' => FindKind::FindBack,
            'T' => FindKind::TillBack,
            _ => return None,
        })
    }

    /// `f`/`t` go forward (and feed an operator inclusively); `F`/`T` go backward.
    fn forward(self) -> bool {
        matches!(self, FindKind::Find | FindKind::Till)
    }

    /// `t`/`T` stop short of the target ("till"); `f`/`F` land on it.
    fn till(self) -> bool {
        matches!(self, FindKind::Till | FindKind::TillBack)
    }

    /// The direction-flipped kind used by `,` (and by `;` after a `,`): f↔F, t↔T.
    fn reversed(self) -> FindKind {
        match self {
            FindKind::Find => FindKind::FindBack,
            FindKind::FindBack => FindKind::Find,
            FindKind::Till => FindKind::TillBack,
            FindKind::TillBack => FindKind::Till,
        }
    }
}

/// A text-object kind (`iw`, `a(`, `i"`, `ap`, …). The object *alphabet* lives
/// in [`ObjectKind::from_key`]; the range search lives in
/// [`Editor::text_object_range`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObjectKind {
    Word(bool),       // w (false) / W (true)
    Pair(char, char), // (), {}, [], <>  (incl. b/B aliases)
    Quote(char),      // " ' `
    Sentence,         // s
    Paragraph,        // p
}

impl ObjectKind {
    fn from_key(c: char) -> Option<ObjectKind> {
        Some(match c {
            'w' => ObjectKind::Word(false),
            'W' => ObjectKind::Word(true),
            '(' | ')' | 'b' => ObjectKind::Pair('(', ')'),
            '{' | '}' | 'B' => ObjectKind::Pair('{', '}'),
            '[' | ']' => ObjectKind::Pair('[', ']'),
            '<' | '>' => ObjectKind::Pair('<', '>'),
            '"' | '\'' | '`' => ObjectKind::Quote(c),
            's' => ObjectKind::Sentence,
            'p' => ObjectKind::Paragraph,
            _ => return None,
        })
    }
}

/// A terminal single-key normal/visual command (everything that is neither a
/// motion, an operator, a text object, nor `r{char}`). Classified in
/// [`parse_command`], applied in [`Editor::execute_normal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NormalCmd {
    InsertBefore,                                    // i
    InsertLineStart,                                 // I
    InsertAfter,                                     // a
    InsertLineEnd,                                   // A
    OpenBelow,                                       // o
    OpenAbove,                                       // O
    DeleteUnder,                                     // x
    DeleteBefore,                                    // X
    DeleteToEol,                                     // D
    ChangeToEol,                                     // C
    SubstituteChar,                                  // s
    EnterReplace,                                    // R
    PasteAfter,                                      // p
    PasteBefore,                                     // P
    Undo,                                            // u
    Redo,                                            // <C-r>
    Join,                                            // J
    ToggleCase,                                      // ~
    EnterVisual,                                     // v
    EnterVisualLine,                                 // V
    EnterCommand,                                    // :
    EnterSearch(SearchDir),                          // / ?
    SearchNext,                                      // n
    SearchPrev,                                      // N
    SearchWord { dir: SearchDir, whole_word: bool }, // * # (g* g# drop boundaries)
    ScrollHalf(bool),                                // <C-d> (true) / <C-u> (false)
    ScrollPage(bool),                                // <C-f> (true) / <C-b> (false)
    AltBuffer,                                       // <C-^> / <C-6>
}

/// The stage of a partially-typed command — what the *next* key means. The
/// `g`-prefix, find-char, replace, and text-object sub-states were eight
/// scattered `Editor` booleans/options; they are one enum now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Stage {
    /// Accumulating a count and/or awaiting a command, operator, or motion.
    #[default]
    Start,
    /// Saw a lone `g`; the next key may complete `gg`/`g*`/`g#`.
    GPending,
    /// Saw `f`/`t`/`F`/`T`; the next key is the target char.
    FindPending(FindKind),
    /// Saw `r`; the next key is the replacement char.
    ReplacePending,
    /// Saw `i`/`a` (operator-pending or visual); the next key is the object kind.
    TextObjectPending(char),
    /// Saw the `<C-w>` window prefix; the next key is the window command.
    WindowPending,
}

/// The accumulated, not-yet-complete normal/visual command — one value in place
/// of the old scattered `count`/`op_count`/`operator`/`gpending`/… fields.
#[derive(Debug, Clone, Default)]
struct PendingCommand {
    /// Count typed after any operator (`d`**2**`w`), or the sole count (`3j`).
    count: Option<usize>,
    /// Count typed before an operator (`2`d`w`), stashed when the operator armed.
    op_count: Option<usize>,
    /// Pending operator (`d`/`c`/`y`) awaiting its motion / text object.
    operator: Option<char>,
    /// What the next key continues; see [`Stage`].
    stage: Stage,
}

/// A fully-resolved normal/visual command, ready for [`Editor::execute`].
enum ResolvedCommand {
    /// A motion — plain movement, or (when `pending.operator` is set) its range.
    Motion(Motion),
    /// A doubled operator over `count` lines (`dd`/`cc`/`yy`).
    DoubledOperator(char),
    /// An operator awaiting a search motion (`d/`, `c?`): open the search prompt.
    OperatorSearch { op: char, dir: SearchDir },
    /// A text object in operator-pending or visual mode (`diw`, `va(`).
    TextObject { ia: char, kind: ObjectKind },
    /// `r{char}`.
    Replace(char),
    /// A visual-mode operator on the current selection (`d`/`y`/`c`).
    VisualOperate(char),
    /// A terminal single-key command (insert, paste, scroll, …).
    Normal(NormalCmd),
    /// A `<C-w>` window command (split, focus, close, …).
    Window(WindowCmd),
}

/// What [`parse_step`] decides for `(pending, key)`. Pure: no buffer, no
/// mutation — the matcher's command oracle folds over this same function.
enum ParseStep {
    /// Incomplete: keep accumulating with this updated pending state.
    Prefix(PendingCommand),
    /// A whole command, ready to execute.
    Complete(ResolvedCommand),
    /// `<Esc>`: reset all pending state, and leave visual mode.
    Cancel,
    /// Reset all pending state (a dead-end key / `r`-cancel); mode unchanged.
    Reset,
    /// An object/find key with no match: in visual keep the selection (and any
    /// count), else reset — mirroring the old find / text-object abort paths.
    AbortObject,
}

/// Map a key to its [`Motion`], or `None` if it is not a (g-free) motion key.
/// This is the motion alphabet; [`Editor::resolve_motion`] is its counterpart
/// that computes where each lands. `f`/`t`/`F`/`T` are not here — they open a
/// [`Stage::FindPending`] first — and `gg` is handled by the g-prefix stage.
fn classify_motion(key: Key) -> Option<Motion> {
    let ch = key.as_char();
    Some(match (key.code, ch) {
        (KeyCode::Left, _) | (_, Some('h')) | (KeyCode::Backspace, _) => Motion::Left,
        (KeyCode::Right, _) | (_, Some('l')) | (_, Some(' ')) => Motion::Right,
        (_, Some('0')) | (KeyCode::Home, _) => Motion::LineStart,
        (_, Some('^')) => Motion::FirstNonBlank,
        (_, Some('$')) | (KeyCode::End, _) => Motion::LineEnd,
        (KeyCode::Down, _) | (_, Some('j')) | (_, Some('\r')) => Motion::Down,
        (KeyCode::Up, _) | (_, Some('k')) => Motion::Up,
        (_, Some('G')) => Motion::GotoLine,
        (_, Some('w')) | (_, Some('W')) => Motion::Word,
        (_, Some('b')) | (_, Some('B')) => Motion::BackWord,
        (_, Some('e')) | (_, Some('E')) => Motion::EndWord,
        (_, Some(';')) => Motion::FindRepeat { reverse: false },
        (_, Some(',')) => Motion::FindRepeat { reverse: true },
        _ => return None,
    })
}

/// THE normal/visual grammar — the single source of truth shared by the editor's
/// executor and (in a later phase) the keymap matcher's oracle. Pure: it reads
/// only `mode`, the accumulated `pending`, and `key`, and never touches the
/// buffer. Mirrors, arm for arm, the dispatch the old `handle_normal` /
/// `handle_normal_command` performed inline.
fn parse_step(mode: Mode, pending: &PendingCommand, key: Key) -> ParseStep {
    use ParseStep::*;

    // `r{char}` is checked before `<Esc>` (as in `handle_normal`): `r<Esc>` (or a
    // non-char key) cancels with no replacement and no mode change.
    if let Stage::ReplacePending = pending.stage {
        return match key.as_char() {
            Some(c) => Complete(ResolvedCommand::Replace(c)),
            None => Reset,
        };
    }

    // `<Esc>` cancels pending state and leaves visual mode — ahead of the find /
    // text-object argument stages, exactly as the old top-level branch was.
    if key.code == KeyCode::Esc {
        return Cancel;
    }

    // Argument stages: the next key is consumed as data, not re-parsed.
    match pending.stage {
        Stage::FindPending(kind) => {
            return match key.as_char() {
                Some(target) => Complete(ResolvedCommand::Motion(Motion::Find(kind, target))),
                None => AbortObject,
            };
        }
        Stage::TextObjectPending(ia) => {
            return match key.as_char().and_then(ObjectKind::from_key) {
                Some(kind) => Complete(ResolvedCommand::TextObject { ia, kind }),
                None => AbortObject,
            };
        }
        Stage::WindowPending => {
            return match window_command(key) {
                Some(cmd) => Complete(ResolvedCommand::Window(cmd)),
                None => Reset,
            };
        }
        Stage::Start | Stage::GPending => {}
        Stage::ReplacePending => unreachable!("handled above"),
    }

    // Count accumulation (Start and GPending only). `0` is a motion unless a
    // count is already building.
    if let Some(c) = key.as_char() {
        if c.is_ascii_digit() && !(c == '0' && pending.count.is_none()) {
            let d = c as usize - '0' as usize;
            let mut next = pending.clone();
            next.count = Some(pending.count.unwrap_or(0) * 10 + d);
            return Prefix(next);
        }
    }

    let gpending = pending.stage == Stage::GPending;

    // `g` prefix: a lone `g` arms it; a second `g` is `gg`.
    if gpending {
        if key.as_char() == Some('g') {
            return Complete(ResolvedCommand::Motion(Motion::GotoTop));
        }
    } else if key.as_char() == Some('g') {
        let mut next = pending.clone();
        next.stage = Stage::GPending;
        return Prefix(next);
    }

    // `i`/`a` introduce a text object when an operator is pending or in visual.
    if pending.operator.is_some() || mode.is_visual() {
        if let Some(c @ ('i' | 'a')) = key.as_char() {
            let mut next = pending.clone();
            next.stage = Stage::TextObjectPending(c);
            return Prefix(next);
        }
    }

    // `f`/`t`/`F`/`T` begin a find-char motion (the target follows).
    if let Some(kind) = key.as_char().and_then(FindKind::from_key) {
        let mut next = pending.clone();
        next.stage = Stage::FindPending(kind);
        return Prefix(next);
    }

    // Direct motions — but while g-pending only `gg` (handled above) is a motion,
    // matching `resolve_motion`'s old `if self.gpending { … }` short-circuit.
    if !gpending {
        if let Some(m) = classify_motion(key) {
            return Complete(ResolvedCommand::Motion(m));
        }
    }

    parse_command(mode, pending, key, gpending)
}

/// The terminal / operator-continuation half of [`parse_step`]: everything that
/// is not a count, g-prefix, text-object intro, find intro, or direct motion.
/// `gpending` only affects whether `*`/`#` keep word boundaries.
fn parse_command(mode: Mode, pending: &PendingCommand, key: Key, gpending: bool) -> ParseStep {
    use NormalCmd as N;
    use ParseStep::*;
    use ResolvedCommand as RC;

    // With an operator pending only a doubled operator, a search-motion hand-off,
    // or a cancel reaches here (motions were resolved just above).
    if let Some(op) = pending.operator {
        return match key.as_char() {
            Some(c) if c == op => Complete(RC::DoubledOperator(op)),
            Some('/') => Complete(RC::OperatorSearch {
                op,
                dir: SearchDir::Forward,
            }),
            Some('?') => Complete(RC::OperatorSearch {
                op,
                dir: SearchDir::Backward,
            }),
            _ => Reset,
        };
    }

    // Ctrl-keyed scrolling, redo, the alternate-buffer toggle, and the `<C-w>`
    // window-command prefix.
    if key.ctrl {
        // `<C-w>` opens the window-command stage (normal mode only — in visual it
        // is left unbound, as in vim). The second key resolves the command.
        if key.code == KeyCode::Char('w') && !mode.is_visual() {
            let mut next = pending.clone();
            next.stage = Stage::WindowPending;
            return Prefix(next);
        }
        return match key.code {
            KeyCode::Char('d') => Complete(RC::Normal(N::ScrollHalf(true))),
            KeyCode::Char('u') => Complete(RC::Normal(N::ScrollHalf(false))),
            KeyCode::Char('f') => Complete(RC::Normal(N::ScrollPage(true))),
            KeyCode::Char('b') => Complete(RC::Normal(N::ScrollPage(false))),
            KeyCode::Char('r') => Complete(RC::Normal(N::Redo)),
            KeyCode::Char('^') | KeyCode::Char('6') => Complete(RC::Normal(N::AltBuffer)),
            _ => Reset,
        };
    }

    // PageDown / PageUp default to a half-page scroll, like <C-d> / <C-u>.
    match key.code {
        KeyCode::PageDown => return Complete(RC::Normal(N::ScrollHalf(true))),
        KeyCode::PageUp => return Complete(RC::Normal(N::ScrollHalf(false))),
        _ => {}
    }

    let c = match key.as_char() {
        Some(c) => c,
        None => return Reset,
    };

    // Visual-mode operators act on the selection immediately.
    if mode.is_visual() {
        match c {
            'd' | 'x' => return Complete(RC::VisualOperate('d')),
            'y' => return Complete(RC::VisualOperate('y')),
            'c' | 's' => return Complete(RC::VisualOperate('c')),
            'v' => return Complete(RC::Normal(N::EnterVisual)),
            'V' => return Complete(RC::Normal(N::EnterVisualLine)),
            ':' => return Complete(RC::Normal(N::EnterCommand)),
            _ => {}
        }
    }

    match c {
        'i' => Complete(RC::Normal(N::InsertBefore)),
        'I' => Complete(RC::Normal(N::InsertLineStart)),
        'a' => Complete(RC::Normal(N::InsertAfter)),
        'A' => Complete(RC::Normal(N::InsertLineEnd)),
        'o' => Complete(RC::Normal(N::OpenBelow)),
        'O' => Complete(RC::Normal(N::OpenAbove)),
        'x' => Complete(RC::Normal(N::DeleteUnder)),
        'X' => Complete(RC::Normal(N::DeleteBefore)),
        'D' => Complete(RC::Normal(N::DeleteToEol)),
        'C' => Complete(RC::Normal(N::ChangeToEol)),
        's' => Complete(RC::Normal(N::SubstituteChar)),
        'R' => Complete(RC::Normal(N::EnterReplace)),
        'd' | 'c' | 'y' => {
            // Begin an operator (prefix): move count → op_count, drop g-pending.
            let mut next = pending.clone();
            next.operator = Some(c);
            next.op_count = pending.count;
            next.count = None;
            next.stage = Stage::Start;
            Prefix(next)
        }
        'r' => {
            let mut next = pending.clone();
            next.stage = Stage::ReplacePending;
            Prefix(next)
        }
        'p' => Complete(RC::Normal(N::PasteAfter)),
        'P' => Complete(RC::Normal(N::PasteBefore)),
        'u' => Complete(RC::Normal(N::Undo)),
        'J' => Complete(RC::Normal(N::Join)),
        '~' => Complete(RC::Normal(N::ToggleCase)),
        'v' => Complete(RC::Normal(N::EnterVisual)),
        'V' => Complete(RC::Normal(N::EnterVisualLine)),
        ':' => Complete(RC::Normal(N::EnterCommand)),
        '/' => Complete(RC::Normal(N::EnterSearch(SearchDir::Forward))),
        '?' => Complete(RC::Normal(N::EnterSearch(SearchDir::Backward))),
        'n' => Complete(RC::Normal(N::SearchNext)),
        'N' => Complete(RC::Normal(N::SearchPrev)),
        '*' => Complete(RC::Normal(N::SearchWord {
            dir: SearchDir::Forward,
            whole_word: !gpending,
        })),
        '#' => Complete(RC::Normal(N::SearchWord {
            dir: SearchDir::Backward,
            whole_word: !gpending,
        })),
        _ => Reset,
    }
}

/// How a key run relates to the normal/visual command grammar, as seen by the
/// keymap matcher's disambiguation oracle ([`command_status`]). Because it is a
/// fold over the very same [`parse_step`] the executor runs, "is this a complete
/// built-in?" can never drift from what actually executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandStatus {
    /// The run is a whole number of finished commands — it ends exactly on a
    /// command boundary (e.g. `gg`, `dd`, `fx`).
    Complete,
    /// The run ends mid-command; more keys could still complete it (e.g. a lone
    /// `g`, or `f` awaiting its target char).
    Prefix,
    /// No command runs this way — a dead-end key, a cancel, or an aborted
    /// find/text-object argument.
    Invalid,
}

/// Classify a key run against the normal/visual command grammar for `mode` by
/// folding [`parse_step`] from a clean command boundary. This is the keymap
/// matcher's **read-only** oracle: it consults the exact grammar the executor
/// runs (never a mirror), so a built-in can never silently lag behind a colliding
/// user mapping for want of being recognized.
///
/// Fold rule: start from a default [`PendingCommand`]; for each key call
/// `parse_step`. On `Prefix(p)` carry `p` forward; on `Complete(_)` **reset to a
/// fresh default** and keep folding the remainder (so a run of *several* finished
/// commands still ends clean); on any cancel/reset/abort short-circuit to
/// `Invalid`. The run is [`Complete`](CommandStatus::Complete) iff it ends on a
/// boundary (nothing carried) and [`Prefix`](CommandStatus::Prefix) iff it ends
/// mid-command.
pub fn command_status(mode: Mode, keys: &[Key]) -> CommandStatus {
    let mut pending = PendingCommand::default();
    let mut at_boundary = true;
    for &key in keys {
        match parse_step(mode, &pending, key) {
            ParseStep::Prefix(p) => {
                pending = p;
                at_boundary = false;
            }
            ParseStep::Complete(_) => {
                pending = PendingCommand::default();
                at_boundary = true;
            }
            ParseStep::Cancel | ParseStep::Reset | ParseStep::AbortObject => {
                return CommandStatus::Invalid;
            }
        }
    }
    if at_boundary {
        CommandStatus::Complete
    } else {
        CommandStatus::Prefix
    }
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

/// A bottom-docked, read-only, navigable panel — nxvim's home for multi-line
/// output like `:messages` and `:ls`. It is **not** a vim window (there is
/// still exactly one text window); it is a transient overlay that grabs focus
/// while open and is dismissed with `q`/`Q`/`<Esc>` (or a click on its `[X]`).
///
/// While a panel is open, [`Editor::input`] routes every key here instead of to
/// the buffer, so the usual vertical motions (`j`/`k`/`gg`/`G`/`<C-d>`/`<C-u>`)
/// scroll the panel rather than the text.
#[derive(Clone)]
struct Panel {
    /// Label shown in the title bar (e.g. `Messages`, `Buffers`).
    title: String,
    /// The full content; the visible slice is `lines[top..top + height]`.
    lines: Vec<String>,
    /// Cursor line within `lines`.
    cursor: usize,
    /// First visible line of `lines` (vertical scroll within the panel).
    top: usize,
    /// Requested content height; [`Editor::panel_rows`] clamps it so the text
    /// window always keeps at least one row.
    height: usize,
    /// `gg` is two keys; the first `g` arms this.
    gpending: bool,
    /// Whether `<CR>` on a line emits a select event (drained into the scripting
    /// `on_select` callback / RPC notification). Built-in viewer panels opt out,
    /// so a stale handler never fires on them.
    wants_select: bool,
    /// Per-line jump target `(path, line, col)` for a **navigable** panel (a
    /// location list, e.g. LSP references): `<CR>` on a line with a target jumps
    /// there via [`Editor::jump_to`] instead of firing a select event. Indexed in
    /// lockstep with `lines`; a missing/`None` entry leaves that line
    /// non-navigable. Empty for an ordinary panel. Because it lives in the panel,
    /// it travels with the `:panelopen` snapshot — so a reopened list still jumps.
    targets: Vec<Option<(PathBuf, usize, usize)>>,
}

/// Whitespace that advances the virtual column from `start` to `target`
/// (`target >= start`), the fill an inserted `<Tab>` lays down. With `expandtab`
/// it is all spaces; otherwise real tabs that jump to each `tabstop` boundary,
/// then spaces for any remainder past the last boundary — vim's tab/space mix.
fn fill_indent(start: usize, target: usize, tabstop: usize, expandtab: bool) -> String {
    if expandtab {
        return " ".repeat(target - start);
    }
    let mut s = String::new();
    let mut col = start;
    loop {
        let next_tab = col - (col % tabstop) + tabstop;
        if next_tab <= target {
            s.push('\t');
            col = next_tab;
        } else {
            break;
        }
    }
    s.push_str(&" ".repeat(target - col));
    s
}

/// Word-wrap `text` into rows no wider than `width` screen cells, for the bottom
/// [`Panel`] (nxvim's only multi-line, wrap-able surface — long hover docs,
/// messages, and location-list rows). Breaks after the last space that fits so
/// words stay whole, hard-breaking a run longer than `width`; an empty line
/// yields a single empty row so it still occupies one. Width is counted in screen
/// cells (tabs and wide chars via [`unicode::byte_at_virtcol`]), matching how the
/// panel is painted, so a wrapped row never overflows its column.
fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut rows = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        // Bytes of `rest` that fit in `width` cells (a grapheme boundary).
        let mut take = unicode::byte_at_virtcol(rest, width, unicode::TABSTOP);
        if take == 0 {
            // A single grapheme wider than `width`: take it whole so we progress.
            take = unicode::next_grapheme(rest, 0).max(1);
        }
        if take >= rest.len() {
            rows.push(rest.to_string());
            break;
        }
        match rest[..take].rfind(' ') {
            // Break after the last space that fits (dropped), keeping words whole.
            Some(sp) if sp > 0 => {
                rows.push(rest[..sp].trim_end().to_string());
                rest = &rest[sp + 1..];
            }
            // No usable space: hard-break mid-run at the width.
            _ => {
                rows.push(rest[..take].to_string());
                rest = &rest[take..];
            }
        }
    }
    rows
}

/// The complete editor state: the open buffers plus the single window's state.
pub struct Editor {
    /// All open buffers, keyed by id. The current buffer is *derived* from the
    /// focused window ([`Editor::cur_buffer`]); reach its text model through
    /// [`Editor::buffer`].
    buffers: BufferStore,
    /// The window layout — the split tree, the focused window, and per-window
    /// view state. Phase 1 holds exactly one window; the buffer it shows is the
    /// current buffer.
    windows: WindowTree,
    /// vim's alternate buffer (`#`), the `<C-^>` target; `None` until a switch
    /// sets it.
    alternate: Option<BufferId>,
    pub mode: Mode,
    pub cursor: Cursor,
    /// First visible buffer line (vertical scroll offset).
    pub top: usize,
    /// First visible screen column (horizontal scroll offset) of the focused
    /// window, under `nowrap`. `0` until a long line scrolls the viewport right;
    /// non-focused windows stash theirs in [`Window::saved_leftcol`]. Mirrors
    /// [`Editor::top`].
    pub leftcol: usize,
    /// Command-line contents (text after the leading `:` / `/` / `?`).
    pub cmdline: String,
    /// Cursor position within [`Editor::cmdline`], as a byte offset in `0..=len`
    /// (always on a char boundary). Insertion, deletion, and the projected
    /// command cursor are all relative to it, so `<Left>`/`<Right>` edit mid-line
    /// rather than only at the end. Reset to 0 each time a command line opens.
    cmdline_col: usize,
    /// What the command line is editing (`:` ex vs `/`,`?` search vs a scripted
    /// `vim.ui.input` prompt). Decides the prompt label and what `<CR>` submits.
    /// Only meaningful in [`Mode::Command`].
    cmdline_kind: CmdlineKind,
    /// The label shown ahead of the command line for a [`CmdlineKind::Prompt`]
    /// (`vim.ui.input`'s `opts.prompt`); empty for `:`/`/`/`?` (those use the
    /// single-char [`Editor::cmdline_prefix`]). Cleared when the prompt closes.
    cmdline_prompt: String,
    /// Resolved `vim.ui.input` prompts awaiting delivery to their Lua callback:
    /// `Some(text)` on `<CR>`, `None` on `<Esc>`/cancel. The server drains this
    /// each tick (like [`Editor::panel_selects`]) and fires the registered
    /// callback. nxvim-native; not a neovim concept.
    pub prompt_results: Vec<Option<String>>,
    /// For an open [`CmdlineKind::Confirm`]: the lowercase accelerator key of each
    /// button, in order. A keypress matching one (case-insensitively) resolves to
    /// its 1-based index. Empty unless a confirm prompt is open.
    confirm_accelerators: Vec<String>,
    /// For an open [`CmdlineKind::Confirm`]: the button `<CR>` selects (1-based;
    /// `0` = none, so `<CR>` cancels like `<Esc>`).
    confirm_default: i64,
    /// The last search pattern, its direction, and its trailing offset, for
    /// `n`/`N` repeat and an empty-pattern re-search. `None` until the first
    /// search.
    last_search: Option<(String, SearchDir, SearchOffset)>,
    /// Operator (`d`/`c`/`y`) waiting on a search motion: set when `d/`,`y?`, …
    /// open a search prompt, applied over the match when the search commits, and
    /// cleared on commit or `<Esc>`. `None` for a plain (movement) search.
    search_operator: Option<char>,
    /// Count prefixed onto the `/`,`?` that opened the current search prompt
    /// (`3/foo` finds the 3rd match), captured before it's reset and applied on
    /// submit. `1` for an un-counted search.
    pending_search_count: usize,
    /// Past search patterns, oldest first, recalled with `<Up>`/`<Down>` (and
    /// `<C-p>`/`<C-n>`) in the search command line.
    search_history: Vec<String>,
    /// Past `:` ex commands, oldest first, recalled with the same keys in the ex
    /// command line. Only interactively-typed lines are recorded — a programmatic
    /// `nvim_command` runs through [`Editor::command`] and never lands here.
    ex_history: Vec<String>,
    /// Position within the active history ([`Editor::search_history`] or
    /// [`Editor::ex_history`], per the open prompt's kind) while browsing it;
    /// `None` when editing a fresh line. Reset each time a command line opens.
    hist_idx: Option<usize>,
    /// Whether `hlsearch` highlighting is currently showing. Set by a search,
    /// cleared by `:nohlsearch`; gates the match spans projected into the `View`.
    search_active: bool,
    /// Cursor position saved when a search prompt opens, so `incsearch` previews
    /// run from a fixed origin and `<Esc>` (or the committed `<CR>`) starts from
    /// where the search began rather than from the last preview hop.
    search_origin: Cursor,
    /// Transient status message (the bottom line when not in command mode).
    /// Set via [`Editor::echo`], which also appends to `messages`.
    pub message: String,
    /// History of every message shown, the backing store for `:messages`.
    pub messages: Vec<String>,
    /// The bottom panel, when open (`:messages`, `:ls`). Grabs input focus.
    panel: Option<Panel>,
    /// The most recently shown panel, retained after it closes (or is replaced)
    /// so `:panelopen` can bring it back with its content and selection intact —
    /// e.g. reopening an LSP references list. `None` until a panel has been shown.
    last_panel: Option<Panel>,
    /// `<CR>` selections made in a select-enabled panel: each is `(0-based line
    /// index, line text)`. Drained by the server to fire the `on_select`
    /// scripting callback and the `nxvim_panel_select` RPC notification.
    pub panel_selects: Vec<(usize, String)>,
    pub should_quit: bool,
    /// Editor options set via `:set` (number column, …).
    pub options: Options,
    /// The highlight-group registry a colorscheme populates via `nvim_set_hl`.
    /// Mutated only by the server through the Lua drain path, keeping the core
    /// state machine pure; queried when resolving captures/chrome to styles.
    pub highlights: Highlights,

    width: usize,
    height: usize,
    /// Remembered target column for vertical motion (vim's `curswant`).
    desired_col: usize,
    /// When set, vertical motion sticks to end-of-line (set by `$`).
    desired_eol: bool,
    /// Per-key: the action just handled was a vertical/keep motion, so the
    /// remembered column must be preserved rather than recomputed.
    preserve_desired: bool,
    /// Per-key: the action just handled requests end-of-line stickiness (`$`).
    eol_request: bool,
    register: Register,

    /// The accumulated, not-yet-complete normal/visual command — the count, the
    /// pending operator, and the [`Stage`] of the in-progress sequence. Decided
    /// by the pure [`parse_step`]; reset on every completed command.
    pending: PendingCommand,
    /// The last find-char motion as `(kind, target)`, replayed by `;` (same
    /// direction) and `,` (opposite). Cross-command memory, not pending state, so
    /// it lives outside [`PendingCommand`] and survives `reset_pending`.
    last_find: Option<(FindKind, char)>,
    /// Set when an undo snapshot has already been taken for the current edit
    /// "session" (e.g. an insert), so we group the whole session into one undo.
    snapshot_taken: bool,
    /// Provenance for soft-tab `<BS>`: `(line, anchor_col)` of the whitespace run
    /// the *immediately preceding* `<Tab>` keypress inserted as spaces (its
    /// `anchor_col` is where the whole run began, preserved across consecutive
    /// tabs). [`handle_insert`](Self::handle_insert) clears it before every other
    /// key, so only Tab-inserted spaces collapse a whole unit on backspace —
    /// hand-typed spaces always delete one at a time. `None` outside that window.
    soft_tab: Option<(usize, usize)>,
    visual_anchor: Cursor,

    /// Set by a scroll command or a cursor motion at the moment it fires:
    /// `(top, cursor.line)` *before* the move. Consumed at the end of `input` to
    /// build `pending_scroll` when the viewport ends up moving more than a line.
    scroll_from: Option<(usize, usize)>,
    /// The scroll gesture from the most recent input, projected into the next
    /// `View` and then cleared (so it animates exactly once).
    pending_scroll: Option<PendingScroll>,

    /// Lua chunks queued by `:lua`, drained by the server's Lua runtime.
    pub lua_queue: Vec<String>,

    /// Ex-commands the core didn't recognize, handed to the server to resolve
    /// against Lua-defined user commands (`nvim_create_user_command`) before
    /// falling back to an unknown-command error. Keeps the core ignorant of the
    /// Lua command table while still routing typed `:Foo` and `vim.cmd.Foo()`
    /// through one place.
    pub deferred_commands: Vec<String>,

    /// Milliseconds the server should block after the current command, set by
    /// `:sleep` and drained via [`Editor::take_sleep`]. Models a slow editor
    /// operation; the server awaits it without freezing the UI.
    pending_sleep: Option<u64>,
}

impl Editor {
    pub fn new() -> Self {
        Editor::with_buffer(Buffer::empty())
    }

    pub fn open(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        Ok(Editor::with_buffer(Buffer::from_file(path.into())?))
    }

    /// Open `path`, falling back to a buffer *named after* `path` (rather than an
    /// unnamed scratch buffer) when the file exists but can't be read — a
    /// directory, a permission error, invalid UTF-8. The failure is echoed so the
    /// user sees it, and because the buffer keeps the name a later `:w` writes
    /// back to the file the user asked for instead of silently clobbering a stray
    /// one. Mirrors neovim, which opens a named buffer and reports an E-message on
    /// an unreadable startup file. (A *missing* file is not an error here:
    /// `from_file` already binds it as a new-file buffer.)
    pub fn open_or_named(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        match Buffer::from_file(&path) {
            Ok(buffer) => Editor::with_buffer(buffer),
            Err(e) => {
                let mut editor = Editor::with_buffer(Buffer::named(path.clone()));
                editor.echo(format!("E484: Can't open file {}: {e}", path.display()));
                editor
            }
        }
    }

    fn with_buffer(buffer: Buffer) -> Self {
        let (buffers, current) = BufferStore::with_one(buffer);
        let windows = WindowTree::with_one(current);
        let mut editor = Editor {
            buffers,
            windows,
            alternate: None,
            mode: Mode::Normal,
            cursor: Cursor::default(),
            top: 0,
            leftcol: 0,
            cmdline: String::new(),
            cmdline_col: 0,
            cmdline_kind: CmdlineKind::Ex,
            cmdline_prompt: String::new(),
            prompt_results: Vec::new(),
            confirm_accelerators: Vec::new(),
            confirm_default: 0,
            last_search: None,
            search_operator: None,
            pending_search_count: 1,
            search_history: Vec::new(),
            ex_history: Vec::new(),
            hist_idx: None,
            search_active: false,
            search_origin: Cursor::default(),
            message: String::new(),
            messages: Vec::new(),
            panel: None,
            last_panel: None,
            panel_selects: Vec::new(),
            should_quit: false,
            options: Options::default(),
            highlights: Highlights::new(),
            width: 80,
            height: 24,
            desired_col: 0,
            desired_eol: false,
            preserve_desired: false,
            eol_request: false,
            register: Register::default(),
            pending: PendingCommand::default(),
            last_find: None,
            snapshot_taken: false,
            soft_tab: None,
            visual_anchor: Cursor::default(),
            scroll_from: None,
            pending_scroll: None,
            lua_queue: Vec::new(),
            deferred_commands: Vec::new(),
            pending_sleep: None,
        };
        // Lay the sole window out into the default area so per-window rect
        // accessors (text width/height) are valid before the first `resize`.
        editor.relayout();
        editor
    }

    // ----- public API used by the server -----------------------------------

    /// The buffer the focused window shows — the *current* buffer. Derived from
    /// the window (vim's model) rather than stored independently, so `:b`/`:e`
    /// rebinding the window's buffer is all it takes to change it.
    fn cur_buffer(&self) -> BufferId {
        self.windows.cur().buffer
    }

    /// Rebind the focused window to show buffer `id`. The buffer-switch seam
    /// (`enter_buffer`, and the `:bdelete` fallback) writes the current buffer
    /// through here; it changes only *which* buffer the window shows.
    fn set_cur_buffer(&mut self, id: BufferId) {
        self.windows.cur_mut().buffer = id;
    }

    /// The current buffer's text model. The focused window shows exactly one
    /// buffer; this resolves it through the store, so the rest of the editor can
    /// keep saying `self.buffer()` without caring how many buffers are open.
    pub fn buffer(&self) -> &Buffer {
        &self.buffers.get(self.cur_buffer()).buffer
    }

    /// Mutable access to the current buffer's text model (see [`Editor::buffer`]).
    pub fn buffer_mut(&mut self) -> &mut Buffer {
        let id = self.cur_buffer();
        &mut self.buffers.get_mut(id).buffer
    }

    /// Read-only access to buffer `id`'s text model, or `None` if no such buffer
    /// is open — the buffer-addressed form of [`Editor::buffer`], for callers that
    /// must read a *non-current* buffer (e.g. converting LSP positions against
    /// each document a multi-file workspace edit touches). Mutation still goes
    /// through [`Editor::apply_edits_to`], so this stays read-only.
    pub fn buffer_of(&self, id: BufferId) -> Option<&Buffer> {
        self.buffers.map.get(&id).map(|ob| &ob.buffer)
    }

    /// Set a buffer-local option on buffer `id` from outside the editor (the Lua
    /// `vim.bo` / `nvim_set_option_value` bridge). `value` is the option's scalar:
    /// a number for `tabstop`/`shiftwidth`/`softtabstop` (booleans arrive through
    /// [`Editor::set_buffer_option_bool`]). It is clamped to each option's valid
    /// range (`tabstop ≥ 1`, `shiftwidth ≥ 0`, `softtabstop ≥ -1`). Unknown options
    /// and unknown buffers are ignored (the Lua side only forwards the wired set,
    /// and the buffer is mirror-guarded).
    pub fn set_buffer_option_num(&mut self, id: BufferId, name: &str, value: i64) {
        let Some(ob) = self.buffers.map.get_mut(&id) else {
            return;
        };
        match name {
            "tabstop" => ob.buffer.options.tabstop = value.max(1) as usize,
            "shiftwidth" => ob.buffer.options.shiftwidth = value.max(0) as usize,
            "softtabstop" => ob.buffer.options.softtabstop = value.max(-1) as isize,
            _ => {}
        }
    }

    /// Set a boolean buffer-local option (currently `expandtab`) on buffer `id`.
    /// The boolean companion to [`Editor::set_buffer_option_num`].
    pub fn set_buffer_option_bool(&mut self, id: BufferId, name: &str, value: bool) {
        let Some(ob) = self.buffers.map.get_mut(&id) else {
            return;
        };
        if name == "expandtab" {
            ob.buffer.options.expandtab = value;
        }
    }

    /// Drain buffer `id`'s LSP edit journal — the buffer-addressed form of the
    /// drain `sync_lsp` does on the current buffer via `take_lsp_edits`, so the
    /// server can flush a `didChange` for a *non-current* buffer a workspace edit
    /// just touched. `None` if no such buffer is open.
    pub fn take_lsp_edits_of(&mut self, id: BufferId) -> Option<EditBatch> {
        self.buffers
            .map
            .get_mut(&id)
            .map(|ob| ob.buffer.take_lsp_edits())
    }

    /// The current buffer's full editor-side state (text + undo/redo + saved
    /// position). Internal helper for the undo path and switching.
    fn cur_mut(&mut self) -> &mut OpenBuffer {
        let id = self.cur_buffer();
        self.buffers.get_mut(id)
    }

    /// The id of the buffer the window currently shows.
    pub fn current_buffer_id(&self) -> BufferId {
        self.cur_buffer()
    }

    /// All open buffer ids, ascending (the `nvim_list_bufs` order).
    pub fn buffer_ids(&self) -> Vec<BufferId> {
        self.buffers.map.keys().copied().collect()
    }

    /// Make `id` the current buffer (the `nvim_set_current_buf` entry point).
    /// A no-op if `id` is not an open buffer.
    pub fn set_current_buffer(&mut self, id: BufferId) {
        self.switch_buffer(id);
    }

    /// The file name of buffer `id` (`""` for an unnamed buffer), or `None` if
    /// no such buffer is open.
    pub fn buffer_name(&self, id: BufferId) -> Option<String> {
        self.buffers.map.get(&id).map(|ob| {
            ob.buffer
                .path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        })
    }

    /// Create a new empty buffer and return its id, without switching to it
    /// (the `nvim_create_buf` entry point).
    pub fn create_buffer(&mut self) -> BufferId {
        self.add_buffer(Buffer::empty())
    }

    /// Editable lines of buffer `id`, or `None` if no such buffer is open
    /// (the buffer-addressed form of [`Editor::lines`]).
    pub fn lines_of(&self, id: BufferId) -> Option<Vec<String>> {
        self.buffers.map.get(&id).map(|ob| ob.buffer.lines())
    }

    /// Number of editable lines in buffer `id`, or `None` if no such buffer is
    /// open. Cheap (no text copy), unlike [`Editor::lines_of`].
    pub fn line_count_of(&self, id: BufferId) -> Option<usize> {
        self.buffers.map.get(&id).map(|ob| ob.buffer.line_count())
    }

    // ----- buffer management ------------------------------------------------

    /// Add a buffer to the store and return its id, without switching to it.
    fn add_buffer(&mut self, buffer: Buffer) -> BufferId {
        self.buffers.insert(buffer)
    }

    /// Make `id` the current buffer: stash the outgoing window position with its
    /// buffer, record the alternate (`#`), and restore the incoming buffer's
    /// saved position. A no-op if `id` is already current or not in the store.
    ///
    /// The window always lands in normal mode; transient pending/scroll state is
    /// dropped. Syntax re-sync across the switch is the server's job (it notices
    /// the current-buffer id changed), so this touches neither `modified` nor the
    /// edit journal — switching a buffer must never make it look edited.
    fn switch_buffer(&mut self, id: BufferId) {
        if id == self.cur_buffer() || !self.buffers.map.contains_key(&id) {
            return;
        }
        // Stash the outgoing position with its buffer; it becomes the alternate.
        let (cursor, top, leftcol) = (self.cursor, self.top, self.leftcol);
        let outgoing = self.cur_buffer();
        let out = self.buffers.get_mut(outgoing);
        out.saved_cursor = cursor;
        out.saved_top = top;
        out.saved_leftcol = leftcol;
        self.alternate = Some(outgoing);

        self.enter_buffer(id);
    }

    /// Make `id` the current buffer and restore its saved window position,
    /// landing in normal mode with transient state cleared. Unlike
    /// [`Editor::switch_buffer`] this does *not* stash the outgoing position —
    /// the caller is responsible for that (or the outgoing buffer is gone, as
    /// when `:bdelete` removes the current one).
    fn enter_buffer(&mut self, id: BufferId) {
        let incoming = self.buffers.get(id);
        let (saved_cursor, saved_top, saved_leftcol) = (
            incoming.saved_cursor,
            incoming.saved_top,
            incoming.saved_leftcol,
        );
        self.set_cur_buffer(id);
        self.cursor = saved_cursor;
        self.top = saved_top;
        self.leftcol = saved_leftcol;
        self.mode = Mode::Normal;
        self.reset_pending();
        self.scroll_from = None;
        self.pending_scroll = None;
        self.message.clear();
        self.clamp_cursor();
        self.ensure_visible();
    }

    /// `<C-^>` — switch to the alternate buffer (`#`), or report `E23` when there
    /// is none (e.g. only one buffer is open).
    fn goto_alternate(&mut self) {
        match self.alternate {
            Some(id) => self.switch_buffer(id),
            None => self.echo("E23: No alternate file"),
        }
    }

    // ----- window management ------------------------------------------------

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
        WindowId(self.windows.next_id)
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
            _ => {}
        }
    }

    /// The editor's global options, for the server to mirror to Lua (`vim.o`).
    pub fn global_options(&self) -> Options {
        self.options
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

    /// Window `id`'s rect as `(x, y, width, height)` in windows-area cells, or
    /// `None` if there is no such window. `height` includes the status-line row;
    /// the API width/height the server returns derive from this.
    pub fn window_rect(&self, id: WindowId) -> Option<(usize, usize, usize, usize)> {
        self.windows
            .windows
            .get(&id)
            .map(|w| (w.rect.x, w.rect.y, w.rect.width, w.rect.height))
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
        // A float inherits the focused window's window-local options (the number
        // gutter), so it renders consistently with the rest of the editor.
        let options = self.windows.get(self.windows.current).options;
        let id = self.windows.alloc_id();
        self.windows.windows.insert(
            id,
            Window {
                buffer: buf,
                saved_cursor: Cursor::default(),
                saved_top: 0,
                saved_leftcol: 0,
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
    fn split(&mut self, dir: SplitDir) {
        let cursor = self.cursor;
        let top = self.top;
        let leftcol = self.leftcol;
        let cur = self.windows.current;
        // Stash the live position into the outgoing window so the old (bottom /
        // right) sibling keeps its view; seed the new window from the same spot.
        {
            let w = self.windows.get_mut(cur);
            w.saved_cursor = cursor;
            w.saved_top = top;
            w.saved_leftcol = leftcol;
        }
        let buffer = self.windows.get(cur).buffer;
        // A split inherits the source window's window-local options, as vim does.
        let options = self.windows.get(cur).options;
        let new_id = self.windows.alloc_id();
        self.windows.windows.insert(
            new_id,
            Window {
                buffer,
                saved_cursor: cursor,
                saved_top: top,
                saved_leftcol: leftcol,
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
    /// fill the freed area. Refuses to close the last window (vim's `E444`); the
    /// quit-when-last semantics belong to `:q`.
    fn close_window(&mut self) {
        let cur = self.windows.current;
        if !self.remove_window(cur) {
            self.echo("E444: Cannot close last window");
        }
    }

    /// Remove window `id` (focused or not) and collapse its parent split. When
    /// the closed window held focus, the spatially nearest survivor takes it (and
    /// its view position is restored); otherwise focus is untouched. Returns
    /// `false` — without touching the layout — if `id` is the last window (it
    /// can't be closed) or not open. Shared by `<C-w>c`/`:close` and the
    /// `nvim_win_close` API.
    fn remove_window(&mut self, id: WindowId) -> bool {
        if self.windows.count() <= 1 || !self.windows.windows.contains_key(&id) {
            return false;
        }
        let closing_rect = self.windows.get(id).rect;
        let was_focused = id == self.windows.current;
        // A float is not in the layout tree — drop it from the float list; a tiled
        // window collapses its parent split.
        if self.windows.get(id).float.is_some() {
            self.windows.floats.retain(|f| *f != id);
        } else {
            remove_leaf(&mut self.windows.root, id);
        }
        self.windows.windows.remove(&id);
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

    /// `<C-w>o` / `:only` — drop every window but the focused one, which expands
    /// to the whole area. A no-op with a single window. The kept window stays
    /// focused, so the live view position is untouched.
    fn only_window(&mut self) {
        if self.windows.count() <= 1 {
            return;
        }
        let keep = self.windows.current;
        self.windows.windows.retain(|id, _| *id == keep);
        self.windows.root = Node::Leaf(keep);
        self.relayout();
        self.ensure_visible();
    }

    /// `<C-w>=` / `:wincmd =` — reset every split to even shares and re-lay.
    fn equalize_windows(&mut self) {
        equalize_node(&mut self.windows.root);
        self.relayout();
        self.ensure_visible();
    }

    /// `<C-w>+`/`-` (height) and `<C-w>>`/`<` (width), and `:resize`/`:vertical
    /// resize` — grow or shrink the focused window by `delta` cells along `axis`,
    /// stealing from (or yielding to) a sibling. A no-op with a single window or
    /// no ancestor split of that orientation. `sizes` are in cells after the last
    /// layout, so this is plain cell arithmetic; the re-layout repaints it.
    fn resize_window(&mut self, axis: SplitDir, delta: isize) {
        let cur = self.windows.current;
        self.resize_window_id(cur, axis, delta);
    }

    /// Resize window `id` (focused or not) by `delta` cells along `axis`. The
    /// id-targeting core of [`Editor::resize_window`], shared with the
    /// `nvim_win_set_width`/`set_height` API. A no-op with one window, a zero
    /// delta, or an unknown id.
    fn resize_window_id(&mut self, id: WindowId, axis: SplitDir, delta: isize) {
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
    fn maximize_window(&mut self, axis: SplitDir) {
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
    fn focus_dir(&mut self, dir: WinDir) {
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
    /// around the layout order.
    fn focus_cycle(&mut self, forward: bool) {
        let order = self.windows.leaves();
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

    /// Make `id` the current window, restoring the buffer it shows and its saved
    /// view position, landing in normal mode with transient state cleared. The
    /// window analogue of [`Editor::enter_buffer`]: it does *not* stash the
    /// outgoing window (the caller did, or it is being closed). The restored
    /// cursor is clamped to the buffer's current line count (it may have shrunk
    /// while this window was inactive).
    fn enter_window(&mut self, id: WindowId) {
        self.windows.current = id;
        let w = self.windows.get(id);
        let (buffer, cursor, top, leftcol) =
            (w.buffer, w.saved_cursor, w.saved_top, w.saved_leftcol);
        self.set_cur_buffer(buffer);
        self.cursor = cursor;
        self.top = top;
        self.leftcol = leftcol;
        self.mode = Mode::Normal;
        self.reset_pending();
        self.scroll_from = None;
        self.pending_scroll = None;
        self.message.clear();
        self.clamp_cursor();
        self.ensure_visible();
    }

    /// The id of an already-open buffer bound to `path`, if any. Matches by
    /// *lexically* normalized path (so `./a` and `a` are the same buffer),
    /// **without touching the filesystem** — the pure core never does the
    /// blocking `canonicalize` syscall this used to run on every `:e`. Symlinks
    /// are not resolved, matching vim's path-based (not inode-based) buffer dedup.
    fn find_buffer_by_path(&self, path: &Path) -> Option<BufferId> {
        let target = normalize_path(path);
        self.buffers.map.iter().find_map(|(id, ob)| {
            let stored = ob.buffer.path.as_ref()?;
            (normalize_path(stored) == target).then_some(*id)
        })
    }

    /// Is the current buffer a throwaway scratch buffer — unnamed, unmodified,
    /// and empty? `:e file` loads into such a buffer in place (vim's behavior),
    /// rather than leaving a stray `[No Name]` behind.
    fn current_is_throwaway(&self) -> bool {
        let b = self.buffer();
        b.path.is_none() && !b.modified && b.line_count() == 1 && b.line(0).is_empty()
    }

    /// Replace the current buffer's contents with `path`'s, preserving the buffer
    /// id. Used by `:e` reload-in-place and to reuse a throwaway buffer. The
    /// loaded buffer is unmodified; the whole-content swap is flagged for syntax
    /// re-sync (`mark_resync` bumps `changedtick`, but we keep `modified` clear
    /// because it is freshly read from disk).
    fn load_into_current(&mut self, path: &Path) {
        match Buffer::from_file(path) {
            Ok(buf) => {
                self.cursor = Cursor::default();
                self.top = 0;
                self.leftcol = 0;
                let ob = self.cur_mut();
                ob.buffer = buf;
                ob.undo_stack.clear();
                ob.redo_stack.clear();
                // Reloaded from disk: a fresh state that is, by definition, saved.
                ob.cur_seq = ob.next_seq;
                ob.next_seq += 1;
                ob.saved_seq = Some(ob.cur_seq);
                ob.buffer.mark_resync();
                ob.buffer.modified = false;
            }
            Err(e) => self.echo(e.to_string()),
        }
    }

    /// Open or switch to the buffer for `path`, then place the cursor at the
    /// 0-based `(line, byte_col)`, clamped to the buffer and a grapheme
    /// boundary. Reuses the `:e` open-or-switch logic (so the jump reuses an
    /// already-open buffer and records the alternate `#`), but never reloads in
    /// place or guards on `modified` — a jump navigates, it does not discard
    /// edits. The column is a **byte** offset into the target line; callers that
    /// hold an LSP position convert the encoding to bytes first.
    ///
    /// A pure composition of the existing buffer-switch and cursor-set paths
    /// (no new state), so every front end and the LSP go-to / diagnostics
    /// location list share one navigation primitive.
    pub fn jump_to(&mut self, path: &Path, line: usize, col: usize) {
        let already_current = self.buffer().path.as_deref() == Some(path);
        if !already_current {
            if let Some(id) = self.find_buffer_by_path(path) {
                self.switch_buffer(id);
            } else if self.current_is_throwaway() {
                self.load_into_current(path);
            } else {
                match Buffer::from_file(path) {
                    Ok(buf) => {
                        let id = self.add_buffer(buf);
                        self.switch_buffer(id);
                    }
                    Err(e) => {
                        self.echo(e.to_string());
                        return;
                    }
                }
            }
        }

        // Land the cursor at (line, byte col), clamped to the buffer. The whole
        // position is rebuilt from a buffer byte index so it snaps to a grapheme
        // boundary and a valid normal-mode resting cell, exactly like a search
        // landing.
        let line = line.min(self.last_line());
        let byte = self.buffer().line_start(line) + col.min(self.buffer().line(line).len());
        self.set_cursor_char(byte);
        self.desired_col = self.cursor_virtcol();
        self.desired_eol = false;
        self.ensure_visible();
    }

    /// Apply a batch of non-overlapping byte-range replacements as **one undo
    /// step**, then place the cursor at `cursor_byte` (clamped to a valid insert
    /// position). Each `(range, text)` removes `range` and inserts `text` at its
    /// start; the batch is applied in descending start order so an earlier edit
    /// never invalidates a later one's offsets. The buffer is re-normalized after.
    ///
    /// Takes plain byte ranges and text — no LSP types — so `nxvim-core` stays
    /// LSP-free while the server's completion-accept (and, later, formatting /
    /// rename appliers) share one primitive. The edits fold into the current undo
    /// group when one is open (accepting a completion mid-insert is part of that
    /// insert's undo block, as in vim); in normal mode they form their own step.
    pub fn apply_edits(
        &mut self,
        mut edits: Vec<(std::ops::Range<usize>, String)>,
        cursor_byte: usize,
    ) {
        self.push_undo();
        // Highest start first: applying a later edit can't shift an earlier one.
        edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
        for (range, text) in edits {
            let len = self.buffer().len_bytes();
            let start = self.buffer().text.floor_char_boundary(range.start.min(len));
            let end = self.buffer().text.floor_char_boundary(range.end.min(len));
            if start < end {
                self.buffer_mut().remove(start..end);
            }
            if !text.is_empty() {
                self.buffer_mut().insert(start, &text);
            }
        }
        self.buffer_mut().normalize();
        self.set_cursor_char_insert(cursor_byte);
        self.desired_col = self.cursor_virtcol();
        self.desired_eol = false;
        self.ensure_visible();
    }

    /// Apply a batch of non-overlapping byte-range replacements to a **specific**
    /// (possibly non-current) buffer as one independent undo step for that buffer.
    /// The LSP-free multi-buffer sibling of [`Editor::apply_edits`]: a workspace
    /// edit (an LSP rename / code action) touches several open buffers at once,
    /// and each must remain independently undoable, so this snapshots and edits
    /// `id` on its own undo history rather than the active buffer's insert group.
    /// Edits apply highest-start-first (so an earlier offset never shifts), the
    /// buffer is re-normalized, and the current buffer's cursor is clamped to the
    /// new text (a non-current buffer's saved cursor is clamped when switched
    /// back). A no-op if `id` is unknown or `edits` is empty.
    ///
    /// Takes plain byte ranges and text — no LSP types — keeping `nxvim-core`
    /// LSP-free; the server converts LSP ranges to bytes (per buffer, per the
    /// negotiated encoding) before calling.
    pub fn apply_edits_to(
        &mut self,
        id: BufferId,
        mut edits: Vec<(std::ops::Range<usize>, String)>,
    ) {
        if edits.is_empty() || !self.buffers.map.contains_key(&id) {
            return;
        }
        self.push_undo_for(id);
        // Highest start first: applying a later edit can't shift an earlier one.
        edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
        for (range, text) in edits {
            let buf = &mut self.buffers.get_mut(id).buffer;
            let len = buf.len_bytes();
            let start = buf.text.floor_char_boundary(range.start.min(len));
            let end = buf.text.floor_char_boundary(range.end.min(len));
            if start < end {
                buf.remove(start..end);
            }
            if !text.is_empty() {
                buf.insert(start, &text);
            }
        }
        self.buffers.get_mut(id).buffer.normalize();
        if id == self.cur_buffer() {
            self.clamp_cursor();
            self.desired_col = self.cursor_virtcol();
            self.desired_eol = false;
            self.ensure_visible();
        }
        // A non-current buffer's saved cursor is clamped by `enter_buffer` on the
        // switch back, so nothing to do here.
    }

    /// Push an undo snapshot for buffer `id` (the buffer-addressed [`push_undo`],
    /// minting a fresh sequence number) so a subsequent edit to it is a distinct,
    /// independently-undoable step. Unlike [`Editor::push_undo`] it does not
    /// consult `snapshot_taken` (that flag tracks the *current* buffer's insert
    /// session); a workspace edit is a one-shot, normal-mode mutation per buffer.
    /// The snapshot's cursor is the live cursor when `id` is current, else the
    /// buffer's saved cursor.
    fn push_undo_for(&mut self, id: BufferId) {
        let (text, cursor, seq) = {
            let ob = self.buffers.get(id);
            let cursor = if id == self.cur_buffer() {
                self.cursor
            } else {
                ob.saved_cursor
            };
            (ob.buffer.text.clone(), cursor, ob.cur_seq)
        };
        let ob = self.buffers.get_mut(id);
        ob.undo_stack.push(Snapshot { text, cursor, seq });
        ob.redo_stack.clear();
        ob.cur_seq = ob.next_seq;
        ob.next_seq += 1;
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

    /// Re-divide the current terminal area across the window tree. The windows
    /// area excludes the global bottom panel (it docks below all windows); the
    /// command/message line is the client's own row, already excluded from the
    /// height the client reports. Re-run on resize and whenever the panel opens
    /// or closes (which grows/shrinks the windows area).
    fn relayout(&mut self) {
        let height = self.height.saturating_sub(self.panel_rows());
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

    /// Take any pending `:sleep` duration in milliseconds, clearing it. The
    /// server awaits this between message handling, so a slow editor operation
    /// never blocks the client (a separate thread/process).
    pub fn take_sleep(&mut self) -> Option<u64> {
        self.pending_sleep.take()
    }

    /// Feed a single key into the editor.
    pub fn input(&mut self, key: Key) {
        // A focused panel grabs every key (navigation + close), bypassing the
        // buffer's mode handling and the `curswant`/scroll bookkeeping below.
        if self.panel.is_some() {
            self.handle_panel(key);
            return;
        }

        self.preserve_desired = false;
        self.eol_request = false;

        match self.mode {
            Mode::Insert | Mode::Replace => self.handle_insert(key),
            Mode::Command => self.handle_command(key),
            _ => self.handle_normal(key),
        }

        // Update vim's `curswant`: vertical motions keep the remembered column,
        // every other action recomputes it from where the cursor landed.
        if !self.preserve_desired {
            self.desired_col = self.cursor_virtcol();
            self.desired_eol = self.eol_request;
        }
        self.ensure_visible();

        // If this key moved the viewport — an explicit scroll command, or a
        // motion that jumped off-screen — record the gesture for the client to
        // animate. A one-line shift (holding `j`/`k` at the edge) is left alone
        // so continuous scrolling stays crisp.
        if let Some((from_top, from_cursor)) = self.scroll_from.take() {
            if from_top.abs_diff(self.top) > 1 {
                // Cap the visual travel so a huge jump (e.g. `G` in a big file)
                // animates a bounded slide of the last couple of screens instead
                // of projecting thousands of lines into the view.
                let cap = self.text_height().saturating_mul(2).max(1);
                let clamp = |from: usize, to: usize| {
                    if from > to {
                        from.min(to + cap)
                    } else {
                        from.max(to.saturating_sub(cap))
                    }
                };
                let from_top = clamp(from_top, self.top);
                let from_cursor = clamp(from_cursor, self.cursor.line);
                let dist = from_top.abs_diff(self.top) as u64;
                self.pending_scroll = Some(PendingScroll {
                    from_top,
                    to_top: self.top,
                    from_cursor,
                    to_cursor: self.cursor.line,
                    duration_ms: (dist * 8).clamp(80, 160),
                });
            }
        }
    }

    /// Run an ex-command directly (the `nvim_command` API entry point).
    pub fn command(&mut self, cmd: &str) {
        self.execute_ex(cmd);
        self.desired_col = self.cursor_virtcol();
        self.desired_eol = false;
        self.ensure_visible();
    }

    /// Editable lines as owned strings (the `nvim_buf_get_lines` entry point).
    pub fn lines(&self) -> Vec<String> {
        self.buffer().lines()
    }

    /// Show `msg` on the message line **and** record it in the `:messages`
    /// history. This is the single place a user-facing message is set (errors,
    /// command output, captured `print`), so the history stays complete; the
    /// server routes its own messages through here too. Each non-empty line is
    /// recorded separately so a multi-line echo lists cleanly in `:messages`.
    pub fn echo(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        for line in msg.split('\n').filter(|l| !l.is_empty()) {
            self.messages.push(line.to_string());
        }
        // Bound the history so a long-running session can't grow it forever.
        if self.messages.len() > MAX_MESSAGES {
            self.messages.drain(0..self.messages.len() - MAX_MESSAGES);
        }
        self.message = msg;
    }

    /// Produce a [`View`] of the current state for a text viewport of the given
    /// size. The client renders the view's regions with its own widgets.
    pub fn view(&mut self, width: usize, height: usize) -> View {
        self.resize(width, height);
        let view = View::from_editor(self);
        self.pending_scroll = None; // animate exactly once
        view
    }

    pub(crate) fn pending_scroll(&self) -> Option<PendingScroll> {
        self.pending_scroll
    }

    /// True while `r` is waiting for the single character to replace with (a
    /// one-shot replace that stays in normal mode). Clients use it to show the
    /// replace cursor shape, matching vim's operator-pending feedback.
    pub(crate) fn pending_replace(&self) -> bool {
        self.pending.stage == Stage::ReplacePending
    }

    /// The fixed end of the visual selection (the other end is [`Self::cursor`]).
    /// Only meaningful while [`Self::mode`] is a visual mode.
    pub(crate) fn visual_anchor(&self) -> Cursor {
        self.visual_anchor
    }

    pub(crate) fn text_height(&self) -> usize {
        // The window's own rows minus its status line. The panel is already
        // excluded from the window rect by `relayout`, so it is not subtracted
        // again here.
        self.windows.cur().rect.height.saturating_sub(1).max(1)
    }

    /// The focused window's text-area width in screen cells: its rect width minus
    /// a bordered float's one-cell inset (on each side) and the number gutter. The
    /// horizontal analog of [`text_height`], feeding the `leftcol` scroll math.
    /// Matches the `width` `view::window_view` projects, so on-screen and policy
    /// agree.
    pub(crate) fn text_width(&self) -> usize {
        let w = self.windows.cur();
        let inset = matches!(&w.float, Some(cfg) if cfg.border != BorderStyle::None) as usize;
        let options = w.options;
        let rect_width = w.rect.width;
        let line_count = self.buffer().line_count();
        let number_width = self.number_width_for(options, line_count);
        rect_width
            .saturating_sub(2 * inset)
            .saturating_sub(number_width)
    }

    // ----- normal / visual mode --------------------------------------------

    /// Drive one key through the normal/visual grammar. A thin loop: the pure
    /// [`parse_step`] decides; [`Editor::execute`] (and the cancel arms here)
    /// apply. All the old inline pending-state bookkeeping now lives in
    /// `parse_step`, so this is the *only* place a normal-mode key enters and the
    /// grammar has exactly one home.
    fn handle_normal(&mut self, key: Key) {
        self.message.clear();
        match parse_step(self.mode, &self.pending, key) {
            ParseStep::Prefix(p) => self.pending = p,
            ParseStep::Complete(cmd) => self.execute(cmd),
            ParseStep::Cancel => {
                self.reset_pending();
                if self.mode.is_visual() {
                    self.mode = Mode::Normal;
                    self.clamp_cursor();
                }
            }
            ParseStep::Reset => self.reset_pending(),
            ParseStep::AbortObject => {
                // A find/text-object miss: in visual the selection (and count)
                // survive — only the half-typed object is dropped; otherwise the
                // whole pending command, operator included, is cancelled.
                if self.mode.is_visual() {
                    self.pending.stage = Stage::Start;
                } else {
                    self.reset_pending();
                }
            }
        }
    }

    /// Apply a fully-resolved command to the buffer, delegating to the existing
    /// effect helpers. The grammar is gone from here — `execute` only dispatches
    /// on the typed [`ResolvedCommand`] `parse_step` produced, so the parse and
    /// the effect cannot drift.
    fn execute(&mut self, cmd: ResolvedCommand) {
        match cmd {
            ResolvedCommand::Motion(m) => {
                // `f`/`t`/`F`/`T` and `;`/`,` set the find memory even on a miss,
                // matching the old `pending_find` block.
                if let Motion::Find(kind, target) = m {
                    self.last_find = Some((kind, target));
                }
                match self.resolve_motion(m) {
                    Some(mr) => self.apply_resolved_motion(mr),
                    // A find/`;`/`,` that doesn't match (or `;`/`,` with no prior
                    // find): an *execution* miss, not a grammar one. Cancel as the
                    // old failed-motion paths did — visual `f`-miss keeps the
                    // count, every other miss resets.
                    None => match m {
                        Motion::Find(..) if self.mode.is_visual() => {
                            self.pending.stage = Stage::Start;
                        }
                        _ => self.reset_pending(),
                    },
                }
            }
            ResolvedCommand::DoubledOperator(op) => self.begin_operator(op),
            ResolvedCommand::OperatorSearch { op, dir } => {
                let count = self.effective_count();
                self.search_operator = Some(op);
                self.enter_search(dir, count);
            }
            ResolvedCommand::TextObject { ia, kind } => {
                let count = self.effective_count();
                if let Some((lo, hi, linewise)) = self.text_object_range(ia, kind, count) {
                    self.apply_text_object(lo, hi, linewise);
                } else if self.mode.is_visual() {
                    // No object at the cursor: keep the selection (and count).
                    self.pending.stage = Stage::Start;
                } else {
                    self.reset_pending();
                }
            }
            ResolvedCommand::Replace(c) => {
                let count = self.effective_count();
                self.replace_char(c, count);
                self.reset_pending();
            }
            ResolvedCommand::VisualOperate(op) => self.visual_operate(op),
            ResolvedCommand::Normal(cmd) => self.execute_normal(cmd),
            ResolvedCommand::Window(cmd) => self.execute_window(cmd),
        }
    }

    /// Apply a `<C-w>` window command. Pending state is already a clean boundary
    /// (the prefix consumed it), so each arm just drives the window layout.
    fn execute_window(&mut self, cmd: WindowCmd) {
        // The resize family honors a leading count (`3<C-w>+`); read it before
        // the pending state is cleared.
        let count = self.effective_count() as isize;
        self.reset_pending();
        match cmd {
            WindowCmd::Split(dir) => self.split(dir),
            WindowCmd::FocusDir(dir) => self.focus_dir(dir),
            WindowCmd::FocusCycle(forward) => self.focus_cycle(forward),
            WindowCmd::Close => self.close_window(),
            WindowCmd::Only => self.only_window(),
            WindowCmd::Quit => self.ex_quit(false),
            WindowCmd::Equalize => self.equalize_windows(),
            WindowCmd::ResizeHeight(grow) => {
                self.resize_window(SplitDir::Horizontal, if grow { count } else { -count })
            }
            WindowCmd::ResizeWidth(grow) => {
                self.resize_window(SplitDir::Vertical, if grow { count } else { -count })
            }
            WindowCmd::MaxHeight => self.maximize_window(SplitDir::Horizontal),
            WindowCmd::MaxWidth => self.maximize_window(SplitDir::Vertical),
        }
    }

    /// Apply a terminal single-key command. Each arm is the old `handle_normal_
    /// command` body verbatim; the dispatch is now on the typed [`NormalCmd`]
    /// rather than a re-matched raw key.
    fn execute_normal(&mut self, cmd: NormalCmd) {
        let count = self.effective_count();
        match cmd {
            NormalCmd::InsertBefore => self.enter_insert_at(self.cursor.col),
            NormalCmd::InsertLineStart => {
                let col = self.first_non_blank(self.cursor.line);
                self.enter_insert_at(col);
            }
            NormalCmd::InsertAfter => {
                let col = (self.cursor.col + 1).min(self.line_len());
                self.enter_insert_at(col);
            }
            NormalCmd::InsertLineEnd => self.enter_insert_at(self.line_len()),
            NormalCmd::OpenBelow => self.open_line(true),
            NormalCmd::OpenAbove => self.open_line(false),
            NormalCmd::DeleteUnder => self.delete_under_cursor(count),
            NormalCmd::DeleteBefore => self.delete_before_cursor(count),
            NormalCmd::DeleteToEol => self.delete_to_eol(),
            NormalCmd::ChangeToEol => {
                self.delete_to_eol();
                self.mode = Mode::Insert;
                self.snapshot_taken = true;
            }
            NormalCmd::SubstituteChar => {
                self.delete_under_cursor(count);
                self.mode = Mode::Insert;
                self.snapshot_taken = true;
            }
            NormalCmd::PasteAfter => self.paste(true, count),
            NormalCmd::PasteBefore => self.paste(false, count),
            NormalCmd::Undo => self.undo(),
            NormalCmd::Redo => self.redo(),
            NormalCmd::Join => self.join_lines(count.max(2)),
            NormalCmd::ToggleCase => self.toggle_case(count),
            // `R` enters Replace mode: snapshot for undo, then overtype until
            // `<Esc>` (the insert handler honors `Mode::Replace`).
            NormalCmd::EnterReplace => {
                self.push_undo();
                self.snapshot_taken = true;
                self.mode = Mode::Replace;
            }
            // From normal mode entering visual anchors the selection; from visual
            // mode `v`/`V` only switch the selection's shape, leaving the anchor.
            NormalCmd::EnterVisual => {
                if !self.mode.is_visual() {
                    self.visual_anchor = self.cursor;
                }
                self.mode = Mode::Visual;
            }
            NormalCmd::EnterVisualLine => {
                if !self.mode.is_visual() {
                    self.visual_anchor = self.cursor;
                }
                self.mode = Mode::VisualLine;
            }
            NormalCmd::EnterCommand => self.enter_command(),
            NormalCmd::EnterSearch(dir) => self.enter_search(dir, count),
            NormalCmd::SearchNext => self.search_repeat(true, count),
            NormalCmd::SearchPrev => self.search_repeat(false, count),
            NormalCmd::SearchWord { dir, whole_word } => {
                self.search_word_under_cursor(dir, whole_word, count)
            }
            NormalCmd::ScrollHalf(down) => self.scroll_half(down),
            NormalCmd::ScrollPage(down) => self.scroll_page(down),
            NormalCmd::AltBuffer => self.goto_alternate(),
        }
        self.reset_pending();
    }

    /// Apply a doubled operator (`dd`/`cc`/`yy`): linewise over `count` lines.
    /// Only reached once the operator is already pending (the first `d`/`c`/`y`
    /// armed it in [`parse_command`]), so this is purely the doubled path.
    fn begin_operator(&mut self, op: char) {
        let count = self.effective_count();
        let last = self.cursor.line + count - 1;
        let target = self.buffer().line_start(last.min(self.last_line()));
        // axis is unused for the operator path, but the field is required.
        let m = MotionResult::linewise(target, MoveAxis::LineAnchor);
        self.pending.operator = None;
        self.apply_operator(op, m);
        self.reset_pending();
    }

    // ----- motions ----------------------------------------------------------

    /// Apply a resolved motion: as an operator's range if one is pending,
    /// otherwise as plain cursor movement (recording the pre-move position so
    /// an off-screen jump animates its scroll, like the explicit scrolls).
    fn apply_resolved_motion(&mut self, m: MotionResult) {
        if let Some(op) = self.pending.operator.take() {
            self.apply_operator(op, m);
        } else {
            self.scroll_from = Some((self.top, self.cursor.line));
            self.apply_movement(m);
        }
        // A completed motion clears the whole pending command. (The old movement
        // path only cleared `count`, but operator/g-prefix/find were already
        // cleared before it ran, so a full reset is equivalent — and now correct,
        // since `parse_step` leaves the stage set until the command finishes.)
        self.reset_pending();
    }

    /// Build a find-char motion. Forward (`f`/`t`) is inclusive, backward
    /// (`F`/`T`) is exclusive, matching how vim feeds these to an operator.
    fn find_motion(
        &self,
        kind: FindKind,
        target: char,
        count: usize,
        repeat: bool,
    ) -> Option<MotionResult> {
        let pos = self.find_char_target(kind, target, count, repeat)?;
        if kind.forward() {
            Some(MotionResult::inclusive(pos))
        } else {
            Some(MotionResult::exclusive(pos))
        }
    }

    /// Byte offset for a find-char motion on the cursor's line, or `None` if the
    /// target is not found. `f`/`F` land on the target; `t`/`T` stop one
    /// grapheme short. `repeat` (a `;`/`,` replay) hops over an immediately
    /// adjacent match for `t`/`T`, so repeats make progress instead of sticking.
    fn find_char_target(
        &self,
        kind: FindKind,
        target: char,
        count: usize,
        repeat: bool,
    ) -> Option<usize> {
        let line = self.cursor.line;
        let s = self.buffer().line(line);
        let base = self.buffer().line_start(line);
        let cur = self.cursor.col;
        let till = kind.till();
        let char_at = |col: usize| s[col..].chars().next();

        let hit = if kind.forward() {
            let mut col = unicode::next_grapheme(&s, cur);
            if till && repeat && col < s.len() && char_at(col) == Some(target) {
                col = unicode::next_grapheme(&s, col);
            }
            let mut found = 0;
            let mut hit = None;
            while col < s.len() {
                if char_at(col) == Some(target) {
                    found += 1;
                    if found == count {
                        hit = Some(col);
                        break;
                    }
                }
                col = unicode::next_grapheme(&s, col);
            }
            hit.map(|c| {
                if till {
                    unicode::prev_grapheme(&s, c)
                } else {
                    c
                }
            })
        } else {
            if cur == 0 {
                return None;
            }
            let mut col = unicode::prev_grapheme(&s, cur);
            if till && repeat && col > 0 && char_at(col) == Some(target) {
                col = unicode::prev_grapheme(&s, col);
            }
            let mut found = 0;
            let mut hit = None;
            loop {
                if char_at(col) == Some(target) {
                    found += 1;
                    if found == count {
                        hit = Some(col);
                        break;
                    }
                }
                if col == 0 {
                    break;
                }
                col = unicode::prev_grapheme(&s, col);
            }
            hit.map(|c| {
                if till {
                    unicode::next_grapheme(&s, c)
                } else {
                    c
                }
            })
        };
        hit.map(|col| base + col)
    }

    /// Where a [`Motion`] lands, as a [`MotionResult`]. The motion *alphabet*
    /// lives in [`classify_motion`]; this is its effect counterpart — it `match`es
    /// the typed enum exhaustively, so a new motion variant must be handled here
    /// too (the compiler enforces it). Returns `None` only for *execution* misses
    /// (a find/`;`/`,` with no match, or `;`/`,` before any find), never for a
    /// classification disagreement. The count and the un-defaulted `raw` count are
    /// derived from the pending command, exactly as the old caller computed them.
    fn resolve_motion(&self, motion: Motion) -> Option<MotionResult> {
        let line = self.cursor.line;
        let last_line = self.last_line();
        let count = self.effective_count();
        let raw = if self.pending.operator.is_some() {
            self.pending.op_count.or(self.pending.count)
        } else {
            self.pending.count
        };

        let result = match motion {
            Motion::GotoTop => {
                let target_line = raw.map(|n| n - 1).unwrap_or(0).min(last_line);
                let target = self.buffer().line_start(target_line);
                MotionResult::linewise(target, MoveAxis::LineAnchor)
            }
            // `;` repeats the last find-char motion; `,` repeats it reversed.
            Motion::FindRepeat { reverse } => {
                let (kind, target) = self.last_find?;
                let kind = if reverse { kind.reversed() } else { kind };
                return self.find_motion(kind, target, count, true);
            }
            Motion::Find(kind, target) => return self.find_motion(kind, target, count, false),
            Motion::Left => {
                let s = self.buffer().line(line);
                let mut col = self.cursor.col;
                for _ in 0..count {
                    col = unicode::prev_grapheme(&s, col);
                }
                MotionResult::exclusive(self.buffer().byte_at(line, col))
            }
            Motion::Right => {
                let s = self.buffer().line(line);
                let mut col = self.cursor.col;
                for _ in 0..count {
                    col = unicode::next_grapheme(&s, col);
                }
                MotionResult::exclusive(self.buffer().byte_at(line, col))
            }
            Motion::LineStart => MotionResult::exclusive(self.buffer().byte_at(line, 0)),
            Motion::FirstNonBlank => {
                let col = self.first_non_blank(line);
                MotionResult::exclusive(self.buffer().byte_at(line, col))
            }
            Motion::LineEnd => {
                let l = (line + count - 1).min(last_line);
                let s = self.buffer().line(l);
                let col = unicode::prev_grapheme(&s, s.len());
                MotionResult {
                    target: self.buffer().byte_at(l, col),
                    kind: MotionKind::Inclusive,
                    axis: MoveAxis::EndOfLine,
                }
            }
            Motion::Down => {
                let l = (line + count).min(last_line);
                MotionResult::linewise(self.buffer().line_start(l), MoveAxis::VerticalKeep)
            }
            Motion::Up => {
                let l = line.saturating_sub(count);
                MotionResult::linewise(self.buffer().line_start(l), MoveAxis::VerticalKeep)
            }
            Motion::GotoLine => {
                let l = raw.map(|n| n - 1).unwrap_or(last_line).min(last_line);
                MotionResult::linewise(self.buffer().line_start(l), MoveAxis::LineAnchor)
            }
            Motion::Word => self.word_motion(count),
            Motion::BackWord => {
                let mut idx = self.cursor_char();
                for _ in 0..count {
                    idx = self.word_backward(idx);
                }
                MotionResult::exclusive(idx)
            }
            Motion::EndWord => {
                let mut idx = self.cursor_char();
                for _ in 0..count {
                    idx = self.word_end(idx);
                }
                MotionResult::inclusive(idx)
            }
        };
        Some(result)
    }

    /// Resolve a `w`/`W` word motion. Special case: `cw` on a non-blank acts like
    /// `ce` — it changes to the end of the word without swallowing the trailing
    /// space — so it returns an inclusive end-of-word target instead.
    fn word_motion(&self, count: usize) -> MotionResult {
        let mut idx = self.cursor_char();
        if self.pending.operator == Some('c')
            && idx <= self.last_char_idx()
            && char_class(self.char_at(idx)) != CharClass::Blank
        {
            for _ in 0..count {
                idx = self.word_end(idx);
            }
            MotionResult::inclusive(idx)
        } else {
            for _ in 0..count {
                idx = self.word_forward(idx);
            }
            MotionResult::exclusive(idx)
        }
    }

    /// Apply a motion as plain cursor movement, maintaining vim's `curswant`.
    fn apply_movement(&mut self, m: MotionResult) {
        match m.axis {
            MoveAxis::Horizontal => {
                self.set_cursor_char(m.target);
                self.clamp_cursor();
            }
            MoveAxis::EndOfLine => {
                self.set_cursor_char(m.target);
                self.clamp_cursor();
                self.eol_request = true;
            }
            MoveAxis::LineAnchor => {
                let line = self
                    .buffer()
                    .byte_to_line(m.target.min(self.last_char_idx()));
                self.cursor.line = line;
                self.cursor.col = self.first_non_blank(line);
                self.clamp_cursor();
            }
            MoveAxis::VerticalKeep => {
                let line = self
                    .buffer()
                    .byte_to_line(m.target.min(self.last_char_idx()));
                self.cursor.line = line;
                self.settle_desired_col(false);
                self.preserve_desired = true;
            }
        }
    }

    fn word_forward(&self, mut idx: usize) -> usize {
        let last = self.last_char_idx();
        if idx >= last {
            return idx;
        }
        let start = char_class(self.char_at(idx));
        if start != CharClass::Blank {
            while idx < last && char_class(self.char_at(idx)) == start {
                idx = self.next_grapheme_idx(idx);
            }
        }
        while idx < last && char_class(self.char_at(idx)) == CharClass::Blank {
            idx = self.next_grapheme_idx(idx);
        }
        idx
    }

    fn word_backward(&self, mut idx: usize) -> usize {
        if idx == 0 {
            return 0;
        }
        idx = self.prev_grapheme_idx(idx);
        while idx > 0 && char_class(self.char_at(idx)) == CharClass::Blank {
            idx = self.prev_grapheme_idx(idx);
        }
        if idx == 0 {
            return 0;
        }
        let cls = char_class(self.char_at(idx));
        while idx > 0 {
            let prev = self.prev_grapheme_idx(idx);
            if char_class(self.char_at(prev)) != cls {
                break;
            }
            idx = prev;
        }
        idx
    }

    fn word_end(&self, mut idx: usize) -> usize {
        let last = self.last_char_idx();
        if idx >= last {
            return idx;
        }
        idx = self.next_grapheme_idx(idx);
        while idx < last && char_class(self.char_at(idx)) == CharClass::Blank {
            idx = self.next_grapheme_idx(idx);
        }
        let cls = char_class(self.char_at(idx));
        while idx < last {
            let next = self.next_grapheme_idx(idx);
            if next > last || char_class(self.char_at(next)) != cls {
                break;
            }
            idx = next;
        }
        idx
    }

    // ----- text objects -----------------------------------------------------

    /// Resolve the absolute charwise byte range `[start, end)` for a text
    /// object. `ia` is `'i'` (inner) or `'a'` (a/around); `obj` is the object
    /// key. Returns `None` for an unknown object key or when no object exists
    /// at the cursor.
    fn text_object_range(
        &self,
        ia: char,
        kind: ObjectKind,
        count: usize,
    ) -> Option<(usize, usize, bool)> {
        // Charwise objects return `(lo, hi)`; tag them `false` here.
        let charwise = |r: Option<(usize, usize)>| r.map(|(lo, hi)| (lo, hi, false));
        match kind {
            ObjectKind::Word(big) => charwise(self.word_object(ia, count, big)),
            ObjectKind::Pair(open, close) => self.pair_object(ia, open, close),
            ObjectKind::Quote(q) => charwise(self.quote_object(ia, q)),
            ObjectKind::Sentence => charwise(self.sentence_object(ia, count)),
            ObjectKind::Paragraph => self.paragraph_object(ia, count), // linewise
        }
    }

    /// Apply the pending operator (or extend the visual selection) to a text
    /// object's range `[lo, hi)`. `linewise` objects (paragraph) select whole
    /// lines; charwise objects span an exact byte range.
    fn apply_text_object(&mut self, lo: usize, hi: usize, linewise: bool) {
        if self.mode.is_visual() {
            if linewise {
                self.mode = Mode::VisualLine;
                self.set_visual_span_lines(lo, hi);
            } else {
                // A linewise-visual charwise object (e.g. `viw`) drops to
                // charwise, like vim.
                if self.mode == Mode::VisualLine {
                    self.mode = Mode::Visual;
                }
                self.set_visual_span(lo, hi);
            }
            self.pending.stage = Stage::Start;
            return;
        }
        if let Some(op) = self.pending.operator.take() {
            if linewise {
                let first_line = self.buffer().byte_to_line(lo);
                self.apply_operator_to_range(op, lo, hi, true, first_line);
            } else {
                self.apply_operator_to_range(op, lo, hi, false, 0);
            }
        }
        self.reset_pending();
    }

    /// Set a linewise visual selection spanning the lines covered by the
    /// byte range `[lo, hi)` (with `hi` a line-start boundary just past the
    /// last line). Anchor on the first line, cursor on the last.
    fn set_visual_span_lines(&mut self, lo: usize, hi: usize) {
        let first = self.buffer().byte_to_line(lo);
        let last = self.buffer().byte_to_line(hi.saturating_sub(1));
        self.visual_anchor = Cursor {
            line: first,
            col: 0,
        };
        self.cursor = Cursor { line: last, col: 0 };
        self.clamp_cursor();
    }

    /// Set the visual selection to span `[lo, hi)`: anchor at the first char,
    /// live cursor on the last char (inclusive). Empty ranges park both at `lo`.
    fn set_visual_span(&mut self, lo: usize, hi: usize) {
        self.set_cursor_char(lo);
        self.visual_anchor = self.cursor;
        let end = if hi > lo {
            self.prev_grapheme_idx(hi)
        } else {
            lo
        };
        self.set_cursor_char(end);
    }

    /// Classify the char at `idx` for span-finding. `big = false` uses the
    /// three-way `char_class` (`0` blank, `1` word, `2` punct); `big = true`
    /// collapses to blank (`0`) vs non-blank (`1`) — vim's `WORD`.
    fn span_class(&self, idx: usize, big: bool) -> u8 {
        match char_class(self.char_at(idx)) {
            CharClass::Blank => 0,
            CharClass::Word => 1,
            CharClass::Punct => {
                if big {
                    1
                } else {
                    2
                }
            }
        }
    }

    /// `[start, end)` of the maximal run around `idx` of chars sharing its
    /// `span_class`. Buffer-wide; `end` never passes the trailing phantom `\n`.
    fn class_span(&self, idx: usize, big: bool) -> (usize, usize) {
        let last = self.last_char_idx();
        let idx = idx.min(last);
        let cls = self.span_class(idx, big);
        let mut start = idx;
        while start > 0 {
            let prev = self.prev_grapheme_idx(start);
            if self.span_class(prev, big) != cls {
                break;
            }
            start = prev;
        }
        let mut end = idx;
        while end < last && self.span_class(end, big) == cls {
            end = self.next_grapheme_idx(end);
        }
        (start, end)
    }

    /// `iw`/`aw` (`big = false`) and `iW`/`aW` (`big = true`). `iw` is the run
    /// under the cursor; `aw` adds the trailing whitespace (or, if there is
    /// none, the leading whitespace). `count` extends over successive spans.
    fn word_object(&self, ia: char, count: usize, big: bool) -> Option<(usize, usize)> {
        let last = self.last_char_idx();
        let cur = self.cursor_char();
        let (start, end0) = self.class_span(cur, big);

        if ia == 'i' {
            let mut end = end0;
            let mut remaining = count.saturating_sub(1);
            while remaining > 0 && end < last {
                let (_, e2) = self.class_span(end, big);
                if e2 == end {
                    break;
                }
                end = e2;
                remaining -= 1;
            }
            return Some((start, end));
        }

        // `aw`: span plus surrounding whitespace.
        let mut start = start;
        let mut end = end0;
        let mut took_trailing = false;
        let mut units = count;

        // Starting on whitespace: the first unit is the blank run plus the
        // following word.
        if self.span_class(cur, big) == 0 {
            if end < last {
                let (_, e2) = self.class_span(end, big);
                end = e2;
            }
            took_trailing = true; // leading whitespace already covered
            units = units.saturating_sub(1);
        }

        while units > 0 {
            // Trailing whitespace after the current word.
            if end < last && self.span_class(end, big) == 0 {
                let (_, e2) = self.class_span(end, big);
                end = e2;
                took_trailing = true;
            } else {
                took_trailing = false;
            }
            units -= 1;
            // Another word follows for a further count.
            if units > 0 && end < last {
                let (_, e2) = self.class_span(end, big);
                end = e2;
            }
        }

        // No trailing whitespace was consumed: take the leading whitespace.
        if !took_trailing && start > 0 {
            let prev = self.prev_grapheme_idx(start);
            if self.span_class(prev, big) == 0 {
                let (s2, _) = self.class_span(prev, big);
                start = s2;
            }
        }
        Some((start, end))
    }

    /// `i(`/`a(` and friends. Find the innermost `open`/`close` pair enclosing
    /// the cursor; `i` excludes the brackets, `a` includes them. The bool is
    /// the linewise flag (see the inner promotion below).
    fn pair_object(&self, ia: char, open: char, close: char) -> Option<(usize, usize, bool)> {
        let open_idx = self.find_unmatched_open(open, close, self.cursor_char())?;
        let close_idx = self.find_match_close(open, close, open_idx)?;

        if ia == 'i' {
            // Linewise promotion: when the inner content is whole lines — the
            // open bracket ends its line and the close bracket starts its line
            // (modulo surrounding whitespace) — `i{`/`i(`/… select the lines
            // *between* the brackets, linewise, leaving the bracket lines. vim
            // keeps the object charwise in visual mode, so skip it there.
            let open_line = self.buffer().byte_to_line(open_idx);
            let close_line = self.buffer().byte_to_line(close_idx);
            if !self.mode.is_visual()
                && close_line > open_line
                && self.rest_of_line_blank(open_idx)
                && self.start_of_line_blank(close_idx)
            {
                let lo = self.buffer().line_start(open_line + 1);
                let hi = self.buffer().line_start(close_line);
                return Some((lo, hi, true));
            }
            return Some((self.next_grapheme_idx(open_idx), close_idx, false));
        }
        Some((open_idx, self.next_grapheme_idx(close_idx), false))
    }

    /// True if everything after byte `idx` to the end of its line is blank.
    fn rest_of_line_blank(&self, idx: usize) -> bool {
        let line = self.buffer().byte_to_line(idx);
        let end = self.buffer().line_start(line) + self.buffer().line_len(line);
        let mut i = self.next_grapheme_idx(idx);
        while i < end {
            if !matches!(self.char_at(i), ' ' | '\t') {
                return false;
            }
            i = self.next_grapheme_idx(i);
        }
        true
    }

    /// True if everything before byte `idx` from its line start is blank.
    fn start_of_line_blank(&self, idx: usize) -> bool {
        let mut i = self.buffer().line_start(self.buffer().byte_to_line(idx));
        while i < idx {
            if !matches!(self.char_at(i), ' ' | '\t') {
                return false;
            }
            i = self.next_grapheme_idx(i);
        }
        true
    }

    /// Scan backward from `from` (inclusive) for the `open` bracket that
    /// encloses it, honoring nesting. A `close` on `from` itself is the closing
    /// half of the wanted pair, so it is not counted.
    fn find_unmatched_open(&self, open: char, close: char, from: usize) -> Option<usize> {
        let mut idx = from;
        let mut depth = 0i32;
        loop {
            let c = self.char_at(idx);
            if c == close && idx != from {
                depth += 1;
            } else if c == open {
                if depth == 0 {
                    return Some(idx);
                }
                depth -= 1;
            }
            if idx == 0 {
                return None;
            }
            idx = self.prev_grapheme_idx(idx);
        }
    }

    /// Scan forward from `open_idx` for its matching `close`, honoring nesting.
    fn find_match_close(&self, open: char, close: char, open_idx: usize) -> Option<usize> {
        let last = self.last_char_idx();
        let mut idx = open_idx;
        let mut depth = 0i32;
        loop {
            let c = self.char_at(idx);
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            if idx >= last {
                return None;
            }
            idx = self.next_grapheme_idx(idx);
        }
    }

    /// `i"`/`a"` (and `'`, `` ` ``). Quote objects are confined to the cursor's
    /// line: quotes are paired left-to-right (1st–2nd, 3rd–4th, …) and the pair
    /// chosen is the one enclosing the cursor, else the first pair beginning at
    /// or after it. `i"` is the text between the quotes; `a"` includes the
    /// quotes plus the trailing whitespace (or, if none, the leading).
    ///
    /// A backslash escapes the following byte (vim's `quoteescape`), so `\"` is
    /// not a delimiter and `\\` is a literal backslash followed by a real quote.
    fn quote_object(&self, ia: char, quote: char) -> Option<(usize, usize)> {
        let line = self.cursor.line;
        let base = self.buffer().line_start(line);
        let s = self.buffer().line(line);
        let bytes = s.as_bytes();
        let q = quote as u8;

        // Quote positions on the line, paired in order. A `\` consumes the next
        // byte, so escaped quotes are skipped (and `\\` leaves the quote real).
        let mut quotes: Vec<usize> = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == q {
                quotes.push(i);
            }
            i += 1;
        }
        let cursor_rel = self.cursor_char() - base;

        // First left-to-right pair whose closing quote is at or after the
        // cursor: it either encloses the cursor or is the next one ahead.
        let (open, close) = quotes
            .chunks_exact(2)
            .map(|p| (p[0], p[1]))
            .find(|&(_, c)| cursor_rel <= c)
            // Dangling-quote fallback (odd quote count, e.g. `"trib"uto"`): the
            // cursor is past the last complete pair but flanked by quotes, so
            // pair the quote before it with the next one. This lets `i"`/`a"`
            // work on both sides of a shared quote. (`"a" "b"`-style gaps still
            // hit the normal path above and seek forward to the next string.)
            .or_else(|| {
                let l = *quotes.iter().rev().find(|&&p| p < cursor_rel)?;
                let r = *quotes.iter().find(|&&p| p >= cursor_rel)?;
                Some((l, r))
            })?;

        let (lo_rel, hi_rel) = match ia {
            'i' => (open + 1, close),
            _ => {
                // `a"`: include the quotes, then the trailing whitespace.
                let is_blank = |i: usize| bytes[i] == b' ' || bytes[i] == b'\t';
                let mut hi = close + 1;
                let trail_start = hi;
                while hi < bytes.len() && is_blank(hi) {
                    hi += 1;
                }
                if hi > trail_start {
                    (open, hi)
                } else {
                    // No trailing whitespace: take the leading whitespace.
                    let mut lo = open;
                    while lo > 0 && is_blank(lo - 1) {
                        lo -= 1;
                    }
                    (lo, hi)
                }
            }
        };
        Some((base + lo_rel, base + hi_rel))
    }

    /// `ip`/`ap` — a paragraph: a run of non-blank lines, or a run of blank
    /// lines, delimited by the other kind. `ap` adds the trailing blank lines
    /// (or, if none, the leading blank lines). Linewise; `count` extends over
    /// successive blocks.
    fn paragraph_object(&self, ia: char, count: usize) -> Option<(usize, usize, bool)> {
        let total = self.buffer().line_count();
        if total == 0 {
            return None;
        }
        let blank = |l: usize| self.buffer().line_len(l) == 0;
        let cur = self.cursor.line.min(total - 1);
        let on_blank = blank(cur);

        let mut first = cur;
        while first > 0 && blank(first - 1) == on_blank {
            first -= 1;
        }
        let mut last = cur;
        while last + 1 < total && blank(last + 1) == on_blank {
            last += 1;
        }

        // `count` extends over further blocks, alternating blank/non-blank.
        let mut kind = on_blank;
        for _ in 1..count {
            if last + 1 >= total {
                break;
            }
            kind = !kind;
            while last + 1 < total && blank(last + 1) == kind {
                last += 1;
            }
        }

        if ia == 'a' {
            // Include the trailing block of the opposite kind (blank lines after
            // a normal paragraph); if there is none, take the preceding one.
            let trailing = !blank(last);
            if last + 1 < total && blank(last + 1) == trailing {
                while last + 1 < total && blank(last + 1) == trailing {
                    last += 1;
                }
            } else {
                let leading = !blank(first);
                while first > 0 && blank(first - 1) == leading {
                    first -= 1;
                }
            }
        }

        let lo = self.buffer().line_start(first);
        let hi = self.buffer().line_start((last + 1).min(total));
        Some((lo, hi, true))
    }

    /// Byte bounds `[start, end)` of the paragraph (block of non-blank lines)
    /// at the cursor; just the current line if the cursor is on a blank line.
    fn current_paragraph_bytes(&self) -> (usize, usize) {
        let total = self.buffer().line_count();
        if total == 0 {
            return (0, 0);
        }
        let blank = |l: usize| self.buffer().line_len(l) == 0;
        let cur = self.cursor.line.min(total - 1);
        let (mut first, mut last) = (cur, cur);
        if !blank(cur) {
            while first > 0 && !blank(first - 1) {
                first -= 1;
            }
            while last + 1 < total && !blank(last + 1) {
                last += 1;
            }
        }
        let start = self.buffer().line_start(first);
        let end = self.buffer().line_start((last + 1).min(total));
        (start, end)
    }

    /// `is`/`as` — a sentence: text ending at `.`/`!`/`?` (optionally followed
    /// by closing `)]"'`) and a space/tab or end of line. Bounded by the
    /// surrounding paragraph. `is` is the sentence text; `as` adds the trailing
    /// whitespace (or, if none, leading). Charwise; `count` extends over
    /// successive sentences.
    fn sentence_object(&self, ia: char, count: usize) -> Option<(usize, usize)> {
        let (p_start, p_end) = self.current_paragraph_bytes();
        if p_start >= p_end {
            return None;
        }
        let is_ws = |c: char| c == ' ' || c == '\t' || c == '\n' || c == '\r';
        let is_close = |c: char| matches!(c, ')' | ']' | '"' | '\'');

        // Each sentence as `(start, text_end, ws_end)`.
        let mut segs: Vec<(usize, usize, usize)> = Vec::new();
        let mut s = p_start;
        while s < p_end && is_ws(self.char_at(s)) {
            s = self.next_grapheme_idx(s);
        }
        let mut i = s;
        while i < p_end {
            let c = self.char_at(i);
            if c == '.' || c == '!' || c == '?' {
                let mut j = self.next_grapheme_idx(i);
                while j < p_end && is_close(self.char_at(j)) {
                    j = self.next_grapheme_idx(j);
                }
                if j >= p_end || is_ws(self.char_at(j)) {
                    let text_end = j;
                    let mut k = j;
                    while k < p_end && is_ws(self.char_at(k)) {
                        k = self.next_grapheme_idx(k);
                    }
                    segs.push((s, text_end, k));
                    s = k;
                    i = k;
                    continue;
                }
            }
            i = self.next_grapheme_idx(i);
        }
        // Trailing run with no terminator.
        if s < p_end {
            let mut text_end = p_end;
            while text_end > s && is_ws(self.char_at(self.prev_grapheme_idx(text_end))) {
                text_end = self.prev_grapheme_idx(text_end);
            }
            segs.push((s, text_end, p_end));
        }
        if segs.is_empty() {
            return None;
        }

        let cursor = self.cursor_char();
        let mut idx = 0;
        for (n, &(start, _, _)) in segs.iter().enumerate() {
            if start <= cursor {
                idx = n;
            }
        }
        let start = segs[idx].0;
        let end = (idx + count.max(1) - 1).min(segs.len() - 1);
        let (_, text_end, ws_end) = segs[end];

        if ia == 'i' {
            Some((start, text_end))
        } else if ws_end > text_end {
            Some((start, ws_end))
        } else {
            // No trailing whitespace: take the leading whitespace instead.
            let mut lo = start;
            while lo > p_start && is_ws(self.char_at(self.prev_grapheme_idx(lo))) {
                lo = self.prev_grapheme_idx(lo);
            }
            Some((lo, ws_end))
        }
    }

    // ----- operators --------------------------------------------------------

    fn apply_operator(&mut self, op: char, m: MotionResult) {
        let cur = self.cursor_char();
        let (lo, hi, linewise, first_line) = match m.kind {
            MotionKind::Exclusive => (min(cur, m.target), max(cur, m.target), false, 0),
            MotionKind::Inclusive => (min(cur, m.target), max(cur, m.target) + 1, false, 0),
            MotionKind::Linewise => {
                let l1 = self.cursor.line;
                let l2 = self
                    .buffer()
                    .byte_to_line(m.target.min(self.last_char_idx()));
                let (a, b) = (min(l1, l2), max(l1, l2));
                let lo = self.buffer().line_start(a);
                let hi = self
                    .buffer()
                    .line_start((b + 1).min(self.buffer().line_count()));
                (lo, hi, true, a)
            }
        };
        self.apply_operator_to_range(op, lo, hi, linewise, first_line);
    }

    /// Apply `op` to the absolute byte range `[lo, hi)`. `linewise`/`first_line`
    /// control linewise settling; charwise callers (motions, text objects) pass
    /// `(false, 0)`. Unlike `apply_operator`, the range is explicit and need not
    /// touch the cursor — text objects span both sides of it.
    fn apply_operator_to_range(
        &mut self,
        op: char,
        lo: usize,
        hi: usize,
        linewise: bool,
        first_line: usize,
    ) {
        if lo >= hi {
            return;
        }
        match op {
            'y' => {
                self.yank_range(lo, hi, linewise);
                if linewise {
                    self.cursor.line = first_line;
                } else {
                    self.set_cursor_char(lo);
                }
                self.clamp_cursor();
            }
            'd' => {
                self.yank_range(lo, hi, linewise);
                self.delete_range(lo, hi);
                if linewise {
                    self.settle_after_linewise_delete(first_line);
                } else {
                    self.set_cursor_char(lo);
                }
                self.clamp_cursor();
            }
            'c' => {
                self.yank_range(lo, hi, linewise);
                if linewise {
                    self.linewise_change(lo, hi, first_line);
                } else {
                    self.delete_range(lo, hi);
                    self.set_cursor_char_insert(lo);
                }
                self.mode = Mode::Insert;
                self.snapshot_taken = true;
            }
            _ => {}
        }
    }

    /// Settle the cursor after a linewise delete: first non-blank of the line that
    /// now occupies the deleted lines' position. Shared by `apply_operator` and
    /// `visual_operate`.
    fn settle_after_linewise_delete(&mut self, first_line: usize) {
        self.cursor.line = first_line.min(self.last_line());
        self.cursor.col = self.first_non_blank(self.cursor.line);
    }

    /// Linewise change (`cc`/`S`, linewise-visual `c`): delete `lo..hi`, reopen a
    /// single empty line at `first_line`, and park the cursor there for insert.
    /// Shared by `apply_operator` and `visual_operate`.
    fn linewise_change(&mut self, lo: usize, hi: usize, first_line: usize) {
        self.delete_range(lo, hi);
        let at = self.buffer().line_start(first_line.min(self.last_line()));
        self.buffer_mut().insert_char(at, '\n');
        self.buffer_mut().normalize();
        self.cursor.line = first_line;
        self.cursor.col = 0;
    }

    fn visual_operate(&mut self, op: char) {
        let (lo, hi, linewise, first_line) = self.visual_range();
        self.push_undo();
        self.yank_range(lo, hi, linewise);
        match op {
            'd' => {
                self.delete_range(lo, hi);
                if linewise {
                    self.settle_after_linewise_delete(first_line);
                } else {
                    self.set_cursor_char(lo);
                }
                self.mode = Mode::Normal;
                self.clamp_cursor();
            }
            'y' => {
                if linewise {
                    self.cursor.line = first_line;
                } else {
                    self.set_cursor_char(lo);
                }
                self.mode = Mode::Normal;
                self.clamp_cursor();
            }
            'c' => {
                if linewise {
                    self.linewise_change(lo, hi, first_line);
                } else {
                    self.delete_range(lo, hi);
                    self.set_cursor_char_insert(lo);
                }
                self.mode = Mode::Insert;
                self.snapshot_taken = true;
            }
            _ => {}
        }
        self.reset_pending();
    }

    fn visual_range(&self) -> (usize, usize, bool, usize) {
        let a = self.visual_anchor;
        let b = self.cursor;
        if self.mode == Mode::VisualLine {
            let (la, lb) = (min(a.line, b.line), max(a.line, b.line));
            let lo = self.buffer().line_start(la);
            let hi = self
                .buffer()
                .line_start((lb + 1).min(self.buffer().line_count()));
            (lo, hi, true, la)
        } else {
            let ca = self.buffer().byte_at(a.line, a.col);
            let cb = self.buffer().byte_at(b.line, b.col);
            let lo = min(ca, cb);
            let hi = max(ca, cb) + 1;
            (lo, hi.min(self.last_char_idx().max(lo + 1)), false, 0)
        }
    }

    // ----- editing primitives ----------------------------------------------

    fn yank_range(&mut self, lo: usize, hi: usize, linewise: bool) {
        let (lo, hi) = self.snap_range(lo, hi);
        if lo >= hi {
            return;
        }
        self.register = Register {
            text: self.buffer().text.slice(lo..hi).to_string(),
            linewise,
        };
    }

    /// Remove `[lo, hi)` bytes, recording undo and keeping the buffer invariant.
    fn delete_range(&mut self, lo: usize, hi: usize) {
        let (lo, hi) = self.snap_range(lo, hi);
        if lo >= hi {
            return;
        }
        self.push_undo();
        self.buffer_mut().remove(lo..hi);
        self.buffer_mut().normalize();
        self.buffer_mut().modified = true;
    }

    /// Clamp a byte range into bounds and onto grapheme boundaries, so a
    /// motion-derived endpoint can never split a cluster (a no-op for ASCII).
    fn snap_range(&self, lo: usize, hi: usize) -> (usize, usize) {
        let hi = hi.min(self.buffer().len_bytes());
        let lo = self.grapheme_floor_abs(lo.min(hi));
        let hi = self.grapheme_ceil_abs(hi);
        (lo, hi)
    }

    fn delete_under_cursor(&mut self, count: usize) {
        let len = self.line_len();
        if len == 0 {
            return;
        }
        let lo = self.cursor_char();
        let line_end = self.buffer().byte_at(self.cursor.line, len);
        let (hi, _) = self.advance_graphemes(lo, count, line_end);
        self.yank_range(lo, hi, false);
        self.delete_range(lo, hi);
        self.clamp_cursor();
    }

    fn delete_before_cursor(&mut self, count: usize) {
        if self.cursor.col == 0 {
            return;
        }
        let new_col = self.cursor.col.saturating_sub(count);
        let lo = self.buffer().byte_at(self.cursor.line, new_col);
        let hi = self.cursor_char();
        self.yank_range(lo, hi, false);
        self.delete_range(lo, hi);
        self.cursor.col = new_col;
        self.clamp_cursor();
    }

    fn delete_to_eol(&mut self) {
        let len = self.line_len();
        let lo = self.cursor_char();
        let hi = self.buffer().byte_at(self.cursor.line, len);
        if lo < hi {
            self.yank_range(lo, hi, false);
            self.delete_range(lo, hi);
        }
        self.clamp_cursor();
    }

    fn replace_char(&mut self, c: char, count: usize) {
        let len = self.line_len();
        let lo = self.cursor_char();
        let line_end = self.buffer().byte_at(self.cursor.line, len);
        let (hi, crossed) = self.advance_graphemes(lo, count, line_end);
        // `r` does nothing unless `count` whole characters remain on the line.
        if crossed < count {
            return;
        }
        self.push_undo();
        self.buffer_mut().remove(lo..hi);
        let repl: String = std::iter::repeat(c).take(count).collect();
        self.buffer_mut().insert(lo, &repl);
        self.buffer_mut().modified = true;
        self.cursor.col =
            (lo - self.buffer().line_start(self.cursor.line)) + (count - 1) * c.len_utf8();
        self.clamp_cursor();
    }

    fn toggle_case(&mut self, count: usize) {
        if self.cursor.col >= self.line_len() {
            return;
        }
        self.push_undo();
        for _ in 0..count {
            if self.cursor.col >= self.line_len() {
                break;
            }
            let idx = self.cursor_char();
            let c = self.char_at(idx);
            let swapped: String = if c.is_uppercase() {
                c.to_lowercase().collect()
            } else {
                c.to_uppercase().collect()
            };
            self.buffer_mut().remove(idx..idx + c.len_utf8());
            self.buffer_mut().insert(idx, &swapped);
            let s = self.buffer().line(self.cursor.line);
            self.cursor.col = unicode::next_grapheme(&s, self.cursor.col);
        }
        self.buffer_mut().modified = true;
        self.clamp_cursor();
    }

    fn join_lines(&mut self, count: usize) {
        let joins = count.saturating_sub(1).max(1);
        self.push_undo();
        for _ in 0..joins {
            if self.cursor.line + 1 >= self.buffer().line_count() {
                break;
            }
            let cur_len = self.line_len();
            let eol = self.buffer().byte_at(self.cursor.line, cur_len);
            // Remove the newline and any leading whitespace of the next line.
            let next_start = self.buffer().line_start(self.cursor.line + 1);
            let mut ws_end = next_start;
            while ws_end < self.last_char_idx() {
                let c = self.char_at(ws_end);
                if c == ' ' || c == '\t' {
                    ws_end += 1;
                } else {
                    break;
                }
            }
            self.buffer_mut().remove(eol..ws_end);
            // Insert a single separating space unless the line was empty.
            if cur_len > 0 {
                self.buffer_mut().insert_char(eol, ' ');
            }
            self.cursor.col = cur_len;
        }
        self.buffer_mut().normalize();
        self.buffer_mut().modified = true;
        self.clamp_cursor();
    }

    fn open_line(&mut self, below: bool) {
        self.push_undo();
        if below {
            let at = self.buffer().byte_at(self.cursor.line, self.line_len());
            self.buffer_mut().insert_char(at, '\n');
            self.cursor.line += 1;
        } else {
            let at = self.buffer().line_start(self.cursor.line);
            self.buffer_mut().insert_char(at, '\n');
        }
        self.buffer_mut().normalize();
        self.cursor.col = 0;
        self.buffer_mut().modified = true;
        self.mode = Mode::Insert;
        self.snapshot_taken = true;
    }

    fn paste(&mut self, after: bool, count: usize) {
        if self.register.text.is_empty() {
            return;
        }
        self.push_undo();
        if self.register.linewise {
            let at = if after {
                self.buffer()
                    .line_start((self.cursor.line + 1).min(self.buffer().line_count()))
            } else {
                self.buffer().line_start(self.cursor.line)
            };
            let chunk = self.register.text.repeat(count);
            self.buffer_mut().insert(at, &chunk);
            self.buffer_mut().normalize();
            self.cursor.line = if after {
                self.cursor.line + 1
            } else {
                self.cursor.line
            };
            self.cursor.col = self.first_non_blank(self.cursor.line);
        } else {
            let len = self.line_len();
            let cur = self.cursor_char();
            let line_end = self.buffer().byte_at(self.cursor.line, len);
            // Paste *after* lands past the whole grapheme under the cursor, never
            // between a base char and its combining mark.
            let at = if after && len > 0 {
                self.next_grapheme_idx(cur).min(line_end)
            } else {
                cur
            };
            let chunk = self.register.text.repeat(count);
            // Byte length of the chunk's final grapheme, so the cursor lands on
            // it (not on a trailing combining mark) — vim leaves it on the last
            // pasted character.
            let last_len = chunk.len() - unicode::prev_grapheme(&chunk, chunk.len());
            let end = at + chunk.len();
            self.buffer_mut().insert(at, &chunk);
            self.set_cursor_char(end.saturating_sub(last_len));
        }
        self.buffer_mut().normalize();
        self.buffer_mut().modified = true;
        self.clamp_cursor();
    }

    // ----- insert mode ------------------------------------------------------

    fn enter_insert_at(&mut self, col: usize) {
        self.push_undo();
        self.snapshot_taken = true;
        self.cursor.col = col.min(self.line_len());
        self.mode = Mode::Insert;
    }

    fn handle_insert(&mut self, key: Key) {
        // The soft-tab marker is valid only for the keystroke immediately after a
        // `<Tab>` (or a chained soft-tab `<BS>`). Take it here so every key clears
        // it; only the Tab/Backspace arms thread it through and may re-arm it.
        let soft_tab = self.soft_tab.take();
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                if self.cursor.col > 0 {
                    let s = self.buffer().line(self.cursor.line);
                    self.cursor.col = unicode::prev_grapheme(&s, self.cursor.col);
                }
                self.clamp_cursor();
                self.snapshot_taken = false;
            }
            KeyCode::Enter => {
                let at = self.cursor_char();
                self.buffer_mut().insert_char(at, '\n');
                self.cursor.line += 1;
                self.cursor.col = 0;
                self.buffer_mut().modified = true;
            }
            KeyCode::Backspace => self.insert_backspace(soft_tab),
            KeyCode::Tab => self.insert_tab(soft_tab),
            KeyCode::Left => {
                let s = self.buffer().line(self.cursor.line);
                self.cursor.col = unicode::prev_grapheme(&s, self.cursor.col);
            }
            KeyCode::Right => {
                let s = self.buffer().line(self.cursor.line);
                self.cursor.col = unicode::next_grapheme(&s, self.cursor.col).min(s.len());
            }
            KeyCode::Up => self.move_vertical(-1, true),
            KeyCode::Down => self.move_vertical(1, true),
            KeyCode::Delete => {
                let len = self.line_len();
                if self.cursor.col < len {
                    let at = self.cursor_char();
                    let s = self.buffer().line(self.cursor.line);
                    let end = self.buffer().line_start(self.cursor.line)
                        + unicode::next_grapheme(&s, self.cursor.col);
                    self.buffer_mut().remove(at..end);
                    self.buffer_mut().modified = true;
                }
            }
            KeyCode::Char(c) => {
                let at = self.cursor_char();
                if self.mode == Mode::Replace && self.cursor.col < self.line_len() {
                    let s = self.buffer().line(self.cursor.line);
                    let end = self.buffer().line_start(self.cursor.line)
                        + unicode::next_grapheme(&s, self.cursor.col);
                    self.buffer_mut().remove(at..end);
                }
                self.buffer_mut().insert_char(at, c);
                self.cursor.col += c.len_utf8();
                self.buffer_mut().modified = true;
            }
            _ => {}
        }
    }

    /// Insert a tab at the cursor. The width it advances by is the buffer's
    /// resolved [`softtabstop`](crate::options::BufferOptions::effective_softtabstop)
    /// (the `softtabstop → shiftwidth → tabstop` chain), measured from the
    /// cursor's current virtual column so a partial tab only fills the remaining
    /// cells. With `expandtab` the fill is spaces; otherwise it's real tabs (each
    /// jumping a `tabstop` boundary) plus any trailing spaces.
    ///
    /// `prev` is the soft-tab marker carried from the previous keystroke; a fill
    /// of pure spaces re-arms the marker (chaining onto a contiguous prior run) so
    /// the next `<BS>` can undo this tab as a unit.
    fn insert_tab(&mut self, prev: Option<(usize, usize)>) {
        let opts = self.buffer().options;
        let unit = opts.effective_softtabstop();
        let start = self.cursor_virtcol();
        let target = start - (start % unit) + unit; // next multiple of the unit
        let ws = fill_indent(start, target, opts.effective_tabstop(), opts.expandtab);
        let begin = self.cursor.col;
        let at = self.cursor_char();
        // `ws` is ASCII (tabs/spaces), so its byte length is its column advance.
        let n = ws.len();
        self.buffer_mut().insert(at, &ws);
        self.cursor.col += n;
        self.buffer_mut().modified = true;
        // Only a pure-spaces fill is unit-deletable (a literal `\t` is one
        // grapheme the normal backspace already removes wholesale). Anchor at the
        // start of a contiguous prior soft-tab run, else where this tab began.
        self.soft_tab = if n > 0 && ws.bytes().all(|b| b == b' ') {
            let line = self.cursor.line;
            let anchor = match prev {
                Some((l, a)) if l == line && a <= begin && self.is_spaces(line, a, begin) => a,
                _ => begin,
            };
            Some((line, anchor))
        } else {
            None
        };
    }

    /// Whether `line[start..end]` (byte columns) is entirely ASCII spaces.
    fn is_spaces(&self, line: usize, start: usize, end: usize) -> bool {
        let s = self.buffer().line(line);
        s.as_bytes()
            .get(start..end)
            .is_some_and(|run| run.iter().all(|&b| b == b' '))
    }

    fn insert_backspace(&mut self, soft_tab: Option<(usize, usize)>) {
        if self.cursor.col > 0 {
            if self.softtab_backspace(soft_tab) {
                return;
            }
            let at = self.cursor_char();
            let start = self.buffer().line_start(self.cursor.line);
            let s = self.buffer().line(self.cursor.line);
            let prev_col = unicode::prev_grapheme(&s, self.cursor.col);
            self.buffer_mut().remove(start + prev_col..at);
            self.cursor.col = prev_col;
            self.buffer_mut().modified = true;
        } else if self.cursor.line > 0 {
            let prev_len = self.buffer().line_len(self.cursor.line - 1);
            let join_at = self.buffer().byte_at(self.cursor.line - 1, prev_len);
            self.buffer_mut().remove(join_at..join_at + 1);
            self.cursor.line -= 1;
            self.cursor.col = prev_len;
            self.buffer_mut().modified = true;
        }
    }

    /// `<BS>` over a soft tab: delete a whole [`softtabstop`] unit of spaces back
    /// to the previous tab boundary, rather than one space at a time — but *only*
    /// for whitespace a `<Tab>` inserted, tracked by `soft_tab` (the marker from
    /// the immediately preceding keystroke). Hand-typed spaces carry no marker, so
    /// they fall through to the normal one-character delete. The delete never
    /// crosses the run's anchor, and re-arms the marker while soft-tab spaces
    /// remain so a second `<BS>` peels off the next unit. Returns `true` when it
    /// handled the delete.
    ///
    /// [`softtabstop`]: crate::options::BufferOptions::effective_softtabstop
    fn softtab_backspace(&mut self, soft_tab: Option<(usize, usize)>) -> bool {
        let Some((line, anchor)) = soft_tab else {
            return false;
        };
        if line != self.cursor.line || anchor >= self.cursor.col {
            return false;
        }
        let opts = self.buffer().options;
        let unit = opts.effective_softtabstop();
        if unit <= 1 {
            return false;
        }
        let s = self.buffer().line(self.cursor.line);
        // The marked run must still be spaces from the anchor up to the cursor.
        if !self.is_spaces(line, anchor, self.cursor.col) {
            return false;
        }
        // Delete back one unit by virtual column, but never past the anchor.
        let ts = opts.effective_tabstop();
        let vcol = unicode::virtcol(&s, self.cursor.col, ts);
        let anchor_vcol = unicode::virtcol(&s, anchor, ts);
        let target_vcol = (((vcol - 1) / unit) * unit).max(anchor_vcol);
        let mut col = self.cursor.col;
        let mut vc = vcol;
        while vc > target_vcol && col > anchor {
            col -= 1;
            vc -= 1;
        }
        if col == self.cursor.col {
            return false;
        }
        let line_start = self.buffer().line_start(self.cursor.line);
        let range = line_start + col..line_start + self.cursor.col;
        self.buffer_mut().remove(range);
        self.cursor.col = col;
        self.buffer_mut().modified = true;
        // Keep peeling on the next <BS> while soft-tab spaces remain before us.
        if col > anchor {
            self.soft_tab = Some((line, anchor));
        }
        true
    }

    // ----- command-line mode ------------------------------------------------

    fn enter_command(&mut self) {
        self.mode = Mode::Command;
        self.cmdline.clear();
        self.cmdline_col = 0;
        self.cmdline_kind = CmdlineKind::Ex;
        self.hist_idx = None;
        self.message.clear();
        self.reset_pending();
    }

    /// Open the command line as a `/` (forward) or `?` (backward) search prompt.
    /// Same `Mode::Command` machinery as `:`; the kind routes `<CR>` to a search
    /// instead of an ex-command. `count` is the prefix on the opening `/`,`?`
    /// (`3/foo` finds the 3rd match), stashed for submit since `reset_pending`
    /// clears it.
    fn enter_search(&mut self, dir: SearchDir, count: usize) {
        self.mode = Mode::Command;
        self.cmdline.clear();
        self.cmdline_col = 0;
        self.cmdline_kind = CmdlineKind::Search(dir);
        self.pending_search_count = count.max(1);
        self.hist_idx = None;
        self.search_origin = self.cursor;
        self.message.clear();
        self.reset_pending();
    }

    fn handle_command(&mut self, key: Key) {
        // A `vim.fn.confirm` dialog resolves on a single keypress, not a typed
        // line, so it owns the key ahead of the line-editing path below.
        if matches!(self.cmdline_kind, CmdlineKind::Confirm) {
            self.handle_confirm(key);
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.cancel_cmdline();
                return;
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.cmdline);
                self.cmdline_col = 0;
                let kind = self.cmdline_kind;
                self.mode = Mode::Normal;
                match kind {
                    CmdlineKind::Ex => {
                        self.remember_ex(&text);
                        self.execute_ex(&text);
                    }
                    CmdlineKind::Search(dir) => {
                        // Commit from the saved origin, not the incsearch preview
                        // hop, so the count search lands deterministically (and
                        // identically to the no-incsearch path).
                        self.cursor = self.search_origin;
                        self.submit_search(&text, dir);
                    }
                    CmdlineKind::Prompt => {
                        // Hand the typed line to the waiting `vim.ui.input` callback
                        // (the server drains `prompt_results` and fires it).
                        self.cmdline_prompt.clear();
                        self.prompt_results.push(Some(text));
                    }
                    // A confirm dialog is resolved by `handle_confirm` (routed at
                    // the top of this fn), so it never reaches the line-submit path.
                    CmdlineKind::Confirm => {}
                }
                return;
            }
            // Backspacing an empty command line exits, like Esc. With text, it
            // deletes the char before the cursor (a no-op at the very start).
            KeyCode::Backspace if self.cmdline.is_empty() => {
                self.cancel_cmdline();
                return;
            }
            KeyCode::Backspace => self.cmdline_backspace(),
            // `<Del>` removes the char *under* the cursor.
            KeyCode::Delete => self.cmdline_delete(),
            // Within-line cursor motion: arrows by a char, Home/End (and the
            // vim-cmdline `<C-b>`/`<C-e>`) to the ends.
            KeyCode::Left => self.cmdline_cursor_left(),
            KeyCode::Right => self.cmdline_cursor_right(),
            KeyCode::Home => self.cmdline_col = 0,
            KeyCode::End => self.cmdline_col = self.cmdline.len(),
            KeyCode::Char('b') if key.ctrl => self.cmdline_col = 0,
            KeyCode::Char('e') if key.ctrl => self.cmdline_col = self.cmdline.len(),
            // Command-history recall (`<Up>`/`<C-p>` older, `<Down>`/`<C-n>`
            // newer), over whichever history the open prompt's kind selects.
            KeyCode::Up => self.cmdline_history_prev(),
            KeyCode::Down => self.cmdline_history_next(),
            KeyCode::Char('p') if key.ctrl => self.cmdline_history_prev(),
            KeyCode::Char('n') if key.ctrl => self.cmdline_history_next(),
            KeyCode::Char(c) if !key.ctrl => self.cmdline_insert(c),
            _ => {}
        }
        // The command line still has focus: refresh the live incsearch preview
        // for the just-edited search pattern (a no-op for an ex command line).
        if let CmdlineKind::Search(dir) = self.cmdline_kind {
            self.update_incsearch_preview(dir);
        }
    }

    /// Abandon the open command line and return to normal mode. For a search
    /// prompt this also rewinds the cursor to where the search began, undoing any
    /// incsearch preview hop (vim's `<Esc>`-cancels-search behavior).
    fn cancel_cmdline(&mut self) {
        if matches!(self.cmdline_kind, CmdlineKind::Search(_)) {
            self.cursor = self.search_origin;
            self.clamp_cursor();
            // Cancelling a `d/`-style search also abandons the pending operator.
            self.search_operator = None;
        }
        if matches!(self.cmdline_kind, CmdlineKind::Prompt) {
            // A cancelled `vim.ui.input` delivers `nil` to its callback (neovim's
            // `on_confirm(nil)` on `<Esc>`).
            self.cmdline_prompt.clear();
            self.prompt_results.push(None);
        }
        self.mode = Mode::Normal;
        self.cmdline.clear();
        self.cmdline_col = 0;
    }

    /// Open the command line as a scripted `vim.ui.input` prompt (Phase 8): a
    /// [`CmdlineKind::Prompt`] showing `label`, prefilled with `default` and the
    /// cursor at its end. `<CR>` / `<Esc>` deliver the result through
    /// [`Editor::prompt_results`]. The server calls this when it drains a queued
    /// `vim.ui.input` request, then fires the registered Lua callback on submit.
    pub fn open_prompt(&mut self, label: String, default: String) {
        self.mode = Mode::Command;
        self.cmdline = default;
        self.cmdline_col = self.cmdline.len();
        self.cmdline_kind = CmdlineKind::Prompt;
        self.cmdline_prompt = label;
    }

    /// Open the command line as a `vim.fn.confirm` button dialog: a
    /// [`CmdlineKind::Confirm`] showing `label` (the message plus rendered
    /// buttons). It has no editable line — a single keypress matching one of
    /// `accelerators` (lowercase, in button order) resolves to that button's
    /// 1-based index, `<CR>` selects `default` (1-based; `0` cancels), and
    /// `<Esc>` / `<C-c>` cancel with `0`. The chosen index is delivered as a
    /// string through [`Editor::prompt_results`] (the channel `Prompt` uses; the
    /// server forwards it to the blocked `vim.fn.confirm`). The server calls this
    /// when it drains a queued confirm request.
    pub fn open_confirm(&mut self, label: String, accelerators: Vec<String>, default: i64) {
        self.mode = Mode::Command;
        self.cmdline.clear();
        self.cmdline_col = 0;
        self.cmdline_kind = CmdlineKind::Confirm;
        self.cmdline_prompt = label;
        self.confirm_accelerators = accelerators;
        self.confirm_default = default;
    }

    /// Resolve an open confirm dialog from a single keypress, pushing the chosen
    /// 1-based index (or `0` to cancel) as a string onto [`Editor::prompt_results`]
    /// and returning to normal mode. An unrecognized key is ignored (the dialog
    /// stays open), matching neovim's "press one of the listed keys" behavior.
    fn handle_confirm(&mut self, key: Key) {
        let index = match key.code {
            KeyCode::Esc => Some(0),
            KeyCode::Char('c') if key.ctrl => Some(0),
            KeyCode::Enter => Some(self.confirm_default),
            KeyCode::Char(c) if !key.ctrl => {
                let pressed = c.to_ascii_lowercase().to_string();
                self.confirm_accelerators
                    .iter()
                    .position(|acc| *acc == pressed)
                    .map(|i| i as i64 + 1)
            }
            _ => None,
        };
        if let Some(index) = index {
            self.mode = Mode::Normal;
            self.cmdline_prompt.clear();
            self.confirm_accelerators.clear();
            self.prompt_results.push(Some(index.to_string()));
        }
    }

    /// The `vim.ui.input` / `vim.fn.confirm` prompt label (empty unless a
    /// [`CmdlineKind::Prompt`] or [`CmdlineKind::Confirm`] is open), projected
    /// into the [`View`] so the client renders it ahead of the editable line in
    /// place of the single-char [`Editor::cmdline_prefix`].
    pub(crate) fn cmdline_prompt(&self) -> &str {
        if matches!(
            self.cmdline_kind,
            CmdlineKind::Prompt | CmdlineKind::Confirm
        ) {
            &self.cmdline_prompt
        } else {
            ""
        }
    }

    // ----- command-line editing ---------------------------------------------

    /// Insert `c` at the command cursor and step the cursor past it.
    fn cmdline_insert(&mut self, c: char) {
        self.cmdline.insert(self.cmdline_col, c);
        self.cmdline_col += c.len_utf8();
    }

    /// Delete the char before the command cursor (`<BS>`); a no-op at the start.
    fn cmdline_backspace(&mut self) {
        if let Some(prev) = self.cmdline_prev_boundary() {
            self.cmdline.remove(prev);
            self.cmdline_col = prev;
        }
    }

    /// Delete the char under the command cursor (`<Del>`); a no-op at the end.
    fn cmdline_delete(&mut self) {
        if self.cmdline_col < self.cmdline.len() {
            self.cmdline.remove(self.cmdline_col);
        }
    }

    /// Move the command cursor one char left (`<Left>`).
    fn cmdline_cursor_left(&mut self) {
        if let Some(prev) = self.cmdline_prev_boundary() {
            self.cmdline_col = prev;
        }
    }

    /// Move the command cursor one char right (`<Right>`).
    fn cmdline_cursor_right(&mut self) {
        if let Some(c) = self.cmdline[self.cmdline_col..].chars().next() {
            self.cmdline_col += c.len_utf8();
        }
    }

    /// Byte offset of the char boundary immediately before the command cursor,
    /// or `None` when it's already at the start. (Char-aware so multibyte input
    /// in the command line edits one whole character at a time.)
    fn cmdline_prev_boundary(&self) -> Option<usize> {
        self.cmdline[..self.cmdline_col]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
    }

    // ----- search -----------------------------------------------------------

    /// Run a search submitted from the `/`,`?` command line. The line is split on
    /// its last unescaped separator into a pattern and a trailing offset
    /// (`/pat/e`). An empty pattern repeats the last search (keeping its pattern,
    /// the just-typed direction, and — unless this line carries its own separator
    /// — its offset); with no previous pattern that is `E35`. The count prefixed
    /// onto the opening `/`,`?` finds the Nth match. A pending operator (`d/`)
    /// applies over the match instead of moving.
    fn submit_search(&mut self, line: &str, dir: SearchDir) {
        let (core, off) = split_search_offset(line, dir.prefix());
        let had_sep = core.len() != line.len();
        let pattern = if core.is_empty() {
            match &self.last_search {
                Some((p, _, _)) => p.clone(),
                None => {
                    self.echo("E35: No previous regular expression");
                    return;
                }
            }
        } else {
            core.clone()
        };
        // A bare empty line repeats verbatim (offset included); any explicit
        // separator — even `//e` over an empty pattern — sets a fresh offset.
        let offset = if had_sep || !core.is_empty() {
            off
        } else {
            self.last_search
                .as_ref()
                .map_or(SearchOffset::None, |(_, _, o)| *o)
        };
        self.remember_search(&pattern);
        self.last_search = Some((pattern.clone(), dir, offset));
        let op = self.search_operator.take();
        let count = self.pending_search_count.max(1);
        self.run_search(&pattern, dir, offset, count, op);
    }

    /// Record a submitted pattern in the search history, skipping a consecutive
    /// duplicate (vim collapses repeats).
    fn remember_search(&mut self, pattern: &str) {
        if self.search_history.last().map(String::as_str) != Some(pattern) {
            self.search_history.push(pattern.to_string());
        }
    }

    /// Record an interactively-submitted `:` command in the ex history, skipping
    /// an empty line or a consecutive duplicate (vim collapses repeats).
    fn remember_ex(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        if !cmd.is_empty() && self.ex_history.last().map(String::as_str) != Some(cmd) {
            self.ex_history.push(cmd.to_string());
        }
    }

    /// `n` (same direction) / `N` (opposite) — repeat the last search `count`
    /// times, reusing its offset. `E35` when there is no last search.
    fn search_repeat(&mut self, same: bool, count: usize) {
        let Some((pattern, last_dir, offset)) = self.last_search.clone() else {
            self.echo("E35: No previous regular expression");
            return;
        };
        let dir = if same { last_dir } else { last_dir.opposite() };
        self.run_search(&pattern, dir, offset, count.max(1), None);
    }

    /// `*`/`#` (and `g*`/`g#`): search for the word under the cursor — forward for
    /// `*`, backward for `#`. `bounded` wraps it in `\b…\b` (the plain `*`/`#`,
    /// whole-word) versus a bare substring (`g*`/`g#`). `E348` with no word under
    /// the cursor.
    fn search_word_under_cursor(&mut self, dir: SearchDir, bounded: bool, count: usize) {
        let Some(word) = self.word_under_cursor() else {
            self.echo("E348: No string under cursor");
            return;
        };
        let pattern = if bounded {
            format!(r"\b{word}\b")
        } else {
            word
        };
        self.remember_search(&pattern);
        self.last_search = Some((pattern.clone(), dir, SearchOffset::None));
        self.run_search(&pattern, dir, SearchOffset::None, count.max(1), None);
    }

    /// The keyword (alphanumerics + `_`) under the cursor, or the next one on the
    /// line if the cursor sits on a non-word char; `None` if the line has none
    /// from the cursor on. Drives `*`/`#`.
    fn word_under_cursor(&self) -> Option<String> {
        let line = self.buffer().line(self.cursor.line);
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        // First word char at or after the cursor column.
        let start = chars.iter().position(|(b, _)| *b >= self.cursor.col)?;
        let mut k = start;
        while k < chars.len() && !is_word(chars[k].1) {
            k += 1;
        }
        if k >= chars.len() {
            return None;
        }
        // If the cursor itself was on the word, take it from its start.
        let on_word = chars
            .get(start)
            .is_some_and(|(b, c)| *b == self.cursor.col && is_word(*c));
        let mut lo = k;
        if on_word {
            while lo > 0 && is_word(chars[lo - 1].1) {
                lo -= 1;
            }
        }
        let mut hi = k;
        while hi < chars.len() && is_word(chars[hi].1) {
            hi += 1;
        }
        Some(chars[lo..hi].iter().map(|(_, c)| *c).collect())
    }

    /// Whether this search should ignore case by *option*: `ignorecase`, unless
    /// `smartcase` is also on and the pattern carries an uppercase character (then
    /// it stays case-sensitive). This is the default the regex compiler starts
    /// from; an embedded `\c`/`\C` in the pattern overrides it.
    fn search_ignorecase(&self, pattern: &str) -> bool {
        self.options.ignorecase
            && !(self.options.smartcase && pattern.chars().any(|c| c.is_uppercase()))
    }

    /// Compile `pattern` (a standard regex) with this editor's case options.
    fn compile_search(&self, pattern: &str) -> Result<SearchRegex, String> {
        SearchRegex::compile(pattern, self.search_ignorecase(pattern))
    }

    /// The next match of the compiled `re` in `dir` from byte offset `from`, as
    /// `(primary, wrapped)` whole-buffer `(start, end)` ranges. `primary` is the
    /// match in the search direction without wrapping; `wrapped` is the first
    /// match from the opposite end (used when `wrapscan` lets the search wrap).
    /// Forward starts one grapheme past `from` so a match *under* it isn't an
    /// immediate self-hit; backward looks left of it. Matching is line-by-line;
    /// side-effect free (the shared core of `run_search` and the incsearch
    /// preview).
    fn search_matches(
        &self,
        re: &SearchRegex,
        dir: SearchDir,
        from: usize,
    ) -> (Option<MatchRange>, Option<MatchRange>) {
        match dir {
            SearchDir::Forward => (
                self.match_forward_from(re, self.next_grapheme_idx(from)),
                self.match_forward_from(re, 0),
            ),
            SearchDir::Backward => (
                self.match_backward_before(re, from),
                self.match_backward_before(re, self.buffer().len_bytes()),
            ),
        }
    }

    /// The first match of `re` whose start is at or after byte `start`, scanning
    /// lines downward to the end of the buffer, as a whole-buffer `(start, end)`
    /// range. Walks each line's non-overlapping match sequence (see
    /// `SearchRegex::find_from`), so a greedy pattern doesn't yield a match that
    /// overlaps the one the cursor already sits in. `None` if no match starts in
    /// `[start, end_of_buffer)`.
    fn match_forward_from(&self, re: &SearchRegex, start: usize) -> Option<MatchRange> {
        let buf = self.buffer();
        let line_count = buf.line_count();
        let mut line = buf.byte_to_line(start.min(self.last_char_idx()));
        let mut col = start.saturating_sub(buf.line_start(line));
        while line < line_count {
            let text = buf.line(line);
            if let Some((s, e)) = re.find_from(&text, col) {
                let base = buf.line_start(line);
                return Some((base + s, base + e));
            }
            line += 1;
            col = 0;
        }
        None
    }

    /// The last match of `re` that *starts* before byte `limit`, scanning lines
    /// upward to the top, as a whole-buffer `(start, end)` range. `None` if
    /// nothing matches before `limit`.
    fn match_backward_before(&self, re: &SearchRegex, limit: usize) -> Option<MatchRange> {
        let buf = self.buffer();
        let limit_line = buf.byte_to_line(limit.min(self.last_char_idx()));
        let limit_col = limit.saturating_sub(buf.line_start(limit_line));
        let mut line = limit_line as isize;
        while line >= 0 {
            let l = line as usize;
            let text = buf.line(l);
            // On the limit line a match must start strictly before the cursor;
            // earlier lines admit any match.
            let cap = if l == limit_line {
                limit_col
            } else {
                text.len() + 1
            };
            if let Some((s, e)) = re
                .find_all(&text)
                .into_iter()
                .take_while(|(s, _)| *s < cap)
                .last()
            {
                let base = buf.line_start(l);
                return Some((base + s, base + e));
            }
            line -= 1;
        }
        None
    }

    /// Find the `count`-th match of `pattern` (a standard regex) in `dir` from the
    /// cursor and act on it: move the cursor (repositioned by `offset`), or — when
    /// `op` is `Some` — apply that operator over the `[origin, match]` motion
    /// instead of moving. Sets the `/pattern` echo, or the BOTTOM/TOP notice when
    /// it wrapped. A miss is `E486` with `wrapscan` (or `E385`/`E384` without it),
    /// an uncompilable pattern `E383`; all leave the cursor unmoved.
    fn run_search(
        &mut self,
        pattern: &str,
        dir: SearchDir,
        offset: SearchOffset,
        count: usize,
        op: Option<char>,
    ) {
        if pattern.is_empty() {
            return;
        }
        // A committed search turns on `hlsearch` highlighting (cleared by `:noh`).
        self.search_active = true;
        let re = match self.compile_search(pattern) {
            Ok(re) => re,
            Err(e) => {
                self.echo(e);
                return;
            }
        };
        // Walk `count` matches over a local cursor so a miss leaves the real one
        // put; the offset and any operator apply once, to the final match.
        let origin = self.cursor_char();
        let mut from = origin;
        let mut last = None;
        let mut wrapped = false;
        for _ in 0..count {
            let (primary, wrap) = self.search_matches(&re, dir, from);
            let (hit, this_wrap) = match primary {
                Some(r) => (r, false),
                None => match wrap.filter(|_| self.options.wrapscan) {
                    Some(r) => (r, true),
                    None => {
                        last = None;
                        break;
                    }
                },
            };
            wrapped = this_wrap;
            from = hit.0;
            last = Some(hit);
        }

        let Some((ms, me)) = last else {
            self.echo(if self.options.wrapscan {
                format!("E486: Pattern not found: {pattern}")
            } else {
                match dir {
                    SearchDir::Forward => {
                        format!("E385: search hit BOTTOM without match for: {pattern}")
                    }
                    SearchDir::Backward => {
                        format!("E384: search hit TOP without match for: {pattern}")
                    }
                }
            });
            return;
        };

        self.place_with_offset(ms, me, offset);
        if let Some(op) = op {
            // The cursor now rests on the (offset-adjusted) match; the operator
            // spans from there back to where the search began.
            let m = MotionResult::horizontal(origin, offset.motion_kind());
            self.apply_operator(op, m);
        } else if wrapped {
            self.echo(match dir {
                SearchDir::Forward => "search hit BOTTOM, continuing at TOP",
                SearchDir::Backward => "search hit TOP, continuing at BOTTOM",
            });
        } else {
            self.message = format!("{}{}", dir.prefix(), pattern);
        }
    }

    /// Settle the cursor for a match spanning bytes `[ms, me)` under `offset`: on
    /// the match start (no offset / `s`, shifted by its char count), on the
    /// match's last char (`e`, likewise shifted), or `n` lines away at the first
    /// non-blank (a line offset).
    fn place_with_offset(&mut self, ms: usize, me: usize, offset: SearchOffset) {
        match offset {
            SearchOffset::None => self.move_to_match(ms),
            SearchOffset::Start(n) => {
                let t = self.shift_graphemes(ms, n);
                self.move_to_match(t);
            }
            SearchOffset::End(n) => {
                let base = if me > ms {
                    self.prev_grapheme_idx(me)
                } else {
                    ms
                };
                let t = self.shift_graphemes(base, n);
                self.move_to_match(t);
            }
            SearchOffset::Line(n) => {
                let last_line = self.last_line() as isize;
                let line =
                    (self.buffer().byte_to_line(ms) as isize + n).clamp(0, last_line) as usize;
                self.cursor.line = line;
                self.cursor.col = self.first_non_blank(line);
                self.clamp_cursor();
            }
        }
    }

    /// Byte offset `n` grapheme clusters from `base` (forward for `n >= 0`,
    /// backward otherwise), clamped to the buffer.
    fn shift_graphemes(&self, base: usize, n: isize) -> usize {
        if n >= 0 {
            self.advance_graphemes(base, n as usize, self.last_char_idx())
                .0
        } else {
            let mut b = base;
            for _ in 0..n.unsigned_abs() {
                b = self.prev_grapheme_idx(b);
            }
            b
        }
    }

    /// Settle the cursor on a search match at byte offset `byte`.
    fn move_to_match(&mut self, byte: usize) {
        self.set_cursor_char(byte);
        self.clamp_cursor();
    }

    /// Refresh the live `incsearch` preview from the typed command line: jump the
    /// cursor (and, via the caller's `ensure_visible`, the viewport) to the match
    /// the pattern would land on, always measured from the fixed search origin so
    /// the preview doesn't drift as the pattern is edited. A no-op when
    /// `incsearch` is off; an empty pattern or a miss just rests at the origin.
    /// Side-effect free beyond the cursor — no message, history, or `last_search`
    /// change (those happen only on the committed `<CR>`).
    fn update_incsearch_preview(&mut self, dir: SearchDir) {
        if !self.options.incsearch {
            return;
        }
        self.cursor = self.search_origin;
        // Preview the pattern only; a trailing `/offset` repositions the preview.
        let (core, offset) = split_search_offset(&self.cmdline, dir.prefix());
        if let Some((ms, me)) = self.preview_match(&core, dir) {
            self.place_with_offset(ms, me, offset);
        } else {
            self.clamp_cursor();
        }
    }

    /// The match range the incsearch preview should rest on for `pattern` from the
    /// search origin in `dir`, honoring `wrapscan`. `None` for an empty pattern, a
    /// pattern that doesn't compile, or one that matches nowhere (the cursor then
    /// stays at the origin).
    fn preview_match(&self, pattern: &str, dir: SearchDir) -> Option<MatchRange> {
        if pattern.is_empty() {
            return None;
        }
        let re = self.compile_search(pattern).ok()?;
        let from = self
            .buffer()
            .byte_at(self.search_origin.line, self.search_origin.col);
        let (primary, wrapped) = self.search_matches(&re, dir, from);
        primary.or(if self.options.wrapscan { wrapped } else { None })
    }

    /// Per visible row (`count` rows from buffer line `base`), the screen-column
    /// spans to paint for search: `(matches, current)`. `matches[row]` lists
    /// every occurrence of the active pattern on that row (the `Search` group);
    /// `current[row]` is the one occurrence the live incsearch preview rests on
    /// (the `IncSearch` group), `None` elsewhere. Both are empty/all-`None` when
    /// nothing should show: while typing an `incsearch` the live command line
    /// lights up, otherwise the last search does — but only while `hlsearch` is on
    /// and a search is active (cleared by `:noh`).
    pub(crate) fn search_highlights_in(
        &self,
        buf: &Buffer,
        cursor: Cursor,
        focused: bool,
        base: usize,
        count: usize,
    ) -> (SearchSpans, IncSearchSpans) {
        let mut matches = vec![Vec::new(); count];
        let mut current = vec![None; count];

        let search_dir = match self.cmdline_kind {
            CmdlineKind::Search(dir) => Some(dir),
            CmdlineKind::Ex | CmdlineKind::Prompt | CmdlineKind::Confirm => None,
        };
        // The live incsearch preview belongs to the focused window (the command
        // line is there); `hlsearch` is global and shows in every window.
        let incsearch = focused
            && self.mode == Mode::Command
            && search_dir.is_some()
            && self.options.incsearch
            && !self.cmdline.is_empty();
        let pattern = if incsearch {
            // Highlight the pattern only, not the `/pat/offset` suffix being typed.
            let sep = search_dir.map_or('/', SearchDir::prefix);
            Some(split_search_offset(&self.cmdline, sep).0)
        } else if self.options.hlsearch && self.search_active {
            self.last_search.as_ref().map(|(p, _, _)| p.clone())
        } else {
            None
        };
        let Some(pattern) = pattern.filter(|p| !p.is_empty()) else {
            return (matches, current);
        };
        // A pattern still mid-edit (incsearch) may not compile yet; show nothing.
        let Ok(re) = self.compile_search(&pattern) else {
            return (matches, current);
        };
        let line_count = buf.line_count();

        for (row, row_spans) in matches.iter_mut().enumerate() {
            let buf_line = base + row;
            if buf_line >= line_count {
                break;
            }
            let text = buf.line(buf_line);
            let ts = buf.options.effective_tabstop();
            for (s, e) in re.find_all(&text) {
                let span = (
                    unicode::virtcol(&text, s, ts),
                    unicode::virtcol(&text, e, ts),
                );
                row_spans.push(span);
                // The preview cursor sits on the start of its match, so an exact
                // column hit on the cursor's line marks the current match.
                if incsearch && buf_line == cursor.line && s == cursor.col {
                    current[row] = Some(span);
                }
            }
        }
        (matches, current)
    }

    /// The history list for the open command line's kind: ex commands for `:`,
    /// search patterns for `/`,`?`. A `vim.ui.input` prompt has no history.
    fn active_history(&self) -> &[String] {
        match self.cmdline_kind {
            CmdlineKind::Ex => &self.ex_history,
            CmdlineKind::Search(_) => &self.search_history,
            CmdlineKind::Confirm => &[],
            CmdlineKind::Prompt => &[],
        }
    }

    /// `<Up>`/`<C-p>` in the command line — recall the previous history entry
    /// (the newest first), replacing the typed line. A no-op with an empty
    /// history.
    fn cmdline_history_prev(&mut self) {
        let len = self.active_history().len();
        if len == 0 {
            return;
        }
        let idx = match self.hist_idx {
            None => len - 1,
            Some(i) => i.saturating_sub(1),
        };
        self.hist_idx = Some(idx);
        self.cmdline = self.active_history()[idx].clone();
        self.cmdline_col = self.cmdline.len();
    }

    /// `<Down>`/`<C-n>` in the command line — move to a newer history entry, or
    /// back to an empty line once past the newest.
    fn cmdline_history_next(&mut self) {
        let len = self.active_history().len();
        match self.hist_idx {
            Some(i) if i + 1 < len => {
                self.hist_idx = Some(i + 1);
                self.cmdline = self.active_history()[i + 1].clone();
                self.cmdline_col = self.cmdline.len();
            }
            Some(_) => {
                self.hist_idx = None;
                self.cmdline.clear();
                self.cmdline_col = 0;
            }
            None => {}
        }
    }

    /// The command-line prompt character for the current [`CmdlineKind`]: `:` for
    /// an ex command, `/` / `?` for a forward / backward search. The client draws
    /// it at the head of the command line.
    pub(crate) fn cmdline_prefix(&self) -> char {
        match self.cmdline_kind {
            CmdlineKind::Ex => ':',
            CmdlineKind::Search(dir) => dir.prefix(),
            // A `vim.ui.input` prompt and a `vim.fn.confirm` dialog render their
            // multi-char label via `cmdline_prompt()` instead; the single-char
            // prefix is unused (a space keeps the projection well-formed).
            CmdlineKind::Prompt | CmdlineKind::Confirm => ' ',
        }
    }

    /// The command cursor's position as a character offset from the start of
    /// [`Editor::cmdline`], for the client to place the terminal cursor.
    pub(crate) fn cmdline_cursor(&self) -> usize {
        self.cmdline[..self.cmdline_col].chars().count()
    }

    fn execute_ex(&mut self, raw: &str) {
        let cmd = raw.trim();
        if cmd.is_empty() {
            return;
        }
        if let Ok(n) = cmd.parse::<usize>() {
            let line = n.saturating_sub(1).min(self.last_line());
            self.cursor.line = line;
            self.cursor.col = self.first_non_blank(line);
            return;
        }

        let (name, bang, args) = split_ex(cmd);
        match name {
            "w" | "write" => self.ex_write(args),
            "q" | "quit" => self.ex_quit(bang),
            "wq" | "x" | "xit" | "exit" => {
                // Write the current buffer, then `:q` it (close the window, or
                // quit on the last window). A failed write leaves the buffer
                // modified, so a last-window quit then reports it.
                self.ex_write(args);
                self.ex_quit(bang);
            }
            "qa" | "qall" | "quita" | "quitall" => self.ex_quit_all(bang),
            "wa" | "wall" => self.ex_write_all(),
            "wqa" | "xa" | "xall" => {
                self.ex_write_all();
                self.ex_quit_all(bang);
            }
            "e" | "edit" => self.ex_edit(args, bang),
            "ene" | "enew" => self.ex_enew(),
            "sp" | "spl" | "split" => self.ex_split(SplitDir::Horizontal, args),
            "vs" | "vsp" | "vsplit" | "vspl" => self.ex_split(SplitDir::Vertical, args),
            "new" => self.ex_new(SplitDir::Horizontal),
            "vne" | "vnew" => self.ex_new(SplitDir::Vertical),
            "clo" | "close" => self.close_window(),
            "hid" | "hide" => self.close_window(),
            "on" | "only" => self.only_window(),
            "res" | "resize" => self.ex_resize(SplitDir::Horizontal, args),
            "vert" | "vertical" | "ver" => self.ex_vertical(args),
            "ls" | "buffers" | "files" => self.ex_buffers(),
            "b" | "bu" | "buf" | "buffer" => {
                if let Some(id) = self.resolve_buffer(args) {
                    self.switch_buffer(id);
                }
            }
            "bn" | "bnext" => self.ex_bnext(parse_count_arg(args)),
            "bp" | "bN" | "bprev" | "bprevious" | "bNext" => self.ex_bprev(parse_count_arg(args)),
            "bf" | "bfirst" | "br" | "brewind" => self.ex_bfirst(),
            "bl" | "blast" => self.ex_blast(),
            "bd" | "bdel" | "bdelete" | "bw" | "bwipe" | "bwipeout" => self.ex_bdelete(args, bang),
            "lua" => self.lua_queue.push(args.to_string()),
            "sleep" | "sl" => match parse_sleep(args) {
                Ok(ms) => self.pending_sleep = Some(ms),
                Err(e) => self.echo(e),
            },
            "mes" | "messages" | "message" => self.ex_messages(),
            "panelopen" | "panelo" => {
                if !self.reopen_last_panel() {
                    self.echo("No panel to reopen");
                }
            }
            // `:setlocal`/`:setl` shares the handler: buffer-local options
            // (tabstop/shiftwidth/expandtab) live on the current buffer, which is
            // exactly what `:set` already targets for them.
            "set" | "se" | "setlocal" | "setl" => self.ex_set(args),
            "noh" | "nohlsearch" => self.search_active = false,
            // `:hi clear` resets the registry to defaults (empty); other `:hi`
            // forms are no-ops — catppuccin defines groups via the API, not `:hi`.
            "hi" | "highlight" => {
                if args.trim() == "clear" {
                    self.highlights.clear();
                }
            }
            // Unknown to the core: defer to the server, which resolves it
            // against Lua user commands (or reports the unknown-command error).
            _ => self.deferred_commands.push(cmd.to_string()),
        }
    }

    fn ex_write(&mut self, args: &str) {
        let path = if args.is_empty() {
            None
        } else {
            Some(PathBuf::from(args))
        };
        match self.buffer_mut().write(path) {
            Ok((bytes, lines)) => {
                // The current state is now what's on disk — undoing/redoing back
                // to it should read as clean.
                let ob = self.cur_mut();
                ob.saved_seq = Some(ob.cur_seq);
                let name = self
                    .buffer()
                    .path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                self.echo(format!("\"{name}\" {lines}L, {bytes}B written"));
            }
            Err(e) => self.echo(e.to_string()),
        }
    }

    /// `:q` / `<C-w>q` — close the focused window. With more than one window open
    /// this is exactly `<C-w>c`: the window goes away and its buffer stays in the
    /// store (other windows may still show it, or it's reachable via `:b`), so
    /// there is nothing to lose and no modified guard — closing a non-last window
    /// onto a modified buffer is fine. Only the **last** window is a real editor
    /// quit, which defers to [`Self::ex_quit_all`] (and its `E37` guard).
    fn ex_quit(&mut self, bang: bool) {
        if self.windows.count() > 1 {
            self.close_window();
            return;
        }
        self.ex_quit_all(bang);
    }

    /// `:qa` (and last-window `:q`) — quit the *editor*, but only if nothing
    /// would be lost. With `!`, exit unconditionally (discarding every buffer).
    /// Otherwise, if any buffer has unsaved changes, *don't* quit: switch the
    /// window to that buffer (the current one if it's the one modified, else the
    /// lowest-numbered modified buffer) and report `E37`, so the user sees what's
    /// blocking the quit. With no modified buffers, exit.
    fn ex_quit_all(&mut self, bang: bool) {
        if bang {
            self.should_quit = true;
            return;
        }
        if self.buffer().modified {
            // Already showing the offending buffer.
            self.echo("E37: No write since last change (add ! to override)");
            return;
        }
        match self.first_modified_buffer() {
            Some(id) => {
                // Surface the blocking buffer, then warn (the switch clears the
                // message, so set it afterwards).
                self.switch_buffer(id);
                self.echo(format!(
                    "E37: No write since last change for buffer {} (add ! to override)",
                    id.0
                ));
            }
            None => self.should_quit = true,
        }
    }

    /// The lowest-numbered buffer with unsaved changes, if any.
    fn first_modified_buffer(&self) -> Option<BufferId> {
        self.buffers
            .map
            .iter()
            .find(|(_, ob)| ob.buffer.modified)
            .map(|(id, _)| *id)
    }

    fn ex_edit(&mut self, args: &str, bang: bool) {
        if args.is_empty() {
            self.echo("E32: No file name");
            return;
        }
        let path = PathBuf::from(args);

        // Re-editing the current file reloads it in place (`:e` / `:e!`),
        // discarding unsaved changes — so the modified guard applies here.
        if self.buffer().path.as_deref() == Some(path.as_path()) {
            if self.buffer().modified && !bang {
                self.echo("E37: No write since last change (add ! to override)");
                return;
            }
            self.load_into_current(&path);
            return;
        }

        // The file is already open in another buffer: just switch to it. The
        // current buffer stays in the list (vim's `hidden` behavior), so there is
        // nothing to lose and no modified guard.
        if let Some(id) = self.find_buffer_by_path(&path) {
            self.switch_buffer(id);
            return;
        }

        // A new file. Reuse a throwaway `[No Name]` buffer if that's all we have
        // (so the first `:e` doesn't strand an empty buffer 1); otherwise open it
        // in a fresh buffer and switch, keeping the current one open.
        if self.current_is_throwaway() {
            self.load_into_current(&path);
        } else {
            match Buffer::from_file(&path) {
                Ok(buf) => {
                    let id = self.add_buffer(buf);
                    self.switch_buffer(id);
                }
                Err(e) => self.echo(e.to_string()),
            }
        }
    }

    /// `:split [file]` / `:vsplit [file]` — split the focused window, then (with a
    /// file argument) `:edit` it in the new window. With no argument both windows
    /// show the same buffer.
    fn ex_split(&mut self, dir: SplitDir, args: &str) {
        self.split(dir);
        let file = args.trim();
        if !file.is_empty() {
            self.ex_edit(file, false);
        }
    }

    /// `:new` / `:vnew` — split the focused window and open a fresh `[No Name]`
    /// buffer in the new window.
    fn ex_new(&mut self, dir: SplitDir) {
        self.split(dir);
        self.ex_enew();
    }

    /// `:res[ize] {n}` — set the focused window's height; `:vertical resize {n}`
    /// routes here with `axis = Vertical` for its width. `{n}` is absolute; `+n`
    /// / `-n` are relative. A bare `:resize` maximizes along the axis (vim). The
    /// extent is text rows for height (the status line is excluded, as in vim) and
    /// columns for width.
    fn ex_resize(&mut self, axis: SplitDir, args: &str) {
        let arg = args.trim();
        if arg.is_empty() {
            self.maximize_window(axis);
            return;
        }
        let delta = if let Some(rest) = arg.strip_prefix('+') {
            rest.trim().parse::<isize>().ok()
        } else if let Some(rest) = arg.strip_prefix('-') {
            rest.trim().parse::<isize>().ok().map(|n| -n)
        } else {
            // Absolute: aim for `target` and resize by the difference from the
            // current extent.
            match arg.parse::<usize>() {
                Ok(target) => {
                    let rect = self.windows.cur().rect;
                    let current = match axis {
                        SplitDir::Horizontal => rect.height.saturating_sub(1),
                        SplitDir::Vertical => rect.width,
                    };
                    Some(target as isize - current as isize)
                }
                Err(_) => None,
            }
        };
        match delta {
            Some(d) => self.resize_window(axis, d),
            None => self.echo("E487: Argument must be a number"),
        }
    }

    /// `:vertical {cmd}` — the `vertical` modifier. Re-routes the split / resize
    /// commands to their vertical (width-dividing) form.
    fn ex_vertical(&mut self, args: &str) {
        let (name, _bang, rest) = split_ex(args.trim());
        match name {
            "res" | "resize" => self.ex_resize(SplitDir::Vertical, rest),
            "sp" | "spl" | "split" => self.ex_split(SplitDir::Vertical, rest),
            "new" => self.ex_new(SplitDir::Vertical),
            "" => self.echo("E471: Argument required"),
            _ => self.deferred_commands.push(format!("vertical {args}")),
        }
    }

    /// `:enew` — open a new, empty `[No Name]` buffer in the window. Reuses a
    /// throwaway current buffer rather than stacking another empty one.
    fn ex_enew(&mut self) {
        if self.current_is_throwaway() {
            return;
        }
        let id = self.add_buffer(Buffer::empty());
        self.switch_buffer(id);
    }

    /// `:wall` — write every modified buffer that has a file name.
    fn ex_write_all(&mut self) {
        let mut written = 0;
        for ob in self.buffers.map.values_mut() {
            if ob.buffer.modified && ob.buffer.path.is_some() && ob.buffer.write(None).is_ok() {
                ob.saved_seq = Some(ob.cur_seq);
                written += 1;
            }
        }
        self.echo(format!("{written} buffer(s) written"));
    }

    // ----- buffer list ------------------------------------------------------

    /// `:ls` / `:buffers` — list the open buffers into the bottom panel, one per
    /// row (id-sorted), with vim's flag columns: `%` current / `#` alternate,
    /// `a` active / `h` hidden, `+` modified.
    fn ex_buffers(&mut self) {
        let current = self.cur_buffer();
        let alternate = self.alternate;
        let live_cursor = self.cursor.line;
        let mut lines = Vec::new();
        let mut current_row = 0;
        for (row, (id, ob)) in self.buffers.map.iter().enumerate() {
            if *id == current {
                current_row = row;
            }
            let flag = if *id == current {
                '%'
            } else if Some(*id) == alternate {
                '#'
            } else {
                ' '
            };
            let active = if *id == current { 'a' } else { 'h' };
            let modified = if ob.buffer.modified { '+' } else { ' ' };
            let name = ob
                .buffer
                .path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "[No Name]".to_string());
            let lnum = if *id == current {
                live_cursor
            } else {
                ob.saved_cursor.line
            } + 1;
            lines.push(format!(
                "{:>3} {flag}{active} {modified} \"{name}\" line {lnum}",
                id.0
            ));
        }
        self.open_panel("Buffers", lines, false, current_row);
        // Wire `<CR>` to jump to the picked buffer, using the same scripting
        // `on_select` mechanism a plugin would: a prelude helper parses the
        // buffer number off the selected line and switches to it. Queued as Lua
        // (like `:lua`); the server runs it after the panel is open, so it sets
        // the handler on this very panel.
        self.lua_queue
            .push("vim.panel.on_select(vim._panel_select_buffer)".to_string());
    }

    /// `:messages` — show the message history in the bottom panel, opened
    /// scrolled to the end with the newest line selected.
    fn ex_messages(&mut self) {
        let lines = self.messages.clone();
        let last = lines.len().saturating_sub(1);
        self.open_panel("Messages", lines, false, last);
    }

    /// Resolve a `:buffer` / `:bdelete` argument to a buffer id: empty = current,
    /// `#` = alternate, a number = that buffer id, otherwise a file-name
    /// substring. Sets the appropriate `E86`/`E94`/`E93` message and returns
    /// `None` when it can't resolve.
    fn resolve_buffer(&mut self, arg: &str) -> Option<BufferId> {
        let arg = arg.trim();
        if arg.is_empty() {
            return Some(self.cur_buffer());
        }
        if arg == "#" {
            return match self.alternate {
                Some(id) => Some(id),
                None => {
                    self.echo("E23: No alternate file");
                    None
                }
            };
        }
        if let Ok(n) = arg.parse::<u64>() {
            let id = BufferId(n);
            if self.buffers.map.contains_key(&id) {
                return Some(id);
            }
            self.echo(format!("E86: Buffer {n} does not exist"));
            return None;
        }
        let matches: Vec<BufferId> = self
            .buffers
            .map
            .iter()
            .filter(|(_, ob)| {
                ob.buffer
                    .path
                    .as_ref()
                    .is_some_and(|p| p.display().to_string().contains(arg))
            })
            .map(|(id, _)| *id)
            .collect();
        match matches.as_slice() {
            [] => {
                self.echo(format!("E94: No matching buffer for {arg}"));
                None
            }
            [one] => Some(*one),
            _ => {
                self.echo(format!("E93: More than one match for {arg}"));
                None
            }
        }
    }

    /// `:bnext` — switch to the buffer `count` positions later in id order,
    /// wrapping around.
    fn ex_bnext(&mut self, count: usize) {
        let ids = self.buffer_ids();
        let len = ids.len();
        if let Some(i) = ids.iter().position(|id| *id == self.cur_buffer()) {
            self.switch_buffer(ids[(i + count) % len]);
        }
    }

    /// `:bprevious` — switch `count` positions earlier in id order, wrapping.
    fn ex_bprev(&mut self, count: usize) {
        let ids = self.buffer_ids();
        let len = ids.len();
        if let Some(i) = ids.iter().position(|id| *id == self.cur_buffer()) {
            self.switch_buffer(ids[(i + len - count % len) % len]);
        }
    }

    /// `:bfirst` — switch to the lowest-numbered buffer.
    fn ex_bfirst(&mut self) {
        if let Some(&id) = self.buffers.map.keys().next() {
            self.switch_buffer(id);
        }
    }

    /// `:blast` — switch to the highest-numbered buffer.
    fn ex_blast(&mut self) {
        if let Some(&id) = self.buffers.map.keys().next_back() {
            self.switch_buffer(id);
        }
    }

    /// `:bdelete` / `:bwipeout` — remove a buffer from the list (default the
    /// current one). Refuses a modified buffer without `!`. When the current
    /// buffer is removed, the window moves to the alternate (or the nearest
    /// remaining id); removing the last buffer leaves a fresh `[No Name]`.
    fn ex_bdelete(&mut self, args: &str, bang: bool) {
        let Some(target) = self.resolve_buffer(args) else {
            return;
        };
        if self.buffers.get(target).buffer.modified && !bang {
            self.echo(format!(
                "E89: No write since last change for buffer {} (add ! to override)",
                target.0
            ));
            return;
        }

        // When removing the current buffer, move to the alternate if it's a
        // distinct, still-open buffer (vim's behavior), else the nearest id.
        let was_current = target == self.cur_buffer();
        let replacement = if was_current {
            self.alternate
                .filter(|a| *a != target && self.buffers.map.contains_key(a))
                .or_else(|| self.neighbor_of(target))
        } else {
            None
        };
        self.buffers.map.remove(&target);
        if self.alternate == Some(target) {
            self.alternate = None;
        }

        if self.buffers.map.is_empty() {
            // Never leave zero buffers: open a fresh, empty one in the window.
            let id = self.add_buffer(Buffer::empty());
            self.set_cur_buffer(id);
            self.alternate = None;
            self.cursor = Cursor::default();
            self.top = 0;
            self.leftcol = 0;
            self.mode = Mode::Normal;
            self.reset_pending();
            self.scroll_from = None;
            self.pending_scroll = None;
        } else if was_current {
            // `current` now dangles; move to the chosen replacement (no stash —
            // the outgoing buffer is gone).
            self.enter_buffer(replacement.expect("a non-empty store has a neighbor"));
        }
    }

    /// The nearest buffer to `id` in id order among the *other* open buffers:
    /// the largest id below it, else the smallest above it. `None` if `id` is the
    /// only buffer.
    fn neighbor_of(&self, id: BufferId) -> Option<BufferId> {
        let below = self.buffers.map.range(..id).next_back().map(|(k, _)| *k);
        below.or_else(|| {
            self.buffers
                .map
                .range((std::ops::Bound::Excluded(id), std::ops::Bound::Unbounded))
                .next()
                .map(|(k, _)| *k)
        })
    }

    // ----- panel ------------------------------------------------------------

    /// Open (or replace) the bottom panel with `title` + `lines` and focus it.
    /// The text window shrinks to make room; the cursor is re-clamped so it
    /// stays visible in the reduced viewport. `wants_select` enables `<CR>`
    /// select events on the panel (the scripting `on_select` callback / RPC
    /// notification); the built-in viewer panels pass `false`. `cursor` is the
    /// initially selected line (0-based, clamped to the last line); the panel is
    /// scrolled so it is visible — `:messages` opens on its last line, `:ls` on
    /// the current buffer.
    ///
    /// Public so it can be driven from the scripting surface (`vim.panel.open`
    /// and the `nxvim_panel_open` RPC), as well as the `:messages` / `:ls`
    /// ex-commands.
    pub fn open_panel(
        &mut self,
        title: impl Into<String>,
        lines: Vec<String>,
        wants_select: bool,
        cursor: usize,
    ) {
        let cursor = cursor.min(lines.len().saturating_sub(1));
        // A panel being replaced (without an explicit close first) is still the
        // "last shown" one, so remember it for `:panelopen`.
        if let Some(replaced) = self.panel.take() {
            self.last_panel = Some(replaced);
        }
        self.panel = Some(Panel {
            title: title.into(),
            lines,
            cursor,
            top: 0,
            height: PANEL_HEIGHT,
            gpending: false,
            wants_select,
            targets: Vec::new(),
        });
        // The panel claimed rows off the bottom: shrink the windows area to match.
        self.relayout();
        self.ensure_visible();
        self.scroll_panel_into_view();
    }

    /// Attach per-line jump targets to the open panel, making it a navigable
    /// location list: `<CR>` on a line whose target is `Some((path, line, col))`
    /// jumps there (and closes the panel) via [`Editor::jump_to`], instead of
    /// firing a select event. The list is indexed in lockstep with the panel's
    /// lines (a shorter list or a `None` entry leaves that line non-navigable). A
    /// no-op when no panel is open. The targets ride along in the `:panelopen`
    /// snapshot, so a reopened list still jumps.
    pub fn set_panel_targets(&mut self, targets: Vec<Option<(PathBuf, usize, usize)>>) {
        if let Some(panel) = self.panel.as_mut() {
            panel.targets = targets;
        }
    }

    /// Enable or disable `<CR>` select events on the open panel
    /// (`vim.panel.on_select`). A no-op when no panel is open.
    pub fn set_panel_on_select(&mut self, wants: bool) {
        if let Some(panel) = self.panel.as_mut() {
            panel.wants_select = wants;
        }
    }

    /// Move the open panel's selection to `index` (0-based, clamped to the last
    /// line) and scroll it into view (`vim.panel.set_cursor` /
    /// `nxvim_panel_set_cursor`). A no-op when no panel is open.
    pub fn set_panel_cursor(&mut self, index: usize) {
        if let Some(panel) = self.panel.as_mut() {
            let last = panel.lines.len().saturating_sub(1);
            panel.cursor = index.min(last);
        }
        self.scroll_panel_into_view();
    }

    /// Move the panel selection to the logical entry occupying display `row`
    /// (0-based, within the visible content) — the mouse-click counterpart to the
    /// keyboard motions (`nxvim_panel_click`). The panel word-wraps, so a display
    /// row maps back to its logical entry by replaying the same wrap walk as
    /// [`Editor::panel_view`], starting from `top`. A row past the last visible
    /// entry selects that last entry. A no-op when no panel is open.
    pub fn set_panel_cursor_by_row(&mut self, row: usize) {
        let height = self.panel_content_height();
        let width = self.panel_width();
        let Some(panel) = self.panel.as_mut() else {
            return;
        };
        let mut display = 0;
        let mut logical = panel.top;
        let mut target = logical.min(panel.lines.len().saturating_sub(1));
        while display < height && logical < panel.lines.len() {
            let span = wrap_to_width(&panel.lines[logical], width).len().max(1);
            target = logical;
            if row < display + span {
                break;
            }
            display += span;
            logical += 1;
        }
        panel.cursor = target;
        self.scroll_panel_into_view();
    }

    /// Replace the open panel's content (`vim.panel.set_lines` /
    /// `nxvim_panel_set_lines`), keeping its title and re-clamping the cursor and
    /// scroll to the new content. A no-op when no panel is open.
    pub fn set_panel_lines(&mut self, lines: Vec<String>) {
        if let Some(panel) = self.panel.as_mut() {
            let last = lines.len().saturating_sub(1);
            panel.lines = lines;
            panel.cursor = panel.cursor.min(last);
            panel.top = panel.top.min(last);
            // The content changed, so any per-line jump targets no longer align.
            panel.targets.clear();
        }
        self.scroll_panel_into_view();
    }

    /// Re-derive the open panel's `top` so its `cursor` line stays within the
    /// visible window — the shared scroll step for opening, content swaps, and
    /// keyboard motion. A no-op when no panel is open.
    fn scroll_panel_into_view(&mut self) {
        let height = self.panel_content_height().max(1);
        let width = self.panel_width();
        let Some(panel) = self.panel.as_mut() else {
            return;
        };
        if panel.cursor < panel.top {
            // Cursor above the window: pin the window to it.
            panel.top = panel.cursor;
            return;
        }
        // Cursor at/below the window: raise `top` toward `cursor` until the lines
        // `[top..=cursor]`, **word-wrapped**, fit in `height` display rows — so the
        // selected entry's last wrapped row stays visible. (A single entry taller
        // than the panel can't fully fit; the loop stops at `top == cursor`,
        // showing it from its first row.) Wrapping makes this display-row aware,
        // where the old logical-line arithmetic clipped tall entries.
        while panel.top < panel.cursor {
            let rows: usize = panel.lines[panel.top..=panel.cursor]
                .iter()
                .map(|line| wrap_to_width(line, width).len())
                .sum();
            if rows <= height {
                break;
            }
            panel.top += 1;
        }
    }

    /// The panel's content width in screen cells: the full terminal width, since
    /// the panel spans it edge to edge with only a top border (no side borders,
    /// no number gutter). Drives the word-wrap in [`Editor::panel_view`].
    fn panel_width(&self) -> usize {
        self.width.max(1)
    }

    /// Close the panel and return focus to the text window, which grows back.
    /// Public for the scripting surface (`vim.panel.close` /
    /// `nxvim_panel_close`); a no-op when no panel is open. The closed panel is
    /// retained as the `:panelopen` target (content + selection preserved).
    pub fn close_panel(&mut self) {
        if let Some(closed) = self.panel.take() {
            self.last_panel = Some(closed);
        }
        // The windows area grows back into the rows the panel held.
        self.relayout();
        self.ensure_visible();
    }

    /// Reopen the most recently shown panel (`:panelopen`) with its retained
    /// content and selection — e.g. bringing an LSP references list back after it
    /// was dismissed. Returns `false` (and changes nothing) when no panel has been
    /// shown yet, so the caller can report it. The retained snapshot is kept, so
    /// the panel can be reopened again after another close.
    pub fn reopen_last_panel(&mut self) -> bool {
        let Some(panel) = self.last_panel.clone() else {
            return false;
        };
        // Re-clamp to the current content and reset the transient `gg` prefix, in
        // case the snapshot was taken mid-keystroke.
        let last = panel.lines.len().saturating_sub(1);
        self.panel = Some(Panel {
            cursor: panel.cursor.min(last),
            top: panel.top.min(last),
            gpending: false,
            ..panel
        });
        // Reopening reclaims the panel's rows from the windows area.
        self.relayout();
        self.ensure_visible();
        self.scroll_panel_into_view();
        true
    }

    /// Whether a panel is currently open (the `nxvim_panel_is_open` query).
    pub fn panel_is_open(&self) -> bool {
        self.panel.is_some()
    }

    /// The open panel's title, or `None` if no panel is open — lets the server
    /// recognize *which* panel a `<CR>` select came from (e.g. route a select on
    /// the LSP code-action list to applying that action, not the generic
    /// `on_select` path).
    pub fn panel_title(&self) -> Option<&str> {
        self.panel.as_ref().map(|p| p.title.as_str())
    }

    /// Total screen rows the panel occupies (its content plus the one title
    /// row), clamped so the text window always keeps at least one row. `0` when
    /// no panel is open.
    fn panel_rows(&self) -> usize {
        match &self.panel {
            None => 0,
            Some(p) => (p.height + 1).min(self.height.saturating_sub(1)),
        }
    }

    /// The panel's visible content height (its rows minus the title), `0` when
    /// no panel is open or it has been clamped to nothing.
    fn panel_content_height(&self) -> usize {
        self.panel_rows().saturating_sub(1)
    }

    /// Project the panel into the renderable [`PanelView`]: the visible content
    /// **word-wrapped** to the panel width, the selected entry's first display row
    /// and how many rows it spans, and the clamped content height. `None` when no
    /// panel is open. (`pub(crate)` so [`View`] can build it while [`Panel`] stays
    /// private.)
    ///
    /// Wrapping is display-only: `cursor`/`top` remain logical-entry indices (so
    /// `j`/`k`/`<CR>`/jump targets address whole entries), but each entry expands
    /// to one or more display rows here so long text is laid out across rows
    /// instead of clipped at the panel's right edge.
    pub(crate) fn panel_view(&self) -> Option<PanelView> {
        let p = self.panel.as_ref()?;
        let height = self.panel_content_height();
        let width = self.panel_width();

        let mut lines: Vec<String> = Vec::new();
        let mut cursor_row = 0;
        let mut cursor_span = 1;
        let mut logical = p.top;
        while lines.len() < height && logical < p.lines.len() {
            let start = lines.len();
            for row in wrap_to_width(&p.lines[logical], width) {
                if lines.len() >= height {
                    break;
                }
                lines.push(row);
            }
            if logical == p.cursor {
                cursor_row = start;
                cursor_span = lines.len().saturating_sub(start).max(1);
            }
            logical += 1;
        }

        Some(PanelView {
            title: p.title.clone(),
            lines,
            cursor_row,
            cursor_span,
            height,
        })
    }

    /// Handle one key while the panel is focused: `q`/`Q`/`<Esc>` close it,
    /// `<CR>` selects the current line (when the panel opted into select events),
    /// and the usual vertical motions (`j`/`k`/`gg`/`G`/`<C-d>`/`<C-u>`, arrows,
    /// `Home`/`End`) move the panel cursor, scrolling the panel to keep it
    /// visible. Everything else is ignored — the buffer is untouched while the
    /// panel has focus.
    fn handle_panel(&mut self, key: Key) {
        self.message.clear();

        // Close keys drop the panel and refocus the text window.
        if key.code == KeyCode::Esc || matches!(key.as_char(), Some('q') | Some('Q')) {
            self.close_panel();
            return;
        }

        if key.code == KeyCode::Enter {
            // A navigable location-list line jumps to its target and closes the
            // panel behind the jump. Owned here (the core already has `jump_to`)
            // so the targets travel with the `:panelopen` snapshot and a reopened
            // list still navigates.
            if let Some(Some((path, line, col))) = self
                .panel
                .as_ref()
                .and_then(|p| p.targets.get(p.cursor).cloned())
            {
                self.close_panel();
                self.jump_to(&path, line, col);
                return;
            }
            // Otherwise `<CR>` selects the current line: record it for the server
            // to dispatch to the scripting `on_select` handler. Only for
            // select-enabled panels, so a stale handler can't fire on a built-in
            // `:messages` viewer.
            if let Some(p) = &self.panel {
                if p.wants_select {
                    if let Some(line) = p.lines.get(p.cursor) {
                        self.panel_selects.push((p.cursor, line.clone()));
                    }
                }
            }
            return;
        }

        let ph = self.panel_content_height().max(1);
        let half = (ph / 2).max(1);
        let Some(panel) = self.panel.as_mut() else {
            return;
        };
        let last = panel.lines.len().saturating_sub(1);

        // `gg` is two keys; the first `g` arms `gpending`.
        if panel.gpending {
            panel.gpending = false;
            if key.as_char() == Some('g') {
                panel.cursor = 0;
            }
        } else if key.as_char() == Some('g') {
            panel.gpending = true;
        } else {
            match (key.code, key.as_char()) {
                (KeyCode::Down, _) | (_, Some('j')) => panel.cursor = (panel.cursor + 1).min(last),
                (KeyCode::Up, _) | (_, Some('k')) => panel.cursor = panel.cursor.saturating_sub(1),
                (_, Some('G')) => panel.cursor = last,
                (KeyCode::Char('d'), _) if key.ctrl => {
                    panel.cursor = (panel.cursor + half).min(last)
                }
                (KeyCode::Char('u'), _) if key.ctrl => {
                    panel.cursor = panel.cursor.saturating_sub(half)
                }
                (KeyCode::Home, _) => panel.cursor = 0,
                (KeyCode::End, _) => panel.cursor = last,
                _ => {}
            }
        }

        // Scroll the panel so the cursor line stays within the visible window.
        self.scroll_panel_into_view();
    }

    // ----- options ----------------------------------------------------------

    /// Handle `:set {options}` (and `:setlocal`, which is identical here — the
    /// buffer-local options apply to the current buffer either way). Each
    /// whitespace-separated token is a boolean option with the usual `no`/`inv`
    /// prefixes and `!`/`?` suffixes (`:set number`, `:set nonu`, `:set rnu!`) or
    /// a number option with `=value` / `?` (`:set tabstop=4`, `:set ts?`).
    fn ex_set(&mut self, args: &str) {
        for tok in args.split_whitespace() {
            match resolve_set(tok) {
                Some(SetCmd::Bool { name, op }) => self.apply_set_bool(name, op),
                Some(SetCmd::Num { name, op }) => self.apply_set_num(name, op),
                None => self.echo(format!("E518: Unknown option: {tok}")),
            }
        }
    }

    /// Apply one resolved boolean `:set` operation. `number` / `relativenumber`
    /// are window-local (they live on the focused window); `expandtab` is
    /// buffer-local (on the current buffer); the rest are global search options on
    /// the editor.
    fn apply_set_bool(&mut self, name: &str, op: SetOp) {
        let slot = match name {
            "number" => &mut self.windows.cur_mut().options.number,
            "relativenumber" => &mut self.windows.cur_mut().options.relativenumber,
            "ignorecase" => &mut self.options.ignorecase,
            "smartcase" => &mut self.options.smartcase,
            "wrapscan" => &mut self.options.wrapscan,
            "hlsearch" => &mut self.options.hlsearch,
            "incsearch" => &mut self.options.incsearch,
            "expandtab" => &mut self.buffer_mut().options.expandtab,
            _ => return,
        };
        match op {
            SetOp::On => *slot = true,
            SetOp::Off => *slot = false,
            SetOp::Toggle => *slot = !*slot,
            SetOp::Query => {
                let label = if *slot {
                    name.to_string()
                } else {
                    format!("no{name}")
                };
                self.echo(label);
            }
        }
    }

    /// Apply one resolved number `:set` operation. The indentation options
    /// (`tabstop` / `shiftwidth` / `softtabstop`) are buffer-local; the horizontal-
    /// scroll governors (`sidescroll` / `sidescrolloff`) are window-local (they live
    /// on the focused window). The assigned value is range-checked per option (vim's
    /// `E487`): `tabstop ≥ 1`, `shiftwidth ≥ 0`, `softtabstop ≥ -1`, the scroll
    /// options `≥ 0`.
    fn apply_set_num(&mut self, name: &str, op: NumOp) {
        match op {
            NumOp::Set(v) => {
                let min = match name {
                    "tabstop" => 1,
                    "shiftwidth" | "sidescroll" | "sidescrolloff" => 0,
                    "softtabstop" => -1,
                    _ => return,
                };
                if v < min {
                    self.echo(format!("E487: Argument must be positive: {name}={v}"));
                    return;
                }
                match name {
                    "sidescroll" => self.windows.cur_mut().options.sidescroll = v as usize,
                    "sidescrolloff" => self.windows.cur_mut().options.sidescrolloff = v as usize,
                    _ => {
                        let opts = &mut self.buffer_mut().options;
                        match name {
                            "tabstop" => opts.tabstop = v as usize,
                            "shiftwidth" => opts.shiftwidth = v as usize,
                            "softtabstop" => opts.softtabstop = v as isize,
                            _ => {}
                        }
                    }
                }
            }
            NumOp::Query => {
                let v: i64 = match name {
                    "sidescroll" => self.windows.cur().options.sidescroll as i64,
                    "sidescrolloff" => self.windows.cur().options.sidescrolloff as i64,
                    _ => {
                        let opts = &self.buffer().options;
                        match name {
                            "tabstop" => opts.tabstop as i64,
                            "shiftwidth" => opts.shiftwidth as i64,
                            "softtabstop" => opts.softtabstop as i64,
                            _ => return,
                        }
                    }
                };
                self.echo(format!("{name}={v}"));
            }
        }
    }

    /// The number-gutter width for a window with window-local `opts` showing a
    /// buffer with `line_count` lines: `0` when both number options are off, else
    /// at least 4 cells, widening to fit the largest line number plus one trailing
    /// space. Sized per window so each gutter fits its own buffer and options.
    pub(crate) fn number_width_for(&self, opts: WindowOptions, line_count: usize) -> usize {
        if !opts.number && !opts.relativenumber {
            return 0;
        }
        (digit_count(line_count) + 1).max(4)
    }

    // ----- undo / redo ------------------------------------------------------

    /// Capture the current text + cursor + sequence number as an undo/redo
    /// snapshot.
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.buffer().text.clone(),
            cursor: self.cursor,
            seq: self.buffers.get(self.cur_buffer()).cur_seq,
        }
    }

    fn push_undo(&mut self) {
        if self.snapshot_taken {
            return;
        }
        let snap = self.snapshot();
        let ob = self.cur_mut();
        ob.undo_stack.push(snap);
        ob.redo_stack.clear();
        // The edit about to happen produces a brand-new state — mint its id so
        // undo can later recognise (and redo can return to) this exact point.
        ob.cur_seq = ob.next_seq;
        ob.next_seq += 1;
    }

    fn undo(&mut self) {
        self.restore(true);
    }

    fn redo(&mut self) {
        self.restore(false);
    }

    /// Shared body of `undo`/`redo`: pop a snapshot off one history stack, push the
    /// current state onto the other, and restore text + cursor. `from_undo` picks
    /// the direction (undo: pop undo / push redo; redo: the reverse).
    fn restore(&mut self, from_undo: bool) {
        let popped = if from_undo {
            self.cur_mut().undo_stack.pop()
        } else {
            self.cur_mut().redo_stack.pop()
        };
        let Some(snap) = popped else {
            self.echo(if from_undo {
                "Already at oldest change"
            } else {
                "Already at newest change"
            });
            return;
        };
        let current = self.snapshot();
        let ob = self.cur_mut();
        if from_undo {
            ob.redo_stack.push(current);
        } else {
            ob.undo_stack.push(current);
        }
        ob.buffer.text = snap.text;
        ob.cur_seq = snap.seq;
        // We're back on a previously-seen state: it's clean only if it's the one
        // last written to disk. (`mark_resync` below sets `modified = true`, so
        // decide this first and re-assert it afterwards.)
        let clean = ob.saved_seq == Some(ob.cur_seq);
        self.cursor = snap.cursor;
        self.buffer_mut().mark_resync();
        self.buffer_mut().modified = !clean;
        self.clamp_cursor();
    }

    // ----- cursor / scrolling helpers --------------------------------------

    /// The last real line index (`line_count - 1`, saturating). The rope's phantom
    /// trailing line is never included.
    fn last_line(&self) -> usize {
        self.buffer().line_count().saturating_sub(1)
    }

    fn cursor_char(&self) -> usize {
        self.buffer().byte_at(self.cursor.line, self.cursor.col)
    }

    fn char_at(&self, idx: usize) -> char {
        // Non-boundary bytes (inside a multi-byte char) read as blank rather
        // than panicking; cursor/operator positions are kept on boundaries.
        self.buffer().text.get_char(idx).unwrap_or(' ')
    }

    /// Byte offset one grapheme-cluster forward from `idx` over the whole buffer.
    /// The trailing `\n` of each line is itself a single-byte grapheme.
    fn next_grapheme_idx(&self, idx: usize) -> usize {
        let line = self.buffer().byte_to_line(idx);
        let start = self.buffer().line_start(line);
        let s = self.buffer().line(line);
        let rel = idx - start;
        if rel < s.len() {
            start + unicode::next_grapheme(&s, rel)
        } else {
            (idx + 1).min(self.buffer().len_bytes())
        }
    }

    /// Byte offset one grapheme-cluster backward from `idx` over the whole buffer.
    fn prev_grapheme_idx(&self, idx: usize) -> usize {
        if idx == 0 {
            return 0;
        }
        let line = self.buffer().byte_to_line(idx);
        let start = self.buffer().line_start(line);
        let s = self.buffer().line(line);
        let rel = idx - start;
        if rel == 0 {
            idx - 1
        } else {
            start + unicode::prev_grapheme(&s, rel.min(s.len()))
        }
    }

    /// Snap an absolute byte offset down to a grapheme boundary.
    fn grapheme_floor_abs(&self, idx: usize) -> usize {
        let line = self.buffer().byte_to_line(idx);
        let start = self.buffer().line_start(line);
        let s = self.buffer().line(line);
        let rel = idx.saturating_sub(start).min(s.len());
        start + unicode::floor_grapheme(&s, rel)
    }

    /// Snap an absolute byte offset up to a grapheme boundary.
    fn grapheme_ceil_abs(&self, idx: usize) -> usize {
        let floored = self.grapheme_floor_abs(idx);
        if floored >= idx {
            floored
        } else {
            self.next_grapheme_idx(floored)
        }
    }

    /// The current buffer's `tabstop`: cells a tab expands to, the grid tabs
    /// render on and the cursor snaps to. Floored at 1 so a degenerate `0`
    /// (set via `:set tabstop=0`) never divides by zero.
    pub fn tabstop(&self) -> usize {
        self.buffer().options.effective_tabstop()
    }

    /// Virtual (screen) column of the cursor on its current line.
    fn cursor_virtcol(&self) -> usize {
        let s = self.buffer().line(self.cursor.line);
        unicode::virtcol(&s, self.cursor.col, self.tabstop())
    }

    /// Advance `count` grapheme clusters forward from byte offset `from`, never
    /// passing `limit`. Returns the new offset and how many clusters were crossed.
    fn advance_graphemes(&self, mut from: usize, count: usize, limit: usize) -> (usize, usize) {
        let mut crossed = 0;
        while crossed < count && from < limit {
            let next = self.next_grapheme_idx(from).min(limit);
            if next == from {
                break;
            }
            from = next;
            crossed += 1;
        }
        (from, crossed)
    }

    /// Snap the cursor column down to the nearest grapheme boundary (a no-op for
    /// ASCII), so byte offsets handed to the rope are always valid.
    fn snap_cursor(&mut self) {
        let s = self.buffer().line(self.cursor.line);
        self.cursor.col = unicode::floor_grapheme(&s, self.cursor.col.min(s.len()));
    }

    fn last_char_idx(&self) -> usize {
        // The trailing '\n' is never a valid cursor position.
        self.buffer().len_bytes().saturating_sub(1)
    }

    fn line_len(&self) -> usize {
        self.buffer().line_len(self.cursor.line)
    }

    fn first_non_blank(&self, line: usize) -> usize {
        let s = self.buffer().line(line);
        s.bytes().take_while(|b| *b == b' ' || *b == b'\t').count()
    }

    fn set_cursor_char(&mut self, idx: usize) {
        let idx = self
            .buffer()
            .text
            .floor_char_boundary(idx.min(self.last_char_idx()));
        let line = self.buffer().byte_to_line(idx);
        self.cursor.line = line;
        self.cursor.col = idx - self.buffer().line_start(line);
        self.snap_cursor();
    }

    fn set_cursor_char_insert(&mut self, idx: usize) {
        let idx = self
            .buffer()
            .text
            .floor_char_boundary(idx.min(self.buffer().len_bytes()));
        let line = self.buffer().byte_to_line(idx);
        self.cursor.line = line;
        self.cursor.col = idx - self.buffer().line_start(line);
        self.snap_cursor();
    }

    fn move_vertical(&mut self, delta: i64, allow_eol: bool) {
        let new = (self.cursor.line as i64 + delta).max(0) as usize;
        self.cursor.line = new.min(self.last_line());
        self.settle_desired_col(allow_eol);
        self.preserve_desired = true;
    }

    /// Place the cursor on the current line at the remembered desired *virtual*
    /// column (or end-of-line when `$`-sticky), clamped to the line and a grapheme
    /// boundary.
    fn settle_desired_col(&mut self, allow_eol: bool) {
        let s = self.buffer().line(self.cursor.line);
        // Furthest valid resting byte: past-end for insert/allow_eol, otherwise
        // the start of the last grapheme (normal mode can't rest past EOL).
        let max_byte = if allow_eol {
            s.len()
        } else {
            unicode::prev_grapheme(&s, s.len())
        };
        let target = if self.desired_eol {
            max_byte
        } else {
            unicode::byte_at_virtcol(&s, self.desired_col, self.tabstop()).min(max_byte)
        };
        self.cursor.col = unicode::floor_grapheme(&s, target);
    }

    fn clamp_cursor(&mut self) {
        let last_line = self.last_line();
        if self.cursor.line > last_line {
            self.cursor.line = last_line;
        }
        let len = self.line_len();
        let max_col = if self.mode.is_insert() {
            len
        } else {
            len.saturating_sub(1)
        };
        if self.cursor.col > max_col {
            self.cursor.col = max_col;
        }
        self.snap_cursor();
    }

    fn scroll_half(&mut self, down: bool) {
        let half = (self.text_height() / 2).max(1) as i64;
        self.scroll_by(if down { half } else { -half });
    }

    fn scroll_page(&mut self, down: bool) {
        let page = self.text_height().saturating_sub(2).max(1) as i64;
        self.scroll_by(if down { page } else { -page });
    }

    /// Scroll the viewport by `delta` lines, vim-style: move both `top` and the
    /// cursor together so the cursor keeps its screen row. Records the pre-move
    /// `(top, cursor.line)` in `scroll_from`; `input` turns that into a
    /// `PendingScroll` if `top` actually changed.
    fn scroll_by(&mut self, delta: i64) {
        self.scroll_from = Some((self.top, self.cursor.line));
        let last = self.last_line() as i64;
        self.top = (self.top as i64 + delta).clamp(0, last) as usize;
        self.move_vertical(delta, false);
        self.clamp_cursor();
    }

    fn ensure_visible(&mut self) {
        let th = self.text_height();
        if self.cursor.line < self.top {
            self.top = self.cursor.line;
        } else if self.cursor.line >= self.top + th {
            self.top = self.cursor.line + 1 - th;
        }
        // Horizontal scroll follows the cursor on the same beat as the vertical
        // one, so every motion that calls `ensure_visible` also keeps the cursor's
        // column on screen under `nowrap`.
        self.ensure_visible_horizontal();
    }

    /// Keep the cursor's screen column within the focused window's text area by
    /// adjusting [`Editor::leftcol`] — the horizontal analog of [`ensure_visible`]
    /// (vim's `nowrap` `w_leftcol`). Honors `sidescroll` (the scroll step: `0`
    /// recenters the cursor, `> 0` scrolls just enough to bring it to the edge) and
    /// `sidescrolloff` (the margin kept between the cursor and the edge). A no-op
    /// for a degenerate (zero-width) text area.
    fn ensure_visible_horizontal(&mut self) {
        let tw = self.text_width();
        if tw == 0 {
            return;
        }
        let opts = self.windows.cur().options;
        // The margin can't claim more than half the window, or the left and right
        // bounds would cross.
        let so = opts.sidescrolloff.min(tw.saturating_sub(1) / 2);
        let recenter = opts.sidescroll == 0;
        let vc = self.cursor_virtcol();
        let left = self.leftcol;

        if vc < left + so {
            // Cursor at/past the left margin: scroll left.
            self.leftcol = if recenter {
                vc.saturating_sub(tw / 2)
            } else {
                vc.saturating_sub(so)
            };
        } else if vc + so + 1 > left + tw {
            // Cursor at/past the right margin: scroll right.
            self.leftcol = if recenter {
                vc.saturating_sub(tw / 2)
            } else {
                (vc + so + 1).saturating_sub(tw)
            };
        }
    }

    // ----- pending-state bookkeeping ---------------------------------------

    fn effective_count(&self) -> usize {
        self.pending.op_count.unwrap_or(1) * self.pending.count.unwrap_or(1)
    }

    fn reset_pending(&mut self) {
        self.pending = PendingCommand::default();
    }
}

impl Default for Editor {
    fn default() -> Self {
        Editor::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Blank,
    Word,
    Punct,
}

/// Number of decimal digits in `n` (at least 1, so `0` is one digit).
fn digit_count(n: usize) -> usize {
    let mut n = n;
    let mut digits = 1;
    while n >= 10 {
        n /= 10;
        digits += 1;
    }
    digits
}

fn char_class(c: char) -> CharClass {
    if c == ' ' || c == '\t' || c == '\n' || c == '\r' {
        CharClass::Blank
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

/// Split an ex-command into `(name, bang, args)`.
fn split_ex(cmd: &str) -> (&str, bool, &str) {
    let bytes = cmd.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    let name = &cmd[..i];
    let mut bang = false;
    if i < bytes.len() && bytes[i] == b'!' {
        bang = true;
        i += 1;
    }
    let args = cmd[i..].trim();
    (name, bang, args)
}

/// Parse a `:sleep` argument: `{n}` = seconds, `{n}m` = milliseconds, empty =
/// 1 second (matching vim). Returns a vim-style `E475` error string for
/// non-integer input.
/// Parse a buffer-navigation count argument (`:bnext 2`). Empty / invalid / zero
/// all mean 1, matching vim's default repeat count.
fn parse_count_arg(args: &str) -> usize {
    args.trim()
        .parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
        .unwrap_or(1)
}

fn parse_sleep(args: &str) -> Result<u64, String> {
    let a = args.trim();
    if a.is_empty() {
        return Ok(1000);
    }
    let invalid = || format!("E475: Invalid argument: {a}");
    match a.strip_suffix('m') {
        Some(ms) => ms.trim().parse::<u64>().map_err(|_| invalid()),
        None => a
            .parse::<u64>()
            .map(|secs| secs.saturating_mul(1000))
            .map_err(|_| invalid()),
    }
}
