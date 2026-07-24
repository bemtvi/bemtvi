# LLM-slop review — fix backlog (2026-07-24)

Consolidated findings from a 7-agent review (core classics, core newer surfaces,
server src, lua bridge+prelude, clients, engines, tests). Temporary working doc —
check items off as they land; delete when done. Bug fixes follow the TDD rule
(failing test first). Line numbers are as of commit 91d3d458.

## A. Drift bugs (behavioral — test-first, one commit each)

- [x] **A1. `<expr>` textlock sandbox leak** — DONE. `Shared::discard_effects`
  (exhaustive destructuring, no `..` — compiler forces classification of every new
  field) + `LuaRuntime::discard_all_effects`; `loop_ops` deliberately survives
  (`nx.schedule` is the textlock escape hatch). Test:
  `keymaps.rs::expr_map_discards_queued_effects` (feedkeys leak vector).
- [x] **A2. `ex_takes_bar` abbreviation drift** — DONE. Rewritten as a
  `(full name, min abbrev len)` prefix table matching the dispatchers'
  abbreviation model (also fixes previously-missing `mak`, `aut`/`auto`/…,
  `comm`/…); unimplemented vim-parity names (`function`, `*do` family) keep
  exact-spelling-only. Tests: `editing/global_cmd.rs::
  {global_full_name,global_abbreviation,normal_abbreviation}_keeps_the_bar`.
- [x] **A3. `:echo` non-ASCII mojibake** — DONE. Both string lexers (and the
  unknown-escape arm + the E15 catch-all) copy whole chars via
  `src[i..].chars().next()` + `len_utf8` advance instead of `byte as char`.
  Test: `editing/core_editing.rs::echo_keeps_non_ascii_intact`.
- [x] **A4. GUI scroll-anim drops `diagnostics_virt`** — DONE (minimal fix): band
  field added through `ScrollAnim`/`ScrollFrame` + painted in the band branch
  mirroring the settled path; `ScrollAnim`/`new`/`frame` made pub for the Tier-1
  data-contract test `nxvim-gui/tests/scroll_band.rs` (+ `Default` derive on
  `ScrollData`). Pixel check needs a GPU (documented Tier-1 convention). The
  structural fix (share the machinery in nxvim-view) remains B1.
- [x] **A5. `save_tick` dual semantics** — DONE. `save_tick` is now a genuinely
  monotonic per-buffer save counter: `mark_clean` no longer touches it (a load is
  not a save), `mark_saved` bumps it (an external write is), and the three
  buffer-replacing reload sites (`load_into_current`, `load_pending_open`,
  `reload_buffer`) carry it across the swap — a fresh `Buffer`'s 0 previously
  made the LSP `!=` diff read every `:e!` reload as a save. Test:
  `nxvim/tests/lsp_features.rs::did_save_fires_per_write_and_never_on_reload`
  (mock record file; also locks one-didSave-per-`:w` incl. repeat `:w`).
  (Checked while here: the post-reload didChange is safe — `mark_resync()` sets
  `batch.resync`, so `did_change_content` ships the full reloaded text.)
