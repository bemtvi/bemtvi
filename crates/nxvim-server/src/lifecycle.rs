//! Buffer/mode lifecycle autocmd emission: the server-side diff that fires
//! `BufReadPost`/`FileType`/`BufEnter`/`InsertEnter`, and `init.lua` sourcing.

use crate::filetype_of;
use crate::Server;
use nxvim_core::BufferId;
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
        let unannounced = !self.announced.contains(&buf);
        let entered = self.last_buffer_id != Some(buf);
        // A transition *into* insert (or replace — neovim fires InsertEnter for
        // both), measured against the last diff so staying in insert won't re-fire.
        let entered_insert = mode.is_insert() && !self.last_mode.is_insert();
        // Track the mode every call — even the no-op fast path — so a later entry
        // is still seen after an insert→normal round trip that took the fast path.
        self.last_mode = mode;
        if !unannounced && !entered && !entered_insert {
            return; // fast path: nothing transitioned
        }

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
