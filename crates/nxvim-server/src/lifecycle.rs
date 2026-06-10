//! Buffer/mode lifecycle autocmd emission: the server-side diff that fires
//! `BufReadPost`/`FileType`/`BufEnter`/`InsertEnter`, and `init.lua` sourcing.

use crate::evloop::LoopCommand;
use crate::filetype_of;
use crate::{FsRead, WatchEvent};
use crate::{Server, WindowRect, INTERNAL_WATCH_BASE};
use nxvim_core::{
    BufferId, DirEntry, FileChangeAction, FileChangeReason, FileStat, TabId, WindowId,
};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

impl Server {
    /// Apply the result of an off-tick fetch (`docs/plans/2026-06-09-edit-host-and-browser-lua.md`
    /// → Phase 3, fs leg) into `buffer` — the deferred startup file (which fills the
    /// initial `[No Name]` buffer) or a later `:edit`. An existing file's bytes or a
    /// new-file marker load into the named replica buffer; a directory becomes the
    /// in-window file explorer (Phase 3g); a genuine read error (a transport failure) is
    /// echoed loudly rather than left as a silent empty buffer.
    /// While the fetch was in flight the editor served the client with the (empty)
    /// buffer — the whole point of fetching the remote file off the editor tick.
    pub(crate) fn apply_open(
        &mut self,
        buffer: BufferId,
        path: String,
        result: io::Result<FsRead>,
    ) {
        match result {
            Ok(FsRead::File(bytes)) => {
                self.load_replica(buffer, path, &String::from_utf8_lossy(&bytes))
            }
            Ok(FsRead::New) => self.load_replica(buffer, path, ""),
            // A directory: build the in-window file explorer listing into the buffer
            // (Phase 3g). The daemon's canonical `dir` path supersedes the requested one
            // (`:e somedir` resolves to its absolute form), so the listing names and
            // navigates from it.
            Ok(FsRead::Dir { path: dir, entries }) => {
                // A reload can't resolve to a directory; drop a stale post marker.
                self.reload_posts.remove(&buffer);
                self.load_dir_replica(buffer, dir, entries)
            }
            Err(e) => {
                // The off-tick re-fetch failed — surface it loudly; no reload happened,
                // so no FileChangedShellPost (the buffer is untouched).
                self.reload_posts.remove(&buffer);
                self.editor
                    .echo(format!("nxvim: could not open {path} over the daemon: {e}"))
            }
        }
    }

    /// Load `contents` into `buffer` as a replica of the remote file named `path`, then
    /// fire the events a fresh read implies. `load_str_into` replaces the named buffer's
    /// content in place; clearing it from `announced` lets the now-named buffer's
    /// `BufReadPost`/`FileType` fire — `FileType` is what drives syntax and LSP. The
    /// filetype comes from `path` directly (the buffer is named for it), so this works
    /// whether or not `buffer` is current. Then refresh the Lua snapshot/mirror and
    /// drive the queued autocmd work.
    fn load_replica(&mut self, buffer: BufferId, path: String, contents: &str) {
        self.editor
            .load_str_into(buffer, Some(path.clone()), contents);
        self.announced.remove(&buffer);
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

    /// Build the file-explorer listing of remote directory `dir` into `buffer` from the
    /// off-tick `read_dir` reply (Phase 3g — the directory analogue of [`load_replica`]).
    /// `load_dir_into` replaces the buffer with the listing (its `dir` marker routes
    /// keys to the explorer); clearing `announced` lets the now-named buffer's
    /// `BufReadPost` fire. A directory has no filetype, so no `FileType`/LSP work — just
    /// refresh the Lua snapshot/mirror and drive the queued autocmd work.
    fn load_dir_replica(&mut self, buffer: BufferId, dir: String, entries: Vec<DirEntry>) {
        self.editor
            .load_dir_into(buffer, PathBuf::from(&dir), entries);
        self.announced.remove(&buffer);
        let _ = self.lua.set_buf_snapshot(buffer.0, &dir, "");
        self.push_buf_mirror();
        self.emit_lifecycle_events();
        self.run_pending();
    }

    /// Dispatch the buffer opens core deferred this convergence (off-tick `:edit` over
    /// the daemon wire): each is fetched over `HostFsAsync` on a spawned task that
    /// delivers `(buffer, path, result)` back to the `open_rx` arm, which fills the
    /// named buffer. Called at the tail of [`run_pending`](Server::run_pending), so an
    /// `:edit` from a keystroke, `vim.cmd('edit ...')`, or a user command is caught
    /// after the editor converges. A no-op when off-tick mode is off or none ran.
    pub(crate) fn drain_pending_opens(&mut self) {
        let Some(fs) = self.host_fs_async.clone() else {
            return;
        };
        for open in self.editor.take_pending_opens() {
            let fs = fs.clone();
            let tx = self.open_tx.clone();
            let buffer = open.buffer;
            let path = open.path.display().to_string();
            tokio::spawn(async move {
                let result = fs.read(path.clone()).await;
                let _ = tx.send((buffer, path, result));
            });
        }
    }

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

        // Tab diff (Phase 3): tabs added/closed and the active-tab change since the
        // last emit. A tab transition always coincides with a window transition (a
        // switch changes the focused window; create/close add or drop windows), but
        // we still fold these into the fast-path guard so a tab event can never be
        // swallowed.
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
            && !entered
            && !entered_insert
            && new_wins.is_empty()
            && closed_wins.is_empty()
            && !win_changed
            && !resized
            && new_tabs.is_empty()
            && closed_tabs.is_empty()
            && !tab_changed
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
        }
        self.known_tabs = tabs;

