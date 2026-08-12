# `winhighlight` — per-window highlight remap (docks first, all windows under the hood)

Status: **COMPLETE** — all 6 phases done (2026-06-20). Phases 1–3 (parse/store,
`WindowView` plumbing, per-window content remap), phase 4 (per-window chrome wire
channel), phase 5 across **all three clients** — TUI (paint-tested), GUI
(code-complete; pixels not agent-verifiable), web (Playwright-verified in headless
Chromium on the real wasm edit-host) — and phase 6 (runnable
`examples/dock-winhighlight/`, the `btv.wo.winhighlight` window-scope surface, and
doc cleanup) are committed. `winhighlight` works end-to-end: `btv.dock.opt(side)` or
`btv.wo`/`vim.wo` remaps per window; a dock paints like a VSCode sidebar.

## Goal

Make `btv.dock.opt(side).winhighlight = 'Normal:NormalSB,SignColumn:NormalSB,EndOfBuffer:Hidden'`
actually recolor that dock, replacing the current fail-loud stub
(`crates/bemtvi-core/src/editor/dock.rs:524`).

`winhighlight` is vim's per-window highlight **remap table**: a comma-separated
list of `from:to` pairs. While rendering a window that has it set, every group
that *would* resolve to `from` resolves to `to` instead, at render-resolution
time. It is a **one-level** alias (the target's own `link` chain is followed, but
remaps don't chain into each other), and **unknown groups on either side are
accepted silently** (vim-faithful — verified against `vendor/neovim`
`parse_winhl_opt`).

The setter surface is dock-only today, but the resolution machinery is identical
for any window. We build the mechanism **per-window** (`WindowOptions`) and let
docks be a thin shorthand over it, so `:set winhighlight` / `btv.wo` later is free.

## Why this is more than a setter

There is exactly **one global `Normal` background** today, resolved once in
`chrome_styles()` (`crates/bemtvi-server/src/redraw.rs:925`) and painted
client-side from a single `view.normal` style (`crates/bemtvi-tui/src/render.rs:67-73,903-910`).
Nothing per-window crosses the wire. `winhighlight` forces highlight resolution to
become **window-aware** at every site that resolves a group name *for a window*:

- the window background (`Normal` / `NormalFloat`),
- gutter groups (`LineNr`, `CursorLineNr`, `SignColumn`, `FoldColumn`),
- `CursorLine`, `EndOfBuffer`,
- treesitter captures (`highlights_for`, `crates/bemtvi-server/src/treesitter.rs:273`),
- extmark / diagnostic / decor overlays (`overlay_highlights_for`).

## Approach: remap late, in the server projection layer (decided)

Keep `bemtvi-core/src/highlight.rs` **pure and window-agnostic** — `resolve` /
`resolve_capture` stay as-is. The remap is a small per-window table carried on
`WindowView` and applied in the server's per-window projection via a
`resolve_remapped(group, &winhl)` helper that rewrites the name (one level) before
calling the existing `highlights.resolve(...)`.

Rejected alternatives:
- *Remap inside the registry* (vim's hidden-namespace approach): would thread
  window context into core's pure highlight layer — against `bemtvi-core` staying
  pure, and bemtvi's namespaces are an extmark concept, not a window one.
- *Dock-only chrome map* (`dock_left_normal` keys): blocks per-window
  `winhighlight` later and special-cases docks the layer-swap model deliberately
  avoids.

The remap is keyed by window, so a focused dock (live on `Editor::windows` via the
layer-swap model) and a parked one resolve identically — the override lives at the
projection layer, not the editing layer.

## Phase 1 — Parse + store (no rendering change yet)

Add `winhighlight: String` to `WindowOptions` and to `DockOptions`
(`crates/bemtvi-core/src/options.rs:584`). Replace the fail-loud arm at
`dock.rs:524` with a store into `dock_options[idx].winhighlight`, then
`relayout()` (mirrors the `title` path).

Add a parser: `"Normal:NormalSB,SignColumn:NormalSB"` → `Vec<(String, String)>`
(or a tiny `WinHl` newtype). Empty string clears. Malformed pairs (no colon) are
dropped per vim — but `echo` a one-line warning so it isn't a *silent* drop
(no-silent-stubs policy applies to our own parsing, even though vim accepts
unknown *group names*).

**Verification:** a Lua test that sets `winhighlight`, reads it back through a new
`btv._dock_opts`-style accessor, and confirms the parsed pairs. No redraw assertion
yet. The existing `dock_winhighlight_is_reported_not_silently_ignored` test
(`tests/dock.rs:913`) is **deleted/inverted here** — it asserts the old stub.

## Phase 2 — Carry the remap onto `WindowView`

Add the parsed remap to `WindowView` (`crates/bemtvi-core/src/view.rs`), populated
in `window_view()` from the window's `WindowOptions.winhighlight`, falling back to
the dock's `DockOptions.winhighlight` for windows in a dock region (`region` is
already computed there). Same plumbing shape as the existing `region` field.

**Verification:** pure-core; no observable output change. Existing view/redraw
tests stay green.

## Content vs. chrome (discovered while scoping Phase 3)

The highlight wire has **two resolution layers**, and `winhighlight` splits cleanly
across them:

- **Content** — treesitter spans, extmark overlays, diagnostics, virt_text, inlay
  hints — is resolved **per window already**, inside `highlights_for` /
  `overlay_highlights_for` / `virt_text_for` / `diagnostics_*` / `inlay_hints_for`
  (each runs per window in `window_value`, `redraw.rs:322`). Remapping these needs
  **no new wire channel** — just thread the window's `WinHl` into those helpers.
- **Chrome** — `Normal`, `LineNr`, `CursorLineNr`, `CursorLine`, `EndOfBuffer`,
  `NormalFloat`, `Visual`, `StatusLine`, … — is resolved **once globally** in
  `chrome_styles` (`redraw.rs:933`) into a single frame map every window shares.
  Remapping these (the sidebar look: `Normal:NormalSB`, dimmed `EndOfBuffer`) needs
  a **new per-window chrome channel** on the wire.

So Phase 3 is content-only (cheap, no protocol change); Phase 4 is the chrome
channel (the keystone, and the actual reason this was deferred).

## Phase 3 — Per-window **content** remap (no wire change)

Add `resolve_winhl(&self, &WinHl, group) -> Option<Style>` and
`resolve_capture_winhl(&self, &WinHl, group)` on `EditHost`: substitute
`winhl.remap(group)` (one level), then call the existing
`highlights.resolve` / `resolve_capture`. Thread a `&WinHl` parameter into every
per-window content resolver and route its `resolve`/`resolve_capture` calls through
the helpers: `highlights_for` (treesitter, `treesitter.rs`), `overlay_highlights_for`
+ `virt_text_for` (`extmarks.rs`), `diagnostics_for` / `diagnostics_virt_text_for` /
`diagnostics_signs_for` (`lsp/diagnostics.rs`), `inlay_hints_for` (`lsp/inlay.rs`),
and the scroll-band projection. The window's `WinHl` is parsed from `win.winhl`
(carried in Phase 2).

Note: a `Normal:NormalSB`-style config remaps **chrome**, so it has *no* visible
content effect — Phase 3 is observable via a content group remap (an extmark
`hl_group` or a treesitter capture). The faithful payoff (sidebar background) lands
in Phase 4.

**Verification:** behavioral test — set an extmark with `hl_group = 'Foo'`, define
`Foo`/`Bar` with distinct colors, set the dock's `winhighlight = 'Foo:Bar'`, and
assert the dock window's `highlights` array carries `Bar`'s style while a main-area
window with the same extmark carries `Foo`'s.

## Phase 4 — Per-window **chrome** channel on the wire (the keystone)

Today every window paints chrome from the single global `chrome_styles` map (and
`view.normal`). Give each window its own resolved chrome where its `WinHl` renames
a chrome group: resolve the CHROME groups per window through `resolve_winhl`,
interned into the existing per-frame `StyleTable`, and attach the overrides to the
window's wire map (only the groups the remap actually touches, so an un-remapped
window adds nothing). The global map stays as the fallback.

