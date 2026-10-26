-- bemtvi Lua prelude — the modeless key layout (opt-in, GUI and web only).
--
-- `'modeless'` makes a modifiable buffer rest in Insert, the way a GUI editor
-- edits: see `docs/plans/2026-10-22-modeless-editing.md`. The core does the editing
-- model: the resting mode, Shift-motion selection, typing over a selection, and the
-- named verbs behind `btv._modeless_action` -> `Editor::apply_modeless_action`. This
-- module is the KEYS: the familiar Ctrl chords, bound in the `modeless` keymap
-- layer. The server matches that layer only while a modeless session is typing or
-- selecting, laid over the Insert maps, so a layout key wins there and every other
-- key still reaches its Insert map (completion, snippets, your own). A vim session
-- never sees these maps.
--
-- Every default is `default = true`, so `btv.keymap.set("modeless", lhs, rhs)` wins
-- over it (bind an empty function to free a key).

btv.modeless = btv.modeless or {}

-- ----- named actions ---------------------------------------------------------
-- `btv.modeless.actions.<name>()` runs a named verb. The core verbs (`copy`, `cut`,
-- `paste`, `undo`, `redo`, `select_all`, `delete_word_back`, `delete_word_forward`,
-- `indent`, `outdent`, `duplicate_line`, `move_line_up`, `move_line_down`,
-- `search_next`, `search_prev`) each run as one undo step, and `reveal_caret` brings a
-- view the wheel scrolled away back to the caret (what a client asks for when an input
-- method starts composing there). The rest compose existing
-- commands: `save`, `find`, `replace`, `command_line`, `palette`, `files`, `close`,
-- `next_buffer`, `prev_buffer`. Bind them with
-- `btv.keymap.set("modeless", "<key>", btv.modeless.actions.<name>)`.
btv.modeless.actions = btv.modeless.actions or {}
for _, name in ipairs({
  "copy",
  "cut",
  "paste",
  "undo",
  "redo",
  "select_all",
  "delete_word_back",
  "delete_word_forward",
  "indent",
  "outdent",
  "duplicate_line",
  "move_line_up",
  "move_line_down",
  "search_next",
  "search_prev",
  "reveal_caret",
}) do
  btv.modeless.actions[name] = function()
    btv._modeless_action(name)
  end
end

local actions = btv.modeless.actions
actions.save = function()
  btv.cmd("write")
end
actions.find = function()
  btv.cmdline.open("", { kind = "/" })
end
actions.replace = function()
  btv.cmdline.open("%s/")
end
actions.command_line = function()
  btv.cmdline.open("")
end
actions.palette = function()
  btv.picker.open("commands")
end
actions.files = function()
  btv.picker.open("files")
end
actions.close = function()
  btv.cmd("bdelete")
end
actions.next_buffer = function()
  btv.cmd("bnext")
end
actions.prev_buffer = function()
  btv.cmd("bprevious")
end

-- `btv.modeless.enable()` turns the session's `'modeless'` option on, the same as
-- `btv.o.modeless = true`. It fails loud under the terminal client and inside a
-- Helix session.
function btv.modeless.enable()
  btv.o.modeless = true
end

-- `btv.modeless.disable()` turns `'modeless'` off, the same as
-- `btv.o.modeless = false`: the session leaves Insert the ordinary way and goes back
-- to the vim model.
function btv.modeless.disable()
  btv.o.modeless = false
end

-- ----- the default layout ----------------------------------------------------
-- The chords a GUI editor uses: Control on Linux and Windows, Command on macOS
-- (`btv.ui.caps().platform == "macos"`), each row spelled both ways. The layout goes
-- in from `UIEnter`, once per platform, since only the attached client knows which
-- one it is; no key reaches the editor before a client attaches.
--
-- In a browser on Windows and Linux, Chrome keeps `<C-w>` and `<C-Tab>` for itself;
-- the web client sends `<C-w>` for Alt-W instead (see `btv.ui.caps().key_labels`).
-- On a Mac the browser keeps Command-W.
local LAYOUT = {
  -- { Control spelling, Command spelling, action, description }
  { "<C-z>", "<D-z>", "undo", "Undo" },
  { "<C-S-z>", "<D-S-z>", "redo", "Redo" },
  { "<C-y>", nil, "redo", "Redo" },
  { "<C-c>", "<D-c>", "copy", "Copy" },
  { "<C-x>", "<D-x>", "cut", "Cut" },
  { "<C-v>", "<D-v>", "paste", "Paste" },
  { "<C-a>", "<D-a>", "select_all", "Select all" },
  { "<C-s>", "<D-s>", "save", "Save" },
  { "<C-f>", "<D-f>", "find", "Find" },
  { "<C-g>", "<D-g>", "search_next", "Find next" },
  { "<C-S-g>", "<D-S-g>", "search_prev", "Find previous" },
  { "<C-p>", "<D-p>", "files", "Go to file" },
  { "<C-S-p>", "<D-S-p>", "palette", "Command palette" },
  { "<C-;>", "<D-;>", "command_line", "Command line" },
  { "<C-w>", "<D-w>", "close", "Close buffer" },
  { "<C-Tab>", "<C-Tab>", "next_buffer", "Next buffer" },
  { "<C-S-Tab>", "<C-S-Tab>", "prev_buffer", "Previous buffer" },
  { "<C-BS>", "<A-BS>", "delete_word_back", "Delete word before" },
  { "<C-Del>", "<A-Del>", "delete_word_forward", "Delete word after" },
  { "<C-]>", "<D-]>", "indent", "Indent" },
  { "<C-S-d>", "<D-S-d>", "duplicate_line", "Duplicate line" },
  { "<A-Up>", "<A-Up>", "move_line_up", "Move line up" },
  { "<A-Down>", "<A-Down>", "move_line_down", "Move line down" },
}

