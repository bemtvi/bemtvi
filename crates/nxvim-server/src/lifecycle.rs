//! Buffer/mode lifecycle autocmd emission: the server-side diff that fires
//! `BufReadPost`/`FileType`/`BufEnter`/`InsertEnter`/`ModeChanged`, and `init.lua`
//! sourcing.

#[cfg(feature = "native")]
use crate::evloop::LoopCommand;
use crate::filetype_of;
use crate::{EditHost, WindowRect};
#[cfg(feature = "native")]
use crate::{FsRead, WatchEvent, INTERNAL_WATCH_BASE};
use nxvim_core::{
    BufferId, DirEntry, FileChangeAction, FileChangeReason, FileStat, TabId, WindowId,
};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

impl EditHost {
    /// Apply the result of an off-tick fetch (`docs/plans/2026-06-09-edit-host-and-browser-lua.md`
    /// → Phase 3, fs leg) into `buffer` — the deferred startup file (which fills the
    /// initial `[No Name]` buffer) or a later `:edit`. An existing file's bytes or a
    /// new-file marker load into the named replica buffer; a directory becomes the
    /// in-window file explorer (Phase 3g); a genuine read error (a transport failure) is
    /// echoed loudly rather than left as a silent empty buffer.
    /// While the fetch was in flight the editor served the client with the (empty)
    /// buffer — the whole point of fetching the remote file off the editor tick.
    #[cfg(feature = "native")]
    pub(crate) fn apply_open(
        &mut self,
        buffer: BufferId,
        path: String,
        result: io::Result<FsRead>,
    ) {
        // A reserved preview fetch (the off-tick branch of `ensure_preview`): route the
        // bytes to the picker preview cache, NOT into a buffer — a read-only preview must
        // not run buffer lifecycle (BufReadPost / FileType). The caller (`on_opens`)
        // repaints via `settle_events`.
        if buffer == crate::redraw::PREVIEW_FETCH_BUF {
            let (lines, ok) = match result {
                Ok(FsRead::File(bytes, _)) => (crate::redraw::bytes_to_preview_lines(&bytes), true),
                Ok(FsRead::New) => (Vec::new(), true),
                Ok(FsRead::Dir { .. }) => (vec!["<directory>".to_string()], false),
                Err(e) => (vec![format!("{path}: {e}")], false),
            };
            self.apply_preview(path, lines, ok);
            return;
        }
        match result {
            // Decode through the shared seam (latin1/utf-16/BOM detection +
            // invalid-UTF-8 resilience), exactly as the local `Buffer::from_file`
            // does — no more `from_utf8_lossy` fork that silently mangled non-UTF-8
            // bytes and corrupted them on the next `:w`.
            Ok(FsRead::File(bytes, stat)) => {
                self.load_replica_bytes(buffer, path, &bytes, true, stat)
            }
            Ok(FsRead::New) => self.load_replica_bytes(buffer, path, b"", false, None),
            // A directory: fill the file-explorer listing from the entries the fetch read
            // (the off-tick fetch is what classified the path as a directory — a local
            // `std::fs` stat can't see a remote path). The daemon's canonical `dir` path
            // supersedes the requested one. The listing is the same shape the explorer
            // plugin produces locally, so its navigation / decor work over the wire.
            Ok(FsRead::Dir { path: dir, entries }) => {
                // A reload can't resolve to a directory; drop a stale post marker.
                self.reload_posts.remove(&buffer);
                // A workspace edit never targets a directory; drop any stranded stash.
                self.pending_replica_edits.remove(&buffer);
                self.load_dir_listing(buffer, dir, entries);
            }
            Err(e) => {
                // The off-tick re-fetch failed — surface it loudly; no reload happened,
                // so no FileChangedShellPost (the buffer is untouched). A workspace edit
                // that was waiting on this file can't apply — drop its stash and report.
                self.reload_posts.remove(&buffer);
                if self.pending_replica_edits.remove(&buffer).is_some() {
                    self.editor.echo(format!(
                        "apply_workspace_edit: could not open {path} over the daemon: {e}"
                    ));
                } else {
                    self.editor
                        .echo(format!("nxvim: could not open {path} over the daemon: {e}"));
                }
            }
        }
    }

    /// Load the remote file's raw `bytes` into `buffer` as a replica of the file named
    /// `path`, then fire the events a fresh read implies. `load_bytes_into` decodes
    /// through the shared encoding seam (so a remote open matches a local one) and
    /// replaces the named buffer's content in place; clearing it from `announced` lets
    /// the now-named buffer's `BufReadPost`/`FileType` fire — `FileType` is what drives
    /// syntax and LSP. The filetype comes from `path` directly (the buffer is named for
    /// it), so this works whether or not `buffer` is current. Then refresh the Lua
    /// snapshot/mirror and drive the queued autocmd work.
    ///
    /// `existed` is whether the remote path was an *existing* file ([`FsRead::File`]) versus
    /// a `:e new-file` ([`FsRead::New`]); `stat` is that file's stat at read time (the daemon
    /// stats it during the read). For an existing file, stamp the `disk` baseline — before
    /// `emit_lifecycle_events` checks `buffer_is_new_file` — so the remote read fires
    /// `BufReadPost`, not `BufNewFile`, and the watch leg's `fs_changed` compares against an
    /// accurate snapshot. A new file keeps its `None` stat (fires `BufNewFile`, as
    /// `vim newfile` does).
    #[cfg(feature = "native")]
    fn load_replica_bytes(
        &mut self,
        buffer: BufferId,
        path: String,
        bytes: &[u8],
        existed: bool,
        stat: Option<nxvim_core::FileStat>,
    ) {
        self.editor
            .load_bytes_into(buffer, Some(path.clone()), bytes);
        if existed {
            self.editor.mark_replica_read_from_disk(buffer, stat);
        }
        // A project-wide rename / code action whose edits reached this (off-tick)
        // file stashed them while the fetch was in flight; apply them now that the
        // real contents have landed, before lifecycle events fire so a
        // `BufReadPost`-driven LSP attach / diagnostics see the renamed text.
        self.apply_pending_replica_edit(buffer);
        self.announced.remove(&buffer);
        self.fired_filetype.remove(&buffer);
        let ft = filetype_of(Some(Path::new(&path))).unwrap_or("");
        let _ = self.lua.set_buf_snapshot(buffer.0, &path, ft);
        self.push_buf_mirror();
        self.emit_lifecycle_events();
        // A remote watch reload (the `HostWatch` leg) deferred its `FileChangedShellPost`
        // to this landing point — fire it now, before `run_pending`, so a handler's
        // queued work drains in the same convergence.
        if self.reload_posts.remove(&buffer) {
            self.fire_file_changed_post(buffer);
        }
        self.run_pending();
    }

    /// Fill `buffer` with the file-explorer listing of remote directory `dir` from an
    /// off-tick fetch's `entries` — the directory analogue of [`load_replica_bytes`],
    /// shared by the native ([`apply_open`](Self::apply_open)) and wasm
    /// ([`complete_fs_read_dir`](Self::complete_fs_read_dir)) off-tick paths. A *remote*
    /// directory is filled server-side from the entries the fetch already read, rather
    /// than re-read by the explorer plugin's `nx.fs` (the `nx.fs` op and the file-open
    /// fetch are separate legs; a daemon may wire only the latter). The result is the
    /// same shape the plugin's local fill produces — a `nomodifiable`, `filetype=nxdir`
    /// buffer named for `dir` — so the plugin's stateless navigation and the decor
    /// provider work identically. Setting the filetype fires `FileType nxdir`, which
    /// installs the activation maps; clearing `announced` lets the now-named buffer's
    /// lifecycle re-fire.
    pub(crate) fn load_dir_listing(
        &mut self,
        buffer: BufferId,
        dir: String,
        entries: Vec<DirEntry>,
    ) {
        let text = nxvim_core::dir_listing(entries);
        self.editor
            .load_bytes_into(buffer, Some(dir.clone()), text.as_bytes());
        // The fill is a read, not an edit; hold it read-only via the `'modifiable'`
        // option (the explorer has no core buffer-kind), and mark it `nxdir` so its
        // `FileType nxdir` autocmd installs the activation maps and the decor provider
        // colours it.
        self.editor
            .set_buffer_option_bool(buffer, "modifiable", false);
        self.editor
            .set_buffer_option_str(buffer, "filetype", "nxdir");
        self.announced.remove(&buffer);
        self.fired_filetype.remove(&buffer);
        let _ = self.lua.set_buf_snapshot(buffer.0, &dir, "nxdir");
        self.push_buf_mirror();
        self.emit_lifecycle_events();
        self.run_pending();
    }

