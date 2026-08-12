-- ~~~ bemtvi btv.picker playground: the fuzzy finder ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/ui-picker \
--       cargo run -p bemtvi -- examples/ui-picker/sample.txt
--
-- `btv.picker` is the native fuzzy finder built on the unified float-list widget
-- (docs/specs/2026-06-14-btv-ui-float-widget.md). The SERVER owns it: a centered
-- float with a PROMPT that grabs every key, a Rust fuzzy matcher that re-ranks as
-- you type (matched chars are highlighted), and — for a dynamic source like live
-- grep — a generation token so a response for a query you have already typed past
-- is dropped. No input loop runs in Lua (ADR 0002).
--
-- In the open picker:
--   type            edit the query (the document is NOT touched)
--   <C-n> / <C-p>   move the selection down / up (also <Down> / <Up>)
--   <CR>            confirm — runs the source's action on the highlighted item
--   <C-t>           open the highlighted item in a NEW TAB (telescope's select_tab)
--   <C-x> / <C-v>   open it in a horizontal / vertical SPLIT
--   <Esc>           cancel
--
-- Picker keys are ordinary `picker`-mode maps, so rebind any of them — e.g. the
-- default tab key is `btv.keymap.set("picker", "<C-t>", btv.picker.actions.confirm_tab)`.
--
-- Sources are thin: they STREAM candidates via `ctx.push`, signal completion by
-- returning (an `btv.async` source returns its promise; btv is promise-only — no
-- `done` callback), and handle `confirm(item)`.
-- The built-in `files` / `live_grep` / `buffers` ship with bemtvi; this config maps
-- them and registers one custom source to show the shape.

vim.g.mapleader = "\\"

--------------------------------------------------------------------------------
-- 1. The shipped sources.
--    \ff  files     — fuzzy file finder (streams `rg --files`, matched locally)
--    \fg  live_grep — live grep      (re-runs `rg` after you pause typing; dynamic)
--    \fb  buffers   — pick an open buffer (in-memory; no process). Each row
--                    leads with the `:ls` facts: bufnr, `%` current / `#`
--                    alternate, `a`ctive/`h`idden, `+` modified, and the
--                    buffer's last cursor line — type-this: `:ls` and compare.
--------------------------------------------------------------------------------
-- Dynamic sources (live_grep) debounce: they re-run only after you stop typing for
-- `btv.picker.debounce` ms (default 250), the in-flight search is cancelled when you
-- type again, and the previous results stay on screen until the new ones arrive.
-- Tune it globally, per source (`debounce = N`), or per open (below).
btv.picker.debounce = 250

btv.keymap.set("n", "<leader>ff", function()
  btv.picker.open("files")
end)
btv.keymap.set("n", "<leader>fg", function()
  btv.picker.open("live_grep")
end)
btv.keymap.set("n", "<leader>fb", function()
  btv.picker.open("buffers")
end)
-- A snappier live grep just for this map (override the debounce per open):
btv.keymap.set("n", "<leader>fG", function()
  btv.picker.open("live_grep", { debounce = 100 })
end)

-- Where a confirmed pick LANDS is governed by 'switchbuf'. bemtvi defaults it to
-- `usetab`: picking (or any jump to) a buffer ALREADY shown in another tab focuses
-- that tab instead of re-opening it in the current window. Try it: \fb a file, hit
-- <C-t> to drop it in a new tab, switch back (`gT`), \fb that same file again with
-- <CR> — focus jumps to the tab already showing it. Tune it like vim:
--   btv.o.switchbuf = "usetab"   -- the default; also "useopen" (current tab only)
--   btv.o.switchbuf = ""         -- classic: always open in the current window
-- (<C-t> always makes a NEW tab regardless — it is an explicit tab gesture.)

--------------------------------------------------------------------------------
-- 2. A custom STATIC source — a fixed list, fuzzy-matched in Rust as you type.
--    \fc  colours  — pick a colour; the choice is echoed.
--
--    The picker is a FIXED box (never content-hugging — that looks ragged). Set
--    the size per source with `width` / `height`: a cell count (e.g. 100) or a
--    CSS-style viewport fraction string — "80vw" / "60vh" / "50%". Omit them for
--    the default (~80vw x 60vh). `btv.picker.open(name, { width=, height= })`
--    overrides per-open.
--
--    `prompt_pos` chooses where the input box sits: "top" (the default) puts it
--    above the results, "bottom" puts it below them (telescope-style). This source
--    asks for the input BELOW the results.
--------------------------------------------------------------------------------
btv.picker.source({
  name = "colours",
  width = "50vw", -- half the editor width …
  height = "40vh", -- … and 40% of its height
  prompt_pos = "bottom", -- input box UNDER the results
  items = function(ctx)
    for _, c in ipairs({
      "crimson",
      "cornflower",
      "chartreuse",
      "cerulean",
      "magenta",
      "marigold",
      "midnight",
      "mauve",
    }) do
      ctx.push({ text = c })
    end
  end,
  confirm = function(item)
    btv.notify("you picked " .. item.text)
  end,
})
btv.keymap.set("n", "<leader>fc", function()
  btv.picker.open("colours")
end)
-- … or override the size at open time (a compact 40x10 cell box):
btv.keymap.set("n", "<leader>fC", function()
  btv.picker.open("colours", { width = 40, height = 10 })
end)

--------------------------------------------------------------------------------
-- 3. A custom DYNAMIC source — re-run per keystroke, the matcher bypassed. This
--    one just echoes the query back (a real one would stream from a process and
--    reap the superseded job via `ctx.on_cancel`, as `live_grep` does).
--    \fe  echo
--------------------------------------------------------------------------------
btv.picker.source({
  name = "echo",
  dynamic = true,
  items = function(ctx)
    if ctx.query ~= "" then
      ctx.push({ text = "search: " .. ctx.query, q = ctx.query })
      ctx.push({ text = "again:  " .. ctx.query, q = ctx.query })
    end
  end,
  confirm = function(item)
    btv.notify("confirmed query: " .. (item.q or ""))
  end,
})
btv.keymap.set("n", "<leader>fe", function()
  btv.picker.open("echo")
end)

--------------------------------------------------------------------------------
-- 4. The PREVIEW pane (Phase 3). A source declares `preview = "file"` or
--    `preview = "location"`; each item then carries the fields that kind needs —
--    `path` (both kinds) and `row` / `col` (location) — and the SERVER renders the
--    file natively into a column beside the list (no Lua runs as the selection
--    moves). The shipped sources already opt in: `files` / `buffers` preview the
--    file ("file"); `live_grep` scrolls to and marks each match ("location").
--
--    Below is a custom "file" source over this example's own files, so you can see
--    the pane swap as you move the selection.
--    \fp  preview
--------------------------------------------------------------------------------
-- This config's own directory, so the source can reference its sibling files
-- regardless of how the config was loaded (`debug.getinfo` is how plugins locate
-- their install path; bemtvi exposes the full `debug` library to user config).
local here = debug.getinfo(1, "S").source:sub(2):match("(.*)[/\\]") or "."
btv.picker.source({
  name = "preview",
  preview = "file", -- the pane on the right shows the highlighted file's head
  items = function(ctx)
    for _, name in ipairs({ "sample.txt", "notes.txt", "init.lua" }) do
      ctx.push({ text = name, path = here .. "/" .. name })
    end
  end,
  confirm = function(item)
    vim.cmd("edit " .. item.path)
  end,
})
btv.keymap.set("n", "<leader>fp", function()
  btv.picker.open("preview")
end)

btv.notify("btv.picker playground — try \\ff \\fg \\fb \\fc \\fe \\fp (and <C-t>/<C-x>/<C-v> in any picker)")
