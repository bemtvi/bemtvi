# Completion documentation — completion plan

> **Status: complete (all three phases ✅).** The menu now carries per-item
> `documentation`, fetches it lazily for the selected item via
> `completionItem/resolve`, and renders it in a preview box beside the popup. The
> sections below are kept as the design record; each phase's "Done when" notes
> what landed and which tests pin it.

## Why this document exists

nxvim **already has a working insert-mode completion menu.** `<C-Space>` in
insert mode fires a real `textDocument/completion`
(`crates/nxvim-server/src/lib.rs` binds it to `LspReqKind::Completion`); the
reply opens a server-owned `CompletionMenu` (`crates/nxvim-server/src/lsp.rs`)
that filters/ranks in place as you type, navigates with `<C-n>`/`<C-p>`, and
accepts the selected item — honoring its `textEdit` range and
`additionalTextEdits` (auto-imports) as one undo step. The e2e suite drives it
against real servers. Each row shows the item's **label** plus its right-aligned
**`detail`** (the type/signature) — see `pmenu_value` / `pmenu_item_width`.

The piece this plan added was the per-item **documentation** — the prose/markdown
block a server attaches to a candidate (function docs, parameter help). When the
plan was written it was missing for two concrete reasons, both now resolved:

1. **`documentation` was dropped at distillation.** `completion_item()`
   (`crates/nxvim-lsp/src/manager.rs`) reduces the protocol `CompletionItem` to
   `CompletionItemData`; it previously kept `label / kind / detail / filter_text /
   sort_text / insert_text / text_edit / additional_text_edits` — but **not**
   `documentation`, and not the opaque `data` blob a server needs to resolve it.
   Phase 1 added both (`documentation` + `resolve_data`).
2. **No `completionItem/resolve`, and no completion client capability at all.**
   nxvim advertised no `completion` block under `text_document` in
   `client_capabilities()`, so it never declared `documentationFormat` or
   `resolveSupport`. This matters because most servers — **notably
   rust_analyzer** — send completion lists *without* documentation (often
   without full `detail` either) and expect the client to fetch it lazily per
   selected item via `completionItem/resolve`. Phase 1 advertised the capability;
   Phase 2 wired the round-trip.

So "show the docs" is three pieces: carry `documentation` end to end (and keep
enough of the original item to resolve), add the `completionItem/resolve`
round-trip for the selected item, and render the result in a preview surface
beside the popup.

This follows the same fail-loud, no-silent-stubs rule as
[`docs/lsp-completion-plan.md`](lsp-completion-plan.md): a server that can't
resolve, or a malformed reply, is logged — never faked into an empty doc that
looks like "no documentation."

> **Not in scope: `vim.lsp.omnifunc`.** That stub
> (`crates/nxvim-lua/src/prelude.lua`) raises `not implemented`, but it is the
> *legacy `i_CTRL-X_CTRL-O` Vimscript omni-completion* entry point — a separate,
> Vimscript-era path. The native menu above does **not** route through it. Do
> not read the omnifunc raise as "completion is missing"; it isn't.

---

## Status legend

- ✅ done   🚧 in progress   ⬜ not started

---

## Phase 1 — Carry `documentation` + `data` end to end; advertise the capability ✅

**Goal.** Make a completion item's `documentation` (and `data`) survive
distillation and reach the menu, and advertise the client capability so servers
that *can* send docs do.

**Why.** Prerequisite for everything else: the preview (Phase 3) has nothing to
show and resolve (Phase 2) has nothing to merge into until the field exists on
`CompletionItemData` and is projected. The `data` round-trip is what makes
resolve possible at all (rust_analyzer rejects a resolve whose `data` it didn't
issue). Advertising the capability is also what some servers gate doc-sending
on.

**Scope.** `crates/nxvim-lsp/src/manager.rs` (`CompletionItemData`,
`completion_item()`, `client_capabilities()`), `crates/nxvim-server/src/lsp.rs`
(`CompletionMenu` carries the field forward — already `raw`-backed).

