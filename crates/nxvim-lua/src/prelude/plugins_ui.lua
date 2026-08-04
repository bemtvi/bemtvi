-- nx.plugins.ui — the lazy.nvim-style UI for the native package manager.
--
-- Two floating surfaces, both built on `nx.view.component` (the Vue-shaped reactive
-- component, prelude/component.lua) over the manager's own state — no buffer
-- mutation, no manual tick-dance:
--
--   * the WELCOME offer (`M.ui.welcome`) — the first-run ask. nxvim ships minimal; on
--     a fresh setup it offers the recommended set as ONE decision (install / skip),
--     with `c` opening the customize CHECKLIST behind it (every plugin pre-ticked and
--     untickable) and `?` opening the set's reference page in a browser. Either screen
--     resolves the same promise with the chosen subset (driven from `M.bootstrap`).
--   * the MANAGER (`:Plugins` / `M.ui.open`) — the dashboard: every declared plugin
--     grouped by load state, with LIVE per-plugin progress (a spinner while a clone /
--     pull runs, a ✓/✗ on finish) wired through `M.on_change` + `M._tasks`, and the
--     verb keymaps in two scopes: UPPER-case for the whole set (I install · U update ·
--     S sync · R restore · X clean) and lower-case for the plugin under the cursor
--     (i · u · s · r · x, each passing `{ plugins = name }` to the same verb).
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
-- the basename of its source / dir — via the manager's own naming helper
-- (`nx.plugins._source_name`), so the checklist labels match exactly what
-- normalize() would install.
local function spec_label(s)
  if type(s) == "string" then
    return nx.plugins._source_name(s) or s
  end
  if s.name then
    return s.name
  end
  local src = s.src or s.url or s[1] or s.dir
  return src and (nx.plugins._source_name(src) or src) or "?"
end

-- The source string for a raw spec ("owner/repo" / url / dir), shown dimmed.
local function spec_source(s)
  if type(s) == "string" then
    return s
  end
  return s.src or s.url or s[1] or s.dir or ""
end

