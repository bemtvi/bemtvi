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
--   * requests ROUTE BY CAPABILITY first — only a server that advertises the
--     feature is a candidate. Here that is always gopls, decided by what the
--     servers said at `initialize` rather than by anything in this config;
--   * among the candidates, `priority` (section 5) decides who leads, and the
--     config name alphabetically breaks a tie. Capability is the filter, priority
--     is the preference;
--   * EVERY cursor verb FANS OUT to the capable servers and merges — hover,
--     signature help, references, symbols, code actions, and the goto family. With
--     this pair only gopls is capable of any of them, so each merge has one
--     contributor: the mechanism is the same, the pairing is what is one-sided. Pair
--     gopls with a second server that offers code actions (efm running a fixer, say)
--     and the chooser fills from both. A goto whose merged list holds ONE place
--     still jumps — the picker appears only when servers disagree;
--   * the verbs that ACT — `format`, `rename` — still pick one server, because two
--     servers' edits cannot both be applied to one buffer;
--   * `{ name = … }` on any verb, or a bare `:LspHover <server>`, overrides all of
--     that for one call — see steps 7-10.
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
--   8. Naming a server is not special to formatting — EVERY verb takes it, because
--      every verb has the same ambiguity once two servers can answer. Cursor on
--      `Printf` again:
--        * `:LspHover gopls`  →  the same hover as `K`, asked for outright.
--        * `<leader>lh` (`hover{ name = "golangci_lint" }`)  →  `LSP client
--          'golangci_lint' does not provide hover`. Attached, but it advertises no
--          `hoverProvider` (step 5) — so the route says THAT, rather than the
--          misleading "no client named…" or, worse, quietly answering from gopls.
--        * `:LspHover nosuch`  →  `No LSP client named 'nosuch' on this buffer`.
--      Three failures, three different fixes, three different messages — and none
--      of them falls back to another server.
--   9. `:LspHover <Tab>`.  →  the wildmenu offers `golangci_lint` and `gopls`: the
--      argument completes from the clients actually on this buffer. (`:LspRename`
--      takes the new identifier first, so ITS server slot is the second word:
--      `:LspRename Foo <Tab>`.)
--  10. `:LspReferences gopls` vs `<leader>lr`. References, symbols and code actions
--      normally FAN OUT and merge; a name narrows the round to one client, so it is
--      how you say "just this server's list" when two servers both index the
--      project. With this pair the results match — only gopls is capable — which is
--      the point: naming the server that would have been picked anyway changes
--      nothing, so it is safe to be explicit.
--  11. Hover MERGES: `K` asks every server that advertises `hoverProvider` and
--      composes one float, each section headed with the client that wrote it. Here
--      only gopls does (step 5), so you get one unheaded section — the merge is
--      invisible until a second server has something to say, which is the right
--      default. Pair gopls with a server that hovers (a second type-checker, an efm
--      instance surfacing docs) and the float grows a `# name` heading per server.
--      Signature help does the same, one line per server prefixed with its name. Put
--      the cursor on `name` — the second argument of the `Printf` call on line 13 —
--      and run `:LspSignatureHelp`.  →  `Printf(format string, a ...any) (n int, err
--      error)    [a ...any]`, the bracket naming the argument you are on. (gopls
--      answers per position, and declines on the opening quote of the first argument;
--      that is the server's judgement, not a route that failed.)
--      `gd` on the `count` inside `fmt.Println(…)` (line 23) merges too — but its
--      merged list holds one place, so it JUMPS, to the `count := 1` on line 21. The
--      picker only appears when two servers disagree about where the definition is,
--      which is information you want rather than a silent coin-flip.
--  12. `:LspInfo` again, now reading the `priority:` lines — section 5 below ranks
--      gopls above the linter.  →  gopls is listed FIRST, `priority: 10`, and
--      golangci_lint second, `priority: 0  (default)`. The listing is in the order
--      requests actually route, so "who answered?" is read top-down: the first
--      server listed that advertises the feature is the one that did.
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
  -- The stated preference (section 5): rank gopls above the linter, so it leads
  -- wherever both could answer — instead of "whichever config name sorts first",
  -- which is the only thing the editor can guess on its own.
  priority = 10,
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

-- …and `name` is not a formatting option, it is a ROUTING one: every language verb
-- takes it, and the ex-commands take it as a bare argument (`:LspHover gopls`).
-- Route to a server that is attached but does not advertise the feature and it says
-- so — it does not fall through to the server that does.
nx.keymap.set("n", "<leader>lh", function()
  nx.lsp.hover({ name = "golangci_lint" })
end)

-- The merging verbs take it too, where it means "this client's list ALONE" instead
-- of every capable server's merged. `:LspReferences gopls` is the ex twin.
nx.keymap.set("n", "<leader>lr", function()
  nx.lsp.references({ name = "gopls" })
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

--------------------------------------------------------------------------------
-- 5. Priority — the DEFAULT route, stated rather than guessed.
--
-- `name` (section 3) answers "which server, this call". `priority` answers "which
-- server, when nobody says": an integer per config, higher first, `0` by default.
-- Without it the order is the config name alphabetically — `golangci_lint` before
-- `gopls`, which is a fact about spelling, not a preference. gopls is ranked 10 up in
-- section 1, so it leads every decision the two could share:
--
--   * a single-target verb (hover, goto, rename, format) asks the highest-ranked
--     server that ADVERTISES the feature — priority orders the candidates, it never
--     promotes a server that cannot answer;
--   * the merged surfaces present in that order — hover sections, code-action rows,
--     reference and symbol lists;
--   * `:LspInfo` lists in that order too, with each `priority:` line, so the listing
--     reads as the explanation of what just happened.
--
-- Here the pairing is one-sided enough that capability already decides everything, so
-- the rank changes no outcome — which is exactly when you want it in the config
-- anyway: it is the line that keeps working when the second server grows a feature
-- the first one has, and stops the answer from silently moving.
nx.keymap.set("n", "<leader>lp", function()
  local rows = {}
  for _, c in ipairs(nx.lsp.clients({ bufnr = 0 })) do
    local cfg = nx.lsp.get_config(c.name)
    rows[#rows + 1] = c.name .. " priority=" .. tostring(cfg.priority or 0)
  end
  table.sort(rows)
  nx.print(table.concat(rows, "   |   "))
end)
