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
use crate::mode::{KeyContext, Mode};
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
mod cmdcomplete;
mod cmdline;
mod command;
mod comment;
mod complete;
mod cursor;
mod decor;
mod dock;
mod ex;
pub mod expr;
mod float;
mod fold;
mod helix;
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
mod quickfix;
mod registers;
mod search;
mod select;
mod selection;
mod signature;
pub mod snippet;
mod syntax;
mod tabs;
mod terminal;
mod undo;
mod view;
mod windows;

// The command grammar + its normal/visual executor. The parse↔execute contract
// types stay private to `command`; only the shared vocabulary is re-exported.
pub use self::cmdcomplete::CmdlineCompleteReq;
pub use self::command::{
    command_pending_after, command_status, CommandContinuation, CommandPending, CommandStatus,
};
pub(crate) use self::command::{
    DockChord, FindKind, FoldCmd, Motion, MotionKind, MotionResult, MoveAxis, ObjectKind,
    PendingCommand, Stage,
};
pub use self::complete::{AcceptBehavior, CompleteConfig, CompleteCtx, CompleteKeys};
pub use self::decor::DecorViewport;
pub use self::menu::{
    CmdlineCandidate, Extent, MenuGeom, MenuItem, MenuMetrics, MenuPlacement, PreviewScroll,
    PreviewTarget, PromptPos,
};
pub use self::mouse::{ClickSurface, MouseClick, MousePos, StatuslineClick, WheelGesture};
pub(crate) use self::multicursor::PlacementSnapshot;
// The shared selection vocabulary both grammars read (see `editor::selection`).
pub(crate) use self::selection::{Range, Selections};
// The off-tick save / open requests (the daemon / edit-host fs path, Phase 3e/3f).
pub use self::buffers::{
    CommitOutcome, FileChangeAction, FileChangeReason, PendingOpen, PendingQuitAll, PendingSave,
    PreWrite, WriteEvent, WriteScope,
};
pub use self::marks::MarkMirrorEntry;
pub use self::persist::{
    FileChangelist, FileFolds, FileMarkEntry, GlobalMarkEntry, InputHistoryEntry, JumpPos,
    NumberedMark, PersistState, PluginEntry, PluginNamespace, RegisterEntry, SessionDock,
    SessionState, SessionTab, SessionWindow, ShadaRequest,
};
pub use self::terminal::TerminalOp;
pub use self::undo::{UndoEntry, UndoTreeView};
// The window layout subsystem (tree types + layout algebra + window methods).
pub(crate) use self::fold::Fold;
pub(crate) use self::jumps::JumpEntry;
pub(crate) use self::view::ViewState;
pub use self::windows::{
    place_aligned, Align, BorderStyle, FloatAnchor, FloatConfig, FloatRelative, Margin,
    WindowConfigSpec,
};
pub(crate) use self::windows::{PendingScroll, TabLabel, WindowLayout, WindowTree};
// Search vocabulary shared by the command line, the parser, and the View.
pub use self::quickfix::{
    LocListEntry, NamedList, NamedListId, QfAction, QfEntry, QfList, QfStack, QfWhich,
};
pub(crate) use self::search::{SearchDir, SearchOffset};
pub(crate) use self::syntax::fill_indent;

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

/// Anchor `path` to the process working directory if it is relative, then
/// lexically [`normalize_path`] it — so a cwd-relative name (`:e src/foo.rs`) and
/// the absolute path that names the same file (e.g. what the LSP hands back) hash
/// to the same key for buffer dedup. The process cwd is the one source of truth:
/// the server keeps `std::env::current_dir()` equal to the editor's effective
/// working dir (`:cd` / `fix_current_dir`), and that is what the LSP runs under
/// too, so this is correct over a remote daemon as well (the daemon's core reads
/// the daemon's cwd, never the client's). Still filesystem-free apart from reading
/// the cwd — no symlink resolution, no blocking `stat`/`canonicalize` — matching
/// vim's path-based (not inode-based) buffer dedup. If the cwd can't be read
/// (some wasm hosts) it degrades to the old lexical-only behavior.
fn absolutize_normalize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize_path(path)
    } else if let Ok(cwd) = std::env::current_dir() {
        normalize_path(&cwd.join(path))
    } else {
        normalize_path(path)
    }
}

/// Do two paths name the same file once anchored to the process cwd? The
/// cwd-aware companion of a raw `==` on the stored buffer path — the comparison
/// every "is this the buffer / file I mean?" site goes through so an absolute and
/// a cwd-relative spelling of one file are treated as one. See
/// [`absolutize_normalize`].
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    absolutize_normalize(a) == absolutize_normalize(b)
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

/// One recorded entry in the `:messages` history: the (single-line) text plus
/// whether it was surfaced as an *error*. The `error` flag drives the red
/// `ErrorMsg` highlight the `:messages` panel paints on error lines — set
/// explicitly for `:echoerr` and inferred from vim's `E###:` error-code prefix
/// for the many command errors that flow through [`Editor::echo`].
#[derive(Debug, Clone)]
pub struct LoggedMessage {
    pub text: String,
    pub error: bool,
}

/// A command line the core queued for the server to run after the tick.
///
/// The two kinds exist so a `|` chain keeps running in the order it was typed:
/// `:MyCmd|w` must write *after* `MyCmd` has run, but the core can't run `MyCmd`
/// itself (it lives in the Lua command table). So the core defers the segment it
/// doesn't know and defers the rest of the line behind it, instead of racing ahead.
#[derive(Debug, Clone)]
pub enum DeferredCmd {
    /// Unknown to the core: the server resolves it against Lua user commands and its
    /// own surface (`:source`, `:make`, …) before reporting an unknown-command error.
    /// `range` is the **explicitly addressed** 0-based inclusive line range the
    /// command carried (`:'<,'>Cmd`, `:5,10Cmd`), or `None` when no address was given
    /// — the core resolves the addresses (it owns the marks and the buffer), the
    /// server's command decides what to do with them.
    Server {
        cmd: String,
        range: Option<(usize, usize)>,
    },
    /// The tail of a `|` chain, to be run back **through the core** once the deferred
    /// segment ahead of it has resolved. Skipped when that segment errored, matching
    /// vim's abandon-the-rest-of-the-line-on-error behavior.
    Chain(String),
}

/// Whether `line` reads as a vim error message: the `E###:` error-code prefix
/// (an `E` followed by one or more digits and a colon, e.g. `E486:`) every
/// built-in error carries. Lets [`Editor::record_message`] flag the bulk of
/// command errors as errors without each call site opting in.
fn is_error_line(line: &str) -> bool {
    let Some(rest) = line.strip_prefix('E') else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && rest.as_bytes().get(digits) == Some(&b':')
}

/// Trim a bounded ring to its newest `cap` entries (dropping the oldest from the
/// front) — the history rings (`'history'`; a cap of 0 disables that history)
/// and the `:messages` log share it.
pub(crate) fn cap_ring<T>(ring: &mut Vec<T>, cap: usize) {
    if ring.len() > cap {
        ring.drain(0..ring.len() - cap);
    }
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
    /// A Helix selection-regex prompt (`s`/`S`/`K`/`Alt-K`): the typed pattern is
    /// applied to the current selection set on `<CR>` via
    /// [`Editor::helix_apply_regex`] (`<Esc>` cancels). Opened from a Helix mode,
    /// it resumes [`Mode::HelixNormal`] on close. See [`crate::editor::helix`].
    HelixRegex(HelixRegexOp),
}

