-- ~~~ nxvim LSP work-done progress — "is the server ready yet?" ~~~
--
-- Run it (from the repo root) — needs `gopls` on your PATH:
--
--     NXVIM_CONFIG=examples/lsp-progress \
--       cargo run -p nxvim -- examples/lsp-progress/sample.go
--
-- ---------------------------------------------------------------------------
-- What this example is about
--
-- A language server does not become useful the moment it starts. It starts, then
-- it reads your project — indexing, loading packages, building a crate graph —
-- and only then does hover/goto/completion mean anything. During that window a
-- server is attached and healthy and answering nothing useful, which is exactly
-- the state that makes an editor look broken.
--
-- LSP has a channel for saying so: `$/progress`, carrying a `WorkDoneProgress`
-- payload. A task is a `begin` -> `report`* -> `end` sequence sharing a token, and
-- a server may run several tokens at once. nxvim surfaces the whole thing:
--
--   * `nx.lsp.progress(filter)` — what every server is busy with RIGHT NOW. A
--     finished task is gone from the list rather than parked at 100%, so a
--     non-empty list means "still working".
--   * the `LspProgress` autocmd — fired on every update, with the update's KIND
--     as its PATTERN (`begin` / `report` / `end`), so `pattern = "end"` narrows to
--     completions. `args.data` carries the whole payload.
--
-- The two rules worth internalizing, both visible in the transcript below:
--
--   1. `title` arrives ONLY on `begin`. A `report` never repeats it.
--   2. An absent `message`/`percentage` on a `report` means "unchanged", NOT
--      "cleared". nxvim folds that for you — `nx.lsp.progress()` always hands you
--      the settled state — but if you read `args.data` off the event yourself, a
--      nil field is "the server said nothing this time".
--
-- Type this / see that:
--
--   1. Start it and watch the statusline's right-hand side. gopls announces
--      "Setting up workspace" and then "Loading packages" as it reads the module.
--      -> a braille spinner turning beside the task's title, and a percentage when
--      gopls sends one. It DISAPPEARS when the work finishes — the segment renders
--      nothing at all when `nx.lsp.progress()` is empty.
--   2. That can be over in under a second on a module this small, so the whole
--      thing is also recorded. `<leader>lP` dumps the transcript.  ->  every update
--      in order, e.g.
--
--          begin  Setting up workspace
--          report Setting up workspace                       (message only)
--          end    Setting up workspace
--
--      Read down the `title` column: only the `begin` line has one. That is rule 1
--      on your own machine, not in a spec.
--   3. `<leader>lp` prints what is in flight at this instant.  ->  after startup,
--      `no LSP work in flight` — because an ENDED task is removed. Press it while
--      step 1 is still running (or right after a `:LspRestart`) and you get the
--      live rows instead.
--   4. `:LspRestart gopls` re-runs the whole sequence without leaving the editor,
--      which is the easy way to watch steps 1-3 again.
--   5. `<leader>ls` splits the window.  ->  the spinner shows in BOTH statuslines
--      while gopls indexes. Progress is a fact about a SERVER, not a buffer, and
--      the segment below filters by `bufnr = 0` so each window reports the servers
--      attached to the buffer IT shows. Open a non-Go buffer (`:enew`) in one
--      split and its bar stays empty while the Go one spins.
--
-- ---------------------------------------------------------------------------

vim.g.mapleader = " "

-- 1. A server that actually reports progress. gopls sends `$/progress` for its
--    workspace setup and package loading on every start.
nx.lsp.config("gopls", {
  cmd = { "gopls" },
  filetypes = { "go" },
  root_markers = { "go.mod", ".git" },
})
nx.lsp.enable({ "gopls" })

