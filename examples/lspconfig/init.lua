-- ~~~ bemtvi-lspconfig — 407 language servers, configured, with nothing to write ~~~
--
-- Run it (from the repo root) — needs `lua-language-server` on your PATH:
--
--     BEMTVI_CONFIG=examples/lspconfig \
--       cargo run -p bemtvi -- examples/lspconfig/sample.lua
--
-- The FIRST run clones the plugin: bemtvi will report it as missing, so run
-- `:PluginSync`, wait for it to finish, then `:q` and start again. After that the
-- clone is cached under your data dir and startup is immediate.
--
-- ---------------------------------------------------------------------------
-- What this example is about
--
-- `btv.lsp` is the control surface: it spawns servers, syncs documents, routes
-- requests, merges diagnostics. What it deliberately does NOT carry is the
-- knowledge of how to run any *particular* server — which binary, which
-- filetypes, which directory is "the project", which settings that server
-- expects. That knowledge is data, it is per-server, and there are 407 of them.
--
-- bemtvi-lspconfig is that data: a native port of nvim-lspconfig, one curated
-- table per server, rewritten against `btv.*` so nothing blocks the editor. Put it
-- on the runtimepath and `btv.lsp.enable("lua_ls")` is the whole configuration —
-- no `cmd`, no `filetypes`, no root logic.
--
-- The thing to notice while you work through the steps: every question a config
-- has to answer about your project — "is there a project-local copy of this
-- binary?", "where does this project root?", "does this package.json depend on
-- tailwind?" — is a filesystem question, and upstream answers all of them with
-- blocking calls that stop the editor. Here they are promises, awaited before the
-- spawn. You never see it, which is the point.
--
-- Type this / see that:
--
--   1. `:LspInfo` once the server is up.  →  `lua_ls`, its root, its capabilities
--      and the log path. Nothing in this file said how to start it (step 1 below
--      is one line); the `cmd`, the `filetypes` and the root markers all came off
--      the bundled `lsp/lua_ls.lua`.
--   2. `:LspInfo` and read the ROOT.  →  this repo's root, found by `.git`. The
--      bundled config declares THREE tiers of markers — `.luarc.json`-class
--      first, then `.stylua.toml`-class, then `.git` last. Each tier is exhausted
--      over the whole tree before the next is tried anywhere, so a `.git` one
--      directory up never beats a `.luarc.json` six directories up. That ordering
--      is why a package inside a monorepo attaches at the monorepo.
--   3. Put the cursor on `btv.buf` (line 8 of sample.lua) and press `K`.  →  a
--      hover float. Then `gd` on `helper` (line 20)  →  jumps to its definition.
--      Both are bemtvi's built-in maps, installed buffer-local on attach.
--   4. Look at line 12 of sample.lua.  →  a diagnostic: `undefined_global`. The
--      bundled settings turned `hint` and `codeLens` on; section 3 below is what
--      taught the server that `btv` is real, and the *other* undefined name is not.
--   5. `grn` on `helper` and type a new name.  →  rename across the file. `gra`
--      offers code actions, `grr` lists references, `gO` lists the file's symbols.
--      Those are this plugin's keymaps (section 4), on top of bemtvi's core set.
--   6. `<leader>lh`.  →  inlay hints toggle on: parameter names and inferred
--      types appear inline. Section 4 turned them on at attach.
--   7. `:LspStop`.  →  the server stops AND is disabled, so it does not silently
--      come back on the next Lua buffer. `:LspStart` brings it back with the
--      config in force now. `:LspRestart` is the one to use after editing
--      `btv.lsp.config` — most servers read their settings only at `initialize`.
--   8. `:lua print(#require("bemtvi-lspconfig").available())`.  →  `407`. And
--      `:lua print(table.concat(require("bemtvi-lspconfig").for_filetype("lua"), ", "))`
--      →  every bundled server that serves Lua, this config's override included.
--   9. `:help bemtvi-lspconfig`.  →  the full reference. (Needs the bemtvi-help
--      plugin, declared below, and `:BtvHelptags ALL` once.)

------------------------------------------------------------------------------
-- 1. Install the plugin, and enable ONE server.
--
--    This is the whole native path. `btv.lsp` reads `lsp/<name>.lua` straight off
--    the runtimepath, so there is no `require`, no registration, and no `cmd` to
--    write. `btv.lsp.enable` may be called before or after `btv.lsp.config`, and a
--    late enable serves buffers that are already open.
------------------------------------------------------------------------------
btv.plugins({
  { "bemtvi/bemtvi-lspconfig" },
  { "bemtvi/bemtvi-help" }, -- so step 9's `:help bemtvi-lspconfig` works
})

btv.lsp.enable("lua_ls")

------------------------------------------------------------------------------
-- 2. Override a bundled config.
--
--    Your table is deep-merged OVER the bundled one and wins. You are not
--    replacing the config — `cmd`, `filetypes` and the root-marker tiers all
--    still come from the plugin; only what you name here changes.
------------------------------------------------------------------------------
btv.lsp.config("lua_ls", {
  settings = {
    Lua = {
      -- bemtvi's Lua is PUC 5.4 — NOT LuaJIT. Telling the server otherwise makes
      -- it offer completions for a runtime you are not running.
      runtime = { version = "Lua 5.4", path = { "lua/?.lua", "lua/?/init.lua" } },
      -- `btv` is the plugin API and `vim` is the bounded compat surface; without
      -- this every `btv.*` call in your own config reads as an undefined global.
      -- The OTHER undefined name in sample.lua still reports, which is how you
      -- can tell this line did something.
      diagnostics = { globals = { "btv", "vim" } },
      hint = { enable = true, arrayIndex = "Disable" },
    },
  },
})

