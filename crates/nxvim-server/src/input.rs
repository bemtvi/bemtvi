//! Keystroke handling: the per-key input loop, the keymap matcher drive, the
//! completion-popup key routing, and mapping (RHS) execution.

use crate::keymap::{MappingRhs, MatchScope, Step};
use crate::EditHost;
use nxvim_core::{parse_keys, Key, KeyCode};

impl EditHost {
    /// Whether this session will CAPTURE the window/tab layout on exit — a workspace
    /// session whose layout capture is opted in (`workspace_session && session_save_layout`,
    /// the same gate `shada_checkpoint`/`shada_flush_final` use). Mirrored into core each
    /// input batch so `:qa` can skip its `E37` guard for a modified unnamed buffer the
    /// session is about to persist. Always `false` off the native build (no workspace
    /// session there).
    pub(crate) fn session_captures_layout(&self) -> bool {
        #[cfg(feature = "native")]
        {
            self.workspace_session && self.lua.session_save_layout()
        }
        #[cfg(not(feature = "native"))]
        {
            false
        }
    }

    pub(crate) fn input(&mut self, keys: &str) {
        // Keep core's "a modified unnamed buffer will be persisted on exit" knowledge
        // fresh before any key (notably `:qa`) is processed: it gates `:qa`'s `E37` skip
        // on the live `session_save_layout` opt-in, which a plugin can toggle.
        let captures = self.session_captures_layout();
        self.editor.set_session_captures_layout(captures);
        // Rebuild the keymap tries if the registry changed since the last batch —
        // once per `nx_input`, not per key, so each keystroke only walks the
        // cached trie (design §6). A map a callback sets mid-batch takes effect on
        // the next batch, an accepted ordering.
        self.refresh_keymaps();
        for key in parse_keys(keys) {
            self.process_key(key);
        }
        self.run_pending();
        // Typeahead queued by `nvim_feedkeys` during this batch (e.g. a keymap RHS
        // that fed keys) is processed now, after the batch's own keys settle.
        self.drain_feedkeys();
    }

    /// Route one input key through the completion popup / mapping engine.
    pub(crate) fn process_key(&mut self, key: Key) {
        // The `nx.complete` engine's popup (incl. the built-in `lsp` source, Phase
        // 4-C) is **non-grabbing** and handled in core: while it is open,
        // `editor.input` (below, via the matcher) intercepts only its control keys
        // and lets every other key edit the document, re-triggering the engine after.
        // So there is no server-side pmenu routing here any more — the bespoke LSP
        // pmenu it replaced is retired.
        //
        // The mapping layer interposes here, ahead of `editor.input`: each key is
        // run through the withhold/replay matcher, which hands back the steps to
        // apply (raw editor keys and/or a fired mapping). The LSP keys (`gd`/`gD`/`gr`
        // go-to, `K` hover, `<C-k>` signature) and the completion triggers are
        // ordinary Lua maps now — installed by `prelude/lsp.lua` on attach and by
        // `nx.complete.setup` — so they ride the matcher like any user map; the
        // `command_status` oracle keeps core's `g`-motions (`gg`/`dgg`/…) intact under
        // the `g`-prefix collision.
        self.feed_matcher(key);
    }

    /// Process the `nvim_feedkeys` typeahead to exhaustion: each queued key is fed
    /// through the matcher (a `remap` feed, so it can trigger mappings) or straight
    /// to the editor (a `noremap` feed), with its effects driven to convergence
    /// before the next. A fed key can re-fill the buffer (a mapping that itself
    /// feeds keys), handled here.
    /// Bounded by a generous budget so a self-perpetuating feed can't spin forever.
    pub(crate) fn drain_feedkeys(&mut self) {
        if self.feed_buffer.is_empty() {
            return;
        }
        // A `nvim_feedkeys` producer (e.g. a popup plugin) may have changed the keymap
        // registry just before feeding (suspending its own triggers so the fed keys
        // hit the real maps); pick that up before feeding.
        self.refresh_keymaps();
        let mut budget = 10_000usize;
        while let Some((key, remap)) = self.feed_buffer.pop_front() {
            if budget == 0 {
                self.editor
                    .echo("E132: feedkeys recursion limit exceeded".to_string());
                self.feed_buffer.clear();
                break;
            }
            budget -= 1;
            if remap {
                self.feed_matcher(key);
            } else {
                self.editor.input(key);
                self.emit_lifecycle_events();
            }
            // Drive the fed key's effects (a fired Lua mapping, queued commands)
            // and any further keys it fed; refresh tries in case a map changed them.
            self.apply_lua_effects();
            self.run_pending();
            self.refresh_keymaps();
        }
    }