-- ---------------------------------------------------------------------------
-- 2. The transcript. `LspProgress` fires on every update; this records the raw
--    `args.data` so step 2 can show what the SERVER sent, un-folded — including
--    the fields it left out.
--
--    Note `args.match`: it is the update's kind. nxvim fires the event with the
--    kind as the autocmd pattern (neovim's contract), which is what makes the
--    `pattern = "end"` handler further down possible at all.
local transcript = {}

nx.autocmd.create("LspProgress", {
  callback = function(args)
    local d = args.data
    transcript[#transcript + 1] = string.format(
      "%-6s %-24s %s%s",
      d.kind,
      d.title or "",
      d.message or "",
      d.percentage and (" " .. d.percentage .. "%") or ""
    )
  end,
})

-- A handler narrowed to ONE kind by pattern. Only completions reach it, so it can
-- say "ready" without inspecting anything.
nx.autocmd.create("LspProgress", {
  pattern = "end",
  callback = function(args)
    local client = nx.lsp.client_by_id(args.data.client_id)
    nx.notify(("%s finished: %s"):format(client and client.name or "?", args.data.title or ""))
  end,
})

-- ---------------------------------------------------------------------------
-- 3. The statusline segment — the same shape the bundled nxvim-line `lsp`
--    component uses, written out here so nothing is hidden behind a plugin.
--
--    Two things make it cheap. The DATA needs no polling: `LspProgress`
--    invalidates the segment, so `render` runs on updates and not per frame. The
--    only thing needing a clock is the SPINNER's animation between a server's
--    reports — and that timer is armed on the first update and stops itself the
--    moment nothing is in flight, so an idle editor has no wakeup at all.
local FRAMES = { "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏" }
local frame = 1
local ticking = false

nx.statusline.segment({
  name = "lspprogress",
  events = { "LspProgress", "LspAttach", "LspDetach", "BufEnter" },
  render = function(ctx)
    -- `bufnr = 0`... but `render` is per WINDOW, so use the rendered window's
    -- buffer: a server busy on another project is not this buffer's status.
    local tasks = nx.lsp.progress({ bufnr = ctx.buf })
    if #tasks == 0 then
      return nil -- nothing in flight: the segment collapses entirely
    end
    local t = tasks[1]
    local parts = { FRAMES[(frame - 1) % #FRAMES + 1] }
    if t.title ~= "" then
      parts[#parts + 1] = t.title
    end
    if t.message then
      -- The server chose this string and it can be a full path; bound it, or a
      -- long one pushes every other section off the bar for the whole index.
      parts[#parts + 1] = #t.message > 24 and (t.message:sub(1, 23) .. "…") or t.message
    end
    if t.percentage then
      parts[#parts + 1] = t.percentage .. "%"
    end
    local text = table.concat(parts, " ")
    if #tasks > 1 then
      text = text .. " (+" .. (#tasks - 1) .. ")" -- several tokens at once
    end
    return { { text = " " .. text .. " ", hl = "DiagnosticInfo" } }
  end,
})

local function tick()
  if #nx.lsp.progress() == 0 then
    ticking = false
    return -- the work is done; stop the clock rather than spin forever
  end
  frame = frame + 1
  nx.statusline.invalidate("lspprogress")
  nx.timer(tick, 100)
end

nx.autocmd.create("LspProgress", {
  callback = function()
    if not ticking then
      ticking = true
      nx.timer(tick, 100)
    end
  end,
})

nx.statusline.setup({
  left = { "mode", "filename" },
  right = { "lspprogress", "diagnostics", "location" },
})

-- ---------------------------------------------------------------------------
-- 4. The two readouts the steps use.

-- <leader>lp — what is in flight at this instant.
nx.keymap.set("n", "<leader>lp", function()
  local tasks = nx.lsp.progress()
  if #tasks == 0 then
    nx.notify("no LSP work in flight")
    return
  end
  local out = {}
  for _, p in ipairs(tasks) do
    out[#out + 1] = string.format(
      "%s  %s  %s%s  (token %s)",
      p.client_name,
      p.title,
      p.message or "",
      p.percentage and (" " .. p.percentage .. "%") or "",
      p.token
    )
  end
  nx.notify(table.concat(out, "\n"))
end, { desc = "LSP: work in flight now" })

-- <leader>lP — the recorded transcript (every update since startup).
nx.keymap.set("n", "<leader>lP", function()
  if #transcript == 0 then
    nx.notify("no LspProgress events recorded yet")
    return
  end
  nx.notify(table.concat(transcript, "\n"))
end, { desc = "LSP: progress transcript" })

-- <leader>ls — a split, for step 5.
nx.keymap.set("n", "<leader>ls", "<C-w>v", { desc = "split" })
