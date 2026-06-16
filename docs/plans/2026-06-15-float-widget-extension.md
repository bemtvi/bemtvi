# Extending the content float — persistence, edge placement, segment highlights

**Status:** **proposed (2026-06-15).** Extends the list-less **content float**
(`nx.ui.float`; the LSP hover / signature-help surface —
[float-widget spec](../specs/2026-06-14-nx-ui-float-widget.md), "What stays out
of this widget") from a fire-and-forget transient into a surface rich enough to
back a **faithful native which-key plugin**, without disturbing the menu widget
or the shared `FloatConfig` placement layer.

## Why

A which-key-style popup (appears when you pause mid-key-sequence, lists the
available continuations, dismisses or refreshes as keys arrive) is exactly the
kind of feature that should be an `nx.*` Lua plugin (ADR 0002 — dogfood the
plugin API), and the observer primitives it needs already exist:

- `nx.on_key(fn, ns)` — fires **before dispatch** for every key
  (`crates/nxvim-server/src/input.rs:28`), so a plugin can rebuild the pending
  prefix itself.
- `nx.keymap.get(mode)` — enumerates mappings with `lhs`/`rhs`/`desc`.
- `nx.timer(fn, ms)` / `vim.defer_fn` — the `timeoutlen` delay.

This is the modern (which-key v3) **observer** architecture: no input-grab, no
blocking `getcharstr` read loop (which can't yield under PUC Lua anyway). The
only gap is the **render surface**. `nx.ui.float` today is too constrained on
three axes:

1. **Auto-dismiss.** The next key clears the float
   (`crates/nxvim-core/src/editor/mod.rs:1431`). A which-key popup must persist
   while you keep typing the sequence.
2. **Placement.** Only `relative = "cursor" | "editor"`
   (`MenuPlacement::{Cursor, Editor}`). which-key's signature look is a
   bottom-anchored bar.
3. **Plain text only.** The client renders `Span::raw`
   (`crates/nxvim-tui/src/render.rs:1934`); keys can't be colored distinctly
   from their descriptions.

All three live in the one pipeline:

```
nx.ui.float(ui.lua) → UiFloatReq(ops.rs) → Editor::open_content_float(float.rs)
  → ContentFloat(core) → ContentFloatView(view.rs)
  → project_content_float(redraw.rs) → ContentFloatData(nxvim-view)
  → render_content_float(nxvim-tui)
```

## Non-goals

- **Not** folding which-key into the menu widget — it has no list, no
  selection, no prompt; it stays a *content float* (the spec's own boundary).
- **Not** a general floating-window manager (multiple simultaneous floats,
  z-order, focus). One content float at a time remains the model; persistence
  just gives that single float an explicit lifetime instead of next-key death.
- **Not** an input-grab. The float never steals keys; the plugin observes via
  `nx.on_key` and the editor keeps dispatching normally.

## The extended `nx.ui.float` surface

```lua
local handle = nx.ui.float(contents, {
  relative   = "cursor" | "editor" | "bottom" | "top",  -- Phase 2 adds bottom/top
  border     = "rounded",        -- unchanged
  title      = "…",              -- unchanged
  persist    = true,             -- Phase 1: survive keystrokes; return a handle
  highlights = {                 -- Phase 3: per-segment colour
    { line = 0, col = 0, end_col = 3, hl = "WhichKey" },
    { line = 0, col = 5, end_col = 9, hl = "WhichKeyDesc" },
  },
})

-- Handle (returned only when persist = true; nil otherwise — back-compat):
handle:update(contents, opts)   -- replace lines / placement / highlights in place
handle:close()                  -- close now (idempotent; no-op if already closed)
handle:is_open()                -- whether this handle still owns the open float
```

A non-persistent call (`persist` falsy) keeps **exactly** today's behaviour:
fire-and-forget, dismissed on the next key, returns `nil`. Hover / signature
help / diagnostics are unaffected.

## Phasing — each phase independently shippable

### Phase 1 — Persistence + handle  *(unblocks a working which-key)*

- **Lua (`ui.lua`)** — when `opts.persist`, allocate a float id, send it, and
  return a handle metatable whose `:update`/`:close` queue further ops bound to
  that id; `:is_open()` reads a Lua-side flag cleared on close. Non-persistent
  path unchanged.
- **Bridge (`ops.rs` / `install.rs` / `runtime.rs`)** — `UiFloatReq` carries
  `id: u64` (0 = transient) and a `close: bool` op; add `nx._ui_float_close(id)`.
  Keep a single ordered `ui_floats` queue so open-then-close within one chunk
  preserves order.
- **Core (`editor/float.rs`, `editor/mod.rs`)** — `ContentFloat` gains
  `persistent: bool` + `id: u64`. `input()` clears only **non-persistent**
  floats. `close_content_float_id(id)` closes iff the open float's id matches (a
  stale handle's close no-ops). `open_content_float` takes the id + persistent
  flag.
- **Server (`effects.rs`)** — route open/replace vs. close by op.
- **Tests** (`tests/…`, black-box): a persistent float stays in the `redraw`
  across several keys; `:close()` removes it; `:update()` swaps its lines; a
  non-persistent float still dies on the next key (regression guard).

### Phase 2 — `bottom` / `top` placement  *(the which-key bar shape)*

- Introduce a **content-float-local** placement so the shared
  `MenuPlacement` (menu + completion) is untouched: add a
  `FloatPlacement { Cursor, Editor, Bottom, Top }` carried by `ContentFloat` /
  `ContentFloatView` (or extend the existing enum only if it proves clean).
- `project_content_float` (`redraw.rs`) computes geometry: `Bottom` anchors the
  box flush to the last text rows spanning (near-)full width; `Top` to row 0.
- **Tests**: `bottom` lands the float at the bottom rows; `top` at the top;
  width spans the text area.

### Phase 3 — Per-segment highlights  *(coloured keys)*

- `opts.highlights` = list of `{ line, col, end_col, hl }`. Thread spans through
  `ContentFloat` → `ContentFloatView` → the redraw map (resolving `hl` group
  names through the existing `StyleTable`, same mechanism as menu match
  highlighting) → `ContentFloatData` → `render_content_float`, which builds
  styled `Span`s instead of one `Span::raw`.
- **Tests**: the float's redraw projection carries the spans with the right
  style ids; out-of-range spans are clamped, not panics.

### Phase 4 — `examples/which-key/`  *(dogfood + end-to-end verify)*

- A native `nx.*` plugin driven by the **`KeyPending` event** (below), not a key
  observer: each event carries `{ mode, keys, continuations }`; the plugin
  debounces with `nx.debounce` (its show-delay policy) and renders the
  persistent, bottom-anchored, highlighted float; an empty payload (prefix
  resolved / cleared) closes it.
- Ship a runnable `examples/which-key/` config + sample, verified end-to-end
  (the example-config convention).

## The pending-key event (the oracle) — design decisions (2026-06-15)

which-key needs to know the *pending key context*, which the keymap matcher
already tracks (`Keymaps::pending` + the per-mode trie) but didn't expose. The
shape, after working it through:

- **Push, not pull.** A `KeyPending` event fires whenever the pending key-context
  **changes** (a prefix grows, or clears), carrying `{ mode, keys,
  continuations }` where each continuation is `{ key, desc, kind = map|group }`.
  No standalone `nx.keymap.pending()` query for now (YAGNI — add later if a
  statusline/showcmd wants on-demand reads).
- **Fires immediately; the plugin debounces.** The engine pushes on every
  pending-change with no built-in delay — show-delay is UI policy, kept in Lua
  via `nx.debounce`. This also dissolves the "close on timeout" problem: the
  idle-flush clearing the prefix just emits an empty `KeyPending`, so the popup
  closes with no re-polling.
- **`nx.on_key` is removed.** The per-keystroke Lua observer had zero consumers,
  wasn't in the ADR 0002 whitelist, and contradicted rule 4 (*no per-keystroke
  Lua*). Its only legitimate solo use (keystroke-cast overlays) is re-addable
  later as a narrow `KeyPressed` event if ever needed. which-key + showcmd-class
  needs are served better by `KeyPending` (engine-computed, no reconstruction).
- **Source order.** A — mapped prefixes (the matcher trie, `desc` stashed on the
  trie `Node`). B — built-in command grammar (operator-pending, `g`/`z`,
  registers, marks). C — active-widget key tables (the picker prompt's
  `handle_picker_key`; the `<C-r>` register case). `KeyPending` must fire from the
  union of these, not the matcher alone, to match real which-key coverage.

A general **`nx.utils.debounce(fn, ms)`** helper (trailing-edge, with `:cancel()`
/ `:flush()`) lands alongside this in the new **`nx.utils`** namespace — the home
for generally-useful helpers the `nx.*` surface exposes to plugin authors, not a
which-key private.

## Testing (black-box, per the no-unit-test rule)

Drive keys against the running server and assert on the `redraw` `float`
sub-map (lines, geometry, border, title, and — Phase 3 — highlight spans), on
the float's persistence across an `nvim_input` barrier, and on the `KeyPending`
notification payload (prefix + continuations). No `#[test]` units in the crates.