    /// Run one key through the general mapping matcher and apply the steps it
    /// produces. The single path into [`Keymaps::feed`], driving the per-key
    /// [`input`](Self::input) loop.
    ///
    /// One key never reaches the matcher: when core is awaiting a *literal
    /// argument* (the `r{char}` replacement, an `f`/`t`/`F`/`T` target, a `"{reg}`
    /// name, or a text-object kind), that key is read raw — like vim's
    /// `plain_vgetc` — straight into the editor. Otherwise the matcher would
    /// withhold an argument such as the `g` of `rg`/`fg` as a live prefix of the
    /// native `gd`/`gr` maps, and the command would appear to hang waiting for a
    /// disambiguating key. The `pending_empty` guard upholds the no-reorder
    /// invariant: a literal arg only ever follows a lead key that already left the
    /// matcher, so nothing is withheld at this point.
    pub(crate) fn feed_matcher(&mut self, key: Key) {
        // A grabbing widget (the picker) matches in its own keymap bucket — no
        // literal-arg bypass and no command-grammar oracle (a widget has no core
        // grammar; an unmatched key replays to the widget's handler).
        if let Some(bucket) = crate::keymap::widget_bucket(self.editor.key_context()) {
            for step in self.keymaps.feed(MatchScope::Widget(bucket), key) {
                self.apply_step(step);
            }
            return;
        }
        // Editing context: a raw-read key bypasses the matcher straight to the editor
        // — any key that *belongs to a multi-key command already in progress* (the
        // motion after an operator, the second key of `g`/`z`/`<C-w>`/`<C-w><C-w>`, the
        // literal arg of `r`/`f`/`"`/mark/text-object), plus the command line's two
        // fixed-grammar sub-states (a `confirm` dialog answer, the `<C-r>{register}`
        // name). None participate in mapping (vim reads them raw), so none route
        // through a keymap bucket. The `pending_empty` guard preserves the inverse —
        // a map *prefix* colliding with a built-in prefix is still resolved by the
        // matcher / `command_status` oracle (see `awaiting_command_continuation`).
        if (self.editor.awaiting_command_continuation() || self.editor.cmdline_reads_raw())
            && self.keymaps.pending_empty()
        {
            self.editor.input(key);
            self.emit_lifecycle_events();
            return;
        }
        let mode = self.editor.mode;
        for step in self.keymaps.feed(MatchScope::Editing(mode), key) {
            self.apply_step(step);
        }
    }

    /// Resolve the mouse-button presses the last gesture queued (drained from the
    /// core) against the keymaps — Primitive A of the explorer-port plan, now covering
    /// all three buttons with modifiers. Each press becomes a `<n-LeftMouse>` /
    /// `<C-RightMouse>` / `<MiddleMouse>`-style [`Key`] looked up in the current
    /// buffer's editing trie: a bound mapping **fires** (the click's RHS — e.g. the
    /// explorer's `<2-LeftMouse>` → open under the cursor), while an unbound click runs
    /// the editor's per-button default
    /// ([`Editor::mouse_apply_default`](nxvim_core::Editor::mouse_apply_default) — the
    /// word/line escalation, shift-extend, `'mousemodel'` dispatch, or `"*` paste).
    /// A plain-left press already placed the cursor (the `<LeftMouse>` default), so a
    /// fired `<LeftMouse>` / `<C-LeftMouse>` map acts on the clicked position; a mapped
    /// right/middle leaves the cursor put and reads the click via `vim.fn.getmousepos()`.
    /// Called right after `editor.mouse` on both the native dispatch and the wasm
    /// edit-host paths (the "two mouse entry points need settle parity" rule), so a
    /// mapping resolves the same way regardless of front end.
    pub(crate) fn resolve_mouse_clicks(&mut self) {
        let clicks = self.editor.take_mouse_clicks();
        let wheels = self.editor.take_mouse_wheels();
        if clicks.is_empty() && wheels.is_empty() {
            return;
        }
        // A click may have moved focus to another window/buffer; rebuild the tries for
        // the now-current buffer so its buffer-local mouse maps are in force.
        self.refresh_keymaps();
        let scope = MatchScope::Editing(self.editor.mode);
        for click in clicks {
            let key = Key {
                code: KeyCode::Mouse {
                    button: click.button,
                    clicks: click.clicks,
                    kind: click.kind,
                },
                shift: click.shift,
                ctrl: click.ctrl,
                alt: click.alt,
            };
            match self.keymaps.lookup_mouse(scope, key) {
                Some(m) => self.fire_mapping(m.rhs, m.silent, m.expr),
                None => self.editor.mouse_apply_default(click),
            }
        }
        // The scroll wheel resolves the same way (`<ScrollWheelUp>` / `<S-ScrollWheelDown>`
        // / …): a bound map fires, else the editor's default scroll runs.
        for wheel in wheels {
            let key = Key {
                code: KeyCode::ScrollWheel(wheel.dir),
                shift: wheel.shift,
                ctrl: wheel.ctrl,
                alt: wheel.alt,
            };
            match self.keymaps.lookup_mouse(scope, key) {
                Some(m) => self.fire_mapping(m.rhs, m.silent, m.expr),
                None => self.editor.mouse_apply_wheel_default(wheel),
            }
        }
    }