**Approach.**
- Add `documentation: Option<String>` to `CompletionItemData`, normalized from
  the protocol `Documentation` (a plain string or a `MarkupContent`) to plain
  display lines — **reuse the hover markup distiller** (`hover_reply`'s
  `MarkupContent`/`MarkedString` → lines logic in `manager.rs`); factor the
  shared bit out rather than duplicating it.
- Add `resolve_data: Option<serde_json::Value>` holding the original item's
  `data` (the minimum rust_analyzer needs), or the whole serialized item if
  simpler — mirrors `CodeActionData.resolve` round-tripping the original
  `CodeAction`.
- In `client_capabilities()`, add a `completion: Some(CompletionClientCapabilities
  { completion_item: Some(CompletionItemCapability { documentation_format:
  Some(vec![Markdown, PlainText]), resolve_support: Some(... ["documentation",
  "detail"]), data_support? ... }), .. })` block, alongside the existing
  `code_action` `resolve_support`/`data_support` pattern.

**Tests.** In `crates/nxvim/tests/lsp.rs` (scripted mock): a `completion` result
whose item carries inline `documentation` surfaces on the menu item — assert it
rides the reply into `CompletionMenu.raw` (a new redraw assertion or a Lua-side
probe, consistent with how the menu is otherwise observed).

**Done when.** ✅ `CompletionItemData` carries `documentation` (markup →
lines, via the shared `markup_lines` distiller hover now also uses) and the
original item as `resolve_data` (whole serialized item, for the Phase 2 resolve
round-trip); `client_capabilities()` advertises `completion.completionItem` with
`documentationFormat: [markdown, plaintext]` + `resolveSupport: [documentation,
detail]`; an eagerly-documented item's docs ride into `CompletionMenu.raw` by
construction. Verified by `completion_capability_advertises_documentation_and_resolve`
(asserts the advertised capability on the recorded `initialize`) and
`a_documented_completion_item_opens_the_menu` (a `MarkupContent`-documented,
`data`-bearing item distills cleanly and reaches the menu) in
`crates/nxvim/tests/lsp.rs`.

**Depends on.** Nothing (the menu already exists).

---

## Phase 2 — `completionItem/resolve` for the selected item ✅

**Goal.** When the selection settles on an item missing `documentation` (and/or
`detail`), fetch it via `completionItem/resolve` and merge the result back into
the menu in place.

**Why.** This is the phase that actually delivers for **rust_analyzer** and most
real servers — they send no docs in the list; resolve is the only way to get
them. (The raw wire method already exists in the `dyn_requests!` table in
`manager.rs` — but only reachable via the generic Lua `client:request`, not from
the native menu.)

**Scope.** `crates/nxvim-lsp/src/manager.rs` (a typed `LspRequest::ResolveCompletion`
/ `LspReply::ResolvedCompletion`, modeled on `ResolveCodeAction` /
`ResolvedCodeAction`), `crates/nxvim-server/src/lsp.rs` (`LspReqKind::CompletionResolve`,
issue on selection-settle, route reply → merge into the open menu).

**Approach.**
- Add `LspReqKind::CompletionResolve` (next int after `ResolveCodeAction`) and
  the `LspRequest`/`LspReply` pair carrying the original item (with its `data`)
  out and the resolved `documentation`/`detail` back.
- Fire it from the selection-move path (`lsp_menu_move`) **debounced** — only
  for the currently-selected item, only when it still lacks
  `documentation`/`detail`, and skip if a resolve for that item is already in
  flight (a small per-item "resolved" flag on the menu entry). A late reply for
  a no-longer-selected (or closed) menu is dropped, like the existing
  generation-gated staleness in `register_lsp_request`.
- On reply, merge `documentation`/`detail` into the matching `raw` entry and
  mark it resolved; `lsp_dirty = true` so the preview repaints.

**Tests.** In `crates/nxvim/tests/lsp.rs`: extend the scripted mock with a
`completion_resolve` reply; assert that selecting an item with no inline docs
issues `completionItem/resolve` with the original item's `data`, and the
resolved documentation lands on the menu item off-tick. A resolve failure logs
and leaves the item docless (loud, not a fake empty doc).

