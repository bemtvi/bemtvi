-- bemtvi:prelude/markdown — the btv.markdown.* surface: turn a markdown string into
-- rendered, styled display lines. This is the same pure CommonMark+GFM renderer the
-- editor uses for LSP hover / completion docs / picker previews (over the native
-- btv._markdown_render bridge), exposed so plugins can render markdown into their own
-- floats/buffers without reimplementing a parser.
--
-- Available on every build (native and browser/wasm) — the renderer is pure Rust in
-- bemtvi-core with no editor state or I/O.

btv.markdown = btv.markdown or {}

-- btv.markdown.render(src) -> { lines = {string,..}, highlights = { hl, .. },
--                             fills = { fill, .. }, code = { block, .. } }
--
-- Parse `src` (CommonMark + GFM: tables, strikethrough, task lists) into stripped
-- display lines with the markup syntax removed (`**bold**` -> `bold`, `# Title` ->
-- `Title`, ` ``` ` fences dropped, `- x` -> `• x`, `> q` -> `▎ q`, `- [ ]` -> `☐`),
-- plus the styling to paint over them. Each highlight is a table:
--
-- ```
-- { line = <1-based line>, col_start = <1-based char col>,
--   col_end = <exclusive char col>, group = "<@markup.* capture>" }
-- ```
--
-- Columns are CHARACTER columns (not bytes), so they index `lines[hl.line]` the way
-- Lua `string.sub` / display code counts. `group` is a neovim `@markup.*` treesitter
-- capture (`@markup.strong`, `@markup.heading.1`, `@markup.raw`, `@markup.link.label`,
-- `@markup.quote`, `@markup.list`, …), so a colorscheme that styles treesitter
-- markdown styles these identically.
--
-- `fills` are row rules — a thematic break (`---`) or a GFM table's header separator:
-- each `{ line = <1-based>, char = "─", group = "<@markup.*>" }` means "repeat `char`
-- from the end of `lines[line]`'s text to the right edge". Those lines are blank, so
-- the rule spans the row; render it as a run out to your surface's width.
--
-- `code` are the fenced code blocks, each
-- `{ first_line = <1-based>, last_line = <1-based, inclusive>, lang = "<fence language>"? }`
-- — `lang` is absent for a bare ` ``` ` fence. Use it to back a block as a code region
-- (set `line_hl_group = "@markup.raw.block"` on `first_line..last_line` — the doc-float
-- look) or to syntax-highlight its body in `lang`.
--
-- Pure and infallible: unsupported constructs still contribute their text.
function btv.markdown.render(src)
  return btv._markdown_render(src or "")
end

-- `btv.markdown.render` reports styling in 1-based CHARACTER columns; extmarks take
-- 0-based BYTE columns. Convert the `c`-th character boundary (1-based; `c` may be one
-- past the last char, an exclusive end) to its 0-based byte offset in `line`.
local function char_to_byte(line, c)
  return (utf8.offset(line, c) or (#line + 1)) - 1
end

-- btv.markdown.to_view(src[, opts]) -> { lines = {string,..}, decor = { mark, .. } }
--
-- Turn markdown `src` into **view-ready** content: the display `lines` plus the `decor`
-- extmarks that style them — exactly the `{ lines, decor }` shape an `btv.view.component`
-- `render` returns (or hand `lines` to `view:set_lines` and `decor` to `view:set_decor`).
-- The higher-level companion to `btv.markdown.render`: `render` gives the raw pieces,
-- `to_view` assembles them into something you can drop straight onto a surface.
--
--   * Prose is *rendered*: the stripped lines, styled with `render`'s `@markup.*` spans
--     (as ranged `hl_group` extmarks in byte columns).
--   * Thematic breaks / table separators become a full-line rule glyph.
--   * Fenced code blocks are LEFT as raw ` ``` ` fences (so tree-sitter can highlight
--     them — see below), backed with a full-width `line_hl_group` code background, and
--     their fence delimiter lines are hidden behind a blanking `virt_text` overlay so the
--     block reads as rendered.
--
-- IMPORTANT — code-block syntax highlighting: the fences are kept on purpose. Mount the
-- surface with `filetype = "markdown"` (e.g.
-- `btv.view.component{...}:mount{ filetype = "markdown", … }`) so the markdown grammar's
-- injections highlight each fenced block in its own language. Without that `filetype` the
-- code still shows (backed, fences hidden) but unhighlighted. Per-language highlighting
-- needs that language's grammar installed.
--
-- `opts` (all optional):
--   * `rule_width` (default 80): cells a thematic-break / table-separator rule spans.
--   * `code_hl_group` (default `"@markup.raw.block"`): the code-block background group.
--
-- Pure Lua over `btv.markdown.render`; no editor state, so it runs on every build.
function btv.markdown.to_view(src, opts)
  opts = opts or {}
  local rule_width = opts.rule_width or 80
  local code_group = opts.code_hl_group or "@markup.raw.block"
  local r = btv.markdown.render(src)

  -- Where each fenced block opens, and which stripped lines are code (prose-only spans
  -- skip these — the injected grammar highlights the code body).
  local block_start, in_code = {}, {}
  for _, b in ipairs(r.code) do
    block_start[b.first_line] = b
    for l = b.first_line, b.last_line do
      in_code[l] = true
    end
  end
  local fill_at = {}
  for _, f in ipairs(r.fills) do
    fill_at[f.line] = f
  end

  local lines, decor = {}, {}
  local out_of = {} -- stripped line (1-based) -> output row (1-based); fences shift rows
  local function push(line)
    lines[#lines + 1] = line
    return #lines
  end

  local i, n = 1, #r.lines
  while i <= n do
    local b = block_start[i]
    if b then
      -- Re-wrap the block in its ``` fences so injection fires, back the whole block as a
      -- code region, and hide the two fence delimiter lines behind a blanking overlay.
      local open_row = push("```" .. (b.lang or ""))
      for l = b.first_line, b.last_line do
        out_of[l] = push(r.lines[l])
      end
      local close_row = push("```")
      for row = open_row, close_row do
        decor[#decor + 1] = { line = row - 1, col = 0, line_hl_group = code_group }
      end
      for _, fr in ipairs({ open_row, close_row }) do
        decor[#decor + 1] = {
          line = fr - 1,
          col = 0,
          virt_text = { { string.rep(" ", #lines[fr]), code_group } },
          virt_text_pos = "overlay",
        }
      end
      i = b.last_line + 1
    else
      local f = fill_at[i]
      if f then
        -- A fill runs from the end of the line's own text to the right edge: blank for a
        -- thematic break / table separator (so the rule is the whole row), text for a
        -- labelled section rule (`─ name ─────`), whose label stays and keeps its span.
        local text = r.lines[i]
        local chars = utf8.len(text) or #text
        local rule = text .. string.rep(f.char, math.max(rule_width - chars, 0))
        out_of[i] = push(rule)
        decor[#decor + 1] = {
          line = out_of[i] - 1,
          col = char_to_byte(rule, chars + 1),
          end_row = out_of[i] - 1,
          end_col = #rule,
          hl_group = f.group,
        }
      else
        out_of[i] = push(r.lines[i])
      end
      i = i + 1
    end
  end

  -- Inline prose spans, remapped to their output row (code lines handled by injection).
  for _, h in ipairs(r.highlights) do
    if not in_code[h.line] then
      local row = out_of[h.line]
      local text = r.lines[h.line]
      decor[#decor + 1] = {
        line = row - 1,
        col = char_to_byte(text, h.col_start),
        end_row = row - 1,
        end_col = char_to_byte(text, h.col_end),
        hl_group = h.group,
      }
    end
  end

  return { lines = lines, decor = decor }
end