    /// Dispatch the buffer opens core deferred this convergence (off-tick `:edit` over
    /// the daemon wire): each is fetched over `HostFsAsync` on a spawned task that
    /// delivers `(buffer, path, result)` back to the `open_rx` arm, which fills the
    /// named buffer. Called at the tail of [`run_pending`](EditHost::run_pending), so an
    /// `:edit` from a keystroke, `vim.cmd('edit ...')`, or a user command is caught
    /// after the editor converges. A no-op when off-tick mode is off or none ran.
    /// Fire `BufReadCmd` for a deferred open and report whether a handler **claimed**
    /// the read (vim's "replace the default read" hook). The handler owns filling
    /// `buffer` (e.g. the file explorer's listing of a directory), so a `true` return
    /// tells the caller to skip the default load. Gated on a `BufReadCmd` handler being
    /// registered, so the common path never crosses into Lua. The path is the autocmd
    /// `pattern` (so a handler can scope itself, e.g. only directories) and the
    /// `<afile>`/`<amatch>` arg; `is_dir` is surfaced as `args.isdir` (the one fs fact a
    /// `*Cmd` handler routinely branches on — the explorer claims directories, declines
    /// files — passed in because the live Lua fs surface is async). The buffer mirror is
    /// refreshed first so the handler can read the (empty) buffer. A throwing handler is
    /// surfaced loud and treated as *unclaimed* (the default read then runs).
    fn fire_buf_read_cmd(&mut self, buffer: BufferId, path: &Path) -> bool {
        if !self.au_active_events.contains("BufReadCmd") {
            return false;
        }
        let path_str = path.to_string_lossy().into_owned();
        // Register the (empty, named) buffer in the Lua mirror before firing so the
        // handler sees a valid, modifiable buffer it can `nvim_buf_set_lines` into —
        // the same snapshot the off-tick landing publishes. The filetype derives from
        // the path; a directory has none (the handler sets `nxdir` itself).
        let ft = filetype_of(Some(Path::new(&path_str))).unwrap_or("");
        let _ = self.lua.set_buf_snapshot(buffer.0, &path_str, ft);
        self.push_buf_mirror();
        // Whether `<amatch>` is a directory — surfaced as `args.isdir`, the fs fact the
        // explorer branches on (claim directories, decline files). A local `std::fs`
        // stat: this fires only for a *local* deferred open (a daemon directory is filled
        // server-side at the fetch landing, not via `BufReadCmd`), so the path is on this
        // machine and the stat is accurate.
        let is_dir = path.is_dir();
        match self
            .lua
            .fire_autocmd_cmd("BufReadCmd", &path_str, buffer.0, is_dir)
        {
            Ok(claimed) => {
                if claimed {
                    // Settle the handler's buffer edits / queued ops now, before the
                    // caller moves on (and before any default read it suppressed). A
                    // direct `apply_lua_effects` drains the `nvim_buf_set_lines` op and
                    // the handler's `set_filetype` / extmark / command effects — this
                    // runs *inside* the post-convergence `drain_pending_opens`, where a
                    // nested `run_pending` does not reach the buffer-op drain.
                    self.apply_lua_effects();
                }
                claimed
            }
            Err(e) => {
                self.editor
                    .echo(format!("E5108: Error in BufReadCmd for {path_str}: {e}"));
                false
            }
        }
    }

    pub(crate) fn drain_pending_opens(&mut self) {
        let opens = self.editor.take_pending_opens();
        if opens.is_empty() {
            return;
        }
        // Make sure the autocmd cache reflects the latest registrations before deciding
        // whether a `BufReadCmd` handler can claim these opens. The per-input-batch path
        // refreshes this, but a drain can also run during startup (a deferred `nxvim .`
        // directory, drained while sourcing the prelude / `init.lua` / package plugins),
        // before any input batch — without this the explorer's just-registered handler
        // would be missed and the directory read as a file.
        self.refresh_au_events();
        let remote = self.fx.has_remote_fs();
        let win = self.editor.current_window_id();
        let tab = self.editor.current_tab_id();
        for open in opens {
            // BufReadCmd (vim's "replace the read" hook): a Lua handler may claim this
            // open and own filling the buffer — netrw / the explorer-as-plugin rides
            // this. A claimed read skips the default load entirely (and, per `*Cmd`
            // semantics, BufReadPost too — the handler sets the filetype, which fires
            // FileType). An open only reaches here when deferred, which a local session
            // does *only* when a BufReadCmd handler is registered, so the fire is never
            // wasted on the common no-handler path.
            if self.fire_buf_read_cmd(open.buffer, &open.path) {
                continue;
            }
            if !remote {
                // A deferred open no handler claimed, in a local (synchronous) session:
                // read it now through `host_fs` and drive the lifecycle events the read
                // implies, the synchronous counterpart of the off-tick `apply_open`
                // landing below.
                self.editor.load_pending_open(open);
                self.push_buf_mirror();
                self.emit_lifecycle_events();
                self.run_pending();
                continue;
            }
            // Resolve a *relative* open against the effective working dir (the edit-host's
            // `DirState`, which a remote `:cd` moves) before it crosses the wire. The
            // daemon serves many sessions and keeps no per-session process cwd, so a bare
            // relative path would resolve against its launch dir and silently ignore
            // `:cd` — so `:cd dir` then `:e file` must read `<dir>/file`. An absolute path
            // crosses unchanged. See `docs/plans/2026-06-23-remote-cwd.md`.
            let path = if open.path.is_relative() {
                let (_, base) = self.dirs.effective(win, tab);
                base.join(&open.path).to_string_lossy().into_owned()
            } else {
                open.path.display().to_string()
            };
            self.fx.fs_fetch(open.buffer, path);
        }
    }

    /// Diff the editor's current buffer against what was last announced and fire
    /// the buffer-lifecycle autocmds the transition implies — the central,
    /// server-side emission point (design D1) that keeps `nxvim-core` free of
    /// event types. Called after each applied input (per key in [`EditHost::input`],
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
        // Buffers the editor read from a file *in place* this tick (a local `:edit`
        // reusing the throwaway `[No Name]`, or a `:e` / `:e!` reload of the current
        // file) keep their bufnr, so they're still "announced" from a prior life. Drop
        // them from `announced` / `fired_filetype` so the read re-fires `BufReadPost`
        // (`BufNewFile`) and `FileType` below — neovim fires those on every read,
        // regardless of whether the buffer id was seen before. The off-tick read path
        // clears these itself when its fetched bytes land (`load_replica_bytes`); this
        // covers the synchronous local read that has no such landing hook.
        for buf in self.editor.take_loaded_in_place() {
            self.announced.remove(&buf);
            self.fired_filetype.remove(&buf);
        }

        let buf = self.editor.current_buffer_id();
        let mode = self.editor.mode;
        let cur_win = self.editor.current_window_id();
        let wins = self.editor.window_ids();

        // A buffer with an open still pending (a deferred `:edit` — every local open now
        // defers behind the explorer's `BufReadCmd` handler, and an off-tick open always
        // does) is named but **empty**: its content lands later this convergence
        // (`drain_pending_opens` → `load_pending_open` / the off-tick landing). Hold its
        // read lifecycle (`BufReadPost`/`FileType`/`BufEnter`) until then, so those fire
        // once over the filled buffer rather than prematurely on the empty one (which
        // would double the `FileType`). `BufLeave` for the buffer being left is unaffected.
        let pending_open = self.editor.has_pending_open(buf);