    /// Resolve a withheld key-prefix on input idle — the matcher's `timeoutlen`
    /// flush (design D4). Mirrors [`input`](Self::input)'s drive, but the steps come
    /// from [`Keymaps::flush`] (no incoming key) instead of `feed`. Refreshing the
    /// tries first keeps the flush consistent with a registry/buffer change since the
    /// last batch; with nothing pending the whole call is a no-op.
    pub(crate) fn input_flush(&mut self) {
        // `:set notimeout` holds an ambiguous mapped prefix *forever* — the next key
        // disambiguates it, never an idle timeout. So drop the idle flush entirely:
        // the withheld prefix stays pending (a which-key popup stays up). The gate
        // lives here, not only client-side, so any client — even one that always
        // sends the flush — honors `notimeout`. With nothing pending this is a no-op
        // either way; the early return just also skips the (cheap) keymap refresh.
        if !self.editor.timeout_enabled() {
            return;
        }
        self.refresh_keymaps();
        // Flush in the active context's scope — a picker's withheld prefix resolves
        // in its bucket, the buffer's in its mode.
        let scope = match crate::keymap::widget_bucket(self.editor.key_context()) {
            Some(bucket) => MatchScope::Widget(bucket),
            None => MatchScope::Editing(self.editor.mode),
        };
        for step in self.keymaps.flush(scope) {
            self.apply_step(step);
        }
        self.run_pending();
    }

    /// Bring the keymap tries up to date for the current buffer. Re-reads the
    /// registry only when `nx._keymaps_version` advanced (one integer read across
    /// the bridge on the common path), and rebuilds the per-mode tries when either
    /// the snapshot or the current buffer changed — the latter so a buffer-local
    /// map (design D6) is in force exactly in its own buffer. Both checks are
    /// cheap; a mapping set or a buffer switched *mid-batch* takes effect on the
    /// next batch, the same accepted ordering the version check already implies.
    pub(crate) fn refresh_keymaps(&mut self) {
        let version = self.lua.keymaps_version();
        if version != self.keymaps.version {
            let snapshot = self.lua.keymaps_snapshot();
            self.keymaps.set_snapshot(version, snapshot);
        }
        let buffer = self.editor.current_buffer_id().0;
        if self.keymaps.needs_build(buffer) {
            self.keymaps.build_for(buffer);
        }
        self.refresh_au_events();
    }

    /// Refresh the cached set of registered autocmd event names when the registry
    /// changed since the last batch (`nx._au_version` advanced) — one integer read
    /// across the bridge on the common path, the event-name list pulled only on a
    /// real change. The per-key lifecycle diff consults the cache before firing a
    /// high-frequency event (`CursorMoved` / `TextChanged`), so the common no-handler
    /// config never re-enters Lua on a bare motion. Called from [`refresh_keymaps`]
    /// (once per input batch), so a handler registered mid-batch takes effect next
    /// batch — the same accepted ordering the keymap-version check already implies.
    pub(crate) fn refresh_au_events(&mut self) {
        let version = self.lua.autocmd_version();
        if version != self.au_event_version {
            self.au_event_version = version;
            self.au_active_events = self.lua.autocmd_event_set().into_iter().collect();
            // Mirror whether a BufReadCmd handler exists down to the core, so a file
            // open defers (enqueues) instead of reading inline — letting the server
            // fire BufReadCmd before the default read (Primitive B of the explorer
            // port). Only changes when the registry version bumps.
            self.editor
                .set_bufreadcmd_active(self.au_active_events.contains("BufReadCmd"));
        }
    }

    /// Apply one matcher [`Step`]: a raw key goes to the editor (with the per-key
    /// lifecycle diff, exactly as the old bare loop did); a fired mapping runs its
    /// RHS.
    pub(crate) fn apply_step(&mut self, step: Step) {
        match step {
            Step::Editor(key) => {
                self.editor.input(key);
                // Per *key*, not per message: a batched `o…<Esc>` must still see
                // the transition into insert on the `o`, which a once-per-input
                // diff would miss (it'd see only the settled Normal end-state).
                self.emit_lifecycle_events();
            }
            Step::Fire { rhs, silent, expr } => self.fire_mapping(rhs, silent, expr),
        }
    }

