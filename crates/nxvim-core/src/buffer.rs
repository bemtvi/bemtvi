//! Text buffers, backed by a rope.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::Result;
use ropey::{LineType, Rope};

use crate::host::{FileStat, HostFs};

/// The line-break convention nxvim tracks. `LF_CR` recognizes both Unix (`\n`)
/// and DOS (`\r\n`) breaks, so files of either `fileformat` split into lines
/// correctly. (Available via ropey's default `metric_lines_lf_cr` feature.)
const LINE_TYPE: LineType = LineType::LF_CR;

/// A single content mutation, recorded so out-of-process consumers (the
/// treesitter syntax worker) can reparse **incrementally**. The byte offsets and
/// `(row, byte-column)` points are exactly tree-sitter's `InputEdit` shape: a
/// region `[start_byte, old_end_byte)` of the previous text became
/// `[start_byte, new_end_byte)` after the edit. `text` is the inserted bytes
/// (empty for a pure deletion), which lets a shadow buffer apply the same change
/// without re-sending the whole file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferEdit {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    /// `(row, byte-column)` at `start_byte`, before the edit.
    pub start_point: (usize, usize),
    /// `(row, byte-column)` at `old_end_byte`, before the edit.
    pub old_end_point: (usize, usize),
    /// `(row, byte-column)` at `new_end_byte`, after the edit.
    pub new_end_point: (usize, usize),
    /// Bytes inserted at `start_byte` (`""` for a deletion).
    pub text: String,
}

/// The set of mutations a [`Buffer`] accumulated since the last drain, plus a
/// `resync` flag set when the whole rope was replaced (undo/redo, `:e`) — a case
/// where sending deltas isn't worth it and the consumer should re-sync from the
/// full text instead.
#[derive(Debug, Clone, Default)]
pub struct EditBatch {
    pub edits: Vec<BufferEdit>,
    pub resync: bool,
}

impl EditBatch {
    /// Nothing changed since the last drain (no deltas and no resync).
    pub fn is_empty(&self) -> bool {
        self.edits.is_empty() && !self.resync
    }
}

// The buffer's last-seen on-disk snapshot is a [`crate::host::FileStat`],
// captured at [`Buffer::from_file`] and refreshed after every successful
// [`Buffer::write`]. Comparing the live filesystem against it ([`Buffer::disk_changed`])
// tells whether something *other* than this editor touched the file, so `:w` can
// refuse to clobber an outside edit (vim's `b_mtime` / `b_orig_size` pair). The
// stat itself is taken through the injected [`HostFs`], not `std::fs`.

