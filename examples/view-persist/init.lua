-- ~~~ nxvim nx.view.component persistence: a sidebar that survives a restart ~~~
--
-- Run it (from the repo root):
--
--     NXVIM_CONFIG=examples/view-persist \
--       cargo run -p nxvim -- --workspace examples/view-persist
--
-- `--workspace` makes this a session-scoped launch (the editor captures + restores the
-- window/tab layout), and `nx.shada.save_layout(true)` below opts the layout capture in.
-- The sidebar is an `nx.view.component` mounted with a stable `persist` id: the editor
-- round-trips only that id and the view's slot — never its content. The component owns the
-- content, loading and saving it through `ctx.store` (its own `nx.shada.plugin()` slice,
-- keyed by the same id). One mount call; the framework picks fresh-vs-restore for you.
--
-- TRY IT:
--   <leader>na   add a note to the list (saved immediately)
--   <leader>nd   delete the note under the cursor
--   <CR>         echo the note under the cursor
--   :qa          quit — then re-run the SAME command above
--
-- After the restart the sidebar comes back in its dock with every note you added: the
-- editor reserved the slot and the component re-ran `setup`, which rebuilt the list from
-- this plugin's own store. Closing the view for good drops the saved notes — the editor
-- never GCs your data for you.

nx.shada.save_layout(true) -- opt the window/tab layout into the session capture

local PERSIST_ID = "notes" -- the stable id this view rides the session by
local STORE_KEY = "view:" .. PERSIST_ID -- where this plugin keeps its own state

-- The "pinned notes" sidebar, as a reactive component. `setup` runs once the surface is
-- ready — whether it was mounted fresh or adopted from a restored session slot, the
-- framework picks — and owns the side effects (load, key binds, persistence). `render` is
-- pure: it maps the current notes to the lines on screen and the framework re-runs it
-- automatically on every mutation.
local Notes = nx.view.component({
  setup = function(ctx)
    -- The notes, loaded from this component's own cross-session store (`ctx.store`, keyed by
    -- the same persist id), or a friendly default on first run. Held as reactive state, so a
    -- write re-renders the view.
    local notes = ctx.reactive({
      list = ctx.store:get(STORE_KEY) or { "Welcome! Press <leader>na to add a note." },
    })

    -- A plain-array snapshot of the reactive list — what crosses to the store (a reactive
    -- proxy serializes empty). Persist after every mutation, so the sidebar is one `:qa` +
    -- re-run away from coming back intact.
    local function save()
      local plain = {}
      for _, note in ipairs(notes.list) do
        plain[#plain + 1] = note
      end
      ctx.store:set(STORE_KEY, plain)
    end

    -- <CR> echoes the note under the cursor.
    ctx.view:on_select(function(line)
      nx.notify("note: " .. (notes.list[line] or ""))
    end)

    -- The editor never deletes your saved state — drop it yourself when the user closes the
    -- view for good, so an abandoned sidebar leaves no orphaned data behind.
    ctx.view:on_close(function()
      ctx.store:delete(STORE_KEY)
    end)

    -- Mutations: writing the reactive list re-renders; then persist. `ctx.keymap_set` binds
    -- buffer-locally to this view.
    ctx.keymap_set("n", "<leader>na", function()
      notes.list[#notes.list + 1] = "note " .. (#notes.list + 1)
      save()
    end, { desc = "Add a persisted note" })

    ctx.keymap_set("n", "<leader>nd", function()
      local line = ctx.line()
      if line and notes.list[line] then
        table.remove(notes.list, line)
        save()
      end
    end, { desc = "Delete the note under the cursor" })

    return notes
  end,

  -- Pure render: the current list IS the lines. (The backend materializes the reactive list
  -- before it crosses to the view, so returning it directly is fine.)
  render = function(notes)
    return { lines = notes.list }
  end,
})

-- Mount it in the left dock under a stable persist id. The framework resolves the owning
-- namespace (here `user`, the config root), threads it through the backing view + the store,
-- and on a restart adopts the reserved slot instead of opening a fresh one — no on_restore
-- handler, no VimEnter fallback, no manual save/render wiring: the component owns all of it.
Notes.mount({
  name = "Notes",
  filetype = "nxnotes",
  persist = PERSIST_ID,
  dock = "left",
  size = 36,
})
