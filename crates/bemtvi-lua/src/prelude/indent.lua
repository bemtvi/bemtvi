-- Indent surfaces (`btv.indent.*`) — the `'indentexpr'` escape hatch.
btv.indent = btv.indent or {}

-- `btv.indent.expr(src)`: compute a line's indent, or clear the expression with
-- `nil`.
--
-- bemtvi indents from tree-sitter's `indents.scm` where a grammar has one; this
-- is the escape hatch for a filetype that has no grammar, or whose indent query
-- gets a construct wrong. It sits **below** the tree-sitter verdict — structure
-- beats a hand-written rule — and **above** `smartindent`/`autoindent`.
--
-- `src` is a string of Lua *source* — an expression, not a function value —
-- because it runs in the bounded compute sandbox: a second, pure VM with a
-- wall-clock deadline, no editor state and no `btv.*`. That means it cannot read
-- the buffer; everything it needs is passed in:
--
-- ```
-- prev         the previous non-blank line ("" when there is none)
-- line         the line being indented
-- lnum         its 1-based number
-- sw           the effective 'shiftwidth'
-- previndent   the previous non-blank line's indent, in columns
-- ```
--
-- Return the target indent in **columns**, or `nil` to decline and let
-- `smartindent`/`autoindent` answer:
--
-- ```lua
-- btv.indent.expr([[
--   line:match("^%s*end") and previndent - sw
--     or prev:match("then%s*$") and previndent + sw
--     or nil
-- ]])
-- ```
--
--
-- The sandbox is **stateless**: nothing carries from one call to the next, and
-- assigning a global raises. That is deliberate — no call shape is a clean
-- once-per-item traversal (`:s` re-runs on every keystroke of the live preview,
-- a foldexpr sees only the rows an edit touched, the picker scorer only the top
-- survivors, `foldtext` is memoized), so an accumulator would be quietly wrong.
-- It is consulted per line, so a `=G` over a large range calls it once per line;
-- an expression that errors, exceeds its deadline, or returns a non-number
-- reports once and is then uninstalled rather than repeating per line.
function btv.indent.expr(src)
  btv._sandbox_set("indent.expr", btv._indent_set_expr, src)
end
