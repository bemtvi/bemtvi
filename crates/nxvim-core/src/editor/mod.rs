//! The editor state machine: turns keys and ex-commands into buffer mutations.
//!
//! This is the rust-native analogue of neovim's `normal.c` / `ops.c` /
//! `edit.c` / `ex_docmd.c`. It is fully synchronous and owns no I/O beyond
//! reading/writing files through [`Buffer`]. The async server feeds it input
//! and reads back state; it never blocks.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::buffer::Buffer;
use crate::clipboard::Clipboard;
use crate::highlight::Highlights;
use crate::host::{HostFs, StdHostFs};
use crate::input::{Key, KeyCode};
use crate::mode::Mode;
use crate::options::{DockOptions, Options};
use crate::search::{RegexEngine, SearchRegex};

/// Memoized search regex for the highlight path, keyed by the
/// `(pattern, ignorecase, engine)` it was compiled from (see
/// [`Editor::search_re_cache`]).
type SearchReCache = Option<(String, bool, RegexEngine, Rc<SearchRegex>)>;
use crate::syntax::SyntaxEngine;
use crate::view::View;

mod buffers;
mod changelist;
mod cmdline;
mod command;
mod cursor;
mod dock;
mod ex;
mod explorer;
mod expr;
mod insert;
mod jumps;
mod marks;
mod menu;
mod motions;
mod mouse;
mod multicursor;
mod operators;
mod options;
mod panel;
mod persist;
mod registers;
mod search;
mod syntax;
mod tabs;
mod terminal;
mod undo;
mod windows;

// The command grammar + its normal/visual executor. The parse↔execute contract
// types stay private to `command`; only the shared vocabulary is re-exported.
pub use self::command::{command_status, CommandStatus};
pub(crate) use self::command::{
    DockChord, FindKind, Motion, MotionKind, MotionResult, MoveAxis, ObjectKind, PendingCommand,
    Stage,
};
pub use self::menu::{
    MenuExtent, MenuItem, MenuPlacement, PreviewScroll, PreviewTarget, PromptPos,
};
pub(crate) use self::multicursor::PlacementSnapshot;
// The off-tick save / open requests (the daemon / edit-host fs path, Phase 3e/3f).
pub use self::buffers::{
    FileChangeAction, FileChangeReason, PendingOpen, PendingQuitAll, PendingSave,
};
pub use self::persist::{
    FileChangelist, FileMarkEntry, GlobalMarkEntry, JumpPos, NumberedMark, PersistState,
    RegisterEntry, ShadaRequest,
};
pub use self::terminal::TerminalOp;
pub use self::undo::{UndoEntry, UndoTreeView};
// The window layout subsystem (tree types + layout algebra + window methods).
pub(crate) use self::jumps::JumpEntry;
pub use self::windows::{BorderStyle, FloatAnchor, FloatConfig, FloatRelative, WindowConfigSpec};
pub(crate) use self::windows::{PendingScroll, TabLabel, WindowLayout, WindowTree};
// Search vocabulary shared by the command line, the parser, and the View.
pub(crate) use self::search::{SearchDir, SearchOffset};
pub(crate) use self::syntax::fill_indent;

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

/// A non-Normal mode parked on a [`Window`] by the `<C-w><C-w>` dock chord, to be
/// resumed when focus returns to that window (see [`Editor::dock_chord_intercept`]
/// and [`Editor::reestablish_mode`]). Carries just enough to re-enter the mode
/// faithfully: the mode itself, the insert/replace **resume column** (captured
/// before the leave-insert backstep, so an append at end-of-line resumes where it
/// was), and the **visual anchor** (the fixed end of a restored selection).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResumeState {
    pub(crate) mode: Mode,
    pub(crate) col: usize,
    pub(crate) visual_anchor: Cursor,
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

/// Stable identifier for a tab page — a named collection of windows with its own
/// split layout. Like [`WindowId`] / [`BufferId`] it is monotonic, 1-based, and
/// never reused, matching neovim's tabpage handles. The first tab is allocated at
/// startup, holding the first window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TabId(pub u64);

/// One tab page in tabline order: a stable id plus, *while not active*, its
/// stashed window layout. The active tab's layout is live on [`Editor::windows`]
/// (the analogue of the focused window's live cursor), so its `tree` here is
/// `None`; every inactive tab stashes its whole [`WindowTree`].
struct TabSlot {
    id: TabId,
    tree: Option<WindowTree>,
}

/// One **layer**'s tab pages plus its active index — the [`Editor::main_tabs`]
/// stack and each open dock's stack in [`Editor::dock_tabs`]. Across the whole
/// editor exactly one `(layer, tab)` tree is `None`: the **focused** layer's
/// active tab, whose tree is live on [`Editor::windows`] (the same trick a single
/// active tab's `None` slot already uses). Every other tab — inactive tabs of any
/// layer, and the active tab of a *non-focused* layer — parks its whole
/// [`WindowTree`] in its own [`TabSlot`]. So a layer's "parked active tree" (the
/// old `main_parked` / dock `Option<WindowTree>`) is simply
/// `tabs[current].tree` while that layer isn't focused.
pub(crate) struct TabStack {
    /// This layer's tab pages, in tabline order.
    tabs: Vec<TabSlot>,
    /// Index into [`TabStack::tabs`] of this layer's active tab.
    current: usize,
}

impl TabStack {
    /// A one-tab stack whose sole tab is `id` and whose tree is **live** (slot
    /// `None`) — the shape a layer takes the instant it becomes focused with its
    /// tree swapped onto [`Editor::windows`].
    fn live(id: TabId) -> Self {
        Self {
            tabs: vec![TabSlot { id, tree: None }],
            current: 0,
        }
    }
}

pub(crate) use registers::{RegKind, RegisterCell, Registers};

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

/// The full materialized state at one undo point: text plus the bits vim keeps
/// across undo. Cloning is cheap — `ropey::Rope` is a persistent rope, so a clone
/// shares structure with the original and only diverged chunks cost memory.
#[derive(Clone)]
struct Snapshot {
    text: ropey::Rope,
    cursor: Cursor,
    /// The buffer's extmarks at this history point. Restored on undo/redo so marks
    /// ride with the text (neovim keeps extmarks across undo) rather than being
    /// dropped by the wholesale-replace [`Buffer::mark_resync`].
    extmarks: crate::extmark::ExtmarkStore,
    /// The buffer's `a`–`z` marks at this history point — restored on undo/redo
    /// alongside `extmarks`, for the same reason: vim keeps marks across undo, and
    /// the wholesale-replace [`Buffer::mark_resync`] would otherwise clear them.
    marks: HashMap<char, (usize, usize)>,
    /// The buffer's change list (and its `g;`/`g,` pointer) at this history point,
    /// restored on undo/redo alongside `marks` so `g;` keeps working across an undo
    /// (the wholesale-replace [`Buffer::mark_resync`] clears it otherwise).
    changelist: (Vec<(usize, usize)>, usize),
    /// The window whose secondary multi-cursor set is baked into `extmarks`'s
    /// `CURSOR_NS`/`ANCHOR_NS` marks. The multi-cursor set is window-local but the
    /// undo tree is per-buffer and shared by every window onto it, so on undo/redo
    /// those marks are re-applied *only* when the focused window matches — else the
    /// editing window's cursors would leak into another window. `None` for a state
    /// with no owning window (the root, a snapshot of a non-current buffer).
    cursor_window: Option<WindowId>,
}

/// Index of a node within [`UndoTree::nodes`]. Stable for a node's lifetime —
/// nodes are only appended, never removed (until `undolevels` pruning lands).
type NodeIdx = usize;

