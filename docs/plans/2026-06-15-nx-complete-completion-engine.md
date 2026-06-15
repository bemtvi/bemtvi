# `nx.complete` — the native completion engine on the unified float-list widget — phased plan

> Working checklist for **Phase 4** of the unified float-list widget
> (`docs/specs/2026-06-14-nx-ui-float-widget.md`) — the last widget phase. The
> widget core (Phase 1, `nx.ui.select`), the prompt input-grab (Phase 2,
> `nx.picker`), and the preview pane (Phase 3) are done; this phase drives the
> same `Menu` from the **insert-mode input path** as a pluggable completion
> engine with built-in `buffer` / `lsp` / `snippets` sources plus third-party
> sources. Lives at `docs/plans/2026-06-15-nx-complete-completion-engine.md`
> alongside the other `docs/plans/*.md`.

## Context

The unified widget (`docs/specs/2026-06-14-nx-ui-float-widget.md`) is one Rust
component — *a float containing a selectable, match-highlighted list*, with an
optional preview and an optional prompt. The engines on top
(`nx.complete` / `nx.picker` / `nx.ui.select`) are thin drivers; the widget does
the work, in Rust, because PUC-5.1 / no-frame-time-Lua (ADR 0002 rule 4) forbids
a Lua hot loop.

Completion is the widget's **fourth orchestration** and the deepest departure
from the other two on one axis the spec calls out — *the query source*:

- **No prompt → the query is the buffer.** Keystrokes go to the document; the
  engine watches the insert path (trigger chars, debounce) and re-ranks against
  the buffer prefix. The editor *is* the input field — completion has no prompt.
- This is the inverse of the picker, where the prompt grabs input. So the
  completion `Menu` is **non-grabbing**: it floats over the text, the document
  keeps taking keystrokes, and only navigation/accept/abort keys are intercepted
  *while the menu is visible*.

### The one wrinkle: a bespoke LSP pmenu already exists

`crates/nxvim-server/src/lsp/completion.rs` already implements a **native LSP
completion menu** — its own `CompletionMenu` struct, its own `pmenu_value`
redraw projection, its own `completion_menu_key` input routing, re-rank,
`completionItem/resolve` docs, and `textEdit`/`additionalTextEdits` accept. It is
**LSP-only, not built on the unified `Menu` widget, not exposed to Lua**, and
**manual-trigger only** (`<C-Space>` / `<C-x><C-o>`; it does not auto-fire as you
type — `crates/nxvim-server/src/lib.rs:1590`).

Decision (with the user, 2026-06-15): **build `nx.complete` on the unified widget
now with the `buffer` source, then fold LSP in as the built-in `"lsp"` source and
retire the bespoke pmenu** (Phase 4-C below). The two coexist only during Phases
4-A/4-B and are guarded so both popups never open at once.

### Strict layering (the rule that shapes every sub-phase)

`nxvim-core` stays **pure and synchronous**. Core owns: the completion `Menu`
state, the prefix derivation from the rope, the native `buffer` word-scan source,
the local `fuzzy::rank` matching, navigation/accept/abort, and a generation
counter + a `complete_query_changes` signal vec for *async* sources. *Everything*
async — LSP requests, `nx.spawn`, debounce timers, Lua source dispatch — lives in
`nxvim-server` and the Lua wrapper, exactly as the picker splits it.

## Core idea: completion is a third `Menu` kind, not a fourth widget

Extend the existing `Menu` (`crates/nxvim-core/src/editor/menu.rs`), do **not** add
a sibling. The widget already renders a cursor-anchored, match-highlighted,
selectable list (the `nx.ui.select` shape). Completion is that shape with two
differences, both modeled on `Menu`:

- **non-grabbing input** — distinguished by a new `MenuKind { Select, Picker,
  Complete }`. The input-path grab guard (`mod.rs:~1322`,
  `if self.menu.is_some() { self.handle_menu(key); return }`) becomes
  `if self.menu_grabs_input() { … }` — true for `Select`/`Picker`, false for
  `Complete`, so completion keystrokes flow on into `handle_insert`.
- **native accept-edit** — the `buffer` source inserts text directly (no Lua
  round-trip). `MenuItem` grows `insert: Option<String>` (the text to apply;
  `None` ⇒ use `label`); `Menu` grows `anchor: usize` (byte offset of the prefix
  start) so accept replaces `[anchor .. cursor)`.