- [x] **A6. Float-config policy drift** — DONE (policy fixes): `parse_border`
  errors loudly on a non-string (was silent `"none"`); empty-title-clears now
  lives once in `effects::normalize_title`, used by `parse_title`,
  `build_float_config`, and the SetConfig arm. Tests:
  `windows.rs::{open_win_non_string_border_errors_loudly,
  empty_title_clears_on_the_lua_op_path}`. NOTE discovered en route:
  `nx._open_float`/`nx._win_set_config` bridges have ZERO prelude callers
  (`vim.api.nvim_open_win` doesn't exist Lua-side) — the api.lua:278-279
  comment describing their mirror write-through is stale; the full
  shared-builder unification of the two decoders remains C-class cleanup
  (fold into C-series if desired).
- [x] **A7. wasm LSP `shutdown()` never sends the shutdown request** — DONE:
  fire-and-forget `shutdown` request framed before the `exit` notification
  (late reply drops with the removed state), matching the native loop and the
  doc. Test: `nxvim-lsp/tests/sync_shutdown.rs` — NOTE it compiles only under
  `--no-default-features` (the sync client is wasm-only by documented decision),
  so run `cargo test -p nxvim-lsp --no-default-features` to exercise it.

## B. Cross-client dedup (move into nxvim-view / nxvim-server)

- [x] **B1. Scroll-anim snapshot+lifecycle → nxvim-view** — DONE. One `ScrollAnim`
  (a single `ScrollData` clone + arm instant, `done()`/`progress()`) plus
  `arm_scroll`/`repaints_destination` in `nxvim-view/src/anim.rs`; the TUI keeps
  its owned skip/take band build, the GUI its `ScrollFrame::of` borrow projection.
  Subsumed A4; `nxvim-gui/tests/scroll_band.rs` still locks the band contract.
- [x] **B2. Remote-image fetch cache → nxvim-view** — DONE. `nxvim_view::images`:
  `ImageFetch`, `RemoteImages` (ensure_fetch *returns* the request so the module
  stays transport-free), `decode_file`/`decode_bytes`/`MAX_EDGE`. Clients keep only
  their paint layers (ratatui protocol / wgpu texture).
- [x] **B3. `parse_connect_uri` → nxvim-server** — DONE, in daemon.rs with
  `CONNECT_URI_SCHEME`; the GUI re-exports it so `tests/remote.rs` covers the
  shared copy.
- [x] **B4. Daemon-session plumbing → nxvim-server** — DONE.
  `session_spawn.rs`: `spawn_session_thread` (duplex + thread + ready-channel
  handshake), `connect_daemon_respawning` (the ×5 child-slot factory),
  `daemon_log_stderr`, `env_daemon_command`/`DAEMON_CMD_ENV`;
  `ReconnectSpec::reject_keep_buffers`. GUI server errors now surface at join.
- [x] **B5. Pure text-fitting helpers → nxvim-view** — DONE. `fit.rs`
  (`pmenu_start` with the GUI's `rows > 0` guard, `pmenu_row`, `elide_middle`,
  `elide_keep_tail`, `gutter_cell` with the TUI's `Option<usize>` filler arg),
  `keys::mouse_modifier(ctrl, shift, alt)`, `images::image_read_reply` (the
  inlined `fetch_image_bytes` successor). `pad_to_width`/`expand_tabs` stay
  deliberately per-client (TUI display-width vs GUI char-based column math) —
  documented on both sides.
- [x] **B6. Dead client pmenu plumbing** — DONE. `pmenu_geometry`/
  `pmenu_doc_geometry` (+ their private `text_inner_rect`/`float_inner` support
  and `paint_doc_scrolled`) deleted; the always-0 `doc_scroll` param removed from
  both render stacks; GUI `mouse_wheel` docstring rewritten (server hit-tests
  wheel events; nothing scrolls client-side).

## C. Server-src dedup

- [ ] **C1. `load_replica_wasm` vs `load_replica_bytes`** — lib.rs:2014-2052 vs
  lifecycle.rs:102-136: identical 10-step bodies under opposite cfgs; extract one
  un-cfg'd `load_replica_common`.
- [ ] **C2. `ts_install` cfg twins** — excmd.rs:470-486 vs 494-508 byte-identical;
  collapse (divergence already lives behind `fx.ts_install`).
- [ ] **C3. LSP `register_*_request` ×3** — lsp/folding.rs:112, lsp/semantic.rs:235,
  lsp/inlay.rs:331 → one `register_buffer_scoped_request(kind, buffer)`.
- [ ] **C4. shada `merge_*` ×6** — shada.rs:885-1160 → generic
  `merge_table<K,V: HasTs>`; also kill the duplicated
  `Err(TableDoesNotExist)/Err(_)` double arms and the per-table `write_state` stanzas.
- [ ] **C5. effects.rs small dups** — six identical action-drain loops (407-449);
  statusline-publish stanza duplicated (1055-1068 vs 3988-4001).
- [ ] **C6. lifecycle `fire_*` tail ×6** — lifecycle.rs:1265-1388; merge
  `fire_buf_win_enter`/`fire_buf_add` (identical but for event name).
- [ ] **C7. daemon.rs mirror pairs** — `decode_spawn`(1653) vs `decode_dproc_open`(2892);
  `split_single_stream`(616) vs `serve_daemon_link`(3793); `RemoteFsJobs::connect`(3143)
  vs `RemoteHttp::connect`(3236).
- [ ] **C8. dispatch.rs** — buffer-handle resolution pasted ×3 (487,520,745 →
  `resolve_buf` sibling of `resolve_win`); `nvim_exec_lua` silently drops non-empty
  `args` (218-244) — error loud per no-silent-stub rule.
- [ ] **C9. `statusline_click_at` re-derives `render_statusline`'s format-resolution**
  — redraw.rs:750-778 vs 795-875 → shared `resolve_fmt_and_pieces`.
- [ ] **C10. redraw hot-path clones** — redraw.rs:1655-1675 `unbundle_rows` clones
  `secondary_selection`/`search`/`virt_lines` per row per frame; borrow instead.
  Also delete dead `let _ = text_width;` (redraw.rs:229).
- [ ] **C11. `:colorscheme` no-arg is a silent no-op** — excmd.rs:598-607; doc says
  "report the active colorscheme". Implement the report or echo loud.

## D. Core dedup / cleanup

- [ ] **D1. `stamp_disk` == `set_disk_stat`** — buffer.rs:767-769 vs 785-787 byte-identical;
  delete one, retarget caller (editor/buffers.rs:959/923).
- [ ] **D2. Menu/MenuItem/Window literals** — `Menu` (~28 fields) ×5
  (menu.rs:623,696,776,1070,1173), `MenuItem` ×4, `Window` (15 fields) ×5
  (windows.rs:544,587,1937,2198,2241) → `Default`/constructor + struct-update.
- [ ] **D3. Substitute-name list ×5 + subst parse prologue ×3** — ex.rs:445,1099,1379,
  1467,1513; `ex_preview_pattern`/`subst_preview_active`/`subst_preview` share
  trim→range→name→bang→delimiter prologue → `is_substitute_name()` +
  `parse_subst_cmdline()`. (Same hazard class as A2.)
- [ ] **D4. Replacement-buffer pick ×2** — buffers.rs:1146-1160 vs 2356-2366+2418 →
  `replacement_in_layer(excluding, layer)`.
- [ ] **D5. helix pending-reset ×3** — helix.rs:334,351,684 → `reset_helix_pending()`.
- [ ] **D6. helix splice-and-refit ×3(+1)** — `helix_surround_add`(1786),
  `helix_rotate_contents`(1513), `helix_align_selections`(1609) (+`apply_surround_ops`
  1964 variant) share (lo,hi,idx,head_high) sort/splice/cum-shift/refit → one helper.
  Offset math = highest silent-corruption risk of the dup family.
- [ ] **D7. marks/jumps/changes listing dups** — `ex_marks` vs `marks_mirror`
  (marks.rs:295 vs 359) full walk ×2; `ex_changes`(changelist.rs:81) vs
  `ex_jumps`(jumps.rs:229) marker/count table; `buffer_display_name` fallback pasted
  ×3 (jumps.rs:248, marks.rs:323,396).
- [ ] **D8. Scroll row-walk ×2** — cursor.rs:536-564 vs 573-596 identical
  fold/virt/wrap per-line step → extract.
- [ ] **D9. Misc small** — ex_delete re-derives `linewise_span` (ex.rs:2059 vs 2103);
  transient-state reset stanza ×4 with one divergent copy missing `message.clear()`
  (windows.rs:2739, buffers.rs:1114, tabs.rs:341, buffers.rs:2445 ← divergent);
  `cap_ring` inlined at cmdline.rs:539 + mod.rs:2430; `tab_window_buffers` re-derives
  `tab_window_ids` (tabs.rs:66 vs 78); 3 identical win-str option stanzas
  (options.rs:536-620 → `set_win_str`); fold.rs:893 `let _ = buf;` dead;
  fold.rs:1320-1359 parses each foldexpr value twice; helix.rs:277 dead `_new_len`
  param on `SurroundOp::new`; dock.rs:416-423 `show_dock` double-relayout no-ops;
  mouse.rs menu press/wheel handler pairs ×2 (1675/1760, 1696/1778); mouse.rs mid-band
  geometry derived ×3 (1007,1024,1209 → `mid_band()`).

## E. Doc-comment repairs (mechanical, one commit)

Functions inserted above another fn's `///` block — docs attached to wrong item:
- [ ] cmdcomplete.rs:273-299 (`cmdline_complete_accept` doc on `cmdline_replace_arg`)
- [ ] menu.rs:1108-1143 (sentence severed; stray tail at 1142), menu.rs:1724-1736
- [ ] mouse.rs:995-1024, 1941-1982, 2467-2499
- [ ] redraw.rs:367-373 (doubled doc), 712-723, 1410-1417, 1576-1586
- [ ] gui render.rs:3683-3684 + 3704-3705 (`elide_middle`/`pmenu_row` docs scrambled)
- [ ] autocmd.lua:305-326 (`nx._fire` doc stranded 68 lines up)
- [ ] search.rs:254-260 doubled doc on `word_under_cursor` (delete abandoned draft)

Stale docs describing removed worlds:
- [ ] luafs.rs:1-35 (BlockingSystem/sync-vim.fn/RemoteLuaFs all gone)
- [ ] nxvim-ts engine.rs:228-231 + loader.rs:7-11,41-46 (folds/textobjects now engine-side)
- [ ] stale `vim.system`/`nx._system` naming: convert.rs:315, runtime.lua:163,
  install.rs:965, runtime.rs:2431

## F. Lua prelude dedup (promote per the nx.utils rule)

- [ ] **F1.** `dirname` ×2 (editorconfig.lua:297, lsp.lua:81) + walk-up-ancestor loop ×2
- [ ] **F2.** basename chain ×3 (stdlib.lua:146, plugins.lua:174, plugins_ui.lua:42 —
  inlined twice in one function)
- [ ] **F3.** `~`-expansion ×3, divergent edges (cmdline_complete.lua:381, vimfn.lua:351,
  plugins.lua:182)
- [ ] **F4.** `build_argv` ×2 (process.lua:16, localseam.lua:25)
- [ ] **F5.** `key_list` keys-spec normalizer ×2 (complete.lua:49, snippet.lua:25)
- [ ] **F6.** caller-source stack walker ×2 (stdlib.lua:159, vimfn.lua:339)
- [ ] **F7.** lsp.lua:88-105 `cursor_word` → use `nx.expand("<cword>")`;
  explorer.lua:72 `edit_escape` → use `nx.fname.escape`
- [ ] **F8. Dead:** `nx._proc_pids` write-only registry + Rust plumbing
  (runtime.lua:163-170, runtime.rs:2431, lib.rs:2145, effects.rs:2853) — delete or
  expose `.pid`; api.lua:1699 dead no-op branch; promise.lua:290 `list_len` wrapper;
  autocmd.lua:143-171 4× lazy-init stanza

## G. Test-suite cleanup

- [x] **G1. Remove/convert the 16 examples-loading tests** — DONE. Deleted 12 whose
  behavior non-example tests already cover (picker ×3, cmdline_complete, autocmds,
  complete, diagnostic_nav, folds_example.rs whole file — editing/folds.rs covers
  indent folds —, regex, session, shada, padding) plus the now-orphaned
  `example_dir`/`pump_until_any_window_has` helpers; converted 4 with unique coverage
  to inline configs (dock winhighlight *render*, markdown float via inline
  `nx.view.component`, smart-indent `vim.o` defaults, syntax float rust-injection via
  a test-written config dir).
- [x] **G2. Promote to nxvim-test-harness** — DONE (3 commits). `DaemonFs` is one
  superset fake (files + optional dirs + `fail_writes` + all the mutators the nine
  copies had grown); `spawn_with_daemon_fs(_init)`, `await_lines(_where)`,
  `poll_menu`/`poll_no_menu`/`menu_of`/`menu_items`, `start_with_config` (+
  `config_init` + file variant), `redraw_after(_matching)`, `message_after`
  (divergent chdir/tabs impls folded into the canonical drain-to-latest),
  `feed_sync`, `start_clocked(_init)`, `start_with_file` (the `open(content)`
  fixture), `buf_name(_of)`, `q()`, `poll_true`, `await_server_exit` all hoisted;
  `temp_file` ×2 → `write_temp`. Divergent-geometry / divergent-convention call
  sites keep one-line local adapters (editing/support 80×25, whole-frame
  `menu_items`, single-shot lsp_complete poller).
- [x] **G3.** DONE — `drain_all_redraws`/`window0_get`/`drain_notify` deleted (zero
  callers), `attach*` trio collapsed onto `attach_with_caps`, and the blanket
  `#![allow(dead_code)]` in editing/support.rs removed outright (nothing left dead).
- [x] **G4.** DONE — keymaps' three visual-`gg`-under-collision tests, encoding's
  Shift_JIS/EUC-JP round-trips, and options' scrolloff/colorcolumn round-trips are
  each one table-driven test.
- [x] **G5.** DONE via G2 — `redraw_after` now genuinely lives in the harness, so
  CLAUDE.md's sentence is true as written.

## H. Engines (small)

- [ ] **H1.** lsp/dispatch.rs:143-369 — route the 8 hand-inlined arms through
  `unwrap_logged` (all but ResolveCodeAction qualify).
- [ ] **H2.** mock.rs:494-526 `record`/`record_named` → one `append_record`.
- [ ] **H3.** ts install.rs:245-281 textobjects stanza → closure over (url, name);
  engine.rs:369 unused `_lang` param → rename `rebuild_all_injection_layers()`.
- [ ] **H4.** XDG dir resolution ×4 crates (lsp log.rs:158, ts lib.rs:29,
  lua host.rs:130, server shada.rs:1273) — consider one shared helper.

## Cross-world duplicates noted for awareness (not necessarily actionable)

- core `fmod_*`/`apply_file_mods` (ex.rs:251-335) vs Lua `fnamemodify` (vimfn.lua) —
  two-world duplicate, drift-prone by construction.
- `dir_listing` (buffer.rs:1003) must match prelude/explorer.lua render byte-for-byte —
  wants a conformance test.
- core mouse geometry mirrors (`tab_cell_width` ↔ tui render_tab_cells;
  `place_docs_beside` core mouse.rs:22 ↔ server redraw.rs:1472) — documented lockstep;
  consider sharing or conformance tests.
- Content-Length framing ×3 (sync_client.rs, mock.rs, daemon lsp leg) — mock's copy is
  a separate test binary, acceptable.
