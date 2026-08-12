-- ~~~ bemtvi --workspace playground: per-directory sessions ~~~
--
-- `--workspace` is a *binary* flag (not a config setting). Launch bemtvi on a
-- directory and it turns that directory into a workspace:
--
--   * it derives a private shada namespace from the directory's absolute path
--     (path separators and other punctuation folded to `-`), so this project's
--     marks / registers / jumplist / search history stay isolated from every
--     other directory — exactly as if you had passed
--     `--shada-namespace <that-derived-token>` by hand;
--   * it saves the window / split / dock / open-buffer layout on exit and
--     restores it on the next launch — no plugin or `btv.shada.save_layout`
--     call needed;
--   * it cds into the workspace directory at startup, so relative paths and
--     `:find` resolve against the project root (pass `--workspace-no-cwd` on the
--     command line to keep the cwd you launched from instead);
--   * it exposes the workspace root to plugins via `btv.workspace`.
--
-- An explicit `--shada-namespace NAME` overrides the derived namespace (the
-- save/restore + `btv.workspace` parts still apply).
--
-- It also works across a remote daemon: `bemtvi --connect-daemon --workspace`
-- (or a `bemtvi://…` target) derives the namespace from the *daemon's* directory,
-- so a remote project gets its own session — with the shada stored locally
-- (default) or on the daemon (`--remote-config`), your choice.
--
-- Run it (from the repo root). Open *this directory* as the workspace:
--
--     BEMTVI_CONFIG=examples/workspace \
--       cargo run -p bemtvi -- --workspace examples/workspace
--
--   1. `:vsplit sample.txt`  — make a split.
--   2. `:qa`                 — quit. The layout is captured into the workspace.
--   3. Relaunch the SAME command — the split comes back automatically.
--
-- Try `\w` any time to see whether you are in a workspace and where its root is.
-- `:pwd` reports the cwd — under `--workspace` it's the workspace root (unless you
-- passed `--workspace-no-cwd`).

vim.g.mapleader = "\\"

--------------------------------------------------------------------------------
-- btv.workspace — the read-only API a plugin uses to detect a workspace launch.
--
--   btv.workspace.active()  -> boolean   (true under `--workspace`)
--   btv.workspace.dir()     -> string?   (the absolute workspace root, or nil)
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>w", function()
  if btv.workspace.active() then
    btv.notify("workspace: " .. btv.workspace.dir(), 2)
    btv.notify("shada namespace: " .. tostring(btv.shada.namespace()), 2)
  else
    btv.notify("not in a workspace (launch with `bemtvi --workspace <dir>`)", 3)
  end
end)

--------------------------------------------------------------------------------
-- btv.wso — per-workspace OPTION overrides. A workspace can pin a global option to a
-- project-specific value that takes PRECEDENCE over the global one (`btv.o`) while the
-- workspace is open, and persists in the workspace shada (re-applied next launch).
-- Only global options qualify (window/buffer options are per-instance).
--
--   btv.wso.foo = v    -- set the override (btv.o.foo then reads v in this workspace)
--   btv.wso.foo = nil  -- clear it (back to the global value)
--   btv.wso.foo        -- read the override, or nil when none
--
-- Try: in a workspace, `\o` flips case-sensitive search just for THIS project; reopen
-- the workspace and it's still set, while other projects keep your global default.
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>o", function()
  if not btv.workspace.active() then
    btv.notify("workspace options need a `--workspace` launch", 3)
    return
  end
  -- Toggle this workspace's `ignorecase` override (independent of the global `btv.o`).
  if btv.wso.ignorecase == nil then
    btv.wso.ignorecase = not btv.o.ignorecase
  else
    btv.wso.ignorecase = not btv.wso.ignorecase
  end
  btv.notify("workspace ignorecase = " .. tostring(btv.wso.ignorecase) .. " (persists)", 2)
end)

-- Greet the user on startup so the example is self-explaining when launched.
btv.autocmd.create("VimEnter", {
  callback = function()
    if btv.workspace.active() then
      btv.notify("workspace ready — layout saves on :qa, restores on relaunch", 2)
    else
      btv.notify("run with `--workspace .` to enable the per-directory session", 3)
    end
  end,
})