-- Where a source comes FROM, one step up from the repository itself:
-- `nxvim/nxvim-tree` -> `github.com/nxvim`, `https://git.sr.ht/~u/p` -> `git.sr.ht/~u`,
-- a path or `file://` -> its parent directory. The offer screen shows the distinct
-- origins of the set instead of listing every source, so the *trust* question ("whose
-- code is this?") is still answerable there — the exact per-plugin sources are one `c`
-- away on the checklist.
local function spec_origin(s)
  local src = spec_source(s)
  if src == "" then
    return ""
  end
  local host, path = src:match("^%a[%w+.-]*://([^/]+)/(.*)$")
  if host then
    -- A URL: keep the host plus its first path segment (the owner), dropping the repo.
    local owner = path:match("^([^/]+)/")
    if owner and path:match("^[^/]+/[^/]+/?$") then
      return host .. "/" .. owner
    end
    -- Not an owner/repo shape (a file:// path, a deep URL): the containing directory.
    return (src:gsub("/+$", ""):gsub("/[^/]*$", ""))
  end
  local owner = src:match("^([%w._-]+)/[%w._-]+$")
  if owner then
    return "github.com/" .. owner -- the manager's own default for `owner/repo`
  end
  return (src:gsub("/+$", ""):gsub("/[^/]*$", "")) -- a plain path: its parent
end

-- The distinct origins of a recommended set, in first-seen order. Recurses into each
-- spec's `dependencies`: a dependency is fetched and run exactly like a top-level
-- plugin, so an origin the user never chose directly still belongs in the summary.
local function set_origins(specs)
  local seen, out = {}, {}
  local function walk(list)
    for _, s in ipairs(list) do
      local o = spec_origin(s)
      if o ~= "" and not seen[o] then
        seen[o] = true
        out[#out + 1] = o
      end
      local deps = type(s) == "table" and (s.dependencies or s.deps)
      if deps then
        walk(deps)
      end
    end
  end
  walk(specs)
  return out
end

-- ----- the customize checklist ------------------------------------------------

-- The lines before the first checklist item (the two intro lines + a blank
-- separator) — fixed so the cursor↔item math is exact.
local WELCOME_HEADER = 3

-- Hand a URL to the platform opener, reporting either way — the `?` key on both
-- welcome screens. A remote/headless session may have no opener at all, which is why
-- the URL is also rendered as plain text for copying.
local function open_doc()
  local url = nx.plugins.RECOMMENDED_DOC_URL
  nx.ui
    .open(url)
    :next(function()
      nx.notify("opened " .. url)
    end)
    :catch(function(err)
      nx.notify(
        "nx.plugins: could not open " .. url .. " (" .. tostring(err and err.message or err) .. ")",
        3
      )
    end)
end

local Customize = nx.view.component({
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

    ctx.keymap_set("n", "?", open_doc, { desc = "Open the reference page for the set" })

    -- `c` is what got the user HERE from the offer screen, so it is deliberately inert
    -- rather than unbound: a repeated press (this screen mounts a tick after the key)
    -- would otherwise reach the `c` operator and leave the view in operator-pending,
    -- swallowing the next keys.
    ctx.keymap_set("n", "c", function() end, { desc = "Customize (already here)" })

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

    add("Untick anything you don't want — each row is fetched from the", "NxPluginsDim")
    add("source shown, then declared in your config.", "NxPluginsDim")
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
        "%d of %d selected · <Space> toggle · a all · <CR> install · ? reference · <Esc> skip",
        selected,
        #view.items
      ),
      "NxPluginsDim"
    )
    return { lines = lines, decor = decor }
  end,
})

-- Mount the customize checklist over a recommended set; `on_done` receives the chosen
-- raw specs ({} on skip). Reached from the offer screen's `c`, never mounted directly
-- by the bootstrap — the first ask is the one-decision offer below.
local function mount_customize(recommended, on_done)
  -- Size to the CONTENT, not to the item count: rows wrap (`wrap = true`, so a
  -- source + description past the inner width takes a second display row), and a set
  -- of a dozen wrapping rows silently loses its last items — and the hint line —
  -- below the bottom edge otherwise. Estimate the wrapped rows the same way the
  -- renderer builds them, then clamp both axes to the screen.
  local cols, rows_avail = nx.o.columns, nx.o.lines
  local width = math.max(40, math.min(88, cols - 6))
  local inner = width - 4 -- the border's 2 columns + `padding = "1 2"` on each side
  local rows = WELCOME_HEADER + 2 -- the header block + the (wrapping) hint line
  for _, raw in ipairs(recommended) do
    local src = spec_source(raw)
    local name = src ~= "" and src or spec_label(raw)
    local desc = (type(raw) == "table" and raw.desc) or ""
    local text = "☑ " .. name .. (desc ~= "" and (" — " .. desc) or "")
    rows = rows + math.max(1, math.ceil(nx.str.displaywidth(text) / inner))
  end
  Customize.mount({
    name = "nx-plugins-customize",
    filetype = "nxpluginscustomize",
    float = {
      width = width,
      -- + the 2 rows the top/bottom `padding` insets, + 1 spare. Past the screen the
      -- list simply scrolls under `j` / `k`.
      height = math.min(rows + 3, math.max(10, rows_avail - 4)),
      align = "center",
      border = "rounded",
      title = "  Choose your plugins  ",
      grab = true,
    },
    props = { recommended = recommended, on_done = on_done },
  })
end

-- ----- the first-run offer ----------------------------------------------------

-- The offer is ONE decision — install the recommended set, or don't — because the set
-- is long enough that listing it here would bury the choice. What it still shows is
-- the size of the set and the ORIGINS its code comes from (the trust question), with
-- `c` opening the checklist for the exact per-plugin sources and `?` the reference
-- page describing every plugin.
local Offer = nx.view.component({
  setup = function(ctx, props)
    local function accept()
      ctx.close()
      props.on_done(props.recommended)
    end
    ctx.keymap_set("n", "<CR>", accept, { desc = "Install the recommended set" })
    ctx.keymap_set("n", "y", accept, { desc = "Install the recommended set" })

    ctx.keymap_set("n", "c", function()
      -- Hand off to the checklist on the NEXT tick: mounting a second grabbing float
      -- in the same tick this one closes races the layer teardown.
      ctx.close()
      nx.on_next_tick(function()
        mount_customize(props.recommended, props.on_done)
      end)
    end, { desc = "Customize the set" })

    ctx.keymap_set("n", "?", open_doc, { desc = "Open the reference page for the set" })

    local function skip()
      ctx.close()
      props.on_done({}) -- {} = skipped / installed nothing
    end
    ctx.keymap_set("n", "<Esc>", skip, { desc = "Skip" })
    ctx.keymap_set("n", "q", skip, { desc = "Skip" })
    ctx.keymap_set("n", "n", skip, { desc = "Skip" })

    -- Wrap the long lines rather than clipping them at the border, and inset the
    -- content from it (both window-local, so they wait for the winid).
    nx.wait_for(ctx.winid)
      :next(function()
        ctx.wo.wrap = true
        ctx.wo.padding = "1 2"
      end)
      :catch(function() end)

    return { recommended = props.recommended }
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
    -- An action row: the key highlighted, its explanation dim.
    local function action(key, what)
      local text = string.format("  %-7s %s", key, what)
      lines[#lines + 1] = text
      local line = #lines - 1
      local kcol = 2
      decor[#decor + 1] = {
        line = line,
        col = kcol,
        end_row = line,
        end_col = kcol + #key,
        hl_group = "NxPluginsHeader",
      }
      decor[#decor + 1] = {
        line = line,
        col = kcol + #key,
        end_row = line,
        end_col = #text,
        hl_group = "NxPluginsDim",
      }
    end

    add("nxvim ships minimal by design — no bundled plugins.", "NxPluginsDim")
    add("")
    add("Install the recommended set?", "NxPluginsHeader")
    -- The size and the ORIGINS on their own row: together with the question they run
    -- past the float width and would wrap mid-word.
    add(
      string.format(
        "%d plugins from %s.",
        #view.recommended,
        table.concat(set_origins(view.recommended), ", ")
      ),
      "NxPluginsDim"
    )
    add("")
    action("<CR>", "Install all of them")
    action("c", "Customize — see every plugin and pick individually")
    action("?", "What's in the set — opens the page below in your browser")
    action("<Esc>", "Skip — :PluginsWelcome reopens this any time")
    add("")
    add(nx.plugins.RECOMMENDED_DOC_URL, "NxPluginsDim")
    return { lines = lines, decor = decor }
  end,
})

-- M.ui.welcome(recommended) -> promise resolving to the chosen raw specs ({} on
-- skip/cancel). Backs the first-run flow in M.bootstrap: the offer screen, with the
-- customize checklist one `c` behind it (both resolve this one promise).
function M.ui.welcome(recommended)
  return nx.promise.new(function(resolve)
    Offer.mount({
      name = "nx-plugins-welcome",
      filetype = "nxpluginswelcome",
      float = {
        width = 74,
        -- The offer's layout is fixed (11 rows), plus the 2 rows the top/bottom
        -- `padding` insets and 1 spare so a long origins list (or an action line, on a
        -- narrow terminal) can take a second display row without falling off the
        -- bottom edge.
        height = 14,
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
  setup = function(ctx, props)
    -- `tick` forces a re-render when manager state (non-reactive: tasks / load flags)
    -- changes; `spin` advances the spinner; `status` holds the disk-checked installed
    -- map; `expanded` tracks which rows show details.
    local state = ctx.reactive({ tick = 0, spin = 0, status = {}, expanded = {} })
    local line_to_name = {} -- rendered line (1-based) -> plugin name, rebuilt each render

    -- Pull the disk-checked `installed` flags into reactive state (off-tick).
    local function refresh_status()
      nx.async(function()
        local rows = nx.await(M.status())
        local map, drift = {}, {}
        for _, r in ipairs(rows) do
          map[r.name] = r.installed
          drift[r.name] = r.drifted
        end
        state.status = map
        -- Drift (the checkout is at a different commit than the lockfile records) rides
        -- the same off-tick status pass, so the row suffix can show it without its own read.
        state.drift = drift
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
    -- transient content float: the next key dismisses it, INCLUDING the manager's
    -- own <Esc>/q maps, since a transient float is now wiped at the per-key dispatch
    -- level (not only when a key reaches `Editor::input`) — so a single <Esc> both
    -- clears this notice and closes the manager beneath it, no grabbing modal needed.
    -- `M.ui._restart_shown` records that it fired (an introspection hook for tests).
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

    -- UPPER-CASE verbs act on the whole declared set; the lower-case twins below act on
    -- the plugin under the cursor. (A row is the obvious unit of work here — you open
    -- the dashboard because ONE plugin needs installing or updating — so the same verbs
    -- have to be reachable per row, not only wholesale.)
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
    ctx.keymap_set("n", "R", function()
      -- Same reporting as `:PluginRestore` (per-plugin failures loud, then a summary) —
      -- shared rather than re-worded here so the two surfaces can't drift apart.
      run(M.restore():next(M._restore_notify))
    end, { desc = "Restore to the lockfile" })

    -- The plugin whose row the cursor is on, or nil — reported on the message line
    -- rather than silently doing nothing, so a verb pressed on a header / blank / the
    -- hint line says why it did not run.
    local function under_cursor(verb)
      local name = line_to_name[ctx.line()]
      if not name then
        nx.notify("nx.plugins: no plugin under the cursor to " .. verb, 3)
      end
      return name
    end
    -- Bind a lower-case per-row verb: resolve the row, then run `fn(name)`.
    local function row_verb(key, verb, fn)
      ctx.keymap_set("n", key, function()
        local name = under_cursor(verb)
        if name then
          fn(name)
        end
      end, { desc = verb:sub(1, 1):upper() .. verb:sub(2) .. " this plugin" })
    end
    row_verb("i", "install", function(name)
      run_installing(M.install({ plugins = name }))
    end)
    row_verb("u", "update", function(name)
      run(M.update({ plugins = name }))
    end)
    row_verb("s", "sync", function(name)
      run_installing(M.sync({ plugins = name }))
    end)
    row_verb("r", "restore", function(name)
      run(M.restore({ plugins = name }):next(M._restore_notify))
    end)
    row_verb("x", "remove", function(name)
      -- Deletes this plugin's clone (never a dev `dir` checkout — `clean` fails loud on
      -- one). The pair with `i` is the "give me a fresh copy" move.
      run(M.clean({ plugins = name }))
    end)

    -- Refresh lives on `<C-r>` because `r` is the per-row restore above, matching the
    -- upper/lower split of every other verb.
    ctx.keymap_set("n", "<C-r>", function()
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

    -- First-run / `:PluginsWelcome` opens the manager WITH an install to run (rather
    -- than syncing silently in the background): kick it off once mounted so the
    -- dashboard shows the live per-plugin progress (spinner → ✓/✗) and pops the
    -- restart notice on completion, right here in the same reused verb path as `S`.
    if props and props.sync_on_open then
      run_installing(M.sync())
    end

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
        if (st.drift or {})[p.name] then
          bits[#bits + 1] = "drifted"
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
            -- The author's own `desc` leads: it says what the plugin is *for*, which
            -- is what a reader opening a row wants before the mechanics below it.
            detail("desc", spec.desc)
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

    -- Two hint lines, because there are two scopes: the upper-case verbs act on
    -- everything, their lower-case twins on the row under the cursor.
    add(
      "all:  I install · U update · S sync · R restore · X clean · <C-r> refresh · q quit",
      "NxPluginsDim"
    )
    add(
      "this: i install · u update · s sync · r restore · x remove · <CR> details",
      "NxPluginsDim"
    )
    return { lines = lines, decor = decor }
  end,
})

-- M.ui.open(opts) — open (or, if already open, leave focused) the manager dashboard.
-- `opts.sync_on_open` makes the freshly-mounted manager run a sync immediately (the
-- welcome / first-run path: show the dashboard, then the live install status), reusing
-- the same verb path as pressing `S`. Ignored if the manager is already open.
function M.ui.open(opts)
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
    props = { sync_on_open = opts and opts.sync_on_open or nil },
  })
  return M.ui._instance
end

nx.command("Plugins", function()
  M.ui.open()
end, { desc = "Open the nx.plugins manager UI (lazy-style dashboard)." })

-- :PluginsWelcome — open the first-run welcome offer ON DEMAND, ignoring the
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
        -- Open the manager and let IT run the install, so the chosen set installs in
        -- view (live progress) instead of silently in the background.
        M.ui.open({ sync_on_open = true })
      end)()
    end)
    :catch(function(err)
      nx.notify("nx.plugins: " .. tostring(err and err.message or err), 4)
    end)
end, { desc = "Open the recommended-plugins welcome checklist and install the chosen set." })

return M.ui
