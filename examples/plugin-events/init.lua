-- Plugin lifecycle events: PluginsLoaded + PluginLoaded
--
-- Run this example (config isolated from your real one):
--
--   NXVIM_CONFIG=examples/plugin-events cargo run -p nxvim -- examples/plugin-events/sample.txt
--
-- It declares three tiny *local-dir* plugins that live next to this file (no network,
-- no :PluginSync) — two eager (alpha, beta) and one lazy (gamma, behind :GammaHello) —
-- then hooks the two events the plugin manager fires.

-- Where this config lives, so the plugins' `dir` paths resolve to an absolute path.
local here = vim.fn.stdpath("config")

------------------------------------------------------------------------------
-- 1. Declare the plugins.
--    alpha & beta are EAGER (no trigger) — they load at startup.
--    gamma is LAZY — it loads the first time you run :GammaHello.
------------------------------------------------------------------------------
nx.plugins({
  {
    name = "alpha",
    dir = here .. "/demo-plugins/alpha",
    -- An ASYNC config (it nx.awaits a file read). `PluginsLoaded` waits for this to
    -- finish, not merely for the load to start.
    config = function()
      local txt = nx.await(nx.fs.read_text(here .. "/demo-plugins/alpha/lua/alpha/init.lua"))
      _G.alpha_bytes = #txt
      require("alpha").setup()
    end,
  },
  {
    name = "beta",
    dir = here .. "/demo-plugins/beta",
    config = function()
      require("beta").setup()
    end,
  },
  {
    name = "gamma",
    dir = here .. "/demo-plugins/gamma",
    cmd = "GammaHello", -- a trigger ⇒ lazy; loads on first :GammaHello
    config = function()
      require("gamma").setup()
    end,
  },
})

------------------------------------------------------------------------------
-- 2. PluginsLoaded — "all my eager plugins are ready".
--    Fires ONCE, after every eager plugin (alpha AND beta) has fully settled —
--    including alpha's async config. Do cross-plugin setup that needs several
--    plugins present here.
--    Type-this / see-that: on startup you should see the notification below, and
--    `:lua print(_G.alpha_ready, _G.beta_ready, _G.alpha_bytes)` prints
--    `true  true  <a number>` — proof both eager configs finished before it fired.
------------------------------------------------------------------------------
nx.on("PluginsLoaded", {}, function()
  nx.notify(
    string.format(
      "PluginsLoaded: eager plugins ready (alpha=%s, beta=%s)",
      tostring(_G.alpha_ready),
      tostring(_G.beta_ready)
    ),
    2
  )
end)

------------------------------------------------------------------------------
-- 3. PluginLoaded — hook ONE specific plugin's load (works for lazy ones too).
--    The event's `pattern` is the plugin name, so this handler only fires for gamma.
--    Type-this / see-that: gamma is lazy, so nothing happens at startup. Run
--    `:GammaHello` — you'll see gamma's own greeting AND the notification below,
--    then `:lua print(_G.gamma_ready)` prints `true`.
------------------------------------------------------------------------------
nx.on("PluginLoaded", { pattern = "gamma" }, function(ev)
  nx.notify("PluginLoaded: '" .. ev.data.name .. "' just loaded", 2)
end)

------------------------------------------------------------------------------
-- 4. A plugin's buffer events still see the STARTUP file.
--    Plugins load asynchronously, so beta's config runs after sample.txt has
--    already been read — a `BufReadPost` handler registered there would seem to
--    have missed it. It hasn't: every read from before the plugins landed is
--    replayed to the handlers they registered, once PluginsLoaded fires.
--    Beta registers plain `BufReadPost` / `FileType` handlers (see
--    demo-plugins/beta/lua/beta/init.lua) — no nxvim-specific event, no sweep.
--
--    Type-this / see-that: start with the command line at the top of this file,
--    then run
--      :lua print(#_G.beta_reads, _G.beta_reads[1])
--    It prints `1  <...>/sample.txt` — beta saw the startup file even though it
--    loaded after it was read. Open another file (`:e demo-plugins/alpha/lua/alpha/init.lua`)
--    and run it again: `2  <...>/sample.txt`, the second read appended normally.
--    Nothing fires twice — the handler below, registered here in init.lua BEFORE
--    the read, sees sample.txt exactly once:
--      :lua print(_G.cfg_reads)   -->  1
------------------------------------------------------------------------------
_G.cfg_reads = 0
nx.on("BufReadPost", {}, function()
  _G.cfg_reads = _G.cfg_reads + 1
end)
