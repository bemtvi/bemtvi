//! Keystroke handling: the per-key input loop, the keymap matcher drive, the
//! completion-popup key routing, and mapping (RHS) execution.

use crate::keymap::{MappingRhs, MatchScope, Step};
use crate::{EditHost, MacroFrame};
use bemtvi_core::{parse_keys, parse_keys_raw, Key, KeyCode};

/// How deep macro playback may nest — a macro playing a macro playing a macro.
/// vim lets a macro call itself and relies on the first failing command to end
/// the recursion; until failure aborts playback (phase 3 of the macro plan) this
/// depth is the backstop, and it is loud when it trips.
const MACRO_MAX_DEPTH: usize = 100;

/// Total keys one `drive_macro_play` call may execute before it gives up, the
/// analogue of the `nvim_feedkeys` drain's budget. Generous enough that a real
/// `10000<F3>a` over a big file completes; small enough that a runaway macro
/// surfaces in a blink rather than hanging the editor.
const MACRO_KEY_BUDGET: usize = 1_000_000;

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
        // On the web build the *origin* is the workspace: the shada blob lives in that
        // origin's OPFS and there is exactly one session per origin, which is what
        // `--workspace` names natively. So there is no second `workspace_session` axis to
        // gate on — the config's `btv.shada.save_layout(true)` is the whole opt-in, and it
        // is still default-off. Returning a hardcoded `false` here (what this did) made
        // that public API a silent no-op in the browser.
        // See docs/plans/2026-08-14-web-session-restore.md.
        #[cfg(not(feature = "native"))]
        {
            self.lua.session_save_layout()
        }
    }

    pub(crate) fn input(&mut self, keys: &str) {
        // Keep core's "a modified unnamed buffer will be persisted on exit" knowledge
        // fresh before any key (notably `:qa`) is processed: it gates `:qa`'s `E37` skip
        // on the live `session_save_layout` opt-in, which a plugin can toggle.
        let captures = self.session_captures_layout();
        self.editor.set_session_captures_layout(captures);
        // Rebuild the keymap tries if the registry changed since the last batch —
        // once per `btv_input`, not per key, so each keystroke only walks the
        // cached trie (design §6). A map a callback sets mid-batch takes effect on
        // the next batch, an accepted ordering.
        self.refresh_keymaps();
        // Parse this batch faithfully when the client's kitty keyboard protocol is
        // on, so a distinct `<C-i>`/`<C-m>`/`<C-[>`/`<C-h>` reaches the matcher rather
        // than being folded onto `<Tab>`/`<CR>`/`<Esc>`/`<BS>` (the keymap LHS is
        // parsed the same way in `build_for`, so the two sides agree). A legacy
        // terminal already sends the named byte, so the fold is the right default.
        let parsed = if self.keyboard_protocol {
            parse_keys_raw(keys)
        } else {
            parse_keys(keys)
        };
        for key in parsed {
            self.process_key(key);
            // A macro this key started (`<F3>a`) runs BEFORE the rest of the batch —
            // vim puts a played register ahead of the remaining typeahead, so
            // `<F3>aj` moves down after the macro ran, not before it.
            self.drive_macro_play();
        }
        // A paste is by construction one batch — the client has the whole payload
        // before it sends anything — so close the span here even if the `<PasteEnd>`
        // bracket never arrived. Without this a truncated or malformed feed would
        // strand the payload in the collector, unapplied, and leave every following
        // keystroke being swallowed into it.
        if let Some(payload) = self.paste_payload.take() {
            self.apply_paste(payload);
        }
        self.editor.set_paste_active(false);
        self.run_pending();
        // Typeahead queued by `nvim_feedkeys` during this batch (e.g. a keymap RHS
        // that fed keys) is processed now, after the batch's own keys settle.
        self.drain_feedkeys();
        // …and a macro one of those fed keys asked for.
        self.drive_macro_play();
    }

    /// Run every macro playback the editor has queued (`{count}<F3>{reg}`) to
    /// exhaustion, then return.
    ///
    /// Playback re-enters the **keymap matcher**, not `Editor::input`: a recording
    /// holds the LHS of every mapping the user fired, so replaying it has to give
    /// those mappings the chance to fire again (see `bemtvi_core`'s
    /// `editor::macros`). That is also why this lives here rather than in core.
    ///
    /// The frames are a stack, so a macro that plays another macro suspends the
    /// caller and resumes it after — `{count}` is a repeat counter on the frame
    /// rather than an expanded key list, so `1000<F3>a` costs nothing to set up.
    /// Two bounds keep a runaway macro from hanging the editor: a nesting cap (a
    /// self-recursive `<F3>a` is legal in vim and terminates on error, so the depth
    /// is what stops it here) and a total key budget, both reported loudly.
    pub(crate) fn drive_macro_play(&mut self) {
        self.collect_macro_play();
        if self.macro_play.is_empty() {
            return;
        }
        // Played keys are not typed keys: a recording in flight captured the
        // `<F3>a` the user pressed, and must not also capture what it expands to.
        self.macro_suppress += 1;
        let mut budget = MACRO_KEY_BUDGET;
        while let Some(frame) = self.macro_play.last_mut() {
            let Some(&key) = frame.keys.get(frame.pos) else {
                // The frame ran out: repeat it, or drop it and resume the caller.
                frame.repeats -= 1;
                if frame.repeats == 0 {
                    self.macro_play.pop();
                    // Resume the caller's register, or report "nothing playing".
                    let outer = self.macro_play.last().map(|f| f.reg);
                    self.editor.set_executing_register(outer);
                } else {
                    frame.pos = 0;
                }
                continue;
            };
            frame.pos += 1;
            if budget == 0 {
                self.editor
                    .echo("E132: macro playback exceeded its key budget".to_string());
                self.macro_play.clear();
                break;
            }
            budget -= 1;
            self.feed_matcher(key);
            // Drive what the key set in motion before the next one, exactly as the
            // typeahead drain does: a Lua mapping's effects, queued ex-commands,
            // keys it fed, and any macro IT asked to play.
            self.apply_lua_effects();
            self.run_pending();
            self.refresh_keymaps();
            self.drain_feedkeys();
            // A failed keystroke ends the playback — every repeat and every
            // suspended frame, as in vim. This is what makes `100<F3>a` safe to
            // type: the run stops at the end of the buffer (or at the first `E###`)
            // instead of grinding the last line 90 more times.
            if self.editor.take_command_failed() {
                self.macro_play.clear();
                break;
            }
            self.collect_macro_play();
        }
        // Whatever ended the run — exhaustion, a failure, a budget trip — nothing
        // is playing now.
        self.editor.set_executing_register(None);
        self.macro_suppress -= 1;
    }

    /// Push a playback the editor just resolved onto the frame stack. Refuses
    /// beyond [`MACRO_MAX_DEPTH`] rather than growing without bound — the loud end
    /// of a macro that plays itself.
    fn collect_macro_play(&mut self) {
        let Some(play) = self.editor.take_macro_play() else {
            return;
        };
        if self.macro_play.len() >= MACRO_MAX_DEPTH {
            self.editor.echo("E169: Command too recursive".to_string());
            self.macro_play.clear();
            self.editor.set_executing_register(None);
            return;
        }
        if play.count == 0 {
            return;
        }
        self.editor.set_executing_register(Some(play.reg));
        self.macro_play.push(MacroFrame {
            reg: play.reg,
            keys: play.keys,
            pos: 0,
            repeats: play.count,
        });
    }

    /// Apply one bracketed paste's payload, collected between the client's
    /// `<PasteStart>` / `<PasteEnd>` markers.
    ///
    /// The fast path hands the whole payload to core as a **single** edit
    /// ([`Editor::paste_literal`]): one rope insert, one settle, and — because the
    /// text never becomes keys — no way for it to trip a mapping, a snippet jump or
    /// a completion confirm. Core declines when the payload has to stay a key
    /// stream (Normal mode, the command line, a terminal job, a grabbing menu,
    /// Replace mode's overtype), and then it is replayed key by key *inside* a paste
    /// span, where the insert-mode guards still keep it literal — slower, same text.
    fn apply_paste(&mut self, payload: Vec<Key>) {
        if let Some(text) = payload_text(&payload) {
            if self.editor.paste_literal(&text) {
                return;
            }
        }
        // `Editor::input` (not `process_key`) for the markers: they must be recorded
        // into the dot-repeat stream so a `.` of this change re-enters paste mode,
        // and they must not reach the keymap matcher.
        self.editor.input(Key::new(KeyCode::PasteStart));
        for key in payload {
            self.process_key(key);
        }
        self.editor.input(Key::new(KeyCode::PasteEnd));
    }

    /// Route one input key through the completion popup / mapping engine.
    pub(crate) fn process_key(&mut self, key: Key) {
        // The bracketed-paste brackets are not keys the user pressed — they delimit a
        // payload the client already had (`bemtvi_view::encode_paste`). Consume them
        // here, ahead of the matcher and the editor, so they can neither be mapped nor
        // reach the buffer as text, and *collect* what they enclose rather than
        // dispatching it: a paste is one edit, and `apply_paste` below applies it as
        // one. Keys arriving between the brackets go nowhere near the matcher, so the
        // payload cannot fire a mapping on its way in.
        match key.code {
            KeyCode::PasteStart => {
                self.paste_payload = Some(Vec::new());
                return;
            }
            KeyCode::PasteEnd => {
                if let Some(payload) = self.paste_payload.take() {
                    self.apply_paste(payload);
                }
                return;
            }
            _ => {}
        }
        if let Some(payload) = self.paste_payload.as_mut() {
            payload.push(key);
            return;
        }
        // The `btv.complete` engine's popup (incl. the built-in `lsp` source, Phase
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
        // `btv.complete.setup` — so they ride the matcher like any user map; the
        // `command_status` oracle keeps core's `g`-motions (`gg`/`dgg`/…) intact under
        // the `g`-prefix collision.

        // "The next key dismisses a transient content float" is a per-*key* rule, so
        // apply it HERE — before the matcher can route this key into a mapping. A
        // mapped key fires its RHS *outside* `Editor::input` (the sole other place
        // that clears the float), so a float wiped only there would survive a
        // Lua-handler map and hang until an unmapped key arrived (the plugin-manager
        // restart notice's two-`<Esc>` bug). Clearing first still lets a map that
        // OPENS a float keep it: this key clears the *previous* float, the RHS opens
        // the new one after, and it lives until the following key. A *persistent*
        // float (which-key) is untouched — the dismissal skips it.
        self.editor.dismiss_transient_content_float();
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
        // Typeahead is not typed input — an in-flight `<F2>` recording must not
        // capture what a plugin fed (it already recorded whatever the user pressed
        // to get here).
        self.macro_suppress += 1;
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
                // A `noremap` feed still fires the built-in `default` maps (cmdline
                // control keys, special-buffer opens) — same as a `noremap` string
                // RHS — so a fed `:cmd<CR>` runs instead of hanging in the cmdline.
                self.feed_noremap_key(key);
            }
            // Drive the fed key's effects (a fired Lua mapping, queued commands)
            // and any further keys it fed; refresh tries in case a map changed them.
            self.apply_lua_effects();
            self.run_pending();
            self.refresh_keymaps();
        }
        self.macro_suppress -= 1;
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
            self.editor_input_typed(key);
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
    /// ([`Editor::mouse_apply_default`](bemtvi_core::Editor::mouse_apply_default) — the
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
    /// registry only when `btv._keymaps_version` advanced (one integer read across
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
    /// changed since the last batch (`btv._au_version` advanced) — one integer read
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
            Step::Editor(key) => self.editor_input_typed(key),
            Step::Fire {
                rhs,
                silent,
                expr,
                lhs,
            } => {
                // A macro records what the user TYPED, so the mapping's LHS goes in
                // — not its RHS (a Lua handler produces no keys at all, and a
                // `noremap` string RHS is fed below under the recording suppression
                // `fire_mapping` holds). The replay re-fires the mapping.
                self.note_macro_keys(&lhs);
                self.fire_mapping(rhs, silent, expr);
            }
        }
    }

    /// Feed one **typed** key to the editor: note it against any in-flight macro
    /// recording, dispatch it, then emit the lifecycle events it triggered.
    ///
    /// The single chokepoint for "a key the user pressed is now executing" — the
    /// matcher's released keys and the literal-argument bypass both come through
    /// here — which is exactly the granularity `<F2>` records at (see
    /// `bemtvi_core`'s `editor::macros`). Keys fed from a mapping RHS, from
    /// `nvim_feedkeys`, or by a macro playing back are *not* typed keys; those
    /// paths raise [`macro_suppress`](Self::note_macro_keys) so they never record.
    ///
    /// The lifecycle emit is per *key*, not per message: a batched `o…<Esc>` must
    /// still see the transition into insert on the `o`, which a once-per-input
    /// diff would miss (it'd see only the settled Normal end-state).
    pub(crate) fn editor_input_typed(&mut self, key: Key) {
        self.note_macro_keys(std::slice::from_ref(&key));
        self.editor.input(key);
        self.emit_lifecycle_events();
    }

    /// Note typed keys against an in-flight `<F2>` recording, unless recording is
    /// suppressed — i.e. unless we are inside a mapping RHS, the `nvim_feedkeys`
    /// typeahead drain, or a macro playback, none of which the user typed.
    pub(crate) fn note_macro_keys(&mut self, keys: &[Key]) {
        if self.macro_suppress > 0 {
            return;
        }
        for &key in keys {
            self.editor.note_macro_key(key);
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
    /// normal path — bemtvi has no expression evaluator for a string RHS.)
    pub(crate) fn fire_mapping(&mut self, rhs: MappingRhs, silent: bool, expr: bool) {
        let restore = silent.then(|| self.editor.message.clone());
        // The pending command as it stood *before* the RHS ran — the count/register
        // typed ahead of the mapping. Compared against the post-fire state below to
        // tell those apart from a command stage the RHS itself armed.
        let pending_before = self.editor.pending_snapshot();
        // Whatever the RHS types, feeds, or executes is not typed input: a macro
        // recorded the LHS that got us here (`apply_step`), so everything the fire
        // produces must stay out of the recording.
        self.macro_suppress += 1;
        match (expr, rhs) {
            (true, MappingRhs::Lua(id)) => self.fire_expr(id),
            (_, rhs) => self.fire_mapping_inner(rhs),
        }
        self.macro_suppress -= 1;
        if let Some(message) = restore {
            self.editor.message = message;
        }
        // The count / register typed before this mapping were its arguments
        // (`v:count` / `v:register`, which the RHS may have just read); the mapping
        // has consumed them, so clear the pending command state. A mapping fires
        // outside `Editor::input`, so the editor never resets this itself, and it
        // would otherwise leak into the next command (`3<leader>x` then `j` would
        // move 3 lines).
        //
        // …*unless* the RHS moved that state, in which case it belongs to the RHS
        // and the next key completes it: a string RHS is fed key by key and may end
        // mid-command on purpose (`X` mapped to `d`, or the browser's `<A-w>` mapped
        // to the `<C-w>` prefix Chrome eats). Clearing those unconditionally
        // swallowed the RHS whole.
        self.editor
            .clear_pending_command_unless_advanced(&pending_before);
    }

    /// Run an `<expr>` Lua RHS and feed the keys it returns. The function computes
    /// keys rather than acting (vim's `<expr>`): it runs under the prelude's textlock
    /// (`btv._expr_lock`, which makes `vim.cmd` raise), and whatever effects it
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
                    self.feed_noremap_key(key);
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
    /// highlight, or queued a panel op / feedkeys despite the textlock has those
    /// effects thrown away here, so only its returned keys ever reach the editor.
    /// The queue list lives with the queues (`Shared::discard_effects`, exhaustive
    /// by destructuring) so it cannot drift from the [`apply_lua_effects`]
    /// (Self::apply_lua_effects) drains; `btv.schedule` requests deliberately
    /// survive (vim.schedule is the documented textlock escape hatch).
    pub(crate) fn discard_lua_effects(&mut self) {
        self.lua.discard_all_effects();
    }

    pub(crate) fn fire_mapping_inner(&mut self, rhs: MappingRhs) {
        match rhs {
            MappingRhs::Lua(id) => {
                // A keymap function commonly reads the cursor / buffer lines; this
                // is a synchronous Lua entry that runs before the trailing
                // `run_pending`, so refresh the mirror first (Phase 6). This also
                // publishes `v:count` / `v:register` from the pending state the
                // count typed ahead of the mapping left behind.
                self.push_buf_mirror();
                // …and having published them, *consume* them before the function
                // runs. A Lua RHS acts (unlike a string RHS, whose keys the count
                // still prefixes), so vim's model is that the mapping has taken the
                // count as its argument: anything the function then executes starts
                // from a clean pending command. Left set, the count leaks into the
                // RHS's own effects and **concatenates** — `3<C-o>` mapped to a
                // function running `vim.cmd("normal! " .. vim.v.count1 .. "\15")`
                // would feed `3` on top of the pending `3` and jump back 33, a
                // silent no-op. (The trailing clear in `fire_mapping` still covers
                // the string-RHS arm, where the count must survive the feed.)
                self.editor.clear_pending_command();
                if let Err(e) = self.lua.run_keymap(id) {
                    self.editor
                        .echo(format!("E5108: Error executing keymap: {e}"));
                }
                self.apply_lua_effects();
            }
            MappingRhs::Keys(keys, _noremap) => {
                // A string RHS that reaches the server is a non-remapping feed: a
                // `noremap` RHS, or a `remap` RHS that exhausted its re-feed budget
                // (recursive remap expansion happens inside the matcher's `feed`,
                // never here). Its keys don't trigger *user* maps — but bemtvi's
                // built-in cmdline control keys (`<CR>` submit, `<Esc>` cancel, …)
                // and special-buffer opens are `default` maps, and vim's built-ins
                // still act under `noremap`, so each key is routed through
                // `feed_noremap_key` (which fires a matching default map and
                // otherwise feeds the editor) rather than straight to the editor —
                // else `:tabnew<CR>` would type the command but never run it.
                for key in keys {
                    self.feed_noremap_key(key);
                }
            }
        }
    }

    /// Feed one key of a **non-remapping** stream (a `noremap` string RHS, an
    /// `<expr>`-computed feed, or an `nvim_feedkeys` `'n'` feed) into the editor.
    ///
    /// The key must not trigger *user* maps (`noremap` never remaps), but the
    /// built-in behaviors bemtvi implements as `default` keymaps must still act, just
    /// as vim's built-ins do under `noremap`: the `cmdline` control keys (`<CR>`
    /// submit, `<Esc>` cancel, `<BS>`, the arrows, `<C-r>`) and the special-buffer
    /// `<CR>`/`-` opens are all `default` maps that are *inert* in `Editor::input`
    /// (they route through the matcher, never `handle_command`). So we look the key
    /// up against the current scope's default maps and fire a match, feeding the
    /// editor raw only when there is none.
    ///
    /// The scope is recomputed from the **live** editor mode on every call, so a
    /// mode-changing RHS lands each key in the right bucket — `:tabnew<CR>` enters
    /// the command line on `:`, types `tabnew`, then matches `<CR>` in the `cmdline`
    /// (`'c'`) bucket and submits. A single fixed scope (as the matcher's internal
    /// remap re-feed uses) would keep looking in the mode the RHS started in and miss
    /// the submit, leaving the command typed-but-unrun.
    pub(crate) fn feed_noremap_key(&mut self, key: Key) {
        // A map set / buffer switch earlier in this same RHS (e.g. `:tabnew` opened a
        // new buffer) may have invalidated the cached tries; refresh before the
        // lookup (a cheap version check on the common no-change path).
        self.refresh_keymaps();
        // Core's fixed-grammar raw reads own the next key ahead of any map, exactly as
        // in `feed_matcher`: a `<C-r>{reg}` name, a `confirm` answer, or the argument
        // of an in-progress multi-key command (`f{char}`, the motion after an
        // operator). Feed those straight to the editor. The `pending_empty` guard
        // mirrors `feed_matcher`'s: a map prefix still mid-match must resolve through
        // the matcher (or the `command_status` oracle), never be abandoned to a raw
        // read.
        if (self.editor.awaiting_command_continuation() || self.editor.cmdline_reads_raw())
            && self.keymaps.pending_empty()
        {
            self.editor.input(key);
            self.emit_lifecycle_events();
            return;
        }
        let scope = match crate::keymap::widget_bucket(self.editor.key_context()) {
            Some(bucket) => MatchScope::Widget(bucket),
            None => MatchScope::Editing(self.editor.mode),
        };
        if let Some(m) = self.keymaps.lookup_default(scope, key) {
            self.fire_mapping(m.rhs, m.silent, m.expr);
        } else {
            self.editor.input(key);
            self.emit_lifecycle_events();
        }
    }
}

/// Reconstruct the pasted text from the keys collected between the brackets — the
/// inverse of the client's `encode_paste`. `None` when the payload holds anything
/// that is not plain text (a modified key, a named key other than the line break /
/// tab the encoder emits), which sends the whole paste down the key-replay path
/// rather than guessing at a character for it.
fn payload_text(payload: &[Key]) -> Option<String> {
    let mut out = String::new();
    for key in payload {
        if key.ctrl || key.alt {
            return None;
        }
        match key.code {
            KeyCode::Char(c) => out.push(c),
            KeyCode::Enter => out.push('\n'),
            KeyCode::Tab => out.push('\t'),
            _ => return None,
        }
    }
    Some(out)
}
