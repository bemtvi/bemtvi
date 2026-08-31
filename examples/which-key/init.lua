-- ~~~ bemtvi native which-key: a live popup of pending-key hints ~~~
--
-- Run it (from the repo root):
--
--     BEMTVI_CONFIG=examples/which-key \
--       cargo run -p bemtvi -- examples/which-key/sample.txt
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
-- built-in grammar, the LSP `g` maps can no longer fire — the oracle still lists
-- them with `available == false`, and this plugin DROPS those rows so the popup only
-- ever shows keys you can actually press.
--
-- This is a real which-key built as an `btv.component` over the pending-key ORACLE — no
-- blocking key reads, no key interception. The component (reactive state + a pure render +
-- lifecycle) is the SAME model the checklist dialog uses; the only difference is the
-- SURFACE: which-key renders on the non-focus "float" backend (it must never take focus or
-- bind keys), the checklist on the focus-taking "view". The two btv signals it reads:
--
--   * btv.on_key_pending(fn)   the engine's pending-prefix ORACLE. The server
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
--   * btv.component{ surface="float" }   the component renders onto a persistent
--                             btv.ui.float under the hood (a non-focus content float that
--                             survives keystrokes). The component owns the open/refresh/
--                             close — an empty `render` hides it — so the plugin never
--                             touches the float handle directly.
--   * btv.utils.debounce(fn, ms)         coalesce the oracle's bursts so a fast,
--                             deliberate sequence (`<Space>w` typed quickly) never
--                             flashes the popup — it only appears when you PAUSE.
--
-- The whole "plugin" is the ~30 lines at the bottom. Everything above is just the
-- demo keymaps it reads — which-key shows whatever maps exist, with their `desc`.

vim.g.mapleader = " "

-- which-key's own highlight groups, so the popup is PRETTY — keys, group labels,
-- and descriptions each in their own colour. Defined explicitly (not borrowed from
-- the colorscheme) so the demo looks right with no theme loaded; a real config
-- would link these to its scheme. Phase 4 of the source-B plan gave `btv.ui.float`
-- per-segment highlighting (a line can be a list of `{ text, hl_group }` chunks),
-- which is what makes this colouring possible.
btv.hl.define(0, "WhichKey", { fg = "#7dcfff" }) -- the key itself (cyan)
btv.hl.define(0, "WhichKeyGroup", { fg = "#bb9af7", bold = true }) -- a +prefix group
btv.hl.define(0, "WhichKeyDesc", { fg = "#c0caf5" }) -- a mapping's description

-- A small leader menu. `ff`/`fg` and `gs`/`gc` are two-key sequences, so `f` and
-- `g` show up as GROUPS (`kind = "group"`) that lead deeper; the single-key maps
-- complete immediately (`kind = "map"`, carrying their `desc`).
btv.keymap.set("n", "<leader>w", function()
  print("write")
end, { desc = "write" })
btv.keymap.set("n", "<leader>q", function()
  print("quit")
end, { desc = "quit" })
btv.keymap.set("n", "<leader>ff", function()
  print("find file")
end, { desc = "find file" })
btv.keymap.set("n", "<leader>fg", function()
  print("live grep")
end, { desc = "live grep" })
btv.keymap.set("n", "<leader>gs", function()
  print("git status")
end, { desc = "git status" })
btv.keymap.set("n", "<leader>gc", function()
  print("git commit")
end, { desc = "git commit" })

-- How long to wait, after the LAST key, before the popup appears (ms). Real
-- which-key uses ~200ms so quick sequences stay invisible.
local DELAY = vim.g.which_key_delay or 200

-- ---------------------------------------------------------------------------
-- The plugin: render one pending context into the float.
-- ---------------------------------------------------------------------------

