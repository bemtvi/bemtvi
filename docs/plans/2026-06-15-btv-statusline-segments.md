# `btv.statusline` — declarative segment registry (the lualine shape)

The native-plugin-API headline item #4 on the build order
([spec §2](../specs/2026-06-11-native-plugin-api.md),
[ADR 0002](../decisions/0002-native-plugin-system.md)). Distinct from the
already-landed `'statusline'` `%`-format engine
([that plan](2026-06-07-statusline.md)): this is the **server-owned, event-keyed
segment registry** a `lualine`-style config targets —

```lua
btv.statusline.setup {
  left  = { "mode", "git", "filename", "diagnostics" },
  right = { "lsp_progress", "filetype", "location" },
}

btv.statusline.segment {
  name   = "git",
  events = { "BufEnter", "DirChanged" },        -- standard autocmd events
  render = function(ctx)                          -- ctx = { buf, win, focused }
    local b = branch_cache[btv.bo[ctx.buf].cwd]
    return b and { { text = " " .. b, hl = "StatusGit" } } or nil
  end,
}

-- async data: recompute off the editor thread, then invalidate yourself
btv.spawn { cmd = "git", args = { "branch", "--show-current" },
  on_exit = function(res) branch_cache[cwd] = res.stdout; btv.statusline.invalidate("git") end }
```

## The model (rule 4: no frame-time Lua)

Segments resolve to **cells** — `{ { text = "…", hl = "Group" }, … }` (or `nil`
for nothing). Two kinds:

- **Built-in segments** (`mode`, `filename`, `filetype`, `location`,
  `modified`, `encoding`, `diagnostics`) resolve **natively in `bemtvi-core`**
  from the per-window `StatuslineCtx` every frame. Pure, cheap, no Lua — so they
  stay live and per-window with zero caching.
- **Custom Lua segments** run their `render(ctx)` **only when invalidated**, not
  per frame. The server caches each one's last-published cells; redraw reads the
  cache. Invalidation is either (a) an explicit `btv.statusline.invalidate(name)`
  — what the async `git`/`lsp_progress` examples actually use — or (b) a declared
  `events` list of standard autocmd events bemtvi already fires (`BufEnter`,
  `FileType`, …). The Lua side registers one autocmd per declared event that
  re-renders + re-publishes the segment, so **no new Rust event vocabulary is
  needed** — it reuses the existing `btv._fire`/autocmd dispatch.

This is faithful to the spec's "re-evaluated only on declared events, never per
frame" while keeping the hot, always-live fields (mode/cursor) Lua-free.

## Why this is cheap to land — reuse the whole projection + paint path

`btv.statusline.setup{}` produces the **same `Vec<StatusSegment>`** the existing
`render_statusline` pipeline already projects and every client already paints.
So **clients need zero changes** (`bemtvi-tui`/`-gui`/`-web` + `bemtvi-view`
already parse and paint the styled `status` / `global_status` arrays). The new
work is: compose segments → `StatusSegment`s (core), and a thin Lua surface +
two host bridges (server).

Composition reuses `statusline::layout`: build a `Vec<Piece>` of
`left cells… , Piece::Align , right cells…`, then call the existing `layout` for
`%=` distribution + `%<`-style truncation against the window width.

## Activation & precedence

`btv.statusline.setup{}` sets an active layout. When a layout is active it
**takes precedence** over the `'statusline'` format option for every window
(the common case — a user picks one or the other). `:set statusline?` still
reports the format string; the segment layout is the `btv`-native surface.
Per-window / active-vs-inactive differentiation and a `'statusline'`-as-fallback
toggle are deferred (see *Out of scope*).

## Phases

### Phase 1 — Core composition + built-in segments (pure, `bemtvi-core`)

`crates/bemtvi-core/src/statusline.rs`:

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

### Phase 2 — Lua surface (`crates/bemtvi-lua/src/prelude/statusline.lua`)

Mirror the `btv.complete` shape:

- `btv.statusline = btv.statusline or {}`; `_segments` registry; `_active` flag.
- `btv.statusline.segment(spec)` — validate `{ name, render, events? }`, store in
  `_segments`.