/// One reachable buffer state in the undo *tree*. Unlike a linear undo stack, a
/// node can have several `children`: undoing then typing something new forks a
/// branch instead of discarding the old future, so every state stays reachable
/// (the data `vim.fn.undotree()` and visualizers like the undotree plugin draw).
/// Mirrors neovim's `u_header_T`, but stores a whole-buffer [`Snapshot`] rather
/// than a line-delta — see the rope-clone note on `Snapshot`.
struct UndoNode {
    /// Sequence number of this state; higher == newer. The root (original loaded
    /// text) is `0`. Used by `:undo {N}` to jump to an arbitrary state.
    seq: u64,
    /// Parent state (the one a plain `u` returns to). `None` only for the root.
    parent: Option<NodeIdx>,
    /// Child states in creation order; `children.last()` is the newest branch,
    /// which is the one a plain `<C-r>` redoes into.
    children: Vec<NodeIdx>,
    /// When this state was created, in **monotonic** seconds since the editor's
    /// time base (injected by the server). Monotonic — not wall-clock — so the
    /// "N minutes ago" label `vim.fn.undotree()` feeds the visualizer stays
    /// correct and non-negative across NTP steps / clock changes. The root's time
    /// is `0` and never displayed (it renders as "(Orig)").
    time: i64,
    /// The save number stamped when this exact state was written to disk, else
    /// `None`. `Some(n)` means it was the buffer's `n`-th write. Surfaced as the
    /// `save` field of `vim.fn.undotree()` entries (neovim's `uh_save_nr`).
    save: Option<u64>,
    snap: Snapshot,
}

/// A buffer's branching undo history. The live buffer text always corresponds to
/// node `cur` plus at most one *uncommitted* edit (tracked by `dirty`): a pending
/// change is materialized into a new child node lazily, at the moment focus
/// leaves the state — the next change-group boundary or an undo/redo/`:undo`.
/// That lazy-commit timing is exactly when the old two-stack model snapshotted,
/// so cursor/text/seq land identically; it just retains the abandoned branches.
struct UndoTree {
    /// Arena of states; `nodes[0]` is always the root (seq 0, original text).
    nodes: Vec<UndoNode>,
    /// The node the live buffer is currently sitting on.
    cur: NodeIdx,
    /// Source of the next `seq`; only ever increments. `seq_last == next_seq - 1`.
    next_seq: u64,
    /// The live buffer has an edit not yet committed to a node. Set when a change
    /// group begins; cleared when the pending state is committed.
    dirty: bool,
    /// Monotonic time the current pending edit began (meaningful only while
    /// `dirty`). Used as the timestamp of the *virtual* current node
    /// `vim.fn.undotree()` shows for an uncommitted edit, so the projection is
    /// stable (doesn't drift with the clock) until the edit commits.
    dirty_since: i64,
    /// Number of writes (`:w`) of this buffer so far — the source of each saved
    /// node's `save` number. `vim.fn.undotree()` reports it as `save_last`.
    /// (Neovim's `b_u_save_nr_last`.)
    save_last: u64,
}

/// A buffer as the editor holds it: the text [`Buffer`] plus the state vim keeps
/// with the buffer rather than the window — undo/redo history and, while the
/// buffer is not current, the last cursor/scroll position so switching back
/// restores the view.
struct OpenBuffer {
    buffer: Buffer,
    /// Branching undo history. The current text state's id is `undo.cur_seq()`.
    undo: UndoTree,
    /// The seq that matches what's on disk, or `None` once history can no longer
    /// return to the saved state. On undo/redo the buffer reads as `modified`
    /// exactly when the landed-on seq differs from this. (Neovim's `b_u_save_nr`.)
    saved_seq: Option<u64>,
    /// Window position saved when this buffer stops being current; restored on
    /// switch-back. Meaningless while the buffer *is* current — the live position
    /// is then [`Editor::cursor`] / `Editor::top` / `Editor::leftcol`.
    saved_cursor: Cursor,
    saved_top: usize,
    saved_leftcol: usize,
}

impl OpenBuffer {
    fn new(buffer: Buffer) -> Self {
        let undo = UndoTree::new(&buffer);
        OpenBuffer {
            buffer,
            undo,
            // A freshly loaded buffer matches disk: state 0 is the saved state.
            saved_seq: Some(0),
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

/// The orientation of a split: `Horizontal` stacks children top-to-bottom
/// (`:split` / `<C-w>s`), `Vertical` places them left-to-right (`:vsplit` /
/// `<C-w>v`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SplitDir {
    Horizontal,
    Vertical,
}

/// A directional window-focus move (`<C-w>h/j/k/l`). A shared primitive: the
/// `<C-w>` command grammar ([`command`]) builds it, the window executor
/// ([`windows`]) consumes it — so it lives here, keeping the two decoupled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WinDir {
    Left,
    Down,
    Up,
    Right,
}

/// Which screen edge a permanent **dock** is pinned to. A dock is a global
/// (cross-tab) editable window region — nxvim's VSCode-style side/bottom panel.
/// Unlike the main window tree it is never disturbed by splits, window switches,
/// or tab changes in the editor area; the top dock sits *above* the tabline. See
/// [`Editor::open_dock`] and the dock subtree in `editor/dock.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DockSide {
    Left,
    Right,
    Top,
    Bottom,
}

impl DockSide {
    /// Every side, in the canonical order docks are carved and rendered.
    pub(crate) const ALL: [DockSide; 4] = [
        DockSide::Left,
        DockSide::Right,
        DockSide::Top,
        DockSide::Bottom,
    ];

    /// Index into the `[_; 4]` per-side arrays on [`Editor`].
    pub(crate) fn idx(self) -> usize {
        match self {
            DockSide::Left => 0,
            DockSide::Right => 1,
            DockSide::Top => 2,
            DockSide::Bottom => 3,
        }
    }

    /// Parse a side keyword (`"left"`/`"right"`/`"top"`/`"bottom"`) — the inverse
    /// of the dock RPC/Lua surface. `None` for an unknown keyword, which each
    /// caller reports loudly (per the no-silent-fallback rule).
    pub(crate) fn from_keyword(s: &str) -> Option<DockSide> {
        Some(match s {
            "left" => DockSide::Left,
            "right" => DockSide::Right,
            "top" => DockSide::Top,
            "bottom" => DockSide::Bottom,
            _ => return None,
        })
    }

    /// The side keyword (`"left"`/`"right"`/`"top"`/`"bottom"`) — the inverse of
    /// [`DockSide::from_keyword`], for messages and the RPC/Lua surface.
    pub(crate) fn keyword(self) -> &'static str {
        match self {
            DockSide::Left => "left",
            DockSide::Right => "right",
            DockSide::Top => "top",
            DockSide::Bottom => "bottom",
        }
    }

    /// A sensible default reserved extent when `nx.dock.open` omits `size`: a wide
    /// gutter for the vertical side bars, a short tray for the horizontal ones.
    pub(crate) fn default_size(self) -> usize {
        match self {
            DockSide::Left | DockSide::Right => 30,
            DockSide::Top | DockSide::Bottom => 10,
        }
    }

    /// The [`DockSide`] reached by moving in `dir` from the main area (the
    /// `<C-w><C-w>{h,j,k,l}` cross): `h`→left, `l`→right, `k`→top, `j`→bottom.
    pub(crate) fn from_dir(dir: WinDir) -> DockSide {
        match dir {
            WinDir::Left => DockSide::Left,
            WinDir::Right => DockSide::Right,
            WinDir::Up => DockSide::Top,
            WinDir::Down => DockSide::Bottom,
        }
    }
}

/// Which window *layer* currently owns the live editing state. The whole editor
/// reads its target from [`Editor::windows`]; keeping the focused layer's tree
/// swapped onto `windows` (the tab-page trick) means `split`/`close`/`focus`/
/// editing/redraw all act on the focused layer with no special-casing. `Main` is
/// the per-tab window tree; `Dock(side)` is one of the global docks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layer {
    Main,
    Dock(DockSide),
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

/// A just-committed visual-mode change, captured by [`Editor::visual_operate`]
/// for dot-repeat. `keys` is the synthesized **size-faithful** reselect-and-
/// operate stream (`v`/`V` + motions + operator) — replaying it from any cursor
/// reselects the same extent vim would, rather than re-running the original
/// motions. For a change (`is_change`), the inserted text and a trailing `<Esc>`
/// are appended at the commit point in [`Editor::input`] (known only once the
/// insert session ends).
pub(crate) struct VisualShape {
    keys: Vec<Key>,
    is_change: bool,
}

/// Project the literal text of an insert session into the keys that retype it,
/// for appending to a dot-repeat replay stream: each char is its key, a newline
/// becomes `<Enter>`. The caller adds the closing `<Esc>`.
fn insert_text_keys(text: &str) -> Vec<Key> {
    text.chars()
        .map(|c| match c {
            '\n' => Key::new(KeyCode::Enter),
            c => Key::char(c),
        })
        .collect()
}

