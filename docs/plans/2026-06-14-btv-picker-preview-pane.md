# `btv.picker` preview pane — the unified widget's Phase 3 — phased plan

> **Status: done (2026-06-14).** Both stages shipped — **3a** (plain file/location
> preview, end-to-end across TUI/GUI/web; server `PreviewCache` + `project_preview`;
> the `preview`-kind bridge) and **3b** (native tree-sitter colours via the stateless
> `SyntaxEngine::highlight_text`). Picker-only scope as decided; the `"markdown"` kind
> and the cursor-placement float-beside layout are **deferred to Phase 4**
> (`btv.complete` docs sidebar — their only consumer). Tests: `bemtvi-server`'s
> `picker.rs` (preview content/geometry/location/placeholder/confirm) and `bemtvi`'s
> `syntax.rs::the_file_preview_pane_is_syntax_highlighted` (3b colours).

> Working checklist for **Phase 3** of the unified float-list widget
> (`docs/specs/2026-06-14-btv-ui-float-widget.md`): the **preview pane**, rendered
> natively by kind, that completes `btv.picker`. Lives at
> `docs/plans/2026-06-14-btv-picker-preview-pane.md` alongside the other plans.

## Context

The unified float-list widget is the one Rust component completion, the picker, and
`btv.ui.select` render through — *a float containing a selectable, match-highlighted
list*, with an optional preview and an optional prompt. **Phase 1** (the list +
placement, backing `btv.ui.select`) and **Phase 2** (the prompt input-grab, the Rust
fuzzy matcher, dynamic-source forwarding with generation tokens, streaming — commits
`84cb5bb`, `515e565`) are done. The picker works end-to-end today *without a preview*.

**This phase adds the preview pane** — the side pane the widget renders, by kind,
for the selected item. Per the spec's preview table, the picker's two kinds are:

| `preview` kind | Renders | For |
| --- | --- | --- |
| `"file"` | the file at `item.path` (rope + native tree-sitter) | file picker |
| `"location"` | the file at `item.path`, scrolled to `item.row`/`col`, range-highlighted | references / grep |

### Scope (decided with the user, 2026-06-14)

1. **Picker-only.** This phase ships the `file` + `location` kinds in the
   **editor-placement** (centered box) *pane-within* layout — what "completes the
   picker." The spec's `"markdown"` kind and the **cursor-placement float-beside**
   layout back the **completion docs sidebar**, whose only consumer is `btv.complete`
   (Phase 4); they land there, not here.
2. **Tree-sitter, staged.** **3a** ships a working *plain-text* preview end-to-end
   (read the file, window to the location, range-highlight the match) across TUI /
   GUI / web. **3b** adds native tree-sitter syntax colors. Each stage is
   independently shippable; 3a is a real preview (not a stub), 3b adds colour.

### Strict layering (unchanged from Phase 2)

`bemtvi-core` stays **pure and synchronous**. Core owns only: the per-item *preview
target* (path + optional location) carried alongside `{label, key}`, and exposing
the **selected** item's target. *Everything* else — reading the file (sync `HostFs`
native, the off-tick FS seam for wasm/daemon), caching it, and rendering it to lines
+ highlight spans — lives in `bemtvi-server`'s redraw layer, where the host FS and the
tree-sitter highlighter already live (`highlights_for`, `treesitter.rs:103`). Core
never reads a file for preview; the server resolves the target into rendered content.

## Core idea: the item carries an optional preview *target*; the server renders it

Today only `{label, key}` crosses the bridge; the full item table stays Lua-side. A
preview needs the server to know *what to render* without re-entering Lua on every
selection move (rule 4 — no Lua at frame time). So the **preview target** crosses
with the pushed item, declaratively:

- The **source** declares a `preview` *kind* (`"file"` | `"location"`); the picker
  carries it so the widget knows to reserve a preview column.
- Each **item** carries the fields that kind needs — `item.path` (both kinds) and
  `item.row` / `item.col` (`location`). The Lua wrapper extracts these into the push
  alongside `label`/`key`; the arbitrary rest of the item stays Lua-side as before.

The server holds a small **preview cache** (last path → file lines, and in 3b the
parsed highlight spans) so moving the selection within results — or simply
re-projecting on every redraw — never re-reads or re-parses unless the *target path*
changes.

