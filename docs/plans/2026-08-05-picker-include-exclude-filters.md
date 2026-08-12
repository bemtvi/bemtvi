# Picker include / exclude filters

*2026-08-05*

## The problem

`files` and `live_grep` run unrestricted — `rg -uu` (`--no-ignore --hidden`) minus
`.git` — since a1f84bc0. That commit was right about the failure it fixed (a
`.gitignore`d file or a dotfile was simply *unfindable*, so the picker quietly
disagreed with you about which files exist) and wrong about the cost it imposed:
every `target/`, `node_modules/`, `.venv/` and build artifact now floods both
pickers. On a repo of any size the real files are buried, and on a big one the
100 000-item cap (`picker.lua:321`) can fill with junk before a source file is
even reached — the flood is a *correctness* problem, not only a comfort one.

Today's only escape is the documented "register your own `files` source to
narrow it" (`docs/features/picker.md:14-20`, worked in
`examples/telescope-parity/init.lua:41-56`). That is a config-time, all-or-nothing
answer to a question that is per-search: *this* time I want only `src/**`, *that*
time I want to include the vendored tree I normally hide.

## The shape

VSCode's search panel: alongside the query, a **files to include** and a **files
to exclude** glob box. Ported to bemtvi's picker, per the answers given:

- **Hidden by default.** The picker looks exactly as it does today. `<C-g>`
  reveals the two rows and cycles focus through query → include → exclude. When
  the rows are collapsed but patterns are active, a compact badge on the prompt
  row keeps you from filtering blindly.
- **Config defaults.** `btv.picker.setup{ exclude = { "**/target/**", … } }`
  pre-fills the boxes for every filterable picker.
- **Persisted across restarts**, via the existing `btv.shada.plugin` store.

```
┌─ Find Files ─────────────────────┐      ┌─ Find Files ─────────────────────┐
│ > handler               [+1 −2]  │      │ > handler                        │
├──────────────────────────────────┤ <C-g>│ include  src/**                  │
│ src/net/handler.rs               │ ───► │ exclude  **/target/**            │ ← focus
│ src/ui/handler.rs                │      ├──────────────────────────────────┤
└──────────────────────────────────┘      │ src/net/handler.rs               │
        collapsed, filters active         └──────────────────────────────────┘
```

## Two decisions that carry the design

### 1. Patterns are normalized once, then handed to *both* engines

The filter has to hold on every leg of both fallback chains: `rg` → `find` →
`btv.fs.walk`, and `rg` → `grep` → `btv.fs.grep`. Only `rg` takes globs (`-g`), so
the naive split — globs to `rg`, `btv.glob` for the other legs — puts two matchers
in play and the legs stop enumerating the same set, which is exactly the property
a1f84bc0 was careful to establish.

Filtering *only* in Lua would fix that but leaves `rg` walking `node_modules`
and burning the item cap on paths we are about to throw away. Pruning at the tool
is not a nicety here; it is what keeps the cap meaningful.

