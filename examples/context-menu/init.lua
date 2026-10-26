-- ~~~ bemtvi context menu: a right-click menu made of groups ~~~
--
-- Run it from the repo root (any client: the terminal, the GUI, the browser):
--
--     BEMTVI_CONFIG=examples/context-menu \
--       cargo run -p bemtvi -- examples/context-menu/sample.txt
--
-- The menu is built from groups: each buffer has an ordered list of group names
-- (`btv.contextmenu.list(buf)`), each name refers to a group in
-- `btv.contextmenu.groups`, and each group gives rows. See
-- docs/features/context-menu.md.

-- 1. The menu. Right-click `banana` on line 1: the cursor moves there and a menu
--    opens at the pointer. Click `Copy`: line 1 is on the clipboard. Select `quick`
--    (double-click it) and right-click inside the selection: it stays selected, and
--    Cut joins the menu. A click anywhere else, or `<Esc>`, dismisses it.

-- 2. Hide a group. Undo and Redo are a group of their own, `undo`; with this, the
--    menu no longer offers them (`u` and `<C-r>` still work).
btv.contextmenu.groups.undo.when = false

-- 3. Add a row to a group. The `clipboard` group's rows are a plain list: this adds
--    `Copy Word`, shown only when nothing is selected. Right-click `cherry`, click
--    `Copy Word`: the clipboard holds `cherry`.
table.insert(btv.contextmenu.groups.clipboard.items, {
  label = "Copy Word",
  detail = '"+yiw',
  when = function(ctx)
    return not ctx.selection
  end,
  action = function()
    btv.cmd('normal! "+yiw')
  end,
})

-- 4. A new group. `case` changes the word under the cursor, in an editable buffer
--    only (a group's `when` hides the whole group). It goes into the DEFAULT list,
--    right after `clipboard`, so every buffer opened from now on shows it. Right-click
--    `fox` and click `UPPERCASE`: it reads `FOX`.
btv.contextmenu.groups.case = {
  when = function(ctx)
    return ctx.modifiable and not ctx.selection
  end,
  items = {
    {
      label = "UPPERCASE",
      action = function()
        btv.cmd("normal! gUiw")
      end,
    },
    {
      label = "lowercase",
      action = function()
        btv.cmd("normal! guiw")
      end,
    },
  },
}
table.insert(btv.contextmenu.default, 3, "case") -- { "undo", "clipboard", "case", … }

-- 5. One buffer's own order. `<leader>m` moves the `commands` group (Command Palette)
--    to the top of THIS buffer's menu. Press it, right-click: the palette comes first.
--    (`<leader>` is `\` unless your config sets `mapleader`.) Then `:enew`,
--    right-click in the new buffer: its menu keeps the default order.
btv.keymap.set("n", "<leader>m", function()
  local list = btv.contextmenu.list(0)
  for i, name in ipairs(list) do
    if name == "commands" then
      table.insert(list, 1, table.remove(list, i))
      return
    end
  end
end, { desc = "Context menu: palette first in this buffer" })

-- 6. A group that answers later. `file` reads the file's size from disk (`btv.fs.stat`
--    returns a promise) before it can say what to show; the menu waits for it, and
--    keeps it where the list puts it. Right-click anywhere in the sample: the last
--    group shows the size, greyed out (it's information, not a verb), and
--    `Copy Path`.
btv.contextmenu.groups.file = {
  when = function(ctx)
    return btv.buf.name(ctx.buf) ~= ""
  end,
  items = function(ctx)
    local path = btv.buf.name(ctx.buf)
    return btv.fs.stat(path):next(function(st)
      return {
        { label = st.size .. " bytes on disk", enabled = false, action = function() end },
        {
          label = "Copy Path",
          action = function()
            return btv.clipboard.write(path)
          end,
        },
      }
    end)
  end,
}
table.insert(btv.contextmenu.default, "file")