/// The complete editor state: the open buffers plus the single window's state.
pub struct Editor {
    /// All open buffers, keyed by id. The current buffer is *derived* from the
    /// focused window ([`Editor::cur_buffer`]); reach its text model through
    /// [`Editor::buffer`].
    buffers: BufferStore,
    /// The window layout of the **focused** layer's active tab — the split tree,
    /// the focused window, and per-window view state. The buffer it shows is the
    /// current buffer. Every other tree (inactive tabs of any layer, and a
    /// non-focused layer's active tab) stashes its layout in its [`TabStack`] slot.
    windows: WindowTree,
    /// The **main** layer's tab pages (see [`TabStack`]). The active tab's slot
    /// holds `None` while the main layer is focused (its layout is live on
    /// [`Editor::windows`]); when a dock is focused its active tab parks here
    /// instead — the role the old `main_parked` field played.
    main_tabs: TabStack,
    /// The next window id to mint, monotonic and **global** across every tab (a
    /// window handle is never reused, matching neovim). The active tab's splits and
    /// every new tab's first window draw from this one counter, so two tabs can
    /// never hand out the same [`WindowId`]. Seeded past the first window (id 1).
    next_win_id: u64,
    /// The next tab id to mint, monotonic and never reused. Seeded past the first
    /// tab (id 1).
    next_tab_id: u64,
    /// The four permanent docks (left, right, top, bottom — indexed by
    /// [`DockSide::idx`]), each its own [`TabStack`] when open and `None` when the
    /// side is closed (so `is_some()` *is* "open" — read it through
    /// [`Editor::dock_is_open`]). Docks are **global**: they live here, outside
    /// [`Editor::main_tabs`], so the same docks show on every main tab and
    /// main-tab/split operations never disturb them — but each dock now carries its
    /// own independent tab pages. The focused dock's active tab holds `None` (its
    /// tree is live on [`Editor::windows`], mirroring the main layer); a
    /// non-focused dock parks its active tree in that tab's slot.
    dock_tabs: [Option<TabStack>; 4],
    /// Which layer currently owns the live editing state on [`Editor::windows`]
    /// (and the live `cursor`/`top`/`leftcol`). `Main` until the first dock is
    /// focused via `<C-w><C-w>`.
    focused_layer: Layer,
    /// Each dock's reserved extent: columns for `Left`/`Right`, rows for
    /// `Top`/`Bottom`. Meaningful only where the dock is open. Indexed by
    /// [`DockSide::idx`].
    dock_sizes: [usize; 4],
    /// Per-dock options — the **dock** scope (see [`DockOptions`]), indexed by
    /// [`DockSide::idx`] like `dock_sizes`. Set through `nx.dock.opt` /
    /// `nx.dock.open{...}`; persists across close/reopen of a side. The dock's
    /// *size* lives in `dock_sizes`, not here.
    dock_options: [DockOptions; 4],
    /// Which docks are **hidden** — present (their [`TabStack`] still parked in
    /// [`Editor::dock_tabs`], so their content/splits/tabs/cursor survive) but
    /// excluded from layout, render, mouse hit-testing and focus-crossing. A hidden
    /// dock is the toggle / auto-hide collapsed state (VSCode-style), distinct from
    /// *closed* (which drops the `TabStack`). Read it through
    /// [`Editor::dock_is_open`] (which is false for a hidden side); never inspect a
    /// raw `dock_tabs[idx].is_some()` for a visibility decision. Indexed by
    /// [`DockSide::idx`].
    dock_hidden: [bool; 4],
    /// The dock a non-directional `<C-w><C-w>{cmd}` (e.g. `<C-w><C-w>v`) crosses
    /// to — the most recently focused dock. Directional crosses pick by edge
    /// instead ([`DockSide::from_dir`]).
    last_dock: DockSide,
    /// In-progress state of the mode-independent `<C-w><C-w>` dock-navigation
    /// chord — the cross-mode path that lets the chord reach the docks from
    /// insert / visual / command / terminal mode, not just Normal (where the
    /// command grammar already owns `<C-w>`). See [`Editor::dock_chord_intercept`].
    dock_chord: DockChord,
    /// One-shot: the next [`Editor::enter_window`] should **resume** the target
    /// window's parked [`Window::resume`] mode rather than forcing Normal. Set only
    /// by the dock chord right before it crosses, and consumed by the first
    /// `enter_window` of that cross — so mode resumption is scoped to the chord and
    /// never leaks into ordinary window/tab/mouse focus changes.
    restore_mode_on_enter: bool,
    /// vim's alternate buffer (`#`), the `<C-^>` target; `None` until a switch
    /// sets it.
    alternate: Option<BufferId>,
    /// The global file marks `A`–`Z`: each names a `(buffer, cursor)`, so a jump
    /// can cross buffers — unlike the buffer-local lowercase marks that live on the
    /// [`Buffer`] and ride its edit choke point. Set by `m{A-Z}`; jumping to one
    /// switches to its buffer first. A mark whose buffer has been closed is treated
    /// as unset on lookup (the jump then errors *E20*) — see
    /// [`Editor::mark_location`].
    global_marks: HashMap<char, (BufferId, Cursor)>,
    /// Global marks restored from a shada store whose target file isn't open yet.
    /// Keyed `A`–`Z` to a `(path, cursor)`; promoted into [`Editor::global_marks`]
    /// (the file opened) lazily on the first jump, so — like vim — a restored
    /// session never loads every marked file at startup, only when `` `A `` is
    /// pressed. Populated by [`Editor::import_persist`]; drained by
    /// [`Editor::resolve_pending_global_mark`].
    pending_global_marks: HashMap<char, (PathBuf, Cursor)>,
    /// Per-file marks restored from a shada store, keyed by *normalized* path to
    /// that file's `{a–z, specials, "}` marks. Seeded into a buffer's live
    /// `marks` (and the path entry drained) the moment the buffer for that path is
    /// loaded or — for the restored startup buffer — at import. Like vim, restored
    /// file marks reattach when the file is reopened, not eagerly at launch. Drained
    /// by [`Editor::seed_pending_file_marks`].
    pending_file_marks: HashMap<PathBuf, HashMap<char, (usize, usize)>>,
    /// The numbered marks `'0`–`'9` — a *pure persistence construct*: `'0` is the
    /// cursor where the last session exited, `'1` the session before, etc. They
    /// have no live capture point (the shada store mints them at load by shifting
    /// the previous set down one), so they live here path-based and resolve to a
    /// buffer lazily on the jump, exactly like [`Editor::pending_global_marks`].
    /// Seeded by [`Editor::import_persist`]; read by `` `0 ``…`` `9 ``.
    numbered_marks: HashMap<char, (PathBuf, Cursor)>,
    /// Per-file changelists restored from a shada store, keyed by *normalized*
    /// path. Seeded into a buffer's live `changelist` when it loads (alongside the
    /// file marks), so `g;`/`g,` walk a reopened file's change history. Drained by
    /// [`Editor::seed_pending_file_marks`].
    pending_changelists: HashMap<PathBuf, Vec<(usize, usize)>>,
    /// The focused window's jumplist restored from a shada store, as `(path, line,
    /// col)` not yet resolved to buffers. Materialized into the live window jumps —
    /// opening the files — on the first `<C-o>`/`<C-i>`, so a restored session can
    /// walk its jump history without bulk-loading every jumped-to file at launch.
    /// Drained by [`Editor::materialize_pending_jumplist`].
    pending_jumplist: Vec<(PathBuf, usize, usize)>,
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
    /// The mode to restore when the command line closes. Normally [`Mode::Normal`],
    /// but a `/`-search opened from [`Mode::MultiCursor`] returns there, so you can
    /// `/`-navigate to a match and keep dropping cursors. Set on entry.
    cmdline_return_mode: Mode,
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
    /// Memoized compiled search regex for the redraw highlight path, keyed by
    /// `(pattern, ignorecase, engine)`. The `hlsearch` pattern is stable across
    /// many frames, so this skips recompiling the regex (an expensive
    /// `RegexBuilder::build` / vim-engine compile) on every repaint of every
    /// window. The engine is part of the key so toggling `'regexsyntax'`
    /// recompiles. `RefCell` for interior mutability behind the `&self` highlight
    /// projection.
    search_re_cache: RefCell<SearchReCache>,
    /// The last `:substitute` as `(pattern, replacement, flag letters)`, for
    /// bare `:s` / `:&` / `:&&` repeats and the `~` replacement recall. The flag
    /// letters exclude any trailing count. `None` until the first substitute.
    last_substitute: Option<(String, String, String)>,
    /// An in-flight `:s///c` confirm substitute: the match-by-match walk paused
    /// on a `replace with … (y/n/a/l/q)?` prompt. While `Some`, every key is the
    /// answer to that prompt (routed ahead of mode handling in [`Editor::input`]);
    /// the buffer is otherwise in normal mode with the cursor on the pending
    /// match. `None` outside a confirm substitute.
    subst_confirm: Option<ex::SubstConfirm>,
    /// True while a `:global` / `:vglobal` is running its per-line command pass,
    /// so a nested `:g` / `:v` in that command fails loud (`E147`) instead of
    /// recursing. Set around the second pass in [`Editor::ex_global`].
    in_global: bool,
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
    /// The floating selectable-list widget, when open (`nx.ui.select`; the shared
    /// picker / completion surface). Grabs input focus like the panel, but floats
    /// over the text. See [`menu`](crate::editor::MenuPlacement).
    menu: Option<menu::Menu>,
    /// Resolved menu outcomes: `Some(key)` when the user confirmed a row (the
    /// source key — a `select` choice index, or a picker item's wrapper key),
    /// `None` on cancel. Drained by the server to deliver the result to its
    /// callback — the menu analogue of [`Editor::prompt_results`].
    pub menu_results: Vec<Option<usize>>,
    /// Picker query edits awaiting a (dynamic) source re-run: each `(generation,
    /// query)`. A *static* source never appends here — the local fuzzy matcher
    /// handles its query edits in core. Drained by the server, which stamps the
    /// generation onto the source run + its pushes so a stale response is dropped.
    pub picker_query_changes: Vec<(u64, String)>,
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
    registers: Registers,

