# A shared scene layer, and a grid protocol for terminal embedding

**Status:** proposal (2026-11-20). Not scheduled. This records an audit of the
renderers and the shape of the work, so it can be picked up later. Nothing here has
been started.

**Goal.** Two things that turn out to be the same work:

1. **Embed the editor in a terminal app** without the host linking ratatui, or Rust at
   all. A host runs `bemtvi --embed`, speaks msgpack-RPC on stdio, and receives a cell
   grid to blit. This is the terminal counterpart of the GUI embed (`bemtvi-mitsuami`).
2. **Make every front end make its layout decisions in one place.** Today each
   renderer reimplements, separately, where the docks go, how a row's highlights,
   selection and virtual text combine, where a menu sits, and so on.

## Why one layer serves both

A thin embedding host needs painted cells, not the structured `View`. Painting a `View`
into cells is the TUI renderer's job, about 3.7k lines that no third-party host should
reimplement. So the embed needs the painter to run inside bemtvi. Making that painter
reusable means separating its *decisions* (cell-unit layout and composition) from its
*output* (ratatui). The GPU renderer makes the same decisions on its own, so the
separated layer can serve it too.

### Why not use the grid as the universal protocol

Neovim made `ext_linegrid` its universal UI protocol. GUIs then needed
`ext_multigrid`, `ext_popupmenu`, `ext_cmdline`, `ext_messages` and `win_viewport` to
get back the structure the grid had thrown away. bemtvi's GUI depends on that
structure:

- smooth scroll with fractional row offsets per window;
- the off-grid glyph model and per-character font fallback;
- floats placed in pixels, with resize handles;
- images, the `'modeless'` caret, and pointer shapes from hit-tests.

A GUI that consumed cells would have to recover which window a cell belongs to from
painted output, which is the kind of heuristic CLAUDE.md rules out. The `View` stays
the wire protocol. The cell grid becomes one *projection* of it, used for terminal
embedding.

## Audit (as of `7ab51b61`)

| | TUI `crates/bemtvi-tui/src/render.rs` (3.7k) | GPU `crates/bemtvi-gpu/src/render.rs` (6.6k) |
|---|---|---|
| Cell-unit layout and composition | ~2.4k | ~2.3k |
| Backend-specific code | ~0.8k: ratatui, the hardware cursor, image protocols, wide-glyph diff repair | ~2.5k: wgpu, glyphon shaping, off-grid glyph placement, fractional scroll, quads |
| Already shared via `bemtvi_view` | `fit_row`, `pmenu_row`/`pmenu_start`, `doc_box`, `gutter_cell`, `fit_gutter_text`, `clip_band_row`, `arm_scroll` | the same set, plus `resize_handle_at`/`text_pointer_at` and scrollbar thumbs |

A third renderer exists: the web client paints the `View` in JS/DOM
(`crates/bemtvi-edithost/web/index.html`, about 2k of its 4.2k lines). It has its own
`dockGeo`, `fitRow`, `renderMenu`, `renderPmenu`, gutter painting, and
`resizeHandleAt`/`textPointerAt`. It already loads Rust compiled to wasm for column
math (`width.js` → `bemtvi_width.wasm`), so the browser can use Rust layout code.

### What is duplicated

- **Region band math.** The stack is
  `[top dock][tabline][left|main|right][global status][bottom dock][cmd]`, with `n + 1`
  cells reserved per open dock and a dock tabline taking that dock's first row. There
  are four copies:
  - TUI `DockLayout` (`render.rs:595-811`);
  - GPU `build_frame` (`render.rs:1037-1254`);
  - `bemtvi_view::hit_test` (`reserved`, `middle_band`, `region_origin`, whose comment
    says to keep it in step with the renderer);
  - web `dockGeo`.
- **Gutter column order.** fold → sign → number → gutter-text → text. Four copies: TUI
  `render_window`, GPU `build_window`, `hit_test::text_pointer_at`, and the web client.
- **Window rect, padding and float inset.** TUI `window_area`/`pad_rect`/`float_block`,
  GPU `build_float`/`pad_rect`, and `hit_test::text_pointer_at`.
- **Row composition.** This is the largest duplication and the hardest part to get
  right.
  - Style layering: highlight span (palette id, else a built-in capture palette), then
    selection, then Search, then IncSearch, then the diagnostic underline.
  - Placement: inline inlay hints and virtual text are spliced in and push later text
    right. Overlay and `win_col` chunks paint over glyphs, merged by `hl_mode`. After
    the end of the text come the eol chunks, then right-aligned chunks, then diagnostic
    virtual text.
  - TUI: `highlight_line` + `cell_style` (~700 lines). GPU: `row_segments` +
    `apply_row_virt` + `splice_insertions` + the recolour passes (~740 lines).
