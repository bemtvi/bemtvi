//! Buffer lifecycle (open/switch/alternate) and the buffer-list ex-commands
//! (`:buffers`/`:b`/`:bnext`/`:bdelete`/…).

use super::*;
use crate::buffer::{Buffer, EditBatch};
use crate::mode::Mode;
use std::path::Path;

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
        match Buffer::from_file(path) {
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
