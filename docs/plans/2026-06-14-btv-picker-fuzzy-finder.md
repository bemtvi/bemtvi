# `btv.picker` — the fuzzy finder on the unified float-list widget — phased plan

> Working checklist for **Phase 2** of the unified float-list widget
> (`docs/specs/2026-06-14-btv-ui-float-widget.md`): the prompt input-grab, the Rust
> fuzzy matcher, dynamic-source forwarding with generation tokens, and incremental
> streaming — which together unlock `btv.picker`. Project convention: lives at
> `docs/plans/2026-06-14-btv-picker-fuzzy-finder.md` alongside the other
> `docs/plans/*.md`.

## Context

The unified float-list widget (`docs/specs/2026-06-14-btv-ui-float-widget.md`) is the
single Rust component that completion, the fuzzy picker, and `btv.ui.select` all
render through — *a float containing a selectable, match-highlighted list*, with an
optional preview and an optional prompt, placed under the cursor or centered over
the editor. The engines on top (`btv.complete` / `btv.picker` / `btv.ui.select`) are
thin drivers; the widget does the work, in Rust, because PUC-5.1 / no-frame-time-Lua
(ADR 0002 rule 4) forbids a Lua hot loop.

**Phase 1 is done** (commits `d648335`, `bf1eb21`). `Menu` in
`crates/bemtvi-core/src/editor/menu.rs` is a floating, input-grabbing, selectable
list with the `MenuPlacement::{Cursor, Editor}` axis already modeled (so the picker
reuses the same widget). It backs `btv.ui.select` end-to-end across TUI / GUI / web:
the server drains `take_ui_selects → open_menu`, the settle fixpoint fires
`menu_results → run_ui_select`, and a chosen *index* round-trips back to Lua while
the arbitrary item table stays Lua-side (`prelude/ui.lua`).

