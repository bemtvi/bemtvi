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
  -- The fourth argument marks these keys as TYPED — this is the user's keyboard,
  -- which is the whole premise of the framework. Plugin typeahead leaves it off,
  -- so an in-flight `<F2>` recording captures a spec's keys and not a plugin's.
  btv._test_clear_scroll()
  btv._feedkeys(keys, opts.remap ~= false, opts.insert or false, true)
  settle(opts.settle or 1)
  return self
end

-- t:cmd(excmd) — run an ex-command, then settle a tick.
function Ctx:cmd(excmd)
  btv._test_clear_scroll()
  vim.cmd(excmd)
  settle(1)
  return self
end

-- t:exec(fn) — run `fn` now (mirrors already fresh) and return its value. `fn` may
-- itself await (it runs in the test coroutine).
function Ctx:exec(fn)
  return fn()
end

-- `t:mouse(button, action, row, col[, modifier])` — send one mouse gesture at a
-- global SCREEN cell, then settle a tick. The vocabulary is the wire's: `button` is
-- `"left"` / `"right"` / `"middle"` / `"wheel"` / `"move"`, `action` is `"press"` /
-- `"drag"` / `"release"` for a button and `"up"` / `"down"` / `"left"` / `"right"`
-- for the wheel, and `modifier` is `""`, `"s"`, `"c"` or `"a"`. Rows and columns are 0-based, as the wire carries them.
--
-- The server owns every hit-test — cell to window to buffer position — so this is
-- the same call a client makes, and a spec drives a click, a drag-select, a
-- multi-click, a wheel scroll or a divider drag exactly as a user would. Multi-click
-- detection compares arrival times against `'mousetime'`, so a double-click is two
-- press/release pairs in quick succession, not a special action.
--
-- ```lua
-- t:mouse("left", "press", 4, 12)
-- t:mouse("left", "release", 4, 12)
-- btv.test.expect(t:cursor()[1]).to_be(5)
-- ```
function Ctx:mouse(button, action, row, col, modifier)
  btv._test_mouse(
    tostring(button),
    tostring(action),
    tostring(modifier or ""),
    tonumber(row) or 0,
    tonumber(col) or 0
  )
  settle(1)
  return self
end

-- t:wait_for(predicate[, opts]) — await until `predicate` returns truthy (polling
-- between ticks), returning that value. Rejects (raising in the test) on timeout.
-- Use for async UI: a debounced popup, a watch-driven refresh. opts = { tries=,
-- interval=, message= } (see btv.wait_for).
function Ctx:wait_for(predicate, opts)
  return btv.await(btv.wait_for(predicate, opts))
end

-- `t:idle()` — the pause. A key that is a live prefix of a mapping is WITHHELD
-- until the next keystroke; the client arms a `'timeoutlen'` timer and, on idle,
-- nudges the server to resolve it. There is no such timer server-side, so a spec
-- that just waits waits forever — `t:idle()` is how it says "the user stopped
-- typing here", and it is the only way a genuinely-ambiguous mapped prefix
-- resolves (`:set notimeout` makes it a no-op, exactly as it does for a client).
--
-- ```lua
-- t:feed("gg")        -- a live prefix of the `ggx` mapping: held
-- t:idle()            -- …and now it resolves to the `gg` built-in
-- ```
function Ctx:idle()
  btv._test_idle()
  settle(1)
  return self
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

-- `t:showcmd()` — the `'showcmd'` corner: the partly-typed command as the client
-- would paint it in the last line's right corner, truncated to vim's 10 columns.
--
-- It is the one view that can see a **withheld** key run. A mapped prefix
-- (`<Space>f` of a `<Space>fs` map) is held by the keymap matcher and never
-- reaches the editor, so no buffer, cursor or mode state moves while it waits —
-- the corner is the only place it exists. The editor's own pending run (a count,
-- an armed register, an operator waiting for its motion, a Visual selection's
-- size) shows here too.
--
-- ```lua
-- t:feed("2d")
-- btv.test.expect(t:showcmd()).to_be("2d")
-- ```
--
-- Empty when nothing is pending — and always empty with `'showcmd'` off, which is
-- what the option means.
function Ctx:showcmd()
  local ui = btv._ui
  return (ui and ui.showcmd) or ""
end

