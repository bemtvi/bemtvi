-- ~~~ nxvim --workspace playground: per-directory sessions ~~~
--
-- `--workspace` is a *binary* flag (not a config setting). Launch nxvim on a
-- directory and it turns that directory into a workspace:
--
--   * it derives a private shada namespace from the directory's absolute path
--     (path separators and other punctuation folded to `-`), so this project's
--     marks / registers / jumplist / search history stay isolated from every
--     other directory — exactly as if you had passed
--     `--shada-namespace <that-derived-token>` by hand;
--   * it saves the window / split / dock / open-buffer layout on exit and
--     restores it on the next launch — no plugin or `nx.shada.save_layout`
--     call needed;
--   * it exposes the workspace root to plugins via `nx.workspace`.
--
-- An explicit `--shada-namespace NAME` overrides the derived namespace (the
-- save/restore + `nx.workspace` parts still apply).
--
-- It also works across a remote daemon: `nxvim --connect-daemon --workspace`
-- (or a `nxvim://…` target) derives the namespace from the *daemon's* directory,
-- so a remote project gets its own session — with the shada stored locally
-- (default) or on the daemon (`--remote-config`), your choice.
--
-- Run it (from the repo root). Open *this directory* as the workspace:
--
--     NXVIM_CONFIG=examples/workspace \
--       cargo run -p nxvim -- --workspace examples/workspace
--
--   1. `:vsplit sample.txt`  — make a split.
--   2. `:qa`                 — quit. The layout is captured into the workspace.
--   3. Relaunch the SAME command — the split comes back automatically.
--
-- Try `\w` any time to see whether you are in a workspace and where its root is.

vim.g.mapleader = "\\"

--------------------------------------------------------------------------------
-- nx.workspace — the read-only API a plugin uses to detect a workspace launch.
--
--   nx.workspace.active()  -> boolean   (true under `--workspace`)
--   nx.workspace.dir()     -> string?   (the absolute workspace root, or nil)
--------------------------------------------------------------------------------
nx.keymap.set("n", "<leader>w", function()
  if nx.workspace.active() then
    nx.notify("workspace: " .. nx.workspace.dir(), 2)
    nx.notify("shada namespace: " .. tostring(nx.shada.namespace()), 2)
  else
    nx.notify("not in a workspace (launch with `nxvim --workspace <dir>`)", 3)
  end
end)

--------------------------------------------------------------------------------
-- nx.wso — per-workspace OPTION overrides. A workspace can pin a global option to a
-- project-specific value that takes PRECEDENCE over the global one (`nx.o`) while the
-- workspace is open, and persists in the workspace shada (re-applied next launch).
-- Only global options qualify (window/buffer options are per-instance).
--
--   nx.wso.foo = v    -- set the override (nx.o.foo then reads v in this workspace)
--   nx.wso.foo = nil  -- clear it (back to the global value)
--   nx.wso.foo        -- read the override, or nil when none
--
-- Try: in a workspace, `\o` flips case-sensitive search just for THIS project; reopen
-- the workspace and it's still set, while other projects keep your global default.
--------------------------------------------------------------------------------
nx.keymap.set("n", "<leader>o", function()
  if not nx.workspace.active() then
    nx.notify("workspace options need a `--workspace` launch", 3)
    return
  end
  -- Toggle this workspace's `ignorecase` override (independent of the global `nx.o`).
  if nx.wso.ignorecase == nil then
    nx.wso.ignorecase = not nx.o.ignorecase
  else
    nx.wso.ignorecase = not nx.wso.ignorecase
  end
  nx.notify("workspace ignorecase = " .. tostring(nx.wso.ignorecase) .. " (persists)", 2)
end)

-- Greet the user on startup so the example is self-explaining when launched.
nx.autocmd.create("VimEnter", {
  callback = function()
    if nx.workspace.active() then
      nx.notify("workspace ready — layout saves on :qa, restores on relaunch", 2)
    else
      nx.notify("run with `--workspace .` to enable the per-directory session", 3)
    end
  end,
})