- `btv.statusline.setup(opts)` — validate `left`/`right` name lists; call
  `btv._statusline_setup(left, right)`; for each custom segment referenced,
  register an autocmd per declared event whose callback calls
  `btv.statusline.invalidate(name)`; do an initial render of every custom segment.
- `btv.statusline.invalidate(name)` / internal `btv._statusline_rerender(name)` —
  build `ctx = { buf, win, focused = true }` from the current editor state, run
  `render(ctx)` under `pcall`, normalize the returned cells, and call
  `btv._statusline_publish(name, texts, groups)`. A render error publishes a loud
  `E:<seg>` cell rather than failing silent.
- Add `statusline.lua` to the prelude load list (`runtime.rs` /
  `prelude/init.lua`, wherever `complete.lua` is listed).

### Phase 3 — Server wiring (`bemtvi-server`)

- `Shared` effect queues: `statusline_setups: Vec<SegmentLayout>` and
  `statusline_publishes: Vec<(String, Vec<SegmentCell>)>`. Bridges
  `btv._statusline_setup` / `btv._statusline_publish` in `install.rs` (same
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

- `examples/btv-statusline/` — a runnable `init.lua` (built-ins + one async
  `git`-style custom segment) + a sample file, verified end-to-end
  (per the example-config convention).
- `crates/bemtvi-server/tests/editing/statusline.rs` (extend): (1) `setup{}` with
  built-ins → assert the projected `status` segments' text + styles in the latest
  redraw; (2) a custom segment + `invalidate` → assert the cell appears/updates;
  (3) an `events`-driven update (trigger `BufEnter`, assert re-render); (4) an
  unknown name → `E:<name>` segment.
- `crates/bemtvi/tests/screen.rs` — end-to-end paint of an `btv.statusline` config.
- Docs: flip the roadmap bullet in `architecture.md` (statusline segments
  **landed**) and the build-order note in the native-plugin-API spec; update
  `known-approximations.md` with the deferred items below.

## Out of scope (v1)

- ~~**Per-window active/inactive differentiation.**~~ **Done** (follow-up, 2026-06-16):
  custom segments are rendered once per window against that window's
  `{ buf, win, focused }` and cached by `(window, name)`. The server re-renders
  from `run_pending` (fresh window mirror) when a segment is invalidated or the
  window layout changes — split/close, focus move, or a window swapping its buffer
  — and prunes the cache for closed windows. So `ctx.focused`/`ctx.buf` are now
  correct in every window. (`EditHost::refresh_statusline_segments`;
  `btv.statusline._rerender` iterates `btv.win.list()`.)
- A `width` field in the custom-segment `ctx` (segments rarely need it; the
  server doesn't yet mirror the per-window statusline width to Lua).
- ~~`'statusline'`-format fallback *per region* / a setup option to mix the two.~~
  **Done** (follow-up, 2026-06-16): `btv.statusline.setup{win=…}` is a window-local
  layout overriding the global one; `setup{win=…, format=true}` opts a window back
  to the `%`-format under a global layout (the mix); `btv.statusline.reset(win)`
  drops the override. Server resolves per window in `EditHost::resolve_window_layout`
  (window override → global → `%`-format); window-local overrides are pruned when a
  window closes. The custom `'tabline'` is now also kept on the `%`-format path
  (never a segment layout).
- ~~Mouse-click segment regions (shared with the deferred tabline `%@…@` work).~~
  **Done** (follow-up, 2026-06-16): a segment spec (or an individual `render` cell)
  can carry `on_click = "v:lua.<fn>"`; a left-click on the cell fires it with
  `(minwid=0, clicks, button, modifiers)`, the same dispatch as the `%`-format
  `%@…%X` regions. The cell's handler rides the publish bridge
  (`StatuslinePublishReq.cells` = `(text, group, on_click)`);
  `compose_segments_with_clicks` wraps a clickable cell in a
  `Piece::ClickStart`/`ClickEnd` so `layout_with_clicks` tracks its column span, and
  `EditHost::statusline_click_at` resolves the clicked column for both surfaces.
  (`examples/btv-statusline/` makes the git segment clickable; tests in
  `bemtvi-server/tests/mouse.rs`.)
- A built-in `git` / `lsp_progress` segment — these are *plugin* segments by
  design (the spec ships them as custom-segment examples), not built-ins.