------------------------------------------------------------------------------
-- 3. The "*" layer — settings every server inherits.
--
--    Applied to all of them at once, and merged UNDER each server's own config,
--    so a per-server override still wins. Useful for capabilities you broadcast
--    everywhere (a completion plugin's, say) and for an `on_attach` you want on
--    every buffer regardless of which server attached.
------------------------------------------------------------------------------
btv.lsp.config("*", {
  on_attach = function(client, bufnr)
    -- Runs for EVERY server on every buffer. `client` is the handle: its name,
    -- its negotiated `offset_encoding`, `supports_method`, `exec_cmd`.
    btv.notify(("LSP: %s attached to buffer %d"):format(client.name, bufnr))
  end,
})

------------------------------------------------------------------------------
-- 4. The convenience path: setup().
--
--    Everything above spelled as one call — plus the extended keymap set and the
--    inlay-hint toggle. It is additive, so calling it here does not undo section
--    2; `servers` is left out because section 1 already enabled what we want.
--
--    The keymaps it installs (on top of bemtvi's own gd/gD/gr/K/<C-k>):
--
--      grn  rename          gri  implementation   <leader>ls  workspace symbols
--      gra  code action     grt  type definition  <leader>lf  format buffer
--      grr  references      gO   document symbols <leader>lh  toggle inlay hints
--      <C-]>  go to definition (normal)  <C-s>  signature help (insert/select)
--
--    The last two are alternative SPELLINGS of maps the core already has — `<C-]>`
--    is the tag jump, with a language server standing in for a tags file, and
--    `<C-s>` is neovim's i_CTRL-S beside bemtvi's `<C-k>`. The core's built-in set
--    stays the small one; the muscle-memory aliases live in the plugin.
--
--    All at the OVERRIDABLE rung, so your own mapping for any of them wins —
--    whether you set it before or after this call.
------------------------------------------------------------------------------
require("bemtvi-lspconfig").setup({
  keymaps = true,
  inlay_hints = true,
})

------------------------------------------------------------------------------
-- 5. Writing your own config, with the same helpers the 407 use.
--
--    A config is a plain table; `lsp/<name>.lua` anywhere on the runtimepath is
--    picked up with no registration. `util` is the async helper surface — every
--    member that touches the filesystem or runs a program returns a PROMISE, and
--    `btv.lsp` awaits `cmd` / `root_dir` / `before_init` before it spawns.
--
--    This one is not enabled (there is no such server to run); it is here as the
--    shape to copy. Uncomment the `btv.lsp.config` call to see it rejected loudly
--    for a missing binary rather than silently doing nothing.
------------------------------------------------------------------------------
local util = require("bemtvi-lspconfig.util")

-- btv.lsp.config("my_ls", {
--   -- Prefer the project's own node_modules/.bin copy, else $PATH. The lookup is
--   -- I/O; upstream's version of this blocks the editor to do it.
--   cmd = util.node_cmd("my-language-server"),
--   filetypes = { "mylang" },
--   -- Priority TIERS: the lockfile tier is exhausted over the whole tree before
--   -- `.git` is considered anywhere.
--   root_markers = { { "package-lock.json", "yarn.lock" }, { ".git" } },
--   -- A root_dir may await as much I/O as the decision needs. Calling on_dir(nil)
--   -- means "no root found"; returning WITHOUT calling it declines the buffer
--   -- outright, which is how a config steps aside for another server.
--   root_dir = util.root_dir(function(bufnr, on_dir)
--     local pkg = btv.await(util.find_upward(util.bufname(bufnr), "package.json"))
--     if not pkg then
--       return -- decline: not a JavaScript project at all
--     end
--     on_dir(util.dirname(pkg))
--   end),
-- })

------------------------------------------------------------------------------
-- 6. A few conveniences for driving the steps above.
------------------------------------------------------------------------------
-- <leader>li — how many servers are on this buffer, and which.
btv.keymap.set("n", "<leader>li", function()
  local names = {}
  for _, c in ipairs(btv.lsp.clients({ bufnr = 0 })) do
    names[#names + 1] = c.name .. " (" .. c.offset_encoding .. ")"
  end
  btv.notify(
    #names
      .. " client(s): "
      .. (table.concat(names, ", ") ~= "" and table.concat(names, ", ") or "none")
  )
end, { desc = "LSP: clients on this buffer" })

-- <leader>lr — where did it decide the project root is?
btv.keymap.set("n", "<leader>lr", function()
  btv.async(function()
    local cfg = btv.lsp.get_config("lua_ls")
    local root = btv.await(util.root(0, cfg.root_markers))
    btv.notify("lua_ls root markers resolved to: " .. tostring(root))
  end)()
end, { desc = "LSP: resolve this buffer's root" })

btv.o.number = true
btv.o.signcolumn = "yes"
