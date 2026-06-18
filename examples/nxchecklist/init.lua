-- ~~~ nxvim nx.view.component playground: a modal floating checkbox dialog ~~~
--
-- Run it (from the repo root):
--
--     NXVIM_CONFIG=examples/nxchecklist cargo run -p nxvim
--
-- This is the use case `nx.view` floats were built for — a centered floating checkbox
-- list you navigate with <Tab>, toggle with <Space>, and confirm/cancel — written with
-- `nx.view.component`, the Vue-shaped component model:
--
--   * setup(ctx, props)  runs ONCE: it makes reactive state and binds keys. (It may be
--                        async — `nx.await` a fetch right inside it. See ASYNC below.)
--   * render(state)      is PURE: state -> { lines, decor }. The framework re-runs it
--                        automatically every time the reactive state changes.
--
-- Note what is GONE versus driving the raw handle: no `nx.schedule` to wait for the
-- buffer number, no manual re-render after each toggle, no bufnr juggling. The framework
-- owns the lifecycle; you write state + a pure render.
--
-- TRY IT interactively:
--   <leader>c        re-open the dialog
--   <Tab> / <S-Tab>  (or j/k) move between items
--   <Space>          toggle the item under the cursor
--   <CR>             confirm — the checked labels are echoed
--   <Esc>            cancel

nx.hl.define(0, "NxChecklistOn", { fg = "#a6e3a1" })
nx.hl.define(0, "NxChecklistHint", { fg = "#6c7086" })

local Checklist = nx.view.component({
  setup = function(ctx, props)
    -- The dialog's only mutable state. Writing `it.checked = …` below re-renders.
    local state = ctx.reactive({ items = props.items })

    -- Derived state: the count of ticked items. Recomputed only when an item's `checked`
    -- changes (not on cursor movement), and cached otherwise.
    local selected = ctx.computed(function()
      local n = 0
      for _, it in ipairs(state.items) do
        if it.checked then
          n = n + 1
        end
      end
      return n
    end)

    local function move(delta)
      local n = #state.items
      ctx.set_cursor((ctx.line() - 1 + delta) % n + 1) -- wrap within the item rows
    end
    -- ctx.keymap_set mirrors nx.keymap.set(mode, lhs, rhs, opts), with buffer + nowait
    -- defaulted to this dialog (override either via opts).
    ctx.keymap_set("n", "<Tab>", function()
      move(1)
    end, { desc = "Next item" })
    ctx.keymap_set("n", "<S-Tab>", function()
      move(-1)
    end, { desc = "Previous item" })
    ctx.keymap_set("n", "j", function()
      move(1)
    end)
    ctx.keymap_set("n", "k", function()
      move(-1)
    end)

    ctx.keymap_set("n", "<Space>", function()
      local it = state.items[ctx.line()]
      if it then
        it.checked = not it.checked -- reactive write → automatic re-render
      end
    end, { desc = "Toggle item" })

    ctx.keymap_set("n", "<CR>", function()
      local chosen = {}
      for _, it in ipairs(state.items) do
        if it.checked then
          chosen[#chosen + 1] = it.label
        end
      end
      ctx.close()
      props.on_done(chosen)
    end, { desc = "Confirm" })

    ctx.keymap_set("n", "<Esc>", function()
      ctx.close()
      props.on_done(nil) -- nil = cancelled
    end, { desc = "Cancel" })

    -- Setup returns the bindings the template reads — the reactive items and the derived
    -- count (Vue's setup-returns-the-template-scope shape).
    return { items = state.items, selected = selected }
  end,

  -- Pure: bindings in, screen out. Re-run for you on every reactive change.
  render = function(view)
    local lines, decor = {}, {}
    for i, it in ipairs(view.items) do
      lines[i] = (it.checked and "☑  " or "☐  ") .. it.label
      if it.checked then
        decor[#decor + 1] =
          { line = i - 1, col = 0, end_row = i - 1, end_col = 3, hl_group = "NxChecklistOn" }
      end
    end
    lines[#lines + 1] = ""
    local hint = string.format(
      "%d selected  •  <Tab> move  <Space> toggle  <CR> ok  <Esc> cancel",
      view.selected() -- the computed; cached unless an item's checked state changed
    )
    lines[#lines + 1] = hint
    decor[#decor + 1] = {
      line = #lines - 1,
      col = 0,
      end_row = #lines - 1,
      end_col = #hint,
      hl_group = "NxChecklistHint",
    }
    return { lines = lines, decor = decor }
  end,
})

local ITEMS = {
  { label = "Format on save" },
  { label = "Inlay hints", checked = true },
  { label = "Auto-pairs" },
  { label = "Relative line numbers", checked = true },
  { label = "Trim trailing whitespace" },
}

local function demo()
  Checklist.mount({
    name = "nxchecklist",
    filetype = "nxchecklist",
    float = {
      width = 52,
      height = #ITEMS + 4,
      border = "rounded",
      title = " Editor options ",
      grab = true,
    },
    props = {
      items = ITEMS,
      on_done = function(chosen)
        if chosen == nil then
          _G.nxchecklist_result = "<cancelled>"
          nx.notify("checklist cancelled", 2)
        else
          _G.nxchecklist_result = table.concat(chosen, ", ")
          nx.notify("enabled: " .. _G.nxchecklist_result, 2)
        end
      end,
    },
  })
end

nx.keymap.set("n", "<leader>c", demo, { desc = "Open the checklist dialog" })

-- ASYNC: setup and render can both await. A real dialog whose items come from disk would
-- write setup like this — note it reads top-to-bottom, no callbacks, and the framework
-- doesn't render until the awaited data has arrived:
--
--   setup = function(ctx, props)
--     local entries = nx.await(nx.fs.readdir(props.path))   -- suspends; resumes with data
--     return ctx.reactive({ items = to_items(entries) })
--   end,

-- Open it once at startup so the playground shows something immediately.
nx.schedule(demo)
