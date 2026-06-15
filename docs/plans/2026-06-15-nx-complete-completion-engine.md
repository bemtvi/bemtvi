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

## Phase 4-B — Async sources: evloop debounce + generation tokens

Mirror the picker's async machinery so non-native sources can stream candidates.

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

## Phase 4-C — The `lsp` source; retire the bespoke pmenu

- Add `"lsp"` as a built-in source feeding the engine: route
  `textDocument/completion` + `completionItem/resolve` results into `menu_push`
  with `MenuItem.insert` = the item's `insert_text`/`text_edit` text.
- **Delete** `lsp/completion.rs`'s bespoke path — `CompletionMenu`, `pmenu_value`,
  `completion_menu_key`, `lsp_menu_*`, the `lsp_pmenu_open` flag — and rebind
  `<C-Space>`/`<C-x><C-o>` to *manually* trigger the engine. `additionalTextEdits`
  (imports) applied on accept by the engine.
- **Multi-source fan-out + priority**: `nx.complete.setup{ sources = { {"lsp",
  priority=100}, {"buffer", priority=10, min_chars=3} } }`; merge ranked results
  across sources by priority then fuzzy score, generation-gated so a slow LSP
  reply for a typed-past prefix is dropped (spec §query-source axis, picker
  Decision 3).
- Regression: confirm `<C-Space>` LSP completion still inserts `textEdit` +
  imports exactly as the bespoke pmenu did, now through the widget.

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

- `nx.complete.source { name, trigger = { chars = {…} }, complete, resolve }` —
  third-party sources (the emoji example from the plugin-API spec §1). Trigger
  chars wake a source only after the configured char.
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
