-- ~~~ bemtvi tree-sitter text objects: select by syntax (vif, vaf, dia, …) ~~~
--
-- Run it (from the repo root) against the sample file:
--
--     BEMTVI_CONFIG=examples/treesitter-textobjects \
--       cargo run -p bemtvi -- examples/treesitter-textobjects/sample.rs
--
-- A *text object* is a region you name after an operator or in visual mode: `diw`
-- deletes a word, `ci"` changes inside quotes. bemtvi adds **tree-sitter** objects
-- that name a region by its place in the *syntax tree* instead of by punctuation —
-- so you can select a whole function, an argument, a comment, or a type no matter
-- how it is bracketed or spaced. As with every text object, `i` means *inside* and
-- `a` means *around*, and they work after an operator (`d`/`c`/`y`/…) and in
-- visual mode:
--
--     f  function        vif / vaf   dif / daf   cif / caf
--     a  argument         via / vaa   dia / daa
--     c  comment          vic / vac   dic / dac
--     t  type (struct,    vit / vat   dit / dat
--        enum, impl, …)
--
-- A count reaches an *enclosing* scope: with the cursor in a nested function,
-- `vif` grabs the inner one and `2vif` the function around it.
--
-- These read the buffer's `textobjects.scm` query, so they need the language's
-- grammar installed. The sample is Rust; install it once (needs a network
-- connection + a C compiler the first time):
--
--     :TSInstall rust
--
-- `:TSInstall` also fetches the text-object query, so nothing else is needed —
-- open `sample.rs` and the objects work immediately.

--------------------------------------------------------------------------------
-- Nothing to configure: tree-sitter text objects are built in and on whenever the
-- grammar (with its `textobjects.scm`) is installed. This config only makes the
-- demo comfortable — it does NOT enable the feature.
--------------------------------------------------------------------------------

-- Show the object menu sooner: after you press an operator + `i`/`a` (e.g. `di`),
-- the pending-key hint lists the available objects — including the tree-sitter
-- ones above. A short timeout surfaces it quickly. (Any which-key plugin draws
-- this same list as a popup; the raw hint shows in the command area regardless.)
vim.o.timeoutlen = 300

-- A visible sign that the grammar is live: line numbers + the current line
-- highlighted, so you can see the cursor while you aim at a function or argument.
vim.o.number = true
vim.o.cursorline = true

--------------------------------------------------------------------------------
-- CUSTOM objects: bind more of what the grammar already captures. The Rust query
-- (like most languages') also tags loops, calls, blocks, conditionals, and returns
-- — `btv.textobject.map` gives them keys. `i`/`a` stays the introducer; the capture
-- is used *verbatim*, so you choose the spelling (bemtvi's `.inner`/`.outer`, or
-- Helix's `.inside`/`.around` if you drop Helix's `textobjects.scm` on the
-- runtimepath, or any capture your own query defines).
--------------------------------------------------------------------------------
btv.textobject.map({
  il = "@loop.inner",
  al = "@loop.outer", -- vil / val — inside / around a loop
  ik = "@call.inner",
  ak = "@call.outer", -- vik / vak — a function call's arguments / the whole call
  ir = "@return.inner",
  ar = "@return.outer", -- vir / var — a return statement
})
-- Overriding a built-in key is allowed too, e.g. to follow Helix's naming after
-- installing Helix's queries:
--   btv.textobject.map({ ["if"] = "@function.inside", ["af"] = "@function.around" })

--------------------------------------------------------------------------------
-- A convenience command to remind you of the keys while you experiment.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("TextObjects", function()
  vim.notify(
    "tree-sitter text objects — i=inside a=around:\n"
      .. "  built-in:  f function   a argument   c comment   t type\n"
      .. "  custom:    l loop   k call   r return   (via btv.textobject.map)\n"
      .. "try: vif  vaf  dia  cic  vit  2vif  vil  vak   (needs :TSInstall rust)"
  )
end, {})

--------------------------------------------------------------------------------
-- TRY IT — open sample.rs, run `:TSInstall rust` once, then:
--
--   * Cursor inside `distance`'s body, `vif`  → selects the function body.
--     `vaf` → selects the whole `fn distance … }` including the signature.
--   * Cursor on the nested closure, `vif` → the closure; `2vif` → `distance`.
--   * Cursor on `target` in `main`, `dia` → deletes just that argument;
--     `cia` → changes it (type the replacement, then <Esc>).
--   * Cursor on the `struct Point` block, `vit` → the type; `dat` → deletes it.
--   * Cursor on a `//` comment line, `vic` / `dac` → the comment object.
--   * Cursor in `total`'s for-loop, `vil` → inside the loop; `vak` on a call →
--     the whole call (both are CUSTOM objects mapped via btv.textobject.map above).
--   * Press `di` (or `vi`) and pause: the object menu lists the built-in f/a/c/t
--     AND your custom l/k/r.
--   * `:TextObjects` reprints the cheatsheet at any time.
--------------------------------------------------------------------------------