    /// The accumulated, not-yet-complete normal/visual command — the count, the
    /// pending operator, and the [`Stage`] of the in-progress sequence. Decided
    /// by the pure [`parse_step`]; reset on every completed command.
    pending: PendingCommand,
    /// The half-typed `gg` prefix while a directory-listing buffer (the file
    /// explorer) is focused: the first `g` arms it so the second completes the
    /// jump-to-top, without delegating a bare `g` to the normal-mode grammar
    /// (where it could start an editing `g`-command). See
    /// [`Editor::handle_explorer`].
    explorer_gpending: bool,
    /// The last find-char motion as `(kind, target)`, replayed by `;` (same
    /// direction) and `,` (opposite). Cross-command memory, not pending state, so
    /// it lives outside [`PendingCommand`] and survives `reset_pending`.
    last_find: Option<(FindKind, char)>,
    /// Keys accumulated since the current normal-mode command boundary — the
    /// in-progress candidate change. Committed to [`Editor::last_change`] when the
    /// command finishes having edited the buffer; cleared when it finishes without
    /// editing. Recorded at the [`Editor::input`] chokepoint (see the wrapper there).
    redo_recording: Vec<Key>,
    /// The committed last buffer-changing command, replayed verbatim by `.`. Empty
    /// until the first dot-repeatable change.
    last_change: Vec<Key>,
    /// True while `.` is re-feeding [`Editor::last_change`] through `input`, so the
    /// replayed keys neither record themselves nor overwrite `last_change`.
    replaying_change: bool,
    /// The buffer's `changedtick` when the current command began. Compared at the
    /// command boundary: a higher tick means the command edited the buffer (commit
    /// it as the last change); equal means a pure motion (discard).
    change_start_tick: u64,
    /// Set when the in-progress command is one `.` must *not* capture (`u`/`<C-r>`,
    /// or anything routed through the command line). Reset at each boundary; when
    /// set at commit time the candidate is discarded even though the buffer changed.
    change_not_repeatable: bool,
    /// The text typed during the most recent insert session — vim's `".` register
    /// (last-insert). Cleared the instant a new insert session begins (the
    /// Normal→Insert transition in [`Editor::input`]) and grown char-by-char in
    /// [`Editor::handle_insert`], so once the session ends it holds exactly what
    /// was inserted, read back by `register_text(Some('.'))` and `"."`/`<C-r>.`.
    insert_text: String,
    /// The in-flight visual-mode change's size-faithful replay stream (see
    /// [`VisualShape`]). Set by [`Editor::visual_operate`] just before it mutates,
    /// consumed at the dot-repeat commit point in [`Editor::input`]; reset at each
    /// Normal-mode command boundary.
    pending_visual: Option<VisualShape>,
    /// Set when an undo snapshot has already been taken for the current edit
    /// "session" (e.g. an insert), so we group the whole session into one undo.
    snapshot_taken: bool,
    /// Current time in **monotonic** seconds, injected by the server before each
    /// message (core does no I/O, so it can't read a clock itself). Stamped onto
    /// undo nodes at commit and surfaced via `vim.fn.undotree()`/`localtime()`;
    /// monotonic so elapsed-time labels are immune to wall-clock jumps.
    now_mono: i64,
    /// Current time in **milliseconds**, injected by the server before each message
    /// (same source as the mouse multi-click clock). Finer-grained than [`now_mono`]
    /// for sub-second timing — the terminal triple-`<Esc>` chord window reads it.
    now_ms: u64,
    /// Provenance for soft-tab `<BS>`: `(line, anchor_col)` of the whitespace run
    /// the *immediately preceding* `<Tab>` keypress inserted as spaces (its
    /// `anchor_col` is where the whole run began, preserved across consecutive
    /// tabs). [`handle_insert`](Self::handle_insert) clears it before every other
    /// key, so only Tab-inserted spaces collapse a whole unit on backspace —
    /// hand-typed spaces always delete one at a time. `None` outside that window.
    soft_tab: Option<(usize, usize)>,
    /// Set by `<C-r>` in Insert / Command mode: the *next* keystroke names the
    /// register whose text is inserted at the cursor (vim's "insert a register"),
    /// then the flag clears. A non-register key (e.g. `<Esc>`) cancels, inserting
    /// nothing. Shared across both modes since only one is active at a time. See
    /// [`Editor::handle_insert`] / [`Editor::handle_command`].
    awaiting_register: bool,
    visual_anchor: Cursor,
    /// State for the in-flight left-button gesture: the multi-click counter that
    /// escalates char → word → line on same-cell presses within `'mousetime'`, and
    /// the anchor a drag extends from. Held across a press → drag → release and the
    /// gap to the next press (so a quick repeat at the same cell is a double-click).
    /// `None` before the first press / after a click outside any window. See
    /// [`crate::editor::mouse`].
    mouse_select: Option<mouse::MouseSelect>,

    /// State for an in-flight separator / status-line drag (Phase 5): which window
    /// edge is grabbed and the press origin the drag resizes against. `None` unless
    /// a left-press landed on a split divider. See [`crate::editor::mouse`].
    mouse_resize: Option<mouse::ResizeDrag>,

    /// Set by a scroll command or a cursor motion at the moment it fires:
    /// `(top, cursor.line)` *before* the move. Consumed at the end of `input` to
    /// build `pending_scroll` when the viewport ends up moving more than a line.
    scroll_from: Option<(usize, usize)>,
    /// The scroll gesture from the most recent input, projected into the next
    /// `View` and then cleared (so it animates exactly once).
    pending_scroll: Option<PendingScroll>,

    /// Undo/redo history for cursor *placement* in [`Mode::MultiCursor`]: each
    /// entry snapshots the placed-cursor set (primary + secondaries) before a
    /// placement command (`<A-c>`, `c`, `{count}c{motion}`, `cc`) mutated it, so
    /// `u`/`<C-r>` *while placing* step through the drops instead of the text undo
    /// tree — a `10cj` undoes as one step. Both stacks are cleared when a placement
    /// session begins or ends; a fresh placement after an undo discards the redo
    /// future. nxvim-native, transient: placement history is not document history.
    placement_undo: Vec<PlacementSnapshot>,
    placement_redo: Vec<PlacementSnapshot>,

