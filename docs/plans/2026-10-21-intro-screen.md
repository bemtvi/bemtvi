# The intro screen

**Status:** Done (both phases) · **Date:** 2026-10-21

Goal: start bemtvi with no file and see an intro screen, the way vim and neovim do: the
app icon drawn in the terminal, the name and version, and the few commands a newcomer
needs. It disappears the moment the editor is used.

## The logo, as text

The icon is pixel art (`crates/bemtvi-gui/assets/icon24.png`, 24×24 with the art in its
top-left 23×23). Each text cell shows two vertical pixels with the upper half block
`▀`: the foreground colour is the top pixel, the background the bottom one. A cell whose
bottom pixel is transparent keeps `▀` with no background, one whose top pixel is
transparent uses `▄` instead, and a cell with both transparent is a space, so the editor
background shows through. 23×23 pixels become 23 columns × 12 rows.

Every pixel keeps its exact colour. The art has 253 distinct colours (subtle per-pixel
variation); quantizing to 12–24 colours visibly loses the grey beak and turns the white
brow cream. Exact colours need one highlight group per distinct top/bottom pair, at most
276, which the registry handles fine.

The pixel data is generated from the PNG, the single source: `gen-icons.py` (which already
derives every other icon size) also writes `prelude/intro_logo.lua`, a palette plus one
string of palette indices per pixel row. Changing the icon and rerunning the script updates
the intro.

## The surface: virtual lines on the startup buffer

The intro is drawn with `virt_lines` extmarks on the empty `[No Name]` buffer, in its own
namespace, so the buffer itself is never modified. All three clients paint a chunk's fg
and bg (TUI and GUI measure `▀` as one cell; the web relies on its monospace font). The
virtual lines sit under the buffer's single line, so the cursor stays on line 1, as in
neovim, and the block is centred in the text area below it.

It is a bundled prelude module (`prelude/intro.lua`) on public `btv.*` surfaces only:
`btv.buf.set_extmark`, `btv.hl.define`, `btv.on`, `btv.win.*`, `btv.ui.caps()`.

## When it shows, and when it goes

Shown at `UIEnter`, the first point where the window size and the client's capabilities
are known, when all of these hold:

- the editor was started with no file arguments (`btv.argv()` is empty) and the current
  buffer is still the unnamed, empty, unmodified one (a restored session or a config that
  opened something keeps its buffers);
- the config did not opt out: `btv.g.intro = false`;
- the text area is big enough for it (otherwise it is skipped, as vim does).

On a client without truecolor the logo is left out and only the text is shown: the
colours are 24-bit, and a downgraded version would not read as the icon.

It is drawn in the startup window, recorded when the intro is armed (before `init.lua`),
not in whatever has focus at `UIEnter`: on a first run the plugin offer is a focused float
by then, and the intro sits behind it.

`:intro` (`btv.intro.show()`) draws it on demand, as vim's `:intro` does, in any
session and despite `btv.g.intro = false`, but only on an empty, unnamed buffer: it is
drawn below that buffer's line, never over text, so elsewhere it refuses and points at
`:enew`. `btv.intro.dismiss()` clears it.

It is cleared, for good, when the buffer changes, when another buffer takes the window,
when `:e file` reuses the startup buffer (`BufReadPost` / `BufNewFile` / `BufFilePost`),
or when a split shows the startup buffer twice. A float or a dock showing its own buffer
leaves it be. It is re-centred on `WinResized` and redrawn on `ColorScheme` (a scheme
that opens with `hi clear` drops the logo's groups).

Two core changes came with it. `btv.win.textoff(win)` reports the gutter width (fold +
sign + number columns, neovim's `getwininfo().textoff`), so the intro centres in the text
area rather than the whole window. And undo no longer swaps in the snapshotted extmark
store: it restores where marks sit but not which marks exist (neovim's model), so a mark
deleted through the API stays deleted and one added after an edit survives undoing it.
Before, undoing the first keystroke brought a dismissed intro back, and the same bug
resurrected any plugin's cleared marks.

The text under the logo:

```
                bemtvi
            version 0.1.0

 type  :q<Enter>          to exit
 type  :Plugins<Enter>    to manage plugins
```

plus a `:help` line only when the `bemtvi-help` plugin is installed (`:help` is
otherwise a pointer to that plugin). Text colours come from highlight groups linked to
the active scheme's (`Title`, `Comment`, `Special`), so they follow any colorscheme.

## Phases

1. **Logo data + the intro module.** `gen-icons.py` emits `intro_logo.lua`; `intro.lua`
   draws, centres, dismisses and re-centres. Server integration tests: shown with no file,
   absent with a file argument, absent with `btv.g.intro = false`, cleared on an edit and on
   `:e other`, the text-only variant without truecolor, re-centred on resize.
2. **Example, docs, and every client.** `:intro` and `btv.intro.show()` / `dismiss()`
   (the example's spec needs the intro on demand: the test runner never arms the startup
   one), `examples/intro/` with its spec, a section in the configuration guide, and a
   visual check in the TUI, the GUI and the browser build. The GUI and the web drew `▀`
   as a font glyph, which left seams between rows (their cells are taller than the em);
   both now draw block elements as rectangles filling the cell (landed with phase 1).
