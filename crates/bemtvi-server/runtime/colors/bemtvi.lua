-- bemtvi — the editor's own colorscheme, drawn from its app icon: `:colorscheme bemtvi`.
--
-- The icon is a bem-te-vi (great kiskadee) in pixel art: a charcoal cap and mask,
-- a lemon-yellow belly, a white brow, tan and brown wings. Those pixels are this
-- palette. The structure is catppuccin's: a grey ladder from `crust` up to `text`,
-- pastel accents with the same names, and the same roles for types (yellow) and
-- constants (peach). Strings are a cornflower `blue`, docstrings a softer
-- `periwinkle` beside it. The other code roles are warmer and greener: keywords
-- (and builtin types) are a deep `forest` green, the trees the bird perches in;
-- functions, and everything catppuccin paints in its blue (directories, titles,
-- urls), are an `orange`; parameters take `crimson`, a deeper red than the error
-- `red`; and fields, properties, tags and links, in place of catppuccin's
-- lavender, take the rust `maroon` of the wing, as does the "changed" diff colour
-- (a red one would read as "removed"). The ladder is the icon's bluish charcoal
-- (`base` is the cap's shadow, deepened, `#1e1e24`) and slate greys. The warm
-- roles take the icon's colours: yellow is the belly, peach the gold/amber of the
-- wing edge, maroon the rust, flamingo the tan, rosewater the brow. The cool
-- roles, which the bird has none of, are pastels at the same lightness, leaning
-- toward the slate hue. The lemon itself is too loud for text; it marks the
-- chrome instead (the mode badge, the current search match, float titles), like the
-- belly on the charcoal bird. The wing brown backs the visual selection.
--
-- It is bundled in the binary (sourced from the embedded runtime, not the
-- runtimepath), so it loads with zero user config, and a truecolor terminal gets it
-- by default. A user `colors/bemtvi.lua` on the runtimepath still shadows it. The
-- One Dark scheme is `:colorscheme neutral`. Written against
-- `btv.hl.define(ns, name, opts)` (ADR 0002).

local p = {
  -- accents (catppuccin role names)
  rosewater = "#ebf3f3", -- the white brow: cursor
  flamingo  = "#d8c2a8", -- the tan wing: identifiers, symbols
  pink      = "#ebaab9", -- escapes, regexes, macros
  mauve     = "#c9a6e4", -- level-6 markdown headings
  red       = "#f08a7e", -- errors, builtin variables
  maroon    = "#dca07a", -- the rust wing edge: fields, properties, tags, links, diffs
  peach     = "#f0b35c", -- the gold/amber: numbers, constants, builtins
  yellow    = "#f4e472", -- the belly, softened for text: types
  green     = "#b6d98b", -- added lines, ok diagnostics, checked list items
  blue      = "#83a7ea", -- strings, inline code: a cornflower blue
  periwinkle = "#9db4ec", -- docstrings: a softer blue beside the string cornflower
  teal      = "#94d7c0", -- characters, lists, tag delimiters
  sky       = "#8ed3e3", -- operators
  sapphire  = "#7fbedf", -- labels
  orange    = "#fb8c3c", -- functions, directories, titles, urls
  forest    = "#5fa86c", -- keywords, builtin types: deeper than the string green
  crimson   = "#e3787f", -- parameters: a deeper red than `red`
  -- the icon's own, unsoftened
  lemon     = "#f9e931", -- the belly: chrome accents only, never body text
  gold      = "#dfaa28",
  -- the grey ladder (the charcoal cap and slate greys)
  text      = "#e2e8e8",
  subtext1  = "#c0c5c4",
  subtext0  = "#a3a8ae",
  overlay2  = "#8d929d",
  overlay1  = "#787d89",
  overlay0  = "#636773",
  surface2  = "#4f525d",
  surface1  = "#3f414b",
  surface0  = "#28282f",
  base      = "#1e1e24",
  mantle    = "#18181d",
  crust     = "#121216",
  -- tints, each an accent mixed into `base` (what catppuccin's `darken` computes)
  wing      = "#473229", -- the brown wing at 60%: the visual selection
  search    = "#584825", -- gold at 30%: every hlsearch match
  paren     = "#35363f", -- surface1 at 70%: the matched bracket's backdrop
  diff_add  = "#394037", -- green at 18%
  diff_chg  = "#2b272a", -- maroon at 7%
  diff_txt  = "#57453e", -- maroon at 30%
  diff_del  = "#443134", -- red at 18%
}

local hl = btv.hl.define

-- Editor chrome.
hl(0, "Normal",       { fg = p.text, bg = p.base })
hl(0, "NormalNC",     { fg = p.text, bg = p.base })
hl(0, "NormalFloat",  { fg = p.text, bg = p.mantle })
hl(0, "FloatBorder",  { fg = p.overlay0, bg = p.mantle })
hl(0, "FloatTitle",   { fg = p.lemon, bg = p.mantle, bold = true })
hl(0, "Cursor",       { fg = p.base, bg = p.rosewater })
hl(0, "lCursor",      { fg = p.base, bg = p.rosewater })
hl(0, "TermCursor",   { fg = p.base, bg = p.rosewater })
hl(0, "LineNr",       { fg = p.surface2 })
hl(0, "CursorLine",   { bg = p.surface0 })
hl(0, "CursorColumn", { bg = p.mantle })
hl(0, "CursorLineNr", { fg = p.yellow })
hl(0, "ColorColumn",  { bg = p.surface0 })
hl(0, "SignColumn",   { fg = p.surface1, bg = p.base })
hl(0, "FoldColumn",   { fg = p.overlay0 })
hl(0, "Folded",       { fg = p.flamingo, bg = p.surface0 })
hl(0, "Visual",       { bg = p.wing })
hl(0, "VisualNOS",    { bg = p.wing })
-- Every hlsearch hit is a dim gold wash with the text kept on top; the match you are
-- on takes the full lemon, so it stands out from the rest.
hl(0, "Search",       { fg = p.text, bg = p.search })
hl(0, "IncSearch",    { fg = p.crust, bg = p.lemon })
hl(0, "CurSearch",    { fg = p.crust, bg = p.lemon })
hl(0, "Substitute",   { fg = p.pink, bg = p.surface1 })
hl(0, "MatchParen",   { fg = p.peach, bg = p.paren, bold = true })
hl(0, "StatusLine",   { fg = p.text, bg = p.mantle })
hl(0, "StatusLineNC", { fg = p.overlay0, bg = p.mantle })
hl(0, "WinSeparator", { fg = p.crust })
hl(0, "VertSplit",    { fg = p.crust })
hl(0, "TabLine",      { fg = p.overlay0, bg = p.crust })
hl(0, "TabLineSel",   { fg = p.text, bg = p.base })
hl(0, "TabLineFill",  { bg = p.mantle })
hl(0, "WinBar",       { fg = p.rosewater })
hl(0, "EndOfBuffer",  { fg = p.surface1 })
hl(0, "NonText",      { fg = p.overlay0 })
hl(0, "Whitespace",   { fg = p.surface1 })
hl(0, "Conceal",      { fg = p.overlay1 })
-- The `^X` / `<xx>` overlay on unprintable control chars: a standout colour.
hl(0, "SpecialKey",   { fg = p.pink, bold = true })
hl(0, "Directory",    { fg = p.orange })
hl(0, "Title",        { fg = p.orange, bold = true })
hl(0, "MsgArea",      { fg = p.text })
hl(0, "ModeMsg",      { fg = p.text, bold = true })
hl(0, "MoreMsg",      { fg = p.orange })
hl(0, "Question",     { fg = p.orange })
hl(0, "ErrorMsg",     { fg = p.red, bold = true, italic = true })
hl(0, "WarningMsg",   { fg = p.yellow })
hl(0, "QuickFixLine", { bg = p.paren, bold = true })
hl(0, "Pmenu",        { fg = p.subtext1, bg = p.mantle })
hl(0, "PmenuSel",     { bg = p.surface1, bold = true })
hl(0, "PmenuSbar",    { bg = p.surface0 })
hl(0, "PmenuThumb",   { bg = p.overlay0 })
hl(0, "PmenuExtra",   { fg = p.overlay0 })
hl(0, "WildMenu",     { bg = p.overlay0 })
-- The completion popup / picker paints its fuzzy-match characters with `Special`
-- (the end of the `CmpItemAbbrMatch` / `TelescopeMatching` chains), and the picker
-- prompt takes it too.
hl(0, "Special",      { fg = p.pink })
hl(0, "CmpItemAbbrMatch",      { fg = p.peach, bold = true })
hl(0, "TelescopeMatching",     { fg = p.peach, bold = true })
hl(0, "TelescopePromptPrefix", { fg = p.lemon })
hl(0, "TelescopeTitle",        { fg = p.lemon, bold = true })
hl(0, "SpellBad",     { sp = p.red, undercurl = true })
hl(0, "SpellCap",     { sp = p.yellow, undercurl = true })
hl(0, "SpellLocal",   { sp = p.orange, undercurl = true })
hl(0, "SpellRare",    { sp = p.green, undercurl = true })
-- Snippet tabstops: an underlined placeholder, the one you're on highlighted.
hl(0, "SnippetTabstop",       { underline = true })
hl(0, "SnippetTabstopActive", { bg = p.surface1, underline = true })
hl(0, "LspSignatureActiveParameter", { fg = p.peach, bold = true })
hl(0, "LspInlayHint",         { fg = p.overlay0, bg = p.mantle })
hl(0, "LspReferenceText",     { bg = p.surface1 })
hl(0, "LspReferenceRead",     { bg = p.surface1 })
hl(0, "LspReferenceWrite",    { bg = p.surface1 })

-- bemtvi's own surfaces: the which-key popup and the statusline segments. The mode
-- badge is the icon at a glance: the lemon belly on the dark cap.
hl(0, "WhichKey",           { link = "NormalFloat" })
hl(0, "WhichKeyGroup",      { fg = p.orange })
hl(0, "WhichKeyDesc",       { fg = p.flamingo })
hl(0, "WhichKeySeparator",  { fg = p.overlay0 })
hl(0, "WhichKeyBorder",     { link = "FloatBorder" })
hl(0, "WhichKeyValue",      { fg = p.overlay0 })
hl(0, "StatusLineMode",     { fg = p.crust, bg = p.lemon, bold = true })
hl(0, "StatusLineModified", { fg = p.peach })

-- Legacy syntax groups: the end of the treesitter capture fallback chain
-- (`@function.call` -> `@function` -> `Function`). The roles are catppuccin's.
hl(0, "Comment",      { fg = p.overlay2, italic = true })
hl(0, "SpecialComment", { link = "Special" })
hl(0, "Constant",     { fg = p.peach })
hl(0, "String",       { fg = p.blue })
hl(0, "Character",    { fg = p.teal })
hl(0, "Number",       { fg = p.peach })
hl(0, "Float",        { link = "Number" })
hl(0, "Boolean",      { fg = p.peach })
hl(0, "Identifier",   { fg = p.flamingo })
hl(0, "Function",     { fg = p.orange })
hl(0, "Statement",    { fg = p.forest })
hl(0, "Conditional",  { fg = p.forest })
hl(0, "Repeat",       { fg = p.forest })
hl(0, "Label",        { fg = p.sapphire })
hl(0, "Operator",     { fg = p.sky })
hl(0, "Keyword",      { fg = p.forest })
hl(0, "Exception",    { fg = p.forest })
hl(0, "PreProc",      { fg = p.pink })
hl(0, "Include",      { fg = p.forest })
hl(0, "Define",       { link = "PreProc" })
hl(0, "Macro",        { fg = p.forest })
hl(0, "PreCondit",    { link = "PreProc" })
hl(0, "StorageClass", { fg = p.yellow })
hl(0, "Structure",    { fg = p.yellow })
hl(0, "Type",         { fg = p.yellow })
hl(0, "Typedef",      { link = "Type" })
hl(0, "SpecialChar",  { link = "Special" })
hl(0, "Tag",          { fg = p.maroon, bold = true })
hl(0, "Delimiter",    { fg = p.overlay2 })
hl(0, "Debug",        { link = "Special" })
hl(0, "Underlined",   { underline = true })
hl(0, "Error",        { fg = p.red })
hl(0, "Todo",         { fg = p.crust, bg = p.lemon, bold = true })

-- Treesitter captures the legacy groups can't express.
hl(0, "@variable",              { fg = p.text })
hl(0, "@variable.builtin",      { fg = p.red })
hl(0, "@variable.parameter",    { fg = p.crimson })
hl(0, "@variable.member",       { fg = p.maroon })
hl(0, "@property",              { fg = p.maroon })
hl(0, "@field",                 { fg = p.maroon })
hl(0, "@constant.builtin",      { fg = p.peach })
hl(0, "@module",                { fg = p.yellow, italic = true })
hl(0, "@namespace",             { fg = p.yellow, italic = true })
hl(0, "@string.documentation",  { fg = p.periwinkle })
hl(0, "@string.regexp",         { fg = p.pink })
hl(0, "@string.escape",         { fg = p.pink })
hl(0, "@string.special.symbol", { fg = p.flamingo })
hl(0, "@string.special.url",    { fg = p.orange, italic = true, underline = true })
hl(0, "@type.builtin",          { fg = p.forest })
hl(0, "@attribute",             { link = "Constant" })
hl(0, "@function.builtin",      { fg = p.peach })
hl(0, "@function.macro",        { fg = p.pink })
hl(0, "@constructor",           { fg = p.yellow })
hl(0, "@keyword.function",      { fg = p.forest })
hl(0, "@keyword.operator",      { fg = p.forest })
hl(0, "@keyword.return",        { fg = p.forest })
hl(0, "@keyword.conditional.ternary", { link = "Operator" })
hl(0, "@punctuation.bracket",   { fg = p.overlay2 })
hl(0, "@punctuation.special",   { link = "Special" })
hl(0, "@comment.error",         { fg = p.crust, bg = p.red })
hl(0, "@comment.warning",       { fg = p.crust, bg = p.yellow })
hl(0, "@comment.hint",          { fg = p.crust, bg = p.orange })
hl(0, "@comment.todo",          { fg = p.crust, bg = p.lemon })
hl(0, "@comment.note",          { fg = p.crust, bg = p.rosewater })
hl(0, "@tag",                   { fg = p.orange })
hl(0, "@tag.builtin",           { fg = p.orange })
hl(0, "@tag.attribute",         { fg = p.yellow, italic = true })
hl(0, "@tag.delimiter",         { fg = p.teal })

-- Markdown markup: how hover / completion-docs floats and markdown buffers render.
-- Inline and fenced code get the `surface0` backdrop so they read as code regions
-- even when the fence's language has no grammar (`@markup.raw.block` backs the whole
-- line through the `line_bg` layer).
hl(0, "@markup",                { fg = p.text })
hl(0, "@markup.heading",        { fg = p.orange, bold = true })
hl(0, "@markup.heading.1",      { fg = p.yellow, bold = true })
hl(0, "@markup.heading.2",      { fg = p.peach, bold = true })
hl(0, "@markup.heading.3",      { fg = p.green, bold = true })
hl(0, "@markup.heading.4",      { fg = p.sapphire, bold = true })
hl(0, "@markup.heading.5",      { fg = p.maroon, bold = true })
hl(0, "@markup.heading.6",      { fg = p.mauve, bold = true })
hl(0, "@markup.strong",         { fg = p.maroon, bold = true })
hl(0, "@markup.italic",         { fg = p.maroon, italic = true })
hl(0, "@markup.strikethrough",  { strikethrough = true })
hl(0, "@markup.underline",      { underline = true })
hl(0, "@markup.raw",            { fg = p.blue, bg = p.surface0 })
hl(0, "@markup.raw.block",      { bg = p.surface0 })
hl(0, "@markup.quote",          { fg = p.flamingo, italic = true })
hl(0, "@markup.math",           { fg = p.orange })
hl(0, "@markup.link",           { fg = p.maroon })
hl(0, "@markup.link.label",     { fg = p.maroon })
hl(0, "@markup.link.url",       { fg = p.orange, italic = true, underline = true })
hl(0, "@markup.list",           { fg = p.teal })
hl(0, "@markup.list.checked",   { fg = p.green })
hl(0, "@markup.list.unchecked", { fg = p.overlay1 })

-- Diffs: the diff-mode line backdrops, the diff-file captures, and git signs.
hl(0, "DiffAdd",      { bg = p.diff_add })
hl(0, "DiffChange",   { bg = p.diff_chg })
hl(0, "DiffText",     { bg = p.diff_txt })
hl(0, "DiffDelete",   { bg = p.diff_del })
hl(0, "Added",        { fg = p.green })
hl(0, "Changed",      { fg = p.maroon })
hl(0, "Removed",      { fg = p.red })
hl(0, "diffAdded",    { fg = p.green })
hl(0, "diffRemoved",  { fg = p.red })
hl(0, "diffChanged",  { fg = p.maroon })
hl(0, "diffFile",     { fg = p.maroon })
hl(0, "diffLine",     { fg = p.overlay0 })
hl(0, "@diff.plus",   { link = "diffAdded" })
hl(0, "@diff.minus",  { link = "diffRemoved" })
hl(0, "@diff.delta",  { link = "diffChanged" })

-- Diagnostics.
hl(0, "DiagnosticError", { fg = p.red })
hl(0, "DiagnosticWarn",  { fg = p.yellow })
hl(0, "DiagnosticInfo",  { fg = p.sky })
hl(0, "DiagnosticHint",  { fg = p.teal })
hl(0, "DiagnosticOk",    { fg = p.green })
hl(0, "DiagnosticUnderlineError", { sp = p.red, undercurl = true })
hl(0, "DiagnosticUnderlineWarn",  { sp = p.yellow, undercurl = true })
hl(0, "DiagnosticUnderlineInfo",  { sp = p.sky, undercurl = true })
hl(0, "DiagnosticUnderlineHint",  { sp = p.teal, undercurl = true })
hl(0, "DiagnosticVirtualTextError", { fg = p.red, bg = p.diff_del })
hl(0, "DiagnosticVirtualTextWarn",  { fg = p.yellow, bg = p.mantle })
hl(0, "DiagnosticVirtualTextInfo",  { fg = p.sky, bg = p.mantle })
hl(0, "DiagnosticVirtualTextHint",  { fg = p.teal, bg = p.mantle })
