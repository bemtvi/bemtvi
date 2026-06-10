//! Buffer lifecycle (open/switch/alternate) and the buffer-list ex-commands
//! (`:buffers`/`:b`/`:bnext`/`:bdelete`/…).

use super::*;
use crate::buffer::{Buffer, DiskChange, EditBatch};
use crate::host::FileStat;
use crate::mode::Mode;
use std::path::Path;

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

    /// Set a string buffer-local option (currently `regexsyntax`) on buffer `id` —
    /// the `vim.bo` string companion to [`Editor::set_buffer_option_num`]. An
    /// unknown option, an unknown buffer, or (for `regexsyntax`) a value other than
    /// `"pcre"`/`"vim"` is ignored; the `:set` ex path is where a bad value fails
    /// loud (`E474`), so a raw `vim.bo` write of garbage leaves the override
    /// untouched (the buffer keeps following the global).
    pub fn set_buffer_option_str(&mut self, id: BufferId, name: &str, value: &str) {
        let Some(ob) = self.buffers.map.get_mut(&id) else {
            return;
        };
        if name == "regexsyntax" {
            ob.buffer.options.regexsyntax = match value {
                "pcre" => crate::options::RegexSyntax::Pcre,
                "vim" => crate::options::RegexSyntax::Vim,
                _ => return,
            };
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
    /// the server forwards to the `vim.treesitter` platform parser as
    /// `nvim_buf_attach` `on_bytes` (so the Lua `LanguageTree` reparses
    /// incrementally instead of re-reading the whole snapshot). Parallel to
    /// [`Editor::take_lsp_edits_of`]; `None` if no such buffer is open.
    pub fn take_lua_ts_edits_of(&mut self, id: BufferId) -> Option<EditBatch> {
        self.buffers
            .map
            .get_mut(&id)
            .map(|ob| ob.buffer.take_lua_ts_edits())
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

    /// The id the next [`Editor::create_buffer`] will hand out. Buffer ids are
    /// monotonic and never reused, so a caller can predict the id of a buffer it
    /// is about to create — the buffer analogue of [`Editor::next_window_id`],
    /// used by the Lua `nvim_create_buf` to return synchronously.
    pub fn next_buffer_id(&self) -> BufferId {
        BufferId(self.buffers.next_id)
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

    /// Whether `id` is an open buffer (`nvim_buf_is_valid`). The RPC surface uses
    /// this to reject a client-supplied buffer handle before binding a window to
    /// it — binding a window to a non-existent buffer would make a later
    /// `buffers.get` on it panic and crash the server.
    pub fn buffer_is_valid(&self, id: BufferId) -> bool {
        self.buffers.map.contains_key(&id)
    }

    /// Add a buffer to the store and return its id, without switching to it.
    pub(crate) fn add_buffer(&mut self, buffer: Buffer) -> BufferId {
        self.buffers.insert(buffer)
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
    /// buffer honestly dirty. `then_quit` carries a deferred `:wq` / `:x`.
    pub(crate) fn enqueue_save(&mut self, path: PathBuf, then_quit: Option<bool>) {
        let buffer = self.cur_buffer();
        let (bytes, lines) = {
            let buf = self.buffer();
            (buf.to_save_bytes(), buf.line_count())
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
        self.buffers.get_mut(buffer).buffer.mark_written(path, stat);
        self.mark_undo_saved(buffer);
    }

    /// Drain the opens the editor deferred this tick (off-tick mode — `:edit` over the
    /// daemon wire). The server fetches each over `HostFsAsync` and fills the named
    /// buffer with [`Editor::load_str_into`]. Empty (a cheap no-op) when off-tick mode
    /// is off or no `:edit` ran.
    pub fn take_pending_opens(&mut self) -> Vec<PendingOpen> {
        std::mem::take(&mut self.pending_opens)
    }

    /// Enqueue an off-tick fetch of `path` into `buffer` (an already-created, empty
    /// buffer the caller has set up and, for `:edit`, switched to). The server reads
    /// the bytes off the editor tick and loads them into `buffer` via
    /// [`Editor::load_str_into`] — the read analogue of [`Editor::enqueue_save`].
    pub(crate) fn enqueue_open(&mut self, buffer: BufferId, path: PathBuf) {
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
        let is_current = buffer == self.cur_buffer();
        let ob = self.buffers.get_mut(buffer);
        let len = ob.buffer.len_bytes();
        ob.buffer.remove(0..len);
        ob.buffer.insert(0, contents);
        ob.buffer.normalize();
        ob.buffer.set_path(name.map(std::path::PathBuf::from));
        ob.buffer.mark_clean();
        // The whole rope was replaced — flag a syntax re-sync (as `load_into_current`
        // does), then root a fresh undo tree at this saved state.
        ob.buffer.mark_resync();
        ob.undo = UndoTree::new(&ob.buffer);
        ob.saved_seq = Some(ob.undo.cur_seq());
        if is_current {
            self.cursor = Cursor::default();
            self.top = 0;
            self.leftcol = 0;
        }
    }

    /// Turn `buffer` into a read-only **directory listing** of `dir` from an off-tick
    /// remote `read_dir` — the explorer analogue of [`Editor::load_str_into`] (daemon /
    /// edit-host split, Phase 3g). In a daemon session core can't read a directory
    /// through the synchronous [`HostFs`](crate::HostFs) without blocking the editor
    /// thread on the network, so `:edit <dir>` / descending into a sub-directory enqueue
    /// a [`PendingOpen`] and the server fetches the entries off-tick; when they land it
    /// calls this to build the listing (via [`Buffer::from_dir_entries`]) into the
    /// already-created buffer. Replaces the buffer in place (preserving its id), roots a
    /// fresh undo tree at the listing, and — when `buffer` is current — resets the
    /// window to the top, as opening a directory does. A no-op if `buffer` was closed
    /// before the fetch landed.
    pub fn load_dir_into(&mut self, buffer: BufferId, dir: PathBuf, entries: Vec<crate::DirEntry>) {
        if !self.buffers.map.contains_key(&buffer) {
            return;
        }
        let listing = Buffer::from_dir_entries(dir, entries);
        let is_current = buffer == self.cur_buffer();
        let ob = self.buffers.get_mut(buffer);
        ob.buffer = listing;
        ob.undo = UndoTree::new(&ob.buffer);
        ob.saved_seq = Some(ob.undo.cur_seq());
        ob.buffer.mark_resync();
        if is_current {
            self.cursor = Cursor::default();
            self.top = 0;
            self.leftcol = 0;
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
    pub(crate) fn goto_alternate(&mut self) {
        match self.alternate {
            Some(id) => self.switch_buffer(id),
            None => self.echo("E23: No alternate file"),
        }
    }

    /// The id of an already-open buffer bound to `path`, if any. Matches by
    /// *lexically* normalized path (so `./a` and `a` are the same buffer),
    /// **without touching the filesystem** — the pure core never does the
    /// blocking `canonicalize` syscall this used to run on every `:e`. Symlinks
    /// are not resolved, matching vim's path-based (not inode-based) buffer dedup.
    pub(crate) fn find_buffer_by_path(&self, path: &Path) -> Option<BufferId> {
        let target = normalize_path(path);
        self.buffers.map.iter().find_map(|(id, ob)| {
            let stored = ob.buffer.path.as_ref()?;
            (normalize_path(stored) == target).then_some(*id)
        })
    }

    /// Is the current buffer a throwaway scratch buffer — unnamed, unmodified,
    /// and empty? `:e file` loads into such a buffer in place (vim's behavior),
    /// rather than leaving a stray `[No Name]` behind.
    pub(crate) fn current_is_throwaway(&self) -> bool {
        let b = self.buffer();
        b.path.is_none() && !b.modified && b.line_count() == 1 && b.line(0).is_empty()
    }

    /// Replace the current buffer's contents with `path`'s, preserving the buffer
    /// id. Used by `:e` reload-in-place and to reuse a throwaway buffer. The
    /// loaded buffer is unmodified; the whole-content swap is flagged for syntax
    /// re-sync (`mark_resync` bumps `changedtick`, but we keep `modified` clear
    /// because it is freshly read from disk).
    pub(crate) fn load_into_current(&mut self, path: &Path) {
        let fs = self.host_fs.clone();
        match Buffer::from_file(path, &*fs) {
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
            }
            Err(e) => self.echo(e.to_string()),
        }
    }

    /// Re-read `buffer` from disk in place — the reload primitive `:checktime`'s
    /// autoread path uses (and the building block the remote-watch slice reuses).
    /// Replaces the rope with the file's current bytes, re-roots the undo tree at
    /// the reloaded (saved) state, refreshes the disk snapshot — so a following
    /// `:checktime` no longer reports a change — and clamps the cursor (the live
    /// one when `buffer` is current, the saved one otherwise) into the new extent
    /// so a now-shorter file can't strand it past the end. On a read failure the
    /// buffer is left untouched and the error echoed. Local-only (synchronous
    /// `host_fs`); the daemon reload is part of the remote-watch slice.
    pub(crate) fn reload_buffer(&mut self, buffer: BufferId) {
        let Some(path) = self.buffers.get(buffer).buffer.path.clone() else {
            return;
        };
        let fs = self.host_fs.clone();
        let new_buf = match Buffer::from_file(&path, &*fs) {
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
    pub(crate) fn checktime(&mut self, target: &str) {
        // A remote stat would have to cross the wire off the editor tick — exactly
        // the `HostWatch` slice we haven't built. Fail loud rather than stat the
        // edit-host's *local* disk for a remote path (which would misreport E211).
        if self.host_fs_offtick {
            self.echo("checktime: remote file watching is not yet wired (daemon session)");
            return;
        }
        let ids = match target.trim() {
            "" => self.buffer_ids(),
            arg => match self.resolve_buffer(arg) {
                Some(id) => vec![id],
                None => return,
            },
        };
        let fs = self.host_fs.clone();
        let autoread = self.options.autoread;
        for id in ids {
            let name = self.buffer_name(id).unwrap_or_default();
            match self.buffers.get(id).buffer.disk_change(&*fs) {
                DiskChange::Unchanged => {}
                DiskChange::Vanished => {
                    self.echo(format!("E211: File \"{name}\" no longer available"))
                }
                DiskChange::Changed => {
                    if self.buffers.get(id).buffer.modified {
                        self.echo(format!(
                            "W12: Warning: File \"{name}\" has changed and the buffer was changed in Vim as well"
                        ));
                    } else if autoread {
                        self.reload_buffer(id);
                    } else {
                        self.echo(format!(
                            "W11: Warning: File \"{name}\" has changed since editing started"
                        ));
                    }
                }
            }
        }
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
        if self.host_fs_offtick {
            let id = self.add_buffer(Buffer::named(path.to_path_buf()));
            self.enqueue_open(id, path.to_path_buf());
            Some(id)
        } else {
            let fs = self.host_fs.clone();
            match Buffer::from_file(path, &*fs) {
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
            if self.host_fs_offtick {
                self.buffer_mut().set_path(Some(path.to_path_buf()));
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
        let already_current = self.buffer().path.as_deref() == Some(path);
        // Open-or-switch into the current window through the shared kernel (so a go-to
        // reuses an already-open buffer, records the alternate `#`, and — in a daemon
        // session — fetches the target over the wire off-tick). A failed *synchronous*
        // load returns `None`; bail rather than land the cursor in a phantom buffer.
        if !already_current && self.edit_in_current_window(path).is_none() {
            return;
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
        // A workspace edit is a complete one-shot; commit it now so it lands as a
        // single undo node, independent of any later edit to `id`.
        self.commit_undo(id);
        if id == self.cur_buffer() {
            self.clamp_cursor();
            self.desired_col = self.cursor_virtcol();
            self.desired_eol = false;
            self.ensure_visible();
        }
        // A non-current buffer's saved cursor is clamped by `enter_buffer` on the
        // switch back, so nothing to do here.
    }

    /// `:ls` / `:buffers` — list the open buffers into the bottom panel, one per
    /// row (id-sorted), with vim's flag columns: `%` current / `#` alternate,
    /// `a` active / `h` hidden, `+` modified.
    pub(crate) fn ex_buffers(&mut self) {
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
    pub(crate) fn ex_messages(&mut self) {
        let lines = self.messages.clone();
        let last = lines.len().saturating_sub(1);
        self.open_panel("Messages", lines, false, last);
    }

    /// `:registers` / `:reg` / `:display` — list the non-empty registers in the
    /// bottom panel, mirroring vim's `Type Name Content` layout (a `c`/`l` type
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
        self.open_panel("Registers", lines, false, 0);
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
    /// wrapping around.
    pub(crate) fn ex_bnext(&mut self, count: usize) {
        let ids = self.buffer_ids();
        let len = ids.len();
        if let Some(i) = ids.iter().position(|id| *id == self.cur_buffer()) {
            self.switch_buffer(ids[(i + count) % len]);
        }
    }

    /// `:bprevious` — switch `count` positions earlier in id order, wrapping.
    pub(crate) fn ex_bprev(&mut self, count: usize) {
        let ids = self.buffer_ids();
        let len = ids.len();
        if let Some(i) = ids.iter().position(|id| *id == self.cur_buffer()) {
            self.switch_buffer(ids[(i + len - count % len) % len]);
        }
    }

    /// `:bfirst` — switch to the lowest-numbered buffer.
    pub(crate) fn ex_bfirst(&mut self) {
        if let Some(&id) = self.buffers.map.keys().next() {
            self.switch_buffer(id);
        }
    }

    /// `:blast` — switch to the highest-numbered buffer.
    pub(crate) fn ex_blast(&mut self) {
        if let Some(&id) = self.buffers.map.keys().next_back() {
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
        self.syntax_close(target);
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
        true
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
}

/// One `:registers` row in vim's layout: a `c`/`l` type column, the `"name`,
/// then the content with embedded newlines shown as `^J` (a trailing one for a
/// linewise register, which keeps its closing `\n`).
fn format_register_line(name: char, text: &str, kind: RegKind) -> String {
    let ty = if kind == RegKind::Line { 'l' } else { 'c' };
    let shown = text.replace('\n', "^J");
    format!("  {ty}  \"{name}   {shown}")
}