        let unannounced = !self.announced.contains(&buf);
        // FileType fires on the buffer's first announce *and* whenever its filetype
        // changes (neovim's `:setfiletype` behavior) — including an in-place buffer
        // reuse across kinds (throwaway → `nxdir` listing, file → directory), which
        // keeps the same id and so stays "announced". Tracked separately from
        // `announced` (which gates the once-only `BufReadPost`).
        let cur_ft = self.editor.buffer_filetype(buf);
        let ft_changed = self.fired_filetype.get(&buf) != Some(&cur_ft);
        let entered = self.last_buffer_id != Some(buf);
        // A transition *into* insert (or replace — neovim fires InsertEnter for
        // both), measured against the last diff so staying in insert won't re-fire.
        let old_mode = self.last_mode;
        let entered_insert = mode.is_insert() && !old_mode.is_insert();
        // The mirror edge: a transition *out of* insert fires `InsertLeave`.
        let left_insert = !mode.is_insert() && old_mode.is_insert();
        // Track the mode every call — even the no-op fast path — so a later entry
        // is still seen after an insert→normal round trip that took the fast path.
        self.last_mode = mode;

        // `ModeChanged` fires on any change to the *reported* `mode()` code — so a
        // Normal↔MultiCursor swap fires `n:m` / `m:n` (MultiCursor reports its own
        // `m`) — with the pattern `old:new` (e.g. "n:i"), matched by a handler's glob (`*:i`, `n:*`,
        // `*:*`, …) exactly as in neovim; a handler reads the transition off
        // `args.match`. Gated on a registered handler so a no-listener session never
        // even builds the pattern string.
        let old_code = old_mode.short_code();
        let new_code = mode.short_code();
        let mode_changed = old_code != new_code && self.au_active_events.contains("ModeChanged");

        // Cursor / text diffs (gated on a registered handler so a bare motion costs
        // nothing when nothing listens). `CursorMoved`(I) fires when the focused
        // window's cursor moves *within the same buffer*; `TextChanged`(I) when the
        // current buffer's `changedtick` advances. Both are suppressed on the same
        // diff as an insert mode-change so `a`/`o`/`<Esc>` don't fire a spurious move
        // (the reposition is part of the transition, as in neovim). The baselines are
        // refreshed unconditionally below — even on the fast-path return — so enabling
        // a handler later can't fire once off a stale position.
        let mode_edge = entered_insert || left_insert;
        let cur_pos = (buf, self.editor.cursor.line, self.editor.cursor.col);
        let cursor_event = if mode.is_insert() {
            "CursorMovedI"
        } else {
            "CursorMoved"
        };
        let want_cursor = self.au_active_events.contains(cursor_event);
        let cursor_moved = !mode_edge
            && want_cursor
            && self
                .last_cursor
                .is_some_and(|(b, l, c)| b == buf && (l, c) != (cur_pos.1, cur_pos.2));

        let cur_tick = self.editor.changedtick_of(buf).unwrap_or(0);
        let text_event = if mode.is_insert() {
            "TextChangedI"
        } else {
            "TextChanged"
        };
        let want_text = self.au_active_events.contains(text_event);
        let text_changed = want_text
            && self
                .last_text
                .is_some_and(|(b, t)| b == buf && t != cur_tick);

        // Refresh the cursor / text baselines every call (mirrors `last_mode`), so a
        // motion that took the fast path is still the reference for the next diff.
        self.last_cursor = Some(cur_pos);
        self.last_text = Some((buf, cur_tick));

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

        // Scroll diff: any window whose `(topline, leftcol)` changed fires
        // `WinScrolled`. Snapshotted unconditionally — like the `WinResized` rect diff
        // above and unlike the `CursorMoved` gate — so the baseline is always current
        // and the *first* scroll after a handler is registered still fires (the fire
        // itself is gated on a handler below). Compare only windows present in both
        // snapshots: a window added/closed since the last diff is a `WinNew`/
        // `WinClosed`, not a scroll.
        let scroll = self.window_scroll_snapshot();
        let scrolled = self.last_window_scroll.as_ref().is_some_and(|prev| {
            scroll.iter().any(|&(w, t, l)| {
                prev.iter()
                    .any(|&(pw, pt, pl)| pw == w && (pt, pl) != (t, l))
            })
        });

        // Tab diff (Phase 3): tabs added/closed and the active-tab change since the
        // last emit. A tab transition always coincides with a window transition (a
        // switch changes the focused window; create/close add or drop windows), but
        // we still fold these into the fast-path guard so a tab event can never be
        // swallowed.
        // Buffer diff: ids gone since the last emit (a `:bdelete` / `nvim_buf_delete`).
        // Each one's Lua-side buffer-local state (commands, keymaps) is purged below so
        // a later buffer reusing the bufnr can't inherit it.
        let live_bufs = self.editor.buffer_ids();
        let closed_bufs: Vec<BufferId> = self
            .known_buffers
            .iter()
            .copied()
            .filter(|b| !live_bufs.contains(b))
            .collect();

        let cur_tab = self.editor.current_tab_id();
        let tabs = self.editor.tab_ids();
        let new_tabs: Vec<TabId> = tabs
            .iter()
            .copied()
            .filter(|t| !self.known_tabs.contains(t))
            .collect();
        let closed_tabs: Vec<TabId> = self
            .known_tabs
            .iter()
            .copied()
            .filter(|t| !tabs.contains(t))
            .collect();
        let tab_changed = self.last_tab_id != Some(cur_tab);

        if !unannounced
            && !ft_changed
            && !entered
            && !entered_insert
            && !left_insert
            && !mode_changed
            && !cursor_moved
            && !text_changed
            && new_wins.is_empty()
            && closed_wins.is_empty()
            && !win_changed
            && !resized
            && !scrolled
            && new_tabs.is_empty()
            && closed_tabs.is_empty()
            && !tab_changed
            && closed_bufs.is_empty()
        {
            return; // fast path: nothing transitioned
        }

        // ----- tab new / leave (outermost, before the window events) -----
        // `TabNew` for freshly-created tabs, then `TabLeave` for the tab we're
        // leaving — vim brackets a switch as `TabLeave → WinLeave → … → WinEnter
        // → TabEnter`, so the tab-leave fires before the window-leave below.
        for t in &new_tabs {
            self.fire_tab("TabNew", *t);
        }
        if tab_changed {
            if let Some(old) = self.last_tab_id {
                self.fire_tab("TabLeave", old);
            }
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
            // Drop its window-local dir (`:lcd`) so a reused window id can't inherit it.
            self.dirs.forget_window(*w);
        }
        self.known_windows = wins;

        let name = self.editor.buffer_name(buf).unwrap_or_default();
        let file_backed = !name.is_empty();

        // Fire-once per buffer (gated by `announced`): file-backed only, a
        // `[No Name]`/scratch buffer was never read. A buffer whose file does not
        // exist on disk fires `BufNewFile` *instead of* `BufReadPost` (matching
        // `vim file-that-does-not-exist`); an existing file fires `BufReadPost`.
        if unannounced && !pending_open {
            self.announced.insert(buf);
            if file_backed {
                let event = if self.editor.buffer_is_new_file(buf) {
                    "BufNewFile"
                } else {
                    "BufReadPost"
                };
                self.fire_lifecycle(event, &name, buf, &name);
            }
        }

        // FileType, on first set and on every filetype *change* (see `ft_changed`).
        // The pattern is the buffer's filetype — an explicit one (`:set ft`,
        // `nx.bo.filetype`, or a core-created special buffer's `set_filetype`: the
        // explorer's `nxdir`, the quickfix display's `qf`, a view's `nxview`/content
        // ft) wins; otherwise the path's extension decides. `None` (an extension-less
        // `[No Name]`) skips firing, matching neovim. It fires for non-file-backed
        // buffers too, so a core-created special buffer's `FileType <ft>` autocmd
        // installs its buffer-local maps (the unified special-buffer model — see
        // docs/plans/2026-06-16-unify-special-buffer-kinds.md).
        if ft_changed && !pending_open {
            if let Some(ft) = &cur_ft {
                self.fire_lifecycle("FileType", ft, buf, &name);
            }
            self.fired_filetype.insert(buf, cur_ft);
        }

