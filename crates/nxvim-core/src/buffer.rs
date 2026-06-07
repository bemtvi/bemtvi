//! Text buffers, backed by a rope.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::Result;
use ropey::{LineType, Rope};

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
            extmarks: crate::extmark::ExtmarkStore::default(),
            marks: HashMap::new(),
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

    /// Load a buffer from `path`. A missing file yields an empty buffer bound to
    /// that path (written on first save), matching `vim file-that-does-not-exist`.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let mut text = if path.exists() {
            // Stream the file straight into the rope rather than reading it into
            // an intermediate `String` first. `from_reader` pulls through a small
            // fixed buffer, so peak memory at open is ~1x the file size (just the
            // rope) instead of ~2x (transient `String` + rope). It still validates
            // UTF-8 and errors on invalid input, matching `read_to_string`.
            Rope::from_reader(std::io::BufReader::new(std::fs::File::open(path)?))?
        } else {
            Rope::new()
        };
        ensure_trailing_newline(&mut text);
        Ok(Buffer {
            text,
            path: Some(path.to_path_buf()),
            modified: false,
            options: crate::options::BufferOptions::default(),
            changedtick: 0,
            save_tick: 0,
            edits: Vec::new(),
            resync: false,
            lsp_edits: Vec::new(),
            lsp_resync: false,
            extmarks: crate::extmark::ExtmarkStore::default(),
            marks: HashMap::new(),
        })
    }

    /// Number of editable lines (excludes the phantom final line).
    pub fn line_count(&self) -> usize {
        self.text.len_lines(LINE_TYPE).saturating_sub(1)
    }

    /// Contents of editable line `idx`, without its trailing newline.
    pub fn line(&self, idx: usize) -> String {
        if idx >= self.line_count() {
            return String::new();
        }
        let mut s = self.text.line(idx, LINE_TYPE).to_string();
        if s.ends_with('\n') {
            s.pop();
            if s.ends_with('\r') {
                s.pop();
            }
        }
        s
    }

    /// Number of bytes in editable line `idx`, excluding the newline.
    pub fn line_len(&self, idx: usize) -> usize {
        self.line(idx).len()
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
    /// pending deltas are moot and the consumer must re-sync from full text. Both
    /// edit journals (syntax and LSP) are reset, so neither sends stale deltas.
    pub fn mark_resync(&mut self) {
        self.edits.clear();
        self.lsp_edits.clear();
        self.resync = true;
        self.lsp_resync = true;
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
        self.edits.push(edit.clone());
        self.lsp_edits.push(edit);
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
    }

    /// Write the buffer to `path` (or its bound path). Returns `(bytes, lines)`.
    pub fn write(&mut self, path: Option<PathBuf>) -> Result<(usize, usize)> {
        let target = path
            .or_else(|| self.path.clone())
            .ok_or_else(|| anyhow::anyhow!("E32: No file name"))?;
        let contents = self.text.to_string();
        std::fs::write(&target, &contents)?;
        let lines = self.line_count();
        self.path = Some(target);
        self.modified = false;
        // Only on a successful write, so a consumer mirroring `save_tick` sees a
        // save exactly when the bytes reached disk (a failed write is no save).
        self.save_tick += 1;
        Ok((contents.len(), lines))
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
    marks.retain(|_, pos| {
        if *pos < s {
            // entirely before the edit — unaffected
            true
        } else if *pos >= oe {
            let row = (pos.0 as isize + (ne.0 as isize - oe.0 as isize)) as usize;
            let col = if pos.0 == oe.0 {
                (pos.1 as isize + (ne.1 as isize - oe.1 as isize)) as usize
            } else {
                pos.1
            };
            *pos = (row, col);
            true
        } else if oe.0 > pos.0 {
            // the mark's whole line was deleted (a newline beyond it went) — drop it
            false
        } else {
            // within-line edit covering the mark column — collapse to the edit start
            *pos = s;
            true
        }
    });
}
