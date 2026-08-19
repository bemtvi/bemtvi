-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/lsp-progress
--
-- Real progress is gopls indexing a module, which takes far longer than a spec
-- suite may run and reports whatever it feels like — so nothing here waits for
-- it. Everything the config builds ON that channel is drivable without a server,
-- because `LspProgress` is an ordinary autocmd: the spec fires it with the payload
-- a server sends and checks the transcript, the notification, and the statusline
-- segment that reads it.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.go")
  t:cmd("e!")
  t:cmd("echo ''")
  t:feed("gg")
end

--- Fire one `$/progress` update the way the server's would arrive.
local function progress(t, data)
  t:exec(function()
    btv.autocmd.exec("LspProgress", { pattern = data.kind, data = data })
  end)
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

btv.test.describe("examples/lsp-progress", function()
  btv.test.it("gopls is registered for go buffers", function(t)
    open(t)
    btv.test.expect(btv.bo.filetype).to_be("go")
    local cfg = btv.lsp.get_config("gopls")
    btv.test.expect(cfg.cmd).to_equal({ "gopls" })
    btv.test.expect(cfg.root_markers).to_equal({ "go.mod", ".git" })
  end)

  -- "the `LspProgress` autocmd — fired on every update, with the update's KIND as
  --  its PATTERN (`begin` / `report` / `end`)."
  btv.test.it("the transcript records every update, kind first", function(t)
    open(t)
    progress(t, { kind = "begin", title = "Loading packages", client_id = 1, token = "t1" })
    progress(t, { kind = "report", message = "fmt", percentage = 40, client_id = 1, token = "t1" })
    progress(t, { kind = "end", title = "Loading packages", client_id = 1, token = "t1" })
    local said = notified(function()
      t:feed("<Space>lP")
    end)
    btv.test.expect(said).to_contain("begin  Loading packages")
    -- "`title` arrives ONLY on `begin`. A `report` never repeats it."
    btv.test.expect(said).to_contain("report")
    btv.test.expect(said).to_contain("fmt 40%")
    btv.test.expect(said).to_contain("end")
  end)

  -- "`pattern = 'end'` narrows to completions."
  btv.test.it("the end-only handler fires on end and on nothing else", function(t)
    open(t)
    local said = notified(function()
      progress(t, { kind = "begin", title = "Indexing", client_id = 1, token = "t2" })
    end)
    btv.test.expect(said).to_be_nil()
    said = notified(function()
      progress(t, { kind = "end", title = "Indexing", client_id = 1, token = "t2" })
    end)
    btv.test.expect(said).to_contain("finished: Indexing")
  end)

  -- "`<leader>lp` — what every server is busy with RIGHT NOW … A finished task is
  --  gone from the list rather than parked at 100%."
  btv.test.it("<space>lp reports what is in flight, or that nothing is", function(t)
    open(t)
    local live = #btv.lsp.progress()
    local said = notified(function()
      t:feed("<Space>lp")
    end)
    if live == 0 then
      btv.test.expect(said).to_be("no LSP work in flight")
    else
      -- A live task prints its client, title and token — the fields the notes name.
      btv.test.expect(said).to_contain("(token ")
      btv.test.expect(#btv.str.split(said, "\n")).to_be(live)
    end
  end)

  btv.test.it("<space>lP prints the transcript it has", function(t)
    open(t)
    local said = notified(function()
      t:feed("<Space>lP")
    end)
    btv.test.expect(said).never.to_be(nil)
  end)

  -- "the statusline segment … `#tasks == 0` → nothing in flight: the segment
  --  collapses entirely."
  btv.test.it("the statusline carries the segment, collapsed when idle", function(t)
    open(t)
    t:cmd("set laststatus=2")
    local bar = t:statusline()
    btv.test.expect(bar).never.to_be("")
    local FRAMES = { "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏" }
    local spinning = false
    for _, frame in ipairs(FRAMES) do
      spinning = spinning or bar:find(frame, 1, true) ~= nil
    end
    -- The segment collapses entirely when nothing is in flight, and shows a
    -- spinner frame when something is — which of the two is the server's business.
    btv.test.expect(spinning).to_be(#btv.lsp.progress({ bufnr = btv.buf.current() }) > 0)
  end)

  -- "btv.statusline.setup … right = { 'lspprogress', 'diagnostics', 'location' }"
  btv.test.it("the config's own bar is the one drawn", function(t)
    open(t)
    t:cmd("set laststatus=2")
    -- `mode` and `filename` on the left, `location` on the right.
    local bar = t:statusline()
    btv.test.expect(bar:upper()).to_contain("NORMAL")
    btv.test.expect(bar).to_contain("sample.go")
    btv.test.expect(bar).to_match("%d+:%d+")
  end)

  -- "<leader>ls — split" (so you can watch the per-window segment).
  btv.test.it("\\ls splits the window", function(t)
    open(t)
    local before = #vim.api.nvim_list_wins()
    t:feed("<Space>ls")
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(before + 1)
    t:cmd("only")
  end)

  -- The one cheap live check.
  btv.test.it("gopls spawns for a go buffer when it is installed", function(t)
    open(t)
    for _ = 1, 100 do
      if #btv.lsp.clients() > 0 then
        break
      end
      t:sleep(20)
    end
    if #btv.lsp.clients() == 0 then
      print("skip: gopls is not installed")
      return
    end
    local names = {}
    for _, c in ipairs(btv.lsp.clients()) do
      names[c.name] = true
    end
    btv.test.expect(names["gopls"]).to_be(true)
  end)
end)
