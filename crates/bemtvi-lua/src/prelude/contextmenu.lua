-- bemtvi Lua prelude — the context menu (`btv.contextmenu`).
--
-- A right-click opens a menu at the pointer, in every session, modeless or vim. The
-- menu is built from GROUPS: each buffer has an ordered list of group names, each
-- name refers to a registered group, and each group gives its rows. The groups that
-- give no rows are dropped, and a separator goes between the rest. See
-- `docs/features/context-menu.md` and `docs/plans/2026-10-25-context-menu-groups.md`.
--
-- The core does the click (it moves the caret, or keeps the selection the click
-- landed in) and fires `MenuPopup`; everything after that is here, on top of
-- `btv.ui.menu`.

btv.contextmenu = btv.contextmenu or {}

-- `btv.contextmenu.groups` is the group registry: a table from a group's name to the
-- group. Change a group in place, or add your own:
--
-- ```lua
-- btv.contextmenu.groups.clipboard = {
--   -- The rows: a list, or a function `(ctx) -> list` that may return a promise.
--   items = {
--     {
--       label = "Copy",
--       action = function(ctx) end,  -- runs when the row is chosen; may return a promise
--       detail = "Ctrl+C",           -- the right-hand hint: a string or `(ctx) -> string|nil`
--       when = function(ctx) end,    -- hide the row unless this returns true
--       enabled = function(ctx) end, -- grey it out unless this returns true
--     },
--   },
--   when = function(ctx) end,        -- hide the whole group unless true (or `false`)
-- }
-- ```
--
-- `when` and `enabled` may also be plain booleans. A field a group or a row doesn't
-- know about is an error naming the group, so a typo doesn't silently drop a row.
btv.contextmenu.groups = btv.contextmenu.groups or {}

-- `btv.contextmenu.default` is the group list an editable buffer starts from, the
-- first time its own list is asked for. Editing it changes what later buffers start
-- with; a buffer that already has its list keeps it.
btv.contextmenu.default = btv.contextmenu.default or { "undo", "clipboard", "lsp", "commands" }