        // Keep the native per-buffer file watches in step with the live buffer set
        // (arm new file-backed buffers, disarm closed ones, re-arm on a reload/save).
        self.sync_buffer_watches();
    }

    /// Reconcile the server's internal per-buffer file watches against the live
    /// buffers — the watch leg's local auto-trigger. For every file-backed buffer
    /// it arms one native watch (reusing the `vim.uv.fs_event` machinery: a
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
    pub(crate) fn sync_buffer_watches(&mut self) {
        if let Some(fs) = self.host_fs_async.clone() {
            // The remote watch leg: one watch per file-backed buffer path, armed on the
            // daemon (`HostFsAsync::watch`). The daemon owns change detection and pushes
            // `fs_changed`, so this tracks only paths — no stat snapshot, no re-arm on a
            // reload (the daemon re-baselines its own view). A `fs_changed` push lands on
            // the `watch_rx` arm and reconciles via `on_remote_file_changed`.
            let want: HashSet<String> = self
                .editor
                .buffer_ids()
                .into_iter()
                .filter_map(|id| self.editor.buffer_watch_key(id))
                .map(|(path, _)| path.to_string_lossy().into_owned())
                .collect();
            for path in &want {
                if self.remote_watches.insert(path.clone()) {
                    fs.watch(path.clone());
                }
            }
            let stale: Vec<String> = self
                .remote_watches
                .iter()
                .filter(|p| !want.contains(*p))
                .cloned()
                .collect();
            for path in stale {
                self.remote_watches.remove(&path);
                fs.unwatch(path);
            }
            return;
        }
        // Desired: one watch per file-backed buffer, keyed on (path, disk snapshot).
        let want: HashMap<BufferId, (PathBuf, Option<FileStat>)> = self
            .editor
            .buffer_ids()
            .into_iter()
            .filter_map(|id| self.editor.buffer_watch_key(id).map(|k| (id, k)))
            .collect();

        // Arm or re-arm anything new or whose key changed (FsEventStart on an
        // existing id replaces its watch, so a re-arm needs no explicit stop).
        for (id, key) in &want {
            if self.buf_watches.get(id) != Some(key) {
                self.evloop.send(LoopCommand::FsEventStart {
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
            self.evloop.send(LoopCommand::FsEventStop {
                id: INTERNAL_WATCH_BASE + id.0,
            });
            self.buf_watches.remove(&id);
        }
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

    /// Fire a tab-lifecycle autocmd (`TabNew`/`TabEnter`/`TabLeave`/`TabClosed`).
    /// The pattern / `<amatch>` is the tab id (as a string, like the window
    /// events); the callback's buffer context is the tab's focused window's buffer
    /// when the tab still exists (`None` for a `TabClosed` tab, already gone).
    /// Mirrors [`Server::fire_window`] for tab events.
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
                let fired = match self.lua.fire_file_changed(reason.as_str(), buf.0, &file) {
                    Ok(fired) => fired,
                    Err(e) => {
                        self.editor
                            .echo(format!("E5108: Error in FileChangedShell autocmd: {e}"));
                        false
                    }
                };
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
    fn fire_file_changed_post(&mut self, buf: BufferId) {
        let Some(file) = self.editor.buffer_name(buf) else {
            return;
        };
        self.push_buf_mirror();
        if let Err(e) = self
            .lua
            .fire_autocmd_buf("FileChangedShellPost", &file, buf.0, &file)
        {
            self.editor
                .echo(format!("E5108: Error in FileChangedShellPost autocmd: {e}"));
        }
        self.apply_lua_effects();
    }

    /// Reconcile a daemon-pushed file change (the `HostWatch` leg's `fs_changed`) — the
    /// remote analogue of [`Server::reconcile_file_change`]. The daemon owns change
    /// detection and self-suppresses the edit-host's own writes, so a push always means
    /// a real external change; the reason follows from the pushed stat (vanished ⇒
    /// `"deleted"`) and the buffer's modified flag (unsaved ⇒ `"conflict"`). The
    /// `FileChangedShell` round-trip is identical to the local path, but a reload can't
    /// be synchronous (it crosses the wire), so it goes off-tick via [`Self::remote_reload`]
    /// and the `FileChangedShellPost` fires when the re-fetch lands.
    pub(crate) fn on_remote_file_changed(&mut self, ev: WatchEvent) {
        let WatchEvent { path, stat } = ev;
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
        let fired = match self.lua.fire_file_changed(reason.as_str(), buf.0, &path) {
            Ok(fired) => fired,
            Err(e) => {
                self.editor
                    .echo(format!("E5108: Error in FileChangedShell autocmd: {e}"));
                false
            }
        };
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
    /// [`Server::apply_open`]. A no-op for a buffer with no path (nothing to re-fetch).
    fn remote_reload(&mut self, buf: BufferId) {
        if self.editor.enqueue_reload(buf) {
            self.reload_posts.insert(buf);
        }
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

    /// Source the package `plugin/` Lua scripts found across the runtimepath, then
    /// every entry's `after/plugin/` last — neovim's startup package-load order
    /// (`pack/*/start/*` plugins included), run *after* `init.lua`. This is what
    /// lets a plugin's `plugin/` script wire its autocmds / register itself (e.g.
    /// nvim-cmp's engine, cmp-buffer's `register_source`) — without it a plugin that
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
