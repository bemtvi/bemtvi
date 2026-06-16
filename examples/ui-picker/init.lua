-- ~~~ nxvim nx.picker playground: the fuzzy finder ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/ui-picker \
--       cargo run -p nxvim -- examples/ui-picker/sample.txt
--
-- `nx.picker` is the native fuzzy finder built on the unified float-list widget
-- (docs/specs/2026-06-14-nx-ui-float-widget.md). The SERVER owns it: a centered
-- float with a PROMPT that grabs every key, a Rust fuzzy matcher that re-ranks as
-- you type (matched chars are highlighted), and — for a dynamic source like live
-- grep — a generation token so a response for a query you have already typed past
-- is dropped. No input loop runs in Lua (ADR 0002).
--
-- In the open picker:
--   type            edit the query (the document is NOT touched)
--   <C-n> / <C-p>   move the selection down / up (also <Down> / <Up>)
--   <CR>            confirm — runs the source's action on the highlighted item
--   <Esc>           cancel
--
-- Sources are thin: they STREAM candidates via `ctx.push`, signal completion by
-- returning (an `nx.async` source returns its promise; nx is promise-only — no
-- `done` callback), and handle `confirm(item)`.
-- The built-in `files` / `live_grep` / `buffers` ship with nxvim; this config maps
-- them and registers one custom source to show the shape.

vim.g.mapleader = "\\"

--------------------------------------------------------------------------------
-- 1. The shipped sources.
--    \ff  files     — fuzzy file finder (streams `rg --files`, matched locally)
--    \fg  live_grep — live grep      (re-runs `rg` after you pause typing; dynamic)
--    \fb  buffers   — pick an open buffer (in-memory; no process)
--------------------------------------------------------------------------------
-- Dynamic sources (live_grep) debounce: they re-run only after you stop typing for
-- `nx.picker.debounce` ms (default 250), the in-flight search is cancelled when you
-- type again, and the previous results stay on screen until the new ones arrive.
-- Tune it globally, per source (`debounce = N`), or per open (below).
nx.picker.debounce = 250

nx.keymap.set("n", "<leader>ff", function()
  nx.picker.open("files")
end)
nx.keymap.set("n", "<leader>fg", function()
  nx.picker.open("live_grep")
end)
nx.keymap.set("n", "<leader>fb", function()
  nx.picker.open("buffers")
end)
-- A snappier live grep just for this map (override the debounce per open):
nx.keymap.set("n", "<leader>fG", function()
  nx.picker.open("live_grep", { debounce = 100 })
end)

--------------------------------------------------------------------------------
-- 2. A custom STATIC source — a fixed list, fuzzy-matched in Rust as you type.
--    \fc  colours  — pick a colour; the choice is echoed.
--
--    The picker is a FIXED box (never content-hugging — that looks ragged). Set
--    the size per source with `width` / `height`: a cell count (e.g. 100) or a
--    CSS-style viewport fraction string — "80vw" / "60vh" / "50%". Omit them for
--    the default (~80vw x 60vh). `nx.picker.open(name, { width=, height= })`
--    overrides per-open.
--
--    `prompt_pos` chooses where the input box sits: "top" (the default) puts it
--    above the results, "bottom" puts it below them (telescope-style). This source
--    asks for the input BELOW the results.
--------------------------------------------------------------------------------
nx.picker.source({
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
    nx.notify("you picked " .. item.text)
  end,
})
nx.keymap.set("n", "<leader>fc", function()
  nx.picker.open("colours")
end)
-- … or override the size at open time (a compact 40x10 cell box):
nx.keymap.set("n", "<leader>fC", function()
  nx.picker.open("colours", { width = 40, height = 10 })
end)

--------------------------------------------------------------------------------
-- 3. A custom DYNAMIC source — re-run per keystroke, the matcher bypassed. This
--    one just echoes the query back (a real one would stream from a process and
--    reap the superseded job via `ctx.on_cancel`, as `live_grep` does).
--    \fe  echo
--------------------------------------------------------------------------------
nx.picker.source({
  name = "echo",
  dynamic = true,
  items = function(ctx)
    if ctx.query ~= "" then
      ctx.push({ text = "search: " .. ctx.query, q = ctx.query })
      ctx.push({ text = "again:  " .. ctx.query, q = ctx.query })
    end
  end,
  confirm = function(item)
    nx.notify("confirmed query: " .. (item.q or ""))
  end,
})
nx.keymap.set("n", "<leader>fe", function()
  nx.picker.open("echo")
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
-- their install path; nxvim exposes the full `debug` library to user config).
local here = debug.getinfo(1, "S").source:sub(2):match("(.*)[/\\]") or "."
nx.picker.source({
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
nx.keymap.set("n", "<leader>fp", function()
  nx.picker.open("preview")
end)

nx.notify("nx.picker playground — try \\ff \\fg \\fb \\fc \\fe \\fp")
