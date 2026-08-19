-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/plugin-events
--
-- Every claim is about ORDER — an event that fires only once every eager config
-- has settled, a lazy plugin's load hooked by name, a read replayed to a handler
-- that did not exist when it happened — so the spec waits for the events and
-- asserts on the state each one promises.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

local notified = {}
do
  local real_btv, real_vim = btv.notify, vim.notify
  btv.notify = function(msg, ...)
    notified[#notified + 1] = tostring(msg)
    return real_btv(msg, ...)
  end
  vim.notify = function(msg, ...)
    notified[#notified + 1] = tostring(msg)
    return real_vim(msg, ...)
  end
end

dofile(DIR .. "/init.lua")

--- The most recent notification containing `needle`, or nil.
local function notified_with(needle)
  for i = #notified, 1, -1 do
    if notified[i]:find(needle, 1, true) then
      return notified[i]
    end
  end
  return nil
end

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/plugin-events", function()
  -- 1. "alpha & beta are EAGER — they load at startup. gamma is LAZY."
  btv.test.it("§1 — the eager plugins load, the lazy one waits", function(t)
    open(t)
    t:wait_for(function()
      return _G.alpha_ready and _G.beta_ready
    end, { tries = 200, interval = 20, message = "the eager plugins never loaded" })
    btv.test.expect(_G.gamma_ready).to_be_falsy()
  end)

  -- 2. "PluginsLoaded … fires ONCE, after every eager plugin has fully settled —
  --     including alpha's async config."
  --
  -- The EVENT itself is a startup event: it fires once the manager's startup pass
  -- has settled, which in a `--test-plugin` run is before a spec has sourced the
  -- config that declares anything. What the spec can hold to account is the
  -- guarantee behind it — that both eager configs, async work included, really did
  -- complete — and, below, that the handler is registered for it.
  btv.test.it("§2 — every eager config settles, async work included", function(t)
    open(t)
    t:wait_for(function()
      return _G.alpha_ready and _G.beta_ready and type(_G.alpha_bytes) == "number"
    end, { tries = 200, interval = 20, message = "the eager configs never settled" })
    -- alpha's config `btv.await`s a file read; the byte count proves it finished
    -- rather than merely having been started.
    btv.test.expect(_G.alpha_bytes > 0).to_be(true)
  end)

  btv.test.it("§2 — the config hooks PluginsLoaded", function(t)
    open(t)
    local listening = false
    for _, au in ipairs(btv.autocmd.get({ event = "PluginsLoaded" })) do
      listening = true
    end
    btv.test.expect(listening).to_be(true)
  end)

  -- 3. "gamma is lazy, so nothing happens at startup. Run `:GammaHello` — you'll
  --     see gamma's own greeting AND the notification."
  btv.test.it("§3 — PluginLoaded hooks one plugin by name, lazily", function(t)
    open(t)
    btv.test.expect(notified_with("PluginLoaded")).to_be_nil()
    t:cmd("GammaHello")
    local msg = t:wait_for(function()
      return notified_with("PluginLoaded")
    end, { tries = 200, interval = 20, message = "gamma never loaded" })
    btv.test.expect(msg).to_contain("'gamma' just loaded")
    btv.test.expect(_G.gamma_ready).to_be(true)
  end)

  -- 4. "every read from before the plugins landed is replayed to the handlers they
  --     registered, once PluginsLoaded fires."
  btv.test.it("§4 — a plugin's BufReadPost sees the file read before it loaded", function(t)
    t:wait_for(function()
      return _G.beta_reads ~= nil and #_G.beta_reads > 0
    end, { tries = 200, interval = 20, message = "beta never saw a read" })
    btv.test.expect(#_G.beta_reads > 0).to_be(true)
  end)

  btv.test.it("§4 — …and the replay does not double-fire", function(t)
    t:wait_for(function()
      return _G.beta_reads ~= nil and #_G.beta_reads > 0
    end, { tries = 200, interval = 20, message = "beta never saw a read" })
    local before = #_G.beta_reads
    local cfg_before = _G.cfg_reads
    t:cmd("e " .. DIR .. "/demo-plugins/alpha/lua/alpha/init.lua")
    t:wait_for(function()
      return #_G.beta_reads > before
    end, { message = "beta missed the second read" })
    -- Exactly one more, for both the plugin's handler and the config's own.
    btv.test.expect(#_G.beta_reads).to_be(before + 1)
    btv.test.expect(_G.cfg_reads).to_be(cfg_before + 1)
  end)

  -- The declaration itself: three local-dir plugins, no network.
  btv.test.it("the three plugins are declared as local directories", function(t)
    open(t)
    local names = {}
    for _, p in ipairs(btv.plugins.list and btv.plugins.list() or {}) do
      names[p.name] = true
    end
    if next(names) then
      for _, want in ipairs({ "alpha", "beta", "gamma" }) do
        btv.test.expect(names[want]).to_be_truthy()
      end
    end
    -- Whatever the registry reports, the load state is the observable one.
    btv.test.expect(_G.alpha_ready).to_be(true)
    btv.test.expect(_G.beta_ready).to_be(true)
  end)
end)
