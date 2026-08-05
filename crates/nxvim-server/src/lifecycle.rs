//! Buffer/mode lifecycle autocmd emission: the server-side diff that fires
//! `BufReadPost`/`FileType`/`BufEnter`/`InsertEnter`/`ModeChanged`, and `init.lua`
//! sourcing.

#[cfg(feature = "native")]
use crate::evloop::LoopCommand;
use crate::filetype_of;
use crate::{EditHost, ExitStage, ReadChain, ReadStage, WindowRect};
#[cfg(feature = "native")]
use crate::{FsRead, WatchEvent, INTERNAL_WATCH_BASE};
use nxvim_core::{
    BufferId, CommitOutcome, DirEntry, FileChangeAction, FileChangeReason, FileStat, TabId,
    WindowId,
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
                // A directory has no text encoding; drop any stashed `++enc` override.
                self.forced_fetch_enc.remove(&buffer);
                // A workspace edit never targets a directory; drop any stranded stash.
                self.pending_replica_edits.remove(&buffer);
                self.load_dir_listing(buffer, dir, entries);
            }
            Err(e) => {
                // The off-tick re-fetch failed — surface it loudly; no reload happened,
                // so no FileChangedShellPost (the buffer is untouched). A workspace edit
                // that was waiting on this file can't apply — drop its stash and report.
                self.reload_posts.remove(&buffer);
                // The forced-encoding reload never landed; drop its stash too.
                self.forced_fetch_enc.remove(&buffer);
                // The probe never landed, so we do not know whether the file is there:
                // never write over it on a guess (that is the clobber this path exists
                // to avoid). The failure is reported below like any other.
                self.pending_create_writes.remove(&buffer);
                // No bytes ⇒ no line text to convert a goto's column against, ever.
                self.pending_goto_cols.remove(&buffer);
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
    /// `vim newfile` does). A serverless OPFS read carries no stat either way
    /// ([`EditHost::complete_fs_read`] passes `None` — a size-only baseline).
    pub(crate) fn load_replica_bytes(
        &mut self,
        buffer: BufferId,
        path: String,
        bytes: &[u8],
        existed: bool,
        stat: Option<nxvim_core::FileStat>,
    ) {
        // A `:e ++enc=` override the drain stashed for this in-flight fetch forces the
        // decode; ordinary opens have no entry and decode through `'fileencodings'`.
        let force_enc = self.forced_fetch_enc.remove(&buffer);
        self.editor
            .load_bytes_into_enc(buffer, Some(path.clone()), bytes, force_enc.as_deref());
        if existed {
            self.editor.mark_replica_read_from_disk(buffer, stat);
        }
        // A project-wide rename / code action whose edits reached this (off-tick)
        // file stashed them while the fetch was in flight; apply them now that the
        // real contents have landed, before lifecycle events fire so a
        // `BufReadPost`-driven LSP attach / diagnostics see the renamed text.
        self.apply_pending_replica_edit(buffer);
        // A `create` that could only ask the filesystem off-tick (`ignoreIfExists` in a
        // daemon / browser session) gets its answer here: `existed` is the probe's
        // result, and decides whether the file is written out or deliberately left as
        // the server found it.
        self.settle_workspace_create(buffer, existed);
        // A goto whose target file this fetch was opening: the core's landing put the
        // cursor on the recorded line with the protocol `character` as a raw byte
        // column, and only now is the line's text here to convert it exactly. After the
        // edits above, which may have moved that text.
        self.settle_pending_goto(buffer);
        // This is a read landing *in place* — the same fact the synchronous paths record
        // (`load_into_current` / `load_pending_open`), reported here because the off-tick
        // read lands in the server rather than in the core. `emit_lifecycle_events` drains
        // it below and does the rest: drop the buffer from `announced` / `fired_filetype`
        // / `fired_encoding` so the read re-fires `BufReadPost`/`FileType` (silently
        // re-seeding the encoding baseline), and fire `BufWinEnter` for a re-read of a
        // *displayed* buffer — which changes no window's buffer, so nothing else in the
        // diff would see it. Recording it rather than clearing the three sets by hand is
        // what keeps the remote tier identical to the local one instead of a near-copy
        // that drifts.
        self.editor.mark_loaded_in_place(buffer);
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
        // A fresh read re-seeds the encoding baseline silently (no `EncodingChanged`).
        self.fired_encoding.remove(&buffer);
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
            // A `:e ++enc=` override can't ride the async fetch (it lands keyed only by
            // buffer); stash it so the landing decodes with the forced encoding.
            if let Some(enc) = open.force_encoding {
                self.forced_fetch_enc.insert(open.buffer, enc);
            }
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
    ///
    /// `BufWinEnter` is the exception that walks **every** window, not just the
    /// current buffer: it fires for each window whose displayed buffer differs from
    /// its baseline this diff, so a session/workspace restore (which fills
    /// non-current windows the current-buffer path never visits) fires it per
    /// restored window.
    pub(crate) fn emit_lifecycle_events(&mut self) {
        // Buffers the editor read from a file *in place* this tick (a local `:edit`
        // reusing the throwaway `[No Name]`, or a `:e` / `:e!` reload of the current
        // file) keep their bufnr, so they're still "announced" from a prior life. Drop
        // them from `announced` / `fired_filetype` so the read re-fires `BufReadPost`
        // (`BufNewFile`) and `FileType` below — neovim fires those on every read,
        // regardless of whether the buffer id was seen before. The off-tick read reports
        // itself the same way when its fetched bytes land (`load_replica_bytes` →
        // `Editor::mark_loaded_in_place`), so both tiers share this one path — including
        // the `BufWinEnter` a re-read owes, which the window diff below cannot see.
        let reread = self.editor.take_loaded_in_place();
        for buf in &reread {
            self.announced.remove(buf);
            self.fired_filetype.remove(buf);
            // A re-read re-detects the encoding; drop the baseline so it re-seeds
            // silently rather than firing `EncodingChanged` for the fresh read.
            self.fired_encoding.remove(buf);
        }

        // A window that inherited its buffer from the window it was split off is its own
        // `BufWinEnter` baseline — nothing was *displayed* there. Seeded here, before the
        // fast-path guard, so the record can never be left undrained by a tick that
        // transitions nothing else. A real display into that window in the same tick
        // (`:split file` is a split *then* a load) moves it off this baseline again, so
        // the walk below still fires.
        for (w, b) in self.editor.take_inherited_windows() {
            self.known_window_buffers.insert(w, b);
        }

        let buf = self.editor.current_buffer_id();
        let mode = self.editor.mode;
        let cur_win = self.editor.current_window_id();
        // Internal doc-float windows (hover / signature / completion / cmdline docs) are
        // UI surfaces, not user windows: exclude them so opening / replacing / moving one
        // (the completion docs float refreshes every keystroke) fires no user
        // `WinNew`/`WinClosed` autocmds — the window twin of the doc-float *buffer* being
        // kept out of `:ls`.
        // `all_window_ids` (every tab of every open layer), **not** `window_ids` (the
        // active tab only): a window parked in a background tab is live — neovim reports
        // it, `nvim_list_wins` lists it — so the diff must keep seeing it. Enumerating
        // the active tab alone made leaving a tab read as "those windows closed" and
        // arriving as "these windows are new", firing a spurious
        // `WinNew`/`WinClosed`/`WinResized`/`BufWinEnter` on every `gt`.
        let wins: Vec<WindowId> = self
            .editor
            .all_window_ids()
            .into_iter()
            .filter(|w| !self.editor.is_doc_float_window(*w))
            .collect();

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
        // `EncodingChanged` (alias `FileEncoding`) fires when the current buffer's
        // `'fileencoding'` *changes* — but NOT on the first sighting: opening a file
        // at its detected encoding is not a change (neovim's global `encoding` stays
        // fixed, so it fires nothing there either). So `enc_changed` requires a prior
        // recorded value that differs; the baseline is seeded silently below.
        let cur_enc = self.editor.buffer_fileencoding(buf).unwrap_or_default();
        let enc_changed = matches!(self.fired_encoding.get(&buf), Some(prev) if prev != &cur_enc);
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

        // Leaving insert resumes diagnostics: apply whatever a server (or
        // `vim.diagnostic.set`) published while `update_in_insert` held it back. Done
        // here — on the mode *diff*, ahead of the fast-path guard and of the
        // `InsertLeave` fire below — so it can't be missed by an exit path that emits
        // no event, and so an `InsertLeave` handler reading diagnostics sees the
        // resumed set rather than the frozen one. A no-op when nothing was held.
        if left_insert {
            self.commit_pending_diagnostics();
        }

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

        // BufWinEnter diff: any window now displaying a buffer other than what the
        // baseline recorded for it — including an *existing, non-current* window
        // whose buffer was swapped (`nvim_win_set_buf`, a session restore filling
        // background windows). None of the other signals above catch that (the
        // current buffer / focus / rects / scroll are all unchanged), so it must be
        // folded into the fast-path guard or a background display change is
        // swallowed. Computed unconditionally (like `scrolled`), so the baseline is
        // rebuilt below even when no handler is registered; the *fire* is gated.
        let bufwin_changed = wins
            .iter()
            .any(|w| self.editor.window_buffer(*w) != self.known_window_buffers.get(w).copied());

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
        // Buffers added to the list since the last emit fire `BufAdd` (neovim alias
        // `BufCreate`) — the symmetric twin of `closed_bufs`/`BufDelete`. The fire is
        // gated on `startup_bufs_seeded` below so the startup buffer never fires it
        // (like `WinNew`/`TabNew` skip the initial window/tab); only buffers created
        // *after* startup do. A file open into a fresh bufnr fires `BufAdd` before its
        // `BufReadPost`; a `:e` that reuses the throwaway `[No Name]` id fires no
        // `BufAdd` (no new id).
        let new_bufs: Vec<BufferId> = live_bufs
            .iter()
            .copied()
            .filter(|b| !self.known_buffers.contains(b))
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
            && !enc_changed
            && new_bufs.is_empty()
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
            && !bufwin_changed
            // A re-read is already drained off the editor, so returning here would lose
            // the `BufWinEnter` the walk below owes it (`:e!` changes no window's buffer,
            // and a reload of a *non-current* buffer moves none of the other signals).
            && reread.is_empty()
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

        // The file-backed test that used to gate `BufReadPost` here now lives inside
        // `drive_read_chain`, which owns the announce sequence.
        let name = self.editor.buffer_name(buf).unwrap_or_default();

        // ----- BufAdd: a buffer newly added to the list -----
        // Fires before the buffer's `BufReadPost` (a file open into a fresh bufnr
        // adds the buffer, then reads it), matching neovim's order. Each fires with
        // its *own* name/context, not the current buffer's, so a `:badd file` that
        // adds without entering still carries the added buffer as `<afile>`.
        // Gated on the startup baseline having been seeded: config sourcing can
        // trigger an early `emit_lifecycle_events` (a `run_pending` at the end of a
        // config that queued work) *before* `known_buffers` is seeded, which would
        // otherwise see the startup buffer as "new" and fire a spurious `BufAdd` for
        // it. The seed (post-config, after any session/view restore churn — hence it
        // can't move earlier) sets `startup_bufs_seeded`, so only buffers added after
        // startup fire `BufAdd`, matching how `WinNew`/`TabNew` skip the initial ones.
        if self.startup_bufs_seeded {
            for b in &new_bufs {
                self.fire_buf_event("BufAdd", *b);
            }
        }

        // ----- BufLeave: the buffer we are leaving, *before* the new one is read -----
        // neovim's order on `:edit other` is `BufLeave` → `BufReadPost` → `BufEnter`:
        // leaving happens first, and only then does the read of what we arrived at run.
        // Firing it after the announce below instead put a plugin's "save this buffer's
        // state on the way out" handler *after* the incoming buffer's `BufReadPost` had
        // already restored state for the new one. Its `BufEnter` twin stays below, on the
        // far side of the chain, because entering is what the chain orders.
        //
        // The old buffer's name is its own, so it carries that context; `last_buffer_id`
        // is rebound with the `BufEnter` fire, not here, so nothing between them can read
        // a half-applied switch.
        if entered && !pending_open {
            if let Some(old) = self.last_buffer_id {
                let old_name = self.editor.buffer_name(old).unwrap_or_default();
                self.fire_lifecycle("BufLeave", &old_name, old, &old_name);
            }
        }

        // Fire-once per buffer (gated by `announced`): file-backed only, a
        // `[No Name]`/scratch buffer was never read. A buffer whose file does not
        // exist on disk fires `BufNewFile` *instead of* `BufReadPost` (matching
        // `vim file-that-does-not-exist`); an existing file fires `BufReadPost`.
        // Driven as a *gated chain* (`BufReadPost` → `FileType` → deferred `BufEnter`)
        // so each stage's async handlers settle before the next fires. With no async
        // handler — nearly always — the whole chain completes inside this one call and
        // the timing is exactly what it was before. See `drive_read_chain`.
        if unannounced && !pending_open {
            self.announced.insert(buf);
            self.begin_read_chain(buf);
        }

        // Re-read the filetype / fileencoding now that `BufReadPost`/`BufNewFile` has
        // run: a read callback may set either (`vim.bo.filetype = "python"` from a
        // shebang/content detector — the canonical "detect filetype in a
        // `BufReadPost`" pattern — or `vim.bo.fileencoding = …`), and that write
        // already landed via `apply_lua_effects` inside `fire_lifecycle` above. The
        // snapshots at the top of this pass predate it, so the `FileType` /
        // `EncodingChanged` decisions below must read the *current* values —
        // otherwise `FileType` fires for the stale pre-callback filetype (a spurious
        // `FileType <detected>` before the real `FileType python`) and only reaches
        // the callback's filetype a diff later via the `run_pending` re-diff,
        // decoupled from `BufReadPost` and out of neovim's `BufReadPost → FileType`
        // order; the encoding baseline seeded below would likewise record the stale
        // value and fire a late `EncodingChanged`.
        let cur_ft = self.editor.buffer_filetype(buf);
        let ft_changed = self.fired_filetype.get(&buf) != Some(&cur_ft);
        let cur_enc = self.editor.buffer_fileencoding(buf).unwrap_or_default();
        let enc_changed = matches!(self.fired_encoding.get(&buf), Some(prev) if prev != &cur_enc);

        // FileType, on first set and on every filetype *change* (see `ft_changed`).
        // The pattern is the buffer's filetype — an explicit one (`:set ft`,
        // `nx.bo.filetype`, or a core-created special buffer's `set_filetype`: the
        // explorer's `nxdir`, the quickfix display's `qf`, a view's `nxview`/content
        // ft) wins; otherwise the path's extension decides. `None` (an extension-less
        // `[No Name]`) skips firing, matching neovim. It fires for non-file-backed
        // buffers too, so a core-created special buffer's `FileType <ft>` autocmd
        // installs its buffer-local maps (the unified special-buffer model — see
        // docs/plans/2026-06-16-unify-special-buffer-kinds.md).
        // Skipped while a read chain owns this buffer: the chain's `FileType` stage fires
        // the initial one, in order behind `BufReadPost`'s settle. This path remains for
        // a *later* filetype change on an already-announced buffer (`:set ft=x`, an
        // in-place buffer reuse across kinds), which is not part of any read sequence.
        if ft_changed && !pending_open && !self.read_chains.contains_key(&buf) {
            if let Some(ft) = &cur_ft {
                self.fire_lifecycle("FileType", ft, buf, &name);
            }
            self.fired_filetype.insert(buf, cur_ft);
        }

        // `EncodingChanged` (alias `FileEncoding`) on a real change to the current
        // buffer's `'fileencoding'`; the first sighting only seeds the baseline (no
        // fire). The `<amatch>` is the new encoding label, like neovim. Held back for
        // a pending open (the encoding isn't final until the bytes land), then seeded
        // silently when they do — so opening a file at its own encoding fires nothing.
        if !pending_open {
            if enc_changed {
                self.fire_lifecycle("EncodingChanged", &cur_enc, buf, &name);
            }
            self.fired_encoding.insert(buf, cur_enc);
        }

        // Fire-every on entry: `BufEnter` for the buffer we entered (both file-backed and
        // [No Name]), closing the `BufLeave → … → BufEnter` bracket the read chain sits
        // inside — its `BufLeave` half fired above, ahead of the announce.
        //
        // A re-read of the buffer that is *already* current re-enters it: `:e!` moves no
        // window and changes no current-buffer id, but neovim's `do_ecmd` runs the whole
        // enter sequence over the fresh read — `BufReadPost` → `FileType` → `BufEnter` →
        // `BufWinEnter` — so a handler that sets up from buffer content re-runs on a
        // reload, which is the point of reloading. The `BufWinEnter` twin is the
        // `reread` walk below; this is its `BufEnter` half. **No `BufLeave`**: nothing was
        // left, and neovim fires none (verified against 0.12.2).
        //
        // Its sibling: the *focused window* now displays the current buffer and did not
        // before. neovim fires `BufEnter` from `enter_buffer`, which runs whenever a
        // window is made to display a buffer — not only when the current buffer *id*
        // changes — and `:split <the file you are already in>` is exactly the case an
        // id-diff cannot see: focus moves to a brand-new window that displays the buffer
        // we were already in. Keyed on the focused window alone, so a *background* window
        // being rebound onto the current buffer (`:bdelete`'s sweep) stays what it is — a
        // display, not an entry.
        let redisplayed_here = self.editor.window_buffer(cur_win) == Some(buf)
            && self.known_window_buffers.get(&cur_win).copied() != Some(buf);
        let reentered = !entered && (reread.contains(&buf) || redisplayed_here);
        if (entered || reentered) && !pending_open {
            self.last_buffer_id = Some(buf);
            // If this buffer's read chain is still in flight (a stage parked on an async
            // handler), hand `BufEnter` to the chain so it lands *after* the gates rather
            // than beating a still-settling `FileType`. A chain that completed
            // synchronously — the common case — is already gone from the map, so this
            // fires inline exactly as before.
            match self.read_chains.get_mut(&buf) {
                Some(c) => c.deferred_enter = true,
                None => self.fire_lifecycle("BufEnter", &name, buf, &name),
            }
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

        // ----- read lifecycle for buffers in NON-current windows -----
        // Announce every displayed-but-unannounced buffer (`BufReadPost`/`BufNewFile`
        // → `FileType`) before the `BufWinEnter` walk below, so a restored background
        // window gets neovim's full per-buffer sequence in order rather than
        // `BufWinEnter` alone. See `announce_displayed_buffers`.
        self.announce_displayed_buffers();

        // ----- BufWinEnter: a window now displaying a buffer it wasn't -----
        // neovim's rule is per *window display*, not per buffer: it fires from the
        // buffer load/switch paths — `open_buffer`, `do_ecmd`'s already-loaded branch,
        // `enter_buffer` — and from nothing in `window.c`. So navigation (a tab switch,
        // `<C-w>w`) never fires, while a *second* window opening an already-displayed
        // buffer does (`:h BufWinEnter` claims otherwise; the doc is stale).
        //
        // The equivalent as a diff: fire for each window whose displayed buffer differs
        // from its baseline. Like the read-lifecycle walk just above (and unlike the
        // current-buffer `BufEnter`), this walks *every* window, so a session/workspace
        // restore — which fills windows the current-buffer diff never visits — fires per
        // restored file. A window created by a bare `:split` was seeded with its
        // inherited buffer at the top of this function, so it starts out matching and
        // stays silent. The baseline (`known_window_buffers`) is rebuilt every diff so it
        // stays current even with no handler; the fire is gated on a registered handler,
        // like `WinScrolled` — so a no-handler session never enters Lua here.
        // `(window, buffer)`: the window is what the event is *about*, and it is
        // installed as the current one for the fire (see `fire_buf_event_in_win`), so a
        // handler doing per-window setup addresses the window that displayed rather than
        // whichever one is focused — which for a session restore filling background
        // windows is not the same thing at all.
        let mut newly_shown: Vec<(WindowId, BufferId)> = Vec::new();
        let mut new_map: HashMap<WindowId, BufferId> = HashMap::new();
        // `wins` was moved into `known_windows` above; it is the same filtered
        // (doc-floats excluded) list.
        for &w in &self.known_windows {
            if let Some(b) = self.editor.window_buffer(w) {
                // Hold a buffer whose open is still pending (a deferred `:edit` whose
                // content lands later this convergence): it's empty and unnamed now,
                // so firing `BufWinEnter` here would announce the placeholder — ahead
                // of `BufReadPost`/`FileType` and against the wrong (pre-load)
                // filetype. Carry the window's *previous* baseline forward instead of
                // recording the placeholder, so it fires once, in neovim's order, over
                // the filled buffer on the load diff — the window twin of the
                // `pending_open` gate on `BufReadPost`/`BufEnter` above (which use the
                // *current* buffer's pending state; here every displayed buffer is
                // checked, so a background window filled by a session restore is covered
                // too).
                if self.editor.has_pending_open(b) {
                    if let Some(prev) = self.known_window_buffers.get(&w).copied() {
                        new_map.insert(w, prev);
                    }
                    continue;
                }
                new_map.insert(w, b);
                if self.known_window_buffers.get(&w).copied() != Some(b) {
                    newly_shown.push((w, b));
                }
            }
        }
        // A displayed buffer re-read from disk keeps its bufnr in the same window, so no
        // window changed what it holds — but neovim fires `BufWinEnter` off the read
        // itself (`open_buffer`, after the modelines), which is what makes `:e!` and a
        // `:edit` reusing the throwaway `[No Name]` in place fire. Skip one already
        // queued above, so a re-read that *also* moved into a window fires once.
        for b in &reread {
            if newly_shown.iter().any(|(_, x)| x == b) {
                continue;
            }
            // The window the re-read is *about*: the focused one when it is showing the
            // buffer (`:e!`, and every reload a user drives), else the first window that
            // displays it — a background reload still has exactly one window's worth of
            // per-window setup to re-run, and neovim likewise fires once, for the window
            // the read happened in.
            let win = Some(cur_win)
                .filter(|w| new_map.get(w) == Some(b))
                .or_else(|| {
                    self.known_windows
                        .iter()
                        .copied()
                        .find(|w| new_map.get(w) == Some(b))
                });
            if let Some(w) = win {
                newly_shown.push((w, *b));
            }
        }
        self.known_window_buffers = new_map;
        if self.au_active_events.contains("BufWinEnter") {
            for (w, b) in newly_shown {
                // Sequenced behind an in-flight read chain, exactly like `BufEnter`
                // above: the buffer became displayed while a stage was still parked on
                // an async handler, and firing now would land `BufWinEnter` *second* —
                // ahead of the `FileType`/`BufEnter` the chain exists to order (vim's
                // order is `BufReadPost` → `FileType` → `BufEnter` → `BufWinEnter`). A
                // chain that completed synchronously — the common case — is already out
                // of the map, so this fires inline exactly as before.
                match self.read_chains.get_mut(&b) {
                    // Appended, not replaced: a chain stays parked across diffs, so a
                    // second window displaying the same buffer while it settles adds its
                    // own fire rather than taking the first one's place. Nothing
                    // re-detects a dropped one — by then the baseline already records the
                    // buffer as shown there.
                    Some(c) if !c.deferred_win_enter.contains(&w) => c.deferred_win_enter.push(w),
                    Some(_) => {}
                    None => self.fire_buf_event_in_win("BufWinEnter", b, w),
                }
            }
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
            self.fired_encoding.remove(b);
            // Abandon an in-flight read chain: a handler can destroy the very buffer
            // being announced (`:bwipeout` from a `BufReadPost` callback), and there is
            // nothing left to announce. Dropping the gate mapping too is what keeps a
            // *hung* handler from leaving both maps populated forever — its gate signal
            // may never arrive, and without this nothing else would ever remove them.
            if let Some(chain) = self.read_chains.remove(b) {
                if let Some(gate) = chain.gate {
                    self.chain_gates.remove(&gate);
                }
            }
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
    /// Spans every tab, like the `wins` set in [`EditHost::emit_lifecycle_events`] — a
    /// snapshot holding only the active tab's windows differs by *membership* across a
    /// tab switch, which the diff cannot tell from an actual resize.
    pub(crate) fn window_rects_snapshot(&self) -> Vec<WindowRect> {
        self.editor
            .all_window_ids()
            .into_iter()
            .filter(|w| !self.editor.is_doc_float_window(*w))
            .map(|w| (w, self.editor.window_rect(w).unwrap_or_default()))
            .collect()
    }

    /// Every window's `(id, topline, leftcol)` in layout order, for the
    /// [`WinScrolled`] diff. Only computed when a `WinScrolled` handler is active.
    /// Spans every tab, for the same reason as [`EditHost::window_rects_snapshot`].
    pub(crate) fn window_scroll_snapshot(&self) -> Vec<(WindowId, usize, usize)> {
        self.editor
            .all_window_ids()
            .into_iter()
            .filter(|w| !self.editor.is_doc_float_window(*w))
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
        self.fire_and_drain(event, &pattern, bufnr, &file);
    }

    /// Fire a buffer-lifecycle event (`BufWinEnter` for `buf` becoming displayed
    /// in a window; `BufAdd` — neovim alias `BufCreate` — for a buffer just added
    /// to the list). Unlike the generic [`fire_lifecycle`](Self::fire_lifecycle)
    /// — which keys the buffer snapshot off the *current* buffer — this resolves
    /// the name and filetype from `buf` itself: a restore fires `BufWinEnter` for
    /// buffers displayed in non-current windows, and a `:badd` never enters the
    /// buffer it adds. The `<amatch>`/`<afile>` is `buf`'s name (a buffer event,
    /// so a `*.md`-patterned or `buffer = N` autocmd matches correctly); a
    /// `[No Name]` buffer fires with an empty name, matching neovim (`BufAdd`
    /// fires for scratch buffers too).
    pub(crate) fn fire_buf_event(&mut self, event: &str, buf: BufferId) {
        let name = self.editor.buffer_name(buf).unwrap_or_default();
        self.fire_buf_lifecycle(event, &name, buf);
    }

    /// [`fire_buf_event`](Self::fire_buf_event) with `win` installed as the **current
    /// window** for the fire — the per-window event (`BufWinEnter`) is *about* a window,
    /// and neovim has entered it by the time a handler runs, so `nx.wo`,
    /// `nx.win.current()` and the cursor reads inside the handler must address that
    /// window. Without it, per-window setup for a window a session restore filled landed
    /// in whichever window happened to be focused — and the whole point of firing per
    /// window is that each one has its own setup to do.
    ///
    /// The context is the *mirror* one (`nx._fire_in_win` → `nx.win.call`), not a real
    /// focus change: running a handler must not move the user's cursor. Reads and
    /// explicit-handle writes resolve against `win`; a drain-time mutation (`nx.cmd`,
    /// feedkeys) raises through `nx._call_ctx_lock` rather than silently landing in the
    /// focused window. When `win` *is* the focused window — everything the user types —
    /// nothing is locked and this is the plain path.
    pub(crate) fn fire_buf_event_in_win(&mut self, event: &str, buf: BufferId, win: WindowId) {
        let name = self.editor.buffer_name(buf).unwrap_or_default();
        self.fire_buf_lifecycle_in(event, &name, buf, Some(win));
    }

    /// [`fire_buf_event`](Self::fire_buf_event) with an explicit `pattern`, for the
    /// buffer events whose `<amatch>` is *not* the buffer's name — `FileType`, whose
    /// pattern is the filetype. Like `fire_buf_event` (and unlike
    /// [`fire_lifecycle`](Self::fire_lifecycle)) every piece of context is resolved
    /// from `buf` itself, so this is the helper the every-window read-lifecycle walk
    /// uses: a buffer restored into a *background* window must announce with its own
    /// name and filetype, not the current buffer's.
    fn fire_buf_lifecycle(&mut self, event: &str, pattern: &str, buf: BufferId) {
        self.fire_buf_lifecycle_in(event, pattern, buf, None);
    }

    /// [`fire_buf_lifecycle`](Self::fire_buf_lifecycle), optionally in `win`'s context.
    fn fire_buf_lifecycle_in(
        &mut self,
        event: &str,
        pattern: &str,
        buf: BufferId,
        win: Option<WindowId>,
    ) {
        let name = self.editor.buffer_name(buf).unwrap_or_default();
        let ft = self.editor.buffer_filetype(buf).unwrap_or_default();
        // The snapshot carries the DISPLAY name (see `set_buf_snapshot`) while the
        // autocmd's `<afile>` stays the path above — the two differ for a pathless
        // surface, and seeding the snapshot from the path blanks it.
        let shown = self.editor.display_name(buf);
        let _ = self.lua.set_buf_snapshot(buf.0, &shown, &ft);
        match win {
            Some(w) => self.fire_and_drain_in_win(event, pattern, buf.0, &name, w),
            None => self.fire_and_drain(event, pattern, buf.0, &name),
        }
    }

    /// Fire a buffer-lifecycle event **gated**, returning whether it settled
    /// synchronously. `true` ⇒ no handler returned a pending promise, so the caller
    /// advances at once (identical timing to the ungated path — the common case, and
    /// the one every no-async config takes). `false` ⇒ the chain is parked under a fresh
    /// gate id and [`drain_au_gate_done`](Self::drain_au_gate_done) resumes it when Lua
    /// signals `nx._au_gate_done`.
    ///
    /// Note the Lua side signals only once the fire has *fully* converged — handlers
    /// settled **and** every replay round done — so the next stage really does see a
    /// settled world, including handlers a plugin registered while loading.
    fn fire_buf_lifecycle_gated(&mut self, event: &str, pattern: &str, buf: BufferId) -> bool {
        let gate_id = self.next_gate_id;
        self.next_gate_id += 1;
        let name = self.editor.buffer_name(buf).unwrap_or_default();
        let ft = self.editor.buffer_filetype(buf).unwrap_or_default();
        let shown = self.editor.display_name(buf);
        let _ = self.lua.set_buf_snapshot(buf.0, &shown, &ft);
        self.push_buf_mirror();
        let settled = match self
            .lua
            .fire_autocmd_buf_gated(event, pattern, buf.0, &name, gate_id)
        {
            Ok(sync_settled) => sync_settled,
            Err(e) => {
                // A throwing handler is reported and treated as settled: a broken
                // handler must not wedge the buffer half-announced, exactly as it must
                // not wedge a write or a quit.
                self.report_autocmd_err(event, Err::<(), _>(e));
                true
            }
        };
        // A handler's synchronous effects land now; an async one's continuation arrives
        // a tick later through `on_loop_event` → `run_pending`.
        self.apply_lua_effects();
        if !settled {
            self.chain_gates.insert(gate_id, buf);
            if let Some(c) = self.read_chains.get_mut(&buf) {
                c.gate = Some(gate_id);
            }
        }
        settled
    }

    /// Drive `buf`'s read chain as far as it can go this tick: fire the next stage, and
    /// if its handlers settled synchronously keep going, otherwise park and return.
    ///
    /// This is the ordering guarantee neovim gets for free by being synchronous — when
    /// `FileType` fires, everything `BufReadPost` triggered has finished. Its practical
    /// payoff is that a `BufReadPost` handler which detects the filetype *asynchronously*
    /// (reading a shebang over the wire, say) is reflected in the `FileType` that
    /// follows, rather than arriving a diff late through the `ft_changed` re-fire.
    ///
    /// The whole chain runs inside this one call whenever no handler goes async, which
    /// is nearly always — so an ordinary open costs exactly what it did before.
    pub(crate) fn drive_read_chain(&mut self, buf: BufferId) {
        loop {
            let Some(chain) = self.read_chains.get(&buf) else {
                return;
            };
            if chain.gate.is_some() {
                return; // parked on an async handler; the gate resumes us
            }
            match chain.stage {
                ReadStage::ReadPost => {
                    // File-backed only: a `[No Name]` / scratch surface was never read.
                    // A file absent from disk fires `BufNewFile` instead, matching
                    // `vim file-that-does-not-exist`.
                    let name = self.editor.buffer_name(buf).unwrap_or_default();
                    let mut parked = false;
                    if !name.is_empty() {
                        let event = if self.editor.buffer_is_new_file(buf) {
                            "BufNewFile"
                        } else {
                            "BufReadPost"
                        };
                        parked = !self.fire_buf_lifecycle_gated(event, &name, buf);
                    }
                    // Advance before returning, so the resume runs the NEXT stage.
                    if let Some(c) = self.read_chains.get_mut(&buf) {
                        c.stage = ReadStage::FileType;
                    }
                    if parked {
                        return;
                    }
                }
                ReadStage::FileType => {
                    // Read the filetype now, not at chain start: the read stage has
                    // settled, so a handler that set `vim.bo.filetype` — synchronously
                    // or asynchronously — is already reflected here.
                    let ft = self.editor.buffer_filetype(buf);
                    let mut parked = false;
                    if self.fired_filetype.get(&buf) != Some(&ft) {
                        if let Some(f) = ft.clone() {
                            parked = !self.fire_buf_lifecycle_gated("FileType", &f, buf);
                        }
                        self.fired_filetype.insert(buf, ft);
                    }
                    if let Some(c) = self.read_chains.get_mut(&buf) {
                        c.stage = ReadStage::Done;
                    }
                    if parked {
                        return;
                    }
                }
                ReadStage::Done => {
                    let done = self.read_chains.remove(&buf);
                    let deferred_enter = done.as_ref().is_some_and(|c| c.deferred_enter);
                    let deferred_win_enter = done.map(|c| c.deferred_win_enter).unwrap_or_default();
                    // The deferred `BufEnter` / `BufWinEnter`, now correctly ordered
                    // behind the gates and against each other (vim fires `BufWinEnter`
                    // last). Skipped if the buffer went away mid-chain (a handler
                    // `:bdelete`d it) — firing for a dead buffer would announce an
                    // empty name.
                    if !self.editor.buffer_ids().contains(&buf) {
                        return;
                    }
                    if deferred_enter {
                        let name = self.editor.buffer_name(buf).unwrap_or_default();
                        self.fire_lifecycle("BufEnter", &name, buf, &name);
                    }
                    for win in deferred_win_enter {
                        // A parked chain runs for as long as its async handlers take, and
                        // one of them may have closed the window or moved it onto
                        // something else. Fire only for a window that is *still* showing
                        // this buffer: neovim fires from inside the window, so a window
                        // that no longer displays it has no display left to announce —
                        // and installing a dead window id as "current" would point a
                        // handler's per-window setup at nothing.
                        if self.editor.window_buffer(win) == Some(buf) {
                            self.fire_buf_event_in_win("BufWinEnter", buf, win);
                        }
                    }
                    return;
                }
            }
        }
    }

    /// Fire the fire-once read lifecycle (`BufReadPost`/`BufNewFile` → `FileType`) for
    /// every buffer displayed in a window that has not been announced yet — the
    /// **non-current** ones the current-buffer diff never visits.
    ///
    /// This is what a session / workspace restore needs. Restore fills background
    /// windows directly (it does not enter each one), so before this walk those buffers
    /// fired `BufWinEnter` and nothing else: everything `FileType`-driven — LSP attach,
    /// treesitter, buffer-local maps — stayed inert until the user focused the window.
    /// Neovim announces every restored file because its session script `:buffer`s each
    /// one into its window, and entering an unloaded buffer loads it.
    ///
    /// Scope matches neovim's: a buffer that lands in a **window** announces; one that
    /// is merely listed-but-unloaded (`:badd`) does not. `BufEnter` is deliberately NOT
    /// fired here — it means "this buffer became current", which is true only of the
    /// focused one, and it is a hot-path event fired per entry by the diff above.
    ///
    /// Ordering per buffer is neovim's `BufReadPost` → `FileType`, and this runs before
    /// the `BufWinEnter` walk so the full sequence lands in order. Called every diff;
    /// the cost when there is nothing to announce is one `announced` lookup per window.
    fn announce_displayed_buffers(&mut self) {
        // Collect first, fire second: a handler can mutate windows/buffers, so the
        // walk must not be iterating the window list while Lua runs.
        let mut todo: Vec<BufferId> = Vec::new();
        let mut seen: std::collections::HashSet<BufferId> = std::collections::HashSet::new();
        for &w in &self.known_windows {
            let Some(b) = self.editor.window_buffer(w) else {
                continue;
            };
            // A buffer whose bytes have not landed yet (a deferred `:edit`, or any
            // off-tick/daemon open) is named but empty: announcing it now would fire
            // against the pre-load filetype. It announces on the diff its content lands,
            // exactly as the current-buffer path holds for `pending_open`.
            if seen.insert(b) && !self.announced.contains(&b) && !self.editor.has_pending_open(b) {
                todo.push(b);
            }
        }
        for b in todo {
            self.announced.insert(b);
            // These chains start with no deferred `BufEnter`: entering is the current
            // buffer's business, and these are by definition the ones the current-buffer
            // diff did not visit. (Nothing carries over either — `announced` gates this
            // walk, and a chain in flight implies the buffer is already announced.)
            self.begin_read_chain(b);
        }
    }

    /// Start `buf`'s read chain at [`ReadStage::ReadPost`] and drive it as far as it
    /// goes this tick.
    ///
    /// Any chain already in flight for `buf` is **abandoned** first, gate mapping and
    /// all. That is not hypothetical: `:e!` re-reads a buffer in place, which drops it
    /// from `announced` and announces it again — from underneath an async `BufReadPost`
    /// handler that is still running. Left mapped, the abandoned chain's gate signal
    /// arrives later, is still keyed to this buffer, and un-parks the *new* chain — so
    /// `FileType` fires released by the previous read's handler while the current read's
    /// is still in flight, which is exactly the ordering the chain exists to guarantee.
    /// (Its own gate signal then finds no chain and is ignored, so the read that really
    /// is in flight never releases anything.) A chain describes one read; a new read
    /// replaces it, the same way a deleted buffer drops it.
    ///
    /// Its **deferred tail carries over**, though: a `BufEnter`/`BufWinEnter` parked on
    /// the old chain records an entry / a first display that really happened and has not
    /// been announced yet. Dropping those with the chain would lose them outright —
    /// nothing re-detects them, since by then the buffer is already current and already
    /// in the displayed baseline.
    fn begin_read_chain(&mut self, buf: BufferId) {
        let (mut deferred_enter, mut deferred_win_enter) = (false, Vec::new());
        if let Some(old) = self.read_chains.remove(&buf) {
            if let Some(gate) = old.gate {
                self.chain_gates.remove(&gate);
            }
            deferred_enter = old.deferred_enter;
            deferred_win_enter = old.deferred_win_enter;
        }
        self.read_chains.insert(
            buf,
            ReadChain {
                stage: ReadStage::ReadPost,
                gate: None,
                deferred_enter,
                deferred_win_enter,
            },
        );
        self.drive_read_chain(buf);
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
        self.fire_and_drain(event, &pattern, bufnr, &file);
    }

    /// Push the current-buffer snapshot into the VM, fire `event` for `pattern` /
    /// `file` with buffer context, surface any callback error, and fold in the Lua
    /// effects the callbacks left. Deferred ex-commands the callbacks queue are
    /// drained by the caller's `run_pending`.
    pub(crate) fn fire_lifecycle(&mut self, event: &str, pattern: &str, buf: BufferId, file: &str) {
        let ft = filetype_of(self.editor.buffer().path.as_deref()).unwrap_or("");
        // `file` is the PATH (it is also the `<afile>` the autocmd fires with); the
        // snapshot's name is the DISPLAY name, which for a file-backed buffer is the same
        // string and for a pathless surface — an `nx.view`, a terminal — is its label
        // rather than `""`. Seeding from the path made every event a statusline listens
        // to (`TextChanged` on a tree's re-render, …) blank the name mid-tick, so the bar
        // flashed `[No Name]` until the next refresh restored it.
        let shown = self.editor.display_name(buf);
        let _ = self.lua.set_buf_snapshot(buf.0, &shown, ft);
        self.fire_and_drain(event, pattern, buf.0, file);
    }

    /// The shared tail of every `fire_*`: refresh the buffer mirror (an autocmd
    /// callback runs before the caller's `run_pending`, so `nx._bufs` / the cursor
    /// must be current), fire the autocmd with buffer context, surface any
    /// callback error, and fold in the Lua effects the callbacks left.
    fn fire_and_drain(&mut self, event: &str, pattern: &str, bufnr: u64, file: &str) {
        self.push_buf_mirror();
        let r = self.lua.fire_autocmd_buf(event, pattern, bufnr, file);
        self.report_autocmd_err(event, r);
        self.apply_lua_effects();
    }

    /// [`fire_and_drain`](Self::fire_and_drain) with `win` current for the fire. The
    /// mirror push comes first, exactly as above — `nx._fire_in_win` swaps its window
    /// context on top of a *fresh* mirror, so the window record it reads is this tick's.
    fn fire_and_drain_in_win(
        &mut self,
        event: &str,
        pattern: &str,
        bufnr: u64,
        file: &str,
        win: WindowId,
    ) {
        self.push_buf_mirror();
        let r = self
            .lua
            .fire_autocmd_buf_in_win(win.0, event, pattern, bufnr, file);
        self.report_autocmd_err(event, r);
        self.apply_lua_effects();
    }

    /// Point the Lua snapshot / mirror at the *written* buffer before firing a write
    /// autocmd, so `vim.bo.filetype` / `nx._bufs` read the written buffer even when it's
    /// not the current one (a `:wall` of a non-current buffer), unlike the generic
    /// [`fire_lifecycle`] which keys off the current buffer.
    fn set_write_snapshot(&mut self, buf: BufferId, path: &str) {
        let ft = filetype_of(Some(Path::new(path))).unwrap_or("");
        let _ = self.lua.set_buf_snapshot(buf.0, path, ft);
        self.push_buf_mirror();
    }

    /// Fire `BufWritePre` as an **awaited gate**, *before* the bytes are serialized: a
    /// handler may return a promise the write must wait on (an async format/trim-on-save),
    /// and may mutate the buffer (which the caller has made current). Returns `true` when
    /// every handler settled **synchronously** — the caller commits the write now; `false`
    /// when
    /// an async handler is still pending — the caller parks the write under `gate_id` and
    /// commits it when `nx._au_gate_done(gate_id)` settles (`drain_au_gate_done`). A
    /// handler that throws is reported and treated as settled (the write still proceeds,
    /// as in vim).
    fn fire_buf_write_pre_gated(&mut self, buf: BufferId, path: &str, gate_id: u64) -> bool {
        self.set_write_snapshot(buf, path);
        let settled =
            match self
                .lua
                .fire_autocmd_buf_gated("BufWritePre", path, buf.0, path, gate_id)
            {
                Ok(sync_settled) => sync_settled,
                Err(e) => {
                    self.report_autocmd_err("BufWritePre", Err::<(), _>(e));
                    true
                }
            };
        // Drain the handlers' synchronous effects (a `vim.cmd` mutation, an echo). An
        // async handler's `:next` continuation does NOT run here — it settles a tick
        // later via `on_loop_event`, which re-enters `run_pending` and fires the gate.
        self.apply_lua_effects();
        settled
    }

    /// Fire `BufWritePost` for `buf` (written to `path`) — the "reload affected tools"
    /// hook, fired after the bytes are on disk. Driven from
    /// [`EditHost::drain_write_events`] after a committed `:w` / `:wall` or a finalized
    /// off-tick save.
    pub(crate) fn fire_buf_write_post(&mut self, buf: BufferId, path: &str) {
        self.set_write_snapshot(buf, path);
        let r = self.lua.fire_autocmd_buf("BufWritePost", path, buf.0, path);
        self.report_autocmd_err("BufWritePost", r);
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
        self.fire_and_drain("BufDelete", &pattern, buf.0, "");
    }

    /// Drain the pre-write intents recorded this convergence (core's
    /// [`Editor::take_pending_pre_writes`]): fire each buffer's `BufWritePre` **before**
    /// committing its write, so a handler's buffer mutation (format / trim-on-save) is
    /// what gets serialized. The written buffer is made current for the fire (vim's
    /// `aucmd_prepbuf`, via [`Editor::begin_write_scope`]) so a mutating handler targets
    /// *it* — the `:wall` non-current-buffer case — then restored. `BufWritePre` is
    /// *awaited*: a handler may return a promise (an async format), and the write commits
    /// only once every handler settles. A write whose handlers all settle synchronously
    /// commits inline (the common case, no timing change); one with a pending async
    /// handler is parked in `pending_gated_writes` under its `gate_id` and committed later
    /// by [`drain_au_gate_done`](Self::drain_au_gate_done). Each committed (or failed)
    /// write advances (or cancels) a `:wqa` quit gate. Called inside
    /// [`run_pending`](EditHost::run_pending)'s fixpoint so a write driven from a
    /// keystroke, `vim.cmd('w')`, or a user command drives in the same convergence.
    pub(crate) fn drain_pre_writes(&mut self) {
        let pre_writes = self.editor.take_pending_pre_writes();
        if pre_writes.is_empty() {
            return;
        }
        let mut committed_any = false;
        for pw in pre_writes {
            // The autocmd's `<afile>` is the explicit `:w {name}` target, else the
            // buffer's own bound path.
            let path = pw
                .path
                .as_ref()
                .map(|p| p.display().to_string())
                .or_else(|| self.editor.buffer_name(pw.buffer))
                .unwrap_or_default();
            let gate_id = self.next_gate_id;
            self.next_gate_id += 1;
            // Make the written buffer current so a mutating BufWritePre handler targets
            // it (a no-op for the already-current single-`:w` buffer). The handler's sync
            // mutation lands inside `fire_buf_write_pre_gated`'s `apply_lua_effects`.
            let scope = self.editor.begin_write_scope(pw.buffer);
            let settled = self.fire_buf_write_pre_gated(pw.buffer, &path, gate_id);
            if settled {
                let outcome = self.editor.commit_pre_write(pw);
                self.editor.end_write_scope(scope);
                self.apply_commit_outcome(outcome);
                committed_any = true;
            } else {
                // An async `BufWritePre` handler is still settling: its sync portion ran
                // in-scope; restore now (the buffer can't stay current across ticks) and
                // park the write. `drain_au_gate_done` commits it when the gate settles.
                self.editor.end_write_scope(scope);
                self.pending_gated_writes.insert(gate_id, pw);
            }
        }
        if committed_any {
            self.reseed_cur_snapshot();
        }
    }

    /// Commit the writes whose `BufWritePre` handlers have now all settled — the async
    /// tail of [`drain_pre_writes`](Self::drain_pre_writes). Lua's
    /// `nx.promise.all_settled(...):next(…)` called `nx._au_gate_done(gate_id)` once every
    /// handler promise settled, landing the id in `au_gate_done`; here we pop the parked
    /// [`PreWrite`](nxvim_core::PreWrite) and commit it (writing by id, so no buffer scope
    /// is needed — only the fire's sync mutation did), from which `BufWritePost` fires (via
    /// [`drain_write_events`](Self::drain_write_events), ordered after this in the
    /// fixpoint), and advance/cancel a `:wqa` gate. Drained inside
    /// [`run_pending`](EditHost::run_pending), which
    /// [`settle_events`](EditHost::settle_events) runs after every async settle.
    pub(crate) fn drain_au_gate_done(&mut self) {
        if self.au_gate_done.is_empty() {
            return;
        }
        let mut committed_any = false;
        // Read chains resumed by this drain, driven after the loop so the borrow of
        // `au_gate_done` is released before a stage re-enters Lua.
        let mut resumed_chains: Vec<BufferId> = Vec::new();
        for id in std::mem::take(&mut self.au_gate_done) {
            if let Some(buf) = self.chain_gates.remove(&id) {
                // A read-chain stage's handlers settled (and its replay rounds are done,
                // so the next stage sees a fully converged world). Unpark and continue.
                if let Some(c) = self.read_chains.get_mut(&buf) {
                    c.gate = None;
                }
                resumed_chains.push(buf);
            } else if self.exit_gate == Some(id) {
                // An exit-sequence gate settled (an async `ExitPre`/`VimLeavePre`/`QuitPre`
                // handler's promises all resolved): clear the park so `drive_exit` — next in
                // the fixpoint — advances to the following stage.
                self.exit_gate = None;
            } else if let Some(pw) = self.pending_gated_writes.remove(&id) {
                let outcome = self.editor.commit_pre_write(pw);
                self.apply_commit_outcome(outcome);
                committed_any = true;
            }
        }
        for buf in resumed_chains {
            self.drive_read_chain(buf);
        }
        if committed_any {
            self.reseed_cur_snapshot();
        }
    }

    /// Drive the gated editor-exit sequence to the next point it must wait — or to the exit
    /// itself. Begins when core commits a quit ([`Editor::take_exit_requested`], set by
    /// `ex_quit_all` once its `E37` guard passes or `!` bypasses it); thereafter each call
    /// fires the current stage's event and either advances (its handlers settled
    /// synchronously) or parks on [`exit_gate`](Self::exit_gate) (a handler returned a pending
    /// promise) and returns, so the fixpoint waits for the async settle. The three `*Pre`
    /// stages are **awaited** — an `ExitPre`/`VimLeavePre` handler can flush/clean up
    /// asynchronously before the editor leaves; the terminal `Leaving` stage fires the
    /// non-gated `VimLeave` and sets [`Editor::should_quit`], which the run loop's quit funnel
    /// then acts on. A quit whose handlers all settle synchronously (the common case — no
    /// handlers at all) runs start-to-finish in one convergence, so `:qa!` still exits on the
    /// same tick. Called inside [`run_pending`](EditHost::run_pending)'s fixpoint (after
    /// [`drain_au_gate_done`](Self::drain_au_gate_done), so a just-settled gate resumes here).
    pub(crate) fn drive_exit(&mut self) {
        // A quit committed this convergence begins the sequence. Only `ex_quit_all` sets the
        // flag, and a re-entrant `:qa` fired *by* an exit handler finds `exit_stage` already
        // `Some`, so the flag it set is simply consumed and ignored — no restart, no cancel.
        if self.exit_stage.is_none() && self.editor.take_exit_requested() {
            self.exit_stage = Some(ExitStage::QuitPre);
        }
        // Never fire the next stage while a prior stage's gate is still pending (a safety
        // net — `drain_au_gate_done` clears `exit_gate` before calling us on resume).
        if self.exit_gate.is_some() {
            return;
        }
        // Advance as far as possible, stopping at the first `*Pre` stage that parks on an
        // async handler or at the exit. Each stage is advanced to `next` *before* its event
        // is fired, so a parked (async) handler resumes at the following stage rather than
        // re-firing the one that parked.
        while let Some(stage) = self.exit_stage {
            let (event, next) = match stage {
                ExitStage::QuitPre => ("QuitPre", ExitStage::ExitPre),
                ExitStage::ExitPre => ("ExitPre", ExitStage::VimLeavePre),
                ExitStage::VimLeavePre => ("VimLeavePre", ExitStage::Leaving),
                ExitStage::Leaving => {
                    // The final hook, fire-and-forget (the editor is leaving — nothing awaits
                    // it). It fires just *before* the native tail's `shada_flush_final`, not
                    // after as in neovim: this keeps the sequence uniform with the wasm build
                    // (whose shada flush is JS-driven on `beforeunload`, with no "after shada"
                    // Rust hook), and is harmless — `VimLeave` is post-persist cleanup, so the
                    // shada-relevant `VimLeavePre` above still runs before the write.
                    self.fire_vim_leave();
                    self.exit_stage = None;
                    self.editor.should_quit = true;
                    return;
                }
            };
            self.exit_stage = Some(next);
            if !self.fire_exit_event_gated(event) {
                return; // parked on an async handler; `drain_au_gate_done` resumes us
            }
        }
    }

    /// Fire one gated exit event (`QuitPre`/`ExitPre`/`VimLeavePre`) for the current buffer
    /// and report whether it settled **synchronously**. `true` ⇒ every handler resolved with
    /// no pending promise, so [`drive_exit`](Self::drive_exit) advances at once; `false` ⇒ a
    /// handler returned a pending promise — the gate is parked in [`exit_gate`](Self::exit_gate)
    /// and cleared by [`drain_au_gate_done`](Self::drain_au_gate_done) once
    /// `nx._au_gate_done(gate_id)` fires. A throwing handler is reported and treated as settled
    /// (a broken cleanup handler must not wedge the quit), matching the `BufWritePre` gate.
    fn fire_exit_event_gated(&mut self, event: &str) -> bool {
        let buf = self.editor.current_buffer_id();
        let file = self.editor.display_name(buf);
        let ft = filetype_of(self.editor.buffer().path.as_deref()).unwrap_or("");
        let _ = self.lua.set_buf_snapshot(buf.0, &file, ft);
        self.push_buf_mirror();
        let gate_id = self.next_gate_id;
        self.next_gate_id += 1;
        let settled = match self
            .lua
            .fire_autocmd_buf_gated(event, &file, buf.0, &file, gate_id)
        {
            Ok(sync_settled) => sync_settled,
            Err(e) => {
                self.report_autocmd_err(event, Err::<(), _>(e));
                true
            }
        };
        // A handler's synchronous effects (an echo, a `vim.cmd`) land now; an async handler's
        // `:next` continuation runs a tick later via `on_loop_event` → `run_pending`.
        self.apply_lua_effects();
        if !settled {
            self.exit_gate = Some(gate_id);
        }
        settled
    }

    /// Fire the non-gated `VimLeave` for the current buffer — the last autocmd before the
    /// editor exits. A handler's returned promise is tracked (surfaced on rejection) but not
    /// awaited: there's nothing left to block for. Driven from
    /// [`drive_exit`](Self::drive_exit)'s terminal stage.
    fn fire_vim_leave(&mut self) {
        let buf = self.editor.current_buffer_id();
        let file = self.editor.buffer_name(buf).unwrap_or_default();
        self.fire_lifecycle("VimLeave", &file, buf, &file);
    }

    /// Route a [`CommitOutcome`] to the `:wqa` quit gate: a synchronous commit advances
    /// it, a synchronous failure cancels it, an off-tick enqueue leaves it to the ack.
    fn apply_commit_outcome(&mut self, outcome: CommitOutcome) {
        match outcome {
            CommitOutcome::Committed(seq) => self.advance_quit_all_gate(seq),
            CommitOutcome::Failed(seq) => self.cancel_quit_all_gate(seq),
            CommitOutcome::Deferred => {}
        }
    }

    /// Drain the writes completed this convergence (core's
    /// [`Editor::take_write_events`]) and fire each one's `BufWritePost`. `BufWritePre`
    /// already fired before the bytes (the pre-write drain, or off-tick pre-enqueue), so
    /// only `BufWritePost` remains. Called inside
    /// [`run_pending`](EditHost::run_pending)'s fixpoint so a committed `:w` / `:wall` or
    /// a daemon save ack fires its `BufWritePost` in the same convergence.
    pub(crate) fn drain_write_events(&mut self) {
        let writes = self.editor.take_write_events();
        if writes.is_empty() {
            return;
        }
        for we in writes {
            let path = we.path.display().to_string();
            self.fire_buf_write_post(we.buffer, &path);
        }
        self.reseed_cur_snapshot();
    }

    /// Re-seed the Lua snapshot (`nx._cur_buf`) at the editor's *current* buffer after a
    /// write drain: firing a write autocmd points the snapshot at the written buffer,
    /// which for a `:wall` may not be the current one, so a later `expand('%')` /
    /// `nvim_buf_get_name(0)` would otherwise read the last-written buffer.
    fn reseed_cur_snapshot(&mut self) {
        let cur = self.editor.current_buffer_id();
        let name = self.editor.display_name(cur);
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
        // Skip every dir the `nx.plugins` manager sources itself (via `source_runtime`):
        // the system-plugin tier AND every managed plugin spec. Re-sourcing them here would
        // run their `plugin/` scripts TWICE — once by the manager, once by this pass. The
        // system tier alone was skipped before, which missed EAGER local-`dir` plugins:
        // their runtimepath entry is added synchronously in `init.lua` (before this pass
        // runs), so both sourced them (a real double for any `plugin/` side effect — e.g. a
        // one-shot prompt or a non-idempotent registration). The manager registry is the
        // source of truth; an empty/absent registry (wasm remote-config, tests) skips
        // nothing, so unmanaged `pack/*/start` plugins are still sourced here as before.
        let manager_dirs = self.manager_owned_plugin_dirs();
        for sub in ["plugin", "after/plugin"] {
            for rt in &runtimepath {
                if manager_dirs.contains(rt) {
                    continue;
                }
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

    /// Source the `plugin/` / `after/plugin/` scripts of a specific set of directories
    /// (the system-plugin tier, in the pre-`init.lua` phase), in the same order and
    /// through the same real effects path as [`source_plugins`](Self::source_plugins).
    /// Their `lua/` trees are already on the runtimepath (spliced at boot), so `require`
    /// resolves; this runs their registration scripts. Reads the LOCAL disk, so a system
    /// plugin loads locally even in a daemon session.
    pub(crate) fn source_specific_plugins(&mut self, dirs: &[PathBuf]) {
        for sub in ["plugin", "after/plugin"] {
            for dir in dirs {
                for file in collect_lua_scripts(&dir.join(sub)) {
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

    /// Every dir the `nx.plugins` manager sources itself — the system tier PLUS every
    /// managed plugin spec — read from the registry. The set
    /// [`source_plugins`](Self::source_plugins) skips so a manager-owned plugin's `plugin/`
    /// scripts are never sourced twice (once by the manager's `source_runtime`, once by that
    /// pass). Empty when the registry is absent (wasm remote-config, tests with no manager).
    fn manager_owned_plugin_dirs(&self) -> Vec<PathBuf> {
        let value = match self.lua.eval_to_value(
            "return nx.plugins and nx.plugins._manager_owned_dirs and nx.plugins._manager_owned_dirs() or {}",
        ) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        match value {
            rmpv::Value::Array(items) => items
                .into_iter()
                .filter_map(|v| v.as_str().map(PathBuf::from))
                .collect(),
            _ => Vec::new(),
        }
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