/// A text buffer.
///
/// Indices are **byte offsets** into the underlying UTF-8 (ropey 2.0's native
/// metric — and the same model vim uses for columns). Invariant: the rope
/// always ends with a trailing `\n`, so an empty buffer is `"\n"` (a single
/// empty editable line) and the number of *editable* lines is
/// `rope.len_lines() - 1`; the final phantom line is never edited or displayed.
///
/// Mutations go through [`Buffer::insert`] / [`Buffer::remove`] (and
/// [`Buffer::insert_char`]) rather than touching `text` directly, so every change
/// is journaled as a [`BufferEdit`] for incremental treesitter reparsing and
/// `changedtick` is kept current.
pub struct Buffer {
    pub text: Rope,
    pub path: Option<PathBuf>,
    pub modified: bool,
    /// Buffer-local options (indentation: `tabstop`/`shiftwidth`/`expandtab`),
    /// independent per buffer so two files can indent differently. Set through
    /// `:set`/`:setlocal` or `vim.bo`, read by the editor when it renders tabs
    /// and inserts indentation.
    pub options: crate::options::BufferOptions,
    /// Monotonic change counter (neovim's `b:changedtick`), bumped on every
    /// mutation. Lets a consumer cheaply tell whether the buffer changed.
    pub changedtick: u64,
    /// Monotonic write counter, bumped on every successful [`Buffer::write`]
    /// (`:w`). The save analogue of `changedtick`: a consumer mirrors it to tell,
    /// without heuristics, exactly when the buffer was saved (drives LSP
    /// `didSave`; a hook future `BufWritePost` autocmds can read too).
    pub save_tick: u64,
    /// Journal of edits since the last [`Buffer::take_edits`] (the treesitter
    /// worker's stream).
    edits: Vec<BufferEdit>,
    /// Set when the entire rope was replaced (undo/redo/reload), so deltas are
    /// meaningless and a full re-sync is required.
    resync: bool,
    /// A **second**, independent edit journal drained by [`Buffer::take_lsp_edits`]
    /// for the LSP `didChange` stream. The treesitter worker and the LSP client
    /// consume edits at different rates — the worker coalesces while a parse is in
    /// flight, the LSP client emits a `didChange` every frame — so one destructive
    /// journal would let whichever drains first (the syntax sync runs first) starve
    /// the other, freezing the language server's copy of the document. Recording
    /// each edit into both journals keeps the two drain cursors independent.
    lsp_edits: Vec<BufferEdit>,
    /// `resync` for the LSP journal (whole-rope replacement: undo/redo/reload).
    lsp_resync: bool,
    /// A **third** independent edit journal drained by
    /// [`Buffer::take_lua_ts_edits`], feeding the Lua `vim.treesitter` platform's
    /// `nvim_buf_attach` `on_bytes` channel (the second, plugin-facing parser a
    /// `get_parser` call builds — distinct from the native highlight engine, which
    /// drains [`Buffer::take_edits`]). Kept separate for the same reason `lsp_edits`
    /// is: the consumers drain at different moments (the native engine on the sync
    /// editor path, the Lua parser on the async server side before any Lua runs), so
    /// one destructive journal would let whichever drains first starve the other and
    /// silently corrupt the Lua-side incremental tree.
    lua_ts_edits: Vec<BufferEdit>,
    /// `resync` for the Lua-treesitter journal (whole-rope replacement → the Lua
    /// `LanguageTree` must fully reparse via `on_reload`, not edit its trees).
    lua_ts_resync: bool,
    /// A **fourth** edit journal, drained by [`Buffer::take_jump_edits`], feeding
    /// the editor's per-window jumplist line-adjustment: a `<C-o>` target on a line
    /// pushed down/up by an edit above it must move with the text (vim's
    /// `mark_adjust`). The jumplist lives on the window (not the buffer), so it
    /// can't ride [`Buffer::record`]'s `shift_marks` like the buffer-local marks do;
    /// instead the editor drains these points and shifts the entries itself. Kept
    /// separate from the other journals for the usual reason — independent drain
    /// timing must not let one consumer starve another.
    jump_edits: Vec<BufferEdit>,
    /// Buffer-anchored extmarks (highlight-layering marks set via
    /// `nvim_buf_set_extmark`), partitioned by namespace. Their byte anchors are
    /// shifted on every edit through [`Buffer::record`] and dropped wholesale on
    /// [`Buffer::mark_resync`]. See [`crate::extmark`].
    pub extmarks: crate::extmark::ExtmarkStore,
    /// Buffer-local marks `a`–`z` (`m{a-z}` set, `` `{x} `` / `'{x}` jump), each a
    /// `(line, byte-column)` position. They live on the buffer — not the window —
    /// so they follow it across switches, matching vim. Like [`extmarks`], they are
    /// kept current through the single edit choke point [`Buffer::record`]: a line
    /// inserted/deleted above a mark shifts its line, text inserted/deleted earlier
    /// in its line shifts its column, and deleting the marked line drops the mark
    /// (so a later jump fails loudly rather than landing somewhere stale). Dropped
    /// wholesale on [`Buffer::mark_resync`] and restored from the undo snapshot on
    /// undo/redo, exactly as `extmarks` are. (Routing/validation lives in
    /// [`crate::editor::marks`]; global `A`–`Z` marks live on the editor.)
    pub marks: HashMap<char, (usize, usize)>,
    /// The **change list** — positions of the changes made in this buffer, oldest
    /// first, navigated with `g;` (older) / `g,` (newer) and listed by `:changes`.
    /// Per-buffer like vim's, and like the buffer-local marks it rides
    /// [`Buffer::record`]: each entry shifts as later edits move its line, and an
    /// entry whose line is deleted is dropped. Coalesced by line — editing the same
    /// line repeatedly keeps one entry at the latest column — and capped at 100. Its
    /// head is the `` `. `` last-change mark. Reset by [`Buffer::mark_resync`] and
    /// snapshotted/restored across undo with [`marks`](Buffer::marks).
    pub changelist: Vec<(usize, usize)>,
    /// The `g;`/`g,` navigation pointer into [`changelist`](Buffer::changelist):
    /// `changelist.len()` means "at the newest change / not navigating"; a new
    /// change resets it there.
    pub changelistidx: usize,
    /// When `Some(dir)`, this buffer is a **directory listing** — nxvim's
    /// in-window file explorer (vim's netrw), not an editable text file. `dir` is
    /// the canonical absolute path being listed; the editor routes its keys through
    /// the `explorer` keymap bucket to
    /// [`crate::editor::Editor::apply_explorer_action`] (open the entry / go up) and
    /// otherwise keeps the listing inert. Built by [`Buffer::from_dir`]; `None`
    /// for every ordinary file/scratch buffer.
    pub dir: Option<PathBuf>,
    /// When `true`, this buffer hosts a **terminal job** — its lines mirror a live
    /// PTY child's screen (and scrollback), pushed in by the server's terminal
    /// engine via [`crate::editor::Editor::terminal_update`], not editable text.
    /// It is non-file (a `:w` refuses, `modified` never sets, no disk backing); in
    /// [`crate::mode::Mode::Terminal`] keystrokes are forwarded to the child, and in
    /// Normal mode the buffer reads as ordinary read-only text for scroll / yank.
    /// The PTY itself lives server-side keyed by this buffer's `BufferId`. `false`
    /// for every ordinary file / scratch / directory buffer.
    pub terminal: bool,
    /// A terminal-job buffer's display name — the child's window title (the OSC
    /// `\e]0;…`/`\e]2;…` sequence a shell or program sets, e.g. `user@host: ~/dir` or
    /// `vim README.md`), surfaced as the buffer name in the statusline. Seeded from the
    /// spawned command at [`crate::editor::Editor::open_terminal`] and updated as the
    /// child changes it. `None` for every non-terminal buffer.
    pub terminal_title: Option<String>,
    /// When `true`, this buffer is an **image opened for preview** (`'imagepreview'`):
    /// it is bound to an image file but its bytes are deliberately *not* read into
    /// the rope — the client renders the picture instead (see
    /// [`Buffer::from_image_file`] and [`crate::view::WindowView::image`]). The rope
    /// stays the empty `"\n"`, so the buffer reads as empty/inert. `false` for every
    /// ordinary file / scratch / directory / terminal buffer.
    pub image: bool,
    /// The file as last seen on disk (mtime + size), captured on read and
    /// refreshed on each successful [`Buffer::write`]. Drives
    /// [`Buffer::disk_changed`], which lets the editor refuse to overwrite a file
    /// that changed underneath us. `None` until we observe an on-disk file (a
    /// scratch buffer, or a `:e new-file` whose target doesn't exist yet).
    disk: Option<FileStat>,
}

