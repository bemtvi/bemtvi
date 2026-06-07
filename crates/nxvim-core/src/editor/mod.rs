//! The editor state machine: turns keys and ex-commands into buffer mutations.
//!
//! This is the rust-native analogue of neovim's `normal.c` / `ops.c` /
//! `edit.c` / `ex_docmd.c`. It is fully synchronous and owns no I/O beyond
//! reading/writing files through [`Buffer`]. The async server feeds it input
//! and reads back state; it never blocks.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::buffer::Buffer;
use crate::clipboard::Clipboard;
use crate::highlight::Highlights;
use crate::input::Key;
use crate::mode::Mode;
use crate::options::Options;
use crate::syntax::SyntaxEngine;
use crate::view::View;

mod buffers;
mod cmdline;
mod command;
mod cursor;
mod ex;
mod insert;
mod marks;
mod motions;
mod operators;
mod options;
mod panel;
mod registers;
mod search;
mod syntax;
mod tabs;
mod undo;
mod windows;

// The command grammar + its normal/visual executor. The parse↔execute contract
// types stay private to `command`; only the shared vocabulary is re-exported.
pub use self::command::{command_status, CommandStatus};
pub(crate) use self::command::{
    FindKind, Motion, MotionKind, MotionResult, MoveAxis, ObjectKind, PendingCommand, Stage,
};
// The window layout subsystem (tree types + layout algebra + window methods).
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

pub(crate) use registers::{RegKind, Registers};

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
    /// The buffer's extmarks at this history point. Restored on undo/redo so marks
    /// ride with the text (neovim keeps extmarks across undo) rather than being
    /// dropped by the wholesale-replace [`Buffer::mark_resync`].
    extmarks: crate::extmark::ExtmarkStore,
    /// The buffer's `a`–`z` marks at this history point — restored on undo/redo
    /// alongside `extmarks`, for the same reason: vim keeps marks across undo, and
    /// the wholesale-replace [`Buffer::mark_resync`] would otherwise clear them.
    marks: HashMap<char, (usize, usize)>,
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

/// The complete editor state: the open buffers plus the single window's state.
pub struct Editor {
    /// All open buffers, keyed by id. The current buffer is *derived* from the
    /// focused window ([`Editor::cur_buffer`]); reach its text model through
    /// [`Editor::buffer`].
    buffers: BufferStore,
    /// The window layout of the **active** tab — the split tree, the focused
    /// window, and per-window view state. The buffer it shows is the current
    /// buffer. Inactive tabs stash their own layout in [`Editor::tabs`].
    windows: WindowTree,
    /// Every tab page in tabline order. The active tab's slot holds `None` (its
    /// layout is live on [`Editor::windows`]); inactive tabs stash theirs. Holds
    /// exactly one tab until the creation surface lands.
    tabs: Vec<TabSlot>,
    /// Index into [`Editor::tabs`] of the active tab.
    current_tab: usize,
    /// The next window id to mint, monotonic and **global** across every tab (a
    /// window handle is never reused, matching neovim). The active tab's splits and
    /// every new tab's first window draw from this one counter, so two tabs can
    /// never hand out the same [`WindowId`]. Seeded past the first window (id 1).
    next_win_id: u64,
    /// The next tab id to mint, monotonic and never reused. Seeded past the first
    /// tab (id 1).
    next_tab_id: u64,
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

    /// The in-process treesitter backend, or `None` in a bare-core test (or a
    /// front end that ships no grammars). The editor owns it so highlights — and,
    /// later, treesitter indentation — are answered **synchronously, in the same
    /// frame** as the edit. It keeps its own shadow text per buffer, so its
    /// methods never borrow [`Editor::buffers`]. Installed by the server via
    /// [`Editor::set_syntax_engine`].
    syntax: Option<Box<dyn SyntaxEngine>>,
    /// The language each buffer was last `open`ed in the engine with, so a query
    /// knows whether to re-`open` (first sync, or the path's language changed) vs
    /// apply incremental `edit` deltas. Dropped when the buffer is deleted.
    syntax_opened: HashMap<BufferId, &'static str>,
    /// Languages whose grammar was *installed but failed to load*, already echoed
    /// once. Dedups the failure message so opening many files of a broken-grammar
    /// language doesn't spam (a *missing* grammar is silent and never recorded).
    syntax_failed: HashSet<&'static str>,
    /// The host clipboard backing the `"+` / `"*` registers, or `None` in a
    /// bare-core test (or a front end whose platform backend failed to start).
    /// Injected by the server via [`Editor::set_clipboard`]; when absent,
    /// selecting `"+` / `"*` errors loudly instead of touching the unnamed
    /// register.
    clipboard: Option<Box<dyn Clipboard>>,
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
            tabs: vec![TabSlot {
                id: TabId(1),
                tree: None,
            }],
            current_tab: 0,
            next_win_id: 2,
            next_tab_id: 2,
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
            last_find: None,
            snapshot_taken: false,
            soft_tab: None,
            visual_anchor: Cursor::default(),
            scroll_from: None,
            pending_scroll: None,
            lua_queue: Vec::new(),
            deferred_commands: Vec::new(),
            pending_sleep: None,
            syntax: None,
            syntax_opened: HashMap::new(),
            syntax_failed: HashSet::new(),
            clipboard: None,
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

    /// Feed a single key into the editor.
    pub fn input(&mut self, key: Key) {
        // A focused panel grabs every key (navigation + close), bypassing the
        // buffer's mode handling and the `curswant`/scroll bookkeeping below.
        if self.panel.is_some() {
            self.handle_panel(key);
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