    /// The text each cursor captured in the **last multi-cursor yank/delete**, in
    /// ascending document order (so entry `i` belongs to the `i`-th cursor by
    /// position). A multi-cursor `p`/`P` pastes each cursor's own entry when the
    /// count still matches the live cursor set; otherwise it falls back to the
    /// unnamed register at every cursor. Populated by [`Editor::edit_each_cursor`]
    /// via the collector below.
    cursor_registers: Vec<RegisterCell>,
    /// Active only during a multi-cursor editing sweep: each per-cursor yank/delete
    /// slice is pushed here as `(range_start, cell)`, then sorted by position into
    /// [`Editor::cursor_registers`] when the sweep ends. `None` outside a sweep, so
    /// single-cursor yanks never touch the per-cursor set.
    cursor_register_collect: Option<Vec<(usize, RegisterCell)>>,

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

    /// The in-process treesitter backend, or `None` in a bare-core test (or a
    /// front end that ships no grammars). The editor owns it so highlights — and,
    /// later, treesitter indentation — are answered **synchronously, in the same
    /// frame** as the edit. It keeps its own shadow text per buffer, so its
    /// methods never borrow [`Editor::buffers`]. Installed by the server via
    /// [`Editor::set_syntax_engine`].
    syntax: Option<Box<dyn SyntaxEngine>>,
    /// The language each buffer was last `open`ed in the engine with, so a query
    /// knows whether to re-`open` (first sync, or the path's language changed) vs
    /// apply incremental `edit` deltas. Dropped when the buffer is deleted. A
    /// `String` (not `&'static str`) because a `vim.treesitter.start` override can
    /// name any runtime language, not just one of the built-in extension table's.
    syntax_opened: HashMap<BufferId, String>,
    /// Languages whose grammar was *installed but failed to load*, already echoed
    /// once. Dedups the failure message so opening many files of a broken-grammar
    /// language doesn't spam (a *missing* grammar is silent and never recorded).
    syntax_failed: HashSet<String>,
    /// Per-buffer **filetype** override — the *language* noun. Absent: the
    /// buffer's filetype is derived from its path's extension (the default
    /// floor). `Some(ft)`: force `ft` (e.g. an extension the table misses);
    /// `Some("")`: explicitly no filetype. Orthogonal to [`Editor::ts_enabled`]:
    /// the filetype is what LSP/indent/statusline key off, independent of whether
    /// treesitter paints. Written by `nx.bo.filetype` / `:set filetype` / `:setf`.
    ts_filetype: HashMap<BufferId, String>,
    /// Per-buffer **treesitter-highlight enable** — the *whether* noun. Absent:
    /// enabled (the default, when a language resolves). `Some(false)`: highlighting
    /// off even though the filetype/language still resolves — so `filetype = rust`
    /// with `ts_highlight = false` keeps LSP/indent on rust while treesitter stays
    /// dark. Written by `nx.bo.ts_highlight` / `:set ts_highlight` / the
    /// `nx.treesitter` (and aliased `vim.treesitter`) `start`/`stop` verbs.
    ts_enabled: HashMap<BufferId, bool>,
    /// The host clipboard backing the `"+` / `"*` registers, or `None` in a
    /// bare-core test (or a front end whose platform backend failed to start).
    /// Injected by the server via [`Editor::set_clipboard`]; when absent,
    /// selecting `"+` / `"*` errors loudly instead of touching the unnamed
    /// register.
    clipboard: Option<Box<dyn Clipboard>>,
    /// The filesystem the editor reads and writes through — the local disk
    /// ([`StdHostFs`]) by default, swappable for a remote/daemon backend via
    /// [`Editor::set_host_fs`] (the edit-host split). An `Rc` rather than a `Box`
    /// so a `&mut`-borrowing buffer write (`self.buffer_mut().write(.., &*fs)`)
    /// can still lend it without aliasing `self`; core is single-threaded, so the
    /// non-atomic refcount is free.
    host_fs: Rc<dyn HostFs>,
    /// Off-tick filesystem mode (the daemon / edit-host split,
    /// `docs/plans/2026-06-09-edit-host-and-browser-lua.md` → Phase 3e/3f): when set,
    /// `:w` snapshots the buffer into [`Editor::pending_saves`] and `:edit` enqueues an
    /// [`Editor::pending_opens`] fetch instead of touching [`Editor::host_fs`]
    /// synchronously, so a remote read/write never blocks the single editor thread. Off
    /// by default — local builds do buffer I/O synchronously. Set via
    /// [`Editor::set_host_fs_offtick`].
    host_fs_offtick: bool,
    /// Writes deferred this tick under off-tick mode, drained by the server with
    /// [`Editor::take_pending_saves`] (the save analogue of [`Editor::prompt_results`]
    /// / [`Editor::panel_selects`]). Always empty when off-tick mode is off.
    pending_saves: Vec<PendingSave>,
    /// Monotonic id for the next [`PendingSave`], so the server can correlate acks and
    /// keep a buffer's overlapping writes ordered.
    next_save_seq: u64,
    /// Buffer opens deferred this tick under off-tick mode (`:edit` over the daemon
    /// wire), drained by the server with [`Editor::take_pending_opens`]. Each names an
    /// already-created (empty) buffer the server fills once the fetch lands. Always
    /// empty when off-tick mode is off.
    pending_opens: Vec<PendingOpen>,
    /// A `:wqa` / `:xa` quit deferred until every write its `:wall` enqueued has acked
    /// (off-tick mode), drained by the server with [`Editor::take_pending_quit_all`]. The
    /// single-buffer `:wq` rides [`PendingSave::then_quit`]; the batch quit needs the
    /// whole set, so core records it here and the server gates the `:qa` on all of them.
    /// `None` unless a `:wqa` with at least one modified file-backed buffer just ran.
    pending_quit_all: Option<PendingQuitAll>,
    /// File-backed buffers awaiting a `:checktime` reconcile this tick, drained by the
    /// server with [`Editor::take_pending_checktime`]. The reconcile fires the
    /// `FileChangedShell` autocmd (a Lua round-trip the pure core can't drive itself)
    /// and honors `v:fcs_choice`, so the *decision* is deferred to the server even
    /// though detection / reload live in core. Both `:checktime` and the per-buffer
    /// file watch ([`Editor::checktime_buffer`]) enqueue here.
    pending_checktime: Vec<BufferId>,

    /// Deferred shada I/O requests (`:wshada` / `:rshada`) raised this tick, drained
    /// by the server with [`Editor::take_pending_shada`]. Core can't touch the store
    /// (it lives behind the server's `ShadaStore` seam), so the ex-command enqueues a
    /// [`ShadaRequest`] and the server runs the flush / re-merge after the tick.
    pending_shada: Vec<ShadaRequest>,

    /// Terminal-job actions (open / input / kill) raised this tick, drained by the
    /// server with [`Editor::take_pending_terminal`]. Core is pure/sync and can't
    /// own a PTY, so a `:terminal` open and every keystroke forwarded in
    /// [`Mode::Terminal`](crate::mode::Mode::Terminal) enqueue a [`TerminalOp`] the
    /// server's terminal engine fulfills — the terminal analogue of
    /// [`pending_opens`](Self::pending_opens) / [`pending_saves`](Self::pending_saves).
    pending_terminal: Vec<TerminalOp>,
    /// Mid-`<C-\>` state in [`Mode::Terminal`](crate::mode::Mode::Terminal): set when
    /// `<C-\>` was pressed, so the next key decides between leaving to Normal (`<C-n>`)
    /// and forwarding both bytes to the child. Always `false` outside terminal mode.
    terminal_pending_backslash: bool,
    /// Consecutive `<Esc>` presses in [`Mode::Terminal`](crate::mode::Mode::Terminal):
    /// a discoverable escape hatch beside the neovim `<C-\><C-n>` chord — three in a
    /// row leave to Normal. Single/double `<Esc>` are still forwarded to the child (so
    /// a program that needs `<Esc>`, like vim/htop, keeps working). Reset by any other
    /// key and on leaving terminal mode.
    terminal_esc_count: u8,
    /// The current terminal's child-cursor position `(line, byte-col)`, stashed by
    /// [`Editor::terminal_update`] each refresh. Re-entering terminal-job mode (`i` /
    /// `a` from terminal-normal) snaps the cursor back here — to the live input
    /// position — rather than leaving it wherever normal-mode navigation parked it.
    terminal_cursor: (usize, usize),
    /// [`now_ms`](Self::now_ms) of the most recent `<Esc>` in terminal mode, so the
    /// triple-`<Esc>` chord only fires on three presses in *quick succession* — a gap
    /// longer than the chord window restarts the run (so a TUI program inside the
    /// terminal that wants a lone `<Esc>` isn't hijacked).
    terminal_last_esc_ms: u64,
    /// Armed by `<C-\><C-r>` (or `<C-S-r>`) in terminal mode: the next keystroke names
    /// the register whose text is sent to the child — the terminal analogue of insert
    /// mode's `<C-r>{register}`. Plain `<C-r>` is left for the child (shell reverse
    /// search), so this is behind a prefix. Always `false` outside that two-key chord.
    terminal_awaiting_register: bool,
}