impl Default for Buffer {
    fn default() -> Self {
        Buffer::empty()
    }
}

impl Buffer {
    pub fn empty() -> Self {
        Buffer {
            text: Rope::from_str("\n"),
            path: None,
            modified: false,
            options: crate::options::BufferOptions::default(),
            changedtick: 0,
            save_tick: 0,
            edits: Vec::new(),
            resync: false,
            lsp_edits: Vec::new(),
            lsp_resync: false,
            lua_ts_edits: Vec::new(),
            lua_ts_resync: false,
            jump_edits: Vec::new(),
            changelist: Vec::new(),
            changelistidx: 0,
            extmarks: crate::extmark::ExtmarkStore::default(),
            marks: HashMap::new(),
            dir: None,
            terminal: false,
            terminal_title: None,
            image: false,
            disk: None,
        }
    }

    /// An empty buffer bound to `path` without touching the filesystem — the
    /// fallback for a file that exists but can't be read (a directory, a
    /// permission error, invalid UTF-8). Preserving the name means a later `:w`
    /// targets the file the user asked for rather than a stray scratch buffer.
    pub fn named(path: impl Into<PathBuf>) -> Self {
        Buffer {
            path: Some(path.into()),
            ..Buffer::empty()
        }
    }

    /// Bind (or clear) the buffer's file path without touching its contents.
    ///
    /// The usual ways a buffer gets a name are [`Buffer::from_file`] and `:w {path}`
    /// ([`Buffer::write`]). This is the setter for the *in-memory* open/save paths
    /// that have no filesystem to go through — notably the browser/WASM build, where
    /// the File System Access API supplies the bytes and the name out of band.
    pub fn set_path(&mut self, path: Option<PathBuf>) {
        self.path = path;
    }

    /// Mark the buffer as matching its backing store: clear `modified` and pin the
    /// save point (`save_tick`) at the current change. This is the state right after
    /// a load or a save — `[+]` clears and any later `disk_changed` check has a
    /// baseline. (A normal `:w` does this inside [`Buffer::write`]; the in-memory
    /// open/save paths call it directly, since their I/O happens elsewhere.)
    pub fn mark_clean(&mut self) {
        self.modified = false;
        self.save_tick = self.changedtick;
    }

    /// Load a buffer from `path`, decoding its bytes by trying `fileencodings` (the
    /// editor's `'fileencodings'`) in order — see [`crate::encoding::decode_to_rope`].
    /// A missing file yields an empty buffer bound to that path (written on first
    /// save), matching `vim file-that-does-not-exist`. A file whose bytes aren't
    /// valid UTF-8 (or any non-UTF-8 file) no longer fails: it decodes through the
    /// `fileencodings` fallback chain, opens, and carries the detected
    /// `'fileencoding'` / `'bomb'` so `:w` reproduces its original bytes.
    pub fn from_file(path: impl AsRef<Path>, fs: &dyn HostFs, fileencodings: &str) -> Result<Self> {
        use std::io::Read;
        let path = path.as_ref();
        let mut options = crate::options::BufferOptions::default();
        let mut text = if fs.exists(path) {
            // Read the raw bytes, then decode through the shared seam — the same
            // decoder every read path (local, daemon, wasm) funnels through, so a
            // file opens identically however it's reached. (This forfeits the old
            // `from_reader` 1x-memory streaming open for ~2x peak at open; correct
            // multi-encoding decoding needs the whole byte stream in hand.)
            let mut bytes = Vec::new();
            fs.open_read(path)?.read_to_end(&mut bytes)?;
            let (decoded, fileencoding, bomb) =
                crate::encoding::decode_to_rope(&bytes, fileencodings);
            options.fileencoding = fileencoding;
            options.bomb = bomb;
            Rope::from_str(&decoded)
        } else {
            Rope::new()
        };
        ensure_trailing_newline(&mut text);
        // Record what's on disk right now (mtime + size), so a later `:w` can tell
        // if the file changed underneath us. `None` for a not-yet-existing file.
        let disk = fs.stat(path);
        Ok(Buffer {
            text,
            path: Some(path.to_path_buf()),
            modified: false,
            options,
            changedtick: 0,
            save_tick: 0,
            disk,
            edits: Vec::new(),
            resync: false,
            lsp_edits: Vec::new(),
            lsp_resync: false,
            lua_ts_edits: Vec::new(),
            lua_ts_resync: false,
            jump_edits: Vec::new(),
            changelist: Vec::new(),
            changelistidx: 0,
            extmarks: crate::extmark::ExtmarkStore::default(),
            marks: HashMap::new(),
            dir: None,
            terminal: false,
            terminal_title: None,
            image: false,
        })
    }

