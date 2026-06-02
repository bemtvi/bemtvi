//! The editor state machine: turns keys and ex-commands into buffer mutations.
//!
//! This is the rust-native analogue of neovim's `normal.c` / `ops.c` /
//! `edit.c` / `ex_docmd.c`. It is fully synchronous and owns no I/O beyond
//! reading/writing files through [`Buffer`]. The async server feeds it input
//! and reads back state; it never blocks.

use std::cmp::{max, min};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::buffer::Buffer;
use crate::highlight::Highlights;
use crate::input::{Key, KeyCode};
use crate::mode::Mode;
use crate::options::{resolve_set, Options, SetOp};
use crate::search::SearchRegex;
use crate::unicode;
use crate::view::{PanelView, View};

/// Default content height of the bottom panel, in rows (vim's quickfix
/// default). The projection clamps it down so the text window keeps a row.
const PANEL_HEIGHT: usize = 10;

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

/// What the command line is editing: an `:` ex command, or a `/`,`?` search.
/// One [`Mode::Command`] serves both; the kind decides the prompt char and what
/// `<CR>` does. Set on entry, read on submit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CmdlineKind {
    Ex,
    Search(SearchDir),
}

#[derive(Clone)]
struct Snapshot {
    text: ropey::Rope,
    cursor: Cursor,
}

/// A buffer as the editor holds it: the text [`Buffer`] plus the state vim keeps
/// with the buffer rather than the window — undo/redo history and, while the
/// buffer is not current, the last cursor/scroll position so switching back
/// restores the view.
struct OpenBuffer {
    buffer: Buffer,
    undo_stack: Vec<Snapshot>,
    redo_stack: Vec<Snapshot>,
    /// Window position saved when this buffer stops being current; restored on
    /// switch-back. Meaningless while the buffer *is* current — the live position
    /// is then [`Editor::cursor`] / `Editor::top`.
    saved_cursor: Cursor,
    saved_top: usize,
}

impl OpenBuffer {
    fn new(buffer: Buffer) -> Self {
        OpenBuffer {
            buffer,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            saved_cursor: Cursor::default(),
            saved_top: 0,
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
}

/// The complete editor state: the open buffers plus the single window's state.
pub struct Editor {
    /// All open buffers, keyed by id. [`Editor::current`] selects the one the
    /// window shows; reach the current text model through [`Editor::buffer`].
    buffers: BufferStore,
    /// The buffer the window currently shows. Always present in `buffers`.
    current: BufferId,
    /// vim's alternate buffer (`#`), the `<C-^>` target; `None` until a switch
    /// sets it.
    alternate: Option<BufferId>,
    pub mode: Mode,
    pub cursor: Cursor,
    /// First visible buffer line (vertical scroll offset).
    pub top: usize,
    /// Command-line contents (text after the leading `:` / `/` / `?`).
    pub cmdline: String,
    /// What the command line is editing (`:` ex vs `/`,`?` search). Decides the
    /// prompt char and what `<CR>` submits. Only meaningful in [`Mode::Command`].
    cmdline_kind: CmdlineKind,
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
    /// Position within [`Editor::search_history`] while browsing it; `None` when
    /// editing a fresh line. Reset each time a search prompt opens.
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

    // pending normal-mode state
    count: Option<usize>,
    op_count: Option<usize>,
    operator: Option<char>,
    gpending: bool,
    pending_replace: bool,
    /// Set when an undo snapshot has already been taken for the current edit
    /// "session" (e.g. an insert), so we group the whole session into one undo.
    snapshot_taken: bool,
    visual_anchor: Cursor,

    /// Set by a scroll command at the moment it fires: `(top, cursor.line)`
    /// *before* the move. Consumed at the end of `input` to build `pending_scroll`.
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
        Editor {
            buffers,
            current,
            alternate: None,
            mode: Mode::Normal,
            cursor: Cursor::default(),
            top: 0,
            cmdline: String::new(),
            cmdline_kind: CmdlineKind::Ex,
            last_search: None,
            search_operator: None,
            pending_search_count: 1,
            search_history: Vec::new(),
            hist_idx: None,
            search_active: false,
            search_origin: Cursor::default(),
            message: String::new(),
            messages: Vec::new(),
            panel: None,
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
            count: None,
            op_count: None,
            operator: None,
            gpending: false,
            pending_replace: false,
            snapshot_taken: false,
            visual_anchor: Cursor::default(),
            scroll_from: None,
            pending_scroll: None,
            lua_queue: Vec::new(),
            deferred_commands: Vec::new(),
            pending_sleep: None,
        }
    }

    // ----- public API used by the server -----------------------------------

    /// The current buffer's text model. The window always shows exactly one
    /// buffer; this resolves it through the store, so the rest of the editor can
    /// keep saying `self.buffer()` without caring how many buffers are open.
    pub fn buffer(&self) -> &Buffer {
        &self.buffers.get(self.current).buffer
    }

    /// Mutable access to the current buffer's text model (see [`Editor::buffer`]).
    pub fn buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers.get_mut(self.current).buffer
    }

