# Modeless context menu

A right-click in a modeless session (`:set modeless`, GUI and web) pops a context
menu at the pointer with the verbs a GUI editor offers there: Cut, Copy, Paste,
Select All, Undo / Redo, the LSP verbs when a server is attached, and the command
palette. Each row shows its shortcut, and a row that can't run greys out.

## Where things stand

- A right press reaches the core as a mappable `<RightMouse>`. With no map, the
  `'mousemodel'` default runs (`mouse.rs::mouse_right_press`). Under the default
  `popup_setpos` it moves the cursor unless the click lands in the Visual
  selection. `popup` was meant to pop a menu but does nothing, because no menu
  widget was built.
- That default doesn't know about the modeless selection. A modeless selection is
  Select mode with a caret record (`nm_sel`), and `pos_in_visual` only tests
  Visual. So a right-click moves the cursor out from under a live Select and leaves
  it stale. This is a bug.
- bemtvi-tree builds its menu in Lua: a buffer-local `<RightMouse>` map reads
  `btv.getmousepos()`, moves its cursor onto the clicked row and awaits
  `btv.ui.select`. That works for a tree. A text editor can't use the same shape:
  a mapped right-click skips the default, so the plugin would have to re-implement
  "keep the selection if the click is inside it, else move the caret". It also
  can't open the list at the pointer, because `btv.ui.select` only opens at the
  text cursor (`MenuPlacement::Cursor`).
- `btv.ui.select` rows are label-only. A select also takes two clicks to choose
  a row: the first highlights it and the second confirms it.
- Clients don't send pointer-move events, so a menu can't track hover.

## Design

The behaviour follows vim. `popup_setpos` moves the caret first and then pops the
menu, and the menu itself is a plugin. The pieces:

1. **The core keeps doing the caret work and asks for a popup.** The
   `popup` / `popup_setpos` default runs as before (modeless-aware now) and then
   queues a *popup request*. The server answers it by firing **`MenuPopup`**,
   vim's event for "just before the popup menu shows". The handler finds the
   click with `btv.getmousepos()`. When nothing handles the event, nothing
   happens, as before. A mapped `<RightMouse>` (bemtvi-tree) still wins over all
   of this, because a map replaces the default.
2. **`btv.ui.menu(entries, opts)` is the context-menu surface.** It runs on the
   same floating-list widget as `btv.ui.select` and returns a promise of the
   chosen entry. Each entry is `{ label, detail?, enabled?, separator? }`:
   - `detail` goes in the existing aligned right-hand `kinds` column, which
     completion already uses for its kind labels.
   - A disabled row is painted with a row highlight (the existing `row_hls`,
     group `BtvMenuDisabled`), is skipped by keyboard navigation and does
     nothing when clicked.
   - A separator is its own row kind on the wire (`separators`), which each
     client draws as a rule in the border colour. A label can't do it: clients
     clip a label at the detail column and draw the detail in its own colour.
   - `opts.anchor = "mouse"` opens the menu at the last mouse cell
     (`MenuPlacement::Pointer`). It goes on the wire as an editor-relative box,
     which every client already places, so placement needs no client change.
     `"cursor"` (the default) keeps today's placement.
3. **A pointer menu acts like a GUI menu.** One click confirms a row. A press
   off the box cancels it, and so does a right press, which then opens a fresh
   menu where it landed. A pointer move over a row highlights it. Clients send a
   `move` gesture only while a menu is open, and only when the pointer cell
   changes.
4. **The modeless menu is prelude Lua** (`prelude/modeless.lua`). It handles
   `MenuPopup` in an editable modeless buffer, and its entries are a table users
   can edit (`btv.modeless.context_menu`). Each entry is `{ label, action, when?,
   enabled? }`, where `action` is a `btv.modeless.actions` name or a function.
   Shortcut hints come from the live `modeless` keymap, so a rebound key shows its
   new chord, written the platform's way (`Ctrl+C`, or `⌘C` on macOS).

A read-only buffer (help, quickfix, a tree) rests in Normal, and the
`btv.modeless.actions` verbs refuse to run there. The default menu therefore opens
only in an editable buffer. A read-only surface that wants a menu binds its own
`<RightMouse>`, as the tree does.

## Phases

### Phase 1: modeless right-click default and `MenuPopup`

- Test first: a right-click outside a modeless selection leaves a stale Select.
  `popup_setpos` in a modeless session should keep a selection the click lands in
  and otherwise collapse to a caret at the click, with caret positions allowed
  past the end of a line, the way a left press places them.
- The core queues a popup request from the `popup` and `popup_setpos` defaults
  (`Editor::take_popup_request`). The server drains it after resolving the mouse
  clicks and fires `MenuPopup` with the current buffer, on both the native path
  and the wasm path.
- Document `MenuPopup` in `docs/autocmd-events.md`.

### Phase 2: `btv.ui.menu` and the pointer menu

- `MenuPlacement::Pointer { row, col }`. `menu_geom` places the box's top-left
  at the cell, flips it up or left when it doesn't fit, and clamps it to the
  editor. `menu_screen` hit-tests it as an editor-absolute box, and the server
  sends `editor_relative`.
- Per-row `detail` / `disabled` / `separator` on the select request, mapped onto
  `MenuItem.kind` / `MenuItem.hl`. Navigation and confirmation skip disabled
  rows.
- Mouse handling: one-click confirm on a pointer menu, right press off the box
  cancels it, and `MoveTo` over a row hovers it.
- `btv.ui.menu` in `prelude/ui.lua`, with its docs.

### Phase 3: the modeless context menu

- `btv.modeless.context_menu` entries and the `MenuPopup` handler in
  `prelude/modeless.lua`.
- Shortcut hints from the keymap.
- Docs in `docs/features/modeless.md`.

### Phase 4: hover from the clients, example, verification

- The GUI and web clients send `move` gestures while a menu is open.
- `examples/modeless/` gets a context-menu section and spec cases.
- Verify in the GUI and in the browser (`verify-modeless.mjs`).

## Status

All four phases are in. The terminal client sends no hover moves: modeless editing
is refused there, and a picker doesn't use hover. The browser checks
(`verify-*.mjs`) weren't run on the Windows machine this was built on, which has no
emscripten toolchain or Playwright.

## Tests

- `crates/bemtvi-server/tests/modeless.rs`: the right-click selection rules,
  `MenuPopup` firing with the click in `getmousepos()`, and the full gesture of
  right-click, menu at the pointer, click a row, verb runs.
- `crates/bemtvi-server/tests/ui_menu.rs`: `btv.ui.menu` placement,
  flipping, disabled / separator skipping, one-click confirm, right-press
  cancel and reopen, hover.
- The example spec under `examples/modeless/test/`.