    /// Open `path` as an **image preview** buffer ([`crate::options::Options::imagepreview`]):
    /// capture its on-disk snapshot (for the status line / change detection) but do
    /// **not** read its bytes into the rope — an image is shown as a picture by the
    /// client, never as text. The result is a valid, empty, unmodified buffer bound
    /// to `path` (so the status line names it and a stray `:w` has a target), flagged
    /// [`image`](Buffer::image) so the window projects an [`crate::view::ImageView`].
    /// A missing file still opens (empty, bound), matching [`Buffer::from_file`].
    pub fn from_image_file(path: impl AsRef<Path>, fs: &dyn HostFs) -> Result<Self> {
        let path = path.as_ref();
        Ok(Buffer {
            image: true,
            disk: fs.stat(path),
            ..Buffer::named(path)
        })
    }

    /// Build a read-only **directory listing** buffer for `path` — the in-window
    /// file explorer nxvim opens when asked to edit a directory (vim's netrw).
    /// The lines are a `../` up-entry followed by the directory's entries sorted
    /// directories-first then case-insensitively by name, each directory suffixed
    /// with `/`. The buffer carries `dir: Some(canonical path)` so the editor
    /// routes navigation keys to the explorer instead of editing it, and the same
    /// path as its `path` so its name shows the directory (matching netrw).
    /// Errors only when the directory can't be read (e.g. no permission); an empty
    /// directory yields just the `../` line.
    pub fn from_dir(path: impl AsRef<Path>, fs: &dyn HostFs) -> Result<Self> {
        let path = path.as_ref();
        // Canonicalize so going up (`../`) and descending (`join`) are
        // unambiguous however the path was spelled (`.`, a relative dir, a
        // symlink). Fall back to the given path if it can't be resolved.
        let dir = fs.canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let entries = fs.read_dir(&dir)?;
        Ok(Self::from_dir_entries(dir, entries))
    }

