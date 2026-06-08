//! Keystroke handling: the per-key input loop, the keymap matcher drive, the
//! completion-popup key routing, and mapping (RHS) execution.

use crate::keymap::{BuiltinAction, MappingRhs, Step};
use crate::Server;
use nxvim_core::{key_to_notation, parse_keys, Key, KeyCode, Mode};

impl Server {
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

    /// Route one input key. A coroutine parked on `vim.fn.getcharstr()` consumes
    /// the key first (vim's blocking `getchar` reads from the typeahead ahead of
    /// the editor); otherwise the key flows through the completion popup / mapping
    /// engine as usual. Every key processed here is also reported to the
    /// `vim.on_key` observers (including a getchar-consumed key, matching neovim).
    pub(crate) fn process_key(&mut self, key: Key) {
        self.notify_on_key(key);
        if let Some(cb) = self.pending_getchar.take() {
            self.resume_getchar(cb, key);
            return;
        }
        // Insert-mode completion popup is modal, stateful UI routing: while it is
        // open it owns every key (navigate / accept / dismiss / live-refresh) ahead
        // of the mapping engine (design B5). A key the popup *doesn't* claim
        // dismisses it and returns `false`, so we fall through to the matcher below
        // — `<C-k>` then fires signature help, `<Esc>` then leaves insert, etc.
        // (`completion_menu_key` is only reached while open.)
        if self.editor.mode == Mode::Insert
            && self.completion_menu_open()
            && self.completion_menu_key(key)
        {
            return;
        }
        // The mapping layer interposes here, ahead of `editor.input`: each key is
        // run through the withhold/replay matcher, which hands back the steps to
        // apply (raw editor keys and/or a fired mapping). The built-in LSP keys —
        // the `gd`/`gD`/`gr` go-to trio, `K` hover, and the insert-mode completion
        // triggers — all ride it as overridable native default mappings (design
        // B2/B3); the `command_status` oracle keeps core's `g`-motions (`gg`/`dgg`/…)
        // intact under the `g`-prefix collision.
        self.feed_matcher(key);
    }

    /// Report `key` (as vim notation) to every `vim.on_key` observer. Guarded by
    /// the cheap `has_on_key` check so a session with no observer pays nothing.
    /// An observer's queued effects drain immediately (an `on_key` that echoes /
    /// `vim.cmd`s shouldn't strand its work until the next chunk).
    pub(crate) fn notify_on_key(&mut self, key: Key) {
        if !self.lua.has_on_key() {
            return;
        }
        if let Err(e) = self.lua.run_on_key(&key_to_notation(key)) {
            self.editor
                .echo(format!("E5108: Error in on_key callback: {e}"));
        }
        self.apply_lua_effects();
    }

    /// Resume a coroutine parked on `vim.fn.getcharstr()` (callback `cb`) with
    /// `key`, the getchar analogue of delivering a `vim.ui.input` result. The
    /// continuation commonly reads buffer/cursor state and may itself park on
    /// another `getcharstr` (re-arming `pending_getchar`) or queue `nvim_feedkeys`,
    /// both handled when its effects drain.
    pub(crate) fn resume_getchar(&mut self, cb: u64, key: Key) {
        self.push_buf_mirror();
        if let Err(e) = self.lua.deliver_getchar(cb, &key_to_notation(key)) {
            self.editor
                .echo(format!("E5108: Error in getcharstr continuation: {e}"));
        }
        self.apply_lua_effects();
    }

    /// Process the `nvim_feedkeys` typeahead to exhaustion: each queued key is fed
    /// through the matcher (a `remap` feed, so it can trigger mappings) or straight
    /// to the editor (a `noremap` feed), with its effects driven to convergence
    /// before the next. A fed key can re-fill the buffer (a mapping that itself
    /// feeds keys) or be claimed by a parked `getcharstr`; both are handled here.
    /// Bounded by a generous budget so a self-perpetuating feed can't spin forever.
    pub(crate) fn drain_feedkeys(&mut self) {
        if self.feed_buffer.is_empty() {
            return;
        }
        // A `nvim_feedkeys` producer (e.g. which-key) may have changed the keymap
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
            // A fed key, like a typed one, is observed and can feed a parked getchar.
            self.notify_on_key(key);
            if let Some(cb) = self.pending_getchar.take() {
                self.resume_getchar(cb, key);
            } else if remap {
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
        if self.editor.awaiting_literal_arg() && self.keymaps.pending_empty() {
            self.editor.input(key);
            self.emit_lifecycle_events();
            return;
        }
        let mode = self.editor.mode;
        for step in self.keymaps.feed(mode, key) {
            self.apply_step(step);
        }
    }

    /// Handle one key while the completion popup is open. Returns `true` when the
    /// key is consumed (navigation, accept, refresh); `false` after **closing**
    /// the menu, so the caller lets the key take its normal effect (`<Esc>` also
    /// leaves insert, a non-word char is inserted, `<C-k>` fires signature help).
    /// A word character or backspace is applied to the editor first, then the menu
    /// re-ranks (or re-requests) against the new prefix in place.
    pub(crate) fn completion_menu_key(&mut self, key: Key) -> bool {
        if key.ctrl {
            return match key.code {
                KeyCode::Char('n') => {
                    self.lsp_menu_move(1);
                    true
                }
                KeyCode::Char('p') => {
                    self.lsp_menu_move(-1);
                    true
                }
                KeyCode::Char('y') => {
                    self.lsp_menu_accept();
                    true
                }
                KeyCode::Char('e') => {
                    self.lsp_menu_close();
                    true
                }
                // Any other ctrl key (e.g. `<C-k>`): dismiss, then let it act.
                _ => {
                    self.lsp_menu_close();
                    false
                }
            };
        }
        match key.code {
            KeyCode::Down => {
                self.lsp_menu_move(1);
                true
            }
            KeyCode::Up => {
                self.lsp_menu_move(-1);
                true
            }
            KeyCode::Enter | KeyCode::Tab => {
                self.lsp_menu_accept();
                true
            }
            // A word character or backspace edits the buffer, then refreshes the
            // menu against the new prefix (the editor inserts/deletes first).
            KeyCode::Backspace => {
                self.editor.input(key);
                self.lsp_menu_after_edit();
                true
            }
            KeyCode::Char(c) if c.is_ascii_alphanumeric() || c == '_' => {
                self.editor.input(key);
                self.lsp_menu_after_edit();
                true
            }
            // `<Esc>` and any other key dismiss the menu, then take normal effect.
            _ => {
                self.lsp_menu_close();
                false
            }
        }
    }

    /// Resolve a withheld key-prefix on input idle — the matcher's `timeoutlen`
    /// flush (design D4). Mirrors [`input`](Self::input)'s drive, but the steps come
    /// from [`Keymaps::flush`] (no incoming key) instead of `feed`. Refreshing the
    /// tries first keeps the flush consistent with a registry/buffer change since the
    /// last batch; with nothing pending the whole call is a no-op.
    pub(crate) fn input_flush(&mut self) {
        self.refresh_keymaps();
        let mode = self.editor.mode;
        for step in self.keymaps.flush(mode) {
            self.apply_step(step);
        }
        self.run_pending();
    }

    /// Bring the keymap tries up to date for the current buffer. Re-reads the
    /// registry only when `vim._keymaps_version` advanced (one integer read across
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
    /// (`vim._expr_lock`, which makes `vim.cmd` raise), and whatever effects it
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
            // insert-mode completion triggers installed at startup.
            MappingRhs::Native(BuiltinAction::Lsp(kind)) => self.request_lsp(kind),
        }
    }
}
