-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/ui-open
--
-- `btv.ui.open` hands its argument to the PLATFORM opener, which would really
-- launch a browser — so each case swaps in a recorder for the duration and asserts
-- on the URI the mapping handed over, plus how the config reacts to the result.
-- The opener's own behavior is the OS's, not this config's.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:cmd("echo ''")
  t:feed("gg")
end

--- Run `body` with `btv.ui.open` / `vim.ui.open` recording instead of launching,
--- resolving with `result`. Returns the URIs they were handed.
local function opened(body, result)
  local uris = {}
  local prev_btv, prev_vim = btv.ui.open, vim.ui.open
  local record = function(uri)
    uris[#uris + 1] = uri
    return btv.promise.resolve(result or { code = 0, stdout = "", stderr = "" })
  end
  btv.ui.open, vim.ui.open = record, record
  local ok, err = pcall(body)
  btv.ui.open, vim.ui.open = prev_btv, prev_vim
  if not ok then
    error(err, 0)
  end
  return uris
end

btv.test.describe("examples/ui-open", function()
  -- "1. \\o — open a URL in your browser."
  btv.test.it("\\o hands the site URL to the opener", function(t)
    open(t)
    local uris = opened(function()
      t:feed("<Bslash>o")
    end)
    btv.test.expect(uris).to_equal({ "https://bemtvi.dev" })
  end)

  -- "this echoes a confirmation once it has launched"
  btv.test.it("\\o reports success off the opener's exit code", function(t)
    open(t)
    opened(function()
      t:feed("<Bslash>o")
      t:sleep(20)
    end)
    t:wait_for(function()
      return (t:message() or ""):find("opened", 1, true) ~= nil
    end, { message = "the success branch never fired" })
    btv.test.expect(t:message()).to_be("opened https://bemtvi.dev")
  end)

  -- "a missing opener is `code = -1` … you decide what a failure means"
  btv.test.it("\\o warns when the opener could not run", function(t)
    open(t)
    opened(function()
      t:feed("<Bslash>o")
      t:sleep(20)
    end, { code = -1, stdout = "", stderr = "" })
    t:wait_for(function()
      return (t:message() or ""):find("could not open", 1, true) ~= nil
    end, { message = "the failure branch never fired" })
    btv.test.expect(t:message()).to_contain("opener exit -1")
  end)

  -- "2. \\O — open the file under the cursor (gx-style)."
  btv.test.it("\\O hands over the WORD under the cursor", function(t)
    open(t)
    -- Line 1 of the sample carries the URL the notes tell you to sit on.
    t:feed("gg0")
    local uris = opened(function()
      t:feed("<Bslash>O")
    end)
    btv.test.expect(uris).to_equal({ "https://bemtvi.dev/docs" })
  end)

  btv.test.it("\\O follows the cursor to another WORD", function(t)
    open(t)
    -- `<cWORD>` is whitespace-delimited, so the arrow further along line 1 is a
    -- WORD of its own — proof the map reads the cursor rather than a fixed URL.
    t:feed("gg0")
    t:feed("f<")
    local uris = opened(function()
      t:feed("<Bslash>O")
    end)
    btv.test.expect(uris[1]).to_be("<-")
  end)

  -- "3. \\v — vim.ui.open: the neovim muscle-memory alias (same behavior)."
  btv.test.it("\\v goes through the vim.ui.open alias", function(t)
    open(t)
    local uris = opened(function()
      t:feed("<Bslash>v")
    end)
    btv.test.expect(uris).to_equal({ "https://github.com" })
  end)

  -- "It is PROMISE-ONLY … the call returns at once with a promise"
  btv.test.it("btv.ui.open returns a promise, not a result", function(t)
    open(t)
    local returned
    local prev = btv.ui.open
    btv.ui.open = function(uri)
      return prev(uri)
    end
    -- Call the real one against a URI nothing will act on, and check its shape
    -- rather than waiting for an opener that may not exist on this machine.
    returned = btv.ui.open("about:blank")
    btv.ui.open = prev
    btv.test.expect(type(returned)).to_be("table")
    btv.test.expect(type(returned.next)).to_be("function")
    btv.test.expect(type(returned.catch)).to_be("function")
  end)

  -- "the neovim muscle-memory alias `vim.ui.open(path)` drives the same path"
  btv.test.it("vim.ui.open takes the same argument and returns a promise", function(t)
    open(t)
    local got = vim.ui.open("about:blank")
    btv.test.expect(type(got)).to_be("table")
    btv.test.expect(type(got.next)).to_be("function")
  end)
end)
