# Session restore on the web build

Status: **done** — 2026-08-14. All four phases landed.

The workspace/session tier — capture the window/tab layout at exit, rebuild it
exactly at boot, and hand each persisted `btv.view` slot back to its owning
plugin — works natively and is inert in the browser. This plan closes that gap.

It is the *tier-1 rule* applied to cross-session state: "a daemon or web/wasm
session is not a degraded mode — any feature that works locally must work
identically over the wire." Today it does not, and worse, it fails **quietly**.

## The gap, precisely

`session_captures_layout()` (`crates/bemtvi-server/src/input.rs`) was:

```rust
#[cfg(feature = "native")]      { self.workspace_session && self.lua.session_save_layout() }
#[cfg(not(feature = "native"))] { false }
```

and the three sites that attach `snap.session` / `snap.workspace_options`
(`shada_checkpoint` / `shada_flush_final` / `shada_write_now`) are all native
redb flush sites. The wasm `EditHost::export_persist` folded in `plugin_data`
and nothing else. So in a browser:

- `btv.shada.save_layout(true)` — a **public** Lua API any web config can call —
  set the flag and did nothing. No warning, no error.
- `btv.view`'s `on_restore` never fired: pending restores are only populated by
  `build_layout` ← `restore_session` ← `apply_pending_session_restore`, which is
  behind `#[cfg(feature = "native")]`.
- `btv.workspace.*` reported nothing (`set_workspace_identity` is `run_io`-only).

Measured, not inferred: a web config calling `btv.shada.save_layout(true)`, then
`:split` + `:vsplit` (1 → 3 windows), `shadaFlush`, reload → the OPFS blob was
267 bytes with no `"session"` key, and the window count came back **1**.

## What makes this tractable

Three things are already in place, which is why this is wiring rather than a
rewrite:

1. **`SessionState` already serializes.** It carries
   `#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]`, and
   `PersistState.session` is a plain `Option<SessionState>` — the web blob is
   `serde_json` over `PersistState`, so the layout crosses for free.
2. **The async-fs problem is already solved in core.**
   `open_buffer_for_restore` (`editor/buffers.rs`) branches on
   `host_fs_offtick` and enqueues a replica open rather than reading
   synchronously. The web build calls `enable_offtick_fs()`, so a restored leaf
   becomes an async OPFS fetch the Worker fulfills — the same path `:e` takes.
3. **The ordering constraint is already satisfied.** Native restores *after*
   config so restored windows inherit the config's window-local options. On web,
   `eh_load_shada` already runs after config sourcing (worker.mjs
   `bootWithConfig`) and before `eh_boot_finish`, which is exactly the right
   seam.

## The one real design decision: what plays `--workspace` on the web

Natively, capture needs **two** opt-ins (`workspace_session` from `--workspace`,
plus `btv.shada.save_layout(true)`), and restore needs a third
(`--restore-session`). A browser has no argv.

**Decision: the origin is the workspace, and `save_layout(true)` means both.**
The web shada is a single OPFS blob scoped to the page's origin — there is one
session per origin by construction, which is precisely what `--workspace` names
natively. So on wasm the gate is `self.lua.session_save_layout()` alone, and
capturing implies restoring (there is no second flag to carry, and a session you
captured but can never restore is the silent no-op we are removing).

Rejected: a `?workspace=` URL param. It would mirror the native flag literally,
but it invents a second axis for a build that has exactly one shada blob, and it
puts session identity in a link that can be copied between tabs.

## Phases

Each phase is independently verifiable and lands as its own commit.

### Phase 1 — capture and carry the layout ✅

Make the persist seam honest in both directions; apply nothing yet.

- `session_captures_layout()`: wasm arm returns `self.lua.session_save_layout()`.
- wasm `EditHost::export_persist`: attach `session` under that gate, mirroring
  the three native flush sites.
- Un-gate the `pending_session` field so both builds carry it; wasm
  `import_persist` *takes* `state.session` into it instead of dropping it on the
  floor (`Editor::apply_persist` ignores the field entirely).

Verified by `web/verify-session.mjs`: with `save_layout(true)` the OPFS blob
carries a `"session"` with the captured split tree; with it off the blob has
none; a blob containing a session re-loads without error. **No restore yet** —
that is Phase 2, so nothing here claims to restore.

