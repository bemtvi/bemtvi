-- nxvim:prelude/markdown — the nx.markdown.* surface: turn a markdown string into
-- rendered, styled display lines. This is the same pure CommonMark+GFM renderer the
-- editor uses for LSP hover / completion docs / picker previews (over the native
-- nx._markdown_render bridge), exposed so plugins can render markdown into their own
-- floats/buffers without reimplementing a parser.
--
-- Available on every build (native and browser/wasm) — the renderer is pure Rust in
-- nxvim-core with no editor state or I/O.

nx.markdown = nx.markdown or {}

-- nx.markdown.render(src) -> { lines = {string,..}, highlights = { hl, .. },
--                             fills = { fill, .. } }
--
-- Parse `src` (CommonMark + GFM: tables, strikethrough, task lists) into stripped
-- display lines with the markup syntax removed (`**bold**` -> `bold`, `# Title` ->
-- `Title`, ` ``` ` fences dropped, `- x` -> `• x`, `> q` -> `▎ q`, `- [ ]` -> `☐`),
-- plus the styling to paint over them. Each highlight is a table:
--
--   { line = <1-based line>, col_start = <1-based char col>,
--     col_end = <exclusive char col>, group = "<@markup.* capture>" }
--
-- Columns are CHARACTER columns (not bytes), so they index `lines[hl.line]` the way
-- Lua string.sub / display code counts. `group` is a neovim `@markup.*` treesitter
-- capture (`@markup.strong`, `@markup.heading.1`, `@markup.raw`, `@markup.link.label`,
-- `@markup.quote`, `@markup.list`, …), so a colorscheme that styles treesitter
-- markdown styles these identically.
--
-- `fills` are whole-line rules — a thematic break (`---`) or a GFM table's header
-- separator: each `{ line = <1-based>, char = "─", group = "<@markup.*>" }` means
-- "repeat `char` across the width of `lines[line]`". Render them as a full-width run.
--
-- Pure and infallible: unsupported constructs still contribute their text.
function nx.markdown.render(src)
  return nx._markdown_render(src or "")
end