    /// Build the directory-listing buffer for `dir` from an already-fetched, unsorted
    /// entry list — the [`HostFs`]-free core of [`Buffer::from_dir`]. The daemon /
    /// edit-host split (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` → Phase 3g)
    /// reads a *remote* directory off the editor tick (over `HostFsAsync`, not the sync
    /// [`HostFs`]) and hands the entries here, so the listing is built the same way
    /// whether the directory was read from local disk or across the wire. `dir` is taken
    /// as the canonical path (the caller canonicalized it — locally via [`HostFs`], or on
    /// the daemon side of the wire), so `../`/`join` navigation is unambiguous.
    pub fn from_dir_entries(dir: PathBuf, entries: Vec<crate::host::DirEntry>) -> Self {
        let mut entries: Vec<(bool, String)> =
            entries.into_iter().map(|e| (e.is_dir, e.name)).collect();
        // Directories first, then case-insensitive by name (netrw's default sort).
        entries.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
        });
        let mut text = String::from("../\n");
        for (is_dir, name) in entries {
            text.push_str(&name);
            if is_dir {
                text.push('/');
            }
            text.push('\n');
        }
        Buffer {
            text: Rope::from_str(&text),
            path: Some(dir.clone()),
            dir: Some(dir),
            terminal: false,
            terminal_title: None,
            modified: false,
            options: crate::options::BufferOptions::default(),
            changedtick: 0,
            save_tick: 0,
            edits: Vec::new(),
            resync: false,
            lsp_edits: Vec::new(),
            lsp_resync: false,
            lua_ts_edits: Vec::new(),
            lua_ts_resync: false,
            jump_edits: Vec::new(),
            changelist: Vec::new(),
            changelistidx: 0,
            extmarks: crate::extmark::ExtmarkStore::default(),
            marks: HashMap::new(),
            image: false,
            // A directory listing is never written back to disk, so it needs no
            // change tracking.
            disk: None,
        }
    }

    /// Number of editable lines (excludes the phantom final line).
    pub fn line_count(&self) -> usize {
        self.text.len_lines(LINE_TYPE).saturating_sub(1)
    }

    /// Contents of editable line `idx`, without its trailing newline.
    pub fn line(&self, idx: usize) -> String {
        self.line_cow(idx).into_owned()
    }

    /// Borrow editable line `idx` (without its trailing newline) as a `&str`
    /// when the line occupies a single contiguous rope chunk — the
    /// overwhelmingly common case — allocating only for the rare line that
    /// straddles a chunk boundary. Prefer this over [`line`](Self::line) on hot
    /// paths (grapheme walks, motions) where the line is only read.
    pub fn line_cow(&self, idx: usize) -> Cow<'_, str> {
        if idx >= self.line_count() {
            return Cow::Borrowed("");
        }
        let sl = self.text.line(idx, LINE_TYPE);
        match sl.as_str() {
            Some(s) => Cow::Borrowed(strip_eol(s)),
            None => Cow::Owned(strip_eol(&sl.to_string()).to_owned()),
        }
    }

    /// Number of bytes in editable line `idx`, excluding the newline. Computed
    /// straight from the rope slice's length (O(log n), no allocation).
    pub fn line_len(&self, idx: usize) -> usize {
        if idx >= self.line_count() {
            return 0;
        }
        let sl = self.text.line(idx, LINE_TYPE);
        let mut n = sl.len();
        if n > 0 && matches!(sl.get_char(n - 1), Ok('\n')) {
            n -= 1;
            if n > 0 && matches!(sl.get_char(n - 1), Ok('\r')) {
                n -= 1;
            }
        }
        n
    }

    /// Byte offset at the start of editable line `idx`.
    pub fn line_start(&self, idx: usize) -> usize {
        self.text.line_to_byte_idx(idx, LINE_TYPE)
    }

    /// Editable line containing byte offset `byte_idx`.
    pub fn byte_to_line(&self, byte_idx: usize) -> usize {
        self.text.byte_to_line_idx(byte_idx, LINE_TYPE)
    }

    /// Byte offset for `(line, col)`, where `col` is a byte offset within the line.
    pub fn byte_at(&self, line: usize, col: usize) -> usize {
        self.line_start(line) + col
    }

    pub fn len_bytes(&self) -> usize {
        self.text.len()
    }

    /// All editable lines as owned strings (used by the API `get_lines`).
    pub fn lines(&self) -> Vec<String> {
        (0..self.line_count()).map(|i| self.line(i)).collect()
    }

    // ----- tracked mutations ------------------------------------------------

    /// Insert `s` at byte offset `byte`, journaling the edit and bumping
    /// `changedtick`. The single insertion choke point.
    pub fn insert(&mut self, byte: usize, s: &str) {
        if s.is_empty() {
            return;
        }
        let start_point = self.point_at(byte);
        self.text.insert(byte, s);
        let new_end_byte = byte + s.len();
        let new_end_point = self.point_at(new_end_byte);
        self.record(BufferEdit {
            start_byte: byte,
            old_end_byte: byte,
            new_end_byte,
            start_point,
            old_end_point: start_point,
            new_end_point,
            text: s.to_string(),
        });
    }

    /// Insert a single character at byte offset `byte`.
    pub fn insert_char(&mut self, byte: usize, c: char) {
        let mut buf = [0u8; 4];
        self.insert(byte, c.encode_utf8(&mut buf));
    }

    /// Remove the byte range `range`, journaling the edit and bumping
    /// `changedtick`. The single removal choke point.
    pub fn remove(&mut self, range: Range<usize>) {
        if range.start >= range.end {
            return;
        }
        let start_point = self.point_at(range.start);
        let old_end_point = self.point_at(range.end);
        let (start, end) = (range.start, range.end);
        self.text.remove(range);
        self.record(BufferEdit {
            start_byte: start,
            old_end_byte: end,
            new_end_byte: start,
            start_point,
            old_end_point,
            new_end_point: start_point,
            text: String::new(),
        });
    }

    /// Mark that the whole rope was replaced (undo/redo, file reload), so any
    /// pending deltas are moot and the consumer must re-sync from full text. All
    /// three edit journals (syntax, LSP, and Lua-treesitter) are reset, so none
    /// send stale deltas.
    pub fn mark_resync(&mut self) {
        self.edits.clear();
        self.lsp_edits.clear();
        self.lua_ts_edits.clear();
        // The jumplist journal is moot too: positions against the old rope can't be
        // shifted into the new one. The editor clears jumplist entries for a
        // resync'd buffer on its own (mirroring marks), so just drop the deltas.
        self.jump_edits.clear();
        self.resync = true;
        self.lsp_resync = true;
        self.lua_ts_resync = true;
        // Byte anchors are meaningless against the wholesale-new rope, and an
        // extmark has no source of truth to rebuild from (unlike the treesitter
        // / LSP journals, which re-derive from the full text), so drop them all —
        // matching neovim losing extmarks on a destructive reload.
        self.extmarks.clear_all();
        // Marks are byte/line positions against the old rope, just as meaningless
        // against the wholesale-new one. Undo/redo restore them from the snapshot
        // captured with the history point (see the editor's `restore`); a genuine
        // reload (`:e!`) has nothing to restore from, so the marks are simply gone,
        // matching vim clearing them on a destructive reload.
        self.marks.clear();
        // The change list's positions are against the old rope too; drop them. Undo
        // restores them from the snapshot (see the editor's `restore_snapshot`); a
        // genuine reload has nothing to restore, matching vim.
        self.changelist.clear();
        self.changelistidx = 0;
        self.changedtick += 1;
        self.modified = true;
    }

    /// Drain the treesitter edit journal accumulated since the last call. The
    /// returned batch is `resync` if the whole rope was replaced in the interim.
    pub fn take_edits(&mut self) -> EditBatch {
        EditBatch {
            edits: std::mem::take(&mut self.edits),
            resync: std::mem::replace(&mut self.resync, false),
        }
    }

    /// Drain the **LSP** edit journal — the independent `didChange` stream
    /// (parallel to [`Buffer::take_edits`], so the syntax sync draining first can
    /// no longer starve the language server's view of the document).
    pub fn take_lsp_edits(&mut self) -> EditBatch {
        EditBatch {
            edits: std::mem::take(&mut self.lsp_edits),
            resync: std::mem::replace(&mut self.lsp_resync, false),
        }
    }

    /// Drain the **Lua-treesitter** edit journal — the independent `on_bytes`
    /// stream feeding the `vim.treesitter` platform parser (parallel to
    /// [`Buffer::take_edits`] / [`Buffer::take_lsp_edits`]). A `resync` batch means
    /// the Lua `LanguageTree` should fully reparse (`on_reload`) rather than edit
    /// its trees with now-meaningless deltas.
    pub fn take_lua_ts_edits(&mut self) -> EditBatch {
        EditBatch {
            edits: std::mem::take(&mut self.lua_ts_edits),
            resync: std::mem::replace(&mut self.lua_ts_resync, false),
        }
    }

    /// Drain the **jumplist** edit journal — the line-adjustment stream the editor
    /// applies to per-window `<C-o>` targets (parallel to the others). `resync` is
    /// irrelevant here (a resync'd buffer's entries are cleared outright), so this
    /// returns the raw edits.
    pub fn take_jump_edits(&mut self) -> Vec<BufferEdit> {
        std::mem::take(&mut self.jump_edits)
    }

    fn record(&mut self, edit: BufferEdit) {
        self.extmarks
            .shift(edit.start_byte, edit.old_end_byte, edit.new_end_byte);
        shift_marks(
            &mut self.marks,
            edit.start_point,
            edit.old_end_point,
            edit.new_end_point,
        );
        // The automatic `'.'` mark — vim's "position of the last change" — rides
        // this same store as the named marks, so it shifts with later edits and is
        // restored on undo for free. It lands where the edit began, which is the
        // last inserted/changed character for a per-keystroke insert. The phantom
        // trailing-newline maintenance `insert` is *not* a user change, so
        // `normalize` saves and restores `'.'` around it (see there).
        self.marks.insert('.', edit.start_point);
        // The change list rides edits the same way: shift the existing entries, then
        // record this change's position as its new head. `normalize` saves/restores
        // it around the phantom-newline insert, exactly as it does `'.'`.
        shift_changelist(
            &mut self.changelist,
            edit.start_point,
            edit.old_end_point,
            edit.new_end_point,
        );
        add_to_changelist(&mut self.changelist, edit.start_point);
        self.changelistidx = self.changelist.len();
        self.edits.push(edit.clone());
        self.lsp_edits.push(edit.clone());
        self.jump_edits.push(edit.clone());
        self.lua_ts_edits.push(edit);
        self.changedtick += 1;
        self.modified = true;
    }

    /// `(row, byte-column)` of byte offset `byte` — tree-sitter's `Point` shape,
    /// where the column is a byte offset within the row.
    fn point_at(&self, byte: usize) -> (usize, usize) {
        let row = self.byte_to_line(byte);
        let col = byte - self.line_start(row);
        (row, col)
    }

    /// Re-establish the trailing-newline invariant after a mutation. The inserted
    /// `\n` is journaled like any edit, so a shadow buffer stays byte-identical.
    pub fn normalize(&mut self) {
        let n = self.text.len();
        // The maintenance insert below is bookkeeping, not a user edit, so it must
        // not claim the `'.'` last-change mark `record` sets. Save it and restore it
        // across the insert; the phantom newline is always at the buffer's end,
        // after every real mark, so it never needs to *shift* one.
        let saved_dot = self.marks.get(&'.').copied();
        // The maintenance insert must not enter the change list either: snapshot it
        // (and its nav pointer) and put it back afterward, as we do for `'.'`.
        let saved_changelist = (self.changelist.clone(), self.changelistidx);
        if n == 0 {
            self.insert(0, "\n");
        } else if self.text.get_char(n - 1).map(|c| c != '\n').unwrap_or(true) {
            self.insert(n, "\n");
        } else {
            return;
        }
        match saved_dot {
            Some(p) => self.marks.insert('.', p),
            None => self.marks.remove(&'.'),
        };
        (self.changelist, self.changelistidx) = saved_changelist;
    }

    /// Write the buffer to `path` (or its bound path). Returns `(bytes, lines)` where
    /// `bytes` is the **encoded** on-disk byte count (which differs from the rope
    /// length for any non-UTF-8 `'fileencoding'` — e.g. ~2x for utf-16, +3 for a
    /// utf-8 BOM). Encoding happens *before* the file is touched, so an unrepresentable
    /// character aborts the write loudly with the file left untouched (no NCR-mangled
    /// half-write).
    pub fn write(&mut self, path: Option<PathBuf>, fs: &dyn HostFs) -> Result<(usize, usize)> {
        let target = path
            .or_else(|| self.path.clone())
            .ok_or_else(|| anyhow::anyhow!("E32: No file name"))?;
        let bytes = self.to_save_bytes()?;
        fs.write_atomic(&target, &bytes)?;
        let lines = self.line_count();
        // Re-stat the file we just wrote so its mtime/size become the new baseline
        // for [`disk_changed`]: after a save, *we* are what's on disk, so the very
        // next `:w` shouldn't think the file changed underneath us.
        self.disk = fs.stat(&target);
        self.path = Some(target);
        self.modified = false;
        // Only on a successful write, so a consumer mirroring `save_tick` sees a
        // save exactly when the bytes reached disk (a failed write is no save).
        self.save_tick += 1;
        Ok((bytes.len(), lines))
    }

    /// The exact bytes [`Buffer::write`] would persist — the rope (including its
    /// maintained trailing newline) encoded back to the buffer's `'fileencoding'`,
    /// with the BOM re-emitted when `'bomb'` is set. **Fails loud** (`E513`) on a
    /// character the target encoding can't represent rather than silently corrupting
    /// the file (see [`crate::encoding::encode_from_str`]). For an *off-core* write that
    /// pushes the bytes elsewhere (the daemon save path snapshots them at command time
    /// and sends them over the wire; the browser writes via the File System Access API),
    /// paired with [`Buffer::mark_written`] once the write lands.
    pub fn to_save_bytes(&self) -> Result<Vec<u8>> {
        crate::encoding::encode_from_str(
            &self.text.to_string(),
            self.options.fileencoding,
            self.options.bomb,
        )
    }

    /// Record a completed *external* write of this buffer to `path` (the in-buffer
    /// half of [`Buffer::write`], minus the I/O): bind the name, stamp `stat` as the
    /// new [`disk_changed`](Buffer::disk_changed) baseline so the next check doesn't
    /// false-positive on our own write, clear `[+]`, and bump `save_tick`. The daemon
    /// save path calls this on the ack — never optimistically at send time — so the
    /// saved-state reflects bytes that are actually on the remote.
    pub fn mark_written(&mut self, path: PathBuf, stat: Option<FileStat>) {
        self.disk = stat;
        self.path = Some(path);
        self.modified = false;
        self.save_tick += 1;
    }

    /// The disk snapshot (mtime+size) from the last read/write, or `None` for a
    /// buffer with no file on disk (scratch, or a `:e new-file` not yet written).
    /// The server keys its per-buffer file watch on `(path, this)` so the watch
    /// re-arms whenever the file we track changes identity (a load/reload/save).
    pub fn disk_stat(&self) -> Option<FileStat> {
        self.disk
    }

    /// Whether the bound file changed on disk since nxvim last read or wrote it —
    /// i.e. something *other* than this buffer touched it. Re-stats the file and
    /// compares its mtime/size against the snapshot from the last read/write.
    ///
    /// A buffer with no path (scratch) never reports changed. A `None`↔`Some`
    /// transition counts: a file that was deleted, or one that appeared where the
    /// buffer expected none (a `:e new-file` whose name another process then
    /// created), both register — in each case a blind `:w` would clobber.
    pub fn disk_changed(&self, fs: &dyn HostFs) -> bool {
        !matches!(self.disk_change(fs), DiskChange::Unchanged)
    }

    /// Classify how the bound file's on-disk state compares to the snapshot from
    /// the last read/write — the richer form of [`disk_changed`](Buffer::disk_changed)
    /// that `:checktime` needs to pick between a reload, a warning, and "file gone".
    /// A path-less (scratch) buffer is always [`DiskChange::Unchanged`].
    pub fn disk_change(&self, fs: &dyn HostFs) -> DiskChange {
        let Some(path) = self.path.as_deref() else {
            return DiskChange::Unchanged;
        };
        match fs.stat(path) {
            stat if stat == self.disk => DiskChange::Unchanged,
            // Snapshot said the file existed (or was just written); now it doesn't.
            None => DiskChange::Vanished,
            // Present but its mtime/size no longer match our snapshot.
            Some(_) => DiskChange::Changed,
        }
    }
}

