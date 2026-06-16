-- ~~~ nxvim native which-key: a live popup of pending-key hints ~~~
--
-- Run it (from the repo root):
--
--     NXVIM_CONFIG=examples/which-key \
--       cargo run -p nxvim -- examples/which-key/sample.txt
--
-- TRY IT: press <leader> (Space) and pause. A bordered popup appears in the
-- BOTTOM-RIGHT corner (relative = "bottom", the classic which-key spot) listing
-- every key that can follow — `w write`, `q quit`, `f +file`, `g +git`. Keep
-- typing into a group (`f`) and the popup REFRESHES to that group's keys;
-- complete a mapping, break the sequence, or wait out the timeout and it closes.
--
-- The built-in command grammar feeds the SAME popup: pause after `z` for the
-- viewport commands (zt/zz/zb…), after `<C-w>` for the window commands, after `g`
-- for the go-to motions merged with the LSP `g` maps, and mid-`f` or after a lone
-- `d` for an "awaiting input" hint card. Once the leader timeout commits `g` to the
-- built-in grammar, the LSP `g` maps can no longer fire — they stay listed but
-- marked `(×)` (`available == false`) so they don't vanish before you've read them.
--
-- This is a real which-key built from TWO nx APIs and nothing else — no blocking
-- key reads, no key interception:
--
--   * nx.on_key_pending(fn)   the engine's pending-prefix ORACLE. The server
--                             watches the mapped-prefix trie and pushes a context
--                             — { mode, keys, continuations = {{key,desc,kind}}, label }
--                             — every time the withheld prefix changes (grows /
--                             descends / clears). It is fire-on-change, not
--                             per-keystroke (ADR 0002 rule 4: no per-key Lua). The
--                             built-in grammar arrives over the SAME event (source
--                             B): the OPEN states (`f` find-char, `r` replace, marks,
--                             operator-pending `d`/`c`/`y`) have no key list, so they
--                             carry a `label` — pausing mid-`f` shows "Find
--                             character". The FINITE built-in prefixes (`z` →
--                             zt/zz/zb…, `g` → gg/gt/g;…, `<C-w>` → the window
--                             commands) carry enumerated `continuations`, rendered
--                             just like the leader menu. For `g` the engine MERGES
--                             the built-in motions with any maps that share the `g`
--                             prefix (the LSP `gd`/`gD`/`gr` defaults), so one popup
--                             shows both.
--   * nx.ui.float(.., {persist=true})   a persistent content float; the returned
--                             handle's :update() repaints it in place and :close()
--                             dismisses it. The popup survives keystrokes so it can
--                             track the sequence as you type.
--   * nx.utils.debounce(fn, ms)         coalesce the oracle's bursts so a fast,
--                             deliberate sequence (`<Space>w` typed quickly) never
--                             flashes the popup — it only appears when you PAUSE.
--
-- The whole "plugin" is the ~30 lines at the bottom. Everything above is just the
-- demo keymaps it reads — which-key shows whatever maps exist, with their `desc`.

vim.g.mapleader = " "

-- A small leader menu. `ff`/`fg` and `gs`/`gc` are two-key sequences, so `f` and
-- `g` show up as GROUPS (`kind = "group"`) that lead deeper; the single-key maps
-- complete immediately (`kind = "map"`, carrying their `desc`).
nx.keymap.set("n", "<leader>w", function()
  print("write")
end, { desc = "write" })
nx.keymap.set("n", "<leader>q", function()
  print("quit")
end, { desc = "quit" })
nx.keymap.set("n", "<leader>ff", function()
  print("find file")
end, { desc = "find file" })
nx.keymap.set("n", "<leader>fg", function()
  print("live grep")
end, { desc = "live grep" })
nx.keymap.set("n", "<leader>gs", function()
  print("git status")
end, { desc = "git status" })
nx.keymap.set("n", "<leader>gc", function()
  print("git commit")
end, { desc = "git commit" })

-- How long to wait, after the LAST key, before the popup appears (ms). Real
-- which-key uses ~200ms so quick sequences stay invisible.
local DELAY = vim.g.which_key_delay or 200

-- ---------------------------------------------------------------------------
-- The plugin: render one pending context into the float.
-- ---------------------------------------------------------------------------

-- Lay the continuations out as an aligned `key  label` grid. Groups get a `+`
-- so a path that only leads deeper reads differently from one that fires. The
-- key column is padded to the widest *display width* (not byte length), so wide
-- or multibyte keys still line up.
--
-- Source B (the built-in grammar: `f` find-char, `r` replace, marks, …) has NO
-- discrete keys to list — its continuation set is open — so it arrives with an
-- empty `continuations` and a `ctx.label` instead. We render that as a single
-- hint card ("Find character"), which is how typing `f` and pausing now shows a
-- popup rather than silently waiting for the target char.
local function lines_for(ctx)
  if #ctx.continuations == 0 then
    return { string.format(" %s ", ctx.label or "…") }
  end
  local keyw = 1
  for _, c in ipairs(ctx.continuations) do
    keyw = math.max(keyw, vim.fn.strdisplaywidth(c.key))
  end
  local rows = {}
  for _, c in ipairs(ctx.continuations) do
    local pad = string.rep(" ", keyw - vim.fn.strdisplaywidth(c.key))
    local label
    if c.kind == "group" then
      label = "+" .. (c.desc ~= "" and c.desc or "more")
    else
      label = c.desc ~= "" and c.desc or ""
    end
    -- `available == false` is a continuation kept visible but no longer firable —
    -- a mapped `g` key (gd/gD/gr) surfaced after the leader timeout committed `g`
    -- to the built-in grammar. nx.ui.float has no inline highlight yet (so no real
    -- graying), so cue it with a trailing marker. See the plan doc's Phase 4 note.
    if c.available == false then
      label = label .. "  (×)"
    end
    rows[#rows + 1] = string.format(" %s%s   %s ", c.key, pad, label)
  end
  return rows
end

local popup -- the open float handle, or nil

-- Debounced opener: builds the grid and opens/repaints the float. Debouncing
-- means many oracle events in a burst collapse to one render of the LAST one.
local open = nx.utils.debounce(function(ctx)
  local lines = lines_for(ctx)
  -- Title the popup `keys — label` so the prefix isn't cryptic: a bare `d` reads as
  -- "d — Delete", `g` as "g — Go". Source-A leader prefixes have no label, so they
  -- title with just the keys (" <Space> ").
  local title = " " .. ctx.keys
  if ctx.label and ctx.label ~= "" then
    title = title .. " — " .. ctx.label
  end
  title = title .. " "
  if popup and popup:is_open() then
    popup:update(lines, { title = title, relative = "bottom" })
  else
    popup = nx.ui.float(lines, {
      persist = true,
      title = title,
      border = "rounded",
      relative = "bottom",
    })
  end
end, DELAY)

nx.on_key_pending(function(ctx)
  -- Cleared context (prefix completed, broke, or timed out): drop any pending
  -- open and close the popup at once — no debounce on the way down, so it never
  -- lingers after you've answered. A live source-B state (find-char, …) has empty
  -- continuations but a non-empty `keys`/`label`, so gate on `keys` alone — an
  -- empty continuation list is NOT "close" anymore.
  if ctx.keys == "" then
    open:cancel()
    if popup then
      popup:close()
      popup = nil
    end
    return
  end
  open(ctx)
end)
