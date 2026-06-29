-- ~~~ nxvim nx.view persistence: a sidebar that survives a restart ~~~
--
-- Run it (from the repo root):
--
--     NXVIM_CONFIG=examples/view-persist \
--       cargo run -p nxvim -- --workspace examples/view-persist
--
-- `--workspace` makes this a session-scoped launch (the editor captures + restores
-- the window/tab layout), and `nx.shada.save_layout(true)` below opts the layout
-- capture in. On top of that, a *plugin view* can opt into the session too: pass a
-- stable `persist` id to `nx.view.create`, and the editor round-trips only that id
-- and the view's slot — never its content. Your plugin owns the content and saves
-- whatever it needs in its own `nx.shada.plugin()` store, keyed by the same id.
--
-- TRY IT:
--   <leader>na   add a note to the list (it's saved immediately)
--   <leader>nd   delete the note under the cursor
--   <CR>         echo the note under the cursor
--   :qa          quit — then re-run the SAME command above
--
-- After the restart the sidebar comes back in its dock with every note you added:
-- the editor reserved the slot, and the `on_restore` handler below rebuilt the view
-- from this plugin's shada. Delete the view for good (close its window) and the
-- saved notes are dropped — the editor never GCs your data for you.

nx.shada.save_layout(true) -- opt the window/tab layout into the session capture

local PERSIST_ID = "notes" -- the stable id this view rides the session by
local STORE_KEY = "view:" .. PERSIST_ID -- where this plugin keeps its own state

-- This plugin's own cross-session store (namespace = "user", the config root). Keyed
-- by STORE_KEY, it survives restarts exactly like registers / marks do.
local function store()
  return nx.shada.plugin()
end

-- The notes to show, loaded from our store (or a friendly default on first run).
local function load_notes()
  return store():get(STORE_KEY) or { "Welcome! Press <leader>na to add a note." }
end

local view = nil -- the live handle, once built
local notes = {} -- the current list of note strings (the source of truth)

-- Persist the current list. Called after every mutation, so the sidebar is always
-- one `:qa` + re-run away from coming back intact.
local function save_notes()
  store():set(STORE_KEY, notes)
end

-- Push `notes` into the view's buffer (views are set wholesale, not edited).
local function render()
  if view then
    view:set_lines(notes)
  end
end

-- Build the view over the current `notes`. When `place` is given (the restore path)
-- we drop the view into the slot the session reserved; otherwise (a fresh run) we
-- mount it ourselves in the left dock.
local function build_view(place)
  local v = nx.view.create({ name = "Notes", filetype = "nxnotes", persist = PERSIST_ID })
  v:set_lines(notes)
  v:on_select(function(line)
    nx.notify("note: " .. (notes[line] or ""))
  end)
  -- The editor never deletes your saved state — do it yourself when the user closes
  -- the view for good, so an abandoned sidebar doesn't leave orphaned data behind.
  v:on_close(function()
    store():delete(STORE_KEY)
  end)
  if place then
    place(v) -- adopt the reserved restore slot
  else
    v:mount({ dock = "left", size = 36 })
  end
  view = v
  return v
end

-- Restore path: the editor reserved this view's slot at boot and now hands it back.
-- We load our own saved notes and rebuild the view into the reserved window.
nx.view.on_restore(function(id, place)
  notes = store():get("view:" .. id) or load_notes()
  build_view(place)
  nx.layer.main() -- start focus in the editor, not the sidebar
end)

-- Fresh-start path: if nothing was restored by VimEnter, open the sidebar ourselves.
nx.autocmd.create("VimEnter", {
  callback = function()
    if not view then
      notes = load_notes()
      build_view(nil)
      nx.layer.main()
    end
  end,
})

-- Mutations: add / delete a note, then re-render + save.
nx.keymap.set("n", "<leader>na", function()
  notes[#notes + 1] = "note " .. (#notes + 1)
  render()
  save_notes()
end, { desc = "Add a persisted note" })

nx.keymap.set("n", "<leader>nd", function()
  local line = view and view:line()
  if line and notes[line] then
    table.remove(notes, line)
    render()
    save_notes()
  end
end, { desc = "Delete the note under the cursor" })
