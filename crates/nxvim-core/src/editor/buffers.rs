//! Buffer lifecycle (open/switch/alternate) and the buffer-list ex-commands
//! (`:buffers`/`:b`/`:bnext`/`:bdelete`/…).

use super::*;
use crate::buffer::{Buffer, BufferKind, DiskChange, EditBatch};
use crate::host::FileStat;
use crate::mode::Mode;
use std::path::Path;

/// Default height (rows) of a bottom-window scratch listing (`:messages`,
/// `:registers`, …) — matches `:copen`'s default and the old bottom panel.
pub(crate) const LISTING_HEIGHT: usize = 10;

/// A buffer write the editor **deferred** off the keystroke tick — the daemon /
/// edit-host save path (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` →
/// Phase 3e). In a daemon session core does *not* write through the synchronous
/// [`HostFs`](crate::HostFs) (that would block the one editor thread on the
/// network); instead `:w` snapshots the buffer into one of these at command time
/// and the orchestration layer (the server) pushes the bytes over the wire,
/// finalizing the buffer's saved-state only on the daemon's ack via
/// [`Editor::finalize_save`]. The server drains the queue with
/// [`Editor::take_pending_saves`].
pub struct PendingSave {
    /// Monotonic id minted per enqueue, so the server can correlate an ack back to
    /// its request and keep one buffer's overlapping writes ordered.
    pub seq: u64,
    /// The buffer being saved. The ack targets *this* buffer specifically, so a
    /// finalize lands correctly even if the user switched away while it was in flight.
    pub buffer: BufferId,
    /// The resolved write target: the `:w {name}` argument, else the buffer's bound
    /// path. (`mark_written` binds the buffer to this on the ack, as `:w` does.)
    pub path: PathBuf,
    /// The bytes snapshotted at command time — so edits made while the write is in
    /// flight can never tear into what gets persisted.
    pub bytes: Vec<u8>,
    /// Line count of the snapshot, for the `"{name}" {lines}L, {bytes}B written`
    /// echo the server emits on the ack (vim reports what was *written*, not the
    /// buffer's possibly-since-edited current state).
    pub lines: usize,
    /// A quit to replay once this write acks: `Some(bang)` for `:wq` / `:x` (run
    /// `:q` / `:q!`), `None` for a plain `:w`. The editor defers the quit until the
    /// bytes are safely on the remote — an unflushed write is never silently
    /// abandoned by an exiting editor.
    pub then_quit: Option<bool>,
}

/// A `:wqa` / `:xa` quit the editor **deferred** until every write of a multi-buffer
/// `:wall` batch has acked — the all-buffers-ack-then-quit machinery of the daemon
/// save path (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` → Phase 3, the fs
/// leg's multi-buffer write slice). The single-buffer `:wq` rides [`PendingSave::then_quit`]
/// (one save, one `:q`); a batch quit can't, because it must wait for *all* of the
/// batch's writes — so core hands the server this set of seqs to watch and the server
/// fires `:qa` only once every one has acked (and **cancels** the quit if any fails,
/// exactly as the single-buffer `:wq` does). The server drains it with
/// [`Editor::take_pending_quit_all`].
pub struct PendingQuitAll {
    /// The `:qa!` bang — force-quit (discard any *other* still-modified buffer) once the
    /// batch acks, vs. `:qa`'s `E37` guard. Carried verbatim from the `:wqa!` / `:xa!`.
    pub bang: bool,
    /// The [`PendingSave::seq`] of every write this `:wqa` enqueued. The server removes
    /// each as it acks; when the set empties it replays the quit. Empty would mean
    /// "nothing to save" — but core quits directly in that case and never enqueues this.
    pub seqs: Vec<u64>,
}

/// A buffer open the editor **deferred** off the keystroke tick — `:edit` over the
/// daemon wire (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` → Phase 3f). In a
/// daemon session core does *not* read through the synchronous [`HostFs`](crate::HostFs)
/// (that would block the one editor thread on the network); `:edit` creates an empty
/// buffer named for the file, switches to it, and enqueues one of these. The server
/// fetches the bytes over `HostFsAsync` off-tick and fills the buffer with
/// [`Editor::load_str_into`] — the read companion to [`PendingSave`].
pub struct PendingOpen {
    /// The (already-created, empty) buffer to fill once the fetch lands. Targeted by id
    /// so the load is correct even if the user switched away while it was in flight.
    pub buffer: BufferId,
    /// The file to fetch — the `:edit {path}` argument (or the reloaded buffer's path).
    pub path: PathBuf,
}

/// Why a file-backed buffer's on-disk state changed, as reported to a
/// `FileChangedShell` handler through `v:fcs_reason` (and the warning
/// [`Editor::warn_file_change`] echoes when no handler redirects it). A subset of
/// neovim's reasons — nxvim's stat snapshot is mtime+size, so it can't distinguish
/// `"mode"` / `"time"` from `"changed"`, and collapses them into `Changed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChangeReason {
    /// The file the buffer was bound to no longer exists (`v:fcs_reason = "deleted"`).
    Deleted,
    /// The file changed on disk **and** the buffer has unsaved edits — a true
    /// conflict, never silently clobbered (`v:fcs_reason = "conflict"`).
    Conflict,
    /// The file changed on disk and the buffer is unmodified, but it was not
    /// autoreloaded (`'noautoread'`) (`v:fcs_reason = "changed"`).
    Changed,
}

impl FileChangeReason {
    /// The `v:fcs_reason` string neovim exposes to a `FileChangedShell` handler.
    pub fn as_str(self) -> &'static str {
        match self {
            FileChangeReason::Deleted => "deleted",
            FileChangeReason::Conflict => "conflict",
            FileChangeReason::Changed => "changed",
        }
    }
}

/// What [`Editor::begin_file_change`] decided about a file-change reconcile — the
/// hand-off to the server, which owns the `FileChangedShell` Lua round-trip the pure
/// core can't drive.
pub enum FileChangeAction {
    /// The file is unchanged — nothing to do.
    None,
    /// The buffer was silently autoreloaded (`'autoread'`, unmodified). No
    /// `FileChangedShell` fires (neovim reloads before the autocmd), but the server
    /// still fires `FileChangedShellPost`.
    Reloaded,
    /// The change needs the `FileChangedShell` round-trip: the server sets
    /// `v:fcs_reason` to this reason, fires the autocmd, and honors `v:fcs_choice`
    /// (`"reload"`/`"edit"` → [`Editor::reload_buffer`]; `"ask"`/none →
    /// [`Editor::warn_file_change`]).
    Autocmd(FileChangeReason),
}