- **Mapping a span or the cursor to a screen column**, including the inlay and inline
  virt-text shifts. TUI: `inlay_cursor_shift`/`virt_cursor_shift`. GPU:
  `overlay_span_cols`/`inlay_shift`/`virt_inline_shift`.
- **Menu, picker and wildmenu geometry.** The wildmenu grows up from the cmdline and
  has no bottom border. The completion style has no top border and shifts left one
  cell. The preview pane is split off with a rule, and the prompt, separator and filter
  rows go on top of the list or below it. TUI `render_menu` + `menu_row_line` (~485
  lines); GPU `build_menu` (~405 lines).
- **Other chrome:** the pmenu box, the content float, tab cells
  (` {count }{label}{+} `), status segment layout, the cmdline, showcmd and hidden-dock
  chips. The chips must also match the core's `hidden_chip_at`.
- **Colour fallback chains:** StatusLine/StatusLineNC, WinSeparator, TabLine*, MsgArea,
  and the built-in capture palette.
- **Width helpers:** `cells`, `cells_before`, `expand_tabs`, `pad_to_width`,
  `take_cells`/`truncate_to_width`, and the `VIRT_POS_*` constants. There are copies in
  the TUI, the GPU, and private ones in `bemtvi_view::band`.

### Divergences already found

Duplicated code has started to drift. Some of these differences may be deliberate. The
code doesn't say which, and each one is a decision to make when that piece moves.

- **Verified:** the TUI clamps the completion popup to the text area (`popup_rect`,
  `render.rs:3660`). The GPU places it where the server says, unclamped
  (`build_pmenu`).
- **Reported by the audit, not yet checked:**
  - The TUI clamps tiled window rects to their region; the GPU does not.
  - Status segments are clipped differently: the TUI fills the row under segments the
    server already fitted, and the GPU does not clip per segment.
  - During a smooth scroll, the GPU does not paint eol and right-aligned virtual text or
    chunk backgrounds. The TUI does, because it slices the band per row.
  - `hl_mode = blend` averages RGB in the TUI. Check the GPU's `virt_overlay_fg` for
    the same rule.
- **The same in both, possibly a bug in both:** status rows use the global
  `view.status_line`/`status_line_nc`, not the per-window `WindowView::status_line(view)`
  that honours `winhighlight`.

## Design

```
View  (wire protocol, unchanged)
  │
  ▼
scene(view, anim) -> Scene        bemtvi-view, no toolkit dependency, pure
  │
  ├─► TUI         snap to whole cells → ratatui buffer → terminal
  ├─► --embed     snap to whole cells → diff → grid_* messages on stdio
  ├─► GPU         draw at full precision → shaping, off-grid glyphs, quads
  └─► (web)       via wasm, optional and last
```

### The `Scene`

Every position is in cell units. Fractional values appear only where the animation puts
them.

- **Surfaces:** regions, tiled windows, floats, and overlays (pmenu, menu, content
  float), each with a rect, a z-order, its border spec, and padding already applied.
  A window with a smooth scroll in progress carries its fractional row and column
  offset, plus the band's rows as data. It must not be baked into fixed rows, or the
  GPU loses fractional scrolling.
- **Rows:** per window, each row has its gutter cells and its text as **styled runs**
  with the composition already done: spans, selection, search, diagnostics, spliced
  inlays and virtual text, overlay chunks, eol and right-aligned chunks. Each run has
  its starting cell column and its cell width. Background layers (line background,
  colorcolumn, cursorline) are separate full-width or per-cell fills, so the GPU can
  keep drawing them as quads.
- **Chrome:** tabline, dock tablines, status rows, the cmdline, showcmd and chips, the
  rows of menus and pickers, and the preview pane, all as styled runs at cell
  positions.
- **Cursors:** primary and secondary, each with a cell position, width and shape. The
  backend decides how to draw them: the terminal hardware cursor, a cell painted in
  reverse, or a GPU quad.
- **Images:** rects plus an image id. A backend that can't draw images reports so
  loudly; it never leaves blank cells without saying why.
- **Styles:** resolved, with the fallback chains already applied. Snapping a colour to
  what a terminal can show (indexed colours, a REVERSED fallback) is the backend's job.

`hit_test` reads the scene's geometry instead of re-deriving it, so its "keep in step"
copy goes away.

### What stays in each backend

- **TUI:** snap to cells, the ratatui buffer, the hardware cursor, the cursor shape,
  image protocols, and the wide-glyph diff repair.
