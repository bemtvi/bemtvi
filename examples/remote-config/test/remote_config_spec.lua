-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/remote-config
--
-- Whether the files came over a daemon wire or off local disk is a TRANSPORT
-- fact — and the notes' whole point is that once they are materialized they run
-- identically either way. So this suite runs the same config the daemon would
-- serve and checks every one of the "how to SEE that the remote config is live"
-- observables. (The fetch itself is covered natively, in the server's
-- daemon-config suite.)

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:cmd("echo ''")
  t:feed("gg")
end

local function notified(body)
  local got
  local prev_vim, prev_btv = vim.notify, btv.notify
  local record = function(msg)
    got = tostring(msg)
  end
  vim.notify, btv.notify = record, record
  local ok, err = pcall(body)
  vim.notify, btv.notify = prev_vim, prev_btv
  if not ok then
    error(err, 0)
  end
  return got
end

btv.test.describe("examples/remote-config", function()
  -- "`:set tabstop?` — shows 7, the distinctive option set here."
  btv.test.it("the config's distinctive option applies", function(t)
    open(t)
    t:cmd("set tabstop?")
    btv.test.expect(t:message()).to_contain("tabstop=7")
    btv.test.expect(vim.g.mapleader).to_be(" ")
  end)

  -- "`:RemoteHello` — a command defined HERE (init.lua)."
  btv.test.it(":RemoteHello is the command this init.lua defines", function(t)
    open(t)
    local said = notified(function()
      t:cmd("RemoteHello")
    end)
    btv.test.expect(said).to_contain("the remote config is live")
  end)

  -- "`:lua btv.notify(_G.REMOTE_GREETING)` — proves `require` resolved a module
  --  from this config's `lua/` tree."
  btv.test.it("require resolved the module under the config's lua/ tree", function(t)
    open(t)
    btv.test.expect(_G.REMOTE_GREETING).to_be("fetched from the daemon, running locally")
    btv.test.expect(require("remote_mod").greeting()).to_be(_G.REMOTE_GREETING)
  end)

  -- "`:RemotePlugin` — a command from a PLUGIN the daemon served (see pack/)."
  --
  -- The runner sources the spec files and nothing else — it does not walk
  -- `pack/*/start/*` the way a real launch does — so the plugin file is sourced
  -- here, exactly as the materialized runtimepath would.
  btv.test.it(":RemotePlugin comes from the packaged plugin", function(t)
    open(t)
    dofile(DIR .. "/pack/demo/start/remote-demo/plugin/remote-demo.lua")
    local said = notified(function()
      t:cmd("RemotePlugin")
    end)
    btv.test.expect(said).to_contain("came from a plugin the daemon served")
  end)

  -- "On startup you get the notification below ('loaded … from the daemon')."
  btv.test.it("the VimEnter announcement names what was loaded", function(t)
    open(t)
    local said = notified(function()
      btv.autocmd.exec("VimEnter", {})
    end)
    btv.test.expect(said).to_contain("loaded init.lua + plugins fetched from the daemon")
    btv.test.expect(said).to_contain("running locally")
  end)
end)