/// Which selection transform a [`CmdlineKind::HelixRegex`] prompt drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HelixRegexOp {
    /// `s` — replace each selection with one selection per regex match within it.
    Select,
    /// `S` — split each selection on the regex (the gaps between matches).
    Split,
    /// `K` — keep only the selections that contain a match.
    Keep,
    /// `Alt-K` — keep only the selections that do *not* contain a match.
    Remove,
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
    /// Which window [`Layer`] this buffer belongs to — the layer of the window it
    /// was last shown in. The buffer list is **per-layer**: `:ls` lists only the
    /// focused layer's buffers, and closing a buffer falls back to a sibling in the
    /// *same* layer (never pulling a dock buffer into the main area, or vice versa).
    /// Updated whenever the buffer is bound into a window ([`Editor::set_cur_buffer`]
    /// / [`Editor::set_window_buffer`]); defaults to `Main` for a freshly created
    /// buffer until it is first displayed.
    layer: Layer,
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
            // Assigned to the live layer as soon as the buffer is shown in a window;
            // the startup buffer (created before any dock) is a main buffer.
            layer: Layer::Main,
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
    /// The alternate **file name** — vim's `#` as a *name* rather than a live
    /// handle. Tracked separately from [`Self::alternate`] because the two outlive
    /// each other differently: vim's `:bdelete` only *unlists* a buffer, so `#`
    /// keeps naming it and `:e #` reloads it from disk. nxvim frees the buffer
    /// outright, so the id can't survive — the name does, which is what makes the
    /// `:%bd|e#` idiom work. Set wherever the alternate is (`switch_buffer`, and a
    /// `:bdelete` of the current buffer, which vim makes the new alternate).
    alternate_name: Option<PathBuf>,
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
    /// Per-file **manual** folds restored from a shada store, keyed by *normalized*
    /// path. Seeded into the focused window's [`FoldState`](crate::editor::fold)
    /// when the file becomes its buffer (drained by [`Editor::seed_pending_folds`]),
    /// so a reopened file gets its `:mkview`-style folds back.
    pending_folds: HashMap<PathBuf, Vec<(usize, usize, bool)>>,
    /// The focused window's jumplist restored from a shada store, as `(path, line,
    /// col)` not yet resolved to buffers. Materialized into the live window jumps —
    /// opening the files — on the first `<C-o>`/`<C-i>`, so a restored session can
    /// walk its jump history without bulk-loading every jumped-to file at launch.
    /// Drained by [`Editor::materialize_pending_jumplist`].
    pending_jumplist: Vec<(PathBuf, usize, usize)>,
    pub mode: Mode,
    /// `i_CTRL-O` one-shot: while `Some`, the editor is in "insert-normal" — it left
    /// Insert (or Replace) for exactly one Normal-mode command and must resume the
    /// stored mode the moment that command settles at a clean Normal boundary. The
    /// value is the mode to return to ([`Mode::Insert`] or [`Mode::Replace`]).
    /// `None` the rest of the time. Consulted by [`Editor::mode_code`] (reports
    /// `niI`/`niR`) and [`Editor::clamp_cursor`] (keeps the EOL-append column, like
    /// `virtualedit=onemore`, so returning to Insert lands past the last char).
    pub(crate) insert_normal: Option<Mode>,
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
    /// The visual mode a [`Mode::Command`] line was opened *from* ([`Mode::Visual`]
    /// / [`Mode::VisualLine`]), or `None` when it wasn't opened over a selection.
    /// vim keeps the selection painted while the command line is open — both a
    /// `/`,`?` search (whose moving end tracks the incsearch preview) and a `:` ex
    /// command (`:'<,'>…`, static selection) — so [`Self::rendered_visual_mode`]
    /// consults this to keep the selection visible. Distinct from
    /// [`Self::cmdline_return_mode`], which governs the *restore* mode on close (a
    /// `:` returns to Normal, unlike a search). Set on entry.
    cmdline_from_visual: Option<Mode>,
    /// The label shown ahead of the command line for a [`CmdlineKind::Prompt`]
    /// (`vim.ui.input`'s `opts.prompt`); empty for `:`/`/`/`?` (those use the
    /// single-char [`Editor::cmdline_prefix`]). Cleared when the prompt closes.
    cmdline_prompt: String,
    /// Resolved `vim.ui.input` prompts awaiting delivery to their Lua callback:
    /// `Some(text)` on `<CR>`, `None` on `<Esc>`/cancel. The server drains this
    /// each tick (like [`Editor::view_selects`]) and fires the registered
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
    /// Current `:normal` nesting depth — bounds a `:normal` whose keys run
    /// another `:normal` so a runaway chain can't overflow the stack (vim caps
    /// this too). Incremented around [`Editor::ex_normal`].
    normal_depth: usize,
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
    /// `nx_command` runs through [`Editor::command`] and never lands here.
    ex_history: Vec<String>,
    /// Per-namespace history for scripted [`CmdlineKind::Prompt`] prompts
    /// (`nx.ui.input{ history = "<namespace>" }`): each namespace is an independent
    /// recall ring, so a plugin's REPL history is separate from another's. Recorded
    /// on submit and recalled with `<Up>`/`<Down>` exactly like the ex / search
    /// histories. Session-only for now (not yet persisted to shada).
    prompt_history: std::collections::HashMap<String, Vec<String>>,
    /// The history namespace of the open prompt (the `history` key passed to the
    /// `nx.ui.input` request), or `None` when the prompt has no history. Set by
    /// [`Editor::open_prompt`]; selects which [`Editor::prompt_history`] ring
    /// `active_history` returns and `submit` records into.
    prompt_history_key: Option<String>,
    /// Position within the active history ([`Editor::search_history`],
    /// [`Editor::ex_history`], or the active [`Editor::prompt_history`] ring, per the
    /// open prompt's kind) while browsing it; `None` when editing a fresh line. Reset
    /// each time a command line opens.
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
    /// Whether the current [`message`](Self::message) reads as an *error* — drives
    /// the red `ErrorMsg` paint the client gives the cmdline message line. Set
    /// alongside `message` on every non-empty assignment: forced true by
    /// [`Editor::echo_err`] (the `:echoerr` path) and inferred from vim's `E###:`
    /// error-code prefix ([`is_error_line`]) by [`Editor::echo`]. Stale-but-unseen
    /// after a `message.clear()` (an empty message never renders); the next
    /// non-empty assignment always refreshes it.
    pub message_error: bool,
    /// History of every message shown, the backing store for `:messages`. Each
    /// entry is one line carrying its error flag (see [`LoggedMessage`]).
    pub messages: Vec<LoggedMessage>,
    /// Live `nx.view` surfaces, keyed by the Lua-allocated view handle id. The
    /// value records the backing [`BufferId`] (the read-only, plugin-owned content
    /// buffer carrying `view: Some(id)`) and how the view is currently mounted, so
    /// `set_lines` / `focus` / `unmount` can resolve the id back to its buffer and
    /// window region. Empty until the first `nx.view.create`. nxvim-native; not a
    /// neovim concept.
    pub(crate) views: HashMap<u64, ViewState>,
    /// `<CR>` selections made on a focused `nx.view` buffer: each is `(view id,
    /// 0-based cursor line)`. Drained by the server to fire the view's Lua
    /// `on_select(line, userdata)` callback. The view analogue of
    /// [`Editor::prompt_results`].
    pub view_selects: Vec<(u64, usize)>,
    /// `nx.view` ids whose window the user just closed (`:q` / `:close` / `<C-w>c` on a
    /// view buffer). Drained by the server to fire the view's Lua `on_close()` handler,
    /// which lets a plugin tear down a *group* of related views (e.g. nxvim-diff closing
    /// all three panes when one is `:q`'d). Recorded only on the user close path
    /// ([`Editor::close_window`]), never on a programmatic `unmount`/`destroy_view`, so a
    /// plugin's own teardown doesn't re-fire it.
    pub view_closes: Vec<u64>,
    /// Slots a session restore reserved for **persisted plugin views** (`nx.view.create{
    /// persist = }`) that no plugin has adopted yet — each a `(namespace, id)` + reserved
    /// placeholder window ([`PendingViewRestore`]). Filled by [`Editor::restore_session`]
    /// at boot (before plugins load), mirrored to Lua as `nx._view_pending`, and drained by
    /// the `nx.view.on_restore` dispatch once plugins are sourced; survivors are orphans
    /// whose slots collapse. Empty outside the boot-restore window.
    pub(crate) pending_view_restores: Vec<self::persist::PendingViewRestore>,
    /// The layer a restored session was quit from — the keyword captured in
    /// [`SessionState::focus_layer`] (`"main"` or a dock side), HELD until the user first
    /// acts. [`Editor::restore_session`] stashes it (it cannot focus a dock mid-restore: the
    /// dock may hold an unadopted placeholder, and the tab build must run on the main layer);
    /// [`Editor::finalize_session_focus`] re-pins it on every settle through startup, since a
    /// sidebar plugin's async (re)build can grab its dock several ticks in — well past the
    /// one `VimEnter` point. The first real key / mouse releases the hold
    /// ([`Editor::clear_session_focus_hold`]). `None` once released or never set.
    pub(crate) pending_session_focus: Option<String>,
    /// The floating selectable-list widget, when open (`nx.ui.select`; the shared
    /// picker / completion surface). Grabs input focus like the panel, but floats
    /// over the text. See [`menu`](crate::editor::MenuPlacement).
    menu: Option<menu::Menu>,
    /// Resolved menu outcomes: `Some(key)` when the user confirmed a row (the
    /// source key — a `select` choice index, or a picker item's wrapper key),
    /// `None` on cancel. Drained by the server to deliver the result to its
    /// callback — the menu analogue of [`Editor::prompt_results`].
    pub menu_results: Vec<Option<usize>>,
    /// The confirm gesture's open mode for the *next* drained picker result — set by
    /// the `confirm`/`confirm_tab`/`confirm_split`/`confirm_vsplit` actions (default
    /// `<CR>`/`<C-t>`/`<C-x>`/`<C-v>`) alongside the [`Editor::menu_results`] push,
    /// and taken by the server when it routes the result to `nx._picker_result`. Only
    /// the picker reads it (`nx.ui.select` ignores it). See [`menu::PickerOpenMode`].
    pub picker_confirm_mode: menu::PickerOpenMode,
    /// "Send the picker's current (filtered) results somewhere" outcomes: each entry
    /// is `(matched item keys in display order, the live query)`, pushed by the
    /// `send_to_list` picker action (which also closes the picker). Drained by the
    /// server, which hands them to Lua to build a list — the bulk-result sibling of the
    /// single-key [`Editor::menu_results`]. Backs the picker's quickfix-style sink; the
    /// query names the per-search list (`<picker>:<query>`).
    pub picker_sends: Vec<(Vec<usize>, String)>,
    /// A frozen window of the most-recently-closed **resumable** picker, captured by
    /// the `confirm`/`cancel`/`send_to_list` actions just before it closes (the
    /// live menu is gone by the time the server drains the outcome). Replayed verbatim
    /// by `nx.picker.resume()` (`<leader>fr`) — a live-grep order isn't stable across
    /// runs, so resume can't re-run the source. Bounded to [`menu::RESUME_WINDOW`]
    /// rows around the cursor. `None` until a resumable picker has closed. See
    /// [`Editor::snapshot_picker_for_resume`] / [`Editor::restore_picker_snapshot`].
    picker_snapshot: Option<menu::Menu>,
    /// The item **keys** of the latest resume snapshot's window, drained by the server
    /// alongside the picker outcome and handed to Lua (`nx._picker_result` /
    /// `nx._picker_send`) so it keeps only those item tables for `confirm` — bounding
    /// Lua's resume memory to the window too. Empty for a non-resumable close.
    pub picker_resume_keys: Vec<usize>,
    /// The list-less **content float** (`nx.ui.float`; the LSP hover / signature
    /// help surface), when open, or `None`. A transient overlay rendering plain
    /// content lines — no list, no selection, **never grabs input**: it is
    /// dismissed by the next key (see [`Editor::input`]). The sibling of [`menu`]
    /// on the shared float placement layer. See [`float`](crate::editor::float).
    content_float: Option<float::ContentFloat>,
    /// Reused scratch buffers backing the **doc floats** (the LSP hover /
    /// signature-help *windows*), keyed by surface name so re-opening a surface
    /// replaces its content in place. Like [`Editor::panel_buffers`] these are
    /// surfaces, not documents — [`Editor::is_doc_float_buffer`] keeps them out of
    /// `:ls` / buffer navigation. See [`float`](crate::editor::float).
    doc_float_buffers: Vec<(String, BufferId)>,
    /// The doc-float **windows** currently open, keyed by surface name: a real,
    /// non-focusable float window per surface (so it inherits mouse hit-testing and
    /// **wheel scroll** for free). Transient like [`content_float`] — the next key
    /// dismisses it in [`Editor::input`] — but a mouse wheel never reaches `input`,
    /// so it scrolls the popup instead of closing it.
    doc_float_wins: Vec<(String, WindowId)>,
    /// The signature of the currently-open **completion docs float** — its markdown +
    /// the popup box geometry it was placed against + `wrap`. `open_completion_docs_float`
    /// skips a redundant close+reopen when the signature is unchanged (a bare mouse wheel
    /// over the float, an idle repaint), so the float keeps its scroll offset instead of
    /// snapping back to the top every event; a keystroke that moves the popup or changes
    /// the selection shifts the signature and re-places it. `None` when the float is
    /// closed. See [`float`](crate::editor::float).
    completion_docs_sig: Option<float::CompletionDocsSig>,
    /// Picker query edits awaiting a (dynamic) source re-run: each `(generation,
    /// query)`. A *static* source never appends here — the local fuzzy matcher
    /// handles its query edits in core. Drained by the server, which stamps the
    /// generation onto the source run + its pushes so a stale response is dropped.
    pub picker_query_changes: Vec<(u64, String)>,
    /// Status-line clicks awaiting the server's `%@handler@…%X` resolution. The
    /// core hit-tests a status-line press to a window + column (it can't run the
    /// Lua handler), pushes a [`StatuslineClick`] here, and the server drains it
    /// after the gesture — recomputing that window's click regions and firing the
    /// handler whose span covers the column. See [`mouse`](crate::editor::mouse).
    pub statusline_clicks: Vec<StatuslineClick>,
    /// Mouse-button presses awaiting the server's keymap resolution. A left-press
    /// places the cursor (the `<LeftMouse>` default) and pushes a [`MouseClick`]
    /// here; the server drains it after the gesture and either fires the
    /// `<n-LeftMouse>` mapping bound in the current buffer or, when none is bound,
    /// calls [`Editor::mouse_apply_default_select`] for the default word/line
    /// escalation. The map-vs-default decision lives in the server because the
    /// keymap engine does (design D1) — the core only records the click. See
    /// [`mouse`](crate::editor::mouse).
    pub mouse_clicks: Vec<mouse::MouseClick>,
    /// The native completion engine's configuration (`nx.complete.setup`).
    /// Disabled until a config arrives, so an editor with no completion config is
    /// byte-for-byte unchanged. See [`complete`](crate::editor::complete).
    complete_config: complete::CompleteConfig,
    /// Completion triggers awaiting an **async** source run: each `(generation,
    /// ctx)`. Empty unless `nx.complete.setup` configured at least one async
    /// source (`has_async`); the native `buffer` source streams nothing here — it
    /// is matched synchronously in core. The completion analogue of
    /// [`Editor::picker_query_changes`]; the server drains it, stamps the
    /// generation onto the source run + its pushes, and drops a stale response.
    pub complete_query_changes: Vec<(u64, complete::CompleteCtx)>,
    /// Monotonic completion generation, bumped on every trigger (each prefix
    /// edit). The token stamped onto the open completion menu and onto any async
    /// source dispatch, so a push from a superseded prefix is dropped — the
    /// completion analogue of the picker's per-query generation.
    complete_gen: u64,
    /// A completion row whose accept is **delegated to its source** (`MenuItem`'s
    /// `source_accept`): the row's `key`, set by [`Editor::complete_accept`] when
    /// such a row is accepted and drained by the server, which applies the edit core
    /// can't (the `lsp` source's `textEdit` + `additionalTextEdits`). `None` when the
    /// last accept was a native `buffer` insert (already applied in core).
    pub complete_accept_request: Option<usize>,
    /// Paired with [`Editor::complete_accept_request`] for a **delegated** accept under
    /// [`AcceptBehavior::Replace`]: the absolute byte offset the server should extend
    /// the replaced range to (the end of the word the caret was inside), so the whole
    /// word is swapped rather than just the typed prefix. `None` ⇒ the server stops at
    /// the cursor (an `Insert` accept, or a caret already at the word's end). Taken by
    /// the server when it applies the delegated edit.
    pub complete_accept_extend_to: Option<usize>,
    /// The signature-help **auto-trigger** characters — the server's advertised
    /// `signatureHelpProvider.{trigger,retrigger}Characters`, pushed in by the host
    /// when an opted-in user has a server that advertises them attached. Non-empty
    /// **iff** the auto-trigger is both enabled and supported; empty leaves the manual
    /// `<C-k>` path untouched. See [`signature`](crate::editor::signature).
    signature_trigger_chars: Vec<char>,
    /// Whether a signature **session** is open — the window from a trigger char until
    /// you leave the call. While set, the signature doc float is *sticky* (kept across
    /// the next-key dismissal in [`Editor::input`]) instead of transient.
    signature_session: bool,
    /// One-shot: an insert keystroke asked for a (re)fired signature-help request. The
    /// host tick drains it into a `textDocument/signatureHelp` (core can't issue LSP),
    /// the completion analogue being [`Editor::complete_query_changes`].
    pub signature_auto_request: bool,
    /// The command-line completion engine's configuration (`nx.cmdline_complete`).
    /// Disabled until a config arrives, so an editor with no command-line completion
    /// is byte-for-byte unchanged. See [`cmdcomplete`](crate::editor::cmdcomplete).
    cmdcomplete: cmdcomplete::CmdlineCompleteConfig,
    /// A pending command-line completion request: core stamps the token being
    /// completed here on `<Tab>` (and on each edit while the menu is open), and the
    /// server resolves it against the bundled catalog source — fetching candidates
    /// in one round-trip and rebuilding the menu via [`Editor::open_cmdline_menu`].
    /// `None` when idle. See [`cmdcomplete`](crate::editor::cmdcomplete).
    pub cmdline_complete_request: Option<cmdcomplete::CmdlineCompleteReq>,
    /// Whether the open [`CmdlineKind::Prompt`] opted into autocomplete
    /// (`nx.ui.input{ complete = fn }`): gates the `<Tab>` wildmenu for the prompt.
    /// Set per-prompt by [`Editor::open_prompt`]; the gate is kind-checked, so a stale
    /// `true` while a non-prompt line is open is inert.
    prompt_complete_active: bool,
    /// Whether the open prompt's wildmenu shows the side **docs** pane
    /// (`nx.ui.input{ complete_docs = true }`): each candidate's `doc` renders in a
    /// panel beside the list, exactly like the `:`-completion docs pane. Set
    /// per-prompt by [`Editor::open_prompt`].
    prompt_complete_docs: bool,
    /// A pending **prompt** completion request — the `nx.ui.input` analogue of
    /// [`Editor::cmdline_complete_request`]. Core stamps the token being completed on
    /// `<Tab>` (and on each edit while the menu is open); the server resolves it by
    /// calling the prompt's per-call `complete` source (sync or async) and rebuilds
    /// the wildmenu via [`Editor::open_prompt_complete_menu`]. `None` when idle.
    pub prompt_complete_request: Option<cmdcomplete::CmdlineCompleteReq>,
    /// The command line as the user typed it **before** the wildmenu rewrote it to a
    /// highlighted candidate (`(line, cursor)`): navigating the wildmenu previews the
    /// selected command in the line (so `<CR>` runs what is shown), and `<Esc>`
    /// restores this snapshot. `None` until a selection first rewrites the line, and
    /// cleared once the menu closes or a real edit commits the preview. See
    /// [`cmdcomplete`](crate::editor::cmdcomplete).
    cmdline_complete_saved: Option<(String, usize)>,
    pub should_quit: bool,
    /// The **effective** global options every read sees (number column, search flags,
    /// …): [`Editor::global_base`] with the per-workspace [`Editor::workspace_options`]
    /// overlay applied on top. Recomputed by [`Editor::recompute_effective_options`]
    /// whenever either layer changes; never written field-by-field outside the setters.
    pub options: Options,
    /// The process-global option values — what `init.lua` / `:set` / `nx.o` write. The
    /// *base* layer beneath [`Editor::workspace_options`]; the workspace overlay takes
    /// precedence, so the effective [`Editor::options`] = this with the overlay applied.
    /// Equal to `options` when no workspace override is active.
    global_base: Options,
    /// The per-workspace option **overlay** (`nx.wso`): canonical global-option name → the
    /// workspace's overriding value, winning over [`Editor::global_base`]. Persisted in the
    /// workspace shada and re-applied at load. Empty outside a workspace / before any
    /// override.
    workspace_options: crate::options::WorkspaceOptions,
    /// The global quickfix **list stack**: errors parsed from command output /
    /// ingested text via `'errorformat'`, kept as vim's up-to-10-deep history that
    /// `:colder`/`:cnewer` walk. The per-window location lists live on each
    /// [`crate::editor::windows::Window`]. See [`quickfix`](crate::editor::quickfix).
    qf: QfStack,
    /// The display buffer for the quickfix window (`:copen`), created lazily on
    /// first open and kept thereafter so its window/cursor persist. The buffer is
    /// an ordinary scratch buffer whose id is remembered here; that id is what
    /// marks it read-only (`is_quickfix_buffer`) and re-rendered on list change. A
    /// window shows the quickfix list iff its buffer is this id.
    qf_bufnr: Option<BufferId>,
    /// The **named-list registry**: window-independent quickfix-flavored lists keyed
    /// by a stable [`NamedListId`]. Each [`NamedList`] owns its own list-stack and
    /// (lazily) its bottom-dock display buffer; storage here (not on a window) is what
    /// lets it survive every window close, unlike a per-window location list. Names
    /// are interned to ids through [`Editor::named_list_id`] so [`QfWhich`] stays
    /// `Copy`. Not persisted. See [`quickfix`](crate::editor::quickfix).
    named_lists: std::collections::HashMap<NamedListId, NamedList>,
    /// Name → id index for the named-list registry, plus the id allocator
    /// ([`Editor::next_named_id`]). Interning a new name allocates the next id and
    /// inserts an empty [`NamedList`] into [`Editor::named_lists`].
    named_by_name: std::collections::HashMap<String, NamedListId>,
    /// The next [`NamedListId`] to hand out (monotonic; ids are never reused).
    next_named_id: u32,
    /// The **named-panel registry**: each distinct panel name (`[Messages]`,
    /// `[Registers]`, `[Buffers]`, `[Marks]`, … and any `nx.panel.open{ name }`) → the one
    /// `nomodifiable` display buffer reused for it. Insertion-ordered for a stable
    /// `:lspanels` listing. Naming makes panels **unique**: re-running a command replaces
    /// its named buffer's content ([`Editor::open_named_panel`]); navigating back to one
    /// via `:lspanels` shows that buffer's last content. These buffers are **panel-only** —
    /// [`Editor::is_panel_buffer`] excludes them from `:ls` / buffer navigation, and
    /// [`Editor::switch_buffer`] reroutes any attempt to show one in a normal window into
    /// opening it as a panel, so a panel never appears as a regular main buffer.
    panel_buffers: Vec<(String, BufferId)>,
    /// The open **panel**: a transient, focus-locked bottom overlay over an ordinary
    /// `nomodifiable` buffer (the successor to the bespoke bottom panel). `Some` while a
    /// panel is up — `messages`/`registers`/`ls`/… and scripted `nx.panel.open` all mount
    /// through [`Editor::open_panel`]. Its presence pins focus to `window` (the
    /// [`focus_window`](Editor::focus_window) guard refuses to leave it — vim's `<C-w>`
    /// nav is inert) until an explicit close ([`Editor::close_panel`]), which restores the
    /// layout and refocuses `prev_window`. Unlike the old panel this carries no content or
    /// navigation state: the buffer *is* the content, motions navigate, and any activation
    /// key (`<CR>`, `q`) is an ordinary buffer-local map installed by a `FileType` autocmd.
    panel: Option<panel::PanelState>,
    /// The STACK of **grabbing** `nx.view` float windows (`v:mount{ float = { grab = true }
    /// }`), innermost last. Like [`Editor::panel`] each hard-locks focus — the
    /// [`focus_window`](Editor::focus_window) guard pins focus to the *topmost* one until it
    /// is unmounted — but they live on floats, not a bottom split, and they NEST: a modal
    /// opened over another modal pushes, and closing it pops focus back to the one below
    /// (each modal's prior-focus is held on its [`ViewMount::Float`](super::view::ViewMount)).
    /// Feeds the guard via [`Editor::focus_lock_window`]. Empty when no modal is up.
    view_float_lock: Vec<WindowId>,
    /// The window focused just before `:copen`, used as the default jump target so
    /// `<CR>` in the quickfix window lands in the code window the list was opened
    /// from (vim's behavior with an empty `'switchbuf'`). Re-validated on use.
    qf_prev_win: Option<WindowId>,
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
    /// The line an auto-indent (`o`/`O`/`<CR>`) just filled with indentation that
    /// the cursor still sits in, *untouched* — vim's `did_ai`. Any content key
    /// clears it (the next `handle_insert` takes it); if it survives to `<Esc>`
    /// the auto-indent is scrubbed so leaving an opened line without typing yields
    /// a truly-empty line, not trailing whitespace. Suppressed by the
    /// `indentemptylines` opt-in (same knob as the `=` blank-line rule).
    ai_open_line: Option<usize>,
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
    /// Where `<Esc>` from [`Mode::Select`] lands (set per-entry by
    /// [`Editor::select_range_in_window`]): `true` keeps the default and parks in
    /// Insert past it (the snippet-placeholder UX), `false` keeps it and drops to
    /// Normal (vim's `v_CTRL-G`). Only meaningful while in Select mode.
    select_escape_insert: bool,
    /// Whether the active [`Mode::Select`] is *linewise* (`gH` / a `<C-g>` toggle
    /// from Visual-Line) rather than charwise (`gh` / `nx.win.select_range`). Threads
    /// into the selection projection and the replace so a linewise Select highlights
    /// and replaces whole lines. Only meaningful while in Select mode.
    select_linewise: bool,
    /// Whether the editor is in a Helix *session* — the selection-first editing
    /// model is active (entered with `:helix`; the Phase-5 plugin drives it the
    /// configurable way). Persists across an Insert session opened by a Helix verb
    /// (`c`), so leaving Insert returns to [`Mode::HelixNormal`] rather than vim's
    /// Normal (see [`Editor::base_normal_mode`]). `false` in the default vim model.
    helix: bool,
    /// Whether Helix-session search defaults to **smart-case** (case-insensitive
    /// unless the pattern carries an uppercase char) — Helix's own default, kept
    /// self-contained from the global `'ignorecase'`/`'smartcase'` that vim-mode
    /// search reads (so entering Helix never mutates them). Consulted only while
    /// [`Editor::helix`] is set (see [`Editor::search_ignorecase`]); toggled by the
    /// `smart_case_on` / `smart_case_off` Helix actions (`nx.helix.smart_case`,
    /// `nx.helix.enable{ smart_case = … }`). Default `true`.
    helix_smart_case: bool,
    /// The count digits accumulated in a Helix mode (`3w`), awaiting a motion.
    /// Kept **out** of the vim [`PendingCommand`] on purpose — the two grammars
    /// disagree about what a motion does, so Helix input never threads through the
    /// operator-pending state machine (see [`crate::editor::helix`]). `None` when no
    /// count is pending.
    helix_count: Option<usize>,
    /// A Helix find-char motion (`f`/`t`/`F`/`T`) awaiting its target character —
    /// the next key completes it. `None` the rest of the time.
    helix_find: Option<FindKind>,
    /// Whether a Helix `r` (replace) is awaiting its replacement character — the
    /// next key overwrites every selected character with it (newlines preserved).
    /// Read raw like [`Self::helix_find`], so a target a `helix`-bucket keymap also
    /// binds is consumed here rather than firing that map.
    helix_replace: bool,
    /// Whether a Helix `z` (view) is awaiting its second key (`zz`/`zt`/`zb`) — a
    /// viewport reposition that leaves the selection put. Read raw like
    /// [`Self::helix_replace`]. `false` outside a `z` sequence.
    helix_view: bool,
    /// Whether a Helix `"` is awaiting its register name — the next key sets
    /// [`PendingCommand::register`] so the following verb (`y`/`d`/`c`/`p`/`P`/`R`)
    /// reads/writes that register. Read raw like [`Self::helix_replace`]. `false`
    /// outside a `"` sequence.
    helix_register: bool,
    /// The in-flight Helix match-mode (`m`) sub-state, awaiting its next key: `mm`
    /// (goto match), `mi`/`ma` (text objects), `ms`/`md`/`mr` (surround add / delete
    /// / replace). Multi-key, so — like [`Self::helix_find`] — it is read raw via
    /// [`Self::awaiting_command_continuation`]. `None` outside a match sequence.
    helix_match: Option<helix::HelixMatch>,
    /// The selection stashed while a Helix surround-replace (`mr{from}`) previews its
    /// target delimiters: after `{from}` is typed the delimiters light up (the live
    /// selection becomes them), and `{to}` restores this original selection once the
    /// swap applies. `None` unless an `mr` is mid-sequence (also restored on `<Esc>`).
    helix_surround_orig: Option<Selections>,
    /// The selection byte ranges `(lo, hi_exclusive)` captured when a Helix
    /// selection-regex prompt (`s`/`S`/`K`/`Alt-K`) opened, so the live preview can
    /// light up the pattern's matches *within* the selections as the pattern is
    /// typed (`search_highlights_in`). Empty outside such a prompt.
    helix_regex_ranges: Vec<(usize, usize)>,
    /// State for the in-flight left-button gesture: the multi-click counter that
    /// escalates char → word → line on same-cell presses within `'mousetime'`, and
    /// the anchor a drag extends from. Held across a press → drag → release and the
    /// gap to the next press (so a quick repeat at the same cell is a double-click).
    /// `None` before the first press / after a click outside any window. See
    /// [`crate::editor::mouse`].
    mouse_select: Option<mouse::MouseSelect>,

    /// Multi-click state for status-line `%@…%X` click regions, kept separate from
    /// [`Editor::mouse_select`] so a status-line click never seeds a text drag/
    /// selection: `(row, col, stamp_ms, count)` of the last status-line press, for
    /// counting a same-cell repeat within `'mousetime'` as a double-/triple-click.
    /// `None` until the first status-line press. See [`crate::editor::mouse`].
    statusline_click_seq: Option<(usize, usize, u64, u8)>,

    /// State for an in-flight separator / status-line drag (Phase 5): which window
    /// edge is grabbed and the press origin the drag resizes against. `None` unless
    /// a left-press landed on a split divider. See [`crate::editor::mouse`].
    mouse_resize: Option<mouse::ResizeDrag>,

    /// The global screen cell of the most recent processed mouse event (press, drag,
    /// release, or wheel), backing [`mouse_pos`](Self::mouse_pos) / `vim.fn.getmousepos`.
    /// `None` before any mouse event. See [`crate::editor::mouse`].
    last_mouse: Option<(usize, usize)>,

    /// Multi-click tracker for the **right / middle** buttons (the left button counts
    /// via the [`mouse_select`](Self::mouse_select) drag tracker): `(button, row, col,
    /// stamp_ms, count)` of the last such press, so a same-button same-cell repeat
    /// within `'mousetime'` escalates the count for `<2-RightMouse>` / `<3-MiddleMouse>`.
    mouse_button_seq: Option<(crate::input::MouseButton, usize, usize, u64, u8)>,

    /// Scroll-wheel gestures awaiting keymap resolution — the wheel counterpart of
    /// [`mouse_clicks`](Self::mouse_clicks): the server fires a bound `<ScrollWheelUp>`
    /// (etc.) map or runs the default scroll. See [`crate::editor::mouse`].
    pub mouse_wheels: Vec<mouse::WheelGesture>,

    /// Set by a scroll command or a cursor motion at the moment it fires:
    /// `(top, cursor.line)` *before* the move. Consumed at the end of `input` to
    /// build `pending_scroll` when the viewport ends up moving more than a line.
    scroll_from: Option<(usize, usize)>,
    /// The scroll gesture from the most recent input, projected into the next
    /// `View` and then cleared (so it animates exactly once).
    pending_scroll: Option<PendingScroll>,

    /// `nx.decor` viewport-changed signal (`editor/decor.rs`). Last-seen
    /// `(buffer, top, bot, changedtick)` per visible window — recomputed when input
    /// settles; a diff bumps `decor_gen[win]` and queues a [`decor::DecorViewport`] in
    /// `decor_dirty` for the server to dispatch to matching providers off-tick. The
    /// `changedtick` is in the key so an edit *within* the visible range (typing a
    /// bracket on screen — no scroll, same `top`/`bot`) still re-dispatches; a
    /// provider's snapshot would otherwise go stale until the next scroll.
    decor_viewports: HashMap<WindowId, (BufferId, usize, usize, u64)>,
    /// Per-window viewport generation, bumped on every visible-range change; a
    /// decor publish carries the generation it was computed for and is dropped at
    /// apply time unless it still matches (the viewport hasn't moved since).
    decor_gen: HashMap<WindowId, u64>,
    /// Windows whose viewport changed since the last drain (latest-wins per window),
    /// drained by the server in `run_pending`.
    decor_dirty: Vec<decor::DecorViewport>,
    /// Extmark namespaces that hold **ephemeral, derived** marks — viewport
    /// decoration-provider publishes (`nx.decor`), republished off-tick on every
    /// viewport/edit change. They are *not* document history, so undo/redo must not
    /// swap them out with the rest of the extmark store: [`Editor::restore_snapshot`]
    /// carries the live marks for these namespaces across a restore. Without that,
    /// undoing to a state captured before a provider first ran (notably the root undo
    /// node, snapshotted at buffer load) would wipe the live marks for one frame until
    /// the re-dispatch republishes them — a visible flash. Populated when a decor
    /// publish first targets a namespace (`mark_extmark_namespace_ephemeral`).
    ephemeral_extmark_ns: HashSet<u32>,

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

    /// The active snippet expansion, or `None` when no snippet is being filled.
    /// Drives `<Tab>`/`<S-Tab>` tabstop navigation and live mirror sync while the
    /// user types into an expanded snippet; ends on `<Esc>` to Normal or on
    /// reaching the final `$0` stop. See [`crate::editor::snippet`].
    snippet: Option<snippet::SnippetSession>,
    /// Configurable tabstop-jump keys (`nx.snippet.setup{ jump_next, jump_prev }`),
    /// defaulting to `<Tab>` / `<S-Tab>`. Consulted only while a [`snippet`] session
    /// is live, so they don't shadow soft-tab insertion otherwise.
    snippet_keys: snippet::SnippetKeys,

    /// Lua chunks queued by `:lua`, drained by the server's Lua runtime.
    pub lua_queue: Vec<String>,

    /// Set by `:={expr}` / `:lua= {expr}`: after the queued `vim.print` chunk has
    /// run (and its output landed in `:messages`), the server pops the `:messages`
    /// panel so the printed value is visible. Drained right after [`lua_queue`].
    pub open_messages_after_lua: bool,

    /// Command lines queued for the server to run after the tick, in order — see
    /// [`DeferredCmd`] for the two kinds. Keeps the core ignorant of the Lua command
    /// table while still routing typed `:Foo` and `vim.cmd.Foo()` through one place.
    pub deferred_commands: Vec<DeferredCmd>,

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
    /// `String` (not `&'static str`) because a `nx.bo.filetype` / `:set filetype`
    /// override can name any runtime language, not just one of the built-in
    /// extension table's.
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
    /// dark. Written by `nx.bo.ts_highlight` / `:set ts_highlight` (and the
    /// [`Editor::ts_start`] / [`Editor::ts_stop`] helpers).
    ts_enabled: HashMap<BufferId, bool>,
    /// User-registered tree-sitter text objects, keyed by the **full** `i`/`a` +
    /// object-key sequence (`"il"`, `"af"`, …) → the exact `textobjects.scm` capture
    /// to select (`"loop.inner"`, `"function.around"`; a leading `@` is stripped on
    /// lookup). Set from Lua via `nx.textobject.map`. Consulted *before* the built-in
    /// object alphabet in [`Editor::resolve_text_object`], so a user can bind new
    /// keys (`il` → `@loop.inner`) *and* override a built-in (`if` →
    /// `@function.inside`, e.g. to drive Helix's queries whose captures use
    /// `.inside`/`.around` rather than nxvim's `.inner`/`.outer`). Empty by default —
    /// the four built-ins (`f`/`a`/`c`/`t`) need no entry here.
    textobject_map: HashMap<String, String>,
    /// Per-buffer **`'commentstring'`** override — the comment template the
    /// `gc`/`gcc` operator wraps lines with (vim's `<left> %s <right>` form, e.g.
    /// `"// %s"` or `"/* %s */"`). Absent: the buffer falls back to its filetype's
    /// built-in template ([`comment::commentstring_for_language`]), so `gc` works
    /// out of the box on a known language. Stored beside [`Editor::ts_filetype`]
    /// (a per-buffer string, not a `Copy` [`BufferOptions`] slot) and written by
    /// `:set commentstring=…` / `nx.bo.commentstring`. Resolved with
    /// [`Editor::effective_commentstring`].
    commentstrings: HashMap<BufferId, String>,
    /// Per-buffer `'foldexpr'` — the expression `foldmethod=expr` folds by. Stored
    /// beside [`Editor::commentstrings`] (a per-buffer string, not a `Copy`
    /// [`BufferOptions`] slot) and written by `:set foldexpr=…` / `nx.bo.foldexpr`.
    /// nxvim evaluates only the canonical tree-sitter foldexpr natively (recognized
    /// by [`Editor::is_treesitter_foldexpr`]); a generic Lua foldexpr is Phase 5.
    /// Absent ⇒ empty (no expr folds).
    foldexprs: HashMap<BufferId, String>,
    /// Per-buffer `'foldmarker'` — the `(start, end)` delimiter pair `foldmethod=marker`
    /// folds by. Stored beside [`Editor::foldexprs`] (a per-buffer pair of strings,
    /// not a `Copy` [`BufferOptions`] slot) and written by `:set foldmarker=…` /
    /// `nx.bo.foldmarker`. Absent ⇒ vim's default `{{{`/`}}}` (see
    /// [`Editor::effective_foldmarker`]).
    foldmarkers: HashMap<BufferId, (String, String)>,
    /// Server-pushed fold data for the *externally-computed* sources — a generic
    /// Lua `'foldexpr'` and LSP `foldingRange`. nxvim-core can't evaluate Lua or
    /// talk to a language server, so the server computes these out-of-band and
    /// pushes the raw result here (tagged with the `changedtick` it was computed
    /// for); [`Editor::refresh_folds`] reads it for the
    /// [`FoldSource::GenericExpr`](crate::editor::fold::FoldSource)/`Lsp` sources,
    /// applying the buffer's `'foldnestmax'`/`'foldminlines'` itself. Keyed by
    /// buffer. See [`Editor::set_foldexpr_values`] / [`Editor::set_lsp_folds`].
    pub(crate) external_folds: HashMap<BufferId, fold::ExternalFolds>,
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
    /// The home directory a leading `~` in a file argument expands against
    /// ([`Editor::expand_file_arg`]). `None` (the default) — the local case — reads
    /// `$HOME` from this process's env. In a remote session the core runs on the
    /// client but the file read lands on the **daemon**, so `~` must mean the daemon's
    /// home; the edit-host seeds the daemon's `$HOME` here at connect (over the same
    /// `config_bundle` handshake that carries the daemon's cwd) via
    /// [`Editor::set_remote_home`].
    remote_home: Option<std::path::PathBuf>,
    /// Whether this session will CAPTURE the window/tab layout on exit — i.e. a
    /// layout-capturing workspace session (`workspace_session && session_save_layout`,
    /// both server-side). The server mirrors it in via [`Editor::set_session_captures_layout`]
    /// each input batch. When set, `:qa` need not block (`E37`) on a *modified unnamed*
    /// buffer shown in the layout, since `export_session` persists its contents with
    /// `'workspacepersistunnamed'` — see [`Editor::quit_safe_unnamed`]. `false` by default
    /// (a non-workspace session loses an abandoned `[No Name]`, so it must still warn).
    session_captures_layout: bool,
    /// Whether a `BufReadCmd` autocmd handler is registered — the server mirrors this
    /// from its `au_active_events` cache via [`Editor::set_bufreadcmd_active`]. When
    /// set, a file open is **deferred** (enqueued like an off-tick read) instead of
    /// read inline, so the server can fire `BufReadCmd` and let a Lua handler claim the
    /// read before the default load runs (vim's "replace the read" hook — netrw rides
    /// it). Off by default, so the common no-handler config reads files inline exactly
    /// as before (zero behavior change). See [`Editor::should_defer_open`].
    bufreadcmd_active: bool,
    /// Writes deferred this tick under off-tick mode, drained by the server with
    /// [`Editor::take_pending_saves`] (the save analogue of [`Editor::prompt_results`]
    /// / [`Editor::view_selects`]). Always empty when off-tick mode is off.
    pending_saves: Vec<PendingSave>,
    /// Monotonic id for the next [`PendingSave`], so the server can correlate acks and
    /// keep a buffer's overlapping writes ordered.
    next_save_seq: u64,
    /// Buffer opens deferred this tick under off-tick mode (`:edit` over the daemon
    /// wire), drained by the server with [`Editor::take_pending_opens`]. Each names an
    /// already-created (empty) buffer the server fills once the fetch lands. Always
    /// empty when off-tick mode is off.
    pending_opens: Vec<PendingOpen>,
    /// The encoding a `:e ++enc=<encoding>` forces for the *next* read, overriding
    /// `'fileencodings'` detection for that one open (vim's `++enc` read option). A
    /// transient the [`ex_edit`](Editor::ex_edit) read path sets and clears within its
    /// own dispatch: the synchronous read ([`read_buffer`](Editor::read_buffer))
    /// consults it, and a *deferred* local open ([`enqueue_open`](Editor::enqueue_open))
    /// copies it onto its [`PendingOpen`] so the later
    /// [`load_pending_open`](Editor::load_pending_open) can restore it. `None` for every
    /// ordinary read (autoreload, workspace edits, initial open).
    forced_read_encoding: Option<String>,
    /// A jump target `(buffer, line, byte-col)` waiting for a **deferred** open to land
    /// — a located navigation (LSP go-to, a picker `<C-t>`/`<C-x>`, `:e +N`) onto a
    /// buffer whose content hasn't been read yet (every local open now defers behind the
    /// explorer's `BufReadCmd` handler; an off-tick open always does). [`land_cursor`]
    /// records it instead of clamping the cursor onto the still-empty buffer; the read
    /// landing ([`load_str_into`] / [`load_pending_open`]) applies it once the lines are
    /// there, so the cursor lands on the located line rather than snapping to the top.
    pending_open_cursor: Option<(BufferId, usize, usize)>,
    /// Buffers whose content was read from a file *in place* this tick — a local
    /// (synchronous) `:edit` that reused the throwaway `[No Name]` or re-read the
    /// current file (`:e` / `:e!`), keeping the same bufnr. Drained by the server
    /// with [`Editor::take_loaded_in_place`], which clears them from its `announced`
    /// / `fired_filetype` sets so the now-(re)read buffer fires `BufReadPost`
    /// (`BufNewFile`) and `FileType` again — neovim fires those on *every* read,
    /// regardless of whether the buffer id was seen before. The off-tick read path
    /// clears `announced` itself when the fetched bytes land, so it does not record
    /// here; only the local in-place read does.
    loaded_in_place: Vec<BufferId>,
    /// A `:wqa` / `:xa` quit deferred until every write its `:wall` enqueued has acked
    /// (off-tick mode), drained by the server with [`Editor::take_pending_quit_all`]. The
    /// single-buffer `:wq` rides [`PendingSave::then_quit`]; the batch quit needs the
    /// whole set, so core records it here and the server gates the `:qa` on all of them.
    /// `None` unless a `:wqa` with at least one modified file-backed buffer just ran.
    pending_quit_all: Option<PendingQuitAll>,
    /// Writes the editor **intends** to make this tick but has not committed — the
    /// pre-write intents `:w` / `:wall` record instead of writing inline, drained by the
    /// server with [`Editor::take_pending_pre_writes`]. The server fires `BufWritePre`,
    /// waits for its handlers to settle, then calls [`Editor::commit_pre_write`] on each,
    /// so a handler's buffer mutation (format/trim-on-save) is what gets serialized —
    /// vim's pre-write contract, which the pure core can't drive itself (it can't
    /// re-enter Lua). See [`PreWrite`].
    pending_pre_writes: Vec<PreWrite>,
    /// Completed writes this tick (a committed `:w` / `:wall`, or a finalized off-tick
    /// save), drained by the server with [`Editor::take_write_events`]. Each carries
    /// whether `BufWritePre` still needs firing (a synchronous commit already fired it
    /// from the pre-write drain; an off-tick ack has not). Recording the *completed*
    /// write here (rather than firing inline) keeps `nxvim-core` free of event types,
    /// exactly as the buffer-lifecycle diff does for `BufEnter`/`FileType`.
    write_events: Vec<WriteEvent>,
    /// File-backed buffers awaiting a `:checktime` reconcile this tick, drained by the
    /// server with [`Editor::take_pending_checktime`]. The reconcile fires the
    /// `FileChangedShell` autocmd (a Lua round-trip the pure core can't drive itself)
    /// and honors `v:fcs_choice`, so the *decision* is deferred to the server even
    /// though detection / reload live in core. Both `:checktime` and the per-buffer
    /// file watch ([`Editor::checktime_buffer`]) enqueue here.
    pending_checktime: Vec<BufferId>,
    /// Set by `<C-w>d` / `<C-w><C-d>` (neovim's built-in "show diagnostics under
    /// the cursor" default), drained by the server with
    /// [`Editor::take_diagnostic_float`]. The float reads the LSP/client diagnostic
    /// store that lives behind the server seam, so core only records the request;
    /// the server opens the float in `run_pending`. (The `]d`/`[d` cursor moves go
    /// the other way — Lua keymaps → `LspOp` — because they only move the cursor,
    /// which core *can* do.)
    pending_diagnostic_float: bool,

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
    /// Whether the current terminal's child has enabled **application cursor-key mode**
    /// (DECCKM, `\E[?1h` — emitted by a full-screen app's `smkx`). When set, the arrow /
    /// Home / End keys must be sent in the `\EO_` form (`\EOA` … `\EOH`/`\EOF`) the app's
    /// terminfo expects, not the default `\E[_` cursor form — otherwise e.g. `less` doesn't
    /// recognize Home/End and treats the trailing letter as a command (`H`→help, `F`→tail).
    /// Mirrored from the vt100 emulator (`screen().application_cursor()`) by the server each
    /// projection via [`Editor::terminal_update`]; reset when a new terminal opens.
    terminal_app_cursor: bool,
}

