# Plan: route LSP doc surfaces through real float windows (mouse + scroll for free)

Status: all three phases implemented (2026-06-18)

## Motivation

The user wants **mouse support and scrolling** for the K-hover popup and the
autocompletion docs sidebar. Today these are list-less *content floats*
(`Editor::content_float`, projected by `redraw.rs::project_content_float`): they
carry pre-styled `VirtChunk` runs, live outside the window tree, and have no
scroll state. Adding a bespoke scroll offset to a non-window is exactly the
friction that motivated this work.

The cleaner architecture — and the neovim model — is to back read-only,
potentially-long doc content with a **real, non-focusable, buffer-backed float
window**. Float windows already inherit everything we want:

- mouse **hit-test** and **wheel scroll** work on any float, focused or not
  (`mouse.rs::window_at_in` walks `tree.floats`; `mouse.rs::mouse_wheel` scrolls
  the window under the pointer, focused or not);
- keyboard scroll / cursor / rendering / border / title come from the normal
  window path (`view.rs::window_view`, `redraw.rs` window projection).

So the win is *deleting* per-overlay scroll special-casing rather than growing
it.

## Findings that revised the original 3-step pitch

Two things discovered while mapping the code changed the plan from what was
floated in chat:

1. **There is no `stylize_markdown`.** Hover / signature / completion-docs all
   strip markdown to **plain text** today (`bemtvi-lsp/src/convert.rs::markup_lines`).
   So markdown *styling* is a net-new feature, not part of getting mouse+scroll.
   It is **out of scope** here. (Possible cheap follow-up: set
   `filetype=markdown` on the scratch buffer so tree-sitter highlights it for
   free — the neovim trick — but that is a separate change.)

2. **The completion-docs sidebar is not a `btv.ui.float`.** It is server-projected
   in `redraw.rs::project_complete_docs` as a *sibling of the bespoke completion
   menu*, and that menu **already has its own bespoke mouse + wheel handling**
   (`mouse.rs::mouse_complete_wheel`, `mouse_menu_wheel`; geometry from
   `menu.rs::menu_geom`). Converting *only* the docs to a lone float window —
   inside an otherwise fully-bespoke widget whose content is server-rendered —
   would be inconsistent and would still need server-side content projection.
   The consistent fix is a **bespoke scroll offset + a `MenuHit::Docs` region**,
   matching how the menu list already scrolls.

Net: the "real float window" treatment is right for the **standalone**
cursor-relative surfaces (K hover, signature help). Completion docs is better
served by matching its parent widget.

## Phases (commit + pause between each)

### Phase 1 — core doc-float window infra + K hover

New core capability, modelled on `panel.rs::open_named_panel`:

- A **reusable named scratch buffer** per doc surface (so re-opening replaces
  content, no buffer leak). Fill via `buffers.rs::load_str_into`, flip
  `modifiable = false`. Backed by a small registry (`doc_float_buffers`), kept
  distinct from `panel_buffers` so it doesn't inherit panel behavior.
- `Editor::open_doc_float(name, lines, placement)`:
  - size to content — width = max `display_width` over lines (capped, ~80),
    height = line count (capped, ~20) — passed as `Extent::Cells`;
  - open a `FloatConfig { relative: Cursor, focusable: false, border: Rounded,
    .. }` via `open_float_window(buf, cfg, /*enter=*/false)` so focus stays in
    the editing window;
  - close any previous window for that surface first (reopen in place);
  - register the new window id as a **transient doc float**.
- **Transient close-on-key:** in `mod.rs::input`, alongside the existing
  `content_float` dismissal, close any registered transient doc-float *windows*
  on the next key. Mouse wheel does **not** go through `input()`, so scrolling
  keeps the popup open (the whole point).
- Rewire `lsp/request.rs::show_hover` to call `open_doc_float` instead of
  `open_content_float`.

Placement note: anchor `NW` with `row = 1` (drop below the cursor line);
`place_float` clamps the box fully on-screen, so a hover near the bottom is
pulled up and stays visible. Strict above/below *flip* (as the old content-float
projection did) is a possible refinement, noted but not required for v1.

Test (black-box, redraw): drive an LSP hover, assert a `floating: true` window
appears carrying the hover lines; assert a wheel event scrolls it; assert a key
closes it.

### Phase 2 — signature help as float window

Rewire `lsp/request.rs::show_signature_help` to the same `open_doc_float` path
(its own surface name / scratch buffer). Own commit.

### Phase 3 — completion docs scroll + mouse (bespoke, REVISED — confirmed)

The user confirmed the bespoke route at the phase-2→3 boundary. Docs stays
server-projected beside the menu, made interactive like the list:

- **scroll offset** is core-owned (`Editor::complete_docs_scroll`), reset to 0 on
  any completion selection change (`complete_select_next`/`prev`/`index`).
- the docs box is **content-sized and server-placed** — the content (LSP cache /
  `resolve` / a plugin's inline doc) is server-owned, so core can't recompute the
  geometry the way it does the menu box (`menu_geom`). Instead the server stashes
  the docs float's **global box** into core each redraw
  (`Editor::stash_complete_docs_hit` ← `CompleteDocsHit{x,y,w,h,total,view_h}`),
  computed in `project_menu` from the focused window origin + gutter. The stash is
  gated on a live completion menu (`complete_docs_hit_at`) so a stale box can't
  fire after the popup closes.
- the completion mouse dispatch guards now also fire over the docs box; a
  wheel-over-docs calls `scroll_complete_docs` (non-wrapping, clamped to
  `total − view_h`) instead of moving the highlight; `project_complete_docs`
  windows the lines from `complete_docs_scroll`. A click on the docs box is
  swallowed (no text leak) but selects nothing.

Native-gated, like the existing docs sidebar. Verified by
`wheeling_over_the_completion_docs_sidebar_scrolls_it` (complete.rs): a tall
inline doc, wheel-down advances the visible top by N lines, the highlight is
unchanged, wheel-up returns to the top.

## Out of scope

- Markdown styling / `stylize_markdown` (separate follow-up).
- Converting the completion *menu* itself to a window (stays bespoke — fuzzy
  filter, 100k windowing, prompt/preview).
- `btv.ui.float` content floats stay as-is (which-key relies on the styled,
  short, non-scrolling surface).