/// How a buffer's bound file on disk compares to the snapshot nxvim took when it
/// last read or wrote it. Drives `:checktime`'s reload-vs-warn decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskChange {
    /// The file's mtime/size still match the snapshot — nothing touched it.
    Unchanged,
    /// The file is still present but was modified by something other than us.
    Changed,
    /// The file the buffer was bound to no longer exists on disk.
    Vanished,
}

/// Strip a single trailing line break (`\n` or `\r\n`) from `s`, leaving any
/// other characters — including interior or leading `\r` — untouched.
fn strip_eol(s: &str) -> &str {
    match s.strip_suffix('\n') {
        Some(rest) => rest.strip_suffix('\r').unwrap_or(rest),
        None => s,
    }
}

fn ensure_trailing_newline(text: &mut Rope) {
    let n = text.len();
    if n == 0 {
        text.insert_char(0, '\n');
    } else if text.get_char(n - 1).map(|c| c != '\n').unwrap_or(true) {
        text.insert_char(n, '\n');
    }
}

/// Shift every buffer-local mark for an edit that replaced the region from point
/// `s` to `oe` with new content ending at point `ne` (the `(row, byte-column)`
/// triple of one [`BufferEdit`]). Called from the single edit choke point
/// [`Buffer::record`], so marks stay correct across every edit path — the same
/// arrangement `extmarks` use.
///
/// Marks are line-oriented, so the rule is expressed on `(row, col)` points
/// rather than bare byte gravity:
/// - A mark **before** the edit (`pos < s`) is untouched.
/// - A mark **at or after** the edit's old end (`pos >= oe`) slides by the edit's
///   row delta, and — only if it sat on the old-end row — by the column delta too.
///   This covers a line inserted/deleted above (row shifts) and text
///   inserted/deleted earlier in the mark's own line (column shifts), and gives a
///   pure insertion *at* the mark right-gravity (the mark rides after inserted
///   text), matching vim.
/// - A mark **inside** the replaced region (`s <= pos < oe`) is dropped when its
///   line is swallowed (`oe.row > pos.row`: a newline at or past the mark's line
///   was deleted, so the line itself is gone — e.g. `dd` on the marked line);
///   otherwise the edit is confined to the mark's line (`cc`, a within-line
///   delete) and the mark collapses to the edit's start rather than vanishing.
fn shift_marks(
    marks: &mut HashMap<char, (usize, usize)>,
    s: (usize, usize),
    oe: (usize, usize),
    ne: (usize, usize),
) {
    if s == oe && oe == ne {
        return; // no-op edit (e.g. an empty insert/remove that still records)
    }
    marks.retain(|_, pos| match shift_point(*pos, s, oe, ne) {
        Some(p) => {
            *pos = p;
            true
        }
        None => false,
    });
}

