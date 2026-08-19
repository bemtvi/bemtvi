-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/workspace
--
-- `--workspace` is a BINARY flag, and the runner does not pass it — so this
-- session is deliberately the not-in-a-workspace half of every branch the config
-- writes. That half is worth pinning: it is the one a reader who forgets the flag
-- hits, and the notes promise each map says so rather than doing nothing. (The
-- workspace half — the derived shada namespace, the layout round-trip, the
-- persisted `btv.wso` overrides — needs two launches, and is covered natively in
-- the server's workspace suite.)

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- Whatever the next notify is handed while `body` runs.
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

btv.test.describe("examples/workspace", function()
  -- "btv.workspace.active() -> boolean (true under `--workspace`)"
  btv.test.it("btv.workspace reports no workspace without the flag", function(t)
    open(t)
    btv.test.expect(btv.workspace.active()).to_be(false)
    btv.test.expect(btv.workspace.dir()).to_be_nil()
  end)

  -- "Try `\\w` any time to see whether you are in a workspace and where its root is."
  btv.test.it("\\w says so when there is no workspace", function(t)
    open(t)
    local said = notified(function()
      t:feed("<Bslash>w")
    end)
    btv.test.expect(said).to_contain("not in a workspace")
    btv.test.expect(said).to_contain("--workspace")
  end)

  -- "`\\o` flips case-sensitive search just for THIS project" — and says what it
  -- needs when there is no project.
  btv.test.it("\\o refuses outside a workspace rather than doing nothing", function(t)
    open(t)
    local before = btv.o.ignorecase
    local said = notified(function()
      t:feed("<Bslash>o")
    end)
    btv.test.expect(said).to_contain("workspace options need a `--workspace` launch")
    -- The global option is untouched: the map declined, it did not fall back to it.
    btv.test.expect(btv.o.ignorecase).to_be(before)
    btv.test.expect(btv.wso.ignorecase).to_be_nil()
  end)

  -- "btv.wso.foo — read the override, or nil when none"
  btv.test.it("btv.wso reads nil for an option with no override", function(t)
    open(t)
    btv.test.expect(btv.wso.ignorecase).to_be_nil()
    btv.test.expect(btv.wso.hlsearch).to_be_nil()
  end)

  -- "Greet the user on startup so the example is self-explaining when launched."
  btv.test.it("the startup greeting is the no-workspace one", function(t)
    open(t)
    -- The `VimEnter` handler already ran; re-fire it the way the editor would.
    local said = notified(function()
      btv.autocmd.exec("VimEnter", {})
    end)
    btv.test.expect(said).to_contain("run with `--workspace .`")
  end)

  -- The two maps the notes name, on the leader the config sets.
  btv.test.it("the config maps \\w and \\o", function(t)
    open(t)
    btv.test.expect(vim.g.mapleader).to_be("\\")
    local lhs = {}
    for _, m in ipairs(t:keymaps("n")) do
      lhs[m.lhs] = true
    end
    btv.test.expect(lhs["\\w"]).to_be(true)
    btv.test.expect(lhs["\\o"]).to_be(true)
  end)
end)