- **GPU:** the wgpu pipeline; glyphon shaping and its per-line cache; off-grid cluster
  detection, scaling and overflow; block-element rects; drawing at fractional y and
  clipping; quad thickness; text occlusion under popups; colour-space conversion.
- **Embed:** snap to cells (shared with the TUI), diff against the previous grid, and
  encode the result as messages.

### The `--embed` grid protocol

A host spawns `bemtvi --embed`. The process pairs the server, or a `--connect-daemon`
session, with a grid adapter in place of the terminal client. The adapter is an
ordinary UI client of the `View` protocol, so the server does not change and a remote
session works the same way as a local one.

Messages to the host, sent in batches that end in `flush`:

| Message | Payload |
|---|---|
| `grid_resize` | `width, height` |
| `hl_define` | `id, {fg, bg, sp, bold, italic, underline: "line"/"curl"/…, reverse, strike}`: a style interned once |
| `grid_line` | `row, col, [[text, hl_id?, repeat?], …]`. `text` is a whole grapheme cluster. A wide character is followed by an empty `""` continuation cell. |
| `grid_scroll` | `top, bot, left, right, rows` |
| `grid_clear` | full repaint |
| `cursor` | `row, col, shape, visible` |
| `flush` | frame boundary |
| `clipboard` / `title` / `bell` | today's `btv_ui_send` and OSC 52, sent as data |

Input from the host stays as it is: `btv_input` with key notation, mouse events at grid
coordinates, `btv_ui_try_resize`, and paste. The adapter owns the timers the TUI client
owns today: `timeoutlen` flush, autoscroll, and the scroll animation. The animation is
gated by an embed capability, because each animated frame costs bandwidth.

## Risks

- **Smooth scroll on the GPU** is the constraint the scene design must meet. The GPU
  paints the band at fractional y and clips. Build a GPU proof of concept on the scene
  before committing to its shape.
- **A scene is allocated every frame.** It is probably fine, since the GPU already
  rebuilds its draw lists every frame. Measure it with the large-flood guard
  (CLAUDE.md: the editor must never freeze).
- **Test coverage of row composition is uneven.** Before moving it, pin the current
  behaviour of both renderers with `paint`/`paint_frames` tests on the TUI side and the
  GPU's headless tests, and mutation-test the merged rules.
- **Divergences.** Each one in the list above needs a decision when its piece moves,
  not a silent pick of one renderer's behaviour.

## Phases

### Phase 1 — geometry

Move the frame's geometry into `bemtvi-view` as a pure function:

- region bands and origins, including dock tablines;
- window rects, padding, float border insets;
- gutter column widths and the text rect;
- overlay anchors (cmdline area, editor area, focused text rect) and the rects of the
  pmenu, menu and content float, including the wildmenu and completion-style offsets
  and the preview split.

Then:

- TUI `DockLayout`, `window_area`, `pad_rect` and `popup_rect`, and the matching parts
  of GPU `build_frame`/`build_window`/`build_menu`, call it.
- `hit_test` calls it too, and its "keep in step" copy goes away.
- Decide the clamping divergences (pmenu, tiled window rects). Write tests that state
  the chosen behaviour for both renderers.

It is small, low-risk, and shippable on its own. It removes three of the four copies of
the band math.

### Phase 2 — row composition

Styled runs per row: style layering, the inlay and virt-text splices, overlay and
`win_col` placement, `hl_mode`, and the eol, right-aligned and diagnostic
virtual-text placement, plus the cursor-column mapping that depends on them. It is the
largest phase. Pin the existing behaviour first (see Risks). Decide whether
end-of-line decorations show during a smooth scroll.

### Phase 3 — chrome

Tabline and dock tablines, status rows (decide on the `winhighlight` StatusLine), the
cmdline, showcmd and chips, menu and picker rows and the preview pane, content-float
lines, the colour fallback chains, and the width helpers, ending with a single copy.

### Phase 4 — `--embed`

The grid adapter and protocol on the TUI's snap-to-cells path, a reference host (a
short Python script, which also serves as the end-to-end test), and loud failure for
unsupported capabilities such as images.

### Phase 5 — web client (optional)

Build the scene for wasm next to `bemtvi_width.wasm`, and move the web client's
painting onto it, retiring most of its JS rendering.

## Not in scope

- Changing the `View` wire protocol.
- A public Rust embedding API that exposes ratatui types. That was rejected in favour
  of the stdio grid protocol, so no host is tied to our ratatui version.
- Running GUI-only behaviour (off-grid glyphs, `'modeless'`, pixel-positioned floats)
  through the grid protocol. A grid host gets what the TUI gets.