**This phase (the spec's Phase 2)** turns that list into the fuzzy finder:

- a **prompt line** that grabs typing into a query buffer (keystrokes never reach
  the document — the deepest difference between completion and the picker);
- a **Rust fuzzy matcher** (nucleo-class) that re-ranks **static** sources locally,
  with matched-character highlight spans — *no Lua per keystroke*;
- **dynamic-source forwarding** (live grep): each query edit re-runs the source,
  the matcher bypassed, under a **generation token** so a response for a query the
  user has typed past is dropped and the superseded job is cancelled;
- **incremental streaming**: results appear as they are found, not only on job exit.

Preview panes are **Phase 3** — this phase is list + prompt only.

### Two scope decisions (with the user, 2026-06-14)

1. **Streaming is in scope now.** The async spawn path is one-shot today:
   `bemtvi-server/src/host.rs` reads the child to completion and emits a single
   `LoopEvent::ProcessExit` carrying the full stdout; `runtime.lua`'s process
   callbacks are one-shot; there is **no `on_stdout` and no `btv.spawn`** (only the
   one-shot `vim.system`). Incremental streaming (`ProcessStdout` events + a
   non-one-shot `on_stdout` dispatch + the `btv.spawn` wrapper) is therefore net-new
   here, not a follow-up.
2. **Ship all three built-in sources**: `files` (static, `rg --files`),
   `live_grep` (dynamic, `rg --vimgrep`), `buffers` (in-memory, no spawn) — together
   they exercise the static + dynamic + in-memory paths.

### Strict layering (the rule that shapes every phase)

`bemtvi-core` stays **pure and synchronous**. Core owns only: the prompt edit, the
monotonic `generation` counter, the local fuzzy matcher (pure `&str` → ranked
indices), and the signal vec `picker_query_changes`. *Everything* async — spawn,
kill, `on_cancel`, Lua dispatch, and the stale-drop of live pushes against
`live_gen` — lives in `bemtvi-server` and the Lua wrapper. This mirrors how the
command oracle (pure, in `command.rs`) coexists with the async server loop.

## Core idea: fold the prompt + matcher into `Menu`, keep the item payload Lua-side

Extend the existing `Menu`, do **not** add a sibling `Picker` — the spec mandates
*one widget* and a second construct would triplicate the navigation handler, the
projection, and the float bookkeeping. The orthogonal-flags model (prompt optional,
preview optional) is exactly *adding capabilities to `Menu`*.

`Menu` (`menu.rs:30`) grows:

```
prompt:      Option<Prompt>   // None = select/completion (buffer is the query);
                              // Some = picker (the prompt grabs input)
all_items:   Vec<MenuItem>    // every streamed-in candidate: { label, key }
filtered:    Vec<usize>       // indices into all_items, ranked — the visible view
match_spans: Vec<Vec<Range>>  // matched-char byte spans per filtered row
dynamic:     bool             // per-source: match-locally vs forward-query
generation:  u64              // bumped on every query edit — the staleness token

struct Prompt { query: String, col: usize }   // single-line input-grab field
```

`MenuItem { label, key }` keeps the **opaque key** (the source item's id) — never
the arbitrary `{path,row,col,…}` table, which stays in the Lua wrapper's
per-generation `items[]` array and is handed to `confirm(item)` by key. This is the
exact `btv.ui.select` round-trip (`ui.lua:47–82`) generalized from "index" to "key".

The dynamic/static split is one branch on the input path: **static** → re-rank
locally; **dynamic** → bump `generation`, emit `(generation, query)` and let the
server re-run the source. Drain ordering in the fixpoint is what makes the token
correct (see Risks).

## Phase 1 — The Rust fuzzy matcher (`bemtvi-core`, pure)

- Add `nucleo-matcher` (matcher-only sub-crate: no async, no IO — honours core
  purity) to `[workspace.dependencies]` in the root `Cargo.toml` with an exact
  `=x.y.z`; pull into `bemtvi-core` via `nucleo-matcher.workspace = true`.
- New pure module `crates/bemtvi-core/src/fuzzy.rs`:
  `rank(query: &str, candidates: &[&str]) -> Vec<(usize, Vec<Range<usize>>)>` —
  ranked candidate indices + matched-char byte spans for highlighting. Pure in/out,
  lives next to its only consumer like the command oracle in `command.rs`.
- No behavioral change yet; unit-free per the no-unit-test rule — exercised through
  the picker tests in Phase 5.

## Phase 2 — Extend `Menu` into the unified widget (`bemtvi-core`, pure/sync)

- Add the fields above to `Menu`; reuse the char-boundary-aware single-line edit
  logic proven in `cmdline.rs` (`cmdline_insert` / `cmdline_backspace` /
  `cmdline_prev_boundary`, ~285–348) for `Prompt`.
- `handle_menu` (`menu.rs:71`): when `prompt` is `Some`, printable keys / backspace
  / Home/End/cursor-motion edit `prompt.query`; `<C-n>/<C-p>/arrows/<CR>/<Esc>`
  still drive the list. On each query edit — **static**: re-run `fuzzy::rank` over
  `all_items`, rebuild `filtered`/`match_spans`, clamp `cursor`; **dynamic**: bump
  `generation`, push `(generation, query.clone())` onto a new
  `Editor::picker_query_changes: Vec<(u64,String)>` (the dynamic analogue of
  `menu_results` / `prompt_results`).
- New pure `Editor::menu_push(items: Vec<MenuItem>, gen: u64)` — append candidates
  and re-rank (static) / append in stream order (dynamic); a push into a closed
  menu is a silent no-op (like `close_menu`, `menu.rs:57`).
- `<CR>` resolves `filtered[cursor]` → its `all_items` key, pushes `Some(key)` onto
  the existing `menu_results: Vec<Option<usize>>` (Phase-1 index → generic key).
- `MenuView` (`view.rs:95`) grows `query`, the filtered labels, and `match_spans`.
- `open_menu` gains the picker shape (prompt + dynamic + `Editor` placement);
  `btv.ui.select` keeps calling the promptless `Cursor` shape unchanged.

## Phase 3 — Streaming spawn primitive (`bemtvi-server` + `bemtvi-lua`)

- `host.rs`: read the child's stdout **incrementally**, emitting newline-delimited
  chunks as a new `LoopEvent::ProcessStdout { id, lines }` (the final `ProcessExit`
  stays). Add the variant in `evloop.rs`.
- `effects.rs`: on `ProcessStdout`, fire the job's `on_stdout` callback —
  **non-one-shot** (persists across chunks, dropped only on `ProcessExit`/kill).
  Needs a persistent-callback registry entry distinct from the one-shot `on_exit`
  path (`runtime.lua` ~49–61).
- `bemtvi-lua`: add `btv.spawn { cmd, args, cwd, on_stdout, on_exit }` over the
  existing `LoopOp::Spawn` (`ops.rs:276`); the handle's `:kill()` maps to
  `LoopOp::Kill` (`ops.rs:288`). The one-shot `vim.system` async `on_exit` path is
  untouched (regression-guard it).
- **wasm**: thread `on_stdout` through the same off-tick / WebTransport proc seam
  the edit-host already uses (Phase 6d proc leg); build `--no-default-features` to
  keep parity. Use `lua_int` for any index Value (wasm `mlua::Integer` is i32).

## Phase 4 — The `btv.picker` engine (Lua wrapper + server orchestration)

- **Lua** — new `crates/bemtvi-lua/src/prelude/picker.lua` (registered like
  `ui.lua`): `btv.picker.source { name, items=function(ctx,push,done), dynamic,
  confirm }` and `btv.picker.open(name)`. The wrapper keeps full item tables Lua-side
  in a per-generation `items[]`; `push(item)` queues only `{ label, key }` + the
  current `gen` over a new `PickerOp::Push`; `ctx.on_cancel(fn)` and `push` close
  over `gen`; confirm resolves `confirm(items[key])`.
- **Server** — `effects.rs` + `lib.rs`: a `pending_picker` (analogue of
  `pending_ui_select`, `lib.rs:531`) holding the `confirm` / `items` / `on_cancel`
  cb-ids, `dynamic`, and `live_gen`. In the settle fixpoint (`effects.rs:1250`),
  **in this order**:
  1. Drain `picker_query_changes` **first**: set `live_gen = gen`; run+clear the
     prior `on_cancel`; re-invoke the source's `items(ctx)` with the new query/gen.
  2. Drain `PickerOp::Push { gen, items }`: **drop the whole batch if
     `gen < live_gen`** (stale); else `editor.menu_push`.
  3. Drain `menu_results` (confirm): fire the picker's `confirm(item)`; closing the
     menu invalidates `pending_picker` and bumps `live_gen` past any in-flight job.
  Add `picker_query_changes` to the convergence break-condition and the
  `MAX_ROUNDS` reset block (`effects.rs:1284`, `1301`).
- **Built-in sources** (engine defaults + the runnable example): `files` =
  `btv.spawn rg --files` streaming pushes (static, local match); `live_grep` =
  `dynamic=true`, per-query `btv.spawn rg --vimgrep -- query` with
  `ctx.on_cancel(function() p:kill() end)`; `buffers` = in-memory from `btv.buf.*`,
  no spawn.
- **Live-query UX** (dynamic sources). To make live grep usable: a `debounce` (ms;
  global default 250 via `btv.picker.debounce`, overridable per source and per open)
  coalesces keystrokes into one run after a pause and a new keystroke cancels the
  in-flight job (`picker_cancel_inflight`); a `max_results`
  cap (default 100000, a runaway-source safety) reaps a query past it. To scale to
  100k+ candidates the widget **windows** its projection — `MenuView` carries only
  metadata + `total`, and the server fetches just the visible rows via
  `Editor::menu_rows`, rebasing `selected` into the window so the client is
  unchanged — and matches **incrementally** (`filtered: Option<Vec<usize>>`:
  passthrough materializes nothing; an active query ranks once and extends only the
  newly-streamed slice, never re-ranking per batch). Lua→server pushes are batched
  (~one crossing per 1000 items). Crucially, a query edit **does not clear
  the list** — core bumps `generation` but keeps the old results displayed; the
  server swaps them only when the new run's first result lands (`menu_push`, gated by
  a per-menu `items_gen`) or clears them when the run completes empty (`menu_finish`,
  signalled by the source's `done()` over `picker_finishes`). Lua keeps `p.items`
  append-only with absolute keys so a still-displayed older result stays confirmable
  during the in-flight window.

## Phase 5 — Redraw + clients + tests + example

- **Redraw** (`redraw.rs:653` `project_menu`): emit the prompt line, the filtered
  labels, and match spans; honour `MenuPlacement::Editor` centering (geometry branch
  stubbed at `redraw.rs:698`). Prompt at **top** in editor mode (spec open-question
  lean; no config knob this phase).
- **Fixed box size (not content-derived).** An `Editor`-placed picker is a *fixed*
  box, never content-hugging (that looks ragged). New `MenuExtent { Cells(u16),
  Frac(f32) }` (bemtvi-core) carried on the `Menu`/`MenuView`; the source/`open` set
  `width`/`height` as a cell count (`100`) or a CSS-style viewport fraction
  (`"80vw"`/`"60vh"`/`"50%"`), crossing the bridge as a raw string the server parses
  (`parse_menu_extent` in `effects.rs`) and resolves against the viewport in
  `project_menu`. Default ~80vw × 60vh. `btv.ui.select` stays content-anchored under
  the cursor. Clients render the **full** box height, padding empty rows past the
  item count.
- **`bemtvi-view`** (`view.rs:319` `MenuData`): decode `query` + per-row match spans.
- **Clients**: extend `render_menu` (TUI `render.rs`), `build_menu` (GUI
  `render.rs`), `renderMenu` (web `index.html`) to draw the prompt line and bold
  matched chars — each already renders the Phase-1 list box.
- **Tests** — new `crates/bemtvi-server/tests/picker.rs`, black-box per the spec
  testing section:
  - open a picker, type → query edits & re-ranks **without touching the document
    buffer**; `nvim_buf_get_lines` is unchanged;
  - match highlighting tracks the query (assert spans in the redraw view);
  - a **stale-generation** push is dropped; a **superseded dynamic query** kills its
    job and cancels (`on_cancel` ran);
  - `cursor` vs `editor` placement geometry;
  - `<CR>` fires `confirm` with the **right item** (verify via the resulting
    buffer/cursor after confirm); `<Esc>` closes with no action.
  Use `drain_to_latest_redraw`-style helpers (harness), never take-first.
- **Example**: `examples/ui-picker/` (config + sample tree) verified end-to-end;
  extend the headless-Chromium `verify-ui.mjs` to drive a picker open → type →
  confirm in the wasm build.

## Risks & notes

- **Push vs query-change race** — drain `picker_query_changes` (bump `live_gen`)
  **before** `PickerOp::Push`, so a late batch from the superseded job fails the
  `gen < live_gen` gate. Wrong order leaks results for a query the user typed past.
- **Non-one-shot `on_stdout`** — must persist across chunks (dropped only on
  exit/kill); the existing process callbacks are one-shot, so a naïve reuse shows
  only the first stdout chunk of a static picker.
- **Confirm racing a late push** — closing on `<CR>` must invalidate
  `pending_picker` and bump `live_gen`, so a killed `rg`'s trailing push is a no-op
  into a closed menu, not applied.
- **Core purity** — core never holds the `confirm`/`items`/`on_cancel` closures,
  never spawns, never compares against live pushes. It owns the counter, the prompt
  edit, the pure matcher, and the `picker_query_changes` signal — nothing else.
- **No silent stubs** — `btv.spawn` must fully implement streaming or fail loud; a
  source that "opens" but never streams is exactly the quietly-broken shape the
  project rule forbids.

## Verification

1. `cargo build --workspace`; `cargo test -p bemtvi-server --test picker`.
2. `cargo clippy --all-targets -- -D warnings` and `cargo fmt --all -- --check`.
3. `cargo run -p bemtvi -- .` → a `<leader>` map to `btv.picker.open("files")`: type
   to fuzzy-filter, `<CR>` opens the file; `live_grep` streams matches and updates
   per keystroke; `buffers` lists open buffers.
4. Build the wasm edit-host `--no-default-features` and drive the picker via the
   extended `verify-ui.mjs` in headless Chromium.
5. Regression: `vim.system` async `on_exit` still fires exactly once with the full
   output (the one-shot path the streaming work sits beside).
