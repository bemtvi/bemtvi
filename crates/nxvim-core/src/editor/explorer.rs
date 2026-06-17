//! The in-window directory listing — nxvim's file explorer (vim's netrw).
//!
//! Opening a directory (`nxvim .`, `:e somedir`, `<CR>` on a listed sub-directory)
//! builds a read-only [`Buffer`] whose lines are the directory's entries (see
//! [`Buffer::from_dir`]). Such a buffer carries `dir: Some(path)`, so
//! [`Buffer::read_only`] is true and edits are refused at the
//! [`modifiable`](Editor::modifiable) chokepoints — the listing stays a faithful
//! picture of the filesystem. It is otherwise an **ordinary buffer in a window**
//! (vim's netrw model): `j`/`k`/`gg`/`G`/`/`/`:` are ordinary normal-mode motions,
//! and its two activation keys — `<CR>` (open) and `-` (parent), via
//! [`apply_explorer_action`](Editor::apply_explorer_action) — are **buffer-local
//! default keymaps** installed by the `FileType nxdir` autocmd, not a special
//! `input()` branch. See docs/plans/2026-06-16-unify-special-buffer-kinds.md.

use super::*;
use crate::buffer::Buffer;
use std::path::Path;

impl Editor {
    /// Whether the current buffer is a directory listing (the file explorer).
    /// When true, [`Editor::key_context`] reports [`KeyContext::Explorer`] so the
    /// matcher routes normal-mode keys through the `explorer` keymap bucket
    /// ([`Editor::apply_explorer_action`]) rather than the editing state machine.
    pub(crate) fn is_explorer_buffer(&self) -> bool {
        self.buffer().dir().is_some()
    }

    /// Open `path` (a directory) as the file explorer. Reuses the current window
    /// in place when it holds a throwaway scratch buffer or is already an explorer
    /// (so `:e dir` from `[No Name]` and descending into a sub-directory both reuse
    /// the window, like netrw); otherwise opens a fresh listing buffer and switches
    /// to it, keeping the current buffer in the list. An unreadable directory fails
    /// loud — its OS error is echoed rather than leaving a blank buffer behind.
    pub(crate) fn enter_dir(&mut self, path: &Path) {
        // Off-tick (daemon session): the directory lives on the remote, so its
        // listing can't be read through the synchronous `host_fs` without blocking
        // the editor thread on the network. Set up the destination buffer — reuse
        // the window in place when it's a throwaway/explorer (netrw-style descend),
        // else a fresh listing buffer keeping the current one open — and enqueue an
        // off-tick fetch. The server reads the entries over `HostFsAsync` and fills
        // the buffer via `load_dir_into`; the old listing shows until then.
        if self.host_fs_offtick {
            let buf = if self.current_is_throwaway() || self.is_explorer_buffer() {
                self.cur_buffer()
            } else {
                let id = self.add_buffer(Buffer::named(path.to_path_buf()));
                self.switch_buffer(id);
                id
            };
            // Mark it `nxdir` now (before the off-tick fill) so the `FileType nxdir`
            // autocmd fires while the buffer is current — `load_dir_into` re-sets it
            // when the entries land, but by then the buffer is already announced.
            self.set_filetype(buf, "nxdir");
            self.enqueue_open(buf, path.to_path_buf());
            return;
        }

        // Path-based dedup: an already-open listing of the same directory is
        // reused (and reset to the top), matching the file open-or-switch path.
        let fs = self.host_fs.clone();
        let canon = fs.canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if self.buffer().dir() != Some(canon.as_path()) {
            if let Some(id) = self.find_buffer_by_path(&canon) {
                self.switch_buffer(id);
                self.explorer_goto(0);
                return;
            }
        }
        match Buffer::from_dir(&canon, &*fs) {
            Ok(buf) => {
                if self.current_is_throwaway() || self.is_explorer_buffer() {
                    // Replace in place, preserving the buffer id (netrw reuses the
                    // window as you descend). The listing is freshly read from
                    // disk, so start a clean undo history rooted at it.
                    self.cursor = Cursor::default();
                    self.top = 0;
                    self.leftcol = 0;
                    let ob = self.cur_mut();
                    ob.buffer = buf;
                    ob.undo = UndoTree::new(&ob.buffer);
                    ob.saved_seq = Some(ob.undo.cur_seq());
                    ob.buffer.mark_resync();
                } else {
                    let id = self.add_buffer(buf);
                    self.switch_buffer(id);
                }
                // `filetype=nxdir` so the `FileType nxdir` autocmd installs the
                // explorer's buffer-local `<CR>`/`-` maps (the unified model).
                self.set_filetype(self.cur_buffer(), "nxdir");
                self.explorer_goto(0);
            }
            Err(e) => self.echo(e.to_string()),
        }
    }

