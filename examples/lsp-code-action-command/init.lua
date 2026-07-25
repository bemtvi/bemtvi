-- ~~~ nxvim nx.lsp.commands — code actions the EDITOR runs ~~~
--
-- Run it (from the repo root) — needs `gopls` on your PATH:
--
--     NXVIM_CONFIG=examples/lsp-code-action-command \
--       cargo run -p nxvim -- examples/lsp-code-action-command/sample.go
--
-- A code action can carry a `command` instead of an `edit`. Most are executed by
-- the server (`workspace/executeCommand`), but some are defined to run on the
-- CLIENT — the server has no way to do them itself. gopls's "Browse gopls feature
-- documentation" is the clearest case: its command is literally
-- `gopls.client_open_url`, and its one argument is a URL. Only the editor can open
-- a browser.
--
-- `nx.lsp.commands[name]` is where you say "I'll handle that one". A registered
-- handler WINS over the round trip; anything unregistered goes to the server that
-- offered the action — the one that offered it, not the buffer's first, because
-- one command name can mean different things to two servers on a buffer.
--
-- Type this / see that:
--
--   1. Press `<leader>ca` anywhere in the file.  →  the chooser lists gopls's
--      command-carrying actions ("Browse documentation for package main", "Browse
--      free symbols", "Show compiler optimization details", "Browse gopls feature
--      documentation", …). None of them carries an edit; every one is a command.
--   2. Pick "Browse gopls feature documentation" (<C-n> to it, <CR>).  →  the
--      HANDLER below runs: the message line shows the URL gopls asked to open, and
--      the URL lands in the unnamed register, so `p` pastes it. No request reaches
--      gopls — the editor answered it. Flip `OPEN_IN_BROWSER` below to actually
--      hand it to your browser.
--      `<leader>cd` is the same thing with the chooser skipped: it filters to
--      `gopls.doc`, which this file matches with exactly one action.
--   3. Pick "Show compiler optimization details for …".  →  NOT registered here, so
--      it round-trips: `workspace/executeCommand` goes back to gopls, which acts on
--      it server-side. Nothing local changes; the editor's whole part is delivering
--      it to the right server. That is the default path — you only register the ones
--      the editor must do itself.
--   4. Press `<leader>cl` to list what is registered.  →  `gopls.client_open_url`,
--      the one name this config claimed.
--
-- `vim.lsp.commands` is the same table under the muscle-memory spelling, so a
-- config written either way is seen by the dispatcher.

vim.g.mapleader = "\\"

-- Set to true to really launch your browser. Left off by default so the example
-- can't surprise you with a new tab.
local OPEN_IN_BROWSER = false
-- Tried in order. `nx.run` RESOLVES with `code = -1` when the binary isn't there
-- (it never rejects), so a platform missing the first simply falls through.
local OPENERS = { "xdg-open", "open" }

-- Hand `url` to the first opener that works, reporting loudly when none does —
-- the handler below must not look like it opened something it didn't.
local function open_url(url, i)
  i = i or 1
  local opener = OPENERS[i]
  if not opener then
    nx.notify(
      "could not open " .. url .. " (none of " .. table.concat(OPENERS, ", ") .. " worked)",
      vim.log.levels.ERROR
    )
    return
  end
  nx.run({ cmd = opener, args = { url } }):next(function(r)
    if r.code ~= 0 then
      open_url(url, i + 1)
    end
  end)
end

--------------------------------------------------------------------------------
-- 1. The client-side handler.
--
-- The signature is `function(command, ctx)`:
--   * `command` is the raw LSP `Command` — `{ title, command, arguments }`. The
--     arguments are the server's own shape; gopls sends this one a single URL.
--   * `ctx.client_id` is the client that OFFERED the action. Resolve it when the
--     handler cares who is asking — with `pyright` + `ruff`, or `gopls` beside a
--     linter, the same command name can belong to either.
--
-- Anything the handler throws is reported, not swallowed: a code action that
-- silently does nothing looks like one that worked.
nx.lsp.commands["gopls.client_open_url"] = function(command, ctx)
  local url = command.arguments and command.arguments[1]
  if type(url) ~= "string" then
    nx.notify("gopls asked to open a URL but sent none", vim.log.levels.WARN)
    return
  end
  local client = vim.lsp.get_client_by_id(ctx.client_id)
  local who = client and client.name or ("client " .. tostring(ctx.client_id))

  -- Park it in the unnamed register so `p` pastes it even without a browser.
  nx.reg.set('"', url)

  if OPEN_IN_BROWSER then
    -- Async, like everything in a handler: `nx.run` returns a promise and never
    -- blocks the editor while a browser starts.
    open_url(url)
  else
    nx.print(who .. " → " .. url .. "   (yanked; set OPEN_IN_BROWSER to launch it)")
  end
end

--------------------------------------------------------------------------------
-- 2. Attach gopls. `go.mod` is the root marker — gopls needs the module root
-- before it offers anything, which is why this example ships one.
nx.lsp.config("gopls", {
  cmd = { "gopls" },
  filetypes = { "go" },
  root_markers = { "go.mod", ".git" },
  on_attach = function(_client, bufnr)
    -- Every gopls action on this file is command-carrying, so the chooser here is
    -- entirely made of the two paths above: the one name we claimed, and the rest.
    nx.keymap.set({ "n", "v" }, "<leader>ca", function()
      nx.lsp.code_action()
    end, { buffer = bufnr })

    -- `only` narrows by LSP kind, hierarchically: `gopls.doc` matches the
    -- `gopls.doc.features` action and nothing else here, so exactly one survives —
    -- and `apply` then skips the chooser entirely. The shortest path to watching
    -- the handler fire.
    nx.keymap.set("n", "<leader>cd", function()
      nx.lsp.code_action({ context = { only = { "gopls.doc" } }, apply = true })
    end, { buffer = bufnr })

    nx.keymap.set("n", "K", nx.lsp.hover, { buffer = bufnr })
  end,
})
nx.lsp.enable("gopls")

--------------------------------------------------------------------------------
-- 3. What this config claims. Unregistered names are not "unsupported" — they are
-- the normal case, executed by the server that offered them.
nx.keymap.set("n", "<leader>cl", function()
  local names = {}
  for name in pairs(nx.lsp.commands) do
    names[#names + 1] = name
  end
  table.sort(names)
  if #names == 0 then
    nx.print("nx.lsp.commands: (none registered — every command round-trips)")
  else
    nx.print("nx.lsp.commands: " .. table.concat(names, ", "))
  end
end)
