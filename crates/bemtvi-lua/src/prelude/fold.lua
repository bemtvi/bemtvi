-- Fold surfaces (`btv.fold.*`) — today just the customizable `'foldtext'`.
btv.fold = btv.fold or {}

-- `btv.fold.text(src)`: set the text a **closed** fold shows on its collapsed
-- row (vim's `'foldtext'`), or restore the built-in default with `nil`.
--
-- `src` is a string of Lua *source* — an expression, not a function value —
-- because it runs in the bounded compute sandbox: a second, pure VM with a
-- wall-clock deadline, no editor state and no `btv.*`. A closure cannot cross
-- between VMs, so the source crosses instead and is compiled there.
--
-- Three names are in scope, and the expression returns the row's text (a number
-- is accepted and rendered as one):
--
-- ```
-- first   the fold's first line, exactly as it appears in the buffer
-- lines   how many lines the fold covers
-- lnum    the 1-based line the fold starts on
-- ```
--
-- ```lua
-- btv.fold.text([[ first:gsub("%s+$", "") .. "  (" .. lines .. " lines)" ]])
-- btv.fold.text(nil)   -- back to the built-in "+-- 12 lines: …"
-- ```
--
--
-- The sandbox is **stateless**: nothing carries from one call to the next, and
-- assigning a global raises. That is deliberate — no call shape is a clean
-- once-per-item traversal (`:s` re-runs on every keystroke of the live preview,
-- a foldexpr sees only the rows an edit touched, the picker scorer only the top
-- survivors, `foldtext` is memoized), so an accumulator would be quietly wrong.
-- The result is memoized on the fold's first line, so a steady screen makes no
-- sandbox calls at all and only a fold whose content changed re-renders.
--
-- An expression that errors, exceeds its deadline, or returns a table reports
-- once and is then uninstalled, rather than repeating the error every frame.
function btv.fold.text(src)
  btv._sandbox_set("fold.text", btv._fold_set_text, src)
end