**Done when.** ✅ Selecting a docless item issues `completionItem/resolve`
(round-tripping the original item incl. its `data`), the resolved
`documentation`/`detail` merge into the open menu in place (the resolved
`detail` is already visible in the pmenu projection; `documentation` lands in
`raw` for the Phase 3 preview), stale/closed-menu replies are dropped (the
request-generation gate + a per-item `resolved` flag / single-slot `resolving`
target on the menu), and a failed/malformed resolve is logged and leaves the
item docless. Typed `LspRequest::ResolveCompletion` / `LspReply::ResolvedCompletion`
in `manager.rs`; `LspReqKind::CompletionResolve` fired from `lsp_menu_move` via
`maybe_resolve_selected`, merged by `merge_resolved_completion`, in
`nxvim-server/src/lsp.rs`. Verified by
`selecting_a_docless_item_resolves_it_and_merges_the_result` and
`a_completion_resolve_failure_leaves_the_item_docless` in `crates/nxvim/tests/lsp.rs`
(the mock gained a `completion_resolve` script field).

**Depends on.** Phase 1 (the field + `data` + advertised capability).

---

## Phase 3 — The documentation preview surface ✅

**Goal.** Render the selected item's `documentation` in a preview box beside the
completion popup (vim's "preview window"/`completeopt=popup` shape).

**Why.** The visible payoff — Phases 1–2 make the data exist; this shows it.

**Scope.** `crates/nxvim-server/src/lsp.rs` (`pmenu_value` projects the selected
item's doc lines), the `pmenu` redraw key (a new `doc` field), and the client's
pmenu overlay renderer (a side box; falls back to below/above like the popup's
own placement logic).

**Approach.**
- Extend the `pmenu` map projected by `pmenu_value` with a `doc` entry: the
  selected item's `documentation` lines (empty/absent ⇒ no preview box), plus
  whatever geometry the client needs to place it (reuse the existing
  below/above/clamp placement reasoning).
- Render it client-side next to the popup; prefer the side with room, mirroring
  `pmenu_value`'s existing fit logic.

**Tests.** In `crates/nxvim-server/tests/editing.rs` (or `lsp.rs` for the
mock-fed case): after opening the menu and selecting a documented item, the
`pmenu` redraw value carries the doc lines — asserted on the redraw view
(take-latest helper), the project's standard surface for UI state.

**Done when.** ✅ The selected item's documentation renders in a preview box
beside the popup; navigating updates it; an item with no docs (or no selection)
shows no box. `pmenu_value` projects the selected item's `documentation` as a
`doc` lines array on the `pmenu` redraw key; the client's `render_pmenu_doc`
floats a second bordered box to the right of the popup (falling back to its
left), wrapping the lines and clipping to the text area. Verified by
`selecting_a_documented_item_shows_a_doc_preview` in `crates/nxvim/tests/lsp.rs`
(asserts both the `doc` lines on the redraw and the painted preview text).

**Depends on.** Phases 1 and 2 (the data); the existing pmenu surface.

---

## Known approximations to expect

- **Single preview, single window.** Like the popup itself, the preview is one
  box in the single-window model — no separate preview-window handle, no
  `completeopt` matrix. (Inherits the single-window root cause in
  [`docs/known-approximations.md`](known-approximations.md).)
- **Markdown is rendered as plain lines**, not styled — same as hover today
  (the markup distiller yields lines, not highlights).
- **Resolve is best-effort per selection.** A very fast scroll through the menu
  may outrun resolve replies; the debounce + staleness drop means some items
  show docs a tick after selection, never the wrong item's docs.

## Suggested order

`1 → 2 → 3`. Phase 1 is pure plumbing + a capability line. Phase 2 is the one
that lights up rust_analyzer (and is where the real value is). Phase 3 is the
render. After Phase 2 the data is observable in tests even before the preview
box exists — so Phases 1–2 can land and be verified independently of the client
UI work in Phase 3.