impl Editor {
    pub fn new() -> Self {
        Editor::with_buffer(Buffer::empty())
    }

    /// Install the filesystem backend the editor reads/writes through. The server
    /// can hand over a remote (daemon-backed) [`HostFs`]; the default is the local
    /// [`StdHostFs`]. Mirrors [`Editor::set_syntax_engine`] / [`Editor::set_clipboard`].
    /// To open a startup file *through* an injected fs (so the first buffer is
    /// fetched through it, not the default disk), use [`Editor::open_or_named_with`]
    /// instead of this on an already-built editor.
    pub fn set_host_fs(&mut self, fs: Rc<dyn HostFs>) {
        self.host_fs = fs;
    }

    /// A clone of the filesystem backend handle, for callers that read files
    /// **outside** the buffer model — the picker's read-only preview pane reads the
    /// selected file through the same FS the editor opens buffers with, so a
    /// daemon-backed `HostFs` is honoured. (Mutation still goes through buffers.)
    pub fn host_fs(&self) -> Rc<dyn HostFs> {
        self.host_fs.clone()
    }

    /// Whether file I/O is routed **off the editor tick** (the daemon / wasm
    /// edit-host: [`set_host_fs_offtick`](Editor::set_host_fs_offtick)). When `true`
    /// a synchronous `host_fs` read is unavailable, so a synchronous preview read
    /// must be skipped (the preview rides the async FS seam instead).
    pub fn host_fs_offtick(&self) -> bool {
        self.host_fs_offtick
    }

    pub fn open(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        Ok(Editor::with_buffer(Buffer::from_file(
            path.into(),
            &StdHostFs,
            crate::encoding::DEFAULT_FILEENCODINGS,
        )?))
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
        Editor::open_or_named_with(path, Rc::new(StdHostFs))
    }

    /// [`Editor::open_or_named`], but the initial buffer is loaded through `fs`,
    /// which is then installed as the editor's [`HostFs`] — so the first buffer is
    /// fetched through the *same* backend as every later `:edit` / `:write`, and
    /// the server can open the startup file through a daemon-backed fs rather than
    /// the local disk (the edit-host split,
    /// `docs/plans/2026-06-09-edit-host-and-browser-lua.md` → Phase 3). The
    /// default-fs [`Editor::open_or_named`] is just this with [`StdHostFs`].
    ///
    /// Directory detection still goes through `std::path::Path::is_dir`; a remote
    /// fs would need a type-bearing stat, which arrives with the daemon wire
    /// protocol. For now only the file *read* / *write* crosses the seam — which
    /// is the part that has to be backend-agnostic.
    pub fn open_or_named_with(path: impl Into<PathBuf>, fs: Rc<dyn HostFs>) -> Self {
        let path = path.into();
        // A directory opens as the in-window file explorer (vim's netrw), not as
        // text. An unreadable directory (no permission) still fails loud, the same
        // way an unreadable file does below.
        let mut editor = if path.is_dir() {
            match Buffer::from_dir(&path, &*fs) {
                Ok(buffer) => Editor::with_buffer(buffer),
                Err(e) => {
                    let mut editor = Editor::with_buffer(Buffer::named(path.clone()));
                    editor.echo(format!("E484: Can't open file {}: {e}", path.display()));
                    editor
                }
            }
        } else {
            match Buffer::from_file(&path, &*fs, crate::encoding::DEFAULT_FILEENCODINGS) {
                Ok(buffer) => Editor::with_buffer(buffer),
                Err(e) => {
                    let mut editor = Editor::with_buffer(Buffer::named(path.clone()));
                    editor.echo(format!("E484: Can't open file {}: {e}", path.display()));
                    editor
                }
            }
        };
        editor.host_fs = fs;
        editor
    }

