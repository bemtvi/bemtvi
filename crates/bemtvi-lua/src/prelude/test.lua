-- btv.test — a native test framework for bemtvi plugins.
--
-- Plugins are pure Lua (ADR 0002), so their tests are too: `btv.test` lets a plugin
-- repo carry a `test/*_spec.lua` suite that drives a REAL editor and asserts on its
-- state, run headlessly by `bemtvi --test-plugin <dir>`. It is the Lua sibling of the
-- Rust black-box harness (`crates/bemtvi-test-harness`): same philosophy (feed vim
-- keys, assert on the resulting buffer / cursor / UI), reachable from a plugin's own
-- repo.
--
--   btv.test.describe("my-plugin", function()
--     btv.test.before_each(function() require("my-plugin").setup({}) end)
--     btv.test.it("does the thing", function(t)
--       t:feed("itext<Esc>")               -- async under the hood: settles the tick
--       btv.test.expect(t:lines()).to_equal({ "text" })
--       btv.test.expect(t:mode()).to_be("n")
--     end)
--   end)
--
-- THE TICK MODEL (why the context methods are async). Fed keys settle at the END of
-- a tick, and the Lua state mirrors (btv._bufs / btv._cur_cursor / …) refresh BEFORE
-- each Lua entry — so a single synchronous chunk that feeds then reads sees stale
-- state. Every `it` body therefore runs inside an `btv.async` coroutine, and the
-- context's driving methods `btv.await` internally: `t:feed` queues the keys then
-- awaits one tick, so the keys drain and the mirrors refresh before the next line.
-- Deterministic (synchronous) input settles in one tick; ASYNC effects (a debounced
-- popup, a timer) need `t:wait_for(predicate)`. This reuses bemtvi's existing
-- async/promise/timer machinery wholesale — no new scheduling primitives.

local M = {}

-- btv.test is GATED. The framework is built here, but exposed only through
-- `btv._install_test`, which the server calls when the `bemtvi --test-plugin` runner
-- turns on test mode (the `btv_enable_test_mode` RPC). In a normal editor session
-- `btv.test` stays nil, so a config or plugin can't depend on the test API — and the
-- UI mirror it reads (`btv._ui`) is likewise only populated under `--test-plugin`.
function btv._install_test()
  btv.test = M
end

-- ----- the suite tree -------------------------------------------------------
-- describe blocks nest into a tree; each holds its own before_each/after_each and
-- the tests declared directly in it. Hooks are resolved at RUN time by walking a
-- test's ancestor chain (outer→inner for before, inner→outer for after), so a
-- before_each declared AFTER an `it` in the same block still applies to it — busted
-- semantics, order-independent.

local function new_node(name, parent)
  return {
    name = name,
    parent = parent,
    children = {},
    tests = {},
    before_each = {},
    after_each = {},
  }
end

local root = new_node(nil, nil)
local current = root

-- reset() — drop every registered test/hook (and the last results). The runner calls
-- it before sourcing a fresh set of spec files.
function M.reset()
  root = new_node(nil, nil)
  current = root
  M._results = nil
  M._done = false
end