Everything else — `all_items` / `filtered` / `match_spans` / `cursor` /
`menu_rows` / the `MenuView` projection / the client render path — is reused
unchanged. `fuzzy::rank` (`crates/nxvim-core/src/fuzzy.rs:29`) is the same matcher
the picker uses.

---

## Phase 4-A — Engine core + the `buffer` source ✅ DONE (2026-06-15)

Self-contained: no async, no Lua-per-keystroke, no LSP. A fully native,
insert-mode, complete-as-you-type popup over current-buffer words. Independently
shippable (working buffer completion), and the foundation the later sources
migrate onto.

**Shipped:** `MenuKind { Select, Picker, Complete }` + `MenuItem.insert` +
`Menu.anchor` (`menu.rs`); the non-grabbing input model (`menu_grabs_input()` gate
in `mod.rs`, the control-key interception + trigger-after-edit in `insert.rs`); the
native `buffer` word-scan source + trigger/prefix/accept in
`crates/nxvim-core/src/editor/complete.rs`; the `CompleteSetupReq` op
(`ops.rs`/`runtime.rs`/`install.rs`) → `editor.configure_complete`; the
`lsp_pmenu_open` coexistence guard synced in the server's `process_key`;
`prelude/complete.lua` (`nx.complete.setup{}`, `buffer` only, unknown sources +
`nx.complete.source{}` fail loud); the completion menu renders through the existing
Cursor-placement `project_menu` (no new client code). Tests:
`crates/nxvim-server/tests/complete.rs` (9, incl. the example). Example:
`examples/ui-complete/`. Defaults: `<C-y>` accepts (not `<CR>`, to avoid eating
newlines), `<C-n>`/`<Tab>`/`<Down>` move, `<C-e>` aborts; engine disabled until
`setup{}`. Builds clean native **and** `--no-default-features` (wasm parity: the
buffer source is pure core).

**Follow-ups added (2026-06-15):**

- **Manual trigger** — `nx.complete.trigger()` (Lua API) and a `keys.trigger`
  mapping in `setup{}` open the popup on demand, ignoring `auto` / `min_chars` (an
  explicit request always offers what's there, even an empty prefix → every buffer
  word). `Editor::complete_manual_trigger` (core) is driven by a payload-free
  `complete_triggers` queue (`nx._complete_trigger` → drained in `effects.rs`).
  This is the manual half that Phase 4-C reuses when `<C-Space>`/`<C-x><C-o>`
  retarget the engine.