impl Editor {
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
            "foldnestmax" => ob.buffer.options.foldnestmax = value.max(1) as usize,
            "foldminlines" => ob.buffer.options.foldminlines = value.max(0) as usize,
            _ => return,
        }
        // `shiftwidth`/`tabstop` change the indent scale and the two `fold*` knobs the
        // computed structure, so a buffer the focused window shows must re-fold.
        self.refresh_folds();
    }

    /// Set a boolean buffer-local option (`expandtab`, `ts_highlight`) on buffer
    /// `id`. The boolean companion to [`Editor::set_buffer_option_num`].
    pub fn set_buffer_option_bool(&mut self, id: BufferId, name: &str, value: bool) {
        // `ts_highlight` is the treesitter-enable noun (`nx.bo.ts_highlight`); it
        // lives in the per-buffer enable map, not an `options` slot, so route it
        // through the dedicated setter (which also drops/restores the parse).
        if name == "ts_highlight" {
            if self.buffers.map.contains_key(&id) {
                self.set_ts_highlight(id, value);
            }
            return;
        }
        let Some(ob) = self.buffers.map.get_mut(&id) else {
            return;
        };
        if name == "expandtab" {
            ob.buffer.options.expandtab = value;
        } else if name == "autoindent" {
            ob.buffer.options.autoindent = value;
        } else if name == "smartindent" {
            ob.buffer.options.smartindent = value;
        } else if name == "autopairs" {
            ob.buffer.options.autopairs = value;
        } else if name == "bomb" {
            ob.buffer.options.bomb = value;
        } else if name == "modifiable" {
            ob.buffer.options.modifiable = value;
        } else if name == "modified" {
            // vim's `:set [no]modified` (`vim.bo.modified = …`): set or clear the
            // change flag directly. Clearing it is how a plugin that fills a buffer as
            // a *read* (a `BufReadCmd` listing) tells the editor "this is the file's
            // content, not an unsaved edit", so the buffer shows no `[+]` and doesn't
            // block `:q` with E37 — the `nx.view` `set_view_lines` clear, reached via
            // the public option surface for a plugin-filled ordinary buffer.
            ob.buffer.modified = value;
        }
    }

    /// Set a string buffer-local option (`regexsyntax` / `fileencoding`) on buffer
    /// `id` — the `vim.bo` string companion to [`Editor::set_buffer_option_num`]. An
    /// unknown option, an unknown buffer, or an invalid value (a `regexsyntax` other
    /// than `"pcre"`/`"vim"`, an unknown `fileencoding` label) is ignored; the
    /// `:set` ex path is where a bad value fails loud (`E474`), so a raw `vim.bo`
    /// write of garbage leaves the option untouched.
    pub fn set_buffer_option_str(&mut self, id: BufferId, name: &str, value: &str) {
        // `filetype` is the treesitter language noun (`nx.bo.filetype`); route it
        // through `set_filetype` (which refreshes the parse). `""` = no filetype.
        if name == "filetype" {
            if self.buffers.map.contains_key(&id) {
                self.set_filetype(id, value);
            }
            return;
        }
        // `commentstring` is the comment-operator template, stored as a per-buffer
        // override beside the filetype (not a `BufferOptions` slot). Empty clears
        // it, falling back to the filetype default.
        if name == "commentstring" {
            if self.buffers.map.contains_key(&id) {
                self.set_commentstring(id, value);
            }
            return;
        }
        // `foldexpr` is likewise a per-buffer string (not a `Copy` `BufferOptions`
        // slot). `set_foldexpr` operates on the focused buffer; only forward a write
        // for that buffer (a non-focused write is a no-op here, matching the
        // focused-window fold model). Rebuilds the structure.
        if name == "foldexpr" {
            if id == self.current_buffer_id() {
                self.set_foldexpr(value);
            }
            return;
        }
        // `foldmarker` is the `foldmethod=marker` delimiter pair, likewise a
        // per-buffer string (not a `Copy` `BufferOptions` slot). Like `foldexpr`,
        // `set_foldmarker` operates on the focused buffer, so only a write for that
        // buffer applies (the focused-window fold model). A malformed value (not a
        // `start,end` pair) is ignored here — the `:set` ex path is the loud one;
        // an empty value resets to vim's default markers.
        if name == "foldmarker" {
            if id == self.current_buffer_id() {
                let parts: Vec<&str> = value.split(',').collect();
                if value.is_empty() {
                    self.reset_foldmarker();
                } else if parts.len() == 2
                    && !parts[0].is_empty()
                    && !parts[1].is_empty()
                    && parts[0] != parts[1]
                {
                    self.set_foldmarker(parts[0], parts[1]);
                }
            }
            return;
        }
        let Some(ob) = self.buffers.map.get_mut(&id) else {
            return;
        };
        if name == "regexsyntax" {
            ob.buffer.options.regexsyntax = match value {
                "pcre" => crate::options::RegexSyntax::Pcre,
                "vim" => crate::options::RegexSyntax::Vim,
                _ => return,
            };
        } else if name == "fileencoding" {
            // Like `regexsyntax`: a raw `vim.bo` write of an unknown label is
            // ignored (the buffer keeps its encoding); the `:set` ex path is where
            // a bad value fails loud (E474). Empty means UTF-8.
            ob.buffer.options.fileencoding = if value.is_empty() {
                crate::encoding::Encoding::UTF8
            } else {
                match crate::encoding::Encoding::from_label(value) {
                    Some(e) => e,
                    None => return,
                }
            };
        } else if name == "fileformat" {
            // The line-ending convention (`nx.bo.fileformat = "dos"`), the `vim.bo`
            // companion to the enumerated `:set fileformat` path. An unknown label is
            // ignored, like `fileencoding` above.
            ob.buffer.options.fileformat = match crate::options::FileFormat::from_label(value) {
                Some(ff) => ff,
                None => return,
            };
        } else if name == "foldmethod" {
            // `nx.bo.foldmethod = "indent"`, the `vim.bo` companion to the enumerated
            // `:set foldmethod` path. An unknown or not-yet-implemented value is
            // ignored here (the `:set` ex path is the loud one); a recognized value
            // rebuilds the fold structure for the new source.
            match crate::options::FoldMethod::from_label(value) {
                Ok(fdm) => ob.buffer.options.foldmethod = fdm,
                Err(_) => return,
            }
            self.refresh_folds();
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

    /// Drain buffer `id`'s **Lua-treesitter** edit journal — the byte-delta stream
    /// the server projects into neovim's `on_bytes` tuples for the Lua side (so it
    /// can reparse incrementally instead of re-reading the whole snapshot).
    /// Parallel to [`Editor::take_lsp_edits_of`]; `None` if no such buffer is open.
    pub fn take_lua_ts_edits_of(&mut self, id: BufferId) -> Option<EditBatch> {
        self.buffers
            .map
            .get_mut(&id)
            .map(|ob| ob.buffer.take_lua_ts_edits())
    }

    /// All open buffer ids, ascending (the `nvim_list_bufs` order). Global across
    /// every layer — the neovim API lists *all* buffers regardless of which dock or
    /// the main area they live in.
    pub fn buffer_ids(&self) -> Vec<BufferId> {
        self.buffers.map.keys().copied().collect()
    }

    /// Open buffer ids that belong to `layer`, ascending. The buffer list is
    /// **per-layer**: each buffer's home is the window layer it was last shown in
    /// (`OpenBuffer::layer`), so a dock's buffers and the main area's buffers form
    /// disjoint lists. Backs the focused-layer `:ls` and the same-layer close
    /// fallback.
    pub(crate) fn buffers_in_layer(&self, layer: Layer) -> Vec<BufferId> {
        self.buffers
            .map
            .iter()
            // A plugin view (`nx.view`: a diff pane, a file tree, …) is a surface, not a
            // document — like the panel buffers below, it never appears in `:ls` or in
            // `:bnext`/`:bprev`/… navigation, so a closed view can't be cycled back into.
            .filter(|(_, ob)| ob.layer == layer && ob.buffer.view_id().is_none())
            // Panel display buffers (`[Messages]`, `[Buffers]`, …) are surfaces, not
            // documents: they never appear in `:ls` or in `:bnext`/`:bprev`/… navigation
            // (which all funnel through here). `:lspanels` lists them instead.
            .filter(|(id, _)| !self.is_panel_buffer(**id) && !self.is_doc_float_buffer(**id))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Record that buffer `id` now lives in `layer` (its window layer). A no-op if
    /// `id` is not an open buffer.
    pub(crate) fn set_buffer_layer(&mut self, id: BufferId, layer: Layer) {
        if let Some(ob) = self.buffers.map.get_mut(&id) {
            ob.layer = layer;
        }
    }

    /// Open buffer ids that belong to the **focused** layer, ascending — the
    /// per-region buffer list that `:ls` shows and the `nx.buf.list{ focused = true }`
    /// Lua API exposes. (The global [`Editor::buffer_ids`] lists every layer.)
    pub fn focused_buffer_ids(&self) -> Vec<BufferId> {
        self.buffers_in_layer(self.focused_layer)
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

    /// Replace lines `[start, end)` of buffer `id` with `replacement` — the
    /// buffer-addressed core of `nvim_buf_set_lines`, and the SOLE buffer-*text*
    /// mutation the `nx.*` Lua API reaches (queued as [`BufOp::SetLines`] and applied
    /// after the chunk). `start`/`end` are 0-based, end-exclusive, and already resolved
    /// (negatives folded, bounds clamped) by the Lua front against the live line count;
    /// they are re-clamped here for safety.
    ///
    /// The edit is one independently-undoable group ([`Editor::push_undo_for`]) routed
    /// through the [`Buffer::insert`] / [`Buffer::remove`] chokepoints — so `changedtick`,
    /// the treesitter / LSP / on-bytes journals, and the buffer mirror all follow it —
    /// and [`Buffer::normalize`]d to keep the rope's single trailing `\n` invariant.
    ///
    /// Fails loud (`Err`) rather than silently no-op when the buffer is gone or read-only
    /// (a live terminal, directory listing, `nx.view`, or quickfix display, or an ordinary
    /// `nomodifiable` buffer) — vim's `E21`. The Lua wrapper rejects the promise with it.
    pub fn api_set_lines(
        &mut self,
        id: BufferId,
        start: usize,
        end: usize,
        replacement: &[String],
    ) -> Result<(), String> {
        // Resolve the byte span under an immutable borrow, then drop it before mutating.
        let (from, to) = {
            let buf = match self.buffer_of(id) {
                Some(b) => b,
                None => return Err(format!("invalid buffer id: {}", id.0)),
            };
            if buf.read_only() || !buf.options.modifiable {
                return Err("E21: Cannot make changes, 'modifiable' is off".to_string());
            }
            let count = buf.line_count();
            let start = start.min(count);
            let end = end.clamp(start, count);
            let from = buf.line_start(start);
            // Through the line *after* the range, or the whole rope (trailing `\n`
            // included) when the range runs to the last line — mirrors `ex_delete`.
            let to = if end >= count {
                buf.len_bytes()
            } else {
                buf.line_start(end)
            };
            (from, to)
        };
        // A quickfix / location-list display buffer is read-only too, but its identity
        // is an editor-side registry rather than a `Buffer` flag (checked separately).
        if self.qf_context_of_buffer(id).is_some() {
            return Err("E21: Cannot make changes, 'modifiable' is off".to_string());
        }

        // Each replacement line becomes a whole line: join with `\n` plus a trailing one
        // so the splice can't merge the last line into the following kept line (an empty
        // replacement is a pure deletion). `normalize` restores the single trailing `\n`.
        let chunk = if replacement.is_empty() {
            String::new()
        } else {
            let mut s = replacement.join("\n");
            s.push('\n');
            s
        };

        self.push_undo_for(id);
        let buf = self
            .buffer_of_mut(id)
            .expect("buffer existed under the borrow above");
        buf.remove(from..to);
        if !chunk.is_empty() {
            buf.insert(from, &chunk);
        }
        buf.normalize();
        buf.modified = true;
        // Keep the focused cursor in bounds when the edit shrank its own buffer (vim
        // adjusts cursors across a set_lines); background windows clamp when refocused.
        if id == self.cur_buffer() {
            self.clamp_cursor();
        }
        Ok(())
    }

    /// Buffer `id`'s `changedtick` (bumped on every edit / resync), or `None` if no
    /// such buffer is open. The server keys its per-buffer highlight memo on this so
    /// an edit invalidates the cached spans for *that* buffer specifically — not just
    /// the focused one.
    pub fn changedtick_of(&self, id: BufferId) -> Option<u64> {
        self.buffers.map.get(&id).map(|ob| ob.buffer.changedtick)
    }

    /// Whether `id` is an open buffer (`nvim_buf_is_valid`). The RPC surface uses
    /// this to reject a client-supplied buffer handle before binding a window to
    /// it — binding a window to a non-existent buffer would make a later
    /// `buffers.get` on it panic and crash the server.
    pub fn buffer_is_valid(&self, id: BufferId) -> bool {
        self.buffers.map.contains_key(&id)
    }

    /// Add a buffer to the store and return its id, without switching to it.
    pub(crate) fn add_buffer(&mut self, buffer: Buffer) -> BufferId {
        let id = self.buffers.insert(buffer);
        // A freshly opened file reattaches any shada-restored marks for its path.
        self.seed_pending_file_marks(id);
        id
    }

    /// Seed buffer `id`'s marks from any shada-restored pending set for its path,
    /// draining that set (one-shot per path). Additive — a mark the running session
    /// already placed on the buffer wins over the restored one. Called wherever a
    /// buffer's path becomes bound (a fresh load, the throwaway-reuse in-place open,
    /// the browser `load_str`, and the restored startup buffer at import), so a
    /// reopened file gets its `a`–`z` / `"` marks back exactly as vim restores them
    /// from shada when the file loads — never eagerly at launch.
    pub(crate) fn seed_pending_file_marks(&mut self, id: BufferId) {
        let Some(path) = self
            .buffers
            .map
            .get(&id)
            .and_then(|ob| ob.buffer.path.clone())
        else {
            return;
        };
        let key = normalize_path(&path);
        // A restored changelist seeds the buffer's `g;`/`g,` history (only when the
        // session hasn't already recorded changes of its own), with the navigation
        // pointer parked at the newest entry.
        if let Some(entries) = self.pending_changelists.remove(&key) {
            let buf = &mut self.buffers.get_mut(id).buffer;
            if buf.changelist.is_empty() {
                buf.changelistidx = entries.len();
                buf.changelist = entries;
            }
        }
        let Some(marks) = self.pending_file_marks.remove(&key) else {
            return;
        };
        let buf = &mut self.buffers.get_mut(id).buffer;
        for (name, pos) in marks {
            buf.marks.entry(name).or_insert(pos);
        }
    }

    /// Open `contents` into the window as if a file named `name` were edited — but
    /// the bytes come from memory, not the filesystem. This is the browser/WASM open
    /// path: the host has no filesystem, so the File System Access API hands us the
    /// file's text and we load it here (the `:e {path}` analogue, minus the disk
    /// read). Reuses the throwaway `[No Name]` buffer like `:e`/`:enew`, then
    /// replaces the text through the normal edit path, binds the name, and resets to
    /// a freshly-read state: unmodified, cursor and scroll at the top.
    pub fn load_str(&mut self, name: Option<String>, contents: &str) {
        if !self.current_is_throwaway() {
            let id = self.add_buffer(Buffer::empty());
            self.switch_buffer(id);
        }
        self.cursor = Cursor::default();
        self.top = 0;
        self.leftcol = 0;

        let ob = self.cur_mut();
        let len = ob.buffer.len_bytes();
        ob.buffer.remove(0..len);
        ob.buffer.insert(0, contents);
        ob.buffer.normalize();
        ob.buffer.set_path(name.map(std::path::PathBuf::from));
        ob.buffer.mark_clean();
        // Freshly loaded text is the new baseline: rebuild the undo tree rooted at it
        // and record that state as saved. Without this the throwaway `[No Name]`
        // buffer's empty-buffer undo root survives the in-place text swap, so the
        // first `u` after an edit reverts the whole file away instead of undoing the
        // edit. Mirrors `load_into_current` (the `:e` read path).
        ob.undo = UndoTree::new(&ob.buffer);
        ob.saved_seq = Some(ob.undo.cur_seq());
        // The freshly named buffer reattaches any shada-restored marks for its path.
        let id = self.cur_buffer();
        self.seed_pending_file_marks(id);
    }

    /// Record that the current buffer was just saved to `name`: bind the name (when
    /// given) and clear the modified flag — the post-`:w` state. The actual write
    /// happens outside core (in the browser build, via the File System Access API),
    /// so this updates only the in-editor bookkeeping.
    pub fn mark_saved(&mut self, name: Option<String>) {
        let buf = self.buffer_mut();
        if let Some(name) = name {
            buf.set_path(Some(std::path::PathBuf::from(name)));
        }
        buf.mark_clean();
    }

    /// Turn on **off-tick filesystem mode**: `:w` (and the write half of `:wq` / `:x`)
    /// snapshots the buffer and enqueues a [`PendingSave`], and `:edit` enqueues a
    /// [`PendingOpen`] fetch, instead of touching the synchronous
    /// [`HostFs`](crate::HostFs). The orchestration layer (the server in a daemon
    /// session) performs the read/write off the editor tick — `finalize_save` on a
    /// write ack, `load_str_into` on a read reply. Off by default — local builds do
    /// buffer I/O synchronously through `host_fs` exactly as before.
    pub fn set_host_fs_offtick(&mut self, on: bool) {
        self.host_fs_offtick = on;
    }

    /// Mirror whether a `BufReadCmd` autocmd handler is registered (the server reads
    /// this from its `au_active_events` cache). When on, a file open is deferred so the
    /// server can fire `BufReadCmd` before the default read — see
    /// [`should_defer_open`](Self::should_defer_open) and the field doc.
    pub fn set_bufreadcmd_active(&mut self, on: bool) {
        self.bufreadcmd_active = on;
    }

    /// Whether a file open should be **deferred** to the server (enqueued as a
    /// [`PendingOpen`]) instead of read inline through [`HostFs`](crate::HostFs). True
    /// in off-tick/daemon mode (the read crosses the wire) *or* when a `BufReadCmd`
    /// handler is registered (the server fires it first, and a handler may claim the
    /// read). The common local config with no `BufReadCmd` handler defers nothing, so
    /// its synchronous read path is unchanged.
    pub(crate) fn should_defer_open(&self) -> bool {
        self.host_fs_offtick || self.bufreadcmd_active
    }

    /// Read `open.path` synchronously through [`HostFs`](crate::HostFs) into
    /// `open.buffer` — the local fill of a deferred open that the server's `BufReadCmd`
    /// fire did **not** claim. Mirrors [`load_into_current`](Self::load_into_current)
    /// (fresh undo rooted at the read, unmodified, syntax re-sync) but targets the
    /// named buffer rather than the current one, and records it as loaded-in-place so
    /// the server re-fires `BufReadPost`/`BufNewFile`/`FileType`. A read error is
    /// echoed, leaving the (empty) buffer in place. A no-op if the buffer was closed
    /// before the drain.
    pub fn load_pending_open(&mut self, open: PendingOpen) {
        let PendingOpen { buffer, path } = open;
        if !self.buffers.map.contains_key(&buffer) {
            return;
        }
        match self.read_buffer(&path) {
            Ok(buf) => {
                let ob = self.buffers.get_mut(buffer);
                ob.buffer = buf;
                ob.undo = UndoTree::new(&ob.buffer);
                ob.saved_seq = Some(ob.undo.cur_seq());
                ob.buffer.mark_resync();
                ob.buffer.modified = false;
                self.loaded_in_place.push(buffer);
                // Land the cursor: a located jump that was waiting on this deferred open
                // goes to its target, else the top (a plain `:edit`).
                self.settle_loaded_cursor(buffer);
            }
            Err(e) => self.echo(e.to_string()),
        }
        self.seed_pending_file_marks(buffer);
    }

    /// Drain the writes the editor deferred this tick (off-tick save mode). The
    /// server takes these after each input, pushes their bytes over the daemon wire,
    /// and finalizes each on its ack. Empty (a cheap no-op) when off-tick mode is off
    /// or no `:w` ran.
    pub fn take_pending_saves(&mut self) -> Vec<PendingSave> {
        std::mem::take(&mut self.pending_saves)
    }

    /// Snapshot the current buffer for an off-tick write to `path` (the `:w` target,
    /// already resolved) and enqueue it. The saved-state is **not** touched here —
    /// `modified`, `save_tick`, and the `disk` baseline change only on the ack
    /// ([`Editor::finalize_save`]), so a write that fails or never acks leaves the
    /// buffer honestly dirty. `then_quit` carries a deferred `:wq` / `:x`. Returns the
    /// minted [`PendingSave::seq`] so a multi-buffer `:wqa` can gate its quit on the
    /// whole batch (the single-buffer `:w` caller ignores it). `None` when the buffer's
    /// text can't be encoded to its `'fileencoding'`: the failure is echoed and nothing
    /// is enqueued (so a `:wq`'s deferred quit never fires — the file is untouched and
    /// the buffer stays honestly dirty), matching the synchronous `:w` fail-loud path.
    pub(crate) fn enqueue_save(&mut self, path: PathBuf, then_quit: Option<bool>) -> Option<u64> {
        self.enqueue_save_of(self.cur_buffer(), path, then_quit)
    }

    /// Snapshot a *specific* buffer for an off-tick write (the multi-buffer `:wall` path,
    /// which writes every modified buffer, not just the current one). The single-buffer
    /// [`Editor::enqueue_save`] is this for `cur_buffer()`. `None` (with the error echoed)
    /// when the buffer can't be encoded to its `'fileencoding'`.
    pub(crate) fn enqueue_save_of(
        &mut self,
        buffer: BufferId,
        path: PathBuf,
        then_quit: Option<bool>,
    ) -> Option<u64> {
        // Encode at snapshot time so an unrepresentable character fails loud *here*,
        // before anything is enqueued — never a silently-mangled write off the tick.
        let (bytes, lines) = {
            let buf = &self.buffers.get(buffer).buffer;
            (buf.to_save_bytes(), buf.line_count())
        };
        let bytes = match bytes {
            Ok(bytes) => bytes,
            Err(e) => {
                self.echo(e.to_string());
                return None;
            }
        };
        let seq = self.next_save_seq;
        self.next_save_seq += 1;
        self.pending_saves.push(PendingSave {
            seq,
            buffer,
            path,
            bytes,
            lines,
            then_quit,
        });
        Some(seq)
    }

    /// Drain the deferred `:wqa` / `:xa` quit (off-tick mode), if one was set this tick.
    /// The server stores the returned seq-set as a gate and replays `:qa` once every
    /// write in it acks. Empty (a cheap `None`) when no batch quit ran.
    pub fn take_pending_quit_all(&mut self) -> Option<PendingQuitAll> {
        self.pending_quit_all.take()
    }

    /// Apply a daemon write ack to the buffer it saved: bind the name, stamp the new
    /// on-disk `stat` baseline, clear `[+]`, bump `save_tick`, and record the written
    /// state as the saved undo node — the deferred half of a synchronous `:w`, run
    /// only once the bytes are confirmed on the remote. A no-op if the buffer was
    /// closed while the write was in flight (nothing to finalize). The server emits
    /// the `written` echo and replays any deferred quit; this only touches core state.
    pub fn finalize_save(&mut self, buffer: BufferId, path: PathBuf, stat: Option<FileStat>) {
        if !self.buffers.map.contains_key(&buffer) {
            return;
        }
        self.buffers
            .get_mut(buffer)
            .buffer
            .mark_written(path.clone(), stat);
        self.mark_undo_saved(buffer);
        // Record the (now-acked) write so the server fires `BufWritePre`/`BufWritePost`,
        // exactly as the synchronous `:w` path does — a daemon save fires the same events.
        self.record_write(buffer, path);
    }

    /// Drain the opens the editor deferred this tick (off-tick mode — `:edit` over the
    /// daemon wire). The server fetches each over `HostFsAsync` and fills the named
    /// buffer with [`Editor::load_str_into`]. Empty (a cheap no-op) when off-tick mode
    /// is off or no `:edit` ran.
    pub fn take_pending_opens(&mut self) -> Vec<PendingOpen> {
        std::mem::take(&mut self.pending_opens)
    }

    /// Whether `buffer` has an open still pending (enqueued but not yet filled — a
    /// deferred `:edit`, off-tick or behind a `BufReadCmd` handler). The server uses
    /// this to hold a freshly-named-but-empty buffer's read lifecycle
    /// (`BufReadPost`/`FileType`/`BufEnter`) until the content actually lands, so those
    /// fire **once**, over the filled buffer, rather than prematurely on the empty one.
    pub fn has_pending_open(&self, buffer: BufferId) -> bool {
        self.pending_opens.iter().any(|o| o.buffer == buffer)
    }

    /// Drain the buffers read from a file *in place* this tick (a local `:edit` that
    /// reused the throwaway `[No Name]` or re-read the current file via `:e` / `:e!`,
    /// keeping the same bufnr). The server clears each from its `announced` /
    /// `fired_filetype` sets before emitting lifecycle events, so the re-read buffer
    /// fires `BufReadPost` (`BufNewFile`) and `FileType` again — neovim fires those on
    /// every read, regardless of whether the buffer id was seen before. Empty (a cheap
    /// no-op) when no in-place read ran this tick.
    pub fn take_loaded_in_place(&mut self) -> Vec<BufferId> {
        std::mem::take(&mut self.loaded_in_place)
    }

    /// Record a completed write of `buffer` to `path` for the server to fire
    /// `BufWritePre` / `BufWritePost` on (the pure core can't drive a Lua autocmd).
    /// Called from the synchronous `:w` / `:wall` write path and from the off-tick
    /// [`Editor::finalize_save`] ack, so a write fires the same events however it
    /// reached disk. A path-less buffer never writes, so this is only ever called
    /// with a real target.
    pub(crate) fn record_write(&mut self, buffer: BufferId, path: PathBuf) {
        self.write_events.push((buffer, path));
    }

    /// Drain the buffers written this tick (a successful `:w` / `:wall` or a finalized
    /// off-tick save), each a `(buffer, path)` the server fires `BufWritePre` /
    /// `BufWritePost` for. Empty (a cheap no-op) when nothing was written.
    pub fn take_write_events(&mut self) -> Vec<(BufferId, PathBuf)> {
        std::mem::take(&mut self.write_events)
    }

    /// Whether any write event is queued — the server's fixpoint loop checks this so a
    /// `:w` driven from an autocmd / user command still fires its write events in the
    /// same convergence.
    pub fn has_write_events(&self) -> bool {
        !self.write_events.is_empty()
    }

    /// Whether `id` is a **new file** — a file-backed buffer (it has a path) whose file
    /// did not exist on disk when it was opened (no disk snapshot). The server fires
    /// `BufNewFile` instead of `BufReadPost` for these, matching `vim file-that-does-not-exist`.
    /// A scratch / `[No Name]` buffer (no path) is not a new file; nor is one read from an
    /// existing file (it has a disk snapshot).
    pub fn buffer_is_new_file(&self, id: BufferId) -> bool {
        self.buffers
            .map
            .get(&id)
            .is_some_and(|ob| ob.buffer.path.is_some() && ob.buffer.disk_stat().is_none())
    }

    /// Mark `id` as read from an *existing* file when the read landed off-tick (daemon /
    /// wasm). Stamp its disk baseline so [`buffer_is_new_file`](Self::buffer_is_new_file)
    /// reports `false` and the server fires `BufReadPost` rather than `BufNewFile`. Pass the
    /// real [`FileStat`](crate::FileStat) when the transport carries one (the daemon stats
    /// the file at read, so the buffer's baseline matches what the watch leg later pushes —
    /// no spurious "changed on disk"); pass `None` when it doesn't (the serverless wasm/OPFS
    /// read, which has no synchronous stat and no watch leg), and a size-only baseline is
    /// synthesized from the just-loaded rope. No-op for a gone or path-less buffer. Call only
    /// for an existing file — a `:e new-file` must keep its `None` stat to fire `BufNewFile`.
    pub fn mark_replica_read_from_disk(&mut self, id: BufferId, stat: Option<crate::FileStat>) {
        let Some(ob) = self.buffers.map.get_mut(&id) else {
            return;
        };
        if ob.buffer.path.is_none() {
            return;
        }
        let stat = stat.unwrap_or(crate::FileStat {
            mtime: None,
            size: ob.buffer.len_bytes() as u64,
        });
        ob.buffer.set_disk_stat(Some(stat));
    }

    /// Enqueue an off-tick fetch of `path` into `buffer` (an already-created, empty
    /// buffer the caller has set up and, for `:edit`, switched to). The server reads
    /// the bytes off the editor tick and loads them into `buffer` via
    /// [`Editor::load_str_into`] — the read analogue of [`Editor::enqueue_save`].
    pub(crate) fn enqueue_open(&mut self, buffer: BufferId, path: PathBuf) {
        // Image preview over off-tick fs (`'imagepreview'`): when previews are on and
        // `path` is an image, do **not** fetch the bytes — the editor never reads an
        // image (never-freeze). Mark the target buffer as an inert preview bound to the
        // path; the client fetches and decodes the bytes out-of-band from the path the
        // redraw carries. Centralized here because every off-tick open/reload funnels
        // through `enqueue_open` (`:edit`, the open kernel, `:e!`/watch reloads), so the
        // policy can't drift between them. (The local sync path mirrors this via
        // `read_buffer` → `Buffer::from_image_file`. Off-tick has no synchronous stat, so
        // the disk version is left unset — the client keys its cache on the path.)
        if self.options.imagepreview && super::is_image_path(Some(&path)) {
            // Stamp the disk baseline (size + mtime the redraw's image marker carries)
            // when we can stat synchronously — a *local* open (every `:edit` now defers
            // through here behind the explorer's `BufReadCmd` handler, so this is the live
            // local image path). Off-tick has no synchronous stat, so it's left unset and
            // the client keys its cache on the path.
            let stat = if self.host_fs_offtick {
                None
            } else {
                self.host_fs.stat(&path)
            };
            if let Some(ob) = self.buffers.map.get_mut(&buffer) {
                let len = ob.buffer.len_bytes();
                if len > 0 {
                    ob.buffer.remove(0..len);
                    ob.buffer.normalize();
                }
                ob.buffer.kind = BufferKind::Image;
                ob.buffer.set_path(Some(path));
                ob.buffer.stamp_disk(stat);
                ob.buffer.modified = false;
                // Bump the preview version so the client re-fetches/re-decodes: off-tick
                // can't stat, so a reopen (`:e`) or a watch-driven reload — both routed
                // here — wouldn't otherwise change the marker, and the client would keep
                // showing the cached picture after the file changed on disk.
                ob.buffer.image_gen = ob.buffer.image_gen.wrapping_add(1);
            }
            return;
        }
        self.pending_opens.push(PendingOpen { buffer, path });
    }

    /// Load `contents` into `buffer` as a freshly-read replica of the file named
    /// `name` — the buffer-targeted form of [`Editor::load_str`], used by the server
    /// when an off-tick fetch (initial open or `:edit`) lands. Replaces the buffer's
    /// text in place (preserving its id), binds the name, marks it unmodified, and
    /// rebuilds the undo tree rooted at the read state (undo cannot cross the read).
    /// When `buffer` is the current one, the window's cursor/scroll reset to the top,
    /// as opening a file does; when it isn't (the user switched away mid-fetch), only
    /// that buffer's content changes and the live window is left untouched. A no-op if
    /// `buffer` was closed before the fetch landed.
    pub fn load_str_into(&mut self, buffer: BufferId, name: Option<String>, contents: &str) {
        if !self.buffers.map.contains_key(&buffer) {
            return;
        }
        let ob = self.buffers.get_mut(buffer);
        let len = ob.buffer.len_bytes();
        ob.buffer.remove(0..len);
        ob.buffer.insert(0, contents);
        ob.buffer.normalize();
        ob.buffer.set_path(name.map(std::path::PathBuf::from));
        // The whole rope was replaced — flag a syntax re-sync (as `load_into_current`
        // does), then root a fresh undo tree at this saved state. `mark_resync` sets
        // `modified` (a rope swap via *editing* is a change), so `mark_clean` must come
        // **after** it: a freshly-read replica is by definition unmodified (the
        // documented contract, and what the watch leg's reload-vs-conflict decision
        // depends on).
        ob.buffer.mark_resync();
        ob.buffer.mark_clean();
        ob.undo = UndoTree::new(&ob.buffer);
        ob.saved_seq = Some(ob.undo.cur_seq());
        self.settle_loaded_cursor(buffer);
    }

    /// Open a built-in read-only **scratch listing** (`:messages`, `:registers`,
    /// `:LspInfo`, …) as a focus-locked bottom **panel** named `name` (see
    /// [`Editor::open_named_panel`]). The listing is an ordinary `nomodifiable` buffer
    /// navigated like any other (motions / search flow through); `q` / `<Esc>` dismiss it
    /// via the `FileType nxlisting` autocmd's buffer-local map. `cursor` is the initially
    /// selected line (0-based, clamped). The named registry buffer is reused, so re-running
    /// the command replaces the content in place.
    pub fn open_scratch_listing(&mut self, name: &str, lines: Vec<String>, cursor: usize) {
        self.open_named_panel(name, lines, cursor, "nxlisting", LISTING_HEIGHT);
    }

    /// `:ls` / `:buffers` — open the buffer list as the `[Buffers]` panel whose `<CR>`
    /// switches to the buffer on the cursor line. Tagged `filetype=nxbuffers`, so the
    /// `FileType nxbuffers` autocmd's buffer-local `<CR>` map (a prelude default) lives only
    /// on this panel and never bleeds onto the plain text listings.
    pub fn open_buffer_listing(&mut self, lines: Vec<String>, cursor: usize) {
        self.open_named_panel("[Buffers]", lines, cursor, "nxbuffers", LISTING_HEIGHT);
    }

    /// Load raw file `bytes` into `buffer` as a freshly-read replica named `name` — the
    /// byte-level counterpart to [`Editor::load_str_into`] used by every *off-tick* read
    /// (daemon / wasm initial open and `:edit`). Decodes through the shared
    /// [`crate::encoding::decode_to_rope`] seam (so a remote file opens identically to a
    /// local one — same `'fileencodings'` detection, same invalid-UTF-8 resilience),
    /// replaces the buffer's text, and records the detected `'fileencoding'` / `'bomb'`
    /// so `:w` reproduces the original bytes. A no-op if `buffer` was closed before the
    /// fetch landed. `load_str_into` is kept for genuinely-already-text callers (scratch
    /// buffers, in-editor swaps).
    pub fn load_bytes_into(&mut self, buffer: BufferId, name: Option<String>, bytes: &[u8]) {
        let (text, fileencoding, bomb) =
            crate::encoding::decode_to_rope(bytes, &self.options.fileencodings);
        self.load_str_into(buffer, name, &text);
        // `load_str_into` no-ops on a closed buffer; mirror its guard before stamping
        // the encoding so a late fetch onto a gone buffer doesn't panic.
        if self.buffers.map.contains_key(&buffer) {
            let ob = self.buffers.get_mut(buffer);
            ob.buffer.options.fileencoding = fileencoding;
            ob.buffer.options.bomb = bomb;
        }
    }

    /// Make `id` the current buffer: stash the outgoing window position with its
    /// buffer, record the alternate (`#`), and restore the incoming buffer's
    /// saved position. A no-op if `id` is already current or not in the store.
    ///
    /// The window always lands in normal mode; transient pending/scroll state is
    /// dropped. Syntax re-sync across the switch is the server's job (it notices
    /// the current-buffer id changed), so this touches neither `modified` nor the
    /// edit journal — switching a buffer must never make it look edited.
    pub(crate) fn switch_buffer(&mut self, id: BufferId) {
        if id == self.cur_buffer() || !self.buffers.map.contains_key(&id) {
            return;
        }
        // A panel buffer only ever shows inside the panel overlay — never as a regular main
        // buffer. Targeting one from *outside* the panel window (`:b [Messages]`, a stray
        // switch) opens it AS a panel instead. The in-panel-window swap is allowed through:
        // it backs both `open_panel`'s own reuse and `:lspanels` navigation (showing the
        // picked panel's last content), so panels always open as panels.
        if self.is_panel_buffer(id) && self.panel_window() != Some(self.windows.current) {
            self.open_panel(id, LISTING_HEIGHT);
            return;
        }
        // Stash the outgoing position with its buffer; it becomes the alternate.
        let (cursor, top, leftcol) = (self.cursor, self.top, self.leftcol);
        let outgoing = self.cur_buffer();
        let out = self.buffers.get_mut(outgoing);
        out.saved_cursor = cursor;
        out.saved_top = top;
        out.saved_leftcol = leftcol;
        // Leaving a buffer records its last-cursor mark `"` (vim's definition), so
        // `` `" `` returns here and shada can persist where each file was left.
        out.buffer.marks.insert('"', (cursor.line, cursor.col));
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
        // A reopened file gets its shada-restored manual folds back when it becomes
        // the focused window's buffer (guarded on the window having no folds yet, so
        // a session's own folds win).
        self.seed_pending_folds();
    }

    /// `<C-^>` — switch to the alternate buffer (`#`), or report `E23` when there
    /// is none (e.g. only one buffer is open).
    pub(crate) fn goto_alternate(&mut self) {
        match self.alternate {
            Some(id) => self.switch_buffer(id),
            None => self.echo("E23: No alternate file"),
        }
    }

    /// `<C-w><C-w>{H,J,K,L}` — move the focused buffer to `target` layer and focus
    /// it there. The source window first falls back to a sibling buffer in its own
    /// layer — the alternate if it's a live same-layer buffer, else the nearest
    /// sibling, else a fresh `[No Name]` — so the moved buffer ends up living only
    /// in `target`. Crossing and showing it there retags its home layer (see
    /// `OpenBuffer::layer`), so the per-layer buffer list (`:ls`) follows it. The
    /// caller guarantees `target` is an open layer.
    pub(crate) fn move_buffer_to_layer(&mut self, target: Layer) {
        let buf = self.cur_buffer();
        let src_layer = self.focused_layer;
        let replacement = self
            .alternate
            .filter(|a| {
                *a != buf
                    && self.buffers.map.contains_key(a)
                    && self.buffers.get(*a).layer == src_layer
            })
            .or_else(|| self.neighbor_of(buf, src_layer));
        match replacement {
            Some(rep) => self.switch_buffer(rep),
            None => {
                let id = self.add_buffer(Buffer::empty());
                self.switch_buffer(id);
            }
        }
        // Cross to the target layer and show `buf` there, restoring its saved view.
        self.switch_layer(target);
        self.switch_buffer(buf);
    }

    /// The id of an already-open buffer bound to `path`, if any. Matches by
    /// cwd-anchored, lexically normalized path (so `./a`, `a`, and the absolute
    /// `<cwd>/a` are the same buffer — the case that makes an absolute LSP path
    /// reuse a buffer opened with a relative name), **without touching the
    /// filesystem** beyond reading the cwd — the pure core never does the blocking
    /// `canonicalize` syscall this used to run on every `:e`. Symlinks are not
    /// resolved, matching vim's path-based (not inode-based) buffer dedup. See
    /// [`super::same_path`] / [`super::absolutize_normalize`].
    pub fn find_buffer_by_path(&self, path: &Path) -> Option<BufferId> {
        let target = super::absolutize_normalize(path);
        self.buffers.map.iter().find_map(|(id, ob)| {
            let stored = ob.buffer.path.as_ref()?;
            (super::absolutize_normalize(stored) == target).then_some(*id)
        })
    }

    /// Does the current buffer hold `path`? The cwd-aware comparison (so an
    /// absolute path and a cwd-relative one for the same file match), used wherever
    /// a caller asks "am I already on this file?" — the `:e` reload guard, the
    /// go-to jump, the LSP location refine. See [`super::same_path`].
    pub fn current_buffer_is(&self, path: &Path) -> bool {
        self.buffer()
            .path
            .as_deref()
            .is_some_and(|p| super::same_path(p, path))
    }

    /// Is the current buffer a throwaway scratch buffer — unnamed, unmodified,
    /// and empty? `:e file` loads into such a buffer in place (vim's behavior),
    /// rather than leaving a stray `[No Name]` behind.
    pub(crate) fn current_is_throwaway(&self) -> bool {
        let b = self.buffer();
        b.path.is_none() && !b.modified && b.line_count() == 1 && b.line(0).is_empty()
    }

    /// Load a buffer for `path`, honoring `'imagepreview'`: an image-extension file
    /// ([`crate::editor::is_image_path`]) opens as an inert **preview** buffer (its
    /// bytes are never read as text — [`Buffer::from_image_file`]) when the option is
    /// on, otherwise as ordinary text ([`Buffer::from_file`]). The shared local-FS
    /// load seam every synchronous open path funnels through. (Off-tick / daemon
    /// opens fetch over the wire and don't preview yet.)
    fn read_buffer(&self, path: &Path) -> anyhow::Result<Buffer> {
        let fs = self.host_fs.clone();
        if self.options.imagepreview && super::is_image_path(Some(path)) {
            Buffer::from_image_file(path, &*fs)
        } else {
            Buffer::from_file(path, &*fs, &self.options.fileencodings)
        }
    }

    /// Replace the current buffer's contents with `path`'s, preserving the buffer
    /// id. Used by `:e` reload-in-place and to reuse a throwaway buffer. The
    /// loaded buffer is unmodified; the whole-content swap is flagged for syntax
    /// re-sync (`mark_resync` bumps `changedtick`, but we keep `modified` clear
    /// because it is freshly read from disk).
    pub(crate) fn load_into_current(&mut self, path: &Path) {
        match self.read_buffer(path) {
            Ok(buf) => {
                self.cursor = Cursor::default();
                self.top = 0;
                self.leftcol = 0;
                let ob = self.cur_mut();
                ob.buffer = buf;
                // Reloaded from disk: discard the old history and start a fresh
                // tree rooted at the reloaded text — a state that is, by
                // definition, saved. Undo cannot cross the reload.
                ob.undo = UndoTree::new(&ob.buffer);
                ob.saved_seq = Some(ob.undo.cur_seq());
                ob.buffer.mark_resync();
                ob.buffer.modified = false;
                // Read in place (same bufnr): record it so the server re-fires
                // `BufReadPost`/`BufNewFile`/`FileType` for this read, just as the
                // off-tick read path does when its fetched bytes land. neovim fires
                // those on every read, even when the buffer id was seen before — so a
                // `:edit` reusing the throwaway `[No Name]` and a `:e!` reload both
                // re-announce.
                let id = self.cur_buffer();
                self.loaded_in_place.push(id);
            }
            Err(e) => self.echo(e.to_string()),
        }
        // Loading a file in place (`:e`, throwaway-reuse) reattaches its shada marks.
        let id = self.cur_buffer();
        self.seed_pending_file_marks(id);
    }

    /// Re-read `buffer` from disk in place — the reload primitive `:checktime`'s
    /// autoread path uses (and the building block the remote-watch slice reuses).
    /// Replaces the rope with the file's current bytes, re-roots the undo tree at
    /// the reloaded (saved) state, refreshes the disk snapshot — so a following
    /// `:checktime` no longer reports a change — and clamps the cursor (the live
    /// one when `buffer` is current, the saved one otherwise) into the new extent
    /// so a now-shorter file can't strand it past the end. On a read failure the
    /// buffer is left untouched and the error echoed. Local-only (synchronous
    /// `host_fs`); the daemon reload is part of the remote-watch slice. `pub` so the
    /// server can drive it from the `FileChangedShell` round-trip (a `v:fcs_choice`
    /// of `"reload"`/`"edit"`).
    pub fn reload_buffer(&mut self, buffer: BufferId) {
        let Some(path) = self.buffers.get(buffer).buffer.path.clone() else {
            return;
        };
        let new_buf = match self.read_buffer(&path) {
            Ok(b) => b,
            Err(e) => {
                self.echo(e.to_string());
                return;
            }
        };
        let is_current = buffer == self.cur_buffer();
        {
            let ob = self.buffers.get_mut(buffer);
            ob.buffer = new_buf;
            // Reloaded from disk: discard the old history and start a fresh tree
            // rooted at the reloaded text, a state that is by definition saved.
            // Undo cannot cross the reload (as `load_into_current` / `:e!` do).
            ob.undo = UndoTree::new(&ob.buffer);
            ob.saved_seq = Some(ob.undo.cur_seq());
            ob.buffer.mark_resync();
            // `mark_resync` sets `modified`; a reload is a fresh read of disk, so clear
            // it back (as `load_into_current` does) — otherwise a reloaded buffer reports
            // dirty and the next reconcile would misread it as a conflict.
            ob.buffer.modified = false;
        }
        if is_current {
            self.clamp_cursor();
            let last = self.last_line();
            self.top = self.top.min(last);
        } else {
            let last = self
                .buffers
                .get(buffer)
                .buffer
                .line_count()
                .saturating_sub(1);
            let ob = self.buffers.get_mut(buffer);
            ob.saved_cursor.line = ob.saved_cursor.line.min(last);
            ob.saved_top = ob.saved_top.min(last);
        }
    }

    /// `:checktime` — re-stat every loaded file-backed buffer (or just `target`)
    /// and reconcile it with what nxvim last read or wrote, mirroring neovim:
    /// an externally-changed but *unmodified* buffer is silently reloaded when
    /// `'autoread'` is on (else a **W11** warning, no reload); a buffer changed on
    /// disk **and** in nxvim is a **W12** conflict (never clobbered); a file that
    /// vanished is **E211**. This is the local behavior the remote `HostWatch`
    /// push (a later slice) triggers over the wire — `:checktime` is both the user
    /// command and the watcher's entry point.
    ///
    /// The reconcile itself is **deferred to the server** (enqueued here, drained by
    /// `Server::run_pending`): firing the `FileChangedShell` autocmd and honoring
    /// `v:fcs_choice` is a synchronous Lua round-trip the pure core can't drive, so
    /// the *decision* lives one layer up. Detection and reload still run in core.
    pub(crate) fn checktime(&mut self, target: &str) {
        // In a daemon session the edit-host can't stat the remote disk off the tick;
        // change detection is the daemon's **always-on** `HostWatch` leg, which pushes
        // every external change for the server to reconcile automatically. So an explicit
        // `:checktime` is redundant here (the watch already covers it) — a no-op, rather
        // than statting the edit-host's *local* disk for a remote path (which would
        // misreport E211).
        if self.host_fs_offtick {
            return;
        }
        let ids = match target.trim() {
            "" => self.buffer_ids(),
            arg => match self.resolve_buffer(arg) {
                Some(id) => vec![id],
                None => return,
            },
        };
        self.pending_checktime.extend(ids);
    }

    /// `:checktime` for a single buffer — the entry point the **watcher** uses (the
    /// server's per-buffer file watch fires this on a change). Enqueues the reconcile
    /// like `:checktime`; a no-op for an unknown buffer or in a daemon session (where
    /// the remote stat arrives via the `HostWatch` server-push leg instead).
    pub fn checktime_buffer(&mut self, id: BufferId) {
        if self.host_fs_offtick || !self.buffers.map.contains_key(&id) {
            return;
        }
        self.pending_checktime.push(id);
    }

    /// Drain the buffers awaiting a `:checktime` reconcile this tick. The server takes
    /// these in `run_pending`, runs detection/reload through the core primitives below,
    /// and fires the `FileChangedShell` round-trip for each. Empty (a cheap no-op) when
    /// no `:checktime` ran and no watch fired.
    pub fn take_pending_checktime(&mut self) -> Vec<BufferId> {
        std::mem::take(&mut self.pending_checktime)
    }

    /// Whether any buffer is awaiting a `:checktime` reconcile — the server's
    /// fixpoint loop checks this so a `FileChangedShell` handler that itself runs
    /// `:checktime` keeps draining instead of stranding the new request.
    pub fn has_pending_checktime(&self) -> bool {
        !self.pending_checktime.is_empty()
    }

    /// Take the pending `<C-w>d` / `<C-w><C-d>` "show diagnostics under the cursor"
    /// request: `true` once after the chord, then cleared. The server drains this in
    /// `run_pending` and opens the diagnostic float — core can't, since the
    /// diagnostic store lives behind the server seam.
    pub fn take_diagnostic_float(&mut self) -> bool {
        std::mem::take(&mut self.pending_diagnostic_float)
    }

    /// Whether buffer `id` has unsaved edits — `false` for an unknown buffer. The
    /// server reads this to classify a remote file change (modified ⇒ a `"conflict"`).
    pub fn buffer_modified(&self, id: BufferId) -> bool {
        self.buffers
            .map
            .get(&id)
            .is_some_and(|ob| ob.buffer.modified)
    }

    /// The global `'autoread'` value — the server consults it on the remote watch leg,
    /// where it can't route through core's (local-disk) [`Editor::begin_file_change`].
    pub fn autoread(&self) -> bool {
        self.options.autoread
    }

    /// Reload the current buffer as an image preview when `'imagepreview'` got
    /// turned on *after* it was already opened as text — the startup file-arg case.
    /// The CLI file is opened at [`Editor`](crate::Editor) construction, before the
    /// user's config runs, so a config that sets `nx.o.imagepreview` can't affect
    /// that first open; the server calls this once after sourcing config to bring it
    /// in line with what `:e %` would now do. A no-op unless previews are on, the
    /// buffer is a known image extension ([`crate::editor::is_image_path`]), and it
    /// isn't already a preview.
    pub fn reconcile_image_preview(&mut self) {
        let b = self.buffer();
        if self.options.imagepreview && !b.is_image() && super::is_image_path(b.path.as_deref()) {
            let id = self.cur_buffer();
            self.reload_buffer(id);
        }
    }

    /// Enqueue an off-tick re-fetch of buffer `id`'s own file into itself — the daemon
    /// reload the remote watch leg drives (the off-tick analogue of
    /// [`Editor::reload_buffer`], which reads the *local* disk synchronously). The
    /// server drains it via [`Editor::take_pending_opens`] and loads the fetched bytes
    /// with [`Editor::load_str_into`]. Returns whether it enqueued — `false` for a
    /// buffer with no path (nothing to re-fetch).
    pub fn enqueue_reload(&mut self, id: BufferId) -> bool {
        let Some(path) = self
            .buffers
            .map
            .get(&id)
            .and_then(|ob| ob.buffer.path.clone())
        else {
            return false;
        };
        self.enqueue_open(id, path);
        true
    }

    /// Detect how a buffer's file changed on disk and apply the part of the reconcile
    /// that needs no autocmd — the first half of neovim's `buf_check_timestamp`. An
    /// externally-changed, *unmodified* buffer under `'autoread'` is silently reloaded
    /// here (neovim reloads *before*, and *without*, firing `FileChangedShell`); every
    /// other change defers to the server's `FileChangedShell` round-trip via the
    /// returned [`FileChangeAction::Autocmd`] reason. [`FileChangeAction::None`] means
    /// nothing changed; [`FileChangeAction::Reloaded`] means the silent autoread reload
    /// ran (the server still fires `FileChangedShellPost`).
    pub fn begin_file_change(&mut self, id: BufferId) -> FileChangeAction {
        if !self.buffers.map.contains_key(&id) {
            return FileChangeAction::None;
        }
        let fs = self.host_fs.clone();
        match self.buffers.get(id).buffer.disk_change(&*fs) {
            DiskChange::Unchanged => FileChangeAction::None,
            // A vanished file never autoreloads — always the round-trip (E211 / a
            // handler's choice). Reason `"deleted"`.
            DiskChange::Vanished => FileChangeAction::Autocmd(FileChangeReason::Deleted),
            DiskChange::Changed => {
                let modified = self.buffers.get(id).buffer.modified;
                if !modified && self.options.autoread {
                    // 'autoread', unmodified, file present: reload silently with no
                    // `FileChangedShell` (matching neovim's pre-autocmd branch).
                    self.reload_buffer(id);
                    FileChangeAction::Reloaded
                } else if modified {
                    FileChangeAction::Autocmd(FileChangeReason::Conflict)
                } else {
                    FileChangeAction::Autocmd(FileChangeReason::Changed)
                }
            }
        }
    }

    /// Echo the default warning for a file change the `FileChangedShell` round-trip did
    /// **not** redirect (no handler, or a handler that left `v:fcs_choice` as `"ask"`):
    /// **E211** (vanished), **W12** (conflict — changed on disk *and* in nxvim), or
    /// **W11** (changed on disk, unmodified buffer, but no autoreload). The reload cases
    /// never reach here (they call [`Editor::reload_buffer`] directly).
    pub fn warn_file_change(&mut self, id: BufferId, reason: FileChangeReason) {
        let name = self.buffer_name(id).unwrap_or_default();
        let msg = match reason {
            FileChangeReason::Deleted => format!("E211: File \"{name}\" no longer available"),
            FileChangeReason::Conflict => format!(
                "W12: Warning: File \"{name}\" has changed and the buffer was changed in Vim as well"
            ),
            FileChangeReason::Changed => {
                format!("W11: Warning: File \"{name}\" has changed since editing started")
            }
        };
        self.echo(msg);
    }

    /// The server's file-watch key for buffer `id`: its on-disk path and the disk
    /// snapshot we last reconciled to, or `None` for a buffer with no file at all (a
    /// scratch / `[No Name]` buffer). The disk snapshot is itself `None` for a
    /// **new-file buffer not yet written** — a path with nothing on disk behind it;
    /// the native watch leg uses that (`disk_stat.is_some()`) to decline arming, since
    /// kqueue/inotify can't watch an absent path and a failed arm would re-arm-spin,
    /// while the daemon leg watches by path regardless (the daemon owns change
    /// detection and dedupes its watch set, so it can't spin). A `:w` re-stamps
    /// `disk_stat`, changing this key, so the native watch arms on the next sync. The
    /// server arms one watch per file-backed buffer and re-arms when this key changes,
    /// so the watch follows the file across reloads/saves (a fresh inode after an
    /// atomic replace gets a fresh watch).
    pub fn buffer_watch_key(&self, id: BufferId) -> Option<(PathBuf, Option<FileStat>)> {
        let ob = self.buffers.map.get(&id)?;
        let path = ob.buffer.path.clone()?;
        Some((path, ob.buffer.disk_stat()))
    }

    /// Load a **fresh** buffer for `path` and return its id — the load atom shared by
    /// every file-open path (`:e`, `:tabnew`, LSP go-to, the explorer). Off-tick aware: in
    /// a daemon session it creates an empty buffer named for `path` and enqueues a
    /// [`PendingOpen`] the server fills over the wire; locally it reads `path`
    /// synchronously via [`Buffer::from_file`]. Does **not** check whether `path` is
    /// already open, switch to it, or place it — callers own placement (current window, a
    /// new tab, …). `None` means a *synchronous* load failed and was already echoed
    /// (off-tick never fails here — the fetch's errors surface later in `apply_open`).
    fn load_new_buffer(&mut self, path: &Path) -> Option<BufferId> {
        if self.should_defer_open() {
            // Deferred (off-tick fetch, or a BufReadCmd handler that may claim the
            // read): create the empty named buffer and enqueue. The server fills it —
            // over the wire off-tick, or via `load_pending_open` locally when no
            // BufReadCmd handler claims it. (When the path is an image preview,
            // `enqueue_open` marks the buffer inert and skips the fetch — the
            // centralized policy for every deferred open path.)
            let id = self.add_buffer(Buffer::named(path.to_path_buf()));
            self.enqueue_open(id, path.to_path_buf());
            Some(id)
        } else {
            match self.read_buffer(path) {
                Ok(buf) => Some(self.add_buffer(buf)),
                Err(e) => {
                    self.echo(e.to_string());
                    None
                }
            }
        }
    }

    /// Find the buffer already open for `path`, or [load](Editor::load_new_buffer) a fresh
    /// one — the find-or-load open kernel. Returns its id **without** switching or placing
    /// it (the caller decides where it goes). Used by `:tabnew` (it hands the id to a new
    /// tab) and the explorer (it switches, then wipes the listing); `:e` / go-to layer
    /// throwaway-reuse on top via [`Editor::edit_in_current_window`]. `None` only on a
    /// synchronous load failure (already echoed).
    pub(crate) fn open_buffer(&mut self, path: &Path) -> Option<BufferId> {
        if let Some(id) = self.find_buffer_by_path(path) {
            return Some(id);
        }
        self.load_new_buffer(path)
    }

    /// Find the buffer already open for `path`, or load a fresh one into the buffer
    /// list **without switching to it** — the find-or-load primitive a *workspace
    /// edit* uses to bring an unopened file's buffer into existence so its edits can
    /// be applied in memory (left modified, persisted by `:wa`, exactly as neovim's
    /// `apply_text_edits` does — it never writes the edit straight to disk).
    ///
    /// Returns `None` (and loads nothing) in a **daemon / off-tick** session: there
    /// the load is an async fetch that would hand back an *empty* buffer to edit into,
    /// so the caller reports the file as unhandled rather than silently corrupting it.
    /// Also `None` on a synchronous local load failure (already echoed).
    ///
    /// Locally the file is read **synchronously**, bypassing the
    /// [`should_defer_open`](Self::should_defer_open) deferral an ordinary
    /// [`open_buffer`](Self::open_buffer) honors. A workspace edit needs the file's
    /// bytes *now* to apply against, and it is not a user `:edit`, so it must not hand
    /// first dibs to a `BufReadCmd` handler (the always-on explorer registers one, yet
    /// it only ever claims *directories* — never a rename target). Deferring would add
    /// an **empty** buffer and enqueue the disk fill for a later tick, so the edit would
    /// land on emptiness and the fill would then clobber it; the direct read keeps the
    /// edit and the file's real contents together.
    pub fn ensure_buffer_loaded(&mut self, path: &Path) -> Option<BufferId> {
        if let Some(id) = self.find_buffer_by_path(path) {
            return Some(id);
        }
        if self.host_fs_offtick {
            return None;
        }
        match self.read_buffer(path) {
            Ok(buf) => Some(self.add_buffer(buf)),
            Err(e) => {
                self.echo(e.to_string());
                None
            }
        }
    }

    /// Off-tick sibling of [`ensure_buffer_loaded`](Self::ensure_buffer_loaded): bring an
    /// unopened file into a buffer whose bytes are fetched **asynchronously** (a
    /// daemon / web session, where the file lives across the wire). Reuses an
    /// already-open buffer; otherwise creates the empty, named replica buffer and
    /// [enqueues](Self::enqueue_open) its fetch, returning the id **without switching**
    /// so a workspace edit can stash its edits against it and apply them when the bytes
    /// land (the synchronous read `ensure_buffer_loaded` does is impossible off-tick, so
    /// the apply is necessarily deferred). Local sessions use `ensure_buffer_loaded`.
    pub fn enqueue_replica_open(&mut self, path: &Path) -> BufferId {
        if let Some(id) = self.find_buffer_by_path(path) {
            return id;
        }
        let id = self.add_buffer(Buffer::named(path.to_path_buf()));
        self.enqueue_open(id, path.to_path_buf());
        id
    }

    /// Open `path` as the **current window's** buffer — the `:e file` / go-to core. Reuses
    /// an already-open buffer, reuses a throwaway `[No Name]` in place (so the first open
    /// doesn't strand an empty buffer 1), or loads a new buffer and switches to it.
    /// Off-tick aware throughout: the throwaway-reuse names the buffer now and fills it
    /// over the wire, and the new-buffer load defers to [`Editor::load_new_buffer`].
    /// Returns the buffer now shown, or `None` if a synchronous load failed (echoed) — the
    /// caller then leaves the current buffer in place rather than navigating into nothing.
    pub(crate) fn edit_in_current_window(&mut self, path: &Path) -> Option<BufferId> {
        if let Some(id) = self.find_buffer_by_path(path) {
            self.switch_buffer(id);
            return Some(id);
        }
        if self.current_is_throwaway() {
            // Reuse the throwaway in place, preserving its id. Off-tick: bind the name now
            // and enqueue the fetch (the empty buffer shows until the bytes land); locally:
            // read it in place.
            let id = self.cur_buffer();
            if self.should_defer_open() {
                self.buffer_mut().set_path(Some(path.to_path_buf()));
                // Shada pending marks are seeded by the off-tick fetch landing; for a
                // BufReadCmd-deferred local open, the synchronous `load_pending_open`
                // seeds them itself (matching the inline `load_into_current`).
                if self.host_fs_offtick {
                    self.seed_pending_file_marks(id);
                }
                self.enqueue_open(id, path.to_path_buf());
            } else {
                self.load_into_current(path);
            }
            return Some(id);
        }
        let id = self.load_new_buffer(path)?;
        self.switch_buffer(id);
        Some(id)
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
        let already_current = self.current_buffer_is(path);
        // A go-to (LSP definition/references, a diagnostic, a location-list entry)
        // is a *jump* in vim's sense: record the pre-jump position into the jump
        // list / previous-context mark first, so `<C-o>` returns here. Done before
        // any buffer switch, while the cursor still sits at the source — the switch
        // rebinds `self.cursor` to the target buffer's saved position. Only on a
        // *real* navigation, though: `jump_to_lsp_location` calls this twice (land,
        // then refine the column on the same line), and a same-file same-line move
        // is not a new jump — vim's jumplist dedups by line anyway.
        if !already_current || line != self.cursor.line {
            self.record_jump_context();
        }
        // Open-or-switch through the `'switchbuf'`-aware kernel: a buffer already shown
        // in another tab (`usetab`) / the current tab (`useopen`) is *focused* there
        // rather than re-opened here; otherwise it edits in the current window (cwd-aware
        // reuse, alternate `#`, off-tick wire fetch). A failed *synchronous* load returns
        // `None`; bail rather than land the cursor in a phantom buffer.
        if !already_current && self.open_path_switchbuf(path).is_none() {
            return;
        }

        self.land_cursor(line, col);
    }

    /// Open `path` in a **new tab** and land the cursor at the 0-based
    /// `(line, byte col)` — the picker's `<C-t>` (`confirm_tab`) on a located item.
    /// An explicit tab gesture, so it ignores `'switchbuf'` (always a fresh tab),
    /// mirroring `:tabedit`'s find-or-load ([`Editor::open_buffer`], off-tick aware)
    /// + [`Editor::new_tab`]; a failed synchronous load still opens an empty tab.
    pub fn jump_to_tab(&mut self, path: &Path, line: usize, col: usize) {
        let options = self.windows.cur().options.clone();
        let buf = self
            .open_buffer(path)
            .unwrap_or_else(|| self.add_buffer(Buffer::empty()));
        self.new_tab(buf, options);
        self.land_cursor(line, col);
    }

    /// Open `path` in a new **split** of the focused window — `vertical` ⇒ a vsplit
    /// (`<C-v>`), else a horizontal split (`<C-x>`) — and land the cursor at the
    /// 0-based `(line, byte col)`, the picker's `confirm_split`/`confirm_vsplit`. An
    /// explicit gesture, so it always splits (ignores `'switchbuf'`) but still reuses
    /// an already-open buffer cwd-aware ([`Editor::edit_in_current_window`], like a
    /// jump). A failed synchronous load leaves the split on the previous buffer and
    /// lands no cursor.
    pub fn jump_to_split(&mut self, path: &Path, line: usize, col: usize, vertical: bool) {
        let dir = if vertical {
            SplitDir::Vertical
        } else {
            SplitDir::Horizontal
        };
        self.split(dir);
        if self.edit_in_current_window(path).is_some() {
            self.land_cursor(line, col);
        }
    }

    /// Open `path` honoring `'switchbuf'`: if a window already shows its buffer in a
    /// tab we may reuse ([`Editor::switchbuf_window`] — any tab for `usetab`, the
    /// current tab for `useopen`), focus that window (switching tabs as needed) and
    /// leave its cursor where it sits; otherwise edit it in the current window
    /// ([`Editor::edit_in_current_window`]). Returns the buffer now shown, or `None`
    /// on a failed synchronous load. The shared jump kernel behind every located
    /// navigation ([`Editor::jump_to`]).
    pub fn open_path_switchbuf(&mut self, path: &Path) -> Option<BufferId> {
        if let Some(buf) = self.find_buffer_by_path(path) {
            if let Some((tab_idx, win)) = self.switchbuf_window(buf) {
                self.goto_tab_window(tab_idx, win);
                return Some(buf);
            }
        }
        self.edit_in_current_window(path)
    }

    /// Switch to an already-loaded buffer honoring `'switchbuf'`: focus a window
    /// already showing it ([`Editor::switchbuf_window`]) — switching tabs for
    /// `usetab` — else swap it into the current window ([`Editor::switch_buffer`],
    /// preserving the buffer's saved cursor). The buffer-number companion of
    /// [`Editor::open_path_switchbuf`], used by the `nx.picker` buffers source. A
    /// no-op for an unknown id.
    pub fn switch_to_buffer_switchbuf(&mut self, buf: BufferId) {
        if !self.buffers.map.contains_key(&buf) {
            return;
        }
        if let Some((tab_idx, win)) = self.switchbuf_window(buf) {
            self.goto_tab_window(tab_idx, win);
        } else {
            self.switch_buffer(buf);
        }
    }

    /// Show an already-loaded buffer in a **new tab** — the picker buffers source's
    /// `<C-t>`. An explicit gesture, so it always makes a new tab (ignores
    /// `'switchbuf'`), unlike [`Editor::switch_to_buffer_switchbuf`]. A no-op for an
    /// unknown id.
    pub fn open_buffer_in_tab(&mut self, buf: BufferId) {
        if !self.buffers.map.contains_key(&buf) {
            return;
        }
        let options = self.windows.cur().options.clone();
        self.new_tab(buf, options);
    }

    /// Show an already-loaded buffer in a **new split** of the focused window —
    /// `vertical` ⇒ a vsplit (`<C-v>`), else horizontal (`<C-x>`) — the picker
    /// buffers source's split gesture. Always splits (ignores `'switchbuf'`); the
    /// raw [`Editor::switch_buffer`] swaps the buffer into the fresh window. A no-op
    /// for an unknown id.
    pub fn open_buffer_in_split(&mut self, buf: BufferId, vertical: bool) {
        if !self.buffers.map.contains_key(&buf) {
            return;
        }
        let dir = if vertical {
            SplitDir::Vertical
        } else {
            SplitDir::Horizontal
        };
        self.split(dir);
        self.switch_buffer(buf);
    }

    /// Land the cursor at the 0-based `(line, byte col)`, clamped to the buffer and
    /// snapped to a grapheme boundary / valid normal-mode resting cell (exactly like
    /// a search landing). The cursor-positioning tail shared by [`Editor::jump_to`]
    /// and [`Editor::jump_to_tab`].
    fn land_cursor(&mut self, line: usize, col: usize) {
        // A located jump onto a buffer whose content is still pending (a deferred open):
        // the buffer is empty, so clamping now would snap to the top and the read landing
        // would reset it anyway. Record the target so the landing applies it once the
        // lines are there (`settle_loaded_cursor`); also set a best-effort clamped cursor
        // now so a synchronous reader sees something sane in the meantime.
        let buf = self.cur_buffer();
        if self.has_pending_open(buf) {
            self.pending_open_cursor = Some((buf, line, col));
        }
        let line = line.min(self.last_line());
        let byte = self.buffer().line_start(line) + col.min(self.buffer().line(line).len());
        self.settle_cursor_byte(byte);
    }

    /// Settle the cursor after a deferred open's content lands in `buffer`: if a located
    /// jump was waiting on it ([`pending_open_cursor`](Editor)), land on that target;
    /// otherwise reset to the top (a plain `:edit` starts at line 1). A no-op unless
    /// `buffer` is current (a background landing keeps its own saved position).
    fn settle_loaded_cursor(&mut self, buffer: BufferId) {
        if buffer != self.cur_buffer() {
            return;
        }
        if let Some((b, line, col)) = self.pending_open_cursor {
            if b == buffer {
                self.pending_open_cursor = None;
                self.cursor = Cursor::default();
                self.top = 0;
                self.leftcol = 0;
                self.land_cursor(line, col);
                return;
            }
        }
        self.cursor = Cursor::default();
        self.top = 0;
        self.leftcol = 0;
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
        // Carry the live cursor through the edits the way neovim's `apply_text_edits`
        // does: an edit that ends *at or before* the cursor shifts it by the row/column
        // delta the replacement introduces, while an edit the cursor sits *inside* — or
        // one after it — leaves it put. So the cursor follows the text it was on (a
        // rename that inserts a `use` import above it carries it down) without a
        // whole-document reformat — one edit spanning the cursor — dragging it to the
        // end of the file. The math runs in (row, col) against the pre-edit buffer, so
        // each edit's original coordinates and its new text's shape are captured here,
        // before the apply loop mutates the rope.
        let is_current = id == self.cur_buffer();
        let cursor_plan: Vec<(usize, usize, usize, usize, usize, usize)> = if is_current {
            let buf = &self.buffers.get(id).buffer;
            edits
                .iter()
                .map(|(range, text)| {
                    let len = buf.len_bytes();
                    let start = buf.text.floor_char_boundary(range.start.min(len));
                    let end = buf.text.floor_char_boundary(range.end.min(len));
                    let start_row = buf.byte_to_line(start);
                    let end_row = buf.byte_to_line(end);
                    let start_col = start - buf.line_start(start_row);
                    let end_col = end - buf.line_start(end_row);
                    // New text shape: how many lines it spans, and the byte length of
                    // its final line (the column the cursor lands at on the last row).
                    let new_lines = text.bytes().filter(|b| *b == b'\n').count() + 1;
                    let last_len = text.rfind('\n').map_or(text.len(), |i| text.len() - i - 1);
                    (start_row, start_col, end_row, end_col, new_lines, last_len)
                })
                .collect()
        } else {
            Vec::new()
        };
        for (range, text) in &edits {
            let buf = &mut self.buffers.get_mut(id).buffer;
            let len = buf.len_bytes();
            let start = buf.text.floor_char_boundary(range.start.min(len));
            let end = buf.text.floor_char_boundary(range.end.min(len));
            if start < end {
                buf.remove(start..end);
            }
            if !text.is_empty() {
                buf.insert(start, text);
            }
        }
        self.buffers.get_mut(id).buffer.normalize();
        if is_current {
            // Replay the edits over the cursor in the same (reverse-document) order
            // they applied, mutating a running (row, col) — neovim's algorithm.
            let (mut row, mut col) = (self.cursor.line, self.cursor.col);
            for &(s_row, s_col, e_row, e_col, new_lines, last_len) in &cursor_plan {
                let row_delta = new_lines as i64 - (e_row - s_row + 1) as i64;
                if e_row < row {
                    row = (row as i64 + row_delta).max(0) as usize;
                } else if e_row == row && e_col <= col {
                    row = (row as i64 + row_delta).max(0) as usize;
                    // The new last-row column, plus how far the cursor sat past the
                    // edit's end; a single-line replacement also keeps its start column.
                    col = last_len + (col - e_col);
                    if new_lines == 1 {
                        col += s_col;
                    }
                }
            }
            let row = row.min(self.last_line());
            let col = col.min(self.buffer().line_len(row));
            self.set_cursor_char(self.buffer().line_start(row) + col);
            self.desired_col = self.cursor_virtcol();
            self.desired_eol = false;
            self.ensure_visible();
        }
        // A workspace edit is a complete one-shot; commit it now so it lands as a
        // single undo node, independent of any later edit to `id`. Committed *after*
        // the cursor settles so the node carries the post-edit cursor — a later redo
        // back to it restores where the edit left the cursor, not where it started.
        self.commit_undo(id);
        // A non-current buffer's saved cursor is clamped by `enter_buffer` on the
        // switch back, so nothing to do here.
    }

    /// `:ls` / `:buffers` — list the open buffers into a read-only `nxbuffers`
    /// listing, one per row (id-sorted), with vim's flag columns: `%` current /
    /// `#` alternate, `a` active / `h` hidden, `+` modified. `<CR>` switches to the
    /// buffer on the cursor line (a buffer-local map from the `FileType` autocmd).
    pub(crate) fn ex_buffers(&mut self) {
        let current = self.cur_buffer();
        let alternate = self.alternate;
        let live_cursor = self.cursor.line;
        let mut lines = Vec::new();
        let mut current_row = 0;
        // `:ls` is scoped to the **focused layer** — a dock lists only its own
        // buffers, the main area only its own. (The neovim `nvim_list_bufs` API
        // stays global; this is the interactive, per-region list.)
        for (row, id) in self
            .buffers_in_layer(self.focused_layer)
            .into_iter()
            .enumerate()
        {
            let ob = self.buffers.get(id);
            if id == current {
                current_row = row;
            }
            let flag = if id == current {
                '%'
            } else if Some(id) == alternate {
                '#'
            } else {
                ' '
            };
            let active = if id == current { 'a' } else { 'h' };
            let modified = if ob.buffer.modified { '+' } else { ' ' };
            let name = ob
                .buffer
                .path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "[No Name]".to_string());
            let lnum = if id == current {
                live_cursor
            } else {
                ob.saved_cursor.line
            } + 1;
            lines.push(format!(
                "{:>3} {flag}{active} {modified} \"{name}\" line {lnum}",
                id.0
            ));
        }
        // A read-only `nxbuffers` listing; its `FileType` autocmd installs the
        // buffer-local `<CR>` that parses the leading buffer number off the cursor
        // line and switches to it (the vim ftplugin model, replacing the old panel
        // `on_select` wiring).
        self.open_buffer_listing(lines, current_row);
    }

    /// `:lspanels` / `:panels` — list the named panels (the surfaces hidden from `:ls`) in
    /// a `[Panels]` panel. Each row is `<bufnr> <name>`; `<CR>` (the `nxpanels` ftplugin's
    /// buffer-local map → `:b <n>`) re-opens the picked panel **in place**, showing its last
    /// content (a swap within the panel window, not a regenerating command). The `[Panels]`
    /// surface omits itself from the list.
    pub(crate) fn ex_lspanels(&mut self) {
        let mut lines: Vec<String> = self
            .panel_buffers
            .iter()
            .filter(|(name, _)| name != "[Panels]")
            .map(|(name, buf)| format!("{:>3} {name}", buf.0))
            .collect();
        if lines.is_empty() {
            lines.push("(no panels yet)".to_string());
        }
        self.open_named_panel("[Panels]", lines, 0, "nxpanels", LISTING_HEIGHT);
    }

    /// `:messages` — show the message history in a read-only scratch listing,
    /// opened scrolled to the end with the newest line selected.
    pub(crate) fn ex_messages(&mut self) {
        let lines: Vec<String> = self.messages.iter().map(|m| m.text.clone()).collect();
        let errors: Vec<bool> = self.messages.iter().map(|m| m.error).collect();
        let last = lines.len().saturating_sub(1);
        self.open_scratch_listing("[Messages]", lines, last);
        // Messages are free-form (notifications, multi-line errors, stack traces),
        // so soft-wrap the panel — the global `nowrap` default would clip a long
        // message off the right edge. Window-local, set after the panel window is
        // mounted (and current), then re-settle the viewport so the wrapped layout
        // keeps the selected (newest) line visible.
        self.windows.cur_mut().options.wrap = true;
        self.ensure_visible();
        // Paint each error line red. Done after the panel is mounted (its buffer
        // is current and freshly loaded, so the marks survive until the next
        // re-open clears them).
        self.highlight_listing_lines(&errors, "ErrorMsg");
    }

    /// `:registers` / `:reg` / `:display` — list the non-empty registers in a
    /// read-only scratch listing, mirroring vim's `Type Name Content` layout (a `c`/`l` type
    /// column, `^J` for embedded newlines). An argument filters to the named
    /// registers (`:reg ab0`). The read-only specials `"%` / `"/` / `":` are
    /// projected from live editor state.
    pub(crate) fn ex_registers(&mut self, args: &str) {
        let filter: Vec<char> = args.chars().filter(|c| !c.is_whitespace()).collect();
        let wanted = |name: char| filter.is_empty() || filter.contains(&name);

        let mut lines = vec!["Type Name Content".to_string()];
        // Stored registers in vim's order: unnamed, numbered, small-delete, named.
        let order = std::iter::once('"')
            .chain('0'..='9')
            .chain(std::iter::once('-'))
            .chain('a'..='z');
        for name in order {
            if !wanted(name) {
                continue;
            }
            if let Some(cell) = self.registers.peek(name) {
                lines.push(format_register_line(name, &cell.text, cell.kind));
            }
        }
        // Read-only specials, resolved against live editor state.
        for name in ['%', '/', ':'] {
            if !wanted(name) {
                continue;
            }
            if let Some((text, kind)) = self.register_text(Some(name)) {
                if !text.is_empty() {
                    lines.push(format_register_line(name, &text, kind));
                }
            }
        }
        self.open_scratch_listing("[Registers]", lines, 0);
    }

    /// Resolve a `:buffer` / `:bdelete` argument to a buffer id: empty = current,
    /// `#` = alternate, a number = that buffer id, otherwise a file-name
    /// substring. Sets the appropriate `E86`/`E94`/`E93` message and returns
    /// `None` when it can't resolve.
    pub(crate) fn resolve_buffer(&mut self, arg: &str) -> Option<BufferId> {
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
    /// wrapping around. Scoped to the focused layer, matching `:ls`.
    pub(crate) fn ex_bnext(&mut self, count: usize) {
        let ids = self.buffers_in_layer(self.focused_layer);
        let len = ids.len();
        if let Some(i) = ids.iter().position(|id| *id == self.cur_buffer()) {
            self.switch_buffer(ids[(i + count) % len]);
        }
    }

    /// `:bprevious` — switch `count` positions earlier in id order, wrapping.
    /// Scoped to the focused layer, matching `:ls`.
    pub(crate) fn ex_bprev(&mut self, count: usize) {
        let ids = self.buffers_in_layer(self.focused_layer);
        let len = ids.len();
        if let Some(i) = ids.iter().position(|id| *id == self.cur_buffer()) {
            self.switch_buffer(ids[(i + len - count % len) % len]);
        }
    }

    /// `:bfirst` — switch to the lowest-numbered buffer in the focused layer.
    pub(crate) fn ex_bfirst(&mut self) {
        if let Some(&id) = self.buffers_in_layer(self.focused_layer).first() {
            self.switch_buffer(id);
        }
    }

    /// `:blast` — switch to the highest-numbered buffer in the focused layer.
    pub(crate) fn ex_blast(&mut self) {
        if let Some(&id) = self.buffers_in_layer(self.focused_layer).last() {
            self.switch_buffer(id);
        }
    }

    /// `:bdelete` / `:bwipeout` — remove a buffer from the list (default the
    /// current one). Refuses a modified buffer without `!`. When the current
    /// buffer is removed, the window moves to the alternate (or the nearest
    /// remaining id); removing the last buffer leaves a fresh `[No Name]`.
    pub(crate) fn ex_bdelete(&mut self, args: &str, bang: bool) {
        let Some(target) = self.resolve_buffer(args) else {
            return;
        };
        self.delete_buffer(target, bang);
    }

    /// Remove buffer `target` from the editor — the shared core of `:bdelete` and
    /// the `nvim_buf_delete` API. Refuses a modified buffer without `force` (the
    /// `E89` guard). When the current buffer is removed, the window moves to the
    /// alternate (or the nearest remaining id); removing the last buffer leaves a
    /// fresh `[No Name]`. Returns `false` (a no-op) when the buffer is unknown or
    /// the `E89` guard blocked an unforced delete of a modified buffer.
    pub fn delete_buffer(&mut self, target: BufferId, force: bool) -> bool {
        if !self.buffers.map.contains_key(&target) {
            return false;
        }
        if self.buffers.get(target).buffer.modified && !force {
            self.echo(format!(
                "E89: No write since last change for buffer {} (add ! to override)",
                target.0
            ));
            return false;
        }

        // When removing the current buffer, move to the alternate if it's a
        // distinct, still-open buffer (vim's behavior), else the nearest id —
        // **within the same layer**: closing a document in the main area must never
        // pull in a dock's buffer (or vice versa). The replacement layer is the
        // focused one, since the focused window is the one losing `target`.
        let was_current = target == self.cur_buffer();
        let layer = self.focused_layer;
        let replacement = if was_current {
            self.alternate
                .filter(|a| {
                    *a != target
                        && self.buffers.map.contains_key(a)
                        && self.buffers.get(*a).layer == layer
                })
                .or_else(|| self.neighbor_of(target, layer))
        } else {
            None
        };
        self.buffers.map.remove(&target);
        self.panel_buffers.retain(|(_, b)| *b != target);
        self.doc_float_buffers.retain(|(_, b)| *b != target);
        // Drop any `nx.view` backed by this buffer. A user `:bd` on a view buffer
        // (e.g. a hidden help/tree view) would otherwise strand the view entry with a
        // freed `BufferId`: `view_buffer` would still return it and `set_view_lines`
        // would panic in `get_mut`. The mirror then clears too, so the plugin's
        // `view:bufnr()` reads nil and it can recreate. (`destroy_view` removes the
        // entry before calling here, so its own deletes aren't double-handled.)
        self.views.retain(|_, v| v.buf != target);
        self.syntax_close(target);
        if self.alternate == Some(target) {
            self.alternate = None;
        }

        // `'bdclosetab'` (nxvim default): if `target` was the *only* buffer the
        // focused tab showed and other tabs are open, close the tab page rather than
        // loading a sibling buffer into it. "Only buffer" means every tiled window of
        // the live tree showed `target` (a split onto another buffer keeps the tab).
        let close_tab = was_current
            && self.options.bdclosetab
            && self.focused_stack().tabs.len() > 1
            && self
                .windows
                .leaves()
                .iter()
                .all(|&id| self.windows.get(id).buffer == target);

        if was_current {
            if close_tab {
                // Drop the tab; a surviving tab's tree becomes live and its window's
                // buffer becomes current. The closed tree (the only windows on
                // `target`) is gone, so the sweep below only touches other tabs/layers.
                self.close_tab();
            } else {
                match replacement {
                    // `current` now dangles; move to the chosen same-layer replacement
                    // (no stash — the outgoing buffer is gone).
                    Some(rep) => self.enter_buffer(rep),
                    // No sibling buffer remains in this layer: open a fresh, empty one
                    // in the window rather than borrowing another layer's buffer. This
                    // also covers the never-leave-zero-buffers case.
                    None => {
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
                    }
                }
            }
        }
        // The focused window is now off `target`, but any *other* window — a split
        // in this layer, or a window in another layer/tab (e.g. a buffer moved to a
        // dock while still shown in the main area) — may still bind the freed id and
        // would panic the next time it is read. Rebind every such window.
        self.rebind_windows_off_buffer(target);
        true
    }

    /// After `target` has been removed from the store, rebind every window across
    /// every layer and tab that still shows it to a valid buffer, so none dangles on
    /// the freed id. Each affected window gets a replacement from **its own** layer
    /// (a sibling buffer there, else a fresh `[No Name]` tagged to that layer); the
    /// focused layer reuses whatever the current window already landed on. The caller
    /// has already handled the focused window itself.
    fn rebind_windows_off_buffer(&mut self, target: BufferId) {
        // A tiny per-layer replacement cache (layers number ≤ 5, so a Vec beats a
        // map). Seed the focused layer with the buffer the current window now shows.
        let mut repl: Vec<(Layer, BufferId)> = vec![(self.focused_layer, self.cur_buffer())];
        for (layer, idx) in self.all_layer_tabs() {
            let affected = self
                .layer_tab_tree(layer, idx)
                .is_some_and(|t| t.all_windows().any(|w| w.buffer == target));
            if !affected {
                continue;
            }
            let rep = match repl.iter().find(|(l, _)| *l == layer) {
                Some((_, r)) => *r,
                None => {
                    // First buffer in this layer (target is already gone from the
                    // store), or a fresh empty tagged to the layer if it has none.
                    let r = self
                        .buffers_in_layer(layer)
                        .into_iter()
                        .next()
                        .unwrap_or_else(|| {
                            let id = self.add_buffer(Buffer::empty());
                            self.set_buffer_layer(id, layer);
                            id
                        });
                    repl.push((layer, r));
                    r
                }
            };
            let lines = self.buffers.get(rep).buffer.line_count();
            if let Some(tree) = self.layer_tab_tree_mut(layer, idx) {
                for w in tree.all_windows_mut() {
                    if w.buffer == target {
                        w.buffer = rep;
                        if w.saved_cursor.line >= lines {
                            w.saved_cursor.line = lines.saturating_sub(1);
                            w.saved_cursor.col = 0;
                        }
                    }
                }
            }
        }
    }

    /// Every `(layer, tab index)` that currently has a window tree — the main layer
    /// plus every dock that exists (visible *or* hidden), across all their tabs. The
    /// iteration space for sweeps that must touch every window everywhere.
    fn all_layer_tabs(&self) -> Vec<(Layer, usize)> {
        let mut layers = vec![Layer::Main];
        for side in DockSide::ALL {
            if self.dock_exists(side) {
                layers.push(Layer::Dock(side));
            }
        }
        let mut out = Vec::new();
        for layer in layers {
            if let Some(stack) = self.stack(layer) {
                out.extend((0..stack.tabs.len()).map(|idx| (layer, idx)));
            }
        }
        out
    }

    /// The nearest buffer to `id` among the *other* open buffers **in `layer`**: the
    /// largest id below it, else the smallest above it. `None` if `id` is the only
    /// buffer in that layer. Per-layer so the close fallback never crosses into a
    /// dock (see [`Editor::delete_buffer`]).
    fn neighbor_of(&self, id: BufferId, layer: Layer) -> Option<BufferId> {
        let ids = self.buffers_in_layer(layer);
        let below = ids.iter().rev().find(|&&b| b < id).copied();
        below.or_else(|| ids.iter().find(|&&b| b > id).copied())
    }
}

/// One `:registers` row in vim's layout: a `c`/`l` type column, the `"name`,
/// then the content with embedded newlines shown as `^J` (a trailing one for a
/// linewise register, which keeps its closing `\n`).
fn format_register_line(name: char, text: &str, kind: RegKind) -> String {
    let ty = if kind == RegKind::Line { 'l' } else { 'c' };
    let shown = text.replace('\n', "^J");
    format!("  {ty}  \"{name}   {shown}")
}
