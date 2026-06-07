//! Buffer/mode lifecycle autocmd emission: the server-side diff that fires
//! `BufReadPost`/`FileType`/`BufEnter`/`InsertEnter`, and `init.lua` sourcing.

use crate::filetype_of;
use crate::{Server, WindowRect};
use nxvim_core::{BufferId, WindowId};
use std::path::Path;

impl Server {
    /// Diff the editor's current buffer against what was last announced and fire
    /// the buffer-lifecycle autocmds the transition implies — the central,
    /// server-side emission point (design D1) that keeps `nxvim-core` free of
    /// event types. Called after each applied input (per key in [`Server::input`],
    /// after `:`-commands and `nvim_set_current_buf`) and once at startup.
    ///
    /// Ordering on first opening a file mirrors neovim: `BufReadPost` → `FileType`
    /// → `BufEnter`. `BufReadPost`/`FileType` fire **once** per buffer (gated by
    /// `announced`) and **only for file-backed buffers** — a `[No Name]` buffer was
    /// never read from a file. `BufEnter` fires on **every** entry. `InsertEnter`
    /// fires on a transition *into* insert (covering `i/a/o/C/cc/s/…` without
    /// touching the core insert chokepoints — the diff sees the result). A cheap
    /// no-op for the vast majority of keys, which change neither buffer nor mode.
    pub(crate) fn emit_lifecycle_events(&mut self) {
        let buf = self.editor.current_buffer_id();
        let mode = self.editor.mode;
        let cur_win = self.editor.current_window_id();
        let wins = self.editor.window_ids();

        let unannounced = !self.announced.contains(&buf);
        let entered = self.last_buffer_id != Some(buf);
        // A transition *into* insert (or replace — neovim fires InsertEnter for
        // both), measured against the last diff so staying in insert won't re-fire.
        let entered_insert = mode.is_insert() && !self.last_mode.is_insert();
        // Track the mode every call — even the no-op fast path — so a later entry
        // is still seen after an insert→normal round trip that took the fast path.
        self.last_mode = mode;

        // Window diff (Phase 5): windows added/closed since the last emit, the
        // focus change, and any rect change. Cheap vecs of ids — computed every
        // call so the fast-path check below sees them.
        let new_wins: Vec<WindowId> = wins
            .iter()
            .copied()
            .filter(|w| !self.known_windows.contains(w))
            .collect();
        let closed_wins: Vec<WindowId> = self
            .known_windows
            .iter()
            .copied()
            .filter(|w| !wins.contains(w))
            .collect();
        let win_changed = self.last_window_id != Some(cur_win);
        let rects = self.window_rects_snapshot();
        let resized = self
            .last_window_rects
            .as_ref()
            .is_some_and(|prev| *prev != rects);

        if !unannounced
            && !entered
            && !entered_insert
            && new_wins.is_empty()
            && closed_wins.is_empty()
            && !win_changed
            && !resized
        {
            return; // fast path: nothing transitioned
        }

        // ----- window leave/new/closed (before the buffer events) -----
        // WinNew for freshly-created windows.
        for w in &new_wins {
            let b = self.editor.window_buffer(*w);
            self.fire_window("WinNew", *w, b);
        }
        // WinLeave for the window we're leaving, then WinClosed for any that are
        // gone (vim's order around a switch is WinLeave → … → WinEnter).
        if win_changed {
            if let Some(old) = self.last_window_id {
                let b = self.editor.window_buffer(old);
                self.fire_window("WinLeave", old, b);
            }
        }
        for w in &closed_wins {
            // The window is already gone, so its buffer is unknown — fire with no
            // buffer context (a buffer-local autocmd can't be bound to it anyway).
            self.fire_window("WinClosed", *w, None);
        }
        self.known_windows = wins;

        let name = self.editor.buffer_name(buf).unwrap_or_default();
        let file_backed = !name.is_empty();

        // Fire-once per buffer, file-backed only: BufReadPost then FileType.
        if unannounced {
            self.announced.insert(buf);
            if file_backed {
                self.fire_lifecycle("BufReadPost", &name, buf, &name);
                // FileType's pattern is the filetype derived from the path; skip
                // it entirely when nothing is detected (matching neovim).
                if let Some(ft) = filetype_of(self.editor.buffer().path.as_deref()) {
                    self.fire_lifecycle("FileType", ft, buf, &name);
                }
            }
        }

        // Fire-every on entry: BufEnter, for both file-backed and [No Name].
        if entered {
            self.last_buffer_id = Some(buf);
            self.fire_lifecycle("BufEnter", &name, buf, &name);
        }

        // Mode event: InsertEnter, with the entered mode's code as the pattern.
        if entered_insert {
            self.fire_lifecycle("InsertEnter", mode.short_code(), buf, &name);
        }

        // ----- window enter / resized (after the buffer events) -----
        if win_changed {
            self.last_window_id = Some(cur_win);
            let b = self.editor.window_buffer(cur_win);
            self.fire_window("WinEnter", cur_win, b);
        }
        if resized {
            let b = self.editor.window_buffer(cur_win);
            self.fire_window("WinResized", cur_win, b);
        }
        self.last_window_rects = Some(rects);
    }