- **Word-start anchor** — the popup anchors under the start of the word being
  completed, not the caret: `Menu.anchor_width` (the prefix's display width) →
  `MenuView.anchor_offset`, which `project_menu` subtracts from the cursor column in
  the Cursor branch. `0` for `select`, so that path is byte-for-byte unchanged.
- **Noselect (cmp-style)** — the auto-opened popup highlights **nothing** until you
  navigate, so a mapped `<CR>` confirm key stays a newline until you've actually
  picked a row (the original preselect-first made `<CR>` hijack every Enter).
  `Menu.selected_active` (false at auto-open, true on first nav / for a manual
  trigger which preselects row 0) → `MenuView`/`MenuData.selected_active`; the
  confirm handler accepts only when active and otherwise lets the key fall through
  (`<CR>` → newline, `<C-y>` → just dismiss). All three clients skip the highlight +
  scroll-from-top when `selected_active` is false. `set_complete_menu(…, preselect)`:
  `false` for auto-typing, `true` for an explicit `nx.complete.trigger()`.
- **Flush, borderless-top popup** — the completion popup drops its **top border**
  (so it abuts the line below the cursor) and each client offsets the box one cell
  **left** of `menu.col` so the left border doesn't push the list off the word.
  Driven by `MenuView.completion` → a `border_top: false` key in the menu redraw map
  (absent ⇒ a full border, the `select`/picker default) → `MenuData.border_top`.
  `project_menu` uses a `vchrome` of 1 (not 2) for the fit/placement math; the three
  clients render it: TUI (`Borders` minus top + 1-cell left shift), GUI
  (`fill_box_no_top` + drop the top pad + left shift), web (`border-top: 0`; its 1px
  rule needs no shift). `col` stays the logical word-start anchor — the border
  compensation is per-client (cell border vs 1px rule).

### Core (`nxvim-core`)

- **`Menu` / `MenuItem` extensions** (`menu.rs`): add `MenuKind`, `MenuItem.insert:
  Option<String>`, `Menu.anchor: usize`. `menu_grabs_input(&self) -> bool`.
  `menu_view()` already projects `placement=Cursor, query=None` for a `Complete`
  menu the same as a `Select` menu — verify, don't duplicate.
- **New `crates/nxvim-core/src/editor/complete.rs`** (declared in `editor/mod.rs`):
  - `CompleteConfig { enabled, auto, min_chars, keys: CompleteKeys }` stored on
    `Editor`; `CompleteKeys { next, prev, confirm, abort: Vec<Key> }` with the
    conventional defaults (`next`=`<C-n>`/`<Tab>`, `prev`=`<C-p>`/`<S-Tab>`,
    `confirm`=`<C-y>`/`<CR>`, `abort`=`<C-e>`). The server resolves any
    user-supplied notation to `Key`s and sets this (core stays parser-free).
  - `Editor::complete_prefix(&self) -> (usize /*anchor*/, String)` — the word
    chars (`char_class == Word`, `motions.rs:8`) immediately left of the cursor.
  - `Editor::buffer_candidates(&self, prefix) -> Vec<String>` — unique words in
    the current buffer (≥ min_chars, excluding the partial word at the cursor),
    pure rope scan.
  - `Editor::complete_trigger(&mut self)` — recompute prefix; if `auto` and
    `prefix.len() >= min_chars` and not suppressed (`lsp_pmenu_open`, below),
    gather + `fuzzy::rank` candidates and open/refresh a `Complete` `Menu`
    anchored at the prefix start (selected = none → first `<C-n>` picks row 0);
    else close any open completion menu. A no-candidates result closes it.
  - `complete_select_next/prev`, `complete_accept() -> bool` (replace
    `[anchor..cursor)` with the item's `insert`, move cursor to end, close),
    `complete_abort()`, `completion_active() -> bool`.
  - `lsp_pmenu_open: bool` on `Editor`, synced by the server (Phase 4-C deletes
    it) so `complete_trigger` no-ops while the bespoke LSP pmenu is up.
- **Input integration** (`insert.rs::handle_insert`): when `completion_active()`,
  match the key against `CompleteKeys` → drive nav/accept/abort and consume; else
  run the normal insert edit, then if `auto` and the key was a printable char or
  backspace, call `complete_trigger()`. `<Esc>` closes the menu as it leaves
  insert. The grab guard in `mod.rs` is loosened to `menu_grabs_input()`.

### Server (`nxvim-server`)

- A `CompleteOp::Setup { enabled, auto, min_chars, keys }` (parsed `Key`s) applied
  in `effects.rs` → `editor.complete_config`. Unknown source names rejected loud.
- Keep `editor.lsp_pmenu_open` in sync wherever `CompletionMenu` opens/closes.
- Redraw: a `Complete` menu projects through the existing `project_menu`
  Cursor-placement path — confirm clients draw it (they already render the
  `select` box). No new client code expected; if anchor-at-word-start vs
  anchor-at-cursor looks off, carry `anchor` into the geometry (minor).

### Lua (`nxvim-lua`)

- New `crates/nxvim-lua/src/prelude/complete.lua` (registered like `picker.lua` /
  `ui.lua`): `nx.complete.setup { sources = { { "buffer", min_chars = 3 } },
  auto = true, keys = { next=…, prev=…, confirm=…, abort=… } }`. Only `"buffer"`
  is recognized this phase; any other source name **errors loud** ("nx.complete
  source 'X' not yet implemented") per the no-silent-stub rule. `keys` notation is
  parsed server-side. `nx.complete.source{}` (plugin sources) is **not** added
  yet — calling it errors loud.

### Tests (`crates/nxvim-server/tests/complete.rs`, black-box)

- `nx.complete.setup` buffer source; type a prefix matching an existing buffer
  word → redraw shows the completion menu with that candidate; the **document
  buffer holds only the typed prefix** until accept.
- `<C-n>`/`<C-p>` move the selection; `<C-y>`/`<CR>` accept → buffer now holds the
  completed word, cursor after it; `<C-e>` aborts → buffer unchanged.
- Typing past `min_chars` opens; backspacing below it closes.
- Match-highlight spans track the prefix (assert spans in the redraw view).
- `<Esc>` closes the menu and leaves insert mode.
- Use `drain_to_latest_redraw`-style helpers (harness), never take-first.

### Example & verification

- `examples/ui-complete/` — config enabling buffer completion + a sample file
  with repeated words; verified end-to-end.
- `cargo build --workspace`; `cargo test -p nxvim-server --test complete`;
  `cargo clippy --all-targets -- -D warnings`; `cargo fmt --all -- --check`.
- Build the wasm edit-host `--no-default-features --features lua51` — the buffer
  source is pure core, so parity should be automatic; confirm it compiles and the
  menu opens in the web build.

---

## Phase 4-B — Async sources: evloop debounce + generation tokens ✅ DONE (2026-06-15)

Mirror the picker's async machinery so non-native sources can stream candidates.

**Shipped (with the user, 2026-06-15): the async substrate *plus* the public
`nx.complete.source{}` API pulled forward from Phase 4-E** — pure async plumbing
with no consumer is untestable under the project's no-silent-stub / faithful-test
rules, and the source API *is* the substrate the later sources build on, so it lands
here as the testable surface (the buffer source is synchronous and exercises none of
the async path). What landed:

- **Core** (`nxvim-core`, pure/sync): `CompleteCtx { buf, row, col, prefix }` snapshot;
  `CompleteConfig.has_async`; a monotonic `Editor::complete_gen` bumped per trigger;
  `Editor::complete_query_changes: Vec<(u64, CompleteCtx)>` (the completion analogue of
  `picker_query_changes`). The completion menu moved onto the **streaming model**: a
  `Menu.complete_prefix` field + `Menu::match_query()` (prompt query for a picker, the
  stored prefix for completion) so `extend_view`/`refilter` rank a streamed async batch
  against the prefix; `set_complete_menu` now takes `(gen, keep_open)` — it stamps
  `generation`/`items_gen = gen` and, with `keep_open`, holds an empty popup so async
  candidates have a widget to land in. `complete_trigger`/`complete_manual_trigger` route
  through one `refresh_complete` that seeds the buffer rows, bumps the generation, and
  emits a `(gen, ctx)` when `has_async`. `Editor::complete_finish(gen)` closes a
  confirmed-empty popup (completion has no prompt to keep up). Native buffer + async
  coexist by **concatenation** (buffer seeds, async appends) — priority-merge is 4-C.
- **Server** (`nxvim-server`, native): the settle fixpoint drains `complete_query_changes`
  → `lua.run_complete_run(gen, ctx)` → `apply_lua_effects` (so the source's debounce
  timer / `nx.spawn` actually starts), added to the convergence + recursion-limit clears;
  `take_complete_pushes()` feeds `menu_push` generation-gated (a batch behind the live
  prefix dropped), `take_complete_finishes()` → `complete_finish`.
- **Lua** (`nxvim-lua`): `CompletePush { gen, label, insert }` op + `complete_pushes` /
  `complete_finishes` Shared queues + `nx._complete_push` / `nx._complete_finish` bridges;
  `run_complete_run` passes the ctx as primitives (the crate can't see `CompleteCtx`).
  `nx._complete_setup` gained a `has_async` arg.
- **Prelude** (`prelude/complete.lua`): `nx.complete.source { name, complete, debounce }`
  registration (reserved built-in names fail loud); `setup{}` validates against built-ins
  **and** registered sources, collects the active async sources into `nx._complete`, and
  passes `has_async`. `nx._complete_run(gen, ctx)` debounces each source via `nx.timer`,
  builds `push`/`done`/`ctx.on_cancel`, batches pushes (`FLUSH_N = 256`), reaps a superseded
  run, and reduces all sources' `done()` to one `nx._complete_finish(gen)`.
  `nx.complete.debounce` default = 120 ms.
- **Tests** (`tests/complete.rs`, +5, 17 total): async source streams alongside buffer +
  accept; async-only source reacts to the live prefix + atomic gen swap; an empty source
  closes the confirmed-empty popup; a reserved built-in name fails loud; and a
  **deterministic generation-gating test** (a source that defers its push under test
  control proves a stale in-flight reply is dropped — no timers, no flakiness).
- **Example** (`examples/ui-complete/`): a `keywords` async source (debounce 80 ms,
  reacts to the prefix) registered alongside `buffer`.
- Builds clean native, `--no-default-features` (wasm subset — buffer stays pure core; async
  needs the native evloop, so **wasm async parity remains Phase 4-E**, exactly like the
  picker's live-grep), `clippy -D warnings`, `fmt --check`.

**Deferred to later sub-phases (unchanged):** multi-source **priority** merge (4-C, today
buffer+async just concatenate), the `lsp` / `snippets` built-ins, `trigger`-char gating per
source, and wasm async parity (4-E).

### Original plan (for reference)

- `complete_query_changes: Vec<(u64 /*gen*/, CompleteCtx)>` on `Editor` (the
  completion analogue of `picker_query_changes`); bumped on each trigger when at
  least one async source is configured. `CompleteCtx { buf, row, col, prefix }` —
  a snapshot, never live state (spec §1).
- `pending_complete` on `EditHost` (analogue of `pending_picker`, `lib.rs:~543`):
  holds the configured async sources + their cb-ids + `live_gen`. Settle fixpoint
  (`effects.rs:~1356`) drains `complete_query_changes` **before** pushes, runs
  each source's `complete(ctx, respond)`; a `respond{items}` for `gen < live_gen`
  is dropped; `menu_push`/`menu_finish` reused with the completion menu.
- **Debounce** via the evloop (a `nx.timer`, as the picker does): coalesce
  keystrokes; a new keystroke cancels the in-flight request. Default ~120 ms,
  configurable (`nx.complete.setup{ debounce = … }`).
- This unlocks third-party sources (Phase 4-E) and is the substrate Phases 4-C/4-D
  build the `lsp` and `snippets` sources on.

---

## Phase 4-C — The `lsp` source; retire the bespoke pmenu ✅ DONE (2026-06-15)

**Shipped (one pass, with the user 2026-06-15):** LSP completion folded in as the
built-in **server-native** `lsp` source on the unified menu, the bespoke pmenu
retired, multi-source priority merge, and a **mock-LSP black-box test harness**
(the repo's first — LSP had *zero* test coverage). Docs-beside-popup deferred to 4-D
(the unified markdown sidebar) per the user's call — `completionItem/resolve` docs
are not fetched yet.

- **The `lsp` source is server-native, not Lua** — LSP plumbing + the encoding-aware
  `textEdit`/`additionalTextEdits` accept live in `nxvim-server` (Lua's mutation API
  is nil; core is LSP-agnostic). The engine carries the menu/prefix/generation;
  accept is routed **per-item**: `buffer` rows insert natively in core, `lsp` rows
  set `MenuItem.source_accept` → core records `Editor::complete_accept_request` → the
  server applies the edit. (`crates/nxvim-server/src/lsp/completion.rs`, rewritten.)
- **Dispatch / reply / cache** — the settle loop drains `complete_query_changes` and,
  when the `lsp` source is configured (`complete_lsp_active`), calls
  `complete_lsp_dispatch`: reuse the cached items when the cursor is still in the same
  word and the last reply was complete (`!isIncomplete`) — re-push at the live gen, no
  round-trip — else issue `textDocument/completion`. The reply (`on_completion_reply`)
  caches the items + word anchor and `menu_push`es them at the live generation,
  gen-gated so a typed-past prefix's reply is dropped.
- **Delegated accept** (`complete_lsp_accept`) reuses the old accept logic verbatim:
  the item's `textEdit` (or `[word..cursor]` fallback) + `additionalTextEdits`
  (imports) in one undo step, cursor shifted past edits that precede it. **Caught a
  real bug via the test**: the accept-request drain was in `apply_lua_effects` (only
  runs when Lua/query work is queued) but a pure `<C-y>` queues neither — moved it to
  `run_pending`, which always runs once per key.
- **Retired**: `CompletionMenu`, `pmenu_value`, `completion_menu_key`, `lsp_menu_*`,
  `on_completion_reply`'s menu path, the `lsp_pmenu_open` core flag (+ its server
  sync), the `match_tier`/`is_subsequence`/`pmenu_item_width` helpers; the `pmenu`
  redraw key is now always `Nil` (kept for wire compat). `<C-Space>`/`<C-x><C-o>`
  rebound from `BuiltinAction::Lsp(Completion)` to a new `BuiltinAction::CompleteTrigger`
  → `complete_manual_trigger` (a no-op until `nx.complete.setup{}` enables the engine
  — **LSP completion is now opt-in via the engine**). Mouse routes rerouted to the
  engine (`complete_select_index` / `complete_accept`).
- **Priority merge** — `MenuItem.priority`; `Menu::sort_complete_view` (called from
  `menu_push` for `Complete` menus) stable-sorts the view by priority desc, fuzzy
  order within a source. Defaults `lsp=100`, `buffer=10`, overridable per entry
  (`{ "buffer", priority = 5 }`). `CompleteSetupReq` gained `lsp` / `buffer_priority`
  / `lsp_priority`; `complete.lua` recognizes `"lsp"` as a built-in.
- **Tests** — `crates/nxvim/tests/lsp_complete.rs` (new, in the `nxvim` crate for
  `CARGO_BIN_EXE_nxvim`): drives the scripted mock LSP (`nxvim --__lsp-mock`) via the
  `$NXVIM_LSP_CMD` env hook + the raw `nx._lsp_start` bridge (the `vim.lsp.*` user API
  isn't wired in Lua yet). Asserts (1) the server's items reach the unified menu and
  the document keeps only the typed prefix, and (2) accept applies `textEdit` +
  `additionalTextEdits` as one edit (`use foo;\nprint_value()`). Existing
  `complete.rs` (17) still green; clippy/fmt/wasm-subset clean.

**Known regressions (temporary, by decision):** no docs-beside-popup for LSP
completions (4-D), LSP `sortText` ordering yields to the engine's fuzzy rank, and
`completionItem/resolve` is not issued. **Behavior change:** LSP completion now
requires `nx.complete.setup{ sources = { { "lsp" } } }` (opt-in) rather than working
out of the box once a server attaches.

---

## Phase 4-D — Snippets + the docs sidebar (widget-spec preview kind `"markdown"`)

- **Snippet expansion on accept**: parse LSP `insertTextFormat = Snippet`
  (`$1` / `${1:default}` / `$0`) and expand into the buffer with tab-through
  placeholders; the `snippets` source (built-in). This is the first snippet engine
  in the repo (none exists today) — fail loud on unsupported snippet constructs
  rather than inserting raw `$1`.
- **Docs sidebar**: `preview = "markdown"` beside the completion menu (cursor
  placement = float beside, flipping for room — the pmenu's existing doc-float
  behavior, now via the unified preview pane from picker Phase 3). Resolved docs
  (`completionItem/resolve`) render server-side, zero Lua at frame time.

---

## Phase 4-E — Plugin sources, configurable triggers, wasm, polish

- `nx.complete.source { name, complete, debounce }` **already landed in Phase 4-B**
  (pulled forward as the testable surface for the async substrate). What remains here:
  `trigger = { chars = {…} }` per-source trigger-char gating (wake a source only after
  the configured char — the emoji example from the plugin-API spec §1) and the
  `resolve` callback for lazily-resolved docs.
- wasm parity for async sources over the off-tick / WebTransport proc seam (as
  picker live-grep does); build `--no-default-features`; `lua_int` for index
  Values (wasm `mlua::Integer` is i32).
- Example with `lsp` + `buffer` + `snippets` + a plugin source; extend the
  headless-Chromium `verify-ui.mjs` to drive completion in the wasm build.

---

## Risks & notes

- **Non-grabbing input model** — the completion `Menu` must *not* go through the
  `handle_menu` grab path; it intercepts only nav/accept/abort while open and lets
  every other key edit the document. Routing it like a select/picker would swallow
  typing — the exact opposite of completion.
- **Two completion popups** — until Phase 4-C, the bespoke LSP pmenu and the engine
  coexist. Guard `complete_trigger` on `lsp_pmenu_open` so they never both open;
  Phase 4-C deletes the guard with the bespoke pmenu.
- **No Lua per keystroke** — the `buffer` source is native rope-scan; async sources
  re-enter Lua only off the input path, debounced, generation-gated (rule 4).
- **No silent stubs** — an unimplemented source name errors loud at `setup`; raw
  snippet syntax is never inserted unexpanded; a source that "registers" but never
  produces candidates is the quietly-broken shape the project rule forbids.

## Verification (each phase)

1. `cargo build --workspace`; `cargo test -p nxvim-server --test complete`.
2. `cargo clippy --all-targets -- -D warnings`; `cargo fmt --all -- --check`.
3. `cargo run -p nxvim -- examples/ui-complete/sample.txt` with the example config:
   type to complete buffer words; `<C-y>` accepts; `<C-e>` aborts.
4. Build the wasm edit-host `--no-default-features --features lua51`.
