-- Markdown rendering for doc popups.
--
-- nxvim renders markdown in its read-only doc popups — LSP hover (press `K` over a
-- symbol in an LSP-backed buffer), completion documentation, and signature help all
-- show *rendered* markdown (bold/headings styled, `#`/`**`/fences stripped) instead
-- of raw markdown text. That happens natively; no config is needed.
--
-- This example demonstrates the same engine you can call yourself — `nx.markdown.render`
-- — by rendering the CURRENT BUFFER's markdown into a styled popup float built from
-- public APIs (`nx.markdown.render` + `nx.ui.float`). Open `sample.md` and press `K`
-- (or run `:MarkdownFloat`) to see it; the next key dismisses the popup.
--
--   Run:  cargo run -p nxvim -- --config-dir examples/markdown examples/markdown/sample.md

-- Turn one rendered line + its char-column→group map into an `nx.ui.float` chunk list
-- (`{ {text, hl_group?}, … }`), coalescing runs of equal group so each styled span is
-- one chunk. Columns from `nx.markdown.render` are 1-based char columns.
local function line_to_chunks(line, col_group)
  local chunks, buf, buf_group, col = {}, {}, nil, 0
  for _, code in utf8.codes(line) do
    col = col + 1
    local group = col_group[col]
    if group ~= buf_group and #buf > 0 then
      chunks[#chunks + 1] = { table.concat(buf), buf_group }
      buf = {}
    end
    buf_group = group
    buf[#buf + 1] = utf8.char(code)
  end
  if #buf > 0 then
    chunks[#chunks + 1] = { table.concat(buf), buf_group }
  end
  -- A blank line still needs a (blank) chunk so the row is kept.
  if #chunks == 0 then
    chunks = { { "", nil } }
  end
  return chunks
end

-- Render markdown `src` into the styled chunk lines `nx.ui.float` draws.
local function markdown_to_float_lines(src)
  local r = nx.markdown.render(src)

  -- Bucket every highlight span into a per-line char-column → group map (later spans
  -- win where they overlap, e.g. bold inside a heading).
  local per_line = {}
  for _, h in ipairs(r.highlights) do
    local map = per_line[h.line]
    if not map then
      map = {}
      per_line[h.line] = map
    end
    for c = h.col_start, h.col_end - 1 do
      map[c] = h.group
    end
  end

  -- Whole-line rules (thematic breaks / table separators) render as a repeated glyph.
  local fill_at = {}
  for _, f in ipairs(r.fills) do
    fill_at[f.line] = f
  end

  local out = {}
  for i, line in ipairs(r.lines) do
    if fill_at[i] then
      out[i] = { { string.rep(fill_at[i].char, 48), fill_at[i].group } }
    else
      out[i] = line_to_chunks(line, per_line[i] or {})
    end
  end
  return out
end

-- Render the current buffer's markdown into a transient popup float at the cursor.
local function markdown_float()
  local lines = vim.api.nvim_buf_get_lines(0, 0, -1, false)
  local rendered = markdown_to_float_lines(table.concat(lines, "\n"))
  nx.ui.float(rendered, { title = " rendered markdown ", border = "rounded" })
end

-- Expose it as both a key (over the buffer) and a command.
nx.keymap.set("n", "K", markdown_float, { desc = "Render this buffer's markdown in a popup" })
nx.command("MarkdownFloat", markdown_float, { desc = "Render the current buffer's markdown in a popup" })
