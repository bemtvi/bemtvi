-- nxtree.icons — the extension/name → { glyph, hl } registry.
--
-- A pure-Lua lookup table seeded with common file kinds plus the two folder
-- glyphs. `get(node)` returns the glyph string and its highlight group for one
-- tree node; `register(map)` extends the table at runtime (the extensibility
-- seam, surfaced as `nxtree.register_icons`). The highlight groups themselves are
-- declared in `M.highlights` and defined once by `nxtree.setup` via `nx.hl.define`
-- — icons.lua never touches the editor, it only describes.
--
-- Glyphs are Nerd-Font codepoints (3 UTF-8 bytes each); render.lua measures them
-- with `#glyph` so the decoration column math is byte-exact regardless.

local M = {}

-- Highlight palette. setup() walks this and calls nx.hl.define for each. Colors
-- are Catppuccin-ish so the tree reads well on the default colorscheme.
M.highlights = {
  NxTreeFolder = { fg = "#89b4fa", bold = true },
  NxTreeRootName = { fg = "#f9e2af", bold = true },
  NxTreeDir = { fg = "#89b4fa" },
  NxTreeFile = { fg = "#cdd6f4" },
  NxTreeLink = { fg = "#94e2d5", italic = true },
  NxTreeIndent = { fg = "#45475a" },
  NxTreeIconRust = { fg = "#fab387" },
  NxTreeIconLua = { fg = "#74c7ec" },
  NxTreeIconJs = { fg = "#f9e2af" },
  NxTreeIconTs = { fg = "#89b4fa" },
  NxTreeIconMd = { fg = "#cdd6f4" },
  NxTreeIconJson = { fg = "#f9e2af" },
  NxTreeIconToml = { fg = "#fab387" },
  NxTreeIconPy = { fg = "#f9e2af" },
  NxTreeIconGo = { fg = "#89dceb" },
  NxTreeIconSh = { fg = "#a6e3a1" },
  NxTreeIconGit = { fg = "#f38ba8" },
  NxTreeIconText = { fg = "#bac2de" },
  NxTreeIconDefault = { fg = "#9399b2" },
}

local FOLDER_CLOSED = ""
local FOLDER_OPEN = ""
local FILE_DEFAULT = ""

-- Lookup by exact filename (highest priority), then by lowercased extension.
local by_name = {
  ["Cargo.toml"] = { glyph = "", hl = "NxTreeIconRust" },
  ["Cargo.lock"] = { glyph = "", hl = "NxTreeIconRust" },
  [".gitignore"] = { glyph = "", hl = "NxTreeIconGit" },
  [".gitattributes"] = { glyph = "", hl = "NxTreeIconGit" },
  ["README.md"] = { glyph = "", hl = "NxTreeIconMd" },
  ["Makefile"] = { glyph = "", hl = "NxTreeIconDefault" },
}

local by_ext = {
  rs = { glyph = "", hl = "NxTreeIconRust" },
  lua = { glyph = "", hl = "NxTreeIconLua" },
  js = { glyph = "", hl = "NxTreeIconJs" },
  mjs = { glyph = "", hl = "NxTreeIconJs" },
  ts = { glyph = "", hl = "NxTreeIconTs" },
  md = { glyph = "", hl = "NxTreeIconMd" },
  json = { glyph = "", hl = "NxTreeIconJson" },
  toml = { glyph = "", hl = "NxTreeIconToml" },
  py = { glyph = "", hl = "NxTreeIconPy" },
  go = { glyph = "", hl = "NxTreeIconGo" },
  sh = { glyph = "", hl = "NxTreeIconSh" },
  bash = { glyph = "", hl = "NxTreeIconSh" },
  txt = { glyph = "", hl = "NxTreeIconText" },
}

-- get(node) -> glyph, hl_group. Directories use the open/closed folder glyph keyed
-- off `node.expanded`; files resolve by exact name then extension, else a default.
function M.get(node)
  if node.type == "directory" then
    return (node.expanded and FOLDER_OPEN or FOLDER_CLOSED), "NxTreeFolder"
  end
  local exact = by_name[node.name]
  if exact then
    return exact.glyph, exact.hl
  end
  local ext = node.name:match("%.([%w]+)$")
  local e = ext and by_ext[ext:lower()]
  if e then
    return e.glyph, e.hl
  end
  return FILE_DEFAULT, "NxTreeIconDefault"
end

-- register(map) — extend the extension table. `map` is `{ ext = { glyph=, hl= }, … }`;
-- a `name = { … }` sub-table (optional) extends the exact-name table. Highlight
-- groups referenced must already be defined (or added to M.highlights before setup).
function M.register(map)
  for k, v in pairs(map or {}) do
    if k == "name" then
      for n, spec in pairs(v) do
        by_name[n] = spec
      end
    else
      by_ext[k] = v
    end
  end
end

return M
