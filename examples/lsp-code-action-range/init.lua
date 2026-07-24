-- ~~~ nxvim nx.lsp.code_action over a RANGE — the visual selection ~~~
--
-- Run it (from the repo root) — needs `gopls` on your PATH:
--
--     NXVIM_CONFIG=examples/lsp-code-action-range \
--       cargo run -p nxvim -- examples/lsp-code-action-range/sample.go
--
-- A `textDocument/codeAction` request carries a RANGE, and the refactor kinds
-- (`refactor.extract`, `refactor.inline`) are exactly the ones a server offers only
-- over a non-empty one. nxvim takes that range from, in order: an explicit
-- `opts.range`, the live Visual / Select selection, else a point at the cursor.
--
-- Type this / see that:
--
--   1. Put the cursor on line 8 (`sum := 0`), press `V` then `3j` to select the
--      loop, then `<leader>ca`.  →  the chooser lists gopls's range refactors
--      ("Extract function", "Extract variable", …). Pick one with <C-n> and <CR>:
--      the selected lines are extracted. Without a selection those entries are
--      absent — the range is what makes them appear.
--   2. Press `<leader>ca` in NORMAL mode with the cursor anywhere.  →  a point
--      request at the cursor: only the actions that apply at that spot.
--   3. Select the same lines and type `:` instead — the line is prefilled `:'<,'>`
--      — then `LspCodeAction<CR>`.  →  the ex form, scoped to the addressed WHOLE
--      lines (an ex address is a line, not a column).
--   4. `<leader>cx` runs the same request with an EXPLICIT range — the same
--      `sum := 0` … `}` block, stated outright — from wherever the cursor is, with
--      no selection at all. Same refactors, no interaction: the non-interactive form.

vim.g.mapleader = "\\"

--------------------------------------------------------------------------------
-- Attach gopls to `go` buffers. `go.mod` is the root marker — gopls needs the
-- module root to offer refactors at all, which is why this example ships one.
nx.lsp.config("gopls", {
  cmd = { "gopls" },
  filetypes = { "go" },
  root_markers = { "go.mod", ".git" },
  on_attach = function(_client, bufnr)
    -- The keymap is set for BOTH normal and visual mode, and that is the whole
    -- trick: pressed in Visual, `nx.lsp.code_action()` finds the live selection and
    -- sends it as the request's range (then consumes it, dropping to Normal — what
    -- vim does for any `:` command that acts on a selection). Pressed in Normal it
    -- falls back to a point at the cursor.
    nx.keymap.set({ "n", "v" }, "<leader>ca", function()
      nx.lsp.code_action()
    end, { buffer = bufnr })

    -- The explicit form: `opts.range` is 0-based rows, 0-based BYTE columns, and
    -- END-EXCLUSIVE (the `nx.win.select_range` convention). It wins over both the
    -- cursor and any live selection, which is what makes it usable headlessly —
    -- from a `BufWritePre` autocmd, say, or a plugin acting on a computed span.
    -- Rows 8..11 end-exclusive = the `sum := 0` … `}` block (file lines 9-12).
    nx.keymap.set("n", "<leader>cx", function()
      nx.lsp.code_action({
        range = { start_row = 8, start_col = 0, end_row = 12, end_col = 0 },
      })
    end, { buffer = bufnr })

    -- Ranges compose with the kind filter: `context.only` narrows to the refactors,
    -- so the chooser skips the quickfixes and organize-imports entries entirely.
    nx.keymap.set({ "n", "v" }, "<leader>cr", function()
      nx.lsp.code_action({ context = { only = { "refactor" } } })
    end, { buffer = bufnr })

    nx.keymap.set("n", "K", nx.lsp.hover, { buffer = bufnr })
  end,
})
nx.lsp.enable("gopls")