    fn with_buffer(buffer: Buffer) -> Self {
        let (buffers, current) = BufferStore::with_one(buffer);
        let windows = WindowTree::with_one(current);
        let mut editor = Editor {
            buffers,
            windows,
            main_tabs: TabStack::live(TabId(1)),
            next_win_id: 2,
            next_tab_id: 2,
            dock_tabs: [None, None, None, None],
            focused_layer: Layer::Main,
            dock_sizes: [0; 4],
            dock_options: Default::default(),
            dock_hidden: [false; 4],
            last_dock: DockSide::Left,
            dock_chord: DockChord::default(),
            restore_mode_on_enter: false,
            alternate: None,
            global_marks: HashMap::new(),
            pending_global_marks: HashMap::new(),
            pending_file_marks: HashMap::new(),
            numbered_marks: HashMap::new(),
            pending_changelists: HashMap::new(),
            pending_jumplist: Vec::new(),
            mode: Mode::Normal,
            cursor: Cursor::default(),
            top: 0,
            leftcol: 0,
            cmdline: String::new(),
            cmdline_col: 0,
            cmdline_kind: CmdlineKind::Ex,
            cmdline_return_mode: Mode::Normal,
            cmdline_prompt: String::new(),
            prompt_results: Vec::new(),
            confirm_accelerators: Vec::new(),
            confirm_default: 0,
            last_search: None,
            search_re_cache: RefCell::new(None),
            last_substitute: None,
            subst_confirm: None,
            in_global: false,
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
            menu: None,
            menu_results: Vec::new(),
            picker_query_changes: Vec::new(),
            should_quit: false,
            options: Options::default(),
            highlights: Highlights::new(),
            width: 80,
            height: 24,
            desired_col: 0,
            desired_eol: false,
            preserve_desired: false,
            eol_request: false,
            registers: Registers::default(),
            pending: PendingCommand::default(),
            explorer_gpending: false,
            last_find: None,
            redo_recording: Vec::new(),
            last_change: Vec::new(),
            replaying_change: false,
            change_start_tick: 0,
            change_not_repeatable: false,
            insert_text: String::new(),
            pending_visual: None,
            snapshot_taken: false,
            now_mono: 0,
            now_ms: 0,
            soft_tab: None,
            awaiting_register: false,
            visual_anchor: Cursor::default(),
            mouse_select: None,
            mouse_resize: None,
            scroll_from: None,
            pending_scroll: None,
            placement_undo: Vec::new(),
            placement_redo: Vec::new(),
            cursor_registers: Vec::new(),
            cursor_register_collect: None,
            lua_queue: Vec::new(),
            deferred_commands: Vec::new(),
            pending_sleep: None,
            syntax: None,
            syntax_opened: HashMap::new(),
            syntax_failed: HashSet::new(),
            ts_filetype: HashMap::new(),
            ts_enabled: HashMap::new(),
            clipboard: None,
            host_fs: Rc::new(StdHostFs),
            host_fs_offtick: false,
            pending_saves: Vec::new(),
            next_save_seq: 0,
            pending_opens: Vec::new(),
            pending_quit_all: None,
            pending_checktime: Vec::new(),
            pending_shada: Vec::new(),
            pending_terminal: Vec::new(),
            terminal_pending_backslash: false,
            terminal_esc_count: 0,
            terminal_cursor: (0, 0),
            terminal_last_esc_ms: 0,
            terminal_awaiting_register: false,
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

    /// Mutable access to buffer `id`'s text model, or `None` if no such buffer is
    /// open. The buffer-addressed counterpart to [`Editor::buffer_mut`], used by
    /// the extmark effect drain to mutate a (possibly non-current) buffer's
    /// [`crate::extmark::ExtmarkStore`] directly. Text mutation still funnels
    /// through [`Editor::apply_edits_to`]; this is for the side metadata.
    pub fn buffer_of_mut(&mut self, id: BufferId) -> Option<&mut Buffer> {
        self.buffers.map.get_mut(&id).map(|ob| &mut ob.buffer)
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

    /// Take any pending `:sleep` duration in milliseconds, clearing it. The
    /// server awaits this between message handling, so a slow editor operation
    /// never blocks the client (a separate thread/process).
    pub fn take_sleep(&mut self) -> Option<u64> {
        self.pending_sleep.take()
    }

    /// Inject the current monotonic time (seconds) — the server calls this before
    /// handling each message so undo-node timestamps and `vim.fn.localtime()`
    /// share one monotonic timeline. See [`Editor::now_mono`].
    pub fn set_now_mono(&mut self, secs: i64) {
        self.now_mono = secs;
    }

    /// Inject the current time in milliseconds (same source as the mouse clock),
    /// before each message. Read by sub-second timing like the terminal triple-`<Esc>`
    /// chord window. See [`Editor::now_ms`].
    pub fn set_now_ms(&mut self, ms: u64) {
        self.now_ms = ms;
    }

    /// Feed a single key into the editor.
    pub fn input(&mut self, key: Key) {
        // A focused panel grabs every key (navigation + close), bypassing the
        // buffer's mode handling and the `curswant`/scroll bookkeeping below.
        if self.panel.is_some() {
            self.handle_panel(key);
            return;
        }

        // A focused menu (`nx.ui.select`, later the picker) grabs every key the
        // same way — navigation + confirm / cancel — floating over the text.
        if self.menu.is_some() {
            self.handle_menu(key);
            return;
        }

        // A directory-listing buffer (the file explorer) owns its keys in normal
        // mode: navigation, `<CR>` to open the entry, `-` to go up. Editing keys
        // are inert so the listing can't be corrupted; `:`/`/`/`?` fall through to
        // open the command line. Once mid-sequence (`g` of `gg`) or in another
        // mode the explorer keeps handling until it returns to a clean normal
        // boundary. See [`Editor::handle_explorer`].
        if self.mode == Mode::Normal && self.is_explorer_buffer() {
            self.handle_explorer(key);
            return;
        }

        // The mode-independent `<C-w><C-w>` dock-navigation chord, ahead of the
        // per-mode routing below so it reaches the docks from *any* mode — insert,
        // visual, command, terminal — not just Normal (where the command grammar
        // already owns `<C-w>`). Consumes the held prefix and the completed cross;
        // a miss replays the held `<C-w>`(s) into the current mode and falls
        // through. Sits after the panel/menu grabs (which own every key) but
        // before the terminal forward, so a terminal `<C-w><C-w>` crosses instead
        // of going to the child.
        if self.dock_chord_intercept(key) {
            return;
        }

        // Terminal-job mode forwards every keystroke to the PTY child as input
        // bytes; `<C-\><C-n>` is the one exception, leaving to Normal. This sits
        // ahead of the mode dispatch and the scroll/dot-repeat bookkeeping below
        // (terminal keys are neither motions nor repeatable edits).
        if self.mode == Mode::Terminal {
            self.handle_terminal_key(key);
            return;
        }

        // Terminal-*normal* mode (Normal mode on a terminal buffer): the buffer
        // reads as ordinary text for scrolling / yanking, but the insert-entering
        // commands return to the job instead of editing the read-only mirror.
        if self.mode == Mode::Normal
            && self.buffer().terminal
            && self.pending.is_clean()
            && !key.ctrl
            && !key.alt
            && matches!(key.code, KeyCode::Char('i' | 'a' | 'A' | 'I' | 'o' | 'O'))
        {
            self.enter_terminal_mode();
            return;
        }

        // A `:s///c` confirm substitute owns every key as the answer to its
        // `replace with … ?` prompt, ahead of mode handling, until it resolves.
        if self.subst_confirm.is_some() {
            self.subst_confirm_key(key);
            return;
        }

        self.preserve_desired = false;
        self.eol_request = false;

        // The mode before dispatch: a Normal→Insert transition on this key starts
        // a fresh insert session, which resets the `".` last-insert accumulator.
        let was_insert = matches!(self.mode, Mode::Insert | Mode::Replace);

        // Snapshot the viewport up front so *any* navigation keystroke that ends up
        // moving it more than a line animates the slide (see
        // [`finalize_scroll_gesture`](Self::finalize_scroll_gesture)) — explicit
        // scrolls, motions, jumps (`<C-o>`/`<C-i>`), the change list (`g;`/`g,`),
        // search (`n`/`N`), marks, … — captured once here rather than in each
        // handler. Two things unset it before the gesture is built: an edit (the
        // `changedtick` check below) keeps typing/deleting crisp, and a
        // buffer/tab/window switch (which clears `scroll_from` itself) has no slide
        // to play. Skipped in insert/command mode, where the viewport tracks typing
        // or the incsearch preview and should stay crisp, not animate per keystroke.
        let pre_tick = self.buffer().changedtick;
        if !self.mode.is_insert() && self.mode != Mode::Command {
            self.scroll_from = Some((self.top, self.cursor.line));
        }

        // Dot-repeat recording. The *outer* call (not a `.` replay) records its
        // raw key stream from one normal/visual command boundary to the next; if
        // the command edited the buffer it becomes `last_change`, replayed by `.`.
        // Replayed keys (`replaying_change`) skip this entirely — they only execute.
        let recording = !self.replaying_change;
        // A new candidate change begins only at a clean *Normal*-mode boundary —
        // not mid-Visual: a visual selection (`v`…`d`) is one change spanning the
        // `v`, its motions, and the operator, so the bracket opens at the `v` and
        // stays open until the operator returns to Normal.
        let starting = recording && self.mode == Mode::Normal && self.pending.is_clean();
        if starting {
            self.redo_recording.clear();
            self.change_start_tick = self.buffer().changedtick;
            self.change_not_repeatable = false;
            self.pending_visual = None;
        }
        if recording {
            self.redo_recording.push(key);
        }

        match self.mode {
            Mode::Insert | Mode::Replace => self.handle_insert(key),
            Mode::Command => self.handle_command(key),
            _ => self.handle_normal(key),
        }

        // A command that just entered insert mode (`i`, `cw`, `o`, `s`, `R`, …)
        // opens a new insert session: clear the `".` accumulator so it captures
        // only this session's typed text. The key that entered insert never types
        // text itself, so clearing after dispatch is safe — the following keys'
        // `handle_insert` calls grow `insert_text` from empty.
        if matches!(self.mode, Mode::Insert | Mode::Replace) && !was_insert {
            self.insert_text.clear();
        }

        if recording {
            // Entering the command line (`:`/`/`/`?`) makes this command
            // non-repeatable: `:d`, `:s`, and operator-search `d/foo` are not
            // `.`-repeatable in vim. Caught centrally since they all transit
            // `Command` mode.
            if self.mode == Mode::Command {
                self.change_not_repeatable = true;
            }
            // The command is finished exactly when we are back at a clean normal-
            // mode boundary — which correctly spans an insert session (`ciw…<Esc>`
            // is not "done" until `<Esc>` returns to Normal).
            let done = self.mode == Mode::Normal && self.pending.is_clean();
            if done {
                let changed = self.buffer().changedtick != self.change_start_tick;
                if changed && !self.change_not_repeatable {
                    // A visual-initiated change replays as a *size-faithful*
                    // reselect (vim reselects the same extent from the new cursor,
                    // not the original motions), so commit the synthesized stream
                    // `visual_operate` stashed; a change (`c`) appends the inserted
                    // text it could only know once insert ended. Everything else
                    // commits its raw recorded keys.
                    self.last_change = match self.pending_visual.take() {
                        Some(mut shape) => {
                            if shape.is_change {
                                shape.keys.extend(insert_text_keys(&self.insert_text));
                                shape.keys.push(Key::new(KeyCode::Esc));
                            }
                            shape.keys
                        }
                        None => std::mem::take(&mut self.redo_recording),
                    };
                }
                self.redo_recording.clear();
            }
        }

        // Update vim's `curswant`: vertical motions keep the remembered column,
        // every other action recomputes it from where the cursor landed.
        if !self.preserve_desired {
            self.desired_col = self.cursor_virtcol();
            self.desired_eol = self.eol_request;
        }
        self.ensure_visible();

        // Edits stay crisp: only a navigation that moved the viewport animates, so
        // a keystroke that changed the buffer drops the snapshot taken above. (A
        // buffer switch already cleared `scroll_from`; comparing its `changedtick`
        // to the pre-dispatch one is then irrelevant — the snapshot is already gone.)
        if self.buffer().changedtick != pre_tick {
            self.scroll_from = None;
        }
        // Turn the surviving snapshot into a gesture when the viewport moved more
        // than a line (an explicit scroll, or a motion/jump/search that landed
        // off-screen); the client animates the slide.
        self.finalize_scroll_gesture();
    }

    /// Turn a recorded `scroll_from` into a `pending_scroll` animation when the
    /// focused window's viewport moved more than a line — an explicit scroll, an
    /// off-screen motion, or the mouse wheel. A one-line shift (holding `j`/`k` at
    /// the edge, or a single-line wheel notch) is left alone so continuous
    /// scrolling stays crisp. Shared by keyboard [`input`](Self::input) and the
    /// wheel ([`mouse`](Self::mouse)) so both animate identically.
    pub(crate) fn finalize_scroll_gesture(&mut self) {
        let Some((from_top, from_cursor)) = self.scroll_from.take() else {
            return;
        };
        if from_top.abs_diff(self.top) <= 1 {
            return;
        }
        // Honor `'scrollanim'` / `'scrollanimduration'`: with animation off (or a
        // zero duration cap) emit no `scroll` descriptor, so the client snaps
        // straight to the destination instead of sliding.
        let dur_cap = self.options.scrollanimduration as u64;
        if !self.options.scrollanim || dur_cap == 0 {
            return;
        }
        // Cap the visual travel so a huge jump (e.g. `G` in a big file) animates a
        // bounded slide of the last couple of screens instead of projecting
        // thousands of lines into the view.
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
        // 8ms per line of travel, clamped to the configurable `scrollanimduration`
        // ceiling (default 160). The floor is the usual 80ms, lowered to the cap
        // itself when the user picks a shorter one so a small cap means a fixed,
        // snappy slide rather than an empty range.
        self.pending_scroll = Some(PendingScroll {
            from_top,
            to_top: self.top,
            from_cursor,
            to_cursor: self.cursor.line,
            duration_ms: (dist * 8).clamp(80.min(dur_cap), dur_cap),
        });
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

    /// The editor's total screen size in `(columns, rows)` — the text-viewport
    /// dimensions the client last sized us to (via [`Editor::resize`] / the most
    /// recent `view`). Backs `vim.o.columns` / `vim.o.lines` (and
    /// `nvim_list_uis`), the values a float-positioning plugin reads to center
    /// and size its windows. NOTE: `rows` is the
    /// editable text height — the client owns the cmdline / status regions — so it
    /// runs a row or two short of neovim's total `lines`; it is the only screen
    /// extent the core knows, and is close enough for float geometry.
    pub fn screen_size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Produce a [`View`] of the current state for a text viewport of the given
    /// size. The client renders the view's regions with its own widgets.
    pub fn view(&mut self, width: usize, height: usize) -> View {
        self.resize(width, height);
        let view = View::from_editor(self);
        self.pending_scroll = None; // animate exactly once
                                    // The preview-scroll gesture is one-shot: the server folded it into its
                                    // persistent offset while projecting `view`, so drop it before the next frame.
        self.clear_preview_scroll();
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

    /// True when the next key is consumed by core as a *literal argument* — the
    /// `r{char}` replacement, an `f`/`t`/`F`/`T{char}` target, a `"{reg}` register
    /// name, or an `i`/`a{kind}` text-object kind. Like vim, these argument keys are
    /// read raw (`plain_vgetc`), **not** through the mapping layer, so the server
    /// routes them straight to [`Self::input`] instead of the keymap matcher. This
    /// is what keeps `rg`/`fg` instant: without it the matcher withholds the `g` as
    /// a live prefix of the native `gd`/`gr` maps and the command appears to hang.
    ///
    /// `GPending` and `WindowPending` are deliberately excluded: `g`-prefix and
    /// `<C-w>`-prefix keys *do* participate in mapping (the native `gd`/`gr`/`gg`
    /// disambiguation, and user `<C-w>x` maps), so they must still go through the
    /// matcher.
    pub fn awaiting_literal_arg(&self) -> bool {
        matches!(
            self.pending.stage,
            Stage::ReplacePending
                | Stage::FindPending(_)
                | Stage::RegisterPending
                | Stage::TextObjectPending(_)
        )
    }

    /// The fixed end of the visual selection (the other end is [`Self::cursor`]).
    /// Only meaningful while [`Self::mode`] is a visual mode.
    pub(crate) fn visual_anchor(&self) -> Cursor {
        self.visual_anchor
    }

    /// The visual mode governing the rendered selection, or `None` when none
    /// should show. Normally just [`Self::mode`] when it's visual — but a `/`,`?`
    /// search *opened from* Visual keeps the selection live (it returns to that
    /// mode on `<CR>`), so while the search command line is open the selection
    /// still renders, extended to the incsearch preview at [`Self::cursor`]. Drives
    /// the View's selection highlight.
    pub(crate) fn rendered_visual_mode(&self) -> Option<Mode> {
        if self.mode.is_visual() {
            return Some(self.mode);
        }
        let searching =
            self.mode == Mode::Command && matches!(self.cmdline_kind, CmdlineKind::Search(_));
        (searching && self.cmdline_return_mode.is_visual()).then_some(self.cmdline_return_mode)
    }

    // ----- pending-state mirror (vim.v.*) ----------------------------------

    /// The count accumulated for the pending normal/visual command — `v:count`.
    /// `0` when no count was typed (matching vim, which reports 0 while the
    /// command still acts with an effective count of 1). A count typed both
    /// before and after an operator (`2d3w`) multiplies, like
    /// [`effective_count`](Self::effective_count).
    pub fn pending_count(&self) -> usize {
        match (self.pending.op_count, self.pending.count) {
            (None, None) => 0,
            (a, b) => a.unwrap_or(1) * b.unwrap_or(1),
        }
    }

    /// `v:count1`: the pending count, but at least 1.
    pub fn pending_count1(&self) -> usize {
        self.pending_count().max(1)
    }

    /// The register named by a leading `"x` for the pending command
    /// (`v:register`), or `"` (the unnamed register) when none was given —
    /// matching vim's default.
    pub fn pending_register(&self) -> char {
        self.pending.register.unwrap_or('"')
    }

    /// The pending operator awaiting its motion (`v:operator` — `d`/`c`/`y`/…),
    /// or `None` when no operator is pending.
    pub fn pending_operator(&self) -> Option<char> {
        self.pending.operator
    }

    /// Discard the accumulated normal/visual command (count, operator, register).
    /// Called when a user mapping fires: the count/register typed ahead of it
    /// (`3<leader>x`, `"a<leader>p`) were the mapping's arguments — surfaced to it
    /// as `v:count` / `v:register` — and the mapping has now consumed them, exactly
    /// as a built-in command would. Without this they would leak into the next
    /// command, since a mapping fires *outside* [`Editor::input`] and so never
    /// reaches the chokepoint that normally resets pending state.
    pub fn clear_pending_command(&mut self) {
        self.reset_pending();
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

/// Map a file path's extension to a treesitter language / filetype name, or
/// `None` for an unknown (or absent) extension — in which case the buffer has no
/// highlighting and no treesitter indentation. This is the single seam where
/// more languages plug in; the server's `filetype_of` (FileType autocmd, LSP)
/// delegates here so the table lives in exactly one place.
pub fn language_of_path(path: Option<&Path>) -> Option<&'static str> {
    let ext = path?.extension()?.to_str()?;
    Some(match ext {
        "rs" => "rust",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" => "typescript",
        "json" => "json",
        "toml" => "toml",
        "md" | "markdown" => "markdown",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "go" => "go",
        "lua" => "lua",
        "html" => "html",
        "css" => "css",
        "yaml" | "yml" => "yaml",
        "zig" => "zig",
        "sh" | "bash" => "bash",
        _ => return None,
    })
}