-- `btv.contextmenu.default_readonly` is the group list a read-only buffer (one that
-- isn't `'modifiable'`: help, the quickfix list, a tree) starts from.
btv.contextmenu.default_readonly = btv.contextmenu.default_readonly or { "clipboard" }

-- buf -> that buffer's group list.
local lists = {}

local function copy_list(t)
  local out = {}
  for i, v in ipairs(t) do
    out[i] = v
  end
  return out
end

local function resolve_buf(buf, who)
  if buf == nil or buf == 0 then
    return btv.buf.current()
  end
  if type(buf) ~= "number" or not btv.buf.is_valid(buf) then
    error(who .. ": no buffer " .. tostring(buf), 3)
  end
  return buf
end

local function check_names(names, who)
  if type(names) ~= "table" then
    error(who .. ": the group list must be a list of group names", 3)
  end
  for i, name in ipairs(names) do
    if type(name) ~= "string" then
      error(who .. ": entry " .. i .. " of the group list must be a group name", 3)
    end
  end
end

-- `btv.contextmenu.list(buf)` -> buffer `buf`'s group list (`0` or nil: the current
-- buffer). The list is live: edit it in place to change that buffer's menu, and no
-- other's. The first time it's asked for, it's made from `btv.contextmenu.default`
-- (or `default_readonly` for a buffer that isn't `'modifiable'` at that moment).
--
-- ```lua
-- table.insert(btv.contextmenu.list(buf), 1, "tree")
-- ```
function btv.contextmenu.list(buf)
  buf = resolve_buf(buf, "btv.contextmenu.list")
  local list = lists[buf]
  if not list then
    local from = btv.bo[buf].modifiable and btv.contextmenu.default
      or btv.contextmenu.default_readonly
    check_names(from, "btv.contextmenu.list")
    list = copy_list(from)
    lists[buf] = list
  end
  return list
end

-- `btv.contextmenu.set(buf, names)` replaces buffer `buf`'s group list with a copy
-- of `names` (`0` or nil: the current buffer). `{}` gives the buffer no menu.
function btv.contextmenu.set(buf, names)
  buf = resolve_buf(buf, "btv.contextmenu.set")
  check_names(names, "btv.contextmenu.set")
  lists[buf] = copy_list(names)
end

btv.on("BufDelete", {}, function(a)
  lists[a.buf] = nil
end)

-- How many buffers hold a list: a test seam for the drop above, which nothing else can
-- observe (`list` refuses a deleted buffer).
function btv.contextmenu._lists_count()
  local n = 0
  for _ in pairs(lists) do
    n = n + 1
  end
  return n
end

local SELECTING = { v = true, V = true, ["\22"] = true, s = true, S = true, ["\19"] = true }

-- The `ctx` every `when`, `enabled`, `detail`, `items` and `action` is passed: built
-- once per right-click.
local function context(buf)
  local mode = btv.mode().mode
  return {
    buf = buf,
    win = btv.win.current(),
    mouse = btv.getmousepos(),
    mode = mode,
    selection = SELECTING[mode] or false,
    modifiable = btv.bo[buf].modifiable and true or false,
    modeless = btv.o.modeless and true or false,
  }
end

-- Read a predicate field (`when` / `enabled`): absent is `true`, a function is called
-- with `ctx`, anything else is its truth.
local function holds(field, ctx)
  if field == nil then
    return true
  end
  if type(field) == "function" then
    return field(ctx) and true or false
  end
  return field and true or false
end

local GROUP_FIELDS = { items = true, when = true }
local ROW_FIELDS = { label = true, action = true, detail = true, when = true, enabled = true }

local function check_fields(t, known, where)
  for k in pairs(t) do
    if not known[k] then
      error(where .. ": unknown field " .. tostring(k), 0)
    end
  end
end

-- A group's rows for this click: a list, a promise of one, or `false` when the group
-- is hidden. Raises for a name with no group, or a group that isn't well-formed.
local function group_items(name, ctx)
  local where = "btv.contextmenu.groups." .. name
  local group = btv.contextmenu.groups[name]
  if group == nil then
    error("btv.contextmenu: no group named " .. name, 0)
  end
  if type(group) ~= "table" then
    error(where .. " must be a table", 0)
  end
  check_fields(group, GROUP_FIELDS, where)
  if not holds(group.when, ctx) then
    return {}
  end
  local items = group.items
  if type(items) == "function" then
    items = items(ctx)
  end
  if items == nil then
    error(where .. ": items must be a list, or a function that returns one", 0)
  end
  return items
end

-- Append group `name`'s applicable rows to `out` as `btv.ui.menu` entries, after a
-- separator when rows came before them.
local function add_rows(name, rows, ctx, out)
  local where = "btv.contextmenu.groups." .. name .. ".items"
  if type(rows) ~= "table" then
    error(where .. " must be a list", 0)
  end
  local mine = {}
  for i, row in ipairs(rows) do
    local at = where .. "[" .. i .. "]"
    if type(row) ~= "table" then
      error(at .. " must be a table", 0)
    end
    check_fields(row, ROW_FIELDS, at)
    if type(row.label) ~= "string" then
      error(at .. " needs a string label", 0)
    end
    if type(row.action) ~= "function" then
      error(at .. " needs an action function", 0)
    end
    if holds(row.when, ctx) then
      local detail = row.detail
      if type(detail) == "function" then
        detail = detail(ctx)
      end
      if detail ~= nil and type(detail) ~= "string" then
        error(at .. ": detail must be a string", 0)
      end
      mine[#mine + 1] = {
        label = row.label,
        detail = detail,
        enabled = holds(row.enabled, ctx),
        action = row.action,
      }
    end
  end
  if #mine > 0 and #out > 0 then
    out[#out + 1] = { separator = true }
  end
  for _, e in ipairs(mine) do
    out[#out + 1] = e
  end
end

-- The entries for `buf`'s menu under `ctx`: a promise, since a group may answer with
-- one. The groups are asked in list order and their rows laid out in list order,
-- whatever order they answer in.
local function collect(buf, ctx)
  return btv.promise.try(function()
    local names = copy_list(btv.contextmenu.list(buf))
    check_names(names, "btv.contextmenu")
    local answers = {}
    for i, name in ipairs(names) do
      answers[i] = group_items(name, ctx)
    end
    return btv.promise.all(answers):next(function(rows)
      local out = {}
      for i, name in ipairs(names) do
        add_rows(name, rows[i], ctx, out)
      end
      return out
    end)
  end)
end

-- `btv.contextmenu.entries(buf)` -> a promise of the rows buffer `buf`'s menu would
-- show right now (`0` or nil: the current buffer), as `btv.ui.menu` entries: each
-- `{ label, detail, enabled, action }`, with `{ separator = true }` between groups.
-- The testing and introspection seam: what `open` would show, without showing it.
function btv.contextmenu.entries(buf)
  buf = resolve_buf(buf, "btv.contextmenu.entries")
  return collect(buf, context(buf))
end

-- `btv.contextmenu.open()` opens the current buffer's menu at the pointer, which is
-- what a right-click does. Returns a promise that settles once the chosen row's
-- action has run (an action that returns a promise is waited for), or once the menu
-- closed with nothing chosen. A menu with no rows doesn't open. Bound to a key with
-- no click behind it, the menu opens at the text cursor.
function btv.contextmenu.open()
  local buf = btv.buf.current()
  local ctx = context(buf)
  return collect(buf, ctx)
    :next(function(entries)
      if #entries == 0 then
        return nil
      end
      return btv.ui.menu(entries, { anchor = "mouse" })
    end)
    :next(function(chosen)
      if chosen then
        return chosen.action(ctx)
      end
    end)
    :next(function() end)
end

-- ----- the default groups ------------------------------------------------------
-- Each works in both editing models: a row runs the modeless verb in a modeless
-- session and the vim keys otherwise.

-- Feed vim keys for a row's verb from whatever mode the click left: from Insert or
-- Replace through `<C-o>`, from Select through `<C-g>` (to Visual), so the keys mean
-- the Normal or Visual command they spell.
local function vim_keys(ctx, keys)
  local mode = ctx.mode
  if mode == "i" or mode == "R" then
    keys = "<C-o>" .. keys
  elseif mode == "s" or mode == "S" or mode == "\19" then
    keys = "<C-g>" .. keys
  end
  btv._feedkeys(keys, false, false)
end

-- A row action: modeless verb `name` in a modeless session, else `vim(ctx)`.
local function verb(name, vim)
  return function(ctx)
    if ctx.modeless then
      return btv.modeless.actions[name]()
    end
    return vim(ctx)
  end
end

-- The key that runs modeless verb `name`, as a hint: only in a modeless editable
-- buffer, where the layout's keys run. Vim's keys are verbs plus motions, not one
-- chord, so a vim session shows none.
local function hint(name)
  return function(ctx)
    if ctx.modeless and ctx.modifiable then
      return btv.modeless.key_hint(name)
    end
    return nil
  end
end

local function modifiable(ctx)
  return ctx.modifiable
end

btv.contextmenu.groups.undo = btv.contextmenu.groups.undo
  or {
    when = modifiable,
    items = {
      {
        label = "Undo",
        detail = hint("undo"),
        action = verb("undo", function()
          btv.cmd("undo")
        end),
      },
      {
        label = "Redo",
        detail = hint("redo"),
        action = verb("redo", function()
          btv.cmd("redo")
        end),
      },
    },
  }

btv.contextmenu.groups.clipboard = btv.contextmenu.groups.clipboard
  or {
    items = {
      {
        label = "Cut",
        detail = hint("cut"),
        -- A modeless Cut takes the line when nothing is selected; vim's `d` needs a
        -- selection to cut.
        when = function(ctx)
          return ctx.modifiable and (ctx.modeless or ctx.selection)
        end,
        action = verb("cut", function(ctx)
          vim_keys(ctx, '"+d')
        end),
      },
      {
        label = "Copy",
        detail = hint("copy"),
        -- Takes the line in an editable buffer when nothing is selected.
        when = function(ctx)
          return ctx.selection or ctx.modifiable
        end,
        action = verb("copy", function(ctx)
          vim_keys(ctx, ctx.selection and '"+y' or '"+yy')
        end),
      },
      {
        label = "Paste",
        detail = hint("paste"),
        when = modifiable,
        action = verb("paste", function(ctx)
          vim_keys(ctx, '"+P')
        end),
      },
      {
        label = "Select All",
        detail = hint("select_all"),
        action = verb("select_all", function()
          btv._feedkeys("<Esc>ggVG", false, false)
        end),
      },
    },
  }

-- Whether a language server attached to `buf` answers `method`.
local function lsp_supports(buf, method)
  for _, c in ipairs(btv.lsp.clients({ bufnr = buf })) do
    if c:supports_method(method) then
      return true
    end
  end
  return false
end

-- An LSP row, greyed out unless an attached server answers `method`.
local function lsp_row(label, method, run)
  return {
    label = label,
    action = function()
      run()
    end,
    enabled = function(ctx)
      return lsp_supports(ctx.buf, method)
    end,
  }
end

btv.contextmenu.groups.lsp = btv.contextmenu.groups.lsp
  or {
    when = function(ctx)
      return #btv.lsp.clients({ bufnr = ctx.buf }) > 0
    end,
    items = {
      lsp_row("Go to Definition", "textDocument/definition", function()
        btv.lsp.definition()
      end),
      lsp_row("Find References", "textDocument/references", function()
        btv.lsp.references()
      end),
      lsp_row("Rename Symbol", "textDocument/rename", function()
        btv.lsp.rename()
      end),
      lsp_row("Format Document", "textDocument/formatting", function()
        btv.lsp.format()
      end),
      lsp_row("Code Actions", "textDocument/codeAction", function()
        btv.lsp.code_action()
      end),
    },
  }

btv.contextmenu.groups.commands = btv.contextmenu.groups.commands
  or {
    items = {
      {
        label = "Command Palette",
        detail = hint("palette"),
        action = function()
          btv.picker.open("commands")
        end,
      },
    },
  }

-- The modes a right-click opens the menu from: not the command line, a pending
-- operator, or a terminal taking keys.
local MENU_MODES = {
  n = true,
  v = true,
  V = true,
  ["\22"] = true,
  s = true,
  S = true,
  ["\19"] = true,
  i = true,
  R = true,
}

-- The right-click. A buffer-local `<RightMouse>` map replaces it for its buffer, and
-- a buffer whose list is empty gets no menu.
btv.on("MenuPopup", {}, function()
  if MENU_MODES[btv.mode().mode] then
    btv.contextmenu.open()
  end
end)