## Phase 3a — Plain-text preview, end-to-end

### Core (`bemtvi-core`, pure/sync)

- `MenuItem` (`menu.rs:77`) grows `preview: Option<PreviewTarget>`:
  ```
  pub struct PreviewTarget {
      pub path: String,
      pub loc:  Option<(usize, usize)>,  // (row, col), 0-based; None ⇒ "file" kind
  }
  ```
  `file` items set `loc = None`; `location` items set `loc = Some((row, col))`.
- `Menu` (`menu.rs:151`) grows `preview: bool` (the picker declares a preview pane);
  `open_picker` (`menu.rs:300`) gains a `preview: bool` parameter. `btv.ui.select`'s
  `open_menu` is unchanged (no preview).
- `Menu::selected_preview(&self) -> Option<&PreviewTarget>` resolves
  `filtered[cursor] → all_items[..].preview` (clamped like `menu_rows`).
- `MenuView` (`view.rs:89`) grows `preview: Option<PreviewTarget>` (the *selected*
  item's target — `None` when the picker has no preview pane or the row carries no
  target). `menu_view` (`menu.rs:531`) fills it from `selected_preview`.

### Bridge + Lua (`bemtvi-lua`)

- `btv.picker.source` spec accepts `preview = "file" | "location"`; `btv.picker.open`
  passes it to `btv._picker_open` (a new bool/string arg) → server `open_picker`.
- `btv._picker_push` (`picker.lua`): when the source declares a preview kind, extract
  `item.path` (+ `item.row`/`item.col` for `location`) into a parallel `previews`
  array sent with `labels`/`keys`. No preview kind ⇒ no previews array (select and
  preview-less pickers unchanged). Validate-loud: a `location`/`file` source whose
  item lacks `path` errors (no silent empty preview).
- Built-in sources gain previews: `files` → `preview = "file"`; `live_grep` →
  `preview = "location"` with `item.row`/`item.col` from `rg --vimgrep`; `buffers`
  → `preview = "file"` (the buffer's backing path; unnamed buffers ⇒ no target).

### Server (`bemtvi-server`)

- `MenuItem` push (`effects.rs:~340`) carries the `previews` through to
  `bemtvi_core::MenuItem.preview`.
- A `PreviewCache` on `EditHost` (`lib.rs`): `{ path: Option<PathBuf>, lines:
  Vec<String> }`. New method `EditHost::resolve_preview(&mut self, target:
  &PreviewTarget, height) -> ProjectedPreview` — read the file via the editor's
  `HostFs` (sync, native; the off-tick seam for wasm/daemon, mirroring `:e`'s
  `PendingOpen`) on a path miss; window `height` lines around `loc.row` (centred,
  clamped to file bounds); return the windowed lines, the 1-based `first_line`, the
  display title (path, home-relativised), and the in-window `loc` range. A read error
  yields a one-line `"<path>: <err>"` placeholder (fail visible, not silent-empty).
- `project_menu` (`redraw.rs:701`): when `m.preview.is_some()`, split the centred box
  — reserve a **preview column** (default ~60% of inner width; list takes the rest
  minus a 1-col separator), call `resolve_preview` for the box's inner height, and add
  a `"preview"` entry to the menu map:
  ```
  preview = {
    lines      = [String],          -- already windowed to the pane height
    first_line = u64,               -- 1-based file line of lines[0]
    title      = String,            -- path
    loc        = [row, col] | nil,  -- match position, relative to lines[0]
    width      = u64,               -- preview column width
    highlights = [],                -- per-line syntax spans; empty in 3a, filled in 3b
  }
  ```
  The existing `width`/`height`/`row`/`col` stay the **whole box**; the list renders
  in `width - preview.width - 1` columns. `btv.ui.select` and preview-less pickers omit
  the `preview` key entirely (clients render exactly as today).

### Wire + clients

- `bemtvi-view` `MenuData` (`view.rs:324`) grows `preview: Option<MenuPreview>`
  (`{ lines, first_line, title, loc, width, highlights }`), decoded from the map.
- TUI `render_menu` (`render.rs:1668`), GUI `build_menu` (`render.rs:1600`), web
  `renderMenu` (`index.html:992`): when `preview` is present, split the inner box into
  the list column (left) + a 1-col separator + the preview column (right); render the
  title on the preview's top border / first row, the windowed lines, and reverse /
  visual-highlight the `loc` range. Each client already draws the list box; this adds
  the right-hand pane and the vertical rule.

### Tests (`crates/bemtvi-server/tests/picker.rs`, black-box)

- A `file` picker: select row → `preview.lines` are that file's head; move selection
  → preview swaps to the new file (assert `preview.title` / first line). Document
  buffer (`nvim_buf_get_lines`) untouched throughout.
- A `location` source: `preview` windows to `loc.row` and the `loc` range is present
  and in-bounds.
- Geometry: with a preview the box splits (list width < box width by `preview.width +
  1`); a preview-less picker / a `select` carries **no** `preview` key.
- An unreadable path yields the visible placeholder line, not a panic or empty pane.
- Confirm / `<Esc>` behave exactly as Phase 2 (preview never affects the outcome).

## Phase 3b — Native tree-sitter syntax colours

- Reuse, don't fork, the highlighter. `highlights_for` (`treesitter.rs:103`) is keyed
  by a registered `BufferId` with live syntax state — wrong shape for a transient
  preview file. Add a **stateless** highlight path to the `SyntaxEngine` trait
  (`bemtvi-core/src/syntax.rs`): `highlight_text(language, text, first_line, last_line)
  -> Vec<Span>` — parse the string into a throwaway tree, run the highlight query over
  the line range, return per-line byte spans, drop the tree. No `BufferId`, no buffer
  lifecycle, no listing / autocmd / file-watch entanglement (the hidden-buffer
  alternative drags all of those in — rejected).
- The server resolves the file's tree-sitter language from its path/extension (reuse
  the filetype → language map the editor already uses), calls `highlight_text` for the
  windowed range, and maps byte spans → screen columns + style-table ids exactly as
  `highlights_for` does (factor the shared byte→col + style-resolve helper so the two
  paths agree). Cache the spans in `PreviewCache` keyed by `(path, mtime)` so
  selection moves within one file don't re-parse.
