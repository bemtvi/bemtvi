# `nx.statusline` — declarative segment registry (the lualine shape)

The native-plugin-API headline item #4 on the build order
([spec §2](../specs/2026-06-11-native-plugin-api.md),
[ADR 0002](../decisions/0002-native-plugin-system.md)). Distinct from the
already-landed `'statusline'` `%`-format engine
([that plan](2026-06-07-statusline.md)): this is the **server-owned, event-keyed
segment registry** a `lualine`-style config targets —

```lua
nx.statusline.setup {
  left  = { "mode", "git", "filename", "diagnostics" },
  right = { "lsp_progress", "filetype", "location" },
}

nx.statusline.segment {
  name   = "git",
  events = { "BufEnter", "DirChanged" },        -- standard autocmd events
  render = function(ctx)                          -- ctx = { buf, win, focused }
    local b = branch_cache[nx.bo[ctx.buf].cwd]
    return b and { { text = " " .. b, hl = "StatusGit" } } or nil
  end,
}

-- async data: recompute off the editor thread, then invalidate yourself
nx.spawn { cmd = "git", args = { "branch", "--show-current" },
  on_exit = function(res) branch_cache[cwd] = res.stdout; nx.statusline.invalidate("git") end }
```

## The model (rule 4: no frame-time Lua)

Segments resolve to **cells** — `{ { text = "…", hl = "Group" }, … }` (or `nil`
for nothing). Two kinds:

- **Built-in segments** (`mode`, `filename`, `filetype`, `location`,
  `modified`, `encoding`, `diagnostics`) resolve **natively in `nxvim-core`**
  from the per-window `StatuslineCtx` every frame. Pure, cheap, no Lua — so they
  stay live and per-window with zero caching.
- **Custom Lua segments** run their `render(ctx)` **only when invalidated**, not
  per frame. The server caches each one's last-published cells; redraw reads the
  cache. Invalidation is either (a) an explicit `nx.statusline.invalidate(name)`
  — what the async `git`/`lsp_progress` examples actually use — or (b) a declared
  `events` list of standard autocmd events nxvim already fires (`BufEnter`,
  `FileType`, …). The Lua side registers one autocmd per declared event that
  re-renders + re-publishes the segment, so **no new Rust event vocabulary is
  needed** — it reuses the existing `nx._fire`/autocmd dispatch.

This is faithful to the spec's "re-evaluated only on declared events, never per
frame" while keeping the hot, always-live fields (mode/cursor) Lua-free.

## Why this is cheap to land — reuse the whole projection + paint path

`nx.statusline.setup{}` produces the **same `Vec<StatusSegment>`** the existing
`render_statusline` pipeline already projects and every client already paints.
So **clients need zero changes** (`nxvim-tui`/`-gui`/`-web` + `nxvim-view`
already parse and paint the styled `status` / `global_status` arrays). The new
work is: compose segments → `StatusSegment`s (core), and a thin Lua surface +
two host bridges (server).

Composition reuses `statusline::layout`: build a `Vec<Piece>` of
`left cells… , Piece::Align , right cells…`, then call the existing `layout` for
`%=` distribution + `%<`-style truncation against the window width.

## Activation & precedence

`nx.statusline.setup{}` sets an active layout. When a layout is active it
**takes precedence** over the `'statusline'` format option for every window
(the common case — a user picks one or the other). `:set statusline?` still
reports the format string; the segment layout is the `nx`-native surface.
Per-window / active-vs-inactive differentiation and a `'statusline'`-as-fallback
toggle are deferred (see *Out of scope*).

## Phases

### Phase 1 — Core composition + built-in segments (pure, `nxvim-core`)

`crates/nxvim-core/src/statusline.rs`:

- New types: `SegmentCell { text: String, group: Option<String> }`, and a
  `SegmentLayout { left: Vec<String>, right: Vec<String> }` (segment *names*).
- `builtin_segment(name, ctx, mode_label) -> Option<Vec<SegmentCell>>` for the
  v1 built-ins: `mode`, `filename` (`%t`), `filepath` (`%f`), `filetype`,
  `location` (`line,col`), `modified`, `readonly`, `encoding`, `diagnostics`.
  Returns `None` for an unknown name (the server routes unknown names to the
  custom-segment cache instead — loud error only if found in neither place).