    /// Execute a fired mapping's RHS (design D7 — a `match` over the enum from day
    /// one, so the LSP backport adds its native action as one more arm). A Lua
    /// function is invoked and its effects folded in (any deferred ex-commands
    /// converge in the batch's trailing `run_pending`, like the autocmd path); a
    /// `noremap` string RHS is fed key-by-key straight to the editor.
    ///
    /// `<silent>` (`silent`) suppresses the message line the mapping leaves: the
    /// line is snapshotted before the fire and restored after, so a `:cmd` echo or
    /// `print` the mapping triggers doesn't linger on the command line. The
    /// `:messages` history (appended by `echo`) is deliberately *not* rewound — the
    /// output is still logged, only its transient display is hidden, matching vim's
    /// "no messages on the command line while executing this mapping." (Effects a
    /// Lua RHS *defers* to the trailing `run_pending` fall outside this window — an
    /// accepted corner, the same ordering caveat the rest of the fire path carries.)
    ///
    /// `<expr>` (`expr`) routes a Lua RHS through [`fire_expr`](Self::fire_expr): the
    /// function is run for its *return value* (the keys to feed), under a textlock
    /// that stops it mutating the editor. (A non-Lua `expr` RHS falls through to the
    /// normal path — nxvim has no expression evaluator for a string RHS.)
    pub(crate) fn fire_mapping(&mut self, rhs: MappingRhs, silent: bool, expr: bool) {
        let restore = silent.then(|| self.editor.message.clone());
        match (expr, rhs) {
            (true, MappingRhs::Lua(id)) => self.fire_expr(id),
            (_, rhs) => self.fire_mapping_inner(rhs),
        }
        if let Some(message) = restore {
            self.editor.message = message;
        }
        // The count / register typed before this mapping were its arguments
        // (`v:count` / `v:register`, which the RHS may have just read); the mapping
        // has consumed them, so clear the pending command state. A mapping fires
        // outside `Editor::input`, so the editor never resets this itself, and it
        // would otherwise leak into the next command (`3<leader>x` then `j` would
        // move 3 lines).
        self.editor.clear_pending_command();
    }

    /// Run an `<expr>` Lua RHS and feed the keys it returns. The function computes
    /// keys rather than acting (vim's `<expr>`): it runs under the prelude's textlock
    /// (`nx._expr_lock`, which makes `vim.cmd` raise), and whatever effects it
    /// queued anyway are **discarded** here — only the returned keys take effect, fed
    /// straight to the editor (noremap; the computed keys are not themselves
    /// remapped, the common case for `<expr>`, which is noremap by default). An error
    /// (a throwing handler, or a textlock violation) is surfaced and nothing is fed.
    pub(crate) fn fire_expr(&mut self, id: u64) {
        self.push_buf_mirror(); // an `<expr>` RHS may read the cursor / buffer lines
        match self.lua.run_keymap_expr(id) {
            Ok(keys) => {
                self.discard_lua_effects();
                for key in parse_keys(&keys) {
                    self.editor.input(key);
                    self.emit_lifecycle_events();
                }
            }
            Err(e) => {
                self.discard_lua_effects();
                self.editor
                    .echo(format!("E5108: Error executing keymap: {e}"));
            }
        }
    }

    /// Drop every side effect the last Lua chunk queued without applying any of them
    /// — the `<expr>` sandbox's safety net: an `<expr>` RHS that printed, set a
    /// highlight, or queued a panel op despite the textlock has those effects thrown
    /// away here, so only its returned keys ever reach the editor. Mirrors the drains
    /// in [`apply_lua_effects`](Self::apply_lua_effects), but discards each.
    pub(crate) fn discard_lua_effects(&mut self) {
        let _ = self.lua.take_highlights();
        let _ = self.lua.take_commands();
        let _ = self.lua.take_output();
        let _ = self.lua.take_picker_actions();
        let _ = self.lua.take_select_actions();
        let _ = self.lua.take_cmdline_actions();
    }

    pub(crate) fn fire_mapping_inner(&mut self, rhs: MappingRhs) {
        match rhs {
            MappingRhs::Lua(id) => {
                // A keymap function commonly reads the cursor / buffer lines; this
                // is a synchronous Lua entry that runs before the trailing
                // `run_pending`, so refresh the mirror first (Phase 6).
                self.push_buf_mirror();
                if let Err(e) = self.lua.run_keymap(id) {
                    self.editor
                        .echo(format!("E5108: Error executing keymap: {e}"));
                }
                self.apply_lua_effects();
            }
            MappingRhs::Keys(keys, _noremap) => {
                // A string RHS that reaches the server is fed straight to the
                // editor, bypassing the trie. The matcher only hands these over
                // for the non-remapping cases: a `noremap` RHS, or a `remap` RHS
                // that exhausted its re-feed budget (recursive remap expansion
                // happens inside the matcher's `feed`, never here).
                for key in keys {
                    self.editor.input(key);
                    self.emit_lifecycle_events();
                }
            }
        }
    }
}