/// Shift one `(row, byte-col)` position across an edit that replaced the byte
/// range `[s, oe)` with text ending at `ne` (tree-sitter `Point`s). Returns the
/// new position, or `None` when the position sat on a line the edit deleted (so
/// the owner — a mark, a jumplist entry — should be dropped). This is the single
/// rule behind both [`shift_marks`] and the editor's jumplist line-adjustment, so
/// marks and `<C-o>` targets ride edits identically (vim's `mark_adjust`).
pub(crate) fn shift_point(
    pos: (usize, usize),
    s: (usize, usize),
    oe: (usize, usize),
    ne: (usize, usize),
) -> Option<(usize, usize)> {
    if pos < s {
        // entirely before the edit — unaffected
        Some(pos)
    } else if pos >= oe {
        let row = (pos.0 as isize + (ne.0 as isize - oe.0 as isize)) as usize;
        let col = if pos.0 == oe.0 {
            (pos.1 as isize + (ne.1 as isize - oe.1 as isize)) as usize
        } else {
            pos.1
        };
        Some((row, col))
    } else if oe.0 > pos.0 {
        // the position's whole line was deleted (a newline beyond it went) — drop it
        None
    } else {
        // within-line edit covering the position's column — collapse to the edit start
        Some(s)
    }
}