impl Editor {
    pub fn new() -> Self {
        Editor::with_buffer(Buffer::empty())
    }

    /// The `mode()` short code for `nvim_get_mode`, accounting for the `i_CTRL-O`
    /// one-shot: while an insert-normal command is pending, vim reports `niI` / `niR`
    /// (Normal for one command, then resuming Insert / Replace), not a plain `n`. The
    /// keymap engine still selects the Normal trie off the raw [`Mode`] enum, so a
    /// `<C-o>`-launched command uses Normal-mode maps.
    pub fn mode_code(&self) -> &'static str {
        match self.insert_normal {
            Some(Mode::Replace) => "niR",
            Some(_) => "niI",
            // Linewise Select (`gH`) reports vim's `S` (charwise is `s`); the keymap
            // engine still selects the `'s'` trie off the raw [`Mode`] enum for both.
            None if self.mode == Mode::Select && self.select_linewise => "S",
            None => self.mode.short_code(),
        }
    }

    /// The uppercase status-line mode label, accounting for the `i_CTRL-O` one-shot:
    /// vim shows `-- (insert) --` / `-- (replace) --` while a single Normal command
    /// runs from Insert. The enum is [`Mode::Normal`] then, so [`Mode::label`] alone
    /// would read `NORMAL` and hide the fact that Insert resumes next.
    pub fn mode_label(&self) -> &'static str {
        match self.insert_normal {
            Some(Mode::Replace) => "(REPLACE)",
            Some(_) => "(INSERT)",
            None if self.mode == Mode::Select && self.select_linewise => "S-LINE",
            None => self.mode.label(),
        }
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

    /// Tell the editor whether the session captures layout on exit (so `:qa` can skip its
    /// `E37` guard for a modified unnamed buffer the session persists). The server keeps
    /// this in sync with `workspace_session && session_save_layout` each input batch.
    pub fn set_session_captures_layout(&mut self, on: bool) {
        self.session_captures_layout = on;
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
    /// Directory detection goes through `std::path::Path::is_dir` (a *local* startup
    /// arg; a remote/daemon startup directory is detected on the server's off-tick
    /// fetch instead). A startup directory opens the file explorer — a pure-Lua plugin
    /// (`prelude/explorer.lua`) — so it can't be filled at construction (the Lua VM /
    /// `init.lua` aren't up yet): the buffer is left empty and named for the directory,
    /// and the open is **enqueued** so the server fires `BufReadCmd` after `init.lua`
    /// sources and the explorer claims it (the same deferral a runtime `:e dir` uses).
    pub fn open_or_named_with(path: impl Into<PathBuf>, fs: Rc<dyn HostFs>) -> Self {
        let path = path.into();
        if path.is_dir() {
            let mut editor = Editor::with_buffer(Buffer::named(path.clone()));
            editor.host_fs = fs;
            let buf = editor.cur_buffer();
            editor.enqueue_open(buf, path);
            return editor;
        }
        // A file (or new file): read it now through `fs`. An unreadable file fails loud.
        let mut editor =
            match Buffer::from_file(&path, &*fs, crate::encoding::DEFAULT_FILEENCODINGS) {
                Ok(buffer) => Editor::with_buffer(buffer),
                Err(e) => {
                    let mut editor = Editor::with_buffer(Buffer::named(path.clone()));
                    editor.echo(format!("E484: Can't open file {}: {e}", path.display()));
                    editor
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
            alternate_name: None,
            global_marks: HashMap::new(),
            pending_global_marks: HashMap::new(),
            pending_file_marks: HashMap::new(),
            numbered_marks: HashMap::new(),
            pending_changelists: HashMap::new(),
            pending_folds: HashMap::new(),
            pending_jumplist: Vec::new(),
            mode: Mode::Normal,
            insert_normal: None,
            cursor: Cursor::default(),
            top: 0,
            leftcol: 0,
            cmdline: String::new(),
            cmdline_col: 0,
            cmdline_kind: CmdlineKind::Ex,
            cmdline_return_mode: Mode::Normal,
            cmdline_from_visual: None,
            cmdline_prompt: String::new(),
            prompt_results: Vec::new(),
            confirm_accelerators: Vec::new(),
            confirm_default: 0,
            last_search: None,
            search_re_cache: RefCell::new(None),
            last_substitute: None,
            subst_confirm: None,
            in_global: false,
            normal_depth: 0,
            search_operator: None,
            pending_search_count: 1,
            search_history: Vec::new(),
            ex_history: Vec::new(),
            prompt_history: std::collections::HashMap::new(),
            prompt_history_key: None,
            hist_idx: None,
            search_active: false,
            search_origin: Cursor::default(),
            message: String::new(),
            message_error: false,
            messages: Vec::new(),
            views: HashMap::new(),
            view_selects: Vec::new(),
            view_closes: Vec::new(),
            pending_view_restores: Vec::new(),
            pending_session_focus: None,
            menu: None,
            menu_results: Vec::new(),
            picker_confirm_mode: menu::PickerOpenMode::default(),
            picker_sends: Vec::new(),
            picker_snapshot: None,
            picker_resume_keys: Vec::new(),
            content_float: None,
            picker_query_changes: Vec::new(),
            statusline_clicks: Vec::new(),
            mouse_clicks: Vec::new(),
            complete_config: complete::CompleteConfig::default(),
            complete_query_changes: Vec::new(),
            signature_trigger_chars: Vec::new(),
            signature_session: false,
            signature_auto_request: false,
            complete_gen: 0,
            complete_accept_request: None,
            complete_accept_extend_to: None,
            cmdcomplete: cmdcomplete::CmdlineCompleteConfig::default(),
            cmdline_complete_saved: None,
            cmdline_complete_request: None,
            prompt_complete_active: false,
            prompt_complete_docs: false,
            prompt_complete_request: None,
            should_quit: false,
            options: Options::default(),
            // The base + overlay start equal to the effective options (no overrides yet);
            // a workspace seed or an `nx.wso` write later diverges them via recompute.
            global_base: Options::default(),
            workspace_options: crate::options::WorkspaceOptions::new(),
            qf: QfStack::default(),
            qf_bufnr: None,
            named_lists: std::collections::HashMap::new(),
            named_by_name: std::collections::HashMap::new(),
            next_named_id: 1,
            panel_buffers: Vec::new(),
            doc_float_buffers: Vec::new(),
            doc_float_wins: Vec::new(),
            completion_docs_sig: None,
            panel: None,
            view_float_lock: Vec::new(),
            qf_prev_win: None,
            highlights: Highlights::new(),
            width: 80,
            height: 24,
            desired_col: 0,
            desired_eol: false,
            preserve_desired: false,
            eol_request: false,
            registers: Registers::default(),
            pending: PendingCommand::default(),
            last_find: None,
            redo_recording: Vec::new(),
            last_change: Vec::new(),
            replaying_change: false,
            change_start_tick: 0,
            change_not_repeatable: false,
            insert_text: String::new(),
            pending_visual: None,
            snapshot_taken: false,
            ai_open_line: None,
            now_mono: 0,
            now_ms: 0,
            soft_tab: None,
            awaiting_register: false,
            visual_anchor: Cursor::default(),
            select_escape_insert: false,
            select_linewise: false,
            helix: false,
            helix_smart_case: true,
            helix_count: None,
            helix_find: None,
            helix_replace: false,
            helix_view: false,
            helix_register: false,
            helix_match: None,
            helix_surround_orig: None,
            helix_regex_ranges: Vec::new(),
            mouse_select: None,
            statusline_click_seq: None,
            mouse_resize: None,
            last_mouse: None,
            mouse_button_seq: None,
            mouse_wheels: Vec::new(),
            scroll_from: None,
            pending_scroll: None,
            decor_viewports: HashMap::new(),
            decor_gen: HashMap::new(),
            decor_dirty: Vec::new(),
            ephemeral_extmark_ns: HashSet::new(),
            placement_undo: Vec::new(),
            placement_redo: Vec::new(),
            cursor_registers: Vec::new(),
            cursor_register_collect: None,
            snippet: None,
            snippet_keys: snippet::SnippetKeys::default(),
            lua_queue: Vec::new(),
            deferred_commands: Vec::new(),
            open_messages_after_lua: false,
            pending_sleep: None,
            syntax: None,
            syntax_opened: HashMap::new(),
            syntax_failed: HashSet::new(),
            ts_filetype: HashMap::new(),
            commentstrings: HashMap::new(),
            foldexprs: HashMap::new(),
            foldmarkers: HashMap::new(),
            external_folds: HashMap::new(),
            ts_enabled: HashMap::new(),
            textobject_map: HashMap::new(),
            clipboard: None,
            host_fs: Rc::new(StdHostFs),
            host_fs_offtick: false,
            remote_home: None,
            session_captures_layout: false,
            bufreadcmd_active: false,
            pending_saves: Vec::new(),
            pending_pre_writes: Vec::new(),
            next_save_seq: 0,
            pending_opens: Vec::new(),
            forced_read_encoding: None,
            pending_open_cursor: None,
            loaded_in_place: Vec::new(),
            pending_quit_all: None,
            write_events: Vec::new(),
            pending_checktime: Vec::new(),
            pending_diagnostic_float: false,
            pending_shada: Vec::new(),
            pending_terminal: Vec::new(),
            terminal_pending_backslash: false,
            terminal_esc_count: 0,
            terminal_cursor: (0, 0),
            terminal_last_esc_ms: 0,
            terminal_awaiting_register: false,
            terminal_app_cursor: false,
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
        // The buffer now lives in the focused layer's window — record that as its
        // home so `:ls` and the close-fallback stay scoped to this layer.
        if let Some(ob) = self.buffers.map.get_mut(&id) {
            ob.layer = self.focused_layer;
        }
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
        // A *transient* content float (hover / signature help / a plain
        // `nx.ui.float`) never grabs input: the next key dismisses it, then is
        // handled normally below (vim closes a hover float on the next motion). It
        // opens off-tick on an LSP reply or synchronously from a mapping *after*
        // this clear, so the float that just opened survives until the following
        // key. A *persistent* float (`nx.ui.float{ persist = true }`, e.g. a
        // which-key popup observing keys via `nx.on_key`) is left alone — it lives
        // until its handle closes it or a replacement, not until the next key.
        self.dismiss_transient_content_float();
        // A doc float (the hover / signature-help *window*) is dismissed the same
        // way on the next key. Unlike the content float it is a real float window,
        // so a mouse wheel — which never flows through `input` — scrolls it instead
        // of closing it (the whole point of backing it with a window). The exception
        // is an active signature *session*: its float is kept across the keystrokes
        // that fill the call (see [`signature`](crate::editor::signature)).
        let in_signature_session = self.signature_session;
        self.close_transient_doc_floats();

        // A focused menu (`nx.ui.select` / the picker) grabs every key the same
        // way — navigation + confirm / cancel — floating over the text. A
        // *completion* menu is the exception: it floats over the text but the
        // buffer is the query, so typing must flow on to `handle_insert` (which
        // intercepts only the engine's control keys); `menu_grabs_input()` is
        // false for it.
        //
        // Both grabbing menus route their nameable keys through their own keymap
        // bucket (the matcher fires them as `apply_picker_action` / `apply_select_
        // action` ahead of this), so only an *unmapped* key reaches here. A picker
        // handles it as query text; a promptless `select` has no query, so an
        // unmapped key is inert.
        if self.menu_grabs_input() {
            if self.key_context() == KeyContext::Picker {
                self.handle_picker_text(key);
            }
            return;
        }

        // The explorer (directory listing), `nx.view` surfaces, and the quickfix /
        // loclist display are all **ordinary `nomodifiable` buffers in a window**
        // (vim's model): every normal-mode key — motions, search, `<C-w>…`, `:` —
        // flows through unchanged here, and edits are refused with `E21` at the
        // `modifiable()` chokepoints. Their one or two special keys (`<CR>` to open /
        // confirm / jump, `-` to go up) are ordinary **buffer-local default keymaps**
        // installed by a `FileType` autocmd (the `nxdir` / `qf` / `nxview` ftplugin
        // model), overridable the standard way — not special-cased in this loop. See
        // docs/plans/2026-06-16-unify-special-buffer-kinds.md.

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
            && self.buffer().is_terminal()
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
            // Select mode's printable-replaces keys route through their own handler,
            // not the Normal/Visual command grammar (see [`Editor::handle_select`]).
            Mode::Select => self.handle_select(key),
            Mode::HelixNormal | Mode::HelixSelect => self.handle_helix(key),
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

        // `i_CTRL-O`: resume the interrupted insert session once the one-shot Normal
        // command has settled. `!was_insert` skips the `<C-o>` keystroke itself
        // (which armed the flag *from* Insert); a command still unfolding across
        // keystrokes (operator-pending, visual, cmdline) stays armed until it either
        // returns to a clean Normal boundary — resume the stored Insert/Replace — or
        // enters Insert on its own (`o`/`a`/`cc`/…), which simply consumes the flag.
        // Runs before the dot-repeat `done` check below so the completing keystroke
        // isn't mistaken for the end of the change: the insert session continues.
        if self.insert_normal.is_some() && !was_insert {
            if self.mode.is_insert() {
                self.insert_normal = None;
            } else if self.mode == Mode::Normal && self.pending.is_clean() {
                self.mode = self.insert_normal.take().expect("armed above");
                // `<C-o>$` resumes Insert *after* the last char, ready to append: the
                // `$` motion itself lands *on* the last char (Normal's EOL), but the
                // pending eol-request means the user asked for the line end, so honour
                // it in Insert terms (one past). Now that `mode` is Insert again,
                // `clamp_cursor` permits the append column.
                if self.eol_request {
                    self.cursor.col = self.line_len();
                }
                self.clamp_cursor();
            }
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
        // Recompute computed (`indent`/…) folds if this keystroke changed the buffer
        // or a fold input. Cache-guarded, so an unchanged buffer pays nothing; a
        // no-op for `foldmethod=manual`.
        self.refresh_folds();
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

        // Detect any visible window whose viewport changed this keystroke (scroll,
        // motion off-screen, buffer/window switch, edit reflow) and queue it for the
        // server to dispatch to `nx.decor` providers off-tick (`editor/decor.rs`).
        self.recompute_decor_dirty();

        // Leaving insert mode (the `<Esc>` that just processed) ends an open signature
        // session and closes its sticky float — you are no longer filling the call.
        if in_signature_session && !self.mode.is_insert() {
            self.end_signature_session();
        }
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
        // straight to the destination instead of sliding. `'scrollanim'` is resolved
        // per-window — the focused window's local override (the side-by-side diff sets
        // it off on its panes), falling back to the global default — since only the
        // focused window's viewport ever animates.
        let dur_cap = self.options.scrollanimduration as u64;
        let scrollanim = self
            .windows
            .cur()
            .options
            .scrollanim
            .unwrap_or(self.options.scrollanim);
        if !scrollanim || dur_cap == 0 {
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

    /// Run an ex-command directly (the `nx_command` API entry point).
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
        self.record_message(&msg, false);
        // Mirror the panel's inference: a message wearing vim's `E###:` error code
        // lights the cmdline red without each of the 100-plus call sites opting in.
        self.message_error = is_error_line(&msg);
        self.message = msg;
    }

    /// Like [`Editor::echo`], but force every recorded line to the *error* level
    /// (the red `ErrorMsg` highlight in `:messages`). This is the `:echoerr`
    /// path — its text needn't carry a vim `E###:` code to count as an error.
    pub fn echo_err(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        self.record_message(&msg, true);
        self.message_error = true;
        self.message = msg;
    }

    /// Append `text` to the `:messages` history *only* — leaving the message
    /// line untouched — splitting it into one [`LoggedMessage`] per non-empty
    /// line. `force_error` flags every line as an error; otherwise each line is
    /// classified by vim's `E###:` error-code convention ([`is_error_line`]), so
    /// the many command errors that flow through [`Editor::echo`] light up red
    /// without threading a flag through 100-plus call sites.
    pub fn record_message(&mut self, text: impl AsRef<str>, force_error: bool) {
        for line in text.as_ref().split('\n').filter(|l| !l.is_empty()) {
            self.messages.push(LoggedMessage {
                text: line.to_string(),
                error: force_error || is_error_line(line),
            });
        }
        // Bound the history so a long-running session can't grow it forever.
        cap_ring(&mut self.messages, MAX_MESSAGES);
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

    /// True when the next key **belongs to a multi-key command already in progress**
    /// — so it is read raw, **not** through the mapping layer (the server routes it
    /// straight to [`Self::input`] instead of the keymap matcher). This is vim's rule,
    /// verified against neovim: the continuation of `dh` (operator + motion), `gj`
    /// (`g`-prefix), `zt` (`z`-prefix), `<C-w>h` (window), the literal arg of
    /// `r{c}`/`f{c}`/`"{reg}`/`m{mark}`/`` `{mark} ``/`i{obj}` — none of these go
    /// through user maps. Without this a bare map on the continuation key breaks the
    /// built-in: nxvim-tree binds `h`/`l` for fold navigation, so `<C-w>h` /
    /// `<C-w><C-w>l` / `dh` would fire the tree's map instead of moving; and a `gd`/`gr`
    /// LSP map would make `rg`/`fg` hang waiting to disambiguate the literal `g`.
    ///
    /// **Read straight off grammar state — no per-command list.** Exactly two sources,
    /// and together they are complete by construction:
    /// - an armed operator awaiting its motion (`d`/`c`/`y`/`=`; stage stays `Start`);
    /// - **any** non-`Start` [`Stage`] — every variant is the editor mid-parse of a
    ///   multi-key command, whether its next key is a grammar char (`g`/`z`/`<C-w>`
    ///   prefixes) or a literal arg (find/replace/register/mark/text-object). A new
    ///   prefix command is a new `Stage`, so it is covered automatically; nothing can
    ///   silently fall out of a hand-kept enumeration.
    ///
    /// A bare count or a selected register (stage `Start`, no operator) is deliberately
    /// **not** here: there the next key starts a *fresh* command that a mapping may
    /// claim (`3<leader>x` reads `v:count`; `"a` then a mapped action reads
    /// `v:register`), matching vim.
    ///
    /// The caller's `pending_empty()` gate is what keeps this from clashing with map
    /// disambiguation: when a map *prefix* collides with a built-in prefix (`gd`/`gr`
    /// over `g`, a user `<C-w>x`), the matcher **withholds** the prefix, so it never
    /// reaches the grammar and the stage stays `Start` — the matcher and its
    /// [`command_status`](crate::command_status) oracle own that case. Only once a
    /// prefix has reached the grammar *raw* (no map collides with it) does its
    /// continuation read raw here.
    ///
    /// The `<C-w><C-w>` dock chord in non-Normal modes ([`DockChord`]) is intentionally
    /// *not* special-cased here: its cross still flows through the matcher and the chord
    /// intercept in [`Self::input`] consumes it (the common case — no map on the cross
    /// key — already works). If a conflicting map ever needs the same raw read there,
    /// the fix is to route that chord through the grammar's `WindowLayerPending`, not to
    /// re-introduce a bespoke clause.
    pub fn awaiting_command_continuation(&self) -> bool {
        self.pending.operator.is_some()
            || self.pending.stage != Stage::Start
            // A pending Helix `f`/`t`/`F`/`T` awaits its target character, read raw
            // exactly like vim's `f{char}` — otherwise a target that a `helix`-bucket
            // keymap also binds (`a`/`i`/`g`/…) would fire that verb instead of being
            // consumed as the find target. The `pending_empty()` gate at the caller
            // keeps this from clashing with map disambiguation.
            || self.helix_find.is_some()
            // A pending Helix `r` awaits its replacement character, read raw for the
            // same reason (`a`/`i`/`g`/… may be mapped in the `helix` bucket).
            || self.helix_replace
            // A pending Helix `z` (view) awaits its placement key raw (`t`/`z`/`b`).
            || self.helix_view
            // A pending Helix `"` awaits its register-name key raw.
            || self.helix_register
            // Match mode (`m…`) awaits its next key raw for the same reason.
            || self.helix_match.is_some()
    }

    /// The fixed end of the visual selection (the other end is [`Self::cursor`]).
    /// Only meaningful while [`Self::mode`] is a visual mode.
    pub(crate) fn visual_anchor(&self) -> Cursor {
        self.visual_anchor
    }

    /// The live selection's extent as `(start_row, start_col, end_row, end_col)` —
    /// 0-based rows, **byte** columns, end-**exclusive** (the `nx.win.select_range`
    /// convention) — or `None` when nothing is selected. Charwise ([`Mode::Visual`] /
    /// [`Mode::Select`]) spans anchor..cursor with the character *under* the cursor
    /// included; linewise ([`Mode::VisualLine`]) spans whole lines, so the end is
    /// column 0 of the row after the last selected one.
    ///
    /// The extent, not the text: what a range-scoped request built from the selection
    /// needs (LSP `textDocument/codeAction` over a selection — nxvim's answer to
    /// neovim's `range_from_selection`), where the operators want the byte range
    /// ([`Self::visual_range_lw`]) instead.
    pub fn selection_extent(&self) -> Option<(usize, usize, usize, usize)> {
        let linewise = match self.mode {
            Mode::Visual => false,
            Mode::VisualLine => true,
            Mode::Select => self.select_linewise,
            _ => return None,
        };
        let (lo, hi, _) = self.visual_range_lw(linewise);
        let buffer = self.buffer();
        let s_row = buffer.byte_to_line(lo);
        let e_row = buffer.byte_to_line(hi);
        let e_col = hi - buffer.line_start(e_row);
        // A charwise selection ends one *byte* past the cursor (the char under it is
        // selected), which lands mid-cluster on anything multi-byte — and a position
        // inside a character is not a legal LSP one (and its byte→UTF-16/32 column
        // conversion would be wrong or panic). Snap forward to the cluster boundary,
        // taking the whole grapheme the selection paints.
        let line = buffer.line(e_row);
        let e_col = if crate::unicode::floor_grapheme(&line, e_col) == e_col {
            e_col
        } else {
            crate::unicode::next_grapheme(&line, e_col)
        };
        Some((s_row, lo - buffer.line_start(s_row), e_row, e_col))
    }

    /// Consume the live selection: stamp the `` `< `` / `` `> `` marks and drop to
    /// Normal on the selection head. What a command that *acts on* the selection and
    /// then edits the buffer does — vim leaves Visual on `:`, and leaving before an
    /// edit lands keeps a stale anchor from surviving into the changed text. A no-op
    /// outside Visual / Select.
    pub fn leave_selection(&mut self) {
        if !self.mode.is_visual() && self.mode != Mode::Select {
            return;
        }
        self.record_visual_marks();
        self.mode = Mode::Normal;
        self.clamp_cursor();
    }

    /// The visual mode governing the rendered selection, or `None` when none
    /// should show. Normally just [`Self::mode`] when it's visual — but a command
    /// line *opened from* Visual keeps the selection painted while it's open, so
    /// [`Self::cmdline_from_visual`] carries the originating mode: a `/`,`?` search
    /// (its moving end tracks the incsearch preview at [`Self::cursor`]) and a `:`
    /// ex command (`:'<,'>…`, a static selection — the cursor/anchor don't move)
    /// both keep it lit. Drives the View's selection highlight.
    pub(crate) fn rendered_visual_mode(&self) -> Option<Mode> {
        if self.mode.is_visual() {
            return Some(self.mode);
        }
        // Select mode ([`Mode::Select`], the P6 snippet primitive) highlights its
        // range like a Visual selection, so it borrows the Visual projection —
        // reported as `VisualLine` when linewise (`gH`), else charwise `Visual`.
        if self.mode == Mode::Select {
            return Some(if self.select_linewise {
                Mode::VisualLine
            } else {
                Mode::Visual
            });
        }
        // A Helix selection (`anchor..head`) renders exactly like a charwise visual
        // selection — inclusive of the head — so it reuses the same projection.
        if self.mode.is_helix() {
            return Some(Mode::Visual);
        }
        // A command line opened over a Visual / Helix selection keeps it painted:
        // `cmdline_from_visual` carries the mode to render (a Helix origin is stored
        // as charwise `Visual`). A `/`,`?` search (its moving end tracks the
        // incsearch preview at `cursor`, except in a Helix session where the
        // selection stays put) and a `:'<,'>...` ex command both stay lit.
        if self.mode == Mode::Command {
            return self.cmdline_from_visual;
        }
        None
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

    /// Clear the per-view transient state when the focused window/tab/buffer is
    /// rebound out from under it: the pending operator/count keys, the in-flight
    /// scroll-anim gesture, and any leftover message-line text. Callers set the
    /// mode themselves — a dock re-entry may *restore* a non-Normal mode rather
    /// than resetting it.
    pub(crate) fn reset_transient_state(&mut self) {
        self.reset_pending();
        self.scroll_from = None;
        self.pending_scroll = None;
        self.message.clear();
    }
}

impl Default for Editor {
    fn default() -> Self {
        Editor::new()
    }
}

/// The extension → treesitter-language / filetype table — the single seam where
/// more languages plug in. Both [`language_of_path`] (an extension → filetype
/// lookup) and [`known_filetypes`] (the distinct filetype names, for `:setfiletype`
/// completion) read it, so the two can never drift.
///
/// The value is the **tree-sitter language** noun, which nxvim also uses as the
/// filetype (a filetype *is* a grammar name here). Coverage is the installable
/// nvim-treesitter grammar set intersected with neovim's own extension table
/// (`runtime/lua/vim/filetype.lua`): an extension appears only if it maps to a
/// language nxvim can `:TSInstall`, so opening the file detects the filetype and,
/// once the grammar is installed, highlights it. Extensions neovim resolves by
/// *content* (`.h` C-vs-C++, `.r`, `.v`, `.m`, `.pl`) are omitted rather than
/// guessed — except the historical `("h", "c")` and a few primary extensions whose
/// dialect split collapses to one grammar here (`.sql`, `.tex`, `.xml`, `.typ`).
/// Rebuild by intersecting `vendor/neovim`'s `extension` table with
/// nvim-treesitter's `parsers.lua` at the pinned `NVIM_TS_REF` when that ref moves.
const EXT_FILETYPE: &[(&str, &str)] = &[
    ("ada", "ada"),
    ("adb", "ada"),
    ("ads", "ada"),
    ("gpr", "ada"),
    ("ino", "arduino"),
    ("pde", "arduino"),
    ("astro", "astro"),
    ("zed", "authzed"),
    ("awk", "awk"),
    ("gawk", "awk"),
    ("sh", "bash"),
    ("bash", "bash"),
    ("la", "bash"),
    ("lai", "bash"),
    ("lo", "bash"),
    ("mdd", "bash"),
    ("bass", "bass"),
    ("bean", "beancount"),
    ("beancount", "beancount"),
    ("bicep", "bicep"),
    ("bb", "bitbake"),
    ("bbappend", "bitbake"),
    ("bbclass", "bitbake"),
    ("bp", "bp"),
    ("bt", "bpftrace"),
    ("brs", "brightscript"),
    ("c", "c"),
    ("h", "c"),
    ("epro", "c"),
    ("mdh", "c"),
    ("qc", "c"),
    ("c3", "c3"),
    ("c3i", "c3"),
    ("c3t", "c3"),
    ("cake", "c_sharp"),
    ("cs", "c_sharp"),
    ("csx", "c_sharp"),
    ("cairo", "cairo"),
    ("capnp", "capnp"),
    ("chatito", "chatito"),
    ("clj", "clojure"),
    ("cljc", "clojure"),
    ("cljs", "clojure"),
    ("cljx", "clojure"),
    ("cmake", "cmake"),
    ("corn", "corn"),
    ("cpon", "cpon"),
    ("cpp", "cpp"),
    ("cc", "cpp"),
    ("cxx", "cpp"),
    ("hpp", "cpp"),
    ("C", "cpp"),
    ("H", "cpp"),
    ("c++", "cpp"),
    ("c++m", "cpp"),
    ("ccm", "cpp"),
    ("cppm", "cpp"),
    ("cxxm", "cpp"),
    ("hh", "cpp"),
    ("hxx", "cpp"),
    ("inl", "cpp"),
    ("ipp", "cpp"),
    ("ixx", "cpp"),
    ("moc", "cpp"),
    ("mpp", "cpp"),
    ("tcc", "cpp"),
    ("tlh", "cpp"),
    ("css", "css"),
    ("csv", "csv"),
    ("cu", "cuda"),
    ("cuh", "cuda"),
    ("cue", "cue"),
    ("dart", "dart"),
    ("drt", "dart"),
    ("desktop", "desktop"),
    ("directory", "desktop"),
    ("dhall", "dhall"),
    ("diff", "diff"),
    ("rej", "diff"),
    ("dj", "djot"),
    ("djot", "djot"),
    ("Dockerfile", "dockerfile"),
    ("dockerfile", "dockerfile"),
    ("dot", "dot"),
    ("gv", "dot"),
    ("dtd", "dtd"),
    ("exs", "elixir"),
    ("elm", "elm"),
    ("lc", "elsa"),
    ("elv", "elvish"),
    ("erl", "erlang"),
    ("hrl", "erlang"),
    ("yaws", "erlang"),
    ("fnl", "fennel"),
    ("fnlm", "fennel"),
    ("fir", "firrtl"),
    ("fish", "fish"),
    ("4th", "forth"),
    ("ft", "forth"),
    ("fth", "forth"),
    ("F", "fortran"),
    ("F03", "fortran"),
    ("F08", "fortran"),
    ("F77", "fortran"),
    ("F90", "fortran"),
    ("F95", "fortran"),
    ("FOR", "fortran"),
    ("FPP", "fortran"),
    ("FTN", "fortran"),
    ("f03", "fortran"),
    ("f08", "fortran"),
    ("f77", "fortran"),
    ("f90", "fortran"),
    ("f95", "fortran"),
    ("for", "fortran"),
    ("fortran", "fortran"),
    ("fpp", "fortran"),
    ("ftn", "fortran"),
    ("fsh", "fsh"),
    ("fsi", "fsharp"),
    ("fsx", "fsharp"),
    ("fc", "func"),
    ("gd", "gdscript"),
    ("gdshader", "gdshader"),
    ("shader", "gdshader"),
    ("prettierignore", "gitignore"),
    ("gleam", "gleam"),
    ("comp", "glsl"),
    ("frag", "glsl"),
    ("geom", "glsl"),
    ("glsl", "glsl"),
    ("rahit", "glsl"),
    ("rcall", "glsl"),
    ("rchit", "glsl"),
    ("rgen", "glsl"),
    ("rint", "glsl"),
    ("rmiss", "glsl"),
    ("tesc", "glsl"),
    ("tese", "glsl"),
    ("vert", "glsl"),
    ("gn", "gn"),
    ("gni", "gn"),
    ("gnuplot", "gnuplot"),
    ("gpi", "gnuplot"),
    ("go", "go"),
    ("gql", "graphql"),
    ("graphql", "graphql"),
    ("graphqls", "graphql"),
    ("gradle", "groovy"),
    ("groovy", "groovy"),
    ("hack", "hack"),
    ("hackpartial", "hack"),
    ("ha", "hare"),
    ("hs", "haskell"),
    ("hs-boot", "haskell"),
    ("hsc", "haskell"),
    ("hsig", "haskell"),
    ("hcl", "hcl"),
    ("tfvars", "hcl"),
    ("heex", "heex"),
    ("hjson", "hjson"),
    ("m3u", "hlsplaylist"),
    ("m3u8", "hlsplaylist"),
    ("hoon", "hoon"),
    ("html", "html"),
    ("htm", "html"),
    ("http", "http"),
    ("hurl", "hurl"),
    ("INI", "ini"),
    ("ini", "ini"),
    ("nmconnection", "ini"),
    ("vbp", "ini"),
    ("wrap", "ini"),
    ("inko", "inko"),
    ("jav", "java"),
    ("java", "java"),
    ("jsh", "java"),
    ("js", "javascript"),
    ("mjs", "javascript"),
    ("cjs", "javascript"),
    ("es", "javascript"),
    ("javascript", "javascript"),
    ("jsm", "javascript"),
    ("jsx", "javascript"),
    ("jinja", "jinja"),
    ("jjdescription", "jjdescription"),
    ("jq", "jq"),
    ("json", "json"),
    ("bd", "json"),
    ("bda", "json"),
    ("cps", "json"),
    ("geojson", "json"),
    ("ipynb", "json"),
    ("json-patch", "json"),
    ("jsonc", "json"),
    ("jsonp", "json"),
    ("jupyterlab-settings", "json"),
    ("mcmeta", "json"),
    ("slnf", "json"),
    ("sublime-project", "json"),
    ("sublime-settings", "json"),
    ("sublime-workspace", "json"),
    ("webmanifest", "json"),
    ("xci", "json"),
    ("json5", "json5"),
    ("jsonnet", "jsonnet"),
    ("libsonnet", "jsonnet"),
    ("jl", "julia"),
    ("JUST", "just"),
    ("Just", "just"),
    ("just", "just"),
    ("kdl", "kdl"),
    ("kos", "kos"),
    ("kt", "kotlin"),
    ("ktm", "kotlin"),
    ("kts", "kotlin"),
    ("lalrpop", "lalrpop"),
    ("aux", "latex"),
    ("bbl", "latex"),
    ("bbx", "latex"),
    ("beamer", "latex"),
    ("brf", "latex"),
    ("cbx", "latex"),
    ("clo", "latex"),
    ("dtx", "latex"),
    ("eps_tex", "latex"),
    ("ind", "latex"),
    ("ins", "latex"),
    ("latex", "latex"),
    ("loe", "latex"),
    ("lof", "latex"),
    ("ltx", "latex"),
    ("nav", "latex"),
    ("nlo", "latex"),
    ("nls", "latex"),
    ("pdf_tex", "latex"),
    ("pgf", "latex"),
    ("pygstyle", "latex"),
    ("pygtex", "latex"),
    ("sty", "latex"),
    ("tex", "latex"),
    ("thm", "latex"),
    ("tikz", "latex"),
    ("vrb", "latex"),
    ("journal", "ledger"),
    ("ldg", "ledger"),
    ("ledger", "ledger"),
    ("leo", "leo"),
    ("liquid", "liquid"),
    ("liq", "liquidsoap"),
    ("lua", "lua"),
    ("nse", "lua"),
    ("rockspec", "lua"),
    ("tlu", "lua"),
    ("luau", "luau"),
    ("md", "markdown"),
    ("markdown", "markdown"),
    ("mdown", "markdown"),
    ("mdwn", "markdown"),
    ("mkd", "markdown"),
    ("mkdn", "markdown"),
    ("mermaid", "mermaid"),
    ("mmd", "mermaid"),
    ("mmdc", "mermaid"),
    ("mlir", "mlir"),
    ("nasm", "nasm"),
    ("nginx", "nginx"),
    ("ncl", "nickel"),
    ("nim", "nim"),
    ("nimble", "nim"),
    ("nims", "nim"),
    ("ninja", "ninja"),
    ("nix", "nix"),
    ("nqc", "nqc"),
    ("nu", "nu"),
    ("cppobjdump", "objdump"),
    ("objdump", "objdump"),
    ("ml", "ocaml"),
    ("mli", "ocaml"),
    ("mlip", "ocaml"),
    ("mll", "ocaml"),
    ("mlp", "ocaml"),
    ("mlt", "ocaml"),
    ("mly", "ocaml"),
    ("odin", "odin"),
    ("dpr", "pascal"),
    ("pas", "pascal"),
    ("cer", "pem"),
    ("crt", "pem"),
    ("csr", "pem"),
    ("pem", "pem"),
    ("al", "perl"),
    ("plx", "perl"),
    ("psgi", "perl"),
    ("ctp", "php"),
    ("php", "php"),
    ("php0", "php"),
    ("php1", "php"),
    ("php2", "php"),
    ("php3", "php"),
    ("php4", "php"),
    ("php5", "php"),
    ("php6", "php"),
    ("php7", "php"),
    ("php8", "php"),
    ("php9", "php"),
    ("phpt", "php"),
    ("phtml", "php"),
    ("theme", "php"),
    ("pcf", "pkl"),
    ("pkl", "pkl"),
    ("po", "po"),
    ("pot", "po"),
    ("pod", "pod"),
    ("pony", "pony"),
    ("ps1", "powershell"),
    ("psd1", "powershell"),
    ("psm1", "powershell"),
    ("pssc", "powershell"),
    ("prisma", "prisma"),
    ("proto", "proto"),
    ("prql", "prql"),
    ("pug", "pug"),
    ("purs", "purescript"),
    ("py", "python"),
    ("ipy", "python"),
    ("ptl", "python"),
    ("pyi", "python"),
    ("pyw", "python"),
    ("ql", "ql"),
    ("qll", "ql"),
    ("rkt", "racket"),
    ("rktd", "racket"),
    ("rktl", "racket"),
    ("rasi", "rasi"),
    ("rasinc", "rasi"),
    ("cshtml", "razor"),
    ("razor", "razor"),
    ("rbs", "rbs"),
    ("rego", "rego"),
    ("pip", "requirements"),
    ("res", "rescript"),
    ("resi", "rescript"),
    ("Rnw", "rnoweb"),
    ("Snw", "rnoweb"),
    ("rnw", "rnoweb"),
    ("snw", "rnoweb"),
    ("resource", "robot"),
    ("robot", "robot"),
    ("roc", "roc"),
    ("ron", "ron"),
    ("rst", "rst"),
    ("builder", "ruby"),
    ("gemspec", "ruby"),
    ("rake", "ruby"),
    ("rant", "ruby"),
    ("rb", "ruby"),
    ("rbi", "ruby"),
    ("rbw", "ruby"),
    ("rjs", "ruby"),
    ("ru", "ruby"),
    ("rxml", "ruby"),
    ("rs", "rust"),
    ("mill", "scala"),
    ("scala", "scala"),
    ("scm", "scheme"),
    ("sld", "scheme"),
    ("ss", "scheme"),
    ("stsg", "scheme"),
    ("sass", "scss"),
    ("scss", "scss"),
    ("sl", "slang"),
    ("slint", "slint"),
    ("smali", "smali"),
    ("smithy", "smithy"),
    ("smk", "snakemake"),
    ("sol", "solidity"),
    ("rq", "sparql"),
    ("sparql", "sparql"),
    ("pkb", "sql"),
    ("pks", "sql"),
    ("sql", "sql"),
    ("tyb", "sql"),
    ("tyc", "sql"),
    ("zsql", "sql"),
    ("nut", "squirrel"),
    ("ipd", "starlark"),
    ("sky", "starlark"),
    ("star", "starlark"),
    ("starlark", "starlark"),
    ("quark", "supercollider"),
    ("sface", "surface"),
    ("svelte", "svelte"),
    ("sw", "sway"),
    ("swift", "swift"),
    ("swiftinterface", "swift"),
    ("sv", "systemverilog"),
    ("svh", "systemverilog"),
    ("td", "tablegen"),
    ("itcl", "tcl"),
    ("itk", "tcl"),
    ("jacl", "tcl"),
    ("tcl", "tcl"),
    ("tk", "tcl"),
    ("tm", "tcl"),
    ("tl", "teal"),
    ("templ", "templ"),
    ("tera", "tera"),
    ("thrift", "thrift"),
    ("tig", "tiger"),
    ("toml", "toml"),
    ("tsv", "tsv"),
    ("tsx", "tsx"),
    ("twig", "twig"),
    ("ts", "typescript"),
    ("cts", "typescript"),
    ("mts", "typescript"),
    ("tsp", "typespec"),
    ("typ", "typst"),
    ("ungram", "ungrammar"),
    ("u", "unison"),
    ("uu", "unison"),
    ("usd", "usd"),
    ("usda", "usd"),
    ("vsh", "v"),
    ("vv", "v"),
    ("vala", "vala"),
    ("vto", "vento"),
    ("hdl", "vhdl"),
    ("vbe", "vhdl"),
    ("vhd", "vhdl"),
    ("vhdl", "vhdl"),
    ("vho", "vhdl"),
    ("vst", "vhdl"),
    ("tape", "vhs"),
    ("vim", "vim"),
    ("vue", "vue"),
    ("wgsl", "wgsl"),
    ("wit", "wit"),
    ("atom", "xml"),
    ("bxml", "xml"),
    ("cdxml", "xml"),
    ("csproj", "xml"),
    ("fsproj", "xml"),
    ("mmi", "xml"),
    ("mpd", "xml"),
    ("psc1", "xml"),
    ("reanim", "xml"),
    ("rss", "xml"),
    ("slnx", "xml"),
    ("spfm", "xml"),
    ("tpm", "xml"),
    ("ui", "xml"),
    ("vbproj", "xml"),
    ("wpl", "xml"),
    ("wsdl", "xml"),
    ("xba", "xml"),
    ("xcu", "xml"),
    ("xlb", "xml"),
    ("xlc", "xml"),
    ("xlf", "xml"),
    ("xliff", "xml"),
    ("xmi", "xml"),
    ("xml", "xml"),
    ("xpfm", "xml"),
    ("xpr", "xml"),
    ("xul", "xml"),
    ("yaml", "yaml"),
    ("yml", "yaml"),
    ("eyaml", "yaml"),
    ("ksy", "yaml"),
    ("kyaml", "yaml"),
    ("kyml", "yaml"),
    ("mplstyle", "yaml"),
    ("yang", "yang"),
    ("yuck", "yuck"),
    ("zig", "zig"),
    ("zon", "zig"),
    ("ziggy", "ziggy"),
    ("ziggy-schema", "ziggy_schema"),
    ("zsh", "zsh"),
    ("zsh-theme", "zsh"),
    ("zunit", "zsh"),
];

/// Map a file path's extension to a treesitter language / filetype name, or
/// `None` for an unknown (or absent) extension — in which case the buffer has no
/// highlighting and no treesitter indentation. The server's `filetype_of` (FileType
/// autocmd, LSP) delegates here, so the [`EXT_FILETYPE`] table lives in one place.
pub fn language_of_path(path: Option<&Path>) -> Option<&'static str> {
    let ext = path?.extension()?.to_str()?;
    EXT_FILETYPE
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, ft)| *ft)
}

/// The distinct filetype names nxvim recognizes (the value set of [`EXT_FILETYPE`]),
/// sorted. These are the highlighting-capable filetypes `:setfiletype` completion
/// offers — the same source of truth extension detection uses, so the list is never
/// stale. A buffer can still be forced to any string; this is just the known set.
pub fn known_filetypes() -> Vec<&'static str> {
    let mut fts: Vec<&'static str> = EXT_FILETYPE.iter().map(|(_, ft)| *ft).collect();
    fts.sort_unstable();
    fts.dedup();
    fts
}

/// The treesitter grammar for a vim **help** file, or `None` when `path` isn't one.
/// Help files aren't identified by extension alone (plain `.txt` is not help), so this
/// can't fold into [`language_of_path`]: it applies neovim's rule — a `.txt` under a
/// `doc/` directory whose file carries a `vim:…ft=help…` modeline. `last_line` is the
/// file's last non-blank line (where the modeline lives). The grammar is `vimdoc`
/// (neovim's `filetype=help` maps to the `vimdoc` parser); callers slot this beside
/// `language_of_path` so a help preview/buffer highlights when that parser is installed.
pub fn language_of_help_doc(path: &Path, last_line: &str) -> Option<&'static str> {
    if path.extension()?.to_str()? != "txt" {
        return None;
    }
    if path.parent()?.file_name()?.to_str()? != "doc" {
        return None;
    }
    is_help_modeline(last_line).then_some("vimdoc")
}

/// Whether `line` is a vim modeline selecting the help filetype (`ft=help` /
/// `filetype=help`), matching neovim's `doc/*.txt` detection. A modeline is a `vim:`
/// token at line start or after whitespace, followed by `:`/space-delimited options;
/// we accept it when any option is exactly `ft=help` or `filetype=help`.
fn is_help_modeline(line: &str) -> bool {
    line.match_indices("vim:").any(|(i, _)| {
        let at_boundary = i == 0 || line.as_bytes()[i - 1].is_ascii_whitespace();
        at_boundary
            && line[i + 4..]
                .split(|c: char| c == ':' || c.is_whitespace())
                .any(|opt| opt == "ft=help" || opt == "filetype=help")
    })
}

/// Whether `path`'s extension names a raster image nxvim can preview — the gate
/// for `'imagepreview'` ([`crate::options::Options::imagepreview`]). Extension-only
/// (no content sniffing): cheap, and the same way the filetype is decided
/// ([`language_of_path`]). The set mirrors the `image` crate's common decoders
/// (the renderer's actual decode lives client-side); case-insensitive.
pub fn is_image_path(path: Option<&Path>) -> bool {
    let Some(ext) = path.and_then(Path::extension).and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "bmp"
            | "webp"
            | "tiff"
            | "tif"
            | "ico"
            | "tga"
            | "qoi"
            | "ppm"
            | "pgm"
            | "pbm"
            | "pnm"
    )
}