### Phase 2 — rebuild the layout at boot ✅

- `apply_pending_session_restore` moved out of the `#[cfg(feature = "native")]`
  shada glue into the ungated `lifecycle.rs`, next to `restore_persisted_views`:
  rebuilding a layout is startup lifecycle, not redb.
- `boot_finish` calls it before the lifecycle seed, in `run_io`'s order — after
  the config, so restored windows inherit its window-local options.
- The baseline seed is factored into `seed_startup_baselines()` and run **twice**
  on a restoring boot: once in `boot_begin` so the config sees a seeded editor,
  and again in `boot_finish` after the restore mints its windows. Without the
  second pass a restoring boot fires 3 spurious `WinNew` / 1 `TabNew` /
  3 `BufAdd` at a config's autocmds (measured by removing the line).
- `finalize_session_focus()` after `VimEnter`, so a session quit from a dock
  reopens there.

The async-open risk was real but benign: restored leaves land as OPFS replica
opens, so the window *tree* is correct immediately and each buffer's text fills
in a tick or two later. `verify-session.mjs` polls the two separately.

### Phase 3 — `btv.view` slots and their plugins ✅

`restore_persisted_views()` now runs on the wasm boot path, between the layout
restore and the baseline seed (`run_io`'s order, so the placeholder churn fires
no spurious `WinNew` / `WinClosed`). No Lua change was needed: the dispatch, the
lazy-plugin wake and the collapse decision all already live in
`btv._run_view_restores()`.

The collapse-coordinator worry did not materialise — an unclaimed slot collapses
correctly with the browser's synchronous `package.preload` plugins, verified
directly rather than assumed. `web/verify-view-restore.mjs` covers the slot being
recorded in the captured layout, `on_restore` receiving the persisted id and
adopting its window, the rebuilt content painting there, and an unclaimed slot
collapsing instead of leaking an empty placeholder window.

### Phase 4 — the surfaces, and the loud edges ✅

- `btv.workspace.active()` / `dir()` now report the web model: the session is a
  workspace rooted at its effective directory (the OPFS root serverless, the
  daemon's directory in a daemon session, re-published wherever that root
  arrives). They said `false` / `nil`, which is not a neutral default — a plugin
  gating persistence on them (bemtvi-dap keys its store that way) silently
  skipped it in a browser.
- `btv.shada.namespace()` is deliberately left `nil` on web. Natively it is the
  `ns/<id>/` token isolating one launch's store from another's; on the web the
  origin does that job and there is no second token. Reporting a fake one would
  be worse than reporting none.
- `btv.wso` now round-trips: `Editor::apply_persist` already seeded the overlay
  on load, so only the wasm export half was missing, which made a `btv.wso`
  write look like it persisted and then quietly not. It rides ungated — natively
  it is gated on `workspace_session` to keep a *global* store free of
  per-workspace overrides, and on the web there is no global store to protect.
- The stale claim in `docs/plans/2026-06-28-plugin-view-persistence.md`
  ("native-only by inheritance") is marked superseded. Its prediction held
  exactly: the shared dispatch needed no changes.

**No `examples/` config.** The repo convention covers config-facing features
runnable with `BEMTVI_CONFIG=examples/<f> cargo run`, and the web half of this
cannot be exercised that way. `web/verify-session.mjs` and
`web/verify-view-restore.mjs` are the runnable demonstration. Adding
`save_layout` to the demo-seed config was considered and rejected: a visitor
landing on a stale layout from a previous visit is a worse first impression than
the tour opening.

## Risks, as they actually resolved

- **A restored layout of async opens.** Every leaf is a deferred OPFS fetch, so
  the window *tree* is correct immediately and each buffer's text fills in a tick
  or two later. `verify-session.mjs` polls the two separately.
- **A stale blob outliving its files.** Resolved differently from the prediction,
  and the prediction was wrong: `build_layout` drops a leaf whose file is gone
  only when the read is **synchronous**. With off-tick fs,
  `open_buffer_for_restore` enqueues a replica open and cannot know the file has
  vanished, so the leaf is *kept* and its buffer stays empty — which is precisely
  what `:e /gone.txt` gives you on this build, so it is the consistent answer
  rather than a hole. Native collapses the split; the web keeps the window. An
  accepted divergence, now pinned by an explicit check in `verify-session.mjs`
  (boot is clean and both windows come back, one empty) instead of an assumption.