- `project_menu` fills `preview.highlights`; clients colour the preview lines using
  the same span renderer they already use for the buffer text.
- Tests: a `.rs` / `.lua` preview carries ≥1 non-default highlight span at the
  expected token; the `loc` range highlight composes over the syntax colours.

## Risks & notes

- **Per-redraw cost.** `project_menu` runs every redraw; `resolve_preview` must be a
  cache *hit* unless the target path changed. Key the cache on path (3a) / `(path,
  mtime)` (3b); never read or parse on a hit.
- **wasm / daemon FS.** Native reads the preview file synchronously (as `:e` does
  natively). wasm/daemon must route the read through the off-tick FS seam (the
  `PendingOpen` analogue) — the same seam OPFS / WebTransport file opens already use.
  Native lands first; the wasm preview leg reuses that seam. Until a path resolves,
  show a `"loading…"` line, never a blank claim of "no preview" (fail visible).
- **No silent stubs.** A source declaring `preview` whose items lack `path` must error
  loud; an unreadable file shows a visible placeholder. A preview that "opens" but
  never shows content is the quietly-broken shape the project forbids.
- **Box geometry.** The preview column is part of the **fixed** box (Phase 2's
  `MenuExtent`), not content-hugging; the list simply gets fewer columns. Empty
  preview rows pad to the box height like the list does.

## Verification

1. `cargo build --workspace`; `cargo test -p bemtvi-server --test picker`.
2. `cargo clippy --all-targets -- -D warnings`; `cargo fmt --all -- --check`.
3. `cargo run -p bemtvi -- .` → `btv.picker.open("files")` shows the highlighted file
   under the cursor as you move; `live_grep` scrolls the preview to each match and
   highlights it; `buffers` previews each buffer's file.
4. Extend `examples/ui-picker/` to exercise the preview; drive the wasm build's
   `verify-ui.mjs` (preview reads via the off-tick seam) if the FS leg is wired.
5. Regression: `btv.ui.select` and a preview-less picker render byte-identically to
   Phase 2 (no `preview` key, no geometry change).