-- `t:scroll()` — the scroll-animation gesture the last frame started, as
-- `{ from_row, to_row, duration_ms }` (buffer lines, and the milliseconds the
-- client will take to slide between them), or nil when that frame started none.
--
-- A **one-shot**: only the frame that begins a slide carries it. It describes what
-- the client is about to animate rather than any state the editor keeps, so no
-- other view can see it — `t:view()` reports the destination the scroll already
-- settled on, animated or not. Always nil under `'noscrollanim'` (and at
-- `'scrollanimduration'` 0), which is exactly what those options mean.
--
-- ```lua
-- t:feed("<C-d>")
-- btv.test.expect(t:scroll().to_row > t:scroll().from_row).to_be(true)
-- ```
function Ctx:scroll()
  local ui = btv._ui
  local s = ui and ui.scroll
  if type(s) ~= "table" then
    return nil
  end
  return s
end

-- `t:matches([row])` — the SEARCH-match highlight over the focused window's rows:
-- a list of `{ first, last, current }` per row (1-based screen rows, matching
-- `t:screen()`; columns are 0-based display cells, end-exclusive). `current` marks
-- the one match being walked onto while a `/` or `:s` command line is open (vim's
-- `IncSearch`, as against the plain `Search` on the rest). With `row`, just that
-- row's matches.
--
-- The match highlight rides its own wire layer — the server sends column spans and
-- the client paints the group itself — so no highlight span is ever emitted for it
-- and `t:highlights()` cannot see a match at all. This is the only view of
-- `'hlsearch'`, of `'incsearch'`, and of the plain pattern preview a half-typed
-- `:s/pat` shows before its replacement opens.
--
-- ```lua
-- t:feed("/needle<CR>")
-- btv.test.expect(#t:matches(3)).to_be(1)
-- ```
function Ctx:matches(row)
  local ui = btv._ui or {}
  local rows, inc = ui.search or {}, ui.incsearch or {}
  local function one(i)
    local out = {}
    for _, span in ipairs(rows[i] or {}) do
      local live = inc[i]
      out[#out + 1] = {
        span[1],
        span[2],
        live ~= nil and live[1] == span[1] and live[2] == span[2],
      }
    end
    return out
  end
  if row ~= nil then
    return one(row)
  end
  local all = {}
  for i = 1, #rows do
    all[i] = one(i)
  end
  return all
end

-- `t:cmdline()` — the open command line as drawn: the `:` / `/` prefix, or an
-- `btv.ui.input` / `btv.ui.confirm` prompt LABEL, followed by the editable text.
-- `nil` when no command line is open.
--
-- The three are separate on the wire (the client draws the prefix and label itself),
-- so the label — what the prompt actually ASKED — is in none of the other views.
--
-- ```lua
-- t:feed(":ene")
-- btv.test.expect(t:cmdline()).to_be(":ene")
-- ```
function Ctx:cmdline()
  local ui = btv._ui or {}
  local prefix, prompt, text = ui.cmdline_prefix or "", ui.cmdline_prompt or "", ui.cmdline or ""
  local line = prefix .. prompt .. text
  if line == "" then
    return nil
  end
  return line
end

-- t:statusline() — the rendered status line text, when the mirror carries it.
function Ctx:statusline()
  local ui = btv._ui
  return ui and ui.statusline or ""
end

-- `t:tabline()` — a custom `'tabline'` as rendered: the one styled row the
-- `%`-format engine produced, with the `%#Group#` / `%nT` items already resolved
-- away. `nil` when `'tabline'` is unset, or when no tabline is drawn this frame.
--
-- A wholly different payload from `t:tabs()`, which reads the STRUCTURED per-region
-- tab cells the client formats itself. Only one of the two is ever drawn — setting
-- `'tabline'` replaces the cells — so a suite for a `%!`-built tabline can see
-- nothing in `t:tabs()` but the built-in labels it was meant to replace.
function Ctx:tabline()
  local ui = btv._ui
  local line = ui and ui.tabline
  if line == nil or line == "" then
    return nil
  end
  return line
end