    /// The current buffer's full editor-side state (text + undo/redo + saved
    /// position). Internal helper for the undo path and switching.
    fn cur_mut(&mut self) -> &mut OpenBuffer {
        self.buffers.get_mut(self.current)
    }

    /// The id of the buffer the window currently shows.
    pub fn current_buffer_id(&self) -> BufferId {
        self.current
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
        if id == self.current || !self.buffers.map.contains_key(&id) {
            return;
        }
        // Stash the outgoing position with its buffer; it becomes the alternate.
        let (cursor, top) = (self.cursor, self.top);
        let outgoing = self.current;
        let out = self.buffers.get_mut(outgoing);
        out.saved_cursor = cursor;
        out.saved_top = top;
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
        let (saved_cursor, saved_top) = (incoming.saved_cursor, incoming.saved_top);
        self.current = id;
        self.cursor = saved_cursor;
        self.top = saved_top;
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

    /// The id of an already-open buffer bound to `path`, if any. Matches by
    /// canonical path when both resolve on disk (so `./a` and `a` are the same
    /// file), else by the stored path verbatim (covers not-yet-written names).
    fn find_buffer_by_path(&self, path: &Path) -> Option<BufferId> {
        let target = std::fs::canonicalize(path).ok();
        self.buffers.map.iter().find_map(|(id, ob)| {
            let stored = ob.buffer.path.as_ref()?;
            let same = match (&target, std::fs::canonicalize(stored).ok()) {
                (Some(t), Some(c)) => *t == c,
                _ => stored.as_path() == path,
            };
            same.then_some(*id)
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
                let ob = self.cur_mut();
                ob.buffer = buf;
                ob.undo_stack.clear();
                ob.redo_stack.clear();
                ob.buffer.mark_resync();
                ob.buffer.modified = false;
            }
            Err(e) => self.echo(e.to_string()),
        }
    }

    /// Resize the *text viewport*. The client owns the screen layout and tells
    /// us only how tall the text area is (status/command lines are the client's
    /// own regions), so the whole height here is editable rows.
    pub fn resize(&mut self, width: usize, height: usize) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.ensure_visible();
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

        // If this key was a scroll command that actually moved the viewport,
        // record the gesture for the client to animate.
        if let Some((from_top, from_cursor)) = self.scroll_from.take() {
            if from_top != self.top {
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

    /// Width in cells available for buffer text: the text-area width minus the
    /// number column. Selection fills measure against this (and future soft-wrap
    /// / horizontal scroll will too), so the gutter is never counted as text.
    pub(crate) fn text_width(&self) -> usize {
        self.width.saturating_sub(self.number_width())
    }

    pub(crate) fn pending_scroll(&self) -> Option<PendingScroll> {
        self.pending_scroll
    }

    /// The fixed end of the visual selection (the other end is [`Self::cursor`]).
    /// Only meaningful while [`Self::mode`] is a visual mode.
    pub(crate) fn visual_anchor(&self) -> Cursor {
        self.visual_anchor
    }

    pub(crate) fn text_height(&self) -> usize {
        // The panel (when open) eats rows off the bottom of the text window.
        self.height.saturating_sub(self.panel_rows()).max(1)
    }

    // ----- normal / visual mode --------------------------------------------

    fn handle_normal(&mut self, key: Key) {
        self.message.clear();

        // `r{char}` waits for one key, then overwrites; `r<Esc>` cancels.
        if self.pending_replace {
            self.pending_replace = false;
            if key.code != KeyCode::Esc {
                if let Some(c) = key.as_char() {
                    let count = self.effective_count();
                    self.replace_char(c, count);
                }
            }
            self.reset_pending();
            return;
        }

        // Escape cancels pending state / leaves visual mode.
        if key.code == KeyCode::Esc {
            self.operator = None;
            self.count = None;
            self.op_count = None;
            self.gpending = false;
            if self.mode.is_visual() {
                self.mode = Mode::Normal;
                self.clamp_cursor();
            }
            return;
        }

        // Count accumulation. `0` is a motion unless a count is already started.
        if let Some(c) = key.as_char() {
            if c.is_ascii_digit() && !(c == '0' && self.count.is_none()) {
                let d = c as usize - '0' as usize;
                self.count = Some(self.count.unwrap_or(0) * 10 + d);
                return;
            }
        }

        // `g` prefix (gg, etc.).
        if !self.gpending {
            if let Some('g') = key.as_char() {
                self.gpending = true;
                return;
            }
        }

        // Operator + motion resolution.
        let raw = if self.operator.is_some() {
            self.op_count.or(self.count)
        } else {
            self.count
        };
        let count = self.effective_count();

        if let Some(m) = self.resolve_motion(key, count, raw) {
            self.gpending = false;
            if let Some(op) = self.operator.take() {
                self.apply_operator(op, m);
                self.reset_pending();
            } else {
                // Movement (normal or visual); selection follows the cursor.
                self.apply_movement(m);
                self.count = None;
            }
            return;
        }

        let gstar = self.gpending;
        self.gpending = false;
        self.handle_normal_command(key, count, gstar);
    }

    fn handle_normal_command(&mut self, key: Key, count: usize, gstar: bool) {
        // With an operator pending, the only thing that lands here is a doubled
        // operator (dd/cc/yy) or a search motion (`d/`,`y?`); anything else
        // cancels the pending operator.
        if let Some(op) = self.operator {
            match key.as_char() {
                Some(c) if c == op => self.begin_operator(op),
                // Open a search prompt as an operator motion; the committed <CR>
                // applies `op` over the match (see `submit_search`).
                Some('/') => {
                    self.search_operator = Some(op);
                    self.enter_search(SearchDir::Forward, count);
                }
                Some('?') => {
                    self.search_operator = Some(op);
                    self.enter_search(SearchDir::Backward, count);
                }
                _ => self.reset_pending(),
            }
            return;
        }

        // Ctrl-keyed scrolling, redo, and the alternate-buffer toggle.
        if key.ctrl {
            match key.code {
                KeyCode::Char('d') => self.scroll_half(true),
                KeyCode::Char('u') => self.scroll_half(false),
                KeyCode::Char('f') => self.scroll_page(true),
                KeyCode::Char('b') => self.scroll_page(false),
                KeyCode::Char('r') => self.redo(),
                // `<C-^>` / `<C-6>`: jump to the alternate buffer (`#`).
                KeyCode::Char('^') | KeyCode::Char('6') => self.goto_alternate(),
                _ => {}
            }
            self.reset_pending();
            return;
        }

        let c = match key.as_char() {
            Some(c) => c,
            None => {
                self.reset_pending();
                return;
            }
        };

        // Visual-mode operators act on the selection immediately.
        if self.mode.is_visual() {
            match c {
                'd' | 'x' => {
                    self.visual_operate('d');
                    return;
                }
                'y' => {
                    self.visual_operate('y');
                    return;
                }
                'c' | 's' => {
                    self.visual_operate('c');
                    return;
                }
                'v' => {
                    self.mode = Mode::Visual;
                    return;
                }
                'V' => {
                    self.mode = Mode::VisualLine;
                    return;
                }
                ':' => {
                    self.enter_command();
                    return;
                }
                _ => {}
            }
        }

        match c {
            'i' => self.enter_insert_at(self.cursor.col),
            'I' => {
                let col = self.first_non_blank(self.cursor.line);
                self.enter_insert_at(col);
            }
            'a' => {
                let col = (self.cursor.col + 1).min(self.line_len());
                self.enter_insert_at(col);
            }
            'A' => self.enter_insert_at(self.line_len()),
            'o' => self.open_line(true),
            'O' => self.open_line(false),
            'x' => self.delete_under_cursor(count),
            'X' => self.delete_before_cursor(count),
            'D' => self.delete_to_eol(),
            'C' => {
                self.delete_to_eol();
                self.mode = Mode::Insert;
                self.snapshot_taken = true;
            }
            's' => {
                self.delete_under_cursor(count);
                self.mode = Mode::Insert;
                self.snapshot_taken = true;
            }
            'd' | 'c' | 'y' => {
                self.begin_operator(c);
                return;
            }
            'r' => {
                self.pending_replace = true;
                return;
            }
            'p' => self.paste(true, count),
            'P' => self.paste(false, count),
            'u' => self.undo(),
            'J' => self.join_lines(count.max(2)),
            '~' => self.toggle_case(count),
            'v' => {
                self.mode = Mode::Visual;
                self.visual_anchor = self.cursor;
            }
            'V' => {
                self.mode = Mode::VisualLine;
                self.visual_anchor = self.cursor;
            }
            ':' => self.enter_command(),
            '/' => self.enter_search(SearchDir::Forward, count),
            '?' => self.enter_search(SearchDir::Backward, count),
            'n' => self.search_repeat(true, count),
            'N' => self.search_repeat(false, count),
            // `*`/`#` search the word under the cursor (whole-word); `g*`/`g#`
            // drop the word boundaries.
            '*' => self.search_word_under_cursor(SearchDir::Forward, !gstar, count),
            '#' => self.search_word_under_cursor(SearchDir::Backward, !gstar, count),
            _ => {}
        }
        self.reset_pending();
    }

    fn begin_operator(&mut self, op: char) {
        if self.operator == Some(op) {
            // Doubled operator: linewise over `count` lines.
            let count = self.effective_count();
            let last = self.cursor.line + count - 1;
            let target = self.buffer().line_start(last.min(self.last_line()));
            // axis is unused for the operator path, but the field is required.
            let m = MotionResult::linewise(target, MoveAxis::LineAnchor);
            self.operator = None;
            self.apply_operator(op, m);
            self.reset_pending();
        } else {
            self.operator = Some(op);
            self.op_count = self.count.take();
        }
    }

    // ----- motions ----------------------------------------------------------

    fn resolve_motion(&self, key: Key, count: usize, raw: Option<usize>) -> Option<MotionResult> {
        let line = self.cursor.line;
        let last_line = self.last_line();

        let kc = key.code;
        let ch = key.as_char();

        if self.gpending {
            if ch == Some('g') {
                let target_line = raw.map(|n| n - 1).unwrap_or(0).min(last_line);
                let target = self.buffer().line_start(target_line);
                return Some(MotionResult::linewise(target, MoveAxis::LineAnchor));
            }
            return None;
        }

        let motion = match (kc, ch) {
            (KeyCode::Left, _) | (_, Some('h')) | (KeyCode::Backspace, _) => {
                let s = self.buffer().line(line);
                let mut col = self.cursor.col;
                for _ in 0..count {
                    col = unicode::prev_grapheme(&s, col);
                }
                MotionResult::exclusive(self.buffer().byte_at(line, col))
            }
            (KeyCode::Right, _) | (_, Some('l')) | (_, Some(' ')) => {
                let s = self.buffer().line(line);
                let mut col = self.cursor.col;
                for _ in 0..count {
                    col = unicode::next_grapheme(&s, col);
                }
                MotionResult::exclusive(self.buffer().byte_at(line, col))
            }
            (_, Some('0')) | (KeyCode::Home, _) => {
                MotionResult::exclusive(self.buffer().byte_at(line, 0))
            }
            (_, Some('^')) => {
                let col = self.first_non_blank(line);
                MotionResult::exclusive(self.buffer().byte_at(line, col))
            }
            (_, Some('$')) | (KeyCode::End, _) => {
                let l = (line + count - 1).min(last_line);
                let s = self.buffer().line(l);
                let col = unicode::prev_grapheme(&s, s.len());
                MotionResult {
                    target: self.buffer().byte_at(l, col),
                    kind: MotionKind::Inclusive,
                    axis: MoveAxis::EndOfLine,
                }
            }
            (KeyCode::Down, _) | (_, Some('j')) | (_, Some('\r')) => {
                let l = (line + count).min(last_line);
                MotionResult::linewise(self.buffer().line_start(l), MoveAxis::VerticalKeep)
            }
            (KeyCode::Up, _) | (_, Some('k')) => {
                let l = line.saturating_sub(count);
                MotionResult::linewise(self.buffer().line_start(l), MoveAxis::VerticalKeep)
            }
            (_, Some('G')) => {
                let l = raw.map(|n| n - 1).unwrap_or(last_line).min(last_line);
                MotionResult::linewise(self.buffer().line_start(l), MoveAxis::LineAnchor)
            }
            (_, Some('w')) | (_, Some('W')) => self.word_motion(count),
            (_, Some('b')) | (_, Some('B')) => {
                let mut idx = self.cursor_char();
                for _ in 0..count {
                    idx = self.word_backward(idx);
                }
                MotionResult::exclusive(idx)
            }
            (_, Some('e')) | (_, Some('E')) => {
                let mut idx = self.cursor_char();
                for _ in 0..count {
                    idx = self.word_end(idx);
                }
                MotionResult::inclusive(idx)
            }
            _ => return None,
        };
        Some(motion)
    }

    /// Resolve a `w`/`W` word motion. Special case: `cw` on a non-blank acts like
    /// `ce` — it changes to the end of the word without swallowing the trailing
    /// space — so it returns an inclusive end-of-word target instead.
    fn word_motion(&self, count: usize) -> MotionResult {
        let mut idx = self.cursor_char();
        if self.operator == Some('c')
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
            KeyCode::Backspace => self.insert_backspace(),
            KeyCode::Tab => {
                let at = self.cursor_char();
                self.buffer_mut().insert_char(at, '\t');
                self.cursor.col += 1;
                self.buffer_mut().modified = true;
            }
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

    fn insert_backspace(&mut self) {
        if self.cursor.col > 0 {
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

    // ----- command-line mode ------------------------------------------------

    fn enter_command(&mut self) {
        self.mode = Mode::Command;
        self.cmdline.clear();
        self.cmdline_kind = CmdlineKind::Ex;
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
        self.cmdline_kind = CmdlineKind::Search(dir);
        self.pending_search_count = count.max(1);
        self.hist_idx = None;
        self.search_origin = self.cursor;
        self.message.clear();
        self.reset_pending();
    }

    fn handle_command(&mut self, key: Key) {
        match key.code {
            KeyCode::Esc => {
                self.cancel_cmdline();
                return;
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.cmdline);
                let kind = self.cmdline_kind;
                self.mode = Mode::Normal;
                match kind {
                    CmdlineKind::Ex => self.execute_ex(&text),
                    CmdlineKind::Search(dir) => {
                        // Commit from the saved origin, not the incsearch preview
                        // hop, so the count search lands deterministically (and
                        // identically to the no-incsearch path).
                        self.cursor = self.search_origin;
                        self.submit_search(&text, dir);
                    }
                }
                return;
            }
            // Backspacing past the start of an empty command line exits, like Esc.
            // (When the line is non-empty, evaluating the guard's `pop()` removes
            // the last char as a side effect and we fall through to re-preview.)
            KeyCode::Backspace if self.cmdline.pop().is_none() => {
                self.cancel_cmdline();
                return;
            }
            // Search-history recall (`<Up>`/`<C-p>` older, `<Down>`/`<C-n>`
            // newer); a no-op for an ex command line.
            KeyCode::Up => self.cmdline_history_prev(),
            KeyCode::Down => self.cmdline_history_next(),
            KeyCode::Char('p') if key.ctrl => self.cmdline_history_prev(),
            KeyCode::Char('n') if key.ctrl => self.cmdline_history_next(),
            KeyCode::Char(c) if !key.ctrl => self.cmdline.push(c),
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
        self.mode = Mode::Normal;
        self.cmdline.clear();
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

    /// The first match of `re` at or after byte `start`, scanning lines downward
    /// to the end of the buffer, as a whole-buffer `(start, end)` range. `None` if
    /// no match lies in `[start, end_of_buffer)`.
    fn match_forward_from(&self, re: &SearchRegex, start: usize) -> Option<MatchRange> {
        let buf = self.buffer();
        let line_count = buf.line_count();
        let mut line = buf.byte_to_line(start.min(self.last_char_idx()));
        let mut col = start.saturating_sub(buf.line_start(line));
        while line < line_count {
            let text = buf.line(line);
            if let Some((s, e)) = re.find_at(&text, col) {
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
    pub(crate) fn search_highlights(
        &self,
        base: usize,
        count: usize,
    ) -> (SearchSpans, IncSearchSpans) {
        let mut matches = vec![Vec::new(); count];
        let mut current = vec![None; count];

        let search_dir = match self.cmdline_kind {
            CmdlineKind::Search(dir) => Some(dir),
            CmdlineKind::Ex => None,
        };
        let incsearch = self.mode == Mode::Command
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
        let line_count = self.buffer().line_count();

        for (row, row_spans) in matches.iter_mut().enumerate() {
            let buf_line = base + row;
            if buf_line >= line_count {
                break;
            }
            let text = self.buffer().line(buf_line);
            for (s, e) in re.find_all(&text) {
                let span = (
                    unicode::virtcol(&text, s, unicode::TABSTOP),
                    unicode::virtcol(&text, e, unicode::TABSTOP),
                );
                row_spans.push(span);
                // The preview cursor sits on the start of its match, so an exact
                // column hit on the cursor's line marks the current match.
                if incsearch && buf_line == self.cursor.line && s == self.cursor.col {
                    current[row] = Some(span);
                }
            }
        }
        (matches, current)
    }

    /// `<Up>`/`<C-p>` in the search command line — recall the previous history
    /// entry (the newest first), replacing the typed line. A no-op outside a
    /// search prompt or with an empty history.
    fn cmdline_history_prev(&mut self) {
        if !matches!(self.cmdline_kind, CmdlineKind::Search(_)) || self.search_history.is_empty() {
            return;
        }
        let idx = match self.hist_idx {
            None => self.search_history.len() - 1,
            Some(i) => i.saturating_sub(1),
        };
        self.hist_idx = Some(idx);
        self.cmdline = self.search_history[idx].clone();
    }

    /// `<Down>`/`<C-n>` in the search command line — move to a newer history
    /// entry, or back to an empty line once past the newest.
    fn cmdline_history_next(&mut self) {
        if !matches!(self.cmdline_kind, CmdlineKind::Search(_)) {
            return;
        }
        match self.hist_idx {
            Some(i) if i + 1 < self.search_history.len() => {
                self.hist_idx = Some(i + 1);
                self.cmdline = self.search_history[i + 1].clone();
            }
            Some(_) => {
                self.hist_idx = None;
                self.cmdline.clear();
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
        }
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
                // Write the current buffer, then quit (`:q` rules: exit unless
                // another buffer is still unsaved). A failed write leaves the
                // current buffer modified, so `:q` then reports it.
                self.ex_write(args);
                self.ex_quit(bang);
            }
            "qa" | "qall" | "quita" | "quitall" => self.ex_quit(bang),
            "wa" | "wall" => self.ex_write_all(),
            "wqa" | "xa" | "xall" => {
                self.ex_write_all();
                self.ex_quit(bang);
            }
            "e" | "edit" => self.ex_edit(args, bang),
            "ene" | "enew" => self.ex_enew(),
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
            "set" | "se" => self.ex_set(args),
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

    /// `:q` — quit the editor, but only if nothing would be lost. With `!`, exit
    /// unconditionally (discarding every buffer). Otherwise, if any buffer has
    /// unsaved changes, *don't* quit: switch the window to that buffer (the
    /// current one if it's the one modified, else the lowest-numbered modified
    /// buffer) and report `E37`, so the user sees what's blocking the quit. With
    /// no modified buffers, exit. (Single-window nxvim, so `:q` and `:qa` are the
    /// same; real windows will split them later.)
    fn ex_quit(&mut self, bang: bool) {
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
        let current = self.current;
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
            return Some(self.current);
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
        if let Some(i) = ids.iter().position(|id| *id == self.current) {
            self.switch_buffer(ids[(i + count) % len]);
        }
    }

    /// `:bprevious` — switch `count` positions earlier in id order, wrapping.
    fn ex_bprev(&mut self, count: usize) {
        let ids = self.buffer_ids();
        let len = ids.len();
        if let Some(i) = ids.iter().position(|id| *id == self.current) {
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
        let was_current = target == self.current;
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
            self.current = id;
            self.alternate = None;
            self.cursor = Cursor::default();
            self.top = 0;
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
        self.panel = Some(Panel {
            title: title.into(),
            lines,
            cursor,
            top: 0,
            height: PANEL_HEIGHT,
            gpending: false,
            wants_select,
        });
        self.ensure_visible();
        self.scroll_panel_into_view();
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

    /// Replace the open panel's content (`vim.panel.set_lines` /
    /// `nxvim_panel_set_lines`), keeping its title and re-clamping the cursor and
    /// scroll to the new content. A no-op when no panel is open.
    pub fn set_panel_lines(&mut self, lines: Vec<String>) {
        if let Some(panel) = self.panel.as_mut() {
            let last = lines.len().saturating_sub(1);
            panel.lines = lines;
            panel.cursor = panel.cursor.min(last);
            panel.top = panel.top.min(last);
        }
        self.scroll_panel_into_view();
    }

    /// Re-derive the open panel's `top` so its `cursor` line stays within the
    /// visible window — the shared scroll step for opening, content swaps, and
    /// keyboard motion. A no-op when no panel is open.
    fn scroll_panel_into_view(&mut self) {
        let ph = self.panel_content_height().max(1);
        if let Some(panel) = self.panel.as_mut() {
            if panel.cursor < panel.top {
                panel.top = panel.cursor;
            } else if panel.cursor >= panel.top + ph {
                panel.top = panel.cursor + 1 - ph;
            }
        }
    }

    /// Close the panel and return focus to the text window, which grows back.
    /// Public for the scripting surface (`vim.panel.close` /
    /// `nxvim_panel_close`); a no-op when no panel is open.
    pub fn close_panel(&mut self) {
        self.panel = None;
        self.ensure_visible();
    }

    /// Whether a panel is currently open (the `nxvim_panel_is_open` query).
    pub fn panel_is_open(&self) -> bool {
        self.panel.is_some()
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

    /// Project the panel into the renderable [`PanelView`]: the visible slice of
    /// its content, the cursor's row within that slice, and the clamped content
    /// height. `None` when no panel is open. (`pub(crate)` so [`View`] can build
    /// it while [`Panel`] stays private.)
    pub(crate) fn panel_view(&self) -> Option<PanelView> {
        let p = self.panel.as_ref()?;
        let height = self.panel_content_height();
        let lines = p.lines.iter().skip(p.top).take(height).cloned().collect();
        Some(PanelView {
            title: p.title.clone(),
            lines,
            cursor_row: p.cursor.saturating_sub(p.top),
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

        // `<CR>` selects the current line: record it for the server to dispatch
        // to the scripting `on_select` handler. Only for select-enabled panels,
        // so a stale handler can't fire on a built-in `:messages` viewer.
        if key.code == KeyCode::Enter {
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

    /// Handle `:set {options}`. Each whitespace-separated token is a boolean
    /// option with the usual `no`/`inv` prefixes and `!`/`?` suffixes (e.g.
    /// `:set number relativenumber`, `:set nonu`, `:set rnu!`).
    fn ex_set(&mut self, args: &str) {
        for tok in args.split_whitespace() {
            match resolve_set(tok) {
                Some((name, op)) => self.apply_set(name, op),
                None => self.echo(format!("E518: Unknown option: {tok}")),
            }
        }
    }

    /// Apply one resolved `:set` operation to the named (canonical) option.
    fn apply_set(&mut self, name: &str, op: SetOp) {
        let slot = match name {
            "number" => &mut self.options.number,
            "relativenumber" => &mut self.options.relativenumber,
            "ignorecase" => &mut self.options.ignorecase,
            "smartcase" => &mut self.options.smartcase,
            "wrapscan" => &mut self.options.wrapscan,
            "hlsearch" => &mut self.options.hlsearch,
            "incsearch" => &mut self.options.incsearch,
            _ => return,
        };
        match op {
            SetOp::On => *slot = true,
            SetOp::Off => *slot = false,
            SetOp::Toggle => *slot = !*slot,
            SetOp::Query => {
                let on = *slot;
                let label = if on {
                    name.to_string()
                } else {
                    format!("no{name}")
                };
                self.echo(label);
            }
        }
    }

    /// Width in cells of the line-number column, `0` when no number option is
    /// on. Sized like vim: at least 4 cells, widening to fit the buffer's
    /// largest line number plus one trailing space.
    pub(crate) fn number_width(&self) -> usize {
        if !self.options.number && !self.options.relativenumber {
            return 0;
        }
        let digits = digit_count(self.buffer().line_count());
        (digits + 1).max(4)
    }

    // ----- undo / redo ------------------------------------------------------

    /// Capture the current text + cursor as an undo/redo snapshot.
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.buffer().text.clone(),
            cursor: self.cursor,
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
        self.cursor = snap.cursor;
        self.buffer_mut().mark_resync();
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

    /// Virtual (screen) column of the cursor on its current line.
    fn cursor_virtcol(&self) -> usize {
        let s = self.buffer().line(self.cursor.line);
        unicode::virtcol(&s, self.cursor.col, unicode::TABSTOP)
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
            unicode::byte_at_virtcol(&s, self.desired_col, unicode::TABSTOP).min(max_byte)
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
    }

    // ----- pending-state bookkeeping ---------------------------------------

    fn effective_count(&self) -> usize {
        self.op_count.unwrap_or(1) * self.count.unwrap_or(1)
    }

    fn reset_pending(&mut self) {
        self.count = None;
        self.op_count = None;
        self.operator = None;
        self.gpending = false;
        self.pending_replace = false;
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
