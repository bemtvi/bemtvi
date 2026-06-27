-- nx.plugins.ui — the lazy.nvim-style UI for the native package manager.
--
-- Two floating surfaces, both built on `nx.view.component` (the Vue-shaped reactive
-- component, prelude/component.lua) over the manager's own state — no buffer
-- mutation, no manual tick-dance:
--
--   * the WELCOME checklist (`M.ui.welcome`) — the first-run offer. nxvim ships
--     minimal; on a fresh setup it presents the recommended set pre-ticked, each item
--     untickable, and resolves to the chosen subset (driven from `M.bootstrap`).
--   * the MANAGER (`:Plugins` / `M.ui.open`) — the dashboard: every declared plugin
--     grouped by load state, with LIVE per-plugin progress (a spinner while a clone /
--     pull runs, a ✓/✗ on finish) wired through `M.on_change` + `M._tasks`, and the
--     verb keymaps (I install · U update · S sync · X clean).
--
-- Loaded AFTER prelude/plugins.lua: it reads the manager's `M.list` / `M.status` /
-- `M._tasks` / `M._specs` and subscribes via `M.on_change`, and it builds on
-- nx.view.component / nx.hl / nx.command / nx.timer — all installed above.

local M = nx.plugins
M.ui = M.ui or {}

-- ----- highlights -------------------------------------------------------------
-- Defined once at load (catppuccin-ish, so the UI reads well on a dark default even
-- before a colorscheme loads; a real colorscheme can redefine these groups).
local HL = {
  NxPluginsLoaded = { fg = "#a6e3a1" }, -- ● a loaded plugin
  NxPluginsInstalled = { fg = "#89b4fa" }, -- ○ installed, not yet loaded
  NxPluginsMissing = { fg = "#f38ba8" }, -- ○ declared but not on disk / a failed op
  NxPluginsBusy = { fg = "#f9e2af" }, -- the spinner frame while git runs
  NxPluginsHeader = { fg = "#b4befe", bold = true }, -- a section / title line
  NxPluginsDim = { fg = "#6c7086" }, -- counts, flags, details, the key hint
}
for name, spec in pairs(HL) do
  nx.hl.define(0, name, spec)
end

-- ----- spec display helpers ---------------------------------------------------

-- The short display name for a RAW spec (string shorthand or table): its `name`, or
-- the basename of its source / dir. Mirrors normalize()'s naming so the checklist
-- labels match what gets installed.
local function spec_label(s)
  if type(s) == "string" then
    return (s:gsub("%.git$", ""):gsub("[/\\]+$", ""):match("[^/\\]+$")) or s
  end
  if s.name then
    return s.name
  end
  local src = s.src or s.url or s[1] or s.dir
  return src and ((src:gsub("%.git$", ""):gsub("[/\\]+$", ""):match("[^/\\]+$")) or src) or "?"
end

-- The source string for a raw spec ("owner/repo" / url / dir), shown dimmed.
local function spec_source(s)
  if type(s) == "string" then
    return s
  end
  return s.src or s.url or s[1] or s.dir or ""
end

-- ----- the welcome checklist --------------------------------------------------

-- The lines before the first checklist item (the two intro lines + a blank
-- separator) — fixed so the cursor↔item math is exact.
local WELCOME_HEADER = 3

