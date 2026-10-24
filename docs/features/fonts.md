# Fonts

`'guifont'` is a list of fonts. For every character, the GUI and the browser use the
first font in the list that has it, the way wezterm's `font_with_fallback` does. So
a coding font can come first, a Nerd Font symbols font next for icons, then a CJK
font and an emoji font, and each character comes from the right one. The order is
yours for every script: a CJK font you list beats the one the system would pick.

```lua
btv.ui.font({
  { "JetBrains Mono", features = { "+zero", "-calt" } },
  { "Symbols Nerd Font Mono", scale = 0.9 },
  "Noto Sans CJK JP",
  "Noto Color Emoji",
}, { size = 14 })
```

That writes the option, which you can also set directly:

```vim
:set guifont=JetBrains\ Mono[features=+zero\ -calt],Symbols\ Nerd\ Font\ Mono[scale=0.9]:h14
```

The `--font` flag and `BEMTVI_GUI_FONT` take the same syntax, for a GUI started
without a config. `'guifont'` wins over them once a config sets it.

## Each font's options

Options go in brackets after a font's name, separated by `;`:

| Option | Value | Does |
| --- | --- | --- |
| `weight` | 1–1000, or `thin`, `extralight`, `light`, `normal`, `medium`, `semibold`, `bold`, `extrabold`, `black` | The weight of its regular text. Bold text stays bold. |
| `scale` | a factor in (0, 4] | Draws its glyphs that much bigger or smaller. A glyph that no longer fits its cell is shrunk to fit unless `'guiglyphoverflow'` lets it spill into the next cell. |
| `features` | space-separated `+tag`, `-tag`, `tag` or `tag=N` | OpenType features: `+ss01` turns a stylistic set on, `-calt` turns contextual alternates (most ligatures) off, `cv01=2` picks an alternate. |

After the list, `:h<size>` sets the size. Other `:` flags (`:b`, `:i`, `:w…`, `:#e-…`)
are accepted and ignored, as in Neovide.

## When something is wrong

A malformed value is refused, whichever way you set it (`:set`, `btv.o`,
`btv.ui.font`), and the font you had stays:

```
E474: Invalid argument: guifont: unknown option wieght on JetBrains Mono
```

A font that isn't installed is skipped and named on the message line:
`guifont: not installed: Fira Kode`. If it was the first in the list, the system
monospace takes its place, so plain text never comes from an icon or emoji font
further down.

## In the browser

The browser takes the list, the size and each font's weight and scale. It can't set
OpenType features per font, so the first font's features apply to all the text,
which is where they're almost always wanted. Features on a later font are reported
on the message line as not applied in the browser.

## Limits

- The terminal client draws with the terminal's own fonts and ignores the option.
- No fonts are bundled: the list uses what's installed.