- **Blob size.** The layout is small, but it now rides every checkpoint. The
  Worker's baseline-diff (`shadaBaseline`) keeps unchanged sessions from
  rewriting, so this does not add write churn.

## Follow-up fix — the restored-focus hold had no release point on the web

Shipped broken and fixed after review. `Editor::finalize_session_focus` **peeks,
never clears**: a restore stashes the layer the session was quit from and
`settle_events` re-asserts it on *every* settle (an fs completion, an LSP reply, a
watch, a proc line) until the first user input releases it. Natively that release
is `clear_session_focus_hold()` in the `btv_input` / `btv_input_mouse` dispatch
arms — and the wasm input entry points (`EditHost::feed` / `EditHost::mouse`,
behind `eh_input` / `eh_input_mouse`) called neither. So on a restoring web
session the hold never lifted: move focus anywhere, and the next async settle
yanked it straight back to the captured layer. Opening a file with `:e` is enough
to trigger it, since the OPFS read completion settles.

The fix mirrors the native boundary: both wasm entry points clear the hold before
the tick. Reproduced first — the check "an async settle after user input does NOT
yank focus back to the restored layer" in `verify-session.mjs` fails against the
pre-fix wasm build (focus returns to `/a.txt`) and passes after.

The general lesson for tier-1 parity: a state machine whose *release* edge lives
in the native RPC dispatch has no counterpart on the web, where the Worker calls
`EditHost` methods directly. Grep the dispatch arms, not just the tick, when
porting a feature that holds state across ticks.

## Follow-up fix — the restored cursor and scroll were clamped against an empty buffer

Found reviewing the feature, and the more visible half of "comes back the way you left
it": the layout came back, the files came back, and the **cursor came back on line 1**.

`Editor::install_restored_tree` sets the focused window's saved cursor/scroll and then
`clamp_cursor()`s. Natively that is safe — the restore's leaf reads are synchronous, so
the buffer already holds the file. Off-tick it is not: `open_buffer_for_restore` only
*enqueues* a replica open, so the buffer is an EMPTY replica at that moment and the clamp
snaps the saved line to the top. The text then lands a tick later and the window looks
perfectly restored, which is why nothing caught it — `verify-session.mjs` asserted the
window count, the file names and the text, never a position.

Two more sites destroy the same value on a multi-tab restore, both against a buffer whose
bytes are still in flight: `stash_focused_view` (leaving a tab writes the placeholder
clamp into the window it is leaving) and `enter_window` (entering a tab clamps the saved
position it just restored).

The core already models exactly this — `pending_open_cursor` / `settle_loaded_cursor`, the
record a jump into a not-yet-fetched buffer uses (`land_cursor`). It was simply not used by
the restore, so the fix is that one rule applied at the three sites: **a window whose
buffer has not arrived yet carries the position it is waiting for, not the placeholder the
clamp produced.** The record grew a `top`, so a *view* (a restored or re-entered window)
also gets its scroll back rather than having it re-derived — without it the cursor returns
on the right line but pinned to the last screen row, since `ensure_visible` re-scrolls from
the top of the file. A jump still carries `top: None` and scrolls minimally, as it should.

The gate is `has_pending_open`, so every synchronous local session is untouched (no
`host_fs_offtick` branch anywhere) — which is also why the whole native suite is unaffected.

Reproduced before fixing, in both worlds:

- Native, over the daemon fs (the same off-tick seam the browser uses):
  `session_restores_the_cursor_over_off_tick_fs`,
  `session_restores_the_cursor_in_every_tab_over_off_tick_fs` and
  `session_restores_the_scroll_position_over_off_tick_fs` in
  `crates/bemtvi-server/tests/session.rs` — each fails on the unfixed core (cursor `(1,0)`
  instead of `(4,0)`; cursor screen row 22 instead of 0). Each of the three edits was
  mutation-tested individually.
- Web: `verify-session.mjs` grew a cursor check, and it reads `row<1>` against a rebuilt
  pre-fix wasm host and `row<3>` after.

The lesson, next to the focus-hold one above: an off-tick open hands out an **empty
buffer** first, so any position clamped against it in the meantime is silently destroyed.
When porting something position-bearing to the off-tick tier, look for the clamp, not just
the read.
