//! The in-window directory listing — nxvim's file explorer (vim's netrw).
//!
//! Opening a directory (`nxvim .`, `:e somedir`, `<CR>` on a listed sub-directory)
//! builds a read-only [`Buffer`] whose lines are the directory's entries (see
//! [`Buffer::from_dir`]). Such a buffer carries `dir: Some(path)`, which makes
//! [`Editor::input`] route its normal-mode keys here instead of through the
//! editing grammar: the listing navigates and opens entries but can never be
//! edited, so it stays a faithful picture of the filesystem.

use super::*;
use crate::buffer::Buffer;
use crate::input::{Key, KeyCode};
use std::path::Path;

impl Editor {
    /// Whether the current buffer is a directory listing (the file explorer).
    /// When true, [`Editor::input`] hands normal-mode keys to
    /// [`Editor::handle_explorer`] rather than the editing state machine.
    pub(crate) fn is_explorer_buffer(&self) -> bool {
        self.buffer().dir.is_some()
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
            self.enqueue_open(buf, path.to_path_buf());
            return;
        }

        // Path-based dedup: an already-open listing of the same directory is
        // reused (and reset to the top), matching the file open-or-switch path.
        let fs = self.host_fs.clone();
        let canon = fs.canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if self.buffer().dir.as_deref() != Some(canon.as_path()) {
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
                self.explorer_goto(0);
            }
            Err(e) => self.echo(e.to_string()),
        }
    }

    /// Handle one key while a directory-listing buffer is focused in normal mode.
    /// `<CR>` opens the entry under the cursor — a file is edited, a sub-directory
    /// is listed in place — and `-` goes up to the parent. Vertical motions
    /// (`j`/`k`/`gg`/`G`/`<C-d>`/`<C-u>`/`<C-f>`/`<C-b>`, arrows, `Home`/`End`)
    /// move the selection. `:`/`/`/`?` fall through to normal handling so the
    /// command line and search still work. Every editing key is inert: the listing
    /// is effectively `nomodifiable`, so it can't be corrupted.
    pub(crate) fn handle_explorer(&mut self, key: Key) {
        self.message.clear();

        // `gg` — the first `g` arms the prefix so we never hand a bare `g` to the
        // editing grammar (where it could begin a `gu`/`gU`/… change command).
        if self.explorer_gpending {
            self.explorer_gpending = false;
            if key.as_char() == Some('g') {
                self.explorer_goto(0);
            }
            return;
        }

        let last = self.last_line();
        let half = (self.text_height() / 2).max(1);
        let page = self.text_height().saturating_sub(2).max(1);
        let cur = self.cursor.line;

        match (key.code, key.as_char(), key.ctrl) {
            (KeyCode::Enter, _, _) => self.explorer_open_entry(),
            (_, Some('-'), false) => self.explorer_up(),
            (_, Some('g'), false) => self.explorer_gpending = true,

            (KeyCode::Down, _, _) | (_, Some('j'), false) => {
                self.explorer_goto((cur + 1).min(last))
            }
            (KeyCode::Up, _, _) | (_, Some('k'), false) => {
                self.explorer_goto(cur.saturating_sub(1))
            }
            (_, Some('G'), false) => self.explorer_goto(last),
            (KeyCode::Char('d'), _, true) => self.explorer_goto((cur + half).min(last)),
            (KeyCode::Char('u'), _, true) => self.explorer_goto(cur.saturating_sub(half)),
            (KeyCode::Char('f'), _, true) | (KeyCode::PageDown, _, _) => {
                self.explorer_goto((cur + page).min(last))
            }
            (KeyCode::Char('b'), _, true) | (KeyCode::PageUp, _, _) => {
                self.explorer_goto(cur.saturating_sub(page))
            }
            (KeyCode::Home, _, _) => self.explorer_goto(0),
            (KeyCode::End, _, _) => self.explorer_goto(last),

            // The command line and search open through normal handling. Each is a
            // single key that only switches mode, so the listing stays intact and
            // `:q` / `:e file` / `/pattern` behave exactly as in any buffer.
            (_, Some(':'), false) | (_, Some('/'), false) | (_, Some('?'), false) => {
                self.handle_normal(key)
            }

            _ => {} // every editing key is inert on a listing
        }
    }

    /// Move the listing selection to `line` (clamped), resting at column 0 and
    /// scrolling it into view. The explorer's one cursor primitive.
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
        let Some(dir) = self.buffer().dir.clone() else {
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
        let Some(dir) = self.buffer().dir.clone() else {
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
        if let Some(id) = self.find_buffer_by_path(path) {
            self.switch_buffer(id);
        } else if self.host_fs_offtick {
            // Off-tick (daemon session): the file is on the remote. Open an empty
            // buffer named for it, switch, and enqueue the fetch — the server fills it
            // over the wire (`load_str_into`). The empty buffer shows until then.
            let id = self.add_buffer(Buffer::named(path.to_path_buf()));
            self.switch_buffer(id);
            self.enqueue_open(id, path.to_path_buf());
        } else {
            let fs = self.host_fs.clone();
            match Buffer::from_file(path, &*fs) {
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
        // Wipe the listing buffer now that we've left it. Forced because it is a
        // read-only listing that is never "modified", so the `E89` guard is moot.
        self.delete_buffer(explorer, true);
    }
}
