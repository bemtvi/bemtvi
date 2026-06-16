//! Keystroke handling: the per-key input loop, the keymap matcher drive, the
//! completion-popup key routing, and mapping (RHS) execution.

#[cfg(feature = "native")]
use crate::keymap::BuiltinAction;
use crate::keymap::{MappingRhs, MatchScope, Step};
use crate::EditHost;
use nxvim_core::{parse_keys, Key};

impl EditHost {
    pub(crate) fn input(&mut self, keys: &str) {
        // Rebuild the keymap tries if the registry changed since the last batch —
        // once per `nvim_input`, not per key, so each keystroke only walks the
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
        // apply (raw editor keys and/or a fired mapping). The built-in LSP keys —
        // the `gd`/`gD`/`gr` go-to trio, `K` hover, and the insert-mode completion
        // triggers — all ride it as overridable native default mappings (design
        // B2/B3); the `command_status` oracle keeps core's `g`-motions (`gg`/`dgg`/…)
        // intact under the `g`-prefix collision.
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
        // Editing context: the literal-argument raw read, then the per-mode matcher.
        if self.editor.awaiting_literal_arg() && self.keymaps.pending_empty() {
            self.editor.input(key);
            self.emit_lifecycle_events();
            return;
        }
        let mode = self.editor.mode;
        for step in self.keymaps.feed(MatchScope::Editing(mode), key) {
            self.apply_step(step);
        }
    }

    /// Resolve a withheld key-prefix on input idle — the matcher's `timeoutlen`
    /// flush (design D4). Mirrors [`input`](Self::input)'s drive, but the steps come
    /// from [`Keymaps::flush`] (no incoming key) instead of `feed`. Refreshing the
    /// tries first keeps the flush consistent with a registry/buffer change since the
    /// last batch; with nothing pending the whole call is a no-op.
    pub(crate) fn input_flush(&mut self) {
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
        let _ = self.lua.take_panel_ops();
        let _ = self.lua.take_picker_actions();
        let _ = self.lua.take_select_actions();
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
            // A built-in default (the LSP keys) runs natively — no key-feeding, so
            // the `<cmd>`/remap caveats never touch it (design B3). `request_lsp`
            // and `LspReqKind` already exist on this branch; the matcher only ever
            // hands us a `Native` RHS for the four normal-mode LSP defaults and the
            // insert-mode completion triggers installed at startup. The whole
            // `Native` rung is native-only (LSP), so on the browser build the
            // `MappingRhs` match has no such arm to cover.
            #[cfg(feature = "native")]
            MappingRhs::Native(BuiltinAction::Lsp(kind)) => self.request_lsp(kind),
            // The `<C-Space>` / `<C-x><C-o>` manual completion trigger: open the
            // engine popup (which dispatches the `lsp` source via the settle loop).
            #[cfg(feature = "native")]
            MappingRhs::Native(BuiltinAction::CompleteTrigger) => {
                self.editor.complete_manual_trigger()
            }
        }
    }
}