    /// Every window's `(id, rect)` in layout order, for the [`WinResized`] diff.
    pub(crate) fn window_rects_snapshot(&self) -> Vec<WindowRect> {
        self.editor
            .window_ids()
            .into_iter()
            .map(|w| (w, self.editor.window_rect(w).unwrap_or_default()))
            .collect()
    }

    /// Fire a window-lifecycle autocmd (`WinNew`/`WinEnter`/`WinLeave`/
    /// `WinClosed`/`WinResized`). The pattern / `<amatch>` is the window id (as a
    /// string, like neovim); the callback's buffer context is the window's buffer
    /// when known. Mirrors [`Server::fire_lifecycle`] for buffer events.
    pub(crate) fn fire_window(&mut self, event: &str, win: WindowId, buf: Option<BufferId>) {
        let pattern = win.0.to_string();
        let (bufnr, file) = match buf {
            Some(b) => (b.0, self.editor.buffer_name(b).unwrap_or_default()),
            None => (0, String::new()),
        };
        // Keep the buffer mirror in lockstep, as the buffer-event path does.
        self.push_buf_mirror();
        if let Err(e) = self.lua.fire_autocmd_buf(event, &pattern, bufnr, &file) {
            self.editor
                .echo(format!("E5108: Error in {event} autocmd: {e}"));
        }
        self.apply_lua_effects();
    }

    /// Push the current-buffer snapshot into the VM, fire `event` for `pattern` /
    /// `file` with buffer context, surface any callback error, and fold in the Lua
    /// effects the callbacks left. Deferred ex-commands the callbacks queue are
    /// drained by the caller's `run_pending`.
    pub(crate) fn fire_lifecycle(&mut self, event: &str, pattern: &str, buf: BufferId, file: &str) {
        let ft = filetype_of(self.editor.buffer().path.as_deref()).unwrap_or("");
        let _ = self.lua.set_buf_snapshot(buf.0, file, ft);
        // Keep the buffer mirror in lockstep: an autocmd callback runs here before
        // the caller's `run_pending`, so refresh `vim._bufs` / the cursor too.
        self.push_buf_mirror();
        if let Err(e) = self.lua.fire_autocmd_buf(event, pattern, buf.0, file) {
            self.editor
                .echo(format!("E5108: Error in {event} autocmd: {e}"));
        }
        self.apply_lua_effects();
    }

    /// Source a startup Lua file (the user's `init.lua`). Missing files are
    /// skipped silently — having no config is normal. A Lua error surfaces on
    /// the message line; effects are drained through the same path as `:lua`.
    pub(crate) fn source_init(&mut self, path: &Path) {
        let src = match std::fs::read_to_string(path) {
            Ok(src) => src,
            Err(_) => return,
        };
        if let Err(e) = self.lua.exec(&src) {
            self.editor
                .echo(format!("E5113: Error while sourcing init.lua: {e}"));
        }
        self.apply_lua_effects();
        self.run_pending();
    }
}
