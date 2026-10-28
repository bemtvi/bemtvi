# `'list'` and `'listchars'`: drawing invisible characters

**Status:** Phase 1 and Phase 2 done.

## Goal

vim's `'list'` (window-local bool, default off) and `'listchars'` (window-local
`key:value` list): when `list` is on, whitespace that is normally invisible is drawn
as a visible glyph. Tabs become `>---`, trailing blanks become `-`, a non-breaking
space becomes `+`, and so on. The glyphs are painted in the `Whitespace` group, and
the `eol` marker in `NonText`, as in neovim.

The default is bemtvi's own, `listchars=tab:→ ,space:·,trail:•,nbsp:◦,eol:¬`. neovim's
`tab:> ,trail:-,nbsp:+` was the first default, and in use `:set list` then changed
nothing on a file indented with spaces (no glyph for a plain space), and its glyphs
are characters real text is full of. The `bemtvi` colorscheme's `Whitespace` was also
too faint to see (1.6:1 on the background). It now links to `NonText`, as in neovim,
and the scheme's `NonText` (whose only first-party user is the `eol` marker) is
surface2, 2.1:1: overlay0 (2.9:1) was tried and competed with the text. Line ends
are drawn by default; `␣` / `↵` were avoided because Noto Sans Mono lacks them.

## Where it lives, and why it isn't a plugin

This is the frame, not a feature on top of it. The substitution changes what the
row *text* is before any client sees it, every client (TUI, GUI, web) has to show
it identically, and it has to agree with the cursor / selection / search column math
on every frame. A `btv.decor` provider could only paint overlays over cells, and it
would run per-viewport in Lua for a pure layout rule the core already has all the
inputs for (`tabstop`, the wrap segment's `start_col`, the line's end). So it goes
in Rust, at the layer that builds rows: `bemtvi-core/src/view.rs::row_skeleton`.

### The width invariant

Every substitution in Phase 1 keeps the **display width** of what it replaces:

- a tab expands to exactly as many glyphs as cells the tab would cover at its
  virtual column (computed from the full-line column, so a tab on a wrapped
  continuation row is still sized correctly);
- a space or nbsp becomes one single-width glyph;
- `eol` appends one cell past the end of the line, where nothing was drawn before.

`listchars` values must be single-width characters (vim's rule, `E474` otherwise).
So every column the projection already carries stays valid: `cursor_screen_col`,
selection / search / incsearch spans, highlight spans, `leftcol` panning,
`clip_row_text`. The rows just stop containing tabs.

### Coloring

`WindowView.listchars` carries the resolved set to the server, which recomputes the
same substitutions (`unicode::list_subs`, shared with core) over each row's buffer
head and merges them into the highlight spans as `Whitespace` byte intervals at
`SPECIAL_KEY_PRIORITY`, the way the `^X` control-char overlay works. `eol` is one
`NonText` cell appended past the line's last column. Both `highlights_for` (native)
and `overlay_highlights_for` (wasm edit-host) do this, so the web build gets it
without a new wire field. The TUI and GUI get fallback styles for `Whitespace` / `NonText` when the
colorscheme doesn't define them, and the web renderer resolves them through
`colorFor`.

## Phases

### Phase 1: core `list` + `listchars` ✅

- Options: `list` (`WindowOptions.list`, bool) and `listchars` / `lcs` (window-local
  string, validated by `parse_listchars` at set time, `E474` on junk), across all
  the usual touchpoints: catalog, `:set`, `btv.wo`/`vim.wo` bridge, `WindowMirror`,
  `state.lua` tables, the `every_known_option_is_wired_not_silent` guard.
- Keys honored: `tab:xy[z]`, `space:c`, `multispace:c…`, `lead:c`,
  `leadmultispace:c…`, `trail:c`, `nbsp:c` (U+00A0 and U+202F), `eol:c`.
  Precedence, as in neovim: `nbsp`; then `trail` for the trailing run; then
  `lead`/`leadmultispace` for the leading run; then `multispace` for runs of 2+
  spaces; then `space`.
- `showwhitespace` / `whitespacechars` are accepted everywhere `list` / `listchars`
  are: bemtvi's own names, since vim's only make sense if you know ed's `l` command.
  They are `ALIASES` entries in `options.rs`, separate from vim's abbreviations, and
  complete as their own rows.
- `extends` / `precedes` / `conceal` are accepted (so a vim config sets cleanly);
  the first two are drawn in Phase 2. Documented on the option, like `fillchars`'s
  unhonored keys.
- `list` on with **no** `tab:` entry: vim shows a tab as `^I`, a 2-cell token that
  breaks the width invariant (every cursor column after it shifts). Phase 1 leaves
  the tab as a normal tab in that case and documents the gap; Phase 2 decides it.
- Tests (server `tests/editing/`): substitution on `t:screen()`-equivalent rows for
  each key, precedence, tab widths at different columns, wrapped continuation rows,
  `nowrap` + `leftcol`, cursor column unchanged by `list`, highlight spans carry
  `Whitespace` / `NonText`; option round-trip in `tests/options.rs`; TUI screen
  test for the glyph colors (`crates/bemtvi/tests/screen.rs`); the browser check
  `crates/bemtvi-edithost/web/verify-listchars.mjs`. Example `examples/listchars/`
  with its own spec.
- Known gap, moved to Phase 2: an `eol` glyph on a wrapped line that exactly fills
  its last row is cut at the window edge.

### Phase 2: `extends` / `precedes`, the `^I` form, and `eol` on a full row ✅

- `nowrap` edge markers ✅: `extends` on the last visible column when the line has
  a cell past it (the `eol` cell counts, as in vim), `precedes` on the first when
  `leftcol` hides text and the line still reaches the window. Placement is
  `unicode::list_edge_marks`, shared by core (which draws them over the cell they
  land on: a marker inside a tab or wide char takes only its own cell, the rest go
  blank) and the server (which colors that cell `NonText`, above the `Whitespace`
  glyph). The settled view's edges ride `WindowView.list_edges`; the scroll band
  gets none, since its rows pan.
  Decision, departing from vim: **no marker on the cursor's cell.** vim draws it,
  which hid the character under the cursor whenever it rode an edge (after `$` on a
  line whose `eol` is past the edge, or stepping right with `l`).
- `eol` on a full wrapped row ✅: `unicode::window_wrap_segments` adds an empty
  last segment when the line's last row has no cell left for the glyph, from
  `WrapOpts.eol`. All eight window callers (layout, cursor row, `gj`/`gk`, the
  mouse, scrolling) go through it, so the row is counted, not only drawn; an Insert
  cursor at the line's end lands on it.
- `^I` — decided against. Drawing it makes a tab's width depend on the window's
  `'listchars'` at every site that computes a column (~45 across core and server,
  plus the clients), for a value neither default uses; a missed site misplaces the
  cursor or a highlight. bemtvi keeps the tab a plain tab and documents it.
- Found on the way, not taken: `:set` has no `+=` / `-=` / `^=` for any option
  (`E518`, loud). `vim.opt.listchars:append()` covers the Lua side.

## Not in scope

- `conceal` interaction (bemtvi has no `'conceallevel'` rendering of syntax
  conceal yet).
- A `btv.*` API beyond the options themselves: `btv.wo.list` / `btv.wo.listchars`
  are the API.
