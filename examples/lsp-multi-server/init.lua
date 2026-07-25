-- ~~~ nxvim TWO language servers on ONE buffer — gopls + golangci-lint ~~~
--
-- Run it (from the repo root) — needs `gopls`, `golangci-lint-langserver` and
-- `golangci-lint` on your PATH:
--
--     NXVIM_CONFIG=examples/lsp-multi-server \
--       cargo run -p nxvim -- examples/lsp-multi-server/sample.go
--
-- Every server enabled for a filetype attaches. There is no "primary" server and
-- no "the" client: a buffer carries a SET of them, each syncing its own copy of
-- the document, each publishing its own diagnostics, each answering the requests
-- it advertises. This is the canonical split — a type-checker beside a linter,
-- the Go spelling of `pyright` + `ruff` or `ts_ls` + `eslint`.
--
-- The two servers here are lopsided on purpose, and it is worth knowing which way
-- before you read the steps. Ask them what they advertise (`<leader>lc`) and you get:
--
--     golangci_lint => (none)
--     gopls         => hover, definition, references, completion, codeAction,
--                      documentFormatting, rename, documentSymbol, workspaceSymbol
--
-- golangci-lint-langserver is a pure PUBLISHER: it answers no request at all, it
-- only pushes diagnostics. And it sorts FIRST alphabetically. So this pairing is
-- the sharpest possible test of routing — under a "use the buffer's first server"
-- rule every hover, goto and completion would go to a server that answers none of
-- them, and the editor would look broken while both servers were healthy. That is
-- the `pyright` + `ruff` bug in its purest form.
--
-- What nxvim does with the set:
--
--   * diagnostics MERGE — every server's set renders, none replaces another, and
--     each carries the `client_id` that published it;
--   * requests ROUTE BY CAPABILITY — the first attached server, in name order,
--     that advertises the feature. Here that is always gopls, decided by what the
--     servers said at `initialize` rather than by anything in this config;
--   * references, symbols and code actions FAN OUT to every *capable* server and
--     merge. With this pair only gopls is capable of any of them, so the merge has
--     one contributor — the mechanism is the same, the pairing is what is
--     one-sided. Pair gopls with a second server that offers code actions (efm
--     running a fixer, say) and the chooser fills from both;
--   * `format{ name = … }` picks WHICH server formats — see step 7 for why that
--     option only starts meaning something once a buffer has several servers.
--
-- Type this / see that:
--
--   1. Wait for both to come up, then `<leader>li`.  →  `2 clients: golangci_lint,
--      gopls`. `nx.lsp.clients{bufnr=0}` returns a LIST — indexing `[1]` and calling
--      it "the" server is the bug this example exists to prevent.
--   2. `<leader>ld` lists the merged diagnostics with the client that published
--      each.  →  FOUR entries: gopls on line 13, and golangci-lint on 13, 18 and
--      21. Two publishers, one list. Neither server can see the other's; the editor
--      is what merges them, and `client_id` is the only thing that says which is
--      which (`source` is server-chosen text — useful, but not a handle).
--   3. Line 13 is reported TWICE — once by each server — because gopls runs the
--      `go vet` analyzers and `.golangci.yml` here leaves `govet` on. That overlap
--      is the ordinary state of a type-checker beside a linter, so watch what it
--      does rather than tuning it away:
--        * TWO underlines, at different columns. The servers disagree about
--          precision: golangci-lint points at the line (col 1), gopls at the
--          offending argument (`name`, cols 19-21). They sit side by side; neither
--          is dropped.
--        * ONE sign and ONE virtual-text line, because those surfaces are per LINE.
--          The merged list is ordered by server, so golangci-lint's wins the row.
--        * The message line follows the CURSOR, so it tells you them apart: sit on
--          the tab at col 1 for golangci-lint's `govet: printf: …`, then move onto
--          `name` for gopls's own wording of the same problem.
--      When you have seen it, `.golangci.yml` shows the real fix — stop the linter
--      re-running what the language server already does.
--   4. Put the cursor on `Printf` (line 13) and press `K`.  →  gopls's hover, with
--      no help from this config. Routing picked it because golangci_lint — which
--      sorts first — advertises no `hoverProvider`.
--   5. `<leader>lc` prints what each server advertises.  →  the table above. This
--      is the input routing runs on; everything in step 3 follows from it.
--   6. `:LspInfo`.  →  the current-buffer block describes BOTH servers, each with
--      its own encoding, sync kind, document version and diagnostic count.
--   7. `<leader>lf` formats with gopls by name — line 23's `fmt.Println( count )`
--      loses its padding. Then `<leader>lF` asks a server that isn't attached.  →
--      `No LSP client named 'nosuch' on this buffer`: naming a server that cannot
--      format REPORTS rather than quietly formatting with a different one, which
--      is the failure the option exists to prevent.
--
-- If only one of the two binaries is installed the config still works — the other
-- simply never attaches, and every routed request goes to the one that did. That
-- is the same code path, with a set of one.