-- `t:screen()` — the focused window's **painted** rows, top to bottom, as a list
-- of strings.
--
-- The sibling of `t:lines()`, and the difference matters: `t:lines()` is buffer
-- text, while this is what the client would actually draw. Anything the editor
-- renders *instead of* a buffer line is only visible here — a closed fold's
-- placeholder, a `~` filler past the end. So a test for `'foldtext'` or
-- `'listchars'` asserts on this; a test for an edit asserts on `t:lines()`.
-- Virtual text and virtual *lines* are not here — they ride their own layers
-- (a virtual line takes a row, but the row reads blank and its text is in the
-- layer), which is what `t:decor()` reads.
--
-- ```lua
-- t:feed("zM")
-- btv.test.expect(t:screen()[4]).to_contain("5 lines: fn alpha")
-- ```
--
-- The text is display-scrubbed the same way the wire's rows are (an unprintable
-- byte reads as `^X` / `<xx>`), so it is character-for-character what is painted.
-- Only the focused window; splits and docks are not included.
function Ctx:screen()
  local ui = btv._ui
  return (ui and ui.screen) or {}
end

-- `t:highlights([row])` — the highlight spans painted over the focused window's
-- rows: a list of `{ first, last, group }` per row (1-based *screen* rows, matching
-- `t:screen()`; columns are 0-based, end-exclusive display cells). With `row`, just
-- that row's spans.
--
-- One of the two views that can see a **decoration** (`t:decor()` is the other):
-- `t:lines()` is buffer text, `t:screen()` is the glyphs drawn, and a highlight —
-- a `btv.decor` provider's mark, a `btv.decor.expr` paint, a treesitter capture —
-- changes neither. Groups are the names as painted, so a test asserts on the group
-- it asked for rather than on a colour.
--
-- ```lua
-- btv.decor.expr([[ local s, e = line:find("TODO") if s then return { { s, e, "Todo" } } end return {} ]])
-- t:feed("iTODO<Esc>")
-- btv.test.expect(t:highlights(1)[1][3]).to_be("Todo")
-- ```
--
-- Only the focused window, like `t:screen()`.
function Ctx:highlights(row)
  local ui = btv._ui
  local rows = (ui and ui.highlights) or {}
  if row == nil then
    return rows
  end
  return rows[row] or {}
end

-- `t:tabs([region])` — a region's own tab pages:
-- `{ labels = { "…" }, current = <1-based index> }`, or a map of every region when
-- `region` is omitted. `region` is `"main"` or a dock side (`"left"` / `"right"` /
-- `"top"` / `"bottom"`).
--
-- Tab pages here are PER REGION — the main area and each open dock carry their own
-- independent set — but `nvim_list_tabpages` reports one global list whichever
-- region is focused, so from Lua the stacks are indistinguishable and a spec cannot
-- tell "a tab was added to the dock" from "a tab was added".
--
-- A region reports the tabline it DRAWS, so a region whose `'showtabline'` hides it
-- (the default `1`, with a single tab) reports no labels.
--
-- ```lua
-- btv.test.expect(#t:tabs("left").labels).to_be(2)
-- btv.test.expect(#t:tabs("main").labels).to_be(1)
-- ```
function Ctx:tabs(region)
  local all = (btv._ui and btv._ui.region_tabs) or {}
  local function one(rt)
    rt = rt or {}
    return { labels = rt.tabs or {}, current = (rt.current or 0) + 1 }
  end
  if region == nil then
    local out = {}
    for name, rt in pairs(all) do
      out[name] = one(rt)
    end
    return out
  end
  return one(all[region])
end