So: do both, and remove the divergence at its root. Both engines are globset —
`btv.glob` compiles through `globset` (`crates/bemtvi-core/src/glob.rs`) and so does
ripgrep — and they differ on exactly one thing that matters: a pattern with no
`/`. `rg -g '!*.lock'` matches `a/b/c.lock` (gitignore's basename rule);
`btv.glob.match("*.lock", "a/b/c.lock")` is `false`, because `*` stops at `/`.

One normalization pass, applied before either engine sees a pattern, closes it:

| typed        | normalized      |
| ------------ | --------------- |
| `*.lock`     | `**/*.lock`     | no `/` ⇒ match at any depth (the gitignore rule, made explicit)
| `target/`    | `target/**`     | trailing `/` ⇒ the directory's contents
| `node_modules` | `**/node_modules/**` \| `**/node_modules` | a bare name is both a dir and a file
| `src/**`     | `src/**`        | already anchored, untouched

Normalized patterns mean the same set to both engines, so `rg`'s pruning can only
remove what `btv.glob` would have removed anyway. `btv.glob` stays authoritative —
every leg's output, `rg`'s included, is tested by it in `push` — and `rg` becomes
a pure optimization that cannot change the answer. One semantics, documented once.

### 2. The filter lives in `push`, not in each source

`ctx.push` (`picker.lua:404-446`) is the single point every candidate crosses. A
`btv.glob.set` pair compiled once per run and tested there gives include/exclude to
**every** source that yields paths — shipped or user-registered — for free, rather
than asking each to re-implement it. Items with no `path` are passed through
untouched.

Sources opt in with `filter = true` in the spec (`files`, `live_grep`, `buffers`);
`<C-g>` in a `keymaps` or `marks` picker fails loud rather than presenting boxes
that would filter nothing.

## Phases

Each phase is a commit, reviewed before the next starts.

Phases 1 and 2 below were **written as one commit**, deliberately. Splitting them
would have landed a `<C-g>` that opens two boxes you can type into and that filter
nothing — a feature that looks finished and isn't, which is the exact shape the
no-silent-stubs rule exists to prevent. The two are kept as separate sections because
they are still separate concerns to review.

### Phase 1 — the multi-field prompt (core + wire + 3 clients)

`Menu` holds one `prompt: Option<Prompt>` (`menu.rs:378`) and eight call sites do
`menu.prompt.as_mut().unwrap()`. Keep `Prompt` (`menu.rs:277-337`) exactly as it is
— it is a good single-line editor and all three fields want it — and introduce the
set around it:

```rust
struct PromptSet {
    query: Prompt,
    include: Prompt,
    exclude: Prompt,
    focus: PromptField,   // Query | Include | Exclude
    expanded: bool,       // are the include/exclude rows drawn
    filterable: bool,     // did the source opt in
}
```

- `Menu::match_query` reads `prompt.query.query`; the `unwrap()` sites become
  `focused_mut()`, so typing edits whichever field has focus.
- Two new picker actions in `apply_picker_action` (`menu.rs:1404`):
  `toggle_filters` (expand + focus `include`; collapse restores focus to `query`)
  and `next_field`. Bound in `btv.picker.actions` (`picker.lua:37-58`) with a
  default `<C-g>`; `<Tab>` is already `toggle_select` and stays.
- Geometry: `chrome = prompt_rows * 2` (`menu.rs:1836`) becomes
  `prompt_rows + filter_rows + separator`. It is recomputed identically in
  `mouse.rs:1950` and in each client — all four must agree or the mouse hit-test
  drifts off the list.
- The badge (`[+1 −2]`) is composed **core-side** into one `filter_badge:
  Option<String>` on `MenuView`, so the clients right-align a string instead of
  three copies of the counting logic.
- Wire: `MenuView` (`core/view.rs:236`) → `redraw.rs:1951-1968` → `MenuData`
  (`bemtvi-view/src/view.rs:603`) → TUI `render.rs:2604`, GUI `render.rs:2706`,
  web `edithost/web/index.html`.

Tests: prompt editing and caret per field, collapse/expand geometry, the badge,
mouse chrome (a click with the rows revealed must still hit the row under the
pointer — four places recompute that budget), and the existing picker suite
unchanged.

### Phase 2 — the patterns reach the sources

- `on_query_changed` (`menu.rs:1644`) carries include/exclude alongside the query,
  and **bumps the generation for a static source too** when a pattern field
  changes — `files` must re-run `rg` with new `-g` args, where a query edit only
  re-ranks locally.
- `picker_query_changes` `(gen, query)` → plus the two pattern lists;
  `run_picker_run` (`bemtvi-lua/src/runtime.rs:2155`) and `btv._picker_run`
  (`picker.lua:344`) grow them, surfacing as `ctx.include` / `ctx.exclude`.
- `btv.picker._normalize(patterns)` implements the table above; `push` compiles the
  two `btv.glob.set`s once per run and filters on `item.path`.
- `files` / `live_grep` splice `-g <pat>` / `-g !<pat>` into their `rg` argv next
  to the existing `-g !.git`. The `find` / `grep` / `btv.fs` legs need no change —
  `push` covers them.
- Pattern edits are debounced on the same 250 ms path as a dynamic query — for a
  **static** source too. Its re-run is a full tree walk, so undebounced, typing
  `node_modules` into the exclude box would spawn a dozen `rg` scans of the repo, one
  per character. Lua decides this by comparing the incoming lines with the previous
  run's, so no "why did this run" flag has to cross the wire.

Tests: a temp tree with `src/a.rs`, `target/junk.rs`, `vendor/b.lock`; assert each
of include-only, exclude-only, and both, over `files` and `live_grep`; assert the
`find`/`btv.fs` legs filter identically — covered by testing BOTH a pure-Lua source
that spawns nothing (so only the sink can be filtering) and the shipped `rg`-backed
ones; assert a bare
`*.lock` catches `vendor/b.lock` (the normalization) and that removing the
normalization fails the test.

### Phase 3 — defaults, per-open seeding, persistence

**Opening a picker with the boxes pre-filled** is a first-class entry point, not
just a config fallback — a keymap or plugin states the scope it wants and the
picker opens already narrowed:

```lua
-- a "find in sources" map: same picker, pre-scoped
vim.keymap.set("n", "<leader>fs", function()
  btv.picker.open("files", { include = { "src/**", "crates/**" } })
end)

-- grep everything except the vendored trees, boxes already showing
btv.picker.open("live_grep", {
  exclude = { "vendor/", "**/*.min.js" },
  filters = "open",          -- reveal the rows instead of the collapsed badge
})
```

`include` / `exclude` take a list or a single string (the `btv.glob.any` idiom, so
a caller taking "a glob or a list of globs" from its own config need not branch),
and are normalized by the same pass as a typed pattern. They **seed** the boxes —
the user can still edit or clear them for that session; they are not a lock.
`filters` (`"open"` | `"collapsed"`, default `"collapsed"`) chooses whether the
rows start revealed, since a caller that pre-filters usually wants that visible.

- `btv.picker.setup{ include = {…}, exclude = {…}, history = N }` supplies the global
  defaults. Precedence, low → high: source spec → `setup` → the most recent line
  used → `btv.picker.open` opts. The per-open opts sit at the top so a programmatic
  picker gets exactly the scope it asked for and is never surprised by a stale box;
  a picker opened *without* pattern opts restores yours.

**Persistence is a history, not a last-value.** What survives is the list of lines
each box has held, most recent first, so `<C-Up>` / `<C-Down>` cycle them the way
the command line cycles commands — the pattern set you work with is small and
recurring (`target/`, `node_modules/`, `src/**`), and a single remembered value
would make every switch between them a re-type. Pre-filling from `history[1]` gives
the last-value behavior for free, so the simpler model is a strict subset.

- The recall lists are handed to the core **whole** at open (a couple of dozen short
  strings), so cycling is synchronous rather than a Lua round-trip per keypress.
  Browse state is the cmdline model: `idx = None` while the line is yours, a stashed
  `draft` to return to, and an edit ends the walk.
- The lines are recorded from the core's **capture at close**, not from the last
  source run — a dynamic source's re-run is debounced, so the run can lag the final
  keystroke by a pattern or two.
- Storage is `btv.shada.plugin("picker")` — already isolated, capped and riding the
  ordinary shada cadence in whichever store the session uses. This needs **no**
  native/web cfg-split work, which the CLAUDE.md persist convention would otherwise
  demand.
- The open bridge hit mlua's 16-argument ceiling, so the six filter values became
  **one table** (`{ on, include, exclude, open, include_history, exclude_history }`),
  the precedent `CompleteSetupArgs` sets. Anything further about the filters goes in
  that table rather than growing the tuple.
- `examples/picker-filters/` + `docs/features/picker.md` + the book.

Tests: precedence across all four layers; `open{ include = "src/**" }` as a string
and as a list; `filters = "open"` starting revealed; a seeded box still editable;
recall walking older/newer/back-to-draft; most-recent-first with no duplicates; a
recalled line actually re-running the source; per-box separation; `history = 0` and
`forget_history`; and a cross-restart round trip through a real redb store
(`tests/shada.rs`).

## Notes

- **`btv.picker.setup` and the "no parallel config table" non-goal.** The
  configurable-widget-keys plan (`2026-06-16`, *Non-goals*) rules out
  `btv.picker.setup{ mappings = … }` on the grounds that the keymap engine *is* the
  configuration surface for keys. That reasoning is specific to mappings and is
  not weakened here: glob defaults are plain data with no engine of their own, and
  `setup` never grows a `mappings` key.
- **`'wildignore'` is not a candidate.** It exists only as an accepted-but-unmodeled
  name in the `btv._o_store` catch-all (`state.lua:551`) — nothing reads it — and it
  covers neither an include side nor per-search editing.
- **`.git` stays hardcoded.** It is excluded because its object store is not
  source, which is not a user preference; it remains outside the box so an empty
  exclude box never means "show me loose objects".
