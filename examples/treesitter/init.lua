-- ~~~ nxvim vim.treesitter playground: query the syntax tree from Lua ~~~
--
-- This drives the `vim.treesitter` Lua platform — neovim's own treesitter Lua
-- running on nxvim's in-process grammars. Plugins (textobjects, AST tools,
-- query-driven motions) use exactly this surface.
--
-- PREREQUISITE: a Rust parser must be installed in nxvim's data dir, laid out
-- like neovim's: `<data>/parser/rust.so`. The quickest way (matching how the
-- tests build it) is to compile tree-sitter-rust's C sources into a `.so`. If no
-- parser is installed, `:TSFunctions` reports a loud, named error — by design.
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/treesitter \
--       cargo run -p nxvim -- examples/treesitter/sample.rs
--
-- Then try the commands below.

--------------------------------------------------------------------------------
-- :TSRoot — print the root node's type and its named-child count.
--    The simplest end-to-end check: get_parser(0) attaches a parser to the
--    current buffer, :parse() reads the buffer snapshot and returns its trees,
--    and tree:root() is the top of the AST.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("TSRoot", function()
  local parser = vim.treesitter.get_parser(0, "rust")
  local root = parser:parse()[1]:root()
  print(("root: %s with %d named children"):format(root:type(), root:named_child_count()))
end, {})

--------------------------------------------------------------------------------
-- :TSFunctions — list every function name in the buffer via a query.
--    query.parse compiles an s-expression query; iter_captures walks the matches
--    and hands back the captured @name nodes; get_node_text pulls each name from
--    the buffer. This is the platform's bread and butter.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("TSFunctions", function()
  local root = vim.treesitter.get_parser(0, "rust"):parse()[1]:root()
  local query = vim.treesitter.query.parse("rust", "(function_item name: (identifier) @name)")
  local names = {}
  for _, node in query:iter_captures(root, 0) do
    names[#names + 1] = vim.treesitter.get_node_text(node, 0)
  end
  table.sort(names)
  print("functions: " .. table.concat(names, ", "))
end, {})

--------------------------------------------------------------------------------
-- :TSPub — list only the functions whose name starts with a capital letter,
--    using a #match? predicate (evaluated by the vendored query.lua over
--    vim.regex). Proves predicate filtering runs end to end.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("TSPub", function()
  local root = vim.treesitter.get_parser(0, "rust"):parse()[1]:root()
  local query = vim.treesitter.query.parse(
    "rust",
    '((function_item name: (identifier) @name) (#match? @name "^[A-Z]"))'
  )
  local names = {}
  for _, node in query:iter_captures(root, 0) do
    names[#names + 1] = vim.treesitter.get_node_text(node, 0)
  end
  print("Capitalized fns: " .. (#names > 0 and table.concat(names, ", ") or "(none)"))
end, {})

--------------------------------------------------------------------------------
-- :TSNodeAt — print the smallest named node under the cursor. Move the cursor
--    around and re-run to watch the AST node change.
--
--    IMPORTANT: get_node resolves against the *parsed* tree, so we parse first.
--    Two nxvim specifics make this mandatory (neovim's docs note the first):
--      * get_node on an unparsed tree yields nothing — there's no background
--        highlighter keeping the buffer's tree parsed (a non-goal here).
--      * neovim's parser cache is weak-valued, so the parser is garbage-collected
--        between command runs. We hold a strong `parser` local across the
--        get_node call so the parse we just did is the tree get_node sees.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("TSNodeAt", function()
  local parser = vim.treesitter.get_parser(0, "rust")
  parser:parse()
  local node = vim.treesitter.get_node({ bufnr = 0, lang = "rust" })
  if node then
    print(("node under cursor: %s"):format(node:type()))
  else
    print("no node under cursor")
  end
end, {})

print("treesitter playground: try :TSRoot, :TSFunctions, :TSPub, :TSNodeAt")
