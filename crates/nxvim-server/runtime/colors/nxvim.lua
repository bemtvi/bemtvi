-- nxvim — the editor's own built-in colorscheme: `:colorscheme nxvim`.
--
-- A One Dark palette, the truecolor sibling of the look the GUI and the
-- browser edit-host ship by default, so a bare terminal `:colorscheme nxvim`
-- lands on the same colors. It is bundled in the binary (sourced from the
-- embedded runtime, not the runtimepath), so it loads with zero user config; a
-- user `colors/nxvim.lua` on the runtimepath still shadows it.
--
-- Written against the canonical `nx.hl.define(ns, name, opts)` highlight setter
-- (ADR 0002) — the same call `vim.api.nvim_set_hl` aliases.

local p = {
  bg          = "#282c34", -- Normal background
  bg_dark     = "#21252b", -- status line / darker chrome
  cursor_line = "#2c313a",
  gutter      = "#4b5263", -- inactive line numbers
  fg          = "#abb2bf", -- Normal foreground / default variable
  comment     = "#5c6370",
  visual      = "#3e4451",
  red         = "#e06c75", -- properties, fields, tags, builtin variables
  green       = "#98c379", -- strings
  orange      = "#d19a66", -- numbers, constants, booleans
  yellow      = "#e5c07b", -- types, constructors, attributes
  blue        = "#61afef", -- functions, labels
  purple      = "#c678dd", -- keywords
  cyan        = "#56b6c2", -- operators, escapes, builtin functions
}

local hl = nx.hl.define

-- Editor chrome (the regions the renderer resolves: Normal, LineNr,
-- CursorLineNr, Visual, Search, IncSearch, StatusLine, EndOfBuffer — plus a few
-- common extras for completeness).
hl(0, "Normal",       { fg = p.fg, bg = p.bg })
hl(0, "NormalFloat",  { fg = p.fg, bg = p.bg_dark })
hl(0, "LineNr",       { fg = p.gutter })
hl(0, "CursorLine",   { bg = p.cursor_line })
hl(0, "CursorLineNr", { fg = p.fg })
hl(0, "Visual",       { bg = p.visual })
hl(0, "Search",       { fg = p.bg, bg = p.yellow })
hl(0, "IncSearch",    { fg = p.bg, bg = p.orange })
hl(0, "CurSearch",    { fg = p.bg, bg = p.orange })
hl(0, "StatusLine",   { fg = p.fg, bg = p.bg_dark })
hl(0, "EndOfBuffer",  { fg = p.bg })
hl(0, "SignColumn",   { bg = p.bg })
hl(0, "ColorColumn",  { bg = p.cursor_line })
hl(0, "MatchParen",   { fg = p.cyan, bold = true })
hl(0, "Pmenu",        { fg = p.fg, bg = p.bg_dark })
hl(0, "PmenuSel",     { fg = p.bg, bg = p.blue })
hl(0, "Title",        { fg = p.blue, bold = true })
hl(0, "Directory",    { fg = p.blue })
hl(0, "ErrorMsg",     { fg = p.red })
hl(0, "WarningMsg",   { fg = p.yellow })
hl(0, "NonText",      { fg = p.gutter })
-- The `^X` / `<xx>` overlay on unprintable control chars (latin1-fallback high
-- bytes, embedded C0 controls): a standout colour so they read as special.
hl(0, "SpecialKey",   { fg = p.purple, bold = true })
hl(0, "Folded",       { fg = p.comment, bg = p.cursor_line })

-- Legacy syntax groups — the terminus of the treesitter capture fallback chain
-- (`@function.call` -> `@function` -> `Function`), so defining these colors every
-- captured token even before the per-capture refinements below.
hl(0, "Comment",     { fg = p.comment, italic = true })
hl(0, "String",      { fg = p.green })
hl(0, "Character",   { fg = p.green })
hl(0, "Number",      { fg = p.orange })
hl(0, "Boolean",     { fg = p.orange })
hl(0, "Float",       { fg = p.orange })
hl(0, "Constant",    { fg = p.orange })
hl(0, "Function",    { fg = p.blue })
hl(0, "Identifier",  { fg = p.fg })
hl(0, "Keyword",     { fg = p.purple })
hl(0, "Conditional", { fg = p.purple })
hl(0, "Repeat",      { fg = p.purple })
hl(0, "Include",     { fg = p.purple })
hl(0, "Exception",   { fg = p.purple })
hl(0, "Type",        { fg = p.yellow })
hl(0, "Operator",    { fg = p.cyan })
hl(0, "Delimiter",   { fg = p.fg })
hl(0, "Label",       { fg = p.blue })
hl(0, "Tag",         { fg = p.red })

-- Per-capture refinements that the broad legacy groups can't express — these
-- match the edit-host's One Dark map token-for-token.
hl(0, "@variable",            { fg = p.fg })
hl(0, "@variable.builtin",    { fg = p.red })
hl(0, "@variable.parameter",  { fg = p.fg })
hl(0, "@property",            { fg = p.red })
hl(0, "@field",               { fg = p.red })
hl(0, "@variable.member",     { fg = p.red })
hl(0, "@constructor",         { fg = p.yellow })
hl(0, "@function.builtin",    { fg = p.cyan })
hl(0, "@function.macro",      { fg = p.cyan })
hl(0, "@keyword.operator",    { fg = p.purple })
hl(0, "@string.escape",       { fg = p.cyan })
hl(0, "@string.special",      { fg = p.cyan })
hl(0, "@type.builtin",        { fg = p.yellow })
hl(0, "@attribute",           { fg = p.yellow })
hl(0, "@annotation",          { fg = p.yellow })
hl(0, "@namespace",           { fg = p.yellow })
hl(0, "@module",              { fg = p.yellow })
hl(0, "@punctuation",         { fg = p.fg })
hl(0, "@punctuation.special", { fg = p.red })
hl(0, "@tag",                 { fg = p.red })
hl(0, "@tag.attribute",       { fg = p.orange })
hl(0, "@tag.delimiter",       { fg = p.fg })

-- Diagnostics (matches the GUI's severity palette).
hl(0, "DiagnosticError", { fg = p.red })
hl(0, "DiagnosticWarn",  { fg = p.yellow })
hl(0, "DiagnosticInfo",  { fg = p.cyan })
hl(0, "DiagnosticHint",  { fg = p.comment })