-- `t:view()` — where the focused window is SCROLLED to:
-- `{ leftcol = <columns>, topline = <buffer line>, numbers = { <line per row> } }`.
--
-- `t:screen()` carries each painted row's full text; the CLIENT is what clips it
-- to the window and slides it left by `leftcol`. So sideways scrolling (`nowrap`)
-- is visible in no row view at all, and the vertical position had to be guessed
-- from the text. `numbers` is the buffer line each painted row shows — the top
-- line, and which lines are visible at all, since a closed fold takes its rows out
-- of the list.
--
-- ```lua
-- t:feed("$")
-- btv.test.expect(t:view().leftcol > 0).to_be(true)
-- ```
function Ctx:view()
  local ui = btv._ui or {}
  local wire = ui.numbers or {}
  -- One entry per painted row, `false` where the row is not a buffer line at all (a
  -- row an extmark reserved for a virtual LINE). The wire leaves those `nil`, which
  -- would end an `ipairs` walk at the first virtual row and hide every row below it.
  local numbers, rows = {}, #(ui.screen or {})
  for i = 1, math.max(rows, #wire) do
    numbers[i] = wire[i] or false
  end
  return { leftcol = ui.leftcol or 0, topline = numbers[1] or nil, numbers = numbers }
end

-- `t:gutter()` — the focused window's reserved gutter, as
-- `{ number_width = <cells>, sign_width = <cells>, total = <cells> }`.
--
-- The CLIENT draws the number and sign columns from the widths the server
-- reserves, so the gutter is in none of the row views — `t:screen()` is the text
-- area alone. Without this a spec could read `'numberwidth'` / `'signcolumn'` back
-- and nothing more, which reports what was ASKED for rather than what was
-- reserved: `'numberwidth'` is a minimum that grows to fit the largest line
-- number, `'signcolumn=auto'` collapses when no sign is placed, and `'nonumber'`
-- removes the column whatever its width says.
--
-- ```lua
-- t:cmd("set number numberwidth=4 signcolumn=yes:2")
-- btv.test.expect(t:gutter().total).to_be(8)
-- ```
function Ctx:gutter()
  local ui = btv._ui or {}
  -- The reserved number width is the width the column takes WHEN drawn; with
  -- `'nonumber'` and `'norelativenumber'` it is not drawn at all.
  local number_width = ui.number_shown and (ui.number_width or 0) or 0
  local sign_width = ui.sign_width or 0
  return {
    number_width = number_width,
    sign_width = sign_width,
    total = number_width + sign_width,
  }
end

-- `t:menu()` — the open float-list menu, or nil when none is up:
-- `{ items = { "…" }, selected = <1-based index|nil> }`.
--
-- One widget serves four features — the insert-mode completion popup, the `:`-line
-- wildmenu, `btv.picker`, and `btv.ui.select` — and it is in none of the other
-- views: not buffer text, not a painted row of the focused window (it floats over
-- them), and not the content float `t:float()` reads. Without this a suite for any
-- of those could only assert on what happened AFTER an accept, never on what was
-- offered or which row led.
--
-- `selected` is nil while nothing is highlighted — the popup opens `noselect`, so
-- `<CR>` runs the typed line until you actually pick a row. `row`/`col`/`width`/
-- `height` describe the box, but in the placement's own coordinate space (see
-- below) — to CLICK a row, probe for the cell rather than deriving it.
--
-- `kinds` is the parallel list of per-row kind labels the completion popup
-- right-aligns (`"Snippet"`, `"Function"`, …), `""` for a row that carries none (a
-- plain buffer word). It is the whole of what the kind column shows, and it is
-- empty for every other user of the widget.
--
-- ```lua
-- t:feed(":ene<Tab>")
-- btv.test.expect(t:menu().items[1]).to_be("enew")
-- btv.test.expect(t:menu().selected).to_be_nil()
-- ```
function Ctx:menu()
  local m = btv._ui and btv._ui.menu
  if not m then
    return nil
  end
  -- One entry per row, `""` where the row has no kind: the wire omits the key
  -- entirely when no row has one, and carries nil in the slots that don't.
  local kinds = {}
  for i = 1, #(m.items or {}) do
    kinds[i] = (m.kinds or {})[i] or ""
  end
  return {
    items = m.items or {},
    kinds = kinds,
    selected = m.selected_active and ((m.selected or 0) + 1) or nil,
    -- Where the box was placed, in the box's OWN space: a cursor-anchored menu (the
    -- completion popup, `btv.ui.select`) reports window-relative cells, while an
    -- editor-level one (the picker) reports windows-area cells. Neither is
    -- necessarily the global screen cell `t:mouse` names, so treat these as the
    -- box's size and rough position, not as mouse coordinates.
    row = m.row,
    col = m.col,
    width = m.width,
    height = m.height,
  }
end