local function set(lhs, action, desc)
  btv.keymap.set("modeless", lhs, actions[action], { default = true, desc = desc })
end

-- Which layouts are in, so a re-attach doesn't register a second copy.
local installed = {}

-- Replace is `<C-h>` off a Mac, which needs the keyboard protocol: without it the
-- client can't tell `<C-h>` from `<BS>`, and the map would take over backspace. A GUI
-- and the browser always have it. On a Mac it is Option-Command-F. (Outdent has no
-- Control chord for the same reason, `<C-[>` being `<Esc>`; `<S-Tab>` outdents.)
local function install()
  local caps = btv.ui.caps()
  local mac = caps.platform == "macos"
  local which = mac and "mac" or "pc"
  if not installed[which] then
    installed[which] = true
    for _, row in ipairs(LAYOUT) do
      local lhs = mac and row[2] or row[1]
      if lhs then
        set(lhs, row[3], row[4])
      end
    end
    if mac then
      set("<A-D-f>", "replace", "Replace")
    end
  end
  if not mac and caps.keyboard_protocol and not installed.ctrl_h then
    installed.ctrl_h = true
    set("<C-h>", "replace", "Replace")
  end
end
btv.on("UIEnter", {}, install)

-- ----- key hints -------------------------------------------------------------
-- How a layout key reads in a menu, for the context menu's hints.

-- The display names of the special keys a layout chord can end in.
local KEY_NAMES = {
  BS = "Backspace",
  Del = "Delete",
  CR = "Enter",
  Esc = "Escape",
  Space = "Space",
}

-- A chord's modifier letters, either case, to the one each stands for: `M` (meta) is
-- `A` (alt), as in key notation.
local MODIFIERS = { C = "C", S = "S", A = "A", M = "A", D = "D" }

-- How a key chord reads in a menu, the platform's way: `<C-S-z>` is `Ctrl+Shift+Z`,
-- or `⇧⌘Z` on macOS. `nil` for anything that isn't a single chord.
local function chord_label(lhs, mac)
  local inner = lhs:match("^<(.+)>$")
  if not inner or inner:find("[<>]") then
    return nil
  end
  local has = {}
  local key = inner
  while true do
    local m, rest = key:match("^(%a)%-(.+)$")
    if not m then
      break
    end
    local mod = MODIFIERS[m:upper()]
    if not mod then
      return nil
    end
    has[mod] = true
    key = rest
  end
  key = KEY_NAMES[key] or (#key == 1 and key:upper()) or key
  if mac then
    return (has.C and "⌃" or "")
      .. (has.A and "⌥" or "")
      .. (has.S and "⇧" or "")
      .. (has.D and "⌘" or "")
      .. key
  end
  local parts = {}
  for _, m in ipairs({ { "C", "Ctrl" }, { "A", "Alt" }, { "S", "Shift" }, { "D", "Super" } }) do
    if has[m[1]] then
      parts[#parts + 1] = m[2]
    end
  end
  parts[#parts + 1] = key
  return table.concat(parts, "+")
end

-- `btv.modeless.key_hint(action)` -> the key that runs `action` in the modeless
-- layout, written the platform's way (`Ctrl+Shift+Z`, or `⇧⌘Z` on macOS), or `nil`
-- when no single key runs it. `action` is a `btv.modeless.actions` name or a
-- function. The key is read from the live `modeless` keymap, so a rebound key shows
-- its new chord: a map you set beats a default on the same key, and where the client
-- stands in another chord for an unreachable one (`btv.ui.caps().key_labels`), the
-- one shown is the chord you press. The context menu's rows use it for their hints
-- (see `btv.contextmenu`).
function btv.modeless.key_hint(action)
  local fn = action
  if type(action) == "string" then
    fn = actions[action]
    if not fn then
      error("btv.modeless.key_hint: no action named " .. action, 2)
    end
  elseif type(action) ~= "function" then
    error("btv.modeless.key_hint: action must be an action name or a function", 2)
  end
  local order, winner = {}, {}
  for _, e in ipairs(btv._keymaps) do
    local layer = false
    for _, m in ipairs(e.modes) do
      layer = layer or m == "modeless"
    end
    if layer and (e.buffer == nil or e.buffer == 0) then
      local prev = winner[e.lhs]
      if not prev then
        order[#order + 1] = e.lhs
      end
      if not prev or prev.default or not e.default then
        winner[e.lhs] = e
      end
    end
  end
  local caps = btv.ui.caps()
  for _, lhs in ipairs(order) do
    local e = winner[lhs]
    if e.rhs.kind == "lua" and btv._keymap_fns[e.id] == fn then
      return chord_label(caps.key_labels[lhs] or lhs, caps.platform == "macos")
    end
  end
  return nil
end
