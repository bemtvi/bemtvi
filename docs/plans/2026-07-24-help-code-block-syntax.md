# Per-language syntax highlighting inside help code blocks

Goal: `>lua` / `>vim` / `>{lang}` code fences in vim help render with real
per-language token highlighting (keywords, strings, …), matching neovim's
tree-sitter injection — in **both** surfaces:

- the picker **preview pane** (server tree-sitter, `bemtvi-ts`), and
- the **help window** itself (the `bemtvi-help` plugin's `btv.view`).

Today both surfaces paint code blocks in a single flat colour: the preview via
vimdoc's `@markup.raw.block` capture (→ `@markup.raw`), the window via the
plugin's `btvHelpCode` (→ `String`) extmark. Neither runs the injected language.

Related just-shipped fix (separate, already landed): the preview pane now
expands tabs to spaces (`expand_preview_tabs` in `redraw.rs`) so tab-indented
code blocks keep their indentation. This plan is only about *colour*.

## Phase 1 — preview pane injection (server, self-contained)

`SyntaxEngine::highlight_text` (the stateless off-buffer highlighter behind the
preview) parses only the host grammar — "no injections" by construction. Extend
it to build injected child layers the same way the buffer path
(`highlights` / `build_injection_layers`) does, but statelessly (no `BufferId`,
no incremental reuse):

1. Parse the host tree (as today).
2. Run the host's `injections.scm` via the existing free fn
   `collect_injection_regions` → `(language, ranges)` sets.
3. For each region, lazily load the child grammar and parse it restricted to the
   ranges (`set_included_ranges`, ranges merged/sorted like the buffer path).
   Bounded BFS to `MAX_INJECTION_DEPTH`, whole pass under `INJECTION_DEADLINE`.
4. Hand `[host, children…]` to the existing `extract_spans` — it already layers
   deeper-wins, so the injected tokens paint over the block background.

The child `Tree`s are kept in an owned `Vec` for the duration so the `Layer`
borrows stay valid. No new public API; `preview_highlights` is unchanged.

Result: a `>lua` block in a help preview shows lua tokens; a bare `>` block
stays flat `@markup.raw.block` (neovim-faithful — no language, no injection).
This also lights up every *other* injected preview (markdown fences, etc.) for
free.

Test: extend the picker preview suite — a `doc/*.txt` with a `>lua` block whose
preview spans include a lua capture (e.g. `keyword`/`function.call`) on the code
line, distinct from the block group. Grammar-gated (skip-if `lua` parser
absent), mirroring the existing `help_doc_preview_resolves_to_vimdoc`.

## Phase 2 — help window injection (Lua API + plugin)

The window is an `btv.view` highlighted by the plugin's own extmarks (core has no
conceal, so the plugin rewrites the `>`/`<` markers out of the displayed text).
Give the plugin a native way to get per-language spans for a snippet:

1. **New `btv.treesitter.highlight(lang, text)` Lua API** → returns per-line
   highlight spans (`{ row, start_col, end_col, group }`), backed by the same
   `Editor::preview_highlights` engine call Phase 1 hardens. Must work over the
   daemon / wasm (tier-1): the highlight runs core-side already, so the verb is
   a request/response like the other `btv.treesitter.*` surfaces — carry the
   `(lang, text)` to the core call and ship the spans back.
2. **`render.lua`**: record each code block's fence *language* (`>lua` → `lua`)
   and its 0-based row range (currently only a `code` row-set is kept; the
   language is discarded in `strip_start`).
3. **`highlight.lua`**: for each `>{lang}` block, extract the code rows' text,
   call `btv.treesitter.highlight(lang, text)`, and place the returned spans as
   extmarks over the code rows (their displayed text == raw text — only the
   fence *marker* lines are rewritten — so snippet byte offsets map straight to
   columns). A bare `>` block keeps the flat `btvHelpCode` paint.

Ship in the canonical plugin repo `~/work/nxvim-plugins/nxvim-help` + bump the
pin in `build-plugins.sh`. Cover with a plugin `--test-plugin` spec asserting a
`>lua` block's code row carries a lua-capture extmark, and add a runnable
`examples/` demo doc.

Commit + pause between phases.
