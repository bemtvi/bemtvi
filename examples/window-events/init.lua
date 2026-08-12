--------------------------------------------------------------------------------
-- Window lifecycle events — what fires when a window displays a buffer.
--
-- Run:
--   BEMTVI_CONFIG=examples/window-events cargo run -p bemtvi -- examples/window-events/sample.txt
--
-- Background: docs/autocmd-events.md, "Buffer lifecycle" and "Window, tab and
-- editor lifecycle". The rules these sections demonstrate are neovim's, measured
-- command by command against nvim 0.12.2.
--------------------------------------------------------------------------------

-- Every event below is appended here, newest last. `:Events` prints the log and
-- clears it, so each section starts from a clean slate.
local log = {}

local function tail(path)
  if path == nil or path == "" then
    return "(no name)"
  end
  return path:match("[^/]+$") or path
end

for _, event in ipairs({
  "BufLeave",
  "BufReadPost",
  "BufEnter",
  "BufWinEnter",
  "WinNew",
  "WinClosed",
  "WinEnter",
  "WinResized",
  "TabEnter",
}) do
  btv.on(event, function(a)
    log[#log + 1] = event .. "(" .. tail(a.file) .. ")"
  end)
end

btv.command("Events", function()
  print(#log == 0 and "no events" or table.concat(log, "  "))
  log = {}
end)

--------------------------------------------------------------------------------
-- 1. BufWinEnter is PER WINDOW, not per buffer.
--
-- It fires when a window starts displaying a buffer it wasn't displaying. Open
-- the same file in a second window and it fires *again* — that window has its own
-- per-window setup to do (a scrollbar, a statusline component, a fold state),
-- and the buffer already being on screen elsewhere is irrelevant to it.
--
-- Type-this:  :Events<CR>          (clear the startup log)
--             :vsplit sample.txt<CR>
--             :Events<CR>
-- See-that:   WinNew  BufEnter  WinEnter  BufWinEnter  WinResized
--             — a second window, a second fire. And note what is NOT there:
--             BufReadPost. The buffer is already here, so a split onto it reads
--             nothing; it only displays.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 2. A bare :split displays NOTHING new.
--
-- The new window inherits the buffer it was split off — nothing was displayed, so
-- nothing fires. `:split <file>` is a split *and then* a load into it, so that one
-- does fire. The two look identical from the outside; the editor knows which is
-- which because the split path records the inherit.
--
-- Type-this:  :Events<CR>
--             :split<CR>
--             :Events<CR>
-- See-that:   WinNew  WinEnter  WinResized — and no BufWinEnter at all.
--
-- Then contrast:
-- Type-this:  :Events<CR>
--             :split other.txt<CR>
--             :Events<CR>
-- See-that:   WinNew  WinEnter  WinResized, then the arrival of a file that
--             really was read: BufLeave(sample.txt)  BufReadPost(other.txt)
--             BufEnter(other.txt)  BufWinEnter(other.txt).
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 3. Navigation fires nothing about displays.
--
-- Switching tabs, or moving between windows with <C-w>w, changes which window has
-- focus — it displays nothing. So no BufWinEnter, and (the bug this example was
-- written for) no WinNew / WinClosed / WinResized either: the windows in the tab
-- you left go on existing while you are away.
--
-- Type-this:  :tabnew other.txt<CR>
--             :Events<CR>            (the new tab's own events — then clear)
--             :tabnext<CR>
--             :Events<CR>
-- See-that:   WinEnter and TabEnter, and nothing else — no WinNew, no WinClosed,
--             no WinResized, no BufWinEnter. (Whether BufLeave / BufEnter join
--             them is a separate question: they fire only if the tab you land in
--             shows a different buffer than the one you left.)
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 4. A reload re-runs the whole enter sequence.
--
-- `:e!` re-reads the file into the same buffer in the same window. Nothing
-- "changed" in any diff sense — same bufnr, same window — but the contents are
-- new, so everything that set itself up from those contents runs again.
--
-- Type-this:  :Events<CR>
--             :e!<CR>
--             :Events<CR>
-- See-that:   BufReadPost(sample.txt)  BufEnter(sample.txt)  BufWinEnter(sample.txt)
--             and no BufLeave — nothing was left.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 5. Why "per window" is the useful rule: real per-window state.
--
-- This is section 1 put to work. Each window that displays sample.txt gets its
-- own 'colorcolumn', cycling through a few positions, so you can see that the
-- handler really ran once per window rather than once per file.
--
-- `btv.wo` works here because the handler runs with the window that displayed as
-- the current one — the editor's focus does not move, but "current" inside the
-- handler is that window, so per-window state lands where it belongs even for a
-- window a session restore filled in the background.
--
-- That holds for the handler's synchronous run. If yours awaits something first,
-- capture the window before the await and write through the explicit handle:
--
--     btv.on("BufWinEnter", function()
--       local win = btv.win.current()
--       return some_async_thing():next(function() btv.wo[win].colorcolumn = "20" end)
--     end)
--
-- Type-this:  :vsplit sample.txt<CR>   then   :vsplit sample.txt<CR>
-- See-that:   each of the three windows shows its column rule in a different
--             place. Under a per-buffer rule the second and third windows would
--             have been skipped and shown no rule at all.
--------------------------------------------------------------------------------
local columns = { "20", "40", "60" }
local shown = 0

btv.on("BufWinEnter", function(a)
  if tail(a.file) ~= "sample.txt" then
    return
  end
  shown = shown + 1
  btv.wo.colorcolumn = columns[(shown - 1) % #columns + 1]
end)
