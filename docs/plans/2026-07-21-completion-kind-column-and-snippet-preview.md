# Completion menu: per-item **kind** column + snippet **doc preview**

Status: in progress (started 2026-07-21)

## Motivation

Testing the `snippets` source surfaced two gaps in the completion popup:

1. **Snippet rows are indistinguishable** from buffer words / LSP items — you can't
   tell which candidate expands a snippet.
2. More generally, the popup shows only a label. Neovim shows a per-item **kind**
   (`Text`, `Function`, `Field`, `Variable`, `Snippet`, …). We want the same: a
   right-aligned kind column per row.

Both are the same fix at heart: surface each candidate's kind. A kind column makes
snippets read `Snippet` for free, and gives LSP items their real kinds.

Separately, the user wants a **doc preview for the highlighted snippet** (its
expansion / description) like the function-doc float LSP items already get. That
float already exists (`sync_complete_docs_float` → `open_completion_docs_float`);
snippet rows just need to carry a `doc`.

## What exists today (map)

- `MenuItem` (`bemtvi-core/src/editor/menu.rs:160`) carries `label`, `insert`,
  `doc`, `resolve`, … but **no `kind`**.
- `MenuGeom.rows` is `Vec<(String, Vec<Range>)>` (label + match spans); redraw
  projects `items` + `match_spans` only (`redraw.rs:1803`). `menu_marked_window`
  (`menu.rs:1705`) is the template for a parallel per-row accessor.
- LSP already computes a numeric kind: `CompletionItemData.kind: u8` via
  `kind_code` (`bemtvi-lsp/src/convert.rs:189`) — but it is **dropped** in
  `complete_lsp_push` (`bemtvi-server/src/lsp/completion.rs:116`) because `MenuItem`
  has nowhere to put it.
- Snippet rows built in `snippet.rs:58` with `doc: None`.
- Lua source push contract: `btv._complete_push(gen, labels, inserts, docs, resolves,
  accepts)` — 6 parallel arrays, decoded into `CompletePush` (`runtime.rs`),
  drained in `effects.rs:1196`. No kind slot.
- Docs float: `sync_complete_docs_float` (`effects.rs:3323`) →
  `selected_complete_docs_md` (`effects.rs:3343`) reads `MenuItem.doc`, else LSP
  cache. Renders via `Editor::open_completion_docs_float`.
- Clients paint rows from `MenuData.items` (`bemtvi-view/src/view.rs:593`): TUI
  `render_menu` (`bemtvi-tui/src/render.rs:2710`), GUI (`bemtvi-gui/src/render.rs:2657`).
  A retired `pmenu_row` (`gui/render.rs:3585`) already right-aligns a `detail`
  column — template for the kind column.

## Phase 1 — kind column, server-native sources (LSP + snippets + buffer) — ✅ DONE

Deliver the visible win end-to-end for the two sources whose kind we know natively.

1. **Core**: add `kind: Option<String>` to `MenuItem` (`menu.rs:160`). Update every
   `MenuItem { … }` literal (buffer seed, snippet, lsp, plugin drain, cmdline,
   picker, select) to set it — `None` everywhere except where a source knows it.
2. **Core accessor**: add `menu_kinds_window(start, count) -> Vec<Option<String>>`
   next to `menu_marked_window` (`menu.rs:1705`), reading `all_items[item_at(i)].kind`.
3. **LSP source**: add `kind_label(u8) -> Option<&'static str>` in `bemtvi-lsp`
   (beside `kind_code`) mapping 1→"Text" … 25→"TypeParameter". Populate
   `MenuItem.kind` in `complete_lsp_push` from `item.kind`.
4. **Snippets source**: `kind: Some("Snippet".into())` in `snippet.rs:58`.
5. **Buffer source**: leave `kind: None` (a bare buffer word has no kind — matches
   nvim-cmp, which shows the source name, not a kind, for buffer words). Revisit if
   we want a `Text` label.
6. **Redraw**: project a `kinds` array parallel to `items`
   (`redraw.rs:1803`) — `Value::Nil` for a kind-less row.
7. **Client mirror**: `MenuData.kinds: Vec<Option<String>>`
   (`bemtvi-view/src/view.rs`), parsed beside `items`.
8. **Clients**: right-align the kind in `render_menu` (TUI + GUI), styled with a
   dim/`CmpItemKind`-ish group. Reuse the retired `pmenu_row` right-align logic.
   Widen the popup to fit `label + gap + kind`.
9. **Tests**: `bemtvi-server/tests/complete.rs` — assert the `kinds` array in the
   projected `menu` map (snippet row → `"Snippet"`, LSP row → `"Function"` via the
   mock, buffer row → nil). Mutation-check by breaking the mapping.

## Phase 2 — Lua source kind + snippet doc preview — ✅ DONE

1. **Lua push contract**: add a `kinds` array to `btv._complete_push` and read
   `item.kind` in `complete.lua`'s `push` (accept a string; default nil). Thread
   through `CompletePush` + the `effects.rs:1196` drain. Document the item shape.
2. **Snippet doc preview**: populate `MenuItem.doc` on snippet rows
   (`snippet.rs:58`) with a rendered preview — a fenced code block of the snippet
   **body** (tabstops shown), optionally a description line. Flows through the
   existing `selected_complete_docs_md` → docs float with zero new plumbing.
   - If we add an optional `description` to `SnippetEntry` / `btv.snippet.add`, show
     it above the body. (Stretch — keep to body-only if the API add is noisy.)
3. **Tests**: Lua source kind round-trips to the `kinds` projection; snippet doc
   float opens with the body when a snippet row is selected.
4. **Example refresh**: extend `examples/` snippet/completion config with a
   type-this/see-that note for the kind column + snippet preview.

## Out of scope

- Icons / nerd-font glyphs per kind (kind is a word label; a later theme concern).
- Reworking the docs-float placement.