/// vim's `JUMPLISTSIZE` applies to the change list too: at most 100 entries.
const CHANGELIST_SIZE: usize = 100;

/// Shift every change-list entry across an edit, dropping any whose line the edit
/// deleted — the per-buffer analogue of [`shift_marks`], using the same rule so a
/// change position tracks its text exactly like a mark does.
fn shift_changelist(
    list: &mut Vec<(usize, usize)>,
    s: (usize, usize),
    oe: (usize, usize),
    ne: (usize, usize),
) {
    if s == oe && oe == ne {
        return;
    }
    list.retain_mut(|pos| match shift_point(*pos, s, oe, ne) {
        Some(p) => {
            *pos = p;
            true
        }
        None => false,
    });
}

/// Add a change at `pos` to the change list (vim's `add_to_changelist`). A change
/// on the same line as the newest entry just updates that entry's column rather
/// than piling up (so typing a word leaves one entry, not one per keystroke); any
/// other line appends. The list is capped at [`CHANGELIST_SIZE`], dropping the
/// oldest on overflow.
fn add_to_changelist(list: &mut Vec<(usize, usize)>, pos: (usize, usize)) {
    if let Some(last) = list.last_mut() {
        if last.0 == pos.0 {
            *last = pos;
            return;
        }
    }
    if list.len() >= CHANGELIST_SIZE {
        list.remove(0);
    }
    list.push(pos);
}