local Welcome = nx.view.component({
  setup = function(ctx, props)
    local items = {}
    for _, raw in ipairs(props.recommended) do
      items[#items + 1] = {
        label = spec_label(raw),
        source = spec_source(raw),
        desc = (type(raw) == "table" and raw.desc) or "",
        checked = true,
        spec = raw,
      }
    end
    local state = ctx.reactive({ items = items })

    -- Move the cursor within the item rows, clamped to the list.
    local function move(delta)
      local n = #state.items
      if n == 0 then
        return
      end
      local idx = (ctx.line() or WELCOME_HEADER + 1) - WELCOME_HEADER
      if idx < 1 then
        idx = 1
      end
      idx = ((idx - 1 + delta) % n) + 1
      ctx.set_cursor(WELCOME_HEADER + idx)
    end
    ctx.keymap_set("n", "j", function()
      move(1)
    end)
    ctx.keymap_set("n", "k", function()
      move(-1)
    end)
    ctx.keymap_set("n", "<Tab>", function()
      move(1)
    end, { desc = "Next" })
    ctx.keymap_set("n", "<S-Tab>", function()
      move(-1)
    end, { desc = "Previous" })

    ctx.keymap_set("n", "<Space>", function()
      local it = state.items[(ctx.line() or 0) - WELCOME_HEADER]
      if it then
        it.checked = not it.checked -- reactive write → re-render
      end
    end, { desc = "Toggle item" })

    ctx.keymap_set("n", "a", function()
      -- Toggle all: if everything is ticked, clear; otherwise tick everything.
      local all = true
      for _, it in ipairs(state.items) do
        if not it.checked then
          all = false
          break
        end
      end
      for _, it in ipairs(state.items) do
        it.checked = not all
      end
    end, { desc = "Toggle all" })

    ctx.keymap_set("n", "<CR>", function()
      local chosen = {}
      for _, it in ipairs(state.items) do
        if it.checked then
          chosen[#chosen + 1] = it.spec
        end
      end
      ctx.close()
      props.on_done(chosen)
    end, { desc = "Install selected" })

    local function skip()
      ctx.close()
      props.on_done({}) -- {} = skipped / installed nothing
    end
    ctx.keymap_set("n", "<Esc>", skip, { desc = "Skip" })
    ctx.keymap_set("n", "q", skip, { desc = "Skip" })

    -- Once the window exists (winid lands a tick after mount, AFTER the float grab
    -- has settled), set the window-local display — wrap long lines (the intro / hint
    -- / descriptions run past the float width) and inset the content from the border
    -- with `padding` — and land the cursor on the FIRST item. Doing the cursor here
    -- rather than via `nx.schedule` is what makes it stick: an earlier same-tick
    -- placement is reset to the top by the grab, so `j` would skip to the 2nd item.
    nx.wait_for(ctx.winid)
      :next(function()
        ctx.wo.wrap = true
        ctx.wo.padding = "1 2"
        -- Defer the cursor one more tick so it lands AFTER the `padding` relayout,
        -- which otherwise resets the float's cursor back to the top.
        nx.on_next_tick(function()
          ctx.set_cursor(WELCOME_HEADER + 1)
        end)
      end)
      :catch(function() end)

    return { items = state.items }
  end,

  render = function(view)
    local lines, decor = {}, {}
    local function add(text, hl)
      lines[#lines + 1] = text
      if hl then
        decor[#decor + 1] =
          { line = #lines - 1, col = 0, end_row = #lines - 1, end_col = #text, hl_group = hl }
      end
    end

    add("nxvim ships minimal by design — no bundled plugins.", "NxPluginsDim")
    add("These are recommended to get you started — untick any you don't want:", "NxPluginsDim")
    add("")

    local selected = 0
    for _, it in ipairs(view.items) do
      local box = it.checked and "☑" or "☐"
      -- Show the FULL source (the exact clone target — owner/repo, url, or dir) as the
      -- item text, NEVER a friendly basename. This is a trust gate: ticking a row will
      -- fetch and run that code, so the user must see precisely what they are approving
      -- (a benign `desc` must not be able to disguise a hostile source). The basename
      -- label is only a last-resort fallback for a spec that carries no source at all.
      local name = it.source ~= "" and it.source or it.label
      local text = box .. " " .. name
      -- The human description, when present, trails the source dim — extra context
      -- alongside the source, never a substitute for it. It is appended as REAL
      -- buffer text (not an eol virt_text) so a description longer than the float
      -- width WRAPS with the window (wrap=true) instead of being clipped at the edge.
      local desc_col
      if it.desc ~= "" then
        desc_col = #text
        text = text .. " — " .. it.desc
      end
      lines[#lines + 1] = text
      local line = #lines - 1
      decor[#decor + 1] = {
        line = line,
        col = 0,
        end_row = line,
        end_col = #box,
        hl_group = it.checked and "NxPluginsLoaded" or "NxPluginsDim",
      }
      if desc_col then
        decor[#decor + 1] = {
          line = line,
          col = desc_col,
          end_row = line,
          end_col = #text,
          hl_group = "NxPluginsDim",
        }
      end
      if it.checked then
        selected = selected + 1
      end
    end

    add(
      string.format(
        "%d of %d selected · <Space> toggle · a all · <CR> install · <Esc> skip",
        selected,
        #view.items
      ),
      "NxPluginsDim"
    )
    return { lines = lines, decor = decor }
  end,
})

-- M.ui.welcome(recommended) -> promise resolving to the chosen raw specs ({} on
-- skip/cancel). Backs the first-run flow in M.bootstrap.
function M.ui.welcome(recommended)
  return nx.promise.new(function(resolve)
    Welcome.mount({
      name = "nx-plugins-welcome",
      filetype = "nxpluginswelcome",
      float = {
        width = 74,
        -- Exact fit: the header lines (WELCOME_HEADER) + one row per item + the hint,
        -- plus the 2 rows the top/bottom `padding` insets, plus 4 spare rows so the
        -- longer descriptions (which now wrap as real text onto a second display row
        -- at this width — e.g. the debugger entry) aren't pushed below the float's
        -- bottom edge.
        height = #recommended + WELCOME_HEADER + 7,
        align = "center",
        border = "rounded",
        title = "  Welcome to nxvim  ",
        grab = true,
      },
      props = { recommended = recommended, on_done = resolve },
    })
  end)
end

-- ----- the manager dashboard --------------------------------------------------

local SPINNER = { "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏" }

-- Stringify a lazy trigger list for the detail view (a `keys` entry may be a table).
local function trig_str(list)
  local out = {}
  for _, v in ipairs(list) do
    out[#out + 1] = type(v) == "table" and tostring(v.lhs or v[1]) or tostring(v)
  end
  return table.concat(out, ", ")
end

local Manager = nx.view.component({
  setup = function(ctx)
    -- `tick` forces a re-render when manager state (non-reactive: tasks / load flags)
    -- changes; `spin` advances the spinner; `status` holds the disk-checked installed
    -- map; `expanded` tracks which rows show details.
    local state = ctx.reactive({ tick = 0, spin = 0, status = {}, expanded = {} })
    local line_to_name = {} -- rendered line (1-based) -> plugin name, rebuilt each render

    -- Pull the disk-checked `installed` flags into reactive state (off-tick).
    local function refresh_status()
      nx.async(function()
        local rows = nx.await(M.status())
        local map = {}
        for _, r in ipairs(rows) do
          map[r.name] = r.installed
        end
        state.status = map
      end)():catch(function(e)
        nx.notify("nx.plugins.ui: " .. tostring(e and e.message or e), 4)
      end)
    end
    refresh_status()

    -- Spinner: while any task is running, advance a frame every 80ms; stop when idle.
    local spinning = false
    local function busy()
      for _, t in pairs(M._tasks) do
        if t.state == "running" then
          return true
        end
      end
      return false
    end
    local function ensure_spin()
      if spinning or not busy() then
        return
      end
      spinning = true
      local function step()
        if busy() then
          state.spin = state.spin + 1
          nx.timer(step, 80)
        else
          spinning = false
          state.tick = state.tick + 1 -- a final repaint to clear the spinners
        end
      end
      step()
    end

    -- Re-render + refresh installed flags + (re)arm the spinner on any manager change.
    local unsub = M.on_change(function()
      state.tick = state.tick + 1
      refresh_status()
      ensure_spin()
    end)
    ctx.on_close(unsub)

    local function run(p)
      p:catch(function(e)
        nx.notify(tostring(e and e.message or e), 4)
      end)
    end

    -- A freshly-cloned plugin is on the runtimepath, but its `plugin/` scripts and
    -- (for an eager plugin) its `config` only run cleanly from a clean startup — so
    -- after an install that actually clones something, prompt to restart. Centered,
    -- transient popup (the next key dismisses it). `M.ui._restart_shown` records that
    -- it fired (an introspection hook for tests).
    local function restart_popup(n)
      if (n or 0) < 1 then
        return
      end
      M.ui._restart_shown = true
      nx.ui.float({
        "Installed " .. n .. " new plugin(s).",
        "",
        "Restart nxvim to finish loading them.",
      }, { title = " Restart required ", relative = "editor", border = "rounded" })
    end

    -- Run a verb that may install, reporting errors and popping the restart notice
    -- when its promise resolves a non-zero install count.
    local function run_installing(p)
      p:next(restart_popup):catch(function(e)
        nx.notify(tostring(e and e.message or e), 4)
      end)
    end

    ctx.keymap_set("n", "I", function()
      run_installing(M.install())
    end, { desc = "Install missing" })
    ctx.keymap_set("n", "U", function()
      run(M.update())
    end, { desc = "Update plugins" })
    ctx.keymap_set("n", "S", function()
      run_installing(M.sync())
    end, { desc = "Sync (install + update)" })
    ctx.keymap_set("n", "X", function()
      run(M.clean())
    end, { desc = "Clean undeclared" })
    ctx.keymap_set("n", "r", function()
      refresh_status()
      state.tick = state.tick + 1
    end, { desc = "Refresh" })
    ctx.keymap_set("n", "<CR>", function()
      local name = line_to_name[ctx.line()]
      if name then
        state.expanded[name] = not state.expanded[name]
      end
    end, { desc = "Toggle details" })
    local function close()
      ctx.close()
    end
    ctx.keymap_set("n", "q", close, { desc = "Close" })
    ctx.keymap_set("n", "<Esc>", close, { desc = "Close" })

    -- Window-local display, set once the window exists: wrap long rows (the key hint,
    -- a row's trailing status) instead of clipping at the border, and inset the
    -- content with `padding` for breathing room.
    nx.wait_for(ctx.winid)
      :next(function()
        ctx.wo.wrap = true
        ctx.wo.padding = "1 2"
      end)
      :catch(function() end)

    return { state = state, line_to_name = line_to_name }
  end,

  render = function(view)
    local st = view.state
    local frame = SPINNER[(st.spin % #SPINNER) + 1]
    local status = st.status or {}
    local plugins = M.list()
    local tasks = M._tasks

    -- Rebuild the line→name map for <CR>.
    local map = view.line_to_name
    for k in pairs(map) do
      map[k] = nil
    end

    local lines, decor = {}, {}
    -- add(text[, hl]) -> the 1-based line number just appended.
    local function add(text, hl)
      lines[#lines + 1] = text
      if hl then
        decor[#decor + 1] =
          { line = #lines - 1, col = 0, end_row = #lines - 1, end_col = #text, hl_group = hl }
      end
      return #lines
    end

    -- Bucket by load/install state (installed defaults optimistic until the async
    -- check lands, then refines on the next tick).
    local loaded, ready, missing = {}, {}, {}
    for _, p in ipairs(plugins) do
      local inst = status[p.name]
      if inst == nil then
        inst = true
      end
      if p.loaded then
        loaded[#loaded + 1] = p
      elseif inst then
        ready[#ready + 1] = p
      else
        missing[#missing + 1] = p
      end
    end

    add(
      string.format(
        "%d plugins · %d loaded · %d not loaded · %d missing",
        #plugins,
        #loaded,
        #ready,
        #missing
      ),
      "NxPluginsDim"
    )
    add("")

    if #plugins == 0 then
      add("No plugins declared yet.", "NxPluginsDim")
      add(
        "Add some with nx.plugins{ … } in your config, then press I to install.",
        "NxPluginsDim"
      )
    end

    -- A detail line, nested one step under its plugin row.
    local function detail(label, val)
      if val == nil or val == "" then
        return
      end
      add("    " .. label .. ": " .. val, "NxPluginsDim")
    end

    local function section(title, list, icon, icon_hl)
      if #list == 0 then
        return
      end
      add(title .. " (" .. #list .. ")", "NxPluginsHeader")
      for _, p in ipairs(list) do
        local task = tasks[p.name]
        local sym, shl = icon, icon_hl
        if task and task.state == "running" then
          sym, shl = frame, "NxPluginsBusy"
        elseif task and task.state == "error" then
          sym, shl = "✗", "NxPluginsMissing"
        end

        -- Items sit one step under their section header.
        local text = "  " .. sym .. " " .. p.name
        local ln = add(text)
        local line = ln - 1
        decor[#decor + 1] =
          { line = line, col = 2, end_row = line, end_col = 2 + #sym, hl_group = shl }
        map[ln] = p.name

        -- Dim suffix: flags, then a SHORT live task word. The error word is "failed"
        -- (the captured stderr can be long / multi-line — it goes in the detail view,
        -- not inline where it would break the virt_text).
        local bits = {}
        if p.lazy then
          bits[#bits + 1] = "lazy"
        end
        if p.pinned then
          bits[#bits + 1] = "pinned"
        end
        local suffix = table.concat(bits, " ")
        local word
        if task then
          word = task.state == "error" and "failed" or task.msg
        end
        if word and word ~= "" then
          suffix = (suffix ~= "" and (suffix .. " · ") or "") .. word
        end
        if suffix ~= "" then
          decor[#decor + 1] =
            { line = line, col = #text, virt_text = { { " — " .. suffix, "NxPluginsDim" } } }
        end

        if st.expanded[p.name] then
          local spec = M._specs[p.name]
          if spec then
            detail("url", spec.url)
            detail("dir", spec._dir)
            detail("branch", spec.branch)
            detail("tag", spec.tag)
            detail("commit", spec.commit)
            local trg = spec._triggers
            if trg then
              for _, k in ipairs({ "cmd", "event", "ft", "keys" }) do
                if trg[k] and #trg[k] > 0 then
                  detail(k, trig_str(trg[k]))
                end
              end
            end
          end
          if task and task.state == "error" and task.msg then
            -- First line of the captured stderr — the why.
            detail("error", (task.msg:gsub("%s+$", "")):match("^[^\n]*") or task.msg)
          end
        end
      end
      add("") -- a blank line of breathing room between sections
    end

    section("Loaded", loaded, "●", "NxPluginsLoaded")
    section("Not loaded", ready, "○", "NxPluginsInstalled")
    section("Missing", missing, "○", "NxPluginsMissing")

    add(
      "I install · U update · S sync · X clean · r refresh · <CR> details · q quit",
      "NxPluginsDim"
    )
    return { lines = lines, decor = decor }
  end,
})

-- M.ui.open() — open (or, if already open, leave focused) the manager dashboard.
function M.ui.open()
  if M.ui._instance and not M.ui._instance._closed then
    return M.ui._instance
  end
  M.ui._instance = Manager.mount({
    name = "nx-plugins",
    filetype = "nxplugins",
    float = {
      -- Screen-relative and centered: reflows on resize, always framed in the middle.
      width = "80%",
      height = "80%",
      align = "center",
      border = "rounded",
      title = "  nxvim plugins  ",
      grab = true,
    },
    props = {},
  })
  return M.ui._instance
end

nx.command("Plugins", function()
  M.ui.open()
end, { desc = "Open the nx.plugins manager UI (lazy-style dashboard)." })

-- :PluginsWelcome — open the first-run welcome checklist ON DEMAND, ignoring the
-- ask-once marker and the "no plugins declared yet" gate that `M.bootstrap` checks.
-- Confirming runs the same accept path as first-run (persist the chosen subset to the
-- managed `<config>/lua/plugins.lua`, declare them, and sync), so it both lets a user
-- re-pick from the recommended set and gives a way to preview the surface. Note it
-- OVERWRITES the managed plugins.lua with the chosen set.
nx.command("PluginsWelcome", function()
  if #M._recommended == 0 then
    return nx.notify(
      "nx.plugins: no recommended set registered — call nx.plugins.recommend{...} first",
      3
    )
  end
  M.ui
    .welcome(M._recommended)
    :next(function(chosen)
      if not chosen or #chosen == 0 then
        return
      end
      return nx.async(function()
        nx.await(M._persist_recommended(chosen))
        M.add(chosen)
        nx.await(M.sync())
      end)()
    end)
    :catch(function(err)
      nx.notify("nx.plugins: " .. tostring(err and err.message or err), 4)
    end)
end, { desc = "Open the recommended-plugins welcome checklist and install the chosen set." })

return M.ui