function M.describe(name, fn)
  if type(name) ~= "string" then
    error("btv.test.describe: name must be a string", 2)
  end
  if type(fn) ~= "function" then
    error("btv.test.describe: body must be a function", 2)
  end
  local node = new_node(name, current)
  current.children[#current.children + 1] = node
  local prev = current
  current = node
  local ok, err = pcall(fn)
  current = prev
  if not ok then
    error(err, 0)
  end
end

function M.it(name, fn)
  if type(name) ~= "string" then
    error("btv.test.it: name must be a string", 2)
  end
  if type(fn) ~= "function" then
    error("btv.test.it: body must be a function", 2)
  end
  current.tests[#current.tests + 1] = { name = name, fn = fn, node = current }
end

function M.before_each(fn)
  current.before_each[#current.before_each + 1] = fn
end

function M.after_each(fn)
  current.after_each[#current.after_each + 1] = fn
end

-- Flatten the tree into an ordered list of runnable cases, each carrying its
-- describe-name path and its resolved before/after hook chains.
local function flatten()
  local out = {}
  local function walk(node, path, befores)
    local p = { table.unpack(path) }
    if node.name then
      p[#p + 1] = node.name
    end
    local bs = { table.unpack(befores) }
    for _, b in ipairs(node.before_each) do
      bs[#bs + 1] = b
    end
    for _, t in ipairs(node.tests) do
      -- after_each runs inner→outer: this node's, then each ancestor's.
      local afs = {}
      local n = node
      while n do
        for _, a in ipairs(n.after_each) do
          afs[#afs + 1] = a
        end
        n = n.parent
      end
      out[#out + 1] = { name = t.name, fn = t.fn, path = p, befores = bs, afters = afs }
    end
    for _, c in ipairs(node.children) do
      walk(c, p, bs)
    end
  end
  walk(root, {}, {})
  return out
end

-- ----- value rendering + deep equality (for assertions) ---------------------

local function render(v, seen)
  local tv = type(v)
  if tv == "string" then
    return string.format("%q", v)
  elseif tv ~= "table" then
    return tostring(v)
  end
  seen = seen or {}
  if seen[v] then
    return "<cycle>"
  end
  seen[v] = true
  local parts = {}
  local n = #v
  for i = 1, n do
    parts[#parts + 1] = render(v[i], seen)
  end
  for k, val in pairs(v) do
    if not (type(k) == "number" and k >= 1 and k <= n and k == math.floor(k)) then
      parts[#parts + 1] = string.format("%s = %s", tostring(k), render(val, seen))
    end
  end
  seen[v] = nil
  return "{ " .. table.concat(parts, ", ") .. " }"
end

local function deep_eq(a, b)
  if a == b then
    return true
  end
  if type(a) ~= "table" or type(b) ~= "table" then
    return false
  end
  for k, v in pairs(a) do
    if not deep_eq(v, b[k]) then
      return false
    end
  end
  for k in pairs(b) do
    if a[k] == nil then
      return false
    end
  end
  return true
end

-- An assertion failure is raised as a tagged table so the runner can tell a clean
-- expectation miss from an unexpected Lua error (a nil index, a typo).
local function fail(msg)
  -- Built as a variable (not an `error({…})` literal) so static linters that type
  -- `error`'s argument as a string don't flag it; Lua raises any value fine, and
  -- btv.await re-raises promise rejections the same way (a table with `.message`).
  local e = { __btvtest_fail = true, message = msg }
  error(e, 0)
end

-- ----- expect ---------------------------------------------------------------

-- expect(value).to_*(…) / expect(value).never.to_*(…). Each matcher throws a tagged
-- failure on mismatch. `.never` flips the sense (and the message).
function M.expect(value)
  local function build(negated)
    local api = {}
    local function check(cond, msg)
      if negated then
        cond = not cond
      end
      if not cond then
        fail((negated and "expected NOT: " or "") .. msg)
      end
    end
    function api.to_equal(other)
      check(deep_eq(value, other), ("expected %s to equal %s"):format(render(value), render(other)))
    end
    function api.to_be(other)
      check(value == other, ("expected %s to be %s"):format(render(value), render(other)))
    end
    function api.to_contain(needle)
      if type(value) == "string" then
        check(
          value:find(needle, 1, true) ~= nil,
          ("expected %q to contain %q"):format(value, needle)
        )
      elseif type(value) == "table" then
        local found = false
        for _, v in ipairs(value) do
          if deep_eq(v, needle) then
            found = true
            break
          end
        end
        check(found, ("expected %s to contain %s"):format(render(value), render(needle)))
      else
        fail("to_contain expects a string or list, got " .. type(value))
      end
    end
    function api.to_match(pattern)
      check(
        type(value) == "string" and value:find(pattern) ~= nil,
        ("expected %s to match pattern %q"):format(render(value), tostring(pattern))
      )
    end
    function api.to_be_truthy()
      check(value and true or false, ("expected %s to be truthy"):format(render(value)))
    end
    function api.to_be_falsy()
      check(not value, ("expected %s to be falsy"):format(render(value)))
    end
    function api.to_be_nil()
      check(value == nil, ("expected %s to be nil"):format(render(value)))
    end
    -- expect(fn).to_error([substr]) — `value` must be a function; calls it and
    -- asserts it raised (optionally that the message contains `substr`).
    function api.to_error(substr)
      if type(value) ~= "function" then
        fail("to_error expects a function, got " .. type(value))
      end
      local ok, err = pcall(value)
      local msg = type(err) == "table" and err.message or tostring(err)
      if substr then
        check(
          not ok and msg:find(substr, 1, true) ~= nil,
          ("expected a thrown error containing %q, got %s"):format(
            substr,
            ok and "no error" or render(msg)
          )
        )
      else
        check(not ok, "expected the function to raise an error")
      end
    end
    return api
  end
  local api = build(false)
  api.never = build(true)
  return api
end

-- ----- the per-test context -------------------------------------------------
-- `t`, passed to each `it` body. Driving methods are async (await a tick); read
-- methods are plain mirror reads, correct because they run after an await.

local Ctx = {}
Ctx.__index = Ctx

-- Settle the editor: yield exactly `n` ticks (default 1) so queued input drains and
-- the Rust→Lua mirrors refresh before the test reads them.
local function settle(n)
  for _ = 1, (n or 1) do
    btv.await(btv.promise.new(function(resolve)
      btv.on_next_tick(function()
        resolve(true)
      end)
    end))
  end
end

-- t:feed(keys[, opts]) — type vim key-notation, then settle one tick. opts.remap
-- (default true, so the plugin's own mappings fire), opts.insert (default false),
-- opts.settle (extra ticks to wait, for chained async). Returns self for chaining.
function Ctx:feed(keys, opts)
  opts = opts or {}
  btv._feedkeys(keys, opts.remap ~= false, opts.insert or false)
  settle(opts.settle or 1)
  return self
end

-- t:cmd(excmd) — run an ex-command, then settle a tick.
function Ctx:cmd(excmd)
  vim.cmd(excmd)
  settle(1)
  return self
end

-- t:exec(fn) — run `fn` now (mirrors already fresh) and return its value. `fn` may
-- itself await (it runs in the test coroutine).
function Ctx:exec(fn)
  return fn()
end

-- t:wait_for(predicate[, opts]) — await until `predicate` returns truthy (polling
-- between ticks), returning that value. Rejects (raising in the test) on timeout.
-- Use for async UI: a debounced popup, a watch-driven refresh. opts = { tries=,
-- interval=, message= } (see btv.wait_for).
function Ctx:wait_for(predicate, opts)
  return btv.await(btv.wait_for(predicate, opts))
end

-- t:sleep(ms) — await a wall-clock delay (for genuinely time-based behavior).
function Ctx:sleep(ms)
  btv.await(btv.promise.delay(ms))
  return self
end

-- ----- hermetic seams (clipboard / temp fs) --------------------------------
-- These let a suite exercise a plugin's clipboard / file I/O against test doubles
-- instead of the host. `btv.test.clipboard` is backed by the in-memory clipboard the
-- runner installs in test mode (the `"+` / `"*` registers round-trip through it).

M.clipboard = {}

-- seed(text[, linewise]) — put `text` on the clipboard as if an external app set it,
-- so a plugin that reads `"+` / `"*` sees it. Settles a tick so the seed lands before
-- the next driving call. Call inside an `it` body (it awaits).
function M.clipboard.seed(text, linewise)
  btv._test_clipboard_seed(text, linewise and true or false)
  settle(1)
end

-- peek() — the clipboard's current contents as `text, linewise` (what a plugin wrote
-- to `"+` / `"*`), or nil when empty.
function M.clipboard.peek()
  local ui = btv._ui
  local c = ui and ui.clipboard
  if not c then
    return nil
  end
  return c.text, c.linewise
end

-- clear() — empty the clipboard.
function M.clipboard.clear()
  M.clipboard.seed("", false)
end

-- tempdir() — a fresh, unique temp directory (already created) for a suite that
-- touches the filesystem, so runs don't collide. Pair with btv.fs to read/write it.
function M.tempdir()
  return btv._test_tempdir()
end

-- ----- state reads (post-await mirror reads) --------------------------------

function Ctx:lines(first, last)
  return btv.buf.lines(0, first or 0, last or -1, false)
end

function Ctx:line(n)
  local ls = btv.buf.lines(0, (n or 1) - 1, n or 1, false)
  return ls[1]
end

function Ctx:current_line()
  return btv.current_line()
end

function Ctx:cursor()
  return btv.cursor.get()
end

-- The short mode string ("n", "i", "v", …). `btv.mode()` returns the
-- nvim_get_mode table; tests almost always want the code, so unwrap it. Use
-- `t:mode_info()` for the full `{ mode, blocking }` table.
function Ctx:mode()
  local m = btv.mode()
  return type(m) == "table" and m.mode or m
end

function Ctx:mode_info()
  return btv.mode()
end

function Ctx:buf()
  return btv._cur_buf and btv._cur_buf.bufnr
end

function Ctx:keymaps(mode)
  return btv.keymap.get(mode or "n")
end

-- ----- UI / redraw inspection (Phase 2: backed by the btv._ui mirror) --------
-- These read the projected UI snapshot the server mirrors each redraw. They return
-- nil / empty until that mirror is wired, so a state-only suite never breaks.

-- t:float() — the content float (which-key popup, hover, …), or nil when closed.
-- { text = joined lines, lines = { raw chunk rows }, title = string|nil }.
function Ctx:float()
  local ui = btv._ui
  local f = ui and ui.float
  if not f or not f.lines then
    return nil
  end
  local texts = {}
  for _, row in ipairs(f.lines) do
    -- A row is either a plain string or a list of { text, hl } chunks.
    if type(row) == "string" then
      texts[#texts + 1] = row
    else
      local parts = {}
      for _, chunk in ipairs(row) do
        parts[#parts + 1] = chunk[1] or ""
      end
      texts[#texts + 1] = table.concat(parts)
    end
  end
  return { text = table.concat(texts, "\n"), lines = f.lines, title = f.title }
end

-- t:message() — the latest echo / message line (the bottom-of-screen text).
function Ctx:message()
  local ui = btv._ui
  return ui and ui.message or ""
end

-- t:statusline() — the rendered status line text, when the mirror carries it.
function Ctx:statusline()
  local ui = btv._ui
  return ui and ui.statusline or ""
end

-- ----- the runner -----------------------------------------------------------

-- Run one hook/test fn, which may await. Returns ok, error-value. Runs inside the
-- caller's coroutine (so awaits suspend it); PUC 5.4 yields across pcall.
local function run_protected(fn, ctx)
  return pcall(fn, ctx)
end

-- Give each test a clean slate, the Lua analogue of the Rust harness's fresh
-- server-per-test: drop to normal mode and open a new empty unnamed buffer, so one
-- test's buffer edits / cursor / mode never bleed into the next. (Global keymaps and
-- user-commands persist — a plugin's `before_each` setup() re-applies them anyway,
-- and tests that assert on them want them present.)
local function fresh_slate()
  -- <Esc> through the keymap matcher (remap = true), NOT straight to the editor: a
  -- raw <Esc> bypasses the matcher and leaves any pending mapping-prefix from the
  -- previous test intact (which would, e.g., keep a which-key popup stuck open).
  -- Going through the matcher aborts the pending prefix and clears it.
  btv._feedkeys("<Esc>", true, false)
  settle(1)
  pcall(function()
    vim.cmd("silent! enew!")
  end)
  settle(1)
end

-- _run() — execute every registered test in declaration order, each with its hook
-- chain, capturing { path, name, status, message, ms }. Asynchronous: it returns at
-- once and sets `btv.test._results` / `btv.test._done` when finished. The runner polls
-- those. One failing test never aborts the rest.
function M._run()
  M._results = nil
  M._done = false
  local cases = flatten()
  btv
    .async(function()
      local results = {}
      for _, case in ipairs(cases) do
        local ctx = setmetatable({}, Ctx)
        local started = btv.now_ms()
        local status, message = "pass", nil

        fresh_slate()

        -- before_each (outer→inner). A hook failure errors the test without running it.
        for _, hook in ipairs(case.befores) do
          local ok, err = run_protected(hook, ctx)
          if not ok then
            status = "error"
            message = "before_each: " .. (type(err) == "table" and err.message or tostring(err))
            break
          end
        end

        if status == "pass" then
          local ok, err = run_protected(case.fn, ctx)
          if not ok then
            if type(err) == "table" and err.__btvtest_fail then
              status, message = "fail", err.message
            else
              status, message = "error", (type(err) == "table" and err.message or tostring(err))
            end
          end
        end

        -- after_each always runs (inner→outer), even after a failure. A cleanup
        -- failure downgrades a pass to error but never overwrites a real failure.
        for _, hook in ipairs(case.afters) do
          local ok, err = run_protected(hook, ctx)
          if not ok and status == "pass" then
            status = "error"
            message = "after_each: " .. (type(err) == "table" and err.message or tostring(err))
          end
        end

        results[#results + 1] = {
          path = case.path,
          name = case.name,
          status = status,
          message = message,
          ms = math.floor((btv.now_ms() - started) + 0.5),
        }
      end
      M._results = results
      M._done = true
      return results
    end)()
    :catch(function(err)
      -- An error in the runner itself (not a test): record it so the runner exits
      -- loud rather than hanging on a result that never arrives.
      M._results = {
        {
          path = {},
          name = "<test runner>",
          status = "error",
          message = "btv.test runner crashed: "
            .. (type(err) == "table" and err.message or tostring(err)),
          ms = 0,
        },
      }
      M._done = true
    end)
end

return M