vim.g.mapleader = "\\"

--------------------------------------------------------------------------------
-- 1. The type-checker. gopls needs the module root before it offers anything,
-- which is why this example ships a `go.mod`.
nx.lsp.config("gopls", {
  cmd = { "gopls" },
  filetypes = { "go" },
  root_markers = { "go.mod", ".git" },
})

--------------------------------------------------------------------------------
-- 2. The linter, on the SAME filetype — that is the whole trick. Both match `go`,
-- so both attach to the buffer.
--
-- golangci-lint-langserver is a thin adapter: it does not analyze anything itself,
-- it runs the command in `init_options.command` and turns its JSON into LSP
-- diagnostics. Which linters run is decided by the `.golangci.yml` beside this
-- file, which deliberately leaves `govet` enabled so that one problem is reported
-- by BOTH servers — see step 3. Read that file for the remedy.
nx.lsp.config("golangci_lint", {
  cmd = { "golangci-lint-langserver" },
  filetypes = { "go" },
  root_markers = { "go.mod", ".git" },
  init_options = {
    -- `--issues-exit-code=1` matters: the adapter reads a non-zero exit as
    -- "there were findings", not as a failure.
    command = { "golangci-lint", "run", "--out-format", "json", "--issues-exit-code=1" },
  },
})

-- Enabling takes a list. Both are dispatched on the same `FileType go`.
nx.lsp.enable({ "gopls", "golangci_lint" })

--------------------------------------------------------------------------------
-- 3. Keymaps. None of them names a server — routing is the editor's job, and that
-- is exactly what makes a config portable across "one server" and "three".
nx.keymap.set("n", "K", nx.lsp.hover)
nx.keymap.set("n", "gd", nx.lsp.definition)
nx.keymap.set({ "n", "v" }, "<leader>ca", function()
  nx.lsp.code_action()
end)

-- …except formatting, which is the one verb where naming is the point: on a buffer
-- where several servers advertise `documentFormatting` they do different things, so
-- "whoever sorts first" is not a choice you want made for you.
nx.keymap.set("n", "<leader>lf", function()
  nx.lsp.format({ name = "gopls" })
end)

-- The other half of that guarantee: a name that is NOT attached reports itself
-- rather than silently falling back to a different server. Asking for `ruff` and
-- quietly getting pyright's formatting is exactly what the option prevents.
nx.keymap.set("n", "<leader>lF", function()
  nx.lsp.format({ name = "nosuch" })
end)

--------------------------------------------------------------------------------
-- 4. Introspection — the two surfaces that make "a buffer has a SET of servers"
-- concrete.

-- Which clients are on this buffer. A `bufnr` filter can return more than one, so
-- iterate it; filter by `name`, or by what a client advertises, when you need a
-- specific one.
nx.keymap.set("n", "<leader>li", function()
  local names = {}
  for _, c in ipairs(nx.lsp.clients({ bufnr = 0 })) do
    names[#names + 1] = c.name
  end
  table.sort(names)
  nx.print(#names .. " clients: " .. table.concat(names, ", "))
end)

-- What each server advertised at `initialize` — the input every routing decision
-- is made from. A config that branches on "is the linter attached?" should ask
-- this, not guess from names.
nx.keymap.set("n", "<leader>lc", function()
  local features = {
    "hoverProvider",
    "definitionProvider",
    "referencesProvider",
    "completionProvider",
    "codeActionProvider",
    "documentFormattingProvider",
    "renameProvider",
    "documentSymbolProvider",
    "workspaceSymbolProvider",
  }
  local rows = {}
  for _, c in ipairs(nx.lsp.clients({ bufnr = 0 })) do
    local on = {}
    for _, f in ipairs(features) do
      if c.server_capabilities[f] then
        on[#on + 1] = (f:gsub("Provider$", ""))
      end
    end
    rows[#rows + 1] = c.name .. " => " .. (next(on) and table.concat(on, ", ") or "(none)")
  end
  table.sort(rows)
  nx.print(table.concat(rows, "   |   "))
end)

-- The merged diagnostics, tagged with the client that published each. The tag is
-- the point: `vim.diagnostic.get` returns one flat list, and without `client_id`
-- there would be no way to tell the type-checker's errors from the linter's.
nx.keymap.set("n", "<leader>ld", function()
  local rows = {}
  for _, d in ipairs(vim.diagnostic.get(0)) do
    local client = d.client_id and vim.lsp.get_client_by_id(d.client_id)
    rows[#rows + 1] = string.format(
      "%d: [%s] %s",
      (d.lnum or 0) + 1,
      client and client.name or "?",
      d.message or ""
    )
  end
  table.sort(rows)
  if #rows == 0 then
    nx.print("no diagnostics yet — the servers may still be starting")
  else
    nx.print(table.concat(rows, "   |   "))
  end
end)
