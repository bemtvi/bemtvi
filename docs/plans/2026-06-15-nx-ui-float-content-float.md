# `nx.ui.float` — the content-float sibling (hover / signature help)

**Status:** **done (2026-06-15).** All three phases landed: the core
`ContentFloat` + view projection + redraw `float` key + the tui/gui/web renderers
+ the `nx.ui.float` Lua primitive (tested in `nxvim-server/tests/ui_float.rs`);
LSP hover / signature help rerouted into the float with `nx.lsp.buf.hover` /
`signature_help` verbs (tested in `nxvim/tests/lsp_float.rs` via `nxvim-lsp::mock`);
and the `examples/ui-float/` config. Native (tui/gui) projection is verified
end-to-end through the redraw view; the web renderer mirrors the verified menu
path. Builds clean on both `native` and `--no-default-features` (wasm-eligible).

Builds the second consumer of the shared
`FloatConfig` placement layer named by the
[float-widget spec](../specs/2026-06-14-nx-ui-float-widget.md): the **list-less
content float**. The list-widget (select → picker → preview → completion,
spec phases 1–4) is done; this adds its sibling — *rendered content* with no
list and no selection — and reroutes LSP **hover** and **signature help** from
their placeholder panel/echo surfaces into it.

> Spec, "What stays out of this widget": *"Hover, signature help, and diagnostic
> floats are not this widget … Keep them as `nx.ui.float` (rendered content), a
> sibling of the list-widget on the shared `FloatConfig` placement layer."*

`nx.ui.float` is the one missing `nx.ui` primitive — `select` / `input` /
`confirm` exist; `float` does not.

## What already exists (so this is mostly wiring)

- The full LSP **hover/signatureHelp request→reply cycle already works**:
  `nx._lsp_buf(kind)` queues `LspOp::BufRequest`, `dispatch.rs` issues it,
  `convert.rs::{hover_reply, signature_help_reply}` return plain display lines,
  and `lsp/request.rs::on_lsp_reply` already handles them — today dead-ending
  into `show_hover` (bottom **panel**) and `show_signature_help` (**echo**). The
  comment there literally says *"the panel is the hover surface until floats
  exist — Decision 7."* This phase replaces those two bodies.
- The **docs sidebar** (`project_complete_docs`, Phase 4-D) is a working
  precedent for *"a cursor-anchored float rendering plain markup lines, flipping
  for room"* — the content float reuses its geometry shape.
- The `nx.ui.select` bridge (`_ui_select` → `Shared::ui_selects` →
  `effects.rs` drain → `Editor::open_menu`) is the exact plumbing the
  `nx.ui.float` bridge mirrors.

The content float is a **lightweight overlay** (like the docs sidebar / menu),
**not** a real `Window` — it projects to a redraw sub-map
`{ lines, row, col, width, height, border, title }` rendered directly by clients.

## Design

### Core (`nxvim-core`)
- New `editor/float.rs`: `ContentFloat { lines: Vec<String>, title: Option<String>,
  border: BorderStyle, placement: MenuPlacement }` (reuse `MenuPlacement`
  Cursor/Editor and `BorderStyle` from `windows.rs`).
- `Editor::content_float: Option<ContentFloat>` + `open_content_float(lines, opts)`
  / `close_content_float()`.
- **Dismissal:** at the top of `Editor::input`, clear `content_float` if set, then
  process the key normally (any key dismisses — it opens off-tick on an LSP reply
  or synchronously from Lua, so the *next* key closes it; vim-faithful for a
  transient popup). Non-grabbing: never owns input.
- `content_float_view() -> Option<ContentFloatView>`.

### View (`nxvim-core/src/view.rs`)
- `ContentFloatView { lines, title, border, placement }`; add
  `View::content_float: Option<ContentFloatView>` populated by `content_float_view()`.

### Server redraw (`nxvim-server/src/redraw.rs`)
- `project_content_float(cf, cursor_screen, text_width, text_height) -> Value`:
  - **Cursor** placement: anchor at the cursor cell; prefer **above** the cursor
    (vim hover), flip below when no room; cap width (≤80) / height (≤ rows
    available); window lines. Mirror `project_complete_docs`' clamp math.
  - **Editor** placement: centered.
  - Emit `{ lines, row, col, width, height, border, title }`.
- Add top-level `"float"` key to the redraw map (sibling of `"menu"`).

### Clients (3)
- `nxvim-tui/src/render.rs`: `render_content_float()` — bordered box, optional
  title on the top border, plain lines (reuse the float/menu border helpers).
- `nxvim-gui/src/render.rs`: `build_content_float()` — bg quads + glyphs + border.
- `nxvim-edithost/web/index.html`: `renderContentFloat()` — a bordered div of rows.

### LSP wiring (`nxvim-server/src/lsp/request.rs`)
- `show_hover(lines)`: empty → echo (unchanged); else
  `open_content_float(lines, { border: Rounded, placement: Cursor })`.
- `show_signature_help(sig, param)`: empty → echo; else build lines
  (`sig`, and the active parameter) and open the float. Update Decision 7 comment.

### Lua surface
- New `prelude/lsp.lua` (minimal slice of nx-lsp Phase A): `nx.lsp.buf.hover()` /
  `nx.lsp.buf.signature_help()` → `nx._lsp_buf(5/6)` (kinds per `LspReqKind`),
  aliased onto `vim.lsp.buf.*`. Register in the prelude loader after `ui.lua`.
- `prelude/ui.lua`: `nx.ui.float(contents, opts)` — `contents` a string or list;
  `opts = { border, title, relative = "cursor"|"editor" }` → `nx._ui_float`.
- `install.rs`: `_ui_float(lines, title, border, relative)` bridge → push
  `UiFloatReq` onto `Shared::ui_floats`; `effects.rs` drains it →
  `Editor::open_content_float`.

### Tests (`nxvim-server/tests/`, black-box)
- `ui_float.rs`: `nx.ui.float({...})` projects a `float` redraw sub-map with the
  lines; the next key dismisses it; `relative="editor"` centers.
- LSP `hover`/`signatureHelp` via `nxvim-lsp::mock` (`$NXVIM_LSP_CMD`): the reply
  lands as a **float** (not a panel/echo) carrying the markup; an empty reply
  echoes; a cursor move before the reply drops it (existing staleness gate).

### Example config
- `examples/ui-float/` — `init.lua` mapping `K` → `nx.lsp.buf.hover()` and a
  command calling `nx.ui.float`, with a sample file, verified end-to-end.

## Phasing (each independently testable)
1. Core `ContentFloat` + view projection + redraw `float` key + the 3 client
   renderers + `nx.ui.float` Lua primitive + `ui_float.rs` test. (Float works
   from Lua everywhere, incl. wasm.)
2. Reroute `show_hover` / `show_signature_help` into the float + `prelude/lsp.lua`
   verbs + mock-LSP tests. (Native LSP consumer.)
3. Example config + docs.