-- Lay the continuations out as an aligned `key  label` grid. Each row is a list
-- of `{ text, hl_group }` CHUNKS (the Phase 4 styled-float form), so the key, the
-- separator, and the description each get their own colour. Groups get a `+` and a
-- distinct group colour, so a path that only leads deeper reads differently from
-- one that fires. The key column is padded to the widest *display width* (not byte
-- length), so wide or multibyte keys still line up.
--
-- Source B (the built-in grammar: `f` find-char, `r` replace, marks, …) has NO
-- discrete keys to list — its continuation set is open — so it arrives with an
-- empty `continuations` and a `ctx.label` instead. We render that as a single
-- hint card ("Find character"), which is how typing `f` and pausing now shows a
-- popup rather than silently waiting for the target char.
local function lines_for(ctx)
  -- Keep only continuations that can still fire. A continuation with
  -- `available == false` is a mapped key (e.g. the LSP `gd`/`gD`/`gr` defaults)
  -- the oracle still reports after the leader timeout committed its prefix to the
  -- built-in grammar — pressing it now does nothing, so we drop it rather than
  -- show a dead row.
  local conts = {}
  for _, c in ipairs(ctx.continuations) do
    if c.available ~= false then
      conts[#conts + 1] = c
    end
  end
  -- An open continuation set (source B: `f` find-char, `r` replace, marks, …) has
  -- no discrete keys and arrives with a `ctx.label` instead; a context whose only
  -- continuations were unavailable drops to empty here too. Either way, render the
  -- label as a single hint card ("Find character").
  if #conts == 0 then
    return { { { string.format(" %s ", ctx.label or "…"), "WhichKeyDesc" } } }
  end
  local keyw = 1
  for _, c in ipairs(conts) do
    keyw = math.max(keyw, vim.fn.strdisplaywidth(c.key))
  end
  local rows = {}
  for _, c in ipairs(conts) do
    local pad = string.rep(" ", keyw - vim.fn.strdisplaywidth(c.key))
    local label, label_hl
    if c.kind == "group" then
      label = "+" .. (c.desc ~= "" and c.desc or "more")
      label_hl = "WhichKeyGroup"
    else
      label = c.desc ~= "" and c.desc or ""
      label_hl = "WhichKeyDesc"
    end
    rows[#rows + 1] = {
      { " ", nil },
      { c.key, "WhichKey" },
      { pad .. "   ", nil },
      { label, label_hl },
      { " ", nil },
    }
  end
  return rows
end

-- The plugin is a FLOAT-backed btv.component: the pending context is reactive state, a pure
-- `render` maps it to the popup's rows, and an EMPTY render hides the popup — so the popup's
-- whole show/refresh/hide lifecycle is declarative. The same component model the checklist
-- dialog uses, but on the "float" surface — which takes NO focus and binds NO keys (which-key
-- must never interrupt the sequence you're typing), instead of the focus-taking "view".
btv
  .component({
    surface = "float",
    setup = function(ctx)
      -- The one piece of state: the current pending context (or nil when there's none).
      local state = ctx.reactive({ pending = nil })

      -- Debounce the SHOW so a fast, deliberate sequence (`<Space>w` typed quickly) never
      -- flashes the popup — it only appears when you PAUSE. The HIDE is immediate (below), so
      -- the popup never lingers after you've answered.
      local show = btv.utils.debounce(function(c)
        state.pending = c
      end, DELAY)

      btv.on_key_pending(function(c)
        -- Cleared context (prefix completed, broke, or timed out): cancel the pending show
        -- and hide at once. A live source-B state (find-char, …) has empty continuations but
        -- a non-empty `keys`/`label`, so gate on `keys` alone.
        if c.keys == "" then
          show:cancel()
          state.pending = nil
        else
          show(c)
        end
      end)

      return state
    end,

    -- Pure: the pending context in, the popup's rows out. `nil` → an empty render → hidden.
    render = function(state)
      local c = state.pending
      if not c then
        return { lines = {} }
      end
      -- Title the popup `keys — label` so the prefix isn't cryptic: a bare `d` reads as
      -- "d — Delete". Source-A leader prefixes have no label, so they title with the keys.
      local title = " " .. c.keys
      if c.label and c.label ~= "" then
        title = title .. " — " .. c.label
      end
      return { lines = lines_for(c), title = title .. " " }
    end,
  })
  .mount({ relative = "bottom", border = "rounded" })