        // Fire-every on entry: `BufLeave` for the buffer we're leaving, then
        // `BufEnter` for the one we entered (both file-backed and [No Name]). vim
        // brackets a buffer switch as `BufLeave → BufEnter`; the old buffer's name is
        // its own, so fire it with that context before rebinding `last_buffer_id`.
        if entered && !pending_open {
            if let Some(old) = self.last_buffer_id {
                let old_name = self.editor.buffer_name(old).unwrap_or_default();
                self.fire_lifecycle("BufLeave", &old_name, old, &old_name);
            }
            self.last_buffer_id = Some(buf);
            self.fire_lifecycle("BufEnter", &name, buf, &name);
        }

        // Mode events: `InsertEnter` on the transition into insert (the entered
        // mode's code is the pattern), `InsertLeave` on the transition back out (the
        // mode we left). A single diff never sees both edges.
        if entered_insert {
            self.fire_lifecycle("InsertEnter", mode.short_code(), buf, &name);
        }
        if left_insert {
            self.fire_lifecycle("InsertLeave", old_mode.short_code(), buf, &name);
        }

        // `ModeChanged` — the general mode-transition signal (fired after the
        // insert-specific events: the specific event, then the general). The pattern
        // is `old:new` of the reported `mode()` codes; a mode-reactive statusline /
        // cursor-shape plugin matches `*:i`, `n:v`, … against it.
        if mode_changed {
            self.fire_lifecycle("ModeChanged", &format!("{old_code}:{new_code}"), buf, &name);
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

        // `WinScrolled` for every window whose viewport offset changed since the last
        // diff. Fired per-window with that window as `<amatch>` (more useful for a
        // diff / scrollbind plugin than neovim's once-with-`v:event`). The baseline is
        // always rebased (even with no handler), so it stays current; the *fire* is
        // gated on a registered handler so a no-handler session never enters Lua here.
        // It rebases to the pre-callback offsets, so a handler that scrolls *another*
        // window re-fires `WinScrolled` for it next diff — the plugin guards its own
        // sync loop, as in neovim.
        let to_fire: Vec<WindowId> = if self.au_active_events.contains("WinScrolled") {
            match self.last_window_scroll.as_ref() {
                Some(prev) => scroll
                    .iter()
                    .filter(|&&(w, t, l)| {
                        prev.iter()
                            .any(|&(pw, pt, pl)| pw == w && (pt, pl) != (t, l))
                    })
                    .map(|&(w, _, _)| w)
                    .collect(),
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };
        self.last_window_scroll = Some(scroll);
        for win in to_fire {
            let b = self.editor.window_buffer(win);
            self.fire_window("WinScrolled", win, b);
        }

        // ----- tab enter / closed (outermost, after the window events) -----
        // `TabEnter` for the now-active tab (after `WinEnter`, closing the
        // `TabLeave → … → TabEnter` bracket), then `TabClosed` for any tab the
        // transition removed (the tab — and its windows — are already gone).
        if tab_changed {
            self.last_tab_id = Some(cur_tab);
            self.fire_tab("TabEnter", cur_tab);
        }
        for t in &closed_tabs {
            self.fire_tab("TabClosed", *t);
            // Drop its tab-local dir (`:tcd`) so a reused tab id can't inherit it.
            self.dirs.forget_tab(*t);
        }
        self.known_tabs = tabs;

        // ----- text / cursor events (finest grained, after the buffer settles) -----
        // `TextChanged`(I) when the buffer's `changedtick` advanced, then
        // `CursorMoved`(I) when the focused window's cursor moved — both gated on a
        // registered handler (the `want_*` checks folded into the booleans above), so
        // an unwatched motion never reaches here. The `I` variant fires in insert.
        if text_changed {
            let event = if mode.is_insert() {
                "TextChangedI"
            } else {
                "TextChanged"
            };
            self.fire_lifecycle(event, &name, buf, &name);
        }
        if cursor_moved {
            let event = if mode.is_insert() {
                "CursorMovedI"
            } else {
                "CursorMoved"
            };
            self.fire_lifecycle(event, &name, buf, &name);
        }

        // ----- working directory: follow the current window (vim's fix_current_dir) -----
        // A window/tab focus change can make a different scope's local dir effective
        // (`:lcd`/`:tcd` are per-window/-tab), so re-apply the current window's
        // effective dir to the process cwd. Cheap no-op when nothing local is in play.
        if win_changed || tab_changed {
            self.fix_current_dir();
        }

        // ----- buffer deletion cleanup -----
        // A deleted buffer's Lua-side buffer-local commands / keymaps must not
        // outlive it (else a reused bufnr inherits them). The `announced` /
        // fire-once set is pruned in step so a reused id re-announces its events.
        for b in &closed_bufs {
            // `BufDelete` fires *before* the buffer-local cleanup, so a buffer-local
            // `BufDelete` autocmd on this buffer still runs. The buffer is already gone
            // from the store, so its name is unavailable — fire with the bufnr context
            // (a `BufDelete` handler keys off `args.buf`, like neovim's `<abuf>`).
            self.fire_buf_delete(*b);
            if let Err(e) = self.lua.cleanup_buffer(b.0) {
                self.editor
                    .echo(format!("E5108: Error cleaning up buffer {}: {e}", b.0));
            }
            self.announced.remove(b);
            self.fired_filetype.remove(b);
        }
        self.known_buffers = live_bufs;

        // Keep the native per-buffer file watches in step with the live buffer set
        // (arm new file-backed buffers, disarm closed ones, re-arm on a reload/save).
        self.sync_buffer_watches();
    }

    /// Dispatch the persisted `nx.view` slots a session restore reserved. The layout came
    /// back at `shada_load` (before plugins) with each persisted view's slot held by a
    /// placeholder window and recorded in the editor's pending list; now that the config and
    /// the boot-sourced plugins are in place, refresh the `nx._view_pending` mirror and run
    /// the Lua dispatch: each owning plugin whose `nx.view.on_restore` is already registered
    /// recreates its view and adopts its reserved window.
    ///
    /// Collapsing the *unclaimed* slots is NOT done here anymore — it is owned by the Lua
    /// restore coordinator (`nx._maybe_collapse_view_restores`). A plugin loaded via
    /// `nx.plugins({ config = … })` registers its handler asynchronously, on a tick *after*
    /// this boot dispatch, so reaping orphans now would collapse its slot before it ever got
    /// to claim it. Instead `nx._run_view_restores()` collapses immediately only when it can
    /// prove no such async load is in flight (the common no-async-plugin launch — enqueuing a
    /// [`ViewOp::CollapseUnclaimed`] we drain below, still before the window-set seed so no
    /// spurious `WinClosed` fires); otherwise the coordinator collapses once the last eager
    /// load settles. A no-op when nothing was reserved, so an ordinary launch pays nothing.
    pub(crate) fn restore_persisted_views(&mut self) {
        if self.editor.view_pending_restores().is_empty() {
            return;
        }
        let _ = self
            .lua
            .set_view_pending(&self.editor.view_pending_restores());
        if let Err(e) = self.lua.exec("nx._run_view_restores()") {
            self.editor
                .echo(format!("E5117: Error restoring plugin views: {e}"));
        }
        self.apply_lua_effects();
        self.run_pending();
    }

    /// Fire the startup `VimEnter` autocmd — the "editor has finished starting"
    /// hook — then drain its effects. Run once, right after `v:vim_did_enter` is
    /// set: `init.lua` and the package `plugin/` scripts have all run, so a handler
    /// (e.g. the package manager's first-run recommended-plugins prompt) sees a
    /// fully started editor. A registry with no `VimEnter` handler is a cheap no-op.
    /// Errors surface on the message line rather than aborting startup.
    pub(crate) fn fire_vim_enter(&mut self) {
        if let Err(e) = self.lua.exec("nx.autocmd.exec('VimEnter', {})") {
            self.editor
                .echo(format!("E5117: Error executing VimEnter autocommands: {e}"));
        }
        self.apply_lua_effects();
        self.run_pending();
    }

    /// Re-apply the current window's effective directory to the process cwd after a
    /// window / tab focus change (vim's `fix_current_dir`). With `:lcd` / `:tcd` in
    /// play the effective dir differs per window / tab, so a switch must `chdir` so
    /// `vim.fn.getcwd` and relative paths track the focused window. `DirChanged` fires
    /// (scope = the source of the now-effective dir) only when the cwd actually moves,
    /// so a switch between windows that share a dir costs nothing. A vanished target
    /// (its directory was removed under us) leaves the cwd untouched rather than
    /// erroring on a passive switch.
    fn fix_current_dir(&mut self) {
        let win = self.editor.current_window_id();
        let tab = self.editor.current_tab_id();
        let (scope, want) = self.dirs.effective(win, tab);
        let want = want.to_path_buf();
        // Daemon session: there is no local process cwd to re-point (a remote path can't be
        // `set_current_dir`'d here, and the daemon is stateless — it has no per-session
        // cwd). The focused window's effective dir is the cwd, mirrored into `nx._cwd` so
        // `vim.fn.getcwd` follows `:lcd`/`:tcd` between windows. When the switch actually
        // crosses a cwd boundary, announce it with `DirChanged` — the remote analogue of
        // the local branch below.
        if self.editor.host_fs_offtick() {
            let (scope, dir) = self.dirs.effective(win, tab);
            let scope_pat = scope.pattern();
            let dir = dir.display().to_string();
            if self.publish_cwd_mirror() {
                let r = self.lua.fire_dir_changed(scope_pat, &dir);
                self.report_autocmd_err("DirChanged", r);
            }
            return;
        }
        if std::env::current_dir().ok().as_deref() == Some(want.as_path()) {
            return; // already there — nothing to chdir or announce
        }
        if std::env::set_current_dir(&want).is_err() {
            return;
        }
        let cwd = std::env::current_dir().unwrap_or(want);
        self.publish_cwd_mirror();
        let r = self
            .lua
            .fire_dir_changed(scope.pattern(), &cwd.display().to_string());
        self.report_autocmd_err("DirChanged", r);
    }

    /// Reconcile a finished off-tick `:cd` (the daemon `fs_chdir` reply) against the
    /// optimistic move it already applied. `ex_chdir` moved the cwd immediately (so an
    /// `:e` / `getcwd` in the same breath sees the new dir) but deferred the announcing
    /// `DirChanged` to here:
    ///
    /// - **Ok(canonical):** reverse the optimistic intermediate and install the daemon's
    ///   *canonical* dir cleanly (so `:cd -` history and the `prev` pointer are right even
    ///   when a symlink resolved differently), then fire `DirChanged` — but only if a later
    ///   `:cd` hasn't already superseded this one (the rollback's guard reports that, in
    ///   which case the newer `:cd` owns the state and its own announce).
    /// - **Err(E344):** roll the optimistic move back to where the cwd was and echo the
    ///   daemon's loud error. No `DirChanged` ever fired for the rejected dir, so no
    ///   handler ran on it.
    ///
    /// A `""`/`~` target made no optimistic move (`undo == None`) — its home is the
    /// daemon's — so it simply installs on Ok / echoes on Err, the original async path.
    /// The settle + repaint is the caller's ([`on_chdir_dones`](Self::on_chdir_dones)).
    /// See `docs/plans/2026-06-23-remote-cwd.md`.
    pub(crate) fn apply_chdir(&mut self, done: crate::cwd::ChdirDone) {
        let Some(pending) = self.pending_chdirs.remove(&done.token) else {
            return; // unknown token (already reconciled) — nothing to do
        };
        let crate::cwd::PendingChdir {
            scope,
            win,
            tab,
            undo,
        } = pending;
        match done.result {
            Ok(canon) => {
                // Reverse the optimistic move first (if any), so the canonical install
                // records the correct `prev`. A `false` return means a later `:cd`
                // superseded this one — leave its state and skip our announce.
                if let Some(u) = undo {
                    if !self.dirs.rollback_optimistic(u) {
                        return;
                    }
                }
                let dir = PathBuf::from(canon);
                self.dirs.set(scope, win, tab, dir.clone());
                self.publish_cwd_mirror();
                let r = self
                    .lua
                    .fire_dir_changed(scope.pattern(), &dir.display().to_string());
                self.report_autocmd_err("DirChanged", r);
            }
            // The daemon's `E344` (the target isn't a readable directory) or a transport
            // failure — roll back the optimistic move and surface it loud, never a silent
            // wrong-cwd.
            Err(e) => {
                if let Some(u) = undo {
                    if self.dirs.rollback_optimistic(u) {
                        self.publish_cwd_mirror();
                    }
                }
                self.editor.echo(e.to_string());
            }
        }
    }

    /// Push the current effective working directory ([`DirState::effective`]) into the
    /// `nx._cwd` Lua mirror that `vim.fn.getcwd()` reads. Called on every cwd change —
    /// the startup seed, `:cd`/`:tcd`/`:lcd`, and a window/tab focus switch — so the
    /// mirror is the single authoritative cwd for both local and daemon sessions. Cheap
    /// (one `O(1)` effective-dir lookup + a Lua table set). Returns whether the cwd
    /// actually *moved* since the last publish (tracked in `published_cwd`), which a
    /// daemon-session focus switch uses to fire `DirChanged` only on a real boundary.
    pub(crate) fn publish_cwd_mirror(&mut self) -> bool {
        let win = self.editor.current_window_id();
        let tab = self.editor.current_tab_id();
        let (_, dir) = self.dirs.effective(win, tab);
        let dir = dir.to_path_buf();
        let moved = self.published_cwd.as_deref() != Some(dir.as_path());
        if let Err(e) = self.lua.set_cwd(&dir.to_string_lossy()) {
            self.editor
                .echo(format!("E5108: Error publishing cwd mirror: {e}"));
        }
        self.published_cwd = Some(dir);
        moved
    }

    /// Reconcile the server's internal per-buffer file watches against the live
    /// buffers — the watch leg's local auto-trigger. For every file-backed buffer
    /// it arms one native watch (reusing the native fs-watch machinery: a
    /// [`LoopCommand::FsEventStart`] on the file's path) keyed on `(path, disk-stat)`;
    /// a buffer whose key changed (a reload/save re-stamped its disk snapshot, so the
    /// file may be a fresh inode after an atomic replace) is **re-armed** on the same
    /// loop id (which replaces the old watch), and a closed buffer is disarmed
    /// ([`LoopCommand::FsEventStop`]). When the watch fires, the [`LoopEvent::FsEvent`]
    /// arm routes it to `editor.checktime_buffer` (see [`INTERNAL_WATCH_BASE`]), which
    /// autoreloads or warns. Declarative (driven off the current buffer set each tick)
    /// rather than hooked into every open/close/rename site. A **daemon** session arms
    /// the watches on the *daemon* instead (the `HostWatch` leg — see below), since the
    /// edit-host can't watch a remote file with a local `notify`.
    #[cfg(feature = "native")]
    pub(crate) fn sync_buffer_watches(&mut self) {
        if self.fx.has_remote_fs() {
            // The remote watch leg: one watch per file-backed buffer path, armed on the
            // daemon (`HostEffects::fs_watch`). The daemon owns change detection and pushes
            // `fs_changed`, so this tracks only paths — no stat snapshot, no re-arm on a
            // reload (the daemon re-baselines its own view). A `fs_changed` push lands on
            // the `watch_rx` arm and reconciles via `on_remote_file_changed`.
            let want: HashMap<String, Option<FileStat>> = self
                .editor
                .buffer_ids()
                .into_iter()
                .filter_map(|id| self.editor.buffer_watch_key(id))
                .map(|(path, stat)| (path.to_string_lossy().into_owned(), stat))
                .collect();
            for (path, stat) in &want {
                if self.remote_watches.insert(path.clone()) {
                    // Pass the buffer's disk baseline so a re-dialed daemon (which lost its
                    // own baselines) detects a change made while the link was down — the
                    // reconnect resync clears `remote_watches` to force this re-arm.
                    self.fx.fs_watch(path.clone(), *stat);
                }
            }
            let stale: Vec<String> = self
                .remote_watches
                .iter()
                .filter(|p| !want.contains_key(*p))
                .cloned()
                .collect();
            for path in stale {
                self.remote_watches.remove(&path);
                self.fx.fs_unwatch(path);
            }
            return;
        }
        // Desired: one watch per file-backed buffer, keyed on (path, disk snapshot).
        // A new-file buffer not yet written has a path but a `None` disk snapshot —
        // nothing on disk to watch. kqueue/inotify can't watch an absent path, so
        // arming one fails, and the arm-failure handler would re-arm the same dead key
        // forever (an unbounded arm→fail→re-arm storm that repaints every cycle). Skip
        // those here: the watch arms on the next sync once `:w` creates the file (which
        // re-stamps the disk snapshot, changing the key).
        let want: HashMap<BufferId, (PathBuf, Option<FileStat>)> = self
            .editor
            .buffer_ids()
            .into_iter()
            .filter_map(|id| self.editor.buffer_watch_key(id).map(|k| (id, k)))
            .filter(|(_, (_, stat))| stat.is_some())
            .collect();

        // Arm or re-arm anything new or whose key changed (FsEventStart on an
        // existing id replaces its watch, so a re-arm needs no explicit stop).
        for (id, key) in &want {
            if self.buf_watches.get(id) != Some(key) {
                self.fx.loop_command(LoopCommand::FsEventStart {
                    id: INTERNAL_WATCH_BASE + id.0,
                    path: key.0.to_string_lossy().into_owned(),
                    recursive: false,
                });
                self.buf_watches.insert(*id, key.clone());
            }
        }
        // Disarm watches for buffers that are gone (or lost their file).
        let stale: Vec<BufferId> = self
            .buf_watches
            .keys()
            .filter(|id| !want.contains_key(id))
            .copied()
            .collect();
        for id in stale {
            self.fx.loop_command(LoopCommand::FsEventStop {
                id: INTERNAL_WATCH_BASE + id.0,
            });
            self.buf_watches.remove(&id);
        }
    }

    /// The browser's per-buffer watch reconcile — the **remote** branch only (the wasm
    /// build has no `notify`, so no local-disk leg exists). Arms one watch per file-backed
    /// buffer path through the off-tick effect seam ([`HostEffects::fs_watch`]); in a daemon
    /// session the Worker forwards each as an `fs_watch` / `fs_unwatch` over the wire and a
    /// `fs_changed` push lands via `eh_remote_file_changed` → [`Self::reconcile_remote_change`].
    /// Mirrors the native `has_remote_fs()` branch (paths only — the daemon owns change
    /// detection, so no stat snapshot / re-arm). A serverless OPFS session has no external
    /// writer to watch, so the Worker simply drops the arm. A no-op when no off-tick fs is
    /// wired (it never is on this build, but keep the gate honest).
    #[cfg(not(feature = "native"))]
    pub(crate) fn sync_buffer_watches(&mut self) {
        if !self.fx.has_remote_fs() {
            return;
        }
        let want: HashMap<String, Option<FileStat>> = self
            .editor
            .buffer_ids()
            .into_iter()
            .filter_map(|id| self.editor.buffer_watch_key(id))
            .map(|(path, stat)| (path.to_string_lossy().into_owned(), stat))
            .collect();
        for (path, stat) in &want {
            if self.remote_watches.insert(path.clone()) {
                // Pass the buffer's disk baseline so a re-dialed daemon detects a change made
                // while the link was down — the reconnect resync clears `remote_watches` to
                // force this re-arm (Phase 7).
                self.fx.fs_watch(path.clone(), *stat);
            }
        }
        let stale: Vec<String> = self
            .remote_watches
            .iter()
            .filter(|p| !want.contains_key(*p))
            .cloned()
            .collect();
        for path in stale {
            self.remote_watches.remove(&path);
            self.fx.fs_unwatch(path);
        }
    }

    /// Land a daemon link **phase** (`"connected"` / `"reconnecting"` / `"disconnected"`) on the
    /// editor off the tick — the shared body both the native run-loop status arm
    /// ([`on_daemon_status`](Self::on_daemon_status)) and the wasm `eh_daemon_status` FFI drive.
    /// Mirrors the phase into `nx.daemon.status()` and fires the `User DaemonStatusChanged`
    /// autocmd (so a statusline component re-renders), and on a genuine reconnect (`reconnected`)
    /// re-syncs the remote seams ([`resync_after_reconnect`](Self::resync_after_reconnect)) before
    /// settling. The native-only `:reconnect` hint on give-up is echoed by the caller, not here.
    pub fn apply_daemon_phase(&mut self, phase: &str, reconnected: bool) {
        // `phase` is one of three fixed literals from the supervisor, so the formatted chunk is
        // injection-safe.
        if let Err(e) = self.lua.exec(&format!("nx._set_daemon_status('{phase}')")) {
            self.editor.echo(format!("DaemonStatusChanged error: {e}"));
        }
        self.apply_lua_effects();
        if reconnected {
            self.resync_after_reconnect();
        }
        self.settle_events(true);
    }

    /// Re-establish the remote seams after a reconnect, off the editor tick. The editor's local
    /// state (buffers, undo, cursor, windows, Lua) survived the outage; only the daemon-backed
    /// seams need rebinding, which the supervisor already did underneath. This re-arms what the
    /// *fresh* daemon doesn't know about:
    ///
    /// - **fs watches** — clear the armed set so [`sync_buffer_watches`](Self::sync_buffer_watches)
    ///   re-sends `fs_watch` for every open buffer (native: carrying its disk baseline, so the
    ///   daemon detects a file changed *during* the outage).
    /// - **LSP** — re-open every server against the new connection
    ///   ([`resync_lsp_after_reconnect`](Self::resync_lsp_after_reconnect)).
    /// - **terminals** — a re-dialed daemon lost every PTY, so the live terminal buffers are
    ///   dead. Freeze each as an exited terminal (editable, output preserved) and tell the user.
    ///   Background jobs already surfaced their own `-1` exit when the link dropped.
    ///
    /// Shared by the native run loop and the wasm edit-host (the daemon-reconnect plan's Phase 7).
    pub(crate) fn resync_after_reconnect(&mut self) {
        self.remote_watches.clear();
        self.sync_buffer_watches();
        self.resync_lsp_after_reconnect();

        let lost: Vec<BufferId> = self
            .editor
            .buffer_ids()
            .into_iter()
            .filter(|&id| self.editor.is_terminal_buffer(id))
            .collect();
        for &buf in &lost {
            self.editor.terminal_closed(buf, -1);
        }
        if !lost.is_empty() {
            let n = lost.len();
            let noun = if n == 1 { "terminal" } else { "terminals" };
            self.editor.echo(format!(
                "daemon reconnected — {n} remote {noun} lost (reopen with :terminal)"
            ));
        }
        self.apply_lua_effects();
    }

    /// Every window's `(id, rect)` in layout order, for the [`WinResized`] diff.
    pub(crate) fn window_rects_snapshot(&self) -> Vec<WindowRect> {
        self.editor
            .window_ids()
            .into_iter()
            .map(|w| (w, self.editor.window_rect(w).unwrap_or_default()))
            .collect()
    }

    /// Every window's `(id, topline, leftcol)` in layout order, for the
    /// [`WinScrolled`] diff. Only computed when a `WinScrolled` handler is active.
    pub(crate) fn window_scroll_snapshot(&self) -> Vec<(WindowId, usize, usize)> {
        self.editor
            .window_ids()
            .into_iter()
            .map(|w| {
                let (top, left) = self.editor.window_scroll(w).unwrap_or((0, 0));
                (w, top, left)
            })
            .collect()
    }

    /// Surface the standard `E5108` echo for an autocmd-firing result: a no-op on
    /// `Ok`, the canonical `Error in {event} autocmd` message on `Err`. The single
    /// home for the error-reporting half every `fire_*` autocmd call site shares.
    pub(crate) fn report_autocmd_err<E: std::fmt::Display>(
        &mut self,
        event: &str,
        res: Result<(), E>,
    ) {
        if let Err(e) = res {
            self.editor
                .echo(format!("E5108: Error in {event} autocmd: {e}"));
        }
    }

    /// Fire `FileChangedShell` for `buf` (changed on disk for `reason`), echoing the
    /// standard error on failure and returning whether a handler ran (so the caller
    /// can honour `v:fcs_choice`). Shared by the local and remote watch reconcilers.
    pub(crate) fn fire_file_changed_checked(
        &mut self,
        reason: FileChangeReason,
        buf: BufferId,
        file: &str,
    ) -> bool {
        match self.lua.fire_file_changed(reason.as_str(), buf.0, file) {
            Ok(fired) => fired,
            Err(e) => {
                self.editor
                    .echo(format!("E5108: Error in FileChangedShell autocmd: {e}"));
                false
            }
        }
    }

    /// Fire a window-lifecycle autocmd (`WinNew`/`WinEnter`/`WinLeave`/
    /// `WinClosed`/`WinResized`). The pattern / `<amatch>` is the window id (as a
    /// string, like neovim); the callback's buffer context is the window's buffer
    /// when known. Mirrors [`EditHost::fire_lifecycle`] for buffer events.
    pub(crate) fn fire_window(&mut self, event: &str, win: WindowId, buf: Option<BufferId>) {
        let pattern = win.0.to_string();
        let (bufnr, file) = match buf {
            Some(b) => (b.0, self.editor.buffer_name(b).unwrap_or_default()),
            None => (0, String::new()),
        };
        // Keep the buffer mirror in lockstep, as the buffer-event path does.
        self.push_buf_mirror();
        let r = self.lua.fire_autocmd_buf(event, &pattern, bufnr, &file);
        self.report_autocmd_err(event, r);
        self.apply_lua_effects();
    }

    /// Fire a tab-lifecycle autocmd (`TabNew`/`TabEnter`/`TabLeave`/`TabClosed`).
    /// The pattern / `<amatch>` is the tab id (as a string, like the window
    /// events); the callback's buffer context is the tab's focused window's buffer
    /// when the tab still exists (`None` for a `TabClosed` tab, already gone).
    /// Mirrors [`EditHost::fire_window`] for tab events.
    pub(crate) fn fire_tab(&mut self, event: &str, tab: TabId) {
        let pattern = tab.0.to_string();
        let buf = self
            .editor
            .tab_current_window(tab)
            .and_then(|w| self.editor.window_buffer(w));
        let (bufnr, file) = match buf {
            Some(b) => (b.0, self.editor.buffer_name(b).unwrap_or_default()),
            None => (0, String::new()),
        };
        // Keep the buffer mirror in lockstep, as the window-event path does.
        self.push_buf_mirror();
        let r = self.lua.fire_autocmd_buf(event, &pattern, bufnr, &file);
        self.report_autocmd_err(event, r);
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
        // the caller's `run_pending`, so refresh `nx._bufs` / the cursor too.
        self.push_buf_mirror();
        let r = self.lua.fire_autocmd_buf(event, pattern, buf.0, file);
        self.report_autocmd_err(event, r);
        self.apply_lua_effects();
    }

    /// Fire the write autocmds for `buf` (written to `path`): `BufWritePre`, its
    /// synonym `BufWrite`, then `BufWritePost`, each with the written buffer as
    /// context (`<afile>` = its path). Driven from [`EditHost::drain_write_events`]
    /// after a successful `:w` / `:wall` or a finalized off-tick save, so a write
    /// fires the same events however it reached disk. The filetype is resolved from
    /// the *written* buffer's own path (so a `:wall` of a non-current buffer carries
    /// the right `vim.bo.filetype`), unlike the generic [`fire_lifecycle`] which keys
    /// off the current buffer.
    ///
    /// NOTE on timing: nxvim's editing core writes synchronously and Lua autocmds
    /// fire at convergence (the server can't re-enter Lua mid-write), so `BufWritePre`
    /// fires just *before* `BufWritePost` but *after* the bytes are on disk. A
    /// `BufWritePre` handler cannot transform what is written — but nxvim has no
    /// synchronous buffer-mutation Lua API either, so that is not reachable regardless;
    /// the firing order (`Pre` → `Post`) and buffer context match neovim.
    pub(crate) fn fire_buf_write(&mut self, buf: BufferId, path: &str) {
        let ft = filetype_of(Some(Path::new(path))).unwrap_or("");
        let _ = self.lua.set_buf_snapshot(buf.0, path, ft);
        self.push_buf_mirror();
        for event in ["BufWritePre", "BufWrite", "BufWritePost"] {
            let r = self.lua.fire_autocmd_buf(event, path, buf.0, path);
            self.report_autocmd_err(event, r);
        }
        self.apply_lua_effects();
    }

    /// Fire `BufDelete` for a buffer that was just removed from the store (a
    /// `:bdelete` / `nvim_buf_delete`). The buffer is already gone, so there's no
    /// name / snapshot to set — fire with the bufnr as context (`args.buf`, neovim's
    /// `<abuf>`), which is what a cleanup handler keys off. Driven from
    /// [`emit_lifecycle_events`] before the buffer-local Lua state is purged, so a
    /// buffer-local `BufDelete` autocmd on that buffer still runs.
    pub(crate) fn fire_buf_delete(&mut self, buf: BufferId) {
        let pattern = buf.0.to_string();
        self.push_buf_mirror();
        let r = self.lua.fire_autocmd_buf("BufDelete", &pattern, buf.0, "");
        self.report_autocmd_err("BufDelete", r);
        self.apply_lua_effects();
    }

    /// Drain the buffers written this convergence (core's [`Editor::take_write_events`])
    /// and fire each one's `BufWritePre`/`BufWritePost` ([`fire_buf_write`]). Called
    /// inside [`run_pending`](EditHost::run_pending)'s fixpoint so a write driven from a
    /// keystroke, `vim.cmd('w')`, a user command, or a daemon save ack fires its events
    /// in the same convergence — and a handler that itself queues work (`vim.cmd`) has
    /// that work drained by the surrounding loop.
    pub(crate) fn drain_write_events(&mut self) {
        let writes = self.editor.take_write_events();
        if writes.is_empty() {
            return;
        }
        for (buf, path) in writes {
            let path = path.display().to_string();
            self.fire_buf_write(buf, &path);
        }
        // `fire_buf_write` points the Lua snapshot (`nx._cur_buf`) at the *written*
        // buffer, which for a `:wall` may not be the current one — re-seed it to the
        // editor's actual current buffer so a later `expand('%')` / `nvim_buf_get_name(0)`
        // isn't left reading the last-written buffer.
        let cur = self.editor.current_buffer_id();
        let name = self.editor.buffer_name(cur).unwrap_or_default();
        let ft = filetype_of(self.editor.buffer().path.as_deref()).unwrap_or("");
        let _ = self.lua.set_buf_snapshot(cur.0, &name, ft);
        self.push_buf_mirror();
    }

    /// Reconcile one file-backed buffer against its disk state — the server half of
    /// `:checktime` and the per-buffer file watch, owning the `FileChangedShell`
    /// round-trip the pure core can't drive. Core's [`Editor::begin_file_change`]
    /// detects the change and applies the no-autocmd part (a silent autoread reload of
    /// an unmodified buffer); this fires `FileChangedShell` for everything else, honors
    /// the `v:fcs_choice` the handler set, and fires `FileChangedShellPost` after a
    /// handled change — the structure of neovim's `buf_check_timestamp`:
    ///
    /// - **no handler** → the default warning (E211/W12/W11) via
    ///   [`Editor::warn_file_change`], then `FileChangedShellPost`.
    /// - **handler, `v:fcs_choice = "reload"`/`"edit"`** → [`Editor::reload_buffer`]
    ///   (`"reload"` is refused for a deleted file, as in neovim), then the post event.
    /// - **handler, `"ask"`** → fall through to the default warning, then the post event.
    /// - **handler, choice left empty** → the handler took over: nothing further, and
    ///   (matching neovim's early `return 2`) **no** `FileChangedShellPost`.
    #[cfg(feature = "native")]
    pub(crate) fn reconcile_file_change(&mut self, buf: BufferId) {
        match self.editor.begin_file_change(buf) {
            FileChangeAction::None => {}
            FileChangeAction::Reloaded => self.fire_file_changed_post(buf),
            FileChangeAction::Autocmd(reason) => {
                let Some(file) = self.editor.buffer_name(buf) else {
                    return;
                };
                // A `FileChangedShell` handler commonly reads buffer state and `vim.v`;
                // refresh the mirror so it sees the live buffer before the round-trip.
                self.push_buf_mirror();
                let fired = self.fire_file_changed_checked(reason, buf, &file);
                self.apply_lua_effects();
                // A handler that left `v:fcs_choice` empty is the one case neovim fires
                // no `FileChangedShellPost` for (it `return 2`s); every other path warns
                // or reloads and then fires the post event.
                let mut fire_post = true;
                if fired {
                    let choice = self.lua.fcs_choice().unwrap_or_default();
                    match choice.as_str() {
                        "reload" if reason != FileChangeReason::Deleted => {
                            self.editor.reload_buffer(buf)
                        }
                        "edit" => self.editor.reload_buffer(buf),
                        "ask" => self.editor.warn_file_change(buf, reason),
                        _ => fire_post = false,
                    }
                } else {
                    self.editor.warn_file_change(buf, reason);
                }
                if fire_post {
                    self.fire_file_changed_post(buf);
                }
            }
        }
    }

    /// Fire `FileChangedShellPost` for `buf` after a file change was handled — even
    /// when nothing reloaded (a warning-only change still fires it, as neovim does).
    /// A cheap no-op when no handler is registered; skipped if the buffer is gone.
    pub(crate) fn fire_file_changed_post(&mut self, buf: BufferId) {
        let Some(file) = self.editor.buffer_name(buf) else {
            return;
        };
        self.push_buf_mirror();
        let r = self
            .lua
            .fire_autocmd_buf("FileChangedShellPost", &file, buf.0, &file);
        self.report_autocmd_err("FileChangedShellPost", r);
        self.apply_lua_effects();
    }

    /// Reconcile a daemon-pushed file change (the `HostWatch` leg's `fs_changed`) — the
    /// remote analogue of [`EditHost::reconcile_file_change`]. The daemon owns change
    /// detection and self-suppresses the edit-host's own writes, so a push always means
    /// a real external change; the reason follows from the pushed stat (vanished ⇒
    /// `"deleted"`) and the buffer's modified flag (unsaved ⇒ `"conflict"`). The
    /// `FileChangedShell` round-trip is identical to the local path, but a reload can't
    /// be synchronous (it crosses the wire), so it goes off-tick via [`Self::remote_reload`]
    /// and the `FileChangedShellPost` fires when the re-fetch lands.
    #[cfg(feature = "native")]
    pub(crate) fn on_remote_file_changed(&mut self, ev: WatchEvent) {
        let WatchEvent { path, stat } = ev;
        self.reconcile_remote_change(path, stat);
    }

    /// Reconcile one daemon-pushed file change (a `fs_changed` for `path` with its new
    /// `stat`, `None` = vanished) — the shared body of the remote watch leg, driven natively
    /// by [`Self::on_remote_file_changed`] (off the run loop's `watch_rx` arm) and in the
    /// browser by [`EditHost::remote_file_changed`] (off `RpcClient.onNotify`). The wire
    /// types ([`WatchEvent`]) are native-only, so the entry points hand in the decomposed
    /// `(path, stat)` instead. See [`Self::on_remote_file_changed`] for the full contract.
    pub(crate) fn reconcile_remote_change(&mut self, path: String, stat: Option<FileStat>) {
        let Some(buf) = self.editor.find_buffer_by_path(Path::new(&path)) else {
            return; // the buffer was closed since the watch was armed
        };
        let reason = if stat.is_none() {
            FileChangeReason::Deleted
        } else if self.editor.buffer_modified(buf) {
            FileChangeReason::Conflict
        } else {
            FileChangeReason::Changed
        };
        // 'autoread', unmodified, still-present file → silent reload (no FileChangedShell),
        // then FileChangedShellPost once the re-fetch lands — the same pre-autocmd branch
        // as the local path, just off-tick.
        if reason == FileChangeReason::Changed && self.editor.autoread() {
            self.remote_reload(buf);
            return;
        }
        // A FileChangedShell handler may read buffer state and `vim.v`.
        self.push_buf_mirror();
        let fired = self.fire_file_changed_checked(reason, buf, &path);
        self.apply_lua_effects();
        let mut do_reload = false;
        let mut fire_post = true;
        if fired {
            let choice = self.lua.fcs_choice().unwrap_or_default();
            match choice.as_str() {
                "reload" if reason != FileChangeReason::Deleted => do_reload = true,
                "edit" => do_reload = true,
                "ask" => self.editor.warn_file_change(buf, reason),
                _ => fire_post = false,
            }
        } else {
            self.editor.warn_file_change(buf, reason);
        }
        if do_reload {
            // The deferred FileChangedShellPost fires when the off-tick re-fetch lands.
            self.remote_reload(buf);
        } else if fire_post {
            self.fire_file_changed_post(buf);
        }
    }

    /// Drive an off-tick reload of `buf` over the daemon wire: enqueue a re-fetch of its
    /// own file and mark it for a `FileChangedShellPost` once the bytes land in
    /// [`EditHost::apply_open`]. A no-op for a buffer with no path (nothing to re-fetch).
    fn remote_reload(&mut self, buf: BufferId) {
        if self.editor.enqueue_reload(buf) {
            self.reload_posts.insert(buf);
        }
    }

    /// Source a startup Lua file (the user's `init.lua`). Missing files are
    /// skipped silently — having no config is normal. A Lua error surfaces on
    /// the message line; effects are drained through the same path as `:lua`. The
    /// script runs to completion synchronously (its options, mappings, and
    /// colorscheme are in place by the time the first frame goes out), matching
    /// neovim sourcing `init.lua` before serving the UI.
    pub(crate) fn source_init(&mut self, path: &Path) {
        let src = match std::fs::read_to_string(path) {
            Ok(src) => src,
            Err(_) => return, // no config = nothing to source
        };
        let name = format!("@{}", path.display());
        if let Err(e) = self.lua.exec_named(&src, &name) {
            self.editor
                .echo(format!("E5113: Error while sourcing init.lua: {e}"));
        }
        self.apply_lua_effects();
        self.run_pending();
    }

    /// Source the package `plugin/` Lua scripts found across the runtimepath, then
    /// every entry's `after/plugin/` last — neovim's startup package-load order
    /// (`pack/*/start/*` plugins included), run *after* `init.lua`. This is what
    /// lets a plugin's `plugin/` script wire its autocmds / register itself (e.g.
    /// a completion plugin's engine, a source's `register_source`) — without it a plugin that
    /// merely `require`s cleanly still never initializes. Each entry's
    /// `plugin/**/*.lua` is sourced sorted (recursing); a script error surfaces on
    /// the message line and does not abort the rest. Effects drain through the same
    /// path as `:lua`. Runs before the first buffer's lifecycle events fire, so a
    /// plugin's `FileType` / `BufReadPost` autocmds catch the initial buffer.
    pub(crate) fn source_plugins(&mut self) {
        // Clone the paths so the immutable runtimepath borrow doesn't outlive the
        // mutable `self` use while sourcing.
        let runtimepath: Vec<PathBuf> = self.lua.runtimepath().to_vec();
        for sub in ["plugin", "after/plugin"] {
            for rt in &runtimepath {
                for file in collect_lua_scripts(&rt.join(sub)) {
                    let src = match std::fs::read_to_string(&file) {
                        Ok(src) => src,
                        Err(_) => continue,
                    };
                    let name = format!("@{}", file.display());
                    if let Err(e) = self.lua.exec_named(&src, &name) {
                        self.editor
                            .echo(format!("Error sourcing {}: {e}", file.display()));
                    }
                    self.apply_lua_effects();
                }
            }
        }
        self.run_pending();
    }
}

/// Every `*.lua` file under `dir` (recursing into subdirectories), sorted by path
/// for a deterministic, neovim-like load order. A missing/unreadable directory
/// yields nothing.
fn collect_lua_scripts(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_lua_into(dir, &mut out);
    out.sort();
    out
}

/// Recursive helper for [`collect_lua_scripts`]: append every `*.lua` file under
/// `dir` to `out`.
fn collect_lua_into(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_lua_into(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("lua") {
            out.push(path);
        }
    }
}