-- `t:rulers()` — the focused window's `'colorcolumn'` rulers, as the 1-based text
-- columns the client is told to paint with the `ColorColumn` group.
--
-- The third view onto a layer that is not buffer text: `t:lines()` cannot see a
-- ruler (it is not text), `t:screen()` cannot (it is a background behind cells
-- that may hold nothing), and `t:highlights()` cannot either — the server sends
-- the column list and the client draws it, so no span is ever emitted. This is
-- also the only place the `+N` rule is visible: a `'textwidth'`-relative entry is
-- accepted but resolves to nothing, while `'colorcolumn'` itself still reads
-- `"+1"`.
--
-- ```lua
-- t:cmd("set colorcolumn=80,120")
-- btv.test.expect(t:rulers()).to_equal({ 80, 120 })
-- ```
--
-- Only the focused window, like `t:screen()`.
function Ctx:rulers()
  local ui = btv._ui
  return (ui and ui.colorcolumn) or {}
end

-- Where a `virt_text` placement draws, by the code the wire carries.
local VIRT_POS =
  { [0] = "eol", [1] = "inline", [2] = "overlay", [3] = "right_align", [4] = "win_col" }

-- `t:decor([row])` — the decoration drawn *beside* the focused window's rows:
-- virtual text, virtual lines, gutter signs, the full-width row tint, and the
-- end-of-line diagnostic message. A list of
-- `{ virt_text = "…", virt_pos = "eol", virt_col = 0, virt_lines = "…", sign = "▶",
-- line_bg = true, diagnostic = "…", severity = 1 }` per row (1-based *screen* rows,
-- matching `t:screen()`); with `row`, just that row's. Each key is absent on a row
-- that carries nothing of that kind.
--
-- The companion of `t:highlights()`, and the split is what each layer *is*: a
-- highlight colours the buffer's own cells, while these draw glyphs that are not in
-- the buffer at all — a `btv.decor.expr` badge or sign, a provider's inline blame, a
-- diagnostic's message. `t:screen()` cannot see them either: it is the buffer's rows
-- as painted, and each of these rides its own layer beside them. (Which is also why
-- `diagnostic` is its own key rather than part of `virt_text`: they are separate
-- layers on the wire, and one row can carry both.)
--
-- ```lua
-- btv.decor.expr([[ return { { 1, 1, "Todo", virt_text = " <- here", sign_text = ">>" } } ]])
-- btv.test.expect(t:decor(1).virt_text).to_be(" <- here")
-- btv.test.expect(t:decor(1).sign).to_be(">>")
-- ```
--
-- `virt_text` is the row's placements joined, since one row can carry several;
-- `virt_pos`/`virt_col` describe the first. `virt_lines` is set on a row that *is* a
-- virtual line — a whole extra screen row, so it reads blank in `t:screen()` and
-- carries its text here instead. `line_bg` is `true` on a row a `line_hl_group`
-- tints full width. Highlight *groups* are absent on purpose: the wire carries a
-- per-frame palette id for these layers, not a name, so `t:highlights()` stays the
-- group-level view.
--
-- Only the focused window, like `t:screen()`.
function Ctx:decor(row)
  local ui = btv._ui
  local virt, signs = (ui and ui.virt_text) or {}, (ui and ui.signs) or {}
  local lines = (ui and ui.virt_lines) or {}
  -- The line-background layer arrives as `{ row, style }` pairs on 0-based screen
  -- rows; fold it into a set keyed the same 1-based way as everything else here.
  local tinted, last_bg = {}, 0
  for _, place in ipairs((ui and ui.line_bg) or {}) do
    local tinted_row = (place[1] or 0) + 1
    tinted[tinted_row] = true
    last_bg = math.max(last_bg, tinted_row)
  end
  -- The diagnostic layer is one entry PER ROW with holes for clean rows, so `#`
  -- on it is unreliable (it stops at the first gap); the painted-row count is the
  -- honest bound.
  local diags = (ui and ui.diagnostics_virt) or {}
  local rows, n = {}, math.max(#virt, #signs, #lines, last_bg, #((ui and ui.screen) or {}))
  for i = 1, n do
    local out = {}
    local places = virt[i]
    if type(places) == "table" and #places > 0 then
      local texts = {}
      for _, place in ipairs(places) do
        for _, chunk in ipairs(place[4] or {}) do
          texts[#texts + 1] = chunk[1]
        end
      end
      out.virt_text = table.concat(texts)
      out.virt_pos = VIRT_POS[places[1][1]]
      out.virt_col = places[1][2]
    end
    -- A virtual line is one row's whole content, carried as a chunk run like a
    -- `virt_text` placement (the row's own `screen` text is blank).
    local chunks = lines[i]
    if type(chunks) == "table" and #chunks > 0 then
      local texts = {}
      for _, chunk in ipairs(chunks) do
        texts[#texts + 1] = chunk[1]
      end
      out.virt_lines = table.concat(texts)
    end
    local sign = signs[i]
    if type(sign) == "table" then
      out.sign = sign[1]
    end
    out.line_bg = tinted[i] or nil
    -- The end-of-line diagnostic message, on its own layer beside the extmark
    -- virtual text (a row can carry both, from different sources).
    local diag = diags[i]
    if type(diag) == "table" then
      out.diagnostic = diag[1]
      out.severity = diag[2]
    end
    rows[i] = out
  end
  if row == nil then
    return rows
  end
  return rows[row] or {}
end

-- ----- the runner -----------------------------------------------------------

-- Run one hook/test fn, which may await. Returns ok, error-value. Runs inside the
-- caller's coroutine (so awaits suspend it); PUC 5.4 yields across pcall.
local function run_protected(fn, ctx)
  return pcall(fn, ctx)
end

-- ----- per-test isolation ---------------------------------------------------
--
-- A test must not be able to affect the next one. Resetting the *buffer* is not
-- enough: options, globals, registers, keymaps, user commands and the sandbox
-- expressions all live above the buffer and used to survive, so a suite's
-- outcome depended on the order its cases happened to run in.
--
-- The fix is a baseline rather than a wipe. `_run` takes a snapshot *after* the
-- spec files have been sourced, so whatever a file installs at load time (a
-- `require("plugin").setup{}`, a `dofile` of an example's `init.lua`) is part of
-- the baseline and stays; anything a *test* changes is put back before the next
-- one. That keeps the install-once model specs are written against while making
-- the cases independent of each other.
--
-- What is restored: global and window-local options (the catalog's names plus the
-- `btv._o_store` catch-all), `btv.g`, the named registers, every sandbox
-- expression surface (from `btv._sandbox_srcs`, which the setters write
-- themselves — see the note there on why this is not a list kept here), the
-- quickfix list, and any keymap, user command or `btv.decor` provider a test
-- *added*. What is not: a keymap, command or provider a test *deleted* (there is
-- no spec to rebuild it from), autocmds, the window layout, and buffers other
-- than the one `enew!` replaces.

local REGISTERS = 'abcdefghijklmnopqrstuvwxyz0123456789"-'

--- Every global-scope option name the core catalog knows.
local function global_option_names()
  local names = {}
  for _, row in ipairs(btv._options_catalog or {}) do
    if row.scope == "global" or row.global_tier then
      names[#names + 1] = row.name
    end
  end
  return names
end

--- The window-local option names. `enew!` gives a fresh *buffer*, so
--- buffer-locals come back from their global tier on their own — but the window
--- survives, and a window-local value shadows the global tier a `btv.o` write
--- sets. Those have to be put back explicitly.
local function window_option_names()
  local names = {}
  for _, row in ipairs(btv._options_catalog or {}) do
    if row.scope == "window" then
      names[#names + 1] = row.name
    end
  end
  return names
end

local function shallow_copy(t)
  local out = {}
  for k, v in pairs(t or {}) do
    out[k] = v
  end
  return out
end

--- Capture everything a test may mutate above the buffer.
local function snapshot()
  local opts = {}
  for _, name in ipairs(global_option_names()) do
    local ok, v = pcall(function()
      return btv.o[name]
    end)
    if ok then
      opts[name] = v
    end
  end
  local wopts = {}
  for _, name in ipairs(window_option_names()) do
    local ok, v = pcall(function()
      return btv.wo[name]
    end)
    if ok then
      wopts[name] = v
    end
  end
  local regs = {}
  for i = 1, #REGISTERS do
    local name = REGISTERS:sub(i, i)
    local ok, v = pcall(btv.reg.get, name)
    regs[name] = ok and v or nil
  end
  local maps = {}
  for _, mode in ipairs({ "n", "i", "v", "x", "s", "o", "c", "t" }) do
    local seen = {}
    for _, m in ipairs(btv.keymap.get(mode) or {}) do
      seen[m.lhs] = true
    end
    maps[mode] = seen
  end
  return {
    options = opts,
    window_options = wopts,
    o_store = shallow_copy(btv._o_store),
    g = shallow_copy(btv.g),
    regs = regs,
    maps = maps,
    commands = shallow_copy(btv._user_commands),
    -- Every sandbox surface, from the registry the setters write themselves —
    -- deliberately not a list maintained here, which is what let `complete.scorer`,
    -- `decor.expr` and the two `qf` surfaces leak between tests for four releases.
    exprs = shallow_copy(btv._sandbox_srcs),
    providers = shallow_copy((btv._decor or {}).providers),
    qflist = { items = btv.qf.getqflist(), title = (btv._qflist or {}).title },
  }
end

--- Put the world back the way `snapshot` found it.
local function restore(b)
  if not b then
    return
  end
  -- Options: write only what actually changed, so an option with a side effect
  -- (a fold rebuild, a redraw) is not poked on every single test.
  for name, want in pairs(b.options) do
    local ok, now = pcall(function()
      return btv.o[name]
    end)
    if ok and now ~= want then
      pcall(function()
        btv.o[name] = want
      end)
    end
  end
  for name, want in pairs(b.window_options or {}) do
    local ok, now = pcall(function()
      return btv.wo[name]
    end)
    if ok and now ~= want then
      pcall(function()
        btv.wo[name] = want
      end)
    end
  end
  for k in pairs(btv._o_store or {}) do
    if b.o_store[k] == nil then
      btv._o_store[k] = nil
    end
  end
  for k, v in pairs(b.o_store) do
    btv._o_store[k] = v
  end

  for k in pairs(btv.g or {}) do
    if b.g[k] == nil then
      btv.g[k] = nil
    end
  end
  for k, v in pairs(b.g) do
    btv.g[k] = v
  end

  for name, want in pairs(b.regs) do
    local ok, now = pcall(btv.reg.get, name)
    if ok and now ~= want then
      pcall(btv.reg.set, name, want or "")
    end
  end

  -- Only additions are undone: a mapping a test *deleted* cannot be rebuilt
  -- from a snapshot of its left-hand sides.
  for mode, seen in pairs(b.maps) do
    for _, m in ipairs(btv.keymap.get(mode) or {}) do
      if not seen[m.lhs] then
        pcall(btv.keymap.del, mode, m.lhs)
      end
    end
  end
  for name in pairs(btv._user_commands or {}) do
    if b.commands[name] == nil then
      btv._user_commands[name] = nil
    end
  end

  -- The sandbox surfaces: every key either side knows, so a surface a *test*
  -- installed is cleared and one the baseline holds is put back. Only what
  -- actually differs is re-set, since installing one recompiles it (and, for the
  -- quickfix render, re-renders every open list).
  local names = {}
  for name in pairs(b.exprs) do
    names[name] = true
  end
  for name in pairs(btv._sandbox_srcs or {}) do
    names[name] = true
  end
  for name in pairs(names) do
    if btv._sandbox_srcs[name] ~= b.exprs[name] then
      local set = btv._sandbox_setter(name)
      if set then
        pcall(set, b.exprs[name])
      end
    end
  end

  -- Decoration providers: like keymaps, only *additions* are undone. A provider is
  -- a closure, so one a test removed cannot be rebuilt from a snapshot.
  local providers = (btv._decor or {}).providers
  if providers then
    local kept = {}
    for _, p in ipairs(b.providers) do
      kept[p] = true
    end
    for i = #providers, 1, -1 do
      if not kept[providers[i]] then
        table.remove(providers, i)
      end
    end
  end

  -- The quickfix list, which is global state a `:make` / `:vimgrep` / `setqflist`
  -- in one test would otherwise hand to the next. Location lists are per window and
  -- go with the window; named lists are not restored.
  if #(btv.qf.getqflist() or {}) > 0 or #(b.qflist.items or {}) > 0 then
    pcall(btv.qf.setqflist, b.qflist.items or {}, "r", { title = b.qflist.title })
  end
end

-- Give each test a clean slate, the Lua analogue of the Rust harness's fresh
-- server-per-test: restore the baseline above, drop to normal mode, and open a
-- new empty unnamed buffer, so one test's state never bleeds into the next.
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
  restore(M._baseline)
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
  -- Taken here, after every spec file has been sourced: a file's load-time setup
  -- belongs to the baseline, a test's changes do not.
  M._baseline = snapshot()
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