- `compose_segments(layout, ctx, mode_label, width, custom: &dyn Fn(&str) ->
  Option<Vec<SegmentCell>>) -> Vec<StatusSegment>`: resolve each name (built-in
  first, else `custom` lookup), build `Piece`s with an `Align` between
  left/right, run `layout(width)`.
- Extend `StatuslineCtx` with diagnostic counts (`diag_error`, `diag_warn`,
  `diag_info`, `diag_hint`) for the `diagnostics` built-in.

Verified end-to-end in Phase 3 (per the convention — no `#[test]` here; the
existing parser exception does not extend to composition).

### Phase 2 — Lua surface (`crates/nxvim-lua/src/prelude/statusline.lua`)

Mirror the `nx.complete` shape:

- `nx.statusline = nx.statusline or {}`; `_segments` registry; `_active` flag.
- `nx.statusline.segment(spec)` — validate `{ name, render, events? }`, store in
  `_segments`.
- `nx.statusline.setup(opts)` — validate `left`/`right` name lists; call
  `nx._statusline_setup(left, right)`; for each custom segment referenced,
  register an autocmd per declared event whose callback calls
  `nx.statusline.invalidate(name)`; do an initial render of every custom segment.
- `nx.statusline.invalidate(name)` / internal `nx._statusline_rerender(name)` —
  build `ctx = { buf, win, focused = true }` from the current editor state, run
  `render(ctx)` under `pcall`, normalize the returned cells, and call
  `nx._statusline_publish(name, texts, groups)`. A render error publishes a loud
  `E:<seg>` cell rather than failing silent.
- Add `statusline.lua` to the prelude load list (`runtime.rs` /
  `prelude/init.lua`, wherever `complete.lua` is listed).

### Phase 3 — Server wiring (`nxvim-server`)

- `Shared` effect queues: `statusline_setups: Vec<SegmentLayout>` and
  `statusline_publishes: Vec<(String, Vec<SegmentCell>)>`. Bridges
  `nx._statusline_setup` / `nx._statusline_publish` in `install.rs` (same
  `sh.borrow_mut().push(...)` pattern as `_complete_*`).
- Drain in `apply_lua_effects` (`effects.rs`): set
  `self.statusline_layout: Option<SegmentLayout>` and update
  `self.statusline_cache: HashMap<String, Vec<SegmentCell>>`.
- `redraw.rs` `render_statusline`: when `statusline_layout` is `Some`, thread
  diagnostic counts into the `StatuslineCtx` and call `compose_segments` (custom
  lookup = the cache), else the existing `%`-format path. Same
  highlight-resolve + `segment_value` projection as today → **clients unchanged**.
- Loud rule: a layout name that is neither a built-in nor in the cache renders a
  visible `E:<name>` segment (no silent blank).

### Phase 4 — Examples, tests, docs

- `examples/nx-statusline/` — a runnable `init.lua` (built-ins + one async
  `git`-style custom segment) + a sample file, verified end-to-end
  (per the example-config convention).
- `crates/nxvim-server/tests/editing/statusline.rs` (extend): (1) `setup{}` with
  built-ins → assert the projected `status` segments' text + styles in the latest
  redraw; (2) a custom segment + `invalidate` → assert the cell appears/updates;
  (3) an `events`-driven update (trigger `BufEnter`, assert re-render); (4) an
  unknown name → `E:<name>` segment.
- `crates/nxvim/tests/screen.rs` — end-to-end paint of an `nx.statusline` config.
- Docs: flip the roadmap bullet in `architecture.md` (statusline segments
  **landed**) and the build-order note in the native-plugin-API spec; update
  `known-approximations.md` with the deferred items below.

## Out of scope (v1)

- **Per-window active/inactive differentiation.** Built-ins render per-window
  natively; custom segments share one cache (the focused-window value). The
  `ctx.focused` field is passed but the cache is keyed by segment name only.
- A `width` field in the custom-segment `ctx` (segments rarely need it; the
  server doesn't yet mirror the per-window statusline width to Lua).
- `'statusline'`-format fallback *per region* / a setup option to mix the two.
- Mouse-click segment regions (shared with the deferred tabline `%@…@` work).
- A built-in `git` / `lsp_progress` segment — these are *plugin* segments by
  design (the spec ships them as custom-segment examples), not built-ins.