    /// Apply a named `explorer` action, dispatched by a `FileType nxdir` buffer-local
    /// keymap (the default `<CR>`/`-` maps in `prelude/keymap.lua`, or a user
    /// override) while a directory-listing buffer is focused. `open` opens the entry
    /// under the cursor — a file is edited, a sub-directory is listed in place — and
    /// `up` goes to the parent. An unknown name fails loud per the no-silent-stub
    /// rule. Navigation (`j`/`k`/`gg`/`G`/`<C-d>`…) is ordinary normal-mode motion on
    /// the `nomodifiable` listing now, so only the activation keys are actions here.
    pub fn apply_explorer_action(&mut self, action: &str) -> Result<(), String> {
        self.message.clear();
        match action {
            "open" => self.explorer_open_entry(),
            "up" => self.explorer_up(),
            other => return Err(format!("unknown explorer action {other:?}")),
        }
        Ok(())
    }

    /// Move the listing selection to `line` (clamped), resting at column 0 and
    /// scrolling it into view. Used when (re)opening a listing to land on the top.
    fn explorer_goto(&mut self, line: usize) {
        self.cursor.line = line.min(self.last_line());
        self.cursor.col = 0;
        self.desired_col = 0;
        self.desired_eol = false;
        self.ensure_visible();
    }

    /// `<CR>` — open the entry under the cursor: descend into a sub-directory
    /// (listed in place), open a file in a new buffer, or go up on `../`. A no-op
    /// on a blank/garbled line.
    fn explorer_open_entry(&mut self) {
        let Some(dir) = self.buffer().dir().map(|p| p.to_path_buf()) else {
            return;
        };
        let line = self.buffer().line(self.cursor.line);
        let entry = line.trim_end_matches('/');
        if entry == ".." {
            self.explorer_up();
            return;
        }
        if entry.is_empty() {
            return;
        }
        let target = dir.join(entry);
        // The listing already encodes whether each entry is a directory (the trailing
        // `/` the listing builder appends), so off-tick we route on that — no remote
        // stat round-trip. Locally we stat directly, as before (a fresh `is_dir`).
        let is_dir = if self.host_fs_offtick {
            self.buffer().line(self.cursor.line).ends_with('/')
        } else {
            target.is_dir()
        };
        if is_dir {
            self.enter_dir(&target);
        } else {
            self.explorer_open_file(&target);
        }
    }

    /// `-` (and `<CR>` on `../`) — list the parent directory. At the filesystem
    /// root there is no parent, so the explorer stays put (as netrw does).
    fn explorer_up(&mut self) {
        let Some(dir) = self.buffer().dir().map(|p| p.to_path_buf()) else {
            return;
        };
        if let Some(parent) = dir.parent() {
            let parent = parent.to_path_buf();
            self.enter_dir(&parent);
        }
    }

    /// Open a file picked from the listing: switch to it if already open, else read
    /// it into a fresh buffer and switch. The explorer is then **destroyed** — it
    /// was a transient picker, so it does not linger in the buffer list or as the
    /// alternate (vim's netrw `bufhidden=wipe`). A read error is echoed and the
    /// listing is kept, so a failed open leaves you back in the explorer.
    fn explorer_open_file(&mut self, path: &Path) {
        let explorer = self.cur_buffer();
        // Open the picked file through the shared kernel — switch to it if already open,
        // else load a fresh buffer (off-tick over the wire in a daemon session, else from
        // local disk). A failed synchronous load is echoed; keep the listing so you land
        // back in the explorer rather than on a blank buffer.
        match self.open_buffer(path) {
            Some(id) => self.switch_buffer(id),
            None => return,
        }
        // Wipe the listing buffer now that we've left it. Forced because it is a
        // read-only listing that is never "modified", so the `E89` guard is moot.
        self.delete_buffer(explorer, true);
    }
}
