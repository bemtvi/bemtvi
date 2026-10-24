# Font fallback chains, wezterm-style

Status: done (2026-10-24)

## Goal

`'guifont'` names a list of fonts, tried in order for every character, in the GUI
and in the browser alike. Each font in the list can carry its own weight, size and
OpenType features. A font that isn't installed, or an option that makes no sense,
is reported on the message line instead of being skipped without a word.

```lua
btv.ui.font({
  { "JetBrains Mono", weight = 500, features = { "+ss01", "-calt" } },
  { "Symbols Nerd Font", scale = 0.9 },
  "Noto Color Emoji",
}, { size = 14 })
-- writes: guifont=JetBrains Mono[weight=500;features=+ss01 -calt],Symbols Nerd Font[scale=0.9],Noto Color Emoji:h14
```

Bundling fonts (wezterm ships JetBrains Mono, Nerd symbols and Noto emoji) is out:
the list uses what is installed.

## Where things stand

- The GUI takes a comma list (`guifont`, `--font`, `BEMTVI_GUI_FONT`). The first
  font shapes the text; the rest are prepended to cosmic-text's *common* fallback
  (`UserFallback`, `render.rs`). cosmic-text consults that list only for characters
  with no per-script platform list, so for CJK, Arabic and other real scripts the
  platform's font wins over the user's.
- The GUI parses `guifont` itself (`parse_guifont`); the core stores the string and
  checks nothing. `:b`/`:i` and other `:` flags are accepted and ignored.
- A family that isn't installed is skipped silently.
- The browser never reads `guifont`.

## Design

**The core owns the syntax.** `bemtvi_core::guifont` parses `'guifont'` into a
`GuiFont { fonts: Vec<FontSpec>, size: Option<f32> }`, where a `FontSpec` is a
family plus `weight: Option<u16>`, `scale: Option<f32>`, `features: Vec<Feature>`.
`:set guifont=…` (and `btv.o.guifont`) rejects a malformed value with `E474`, naming
the bad part. The redraw relays the parsed form (`guifont_spec`) beside the string,
so neither client parses anything. The extended syntax keeps vim/Neovide's: a comma
list, then `:h<size>`; per-font options go in brackets after a family, `;`-separated:

```
Family[weight=500;scale=0.9;features=+ss01 -calt cv01=2],Other:h14
```

`weight` is 100–900 (or `bold`, `normal`), `scale` a factor in (0, 4], `features`
space-separated `+tag`, `-tag`, `tag` or `tag=N` with a four-letter tag. An unknown
key is `E474`. Trailing `:` flags other than `:h` stay accepted and ignored, as in
Neovide.

**The GUI resolves fonts per character.** On `set_font` the renderer loads each
configured font's character coverage (as `ItalicFace` does for italics). Shaping
splits each run so every grapheme goes to the first configured font that has its
base character, with that font's family pinned and its options applied: weight for
non-bold text (bold stays bold), features via `Attrs::font_features`, scale via a
per-run `metrics_opt`. A grapheme no configured font covers keeps the primary's
attrs, and cosmic-text's platform fallback finds it as today. This makes the order
strictly the user's for every script, so `UserFallback` goes back to the platform
list. A scaled glyph that no longer fits its cell takes the existing mask-and-place
path, which shrinks it to its box unless `'guiglyphoverflow'` lets it spill.

**Missing fonts are reported.** After `set_font` the GUI checks each family against
the font database (family names, case-insensitive) and reports the missing ones
once per `guifont` value with `btv.notify` at WARN:
`guifont: not installed: Fira Kode`. The browser does the same with the classic
measurement test (a string's width in `'X', monospace` against `monospace`, and
against `serif`).

**The browser** loads each font in the list as a `FontFace` over `local()` sources,
aliased `btv-font-N`, then sets the grid's `font-family` to the aliases (then
`monospace`) and its size from `:h`, re-measuring the cell and re-reporting the grid
like a resize. CSS already tries the list in order per character. The `local()`
sources name the face for the font's weight (its family, its full name `Family
Style`, its PostScript name `Family-Style`), so weight applies, and its scale rides
as `size-adjust`. Whether the load succeeds also says whether the font is installed;
when no `local()` name matches, a canvas width test decides whether the family
renders at all by name (then it's used without its weight or scale, and that's
reported). OpenType features can't be set per face in Chrome, so the primary's
features apply to the whole grid, which is where they are almost always wanted, and
features on a later font are reported as not applied in the browser. Reports go to
the message line once per value, as in the GUI.

## Phases

1. **The syntax in the core.** `bemtvi_core::guifont`, `E474` on `:set`, the parsed
   relay, `btv.ui.font`, the GUI reading the relayed spec instead of parsing (no
   change in what it draws). Tests: the server's `E474` cases and the relayed spec.
2. **Per-character fonts in the GUI.** Coverage, run splitting, weight / features /
   scale per font, strict order, the missing-font warning. Tests: tiny fonts
   generated with fonttools and committed under `crates/bemtvi-gui/tests/fonts/`
   (each covers a known handful of characters), driving the pure resolution and a
   real shape through a `FontSystem` over just those files, no GPU.
3. **The browser.** The `FontFace` loading, size, cell re-measure, the primary's
   features, and the missing-font and not-applied reports. Test: a
   `verify-guifont.mjs` Playwright check, which skips on a machine without its two
   fonts.
4. **Docs and example.** A `docs/features/fonts.md` page, the book entry, and
   `examples/fonts/` with its spec.

## Outcome

All four phases landed as planned, with these differences:

- **The browser applies more than planned.** A `FontFace` over `local()` sources
  picks a font's face for its weight and carries its scale as `size-adjust`, so
  only per-font OpenType features stay GUI-only. The primary's apply to the grid.
- **Coverage is read from the font's cmap** (`ttf-parser`). cosmic-text's
  `Font::unicode_codepoints` is filled only for fonts with both GPOS and GSUB
  tables, which also left italic off for an italic face missing either. Italic now
  uses the same read.
- **A missing first font gives its place to the system monospace**, not to the next
  font in the list, which may be an emoji font that covers digits.
- **`:h` with a bad size is `E474`** where the GUI used to ignore it.
- **The GUI's message-line report** for a missing font has no automated test: it
  needs a running window. The resolution it reports from is tested.