This is the bulk of the new code and the reason `winhighlight` was deferred.

**Verification:** set `winhighlight = 'Normal:NormalSB'`, define `NormalSB` with a
distinct bg, assert the dock window's resolved `normal`/background style differs
from the main area's and resolves to `NormalSB`'s color.

## Phase 5 — Client parity (TUI, GUI, web)

The per-window background only renders once each client prefers the window's
`normal` over the global one:

- **TUI** — `window_bg()` (`crates/bemtvi-tui/src/render.rs:67`) takes the
  per-window style, falling back to `view.normal`. Straightforward.
- **GUI** (wgpu) — same per-window background wiring or it diverges from TUI.
- **web** (wasm `EditHost`) — needs explicit attention: highlights were
  native-gated and web required a separate overlay path
  (`memory/web-decor-extmark-highlights-overlay.md`); the per-window background
  must be wired through the web renderer too, not assumed for free.

**Verification:** TUI is agent-testable via redraw assertions (Phase 4 already
covers the wire). GUI is not screencapturable from the agent shell
(`memory/gui-window-not-screencapturable-from-agent.md`) — confirm the pipeline
runs clean and ask the user to eyeball pixels. Web is Playwright-driveable via the
`window.__bemtvi` hook.

## Phase 6 — Example config + docs

Ship a runnable `examples/dock-winhighlight/` (a left dock styled like a VSCode
sidebar: `Normal:NormalSB`, dimmed `EndOfBuffer`), per the example-config
convention. Update the `btv.dock` doc comment block in
`crates/bemtvi-lua/src/prelude/btv.lua` and drop the "not implemented" language in
`dock.rs` / `ops.rs` / `install.rs`.

## Gotchas

- **Don't chain remaps.** `winhighlight=Normal:A` plus a global `A→B` link uses
  `B` (target's link is followed); but `winhighlight=Normal:A,A:B` does **not**
  make `Normal` resolve `B`. The `resolve_remapped` substitution is strictly one
  level before handing off to the registry's own link-following.
- **Unknown groups stay silent at the group level** (vim-faithful) — only
  *syntactically* malformed pairs warn. An unknown `to` simply resolves to nothing
  and falls back, an unknown `from` never matches.
- **Layer-swap means key by window, not by `Editor::windows` slot** — a parked
  dock must resolve the same as when focused, so the remap rides `WindowView`
  (projection), never live editor window state.
- **Per-frame `StyleTable` interning** already dedupes styles to small ids — the
  per-window `normal` is just another interned style, no new palette machinery.
