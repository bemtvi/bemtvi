-- Windows file associations (`btv.file_associations`): put bemtvi in Explorer's
-- "Open with" list for the file types it recognizes, with bemtvi's document icon.
--
-- The registry work is native (`btv._file_assoc`, src/fileassoc.rs); this module is the
-- surface around it: the public `register` / `unregister`, the `:RegisterFileTypes` /
-- `:UnregisterFileTypes` commands, and the one-time offer `bemtvi-gui` makes on its
-- first start. Both are armed only by a launch that asks for them
-- (`ServerInit.offer_file_associations`, set by the GUI's launch): the TUI, an app
-- embedding the editor and the test suites never see the offer, and can't register.
--
-- The answer is kept in a small file under `stdpath("state")`: `registered` and the exe
-- the keys name, or `declined`. Either way the offer never shows again. A `registered`
-- answer whose exe is gone (the zip unpacked in a new folder) is renewed at startup, so
-- the keys never point at a program that isn't there; another copy that still exists
-- keeps them.
btv.file_associations = btv.file_associations or {}
local M = btv.file_associations

local lfs = btv.fs_local

-- Overrides for tests: the `HKCU` subkey the keys go under (`nil`: `Software\Classes`)
-- and where the answer is kept (`nil`: `stdpath("state")/file-associations`).
M._classes = nil
M._answer_path = nil

local KEY_MODES = { "n", "modeless" }

btv.hl.define(0, "BtvFileAssocHeader", { fg = "#b4befe", bold = true })
btv.hl.define(0, "BtvFileAssocDim", { fg = "#6c7086" })

local function answer_path()
  return M._answer_path or btv.utils.local_path_join(vim.fn.stdpath("state"), "file-associations")
end

-- Whether this session runs in `bemtvi-gui`, the program the keys name: set by its launch
-- (`M._arm`). The TUI or an app embedding the editor would register a program without
-- the document icon, so they are refused rather than left to point every type at it.
local is_gui = false

local function require_gui()
  if not is_gui then
    -- Naming the program is a courtesy: one that can't be told (no exe path) mustn't
    -- replace this error with its own.
    local ok, exe = pcall(btv._file_assoc, "exe")
    error(
      "only bemtvi-gui registers itself for file types (this session runs in "
        .. (ok and tostring(exe) or "another program")
        .. ")",
      0
    )
  end
end

local function message(err)
  return tostring(type(err) == "table" and err.message or err)
end

-- How often a pending pass is checked, in ms.
local POLL_MS = 10

-- How many failures an error message names before it says how many more there are.
local NAMED_FAILURES = 3

-- A pass's error: `message` for the reader, the rest for a caller that wants to know how
-- far it got. Prints as its message, so an uncaught rejection still says what failed.
local Failure = {
  __tostring = function(e)
    return e.message
  end,
}

local function failure_message(what, r)
  local named = {}
  for i = 1, math.min(#r.failed, NAMED_FAILURES) do
    named[i] = r.failed[i]
  end
  local more = #r.failed - #named
  local list = table.concat(named, "; ")
    .. (more > 0 and string.format(" (and %d more)", more) or "")
  if r.extensions == 0 then
    return string.format("%s no file types: %s", what, list)
  end
  return string.format("%s %d file types, but not all of them: %s", what, r.extensions, list)
end

-- One pass at a time, in the order asked for: a register right after an unregister must
-- not race it on another thread. `tail` settles (never rejects) when the last queued
-- pass is done.
local tail = btv.promise.resolve()

-- Run the native `action` pass off the server's Lua thread, polling between ticks for
-- its outcome. Fulfils with `{ exe, extensions }`; rejects when it couldn't run, or
-- with a `Failure` listing what it couldn't do when it got only part of the way.
local function run(action, what)
  local prev = tail
  local release
  tail = btv.promise.new(function(resolve)
    release = resolve
  end)
  return btv.async(function()
    btv.await(prev)
    local ok, r = pcall(function()
      require_gui()
      local id = btv._file_assoc(action, M._classes)
      return btv.await(btv.wait_for(function()
        return btv._file_assoc("poll", id)
      end, { tries = math.huge, interval = POLL_MS }))
    end)
    release()
    if not ok then
      error(r, 0)
    end
    if r.error then
      error(r.error, 0)
    end
    if #r.failed > 0 then
      error(
        setmetatable({
          message = failure_message(what, r),
          exe = r.exe,
          extensions = r.extensions,
          failed = r.failed,
        }, Failure),
        0
      )
    end
    return { exe = r.exe, extensions = r.extensions }
  end)()
end

-- `btv.file_associations.register()` → a promise of `{ exe = <path>, extensions = <count> }`.
--
-- Adds the running program to Explorer's "Open with" list for every extension bemtvi
-- recognizes (plus `.txt` and `.log`), and for any file Windows counts as text. Files
-- opened with bemtvi get its document icon, including through a type you had already
-- pointed at this program by hand (Explorer's "Browse…"), when that type has no icon of
-- its own. The keys go under
-- `HKEY_CURRENT_USER\Software\Classes`, so no administrator rights are needed and
-- other users are untouched. The registry work runs off the editor's thread, so the
-- editor keeps responding meanwhile.
--
-- Windows doesn't let a program make itself the default: pick bemtvi once per type in
-- "Open with" (`Choose another app`, then `Always`) and it is. Rejects outside
-- `bemtvi-gui` or off Windows. A type whose keys can't be written doesn't stop the
-- others: the promise then rejects with an error whose `message` names what failed,
-- and whose `exe`, `extensions` (how many types did register) and `failed` (one line
-- per failure) say how far it got.
function M.register()
  return run("register", 'bemtvi is in "Open with" for')
end

-- `btv.file_associations.unregister()` → a promise of `{ exe = <path>, extensions = <count> }`.
--
-- Removes what `register()` added, including the icon it gave a type of yours (the type
-- itself stays). A type you made bemtvi's default through its "Open with" entry loses
-- that default with it, and Windows asks again which program opens it; one you pointed
-- at the program file itself (Explorer's "Browse…") keeps opening in bemtvi.
-- Rejects outside `bemtvi-gui` or off Windows, and like `register()` when some keys
-- can't be removed.
function M.unregister()
  return run("unregister", 'bemtvi is out of "Open with" for')
end

-- Record the user's answer (`"registered"` with the exe, or `"declined"`).
local function save_answer(answer, exe)
  return btv.async(function()
    local path = answer_path()
    btv.await(lfs.mkdir(btv.utils.dirname(path), { recursive = true }))
    btv.await(lfs.write(path, answer .. "\n" .. (exe or "") .. "\n"))
  end)()
end

-- The saved answer, or `nil` when there is none: `{ answer = …, exe = … }`.
local function read_answer()
  return btv.async(function()
    local path = answer_path()
    if not btv.await(lfs.exists(path)) then
      return nil
    end
    -- One that can't be read as text (garbled, or a directory) is no answer either,
    -- rather than an error every start that never offers again.
    local ok, text = pcall(btv.await, lfs.read_text(path))
    if not ok then
      return nil
    end
    local answer, exe = text:match("^([^\r\n]*)\r?\n?([^\r\n]*)")
    -- A file that holds neither answer (emptied, cut short) is no answer: offer again.
    if answer ~= "registered" and answer ~= "declined" then
      return nil
    end
    return { answer = answer, exe = exe }
  end)()
end

local function registered_notice(r)
  return string.format(
    'bemtvi is in "Open with" for %d file types. To make it a type\'s default, '
      .. "pick it once there (Choose another app, then Always).",
    r.extensions
  )
end

-- Whether a rejected pass still got some of the way: a `Failure` with types done.
local function partly_done(e)
  return getmetatable(e) == Failure and e.extensions > 0
end

-- Register and record it, reporting either way on the message line. The answer is
-- `registered` with the exe whenever any type registered, even when others failed (the
-- error names them, and `:RegisterFileTypes` tries again), so a key Windows won't let
-- bemtvi write doesn't bring the offer back every start. When nothing registered, the
-- offer (`from_offer`) records `declined`, for the same reason; the command leaves the
-- answer as it was.
local function register_and_save(from_offer)
  return M.register()
    :next(function(r)
      btv.notify(registered_notice(r), vim.log.levels.INFO)
      return save_answer("registered", r.exe)
    end, function(e)
      btv.notify("file associations: " .. message(e), vim.log.levels.ERROR)
      if partly_done(e) then
        return save_answer("registered", e.exe)
      elseif from_offer then
        return save_answer("declined")
      end
    end)
    :catch(function(e)
      btv.notify("file associations: can't save the answer: " .. message(e), vim.log.levels.ERROR)
    end)
end

-- The one-time offer: a float with one decision, register or not.
local Offer = btv.view.component({
  setup = function(ctx, props)
    -- Answer exactly once. Closing the float any other way (`:q`, `<C-w>c`) is "not
    -- now", like `<Esc>`, so the offer never hangs unanswered.
    local done = false
    local function answer(yes)
      return function()
        local already = done
        done = true
        ctx.close()
        if not already then
          props.on_done(yes)
        end
      end
    end
    ctx.on_close(function()
      if not done then
        done = true
        props.on_done(false)
      end
    end)
    for _, key in ipairs({ "<CR>", "y" }) do
      ctx.keymap_set(KEY_MODES, key, answer(true), { desc = "Register bemtvi for file types" })
    end
    for _, key in ipairs({ "<Esc>", "q", "n" }) do
      ctx.keymap_set(KEY_MODES, key, answer(false), { desc = "Don't register" })
    end
    btv
      .wait_for(ctx.winid)
      :next(function()
        ctx.wo.wrap = true
        ctx.wo.padding = "1 2"
      end)
      :catch(function() end)
    return {}
  end,

  render = function()
    local lines, decor = {}, {}
    local function add(text, hl)
      lines[#lines + 1] = text
      if hl then
        decor[#decor + 1] =
          { line = #lines - 1, col = 0, end_row = #lines - 1, end_col = #text, hl_group = hl }
      end
    end
    local function action(key, what)
      local text = string.format("  %-7s %s", key, what)
      lines[#lines + 1] = text
      local line = #lines - 1
      decor[#decor + 1] = {
        line = line,
        col = 2,
        end_row = line,
        end_col = 2 + #key,
        hl_group = "BtvFileAssocHeader",
      }
      decor[#decor + 1] = {
        line = line,
        col = 2 + #key,
        end_row = line,
        end_col = #text,
        hl_group = "BtvFileAssocDim",
      }
    end
    add("Open code and text files in bemtvi from Explorer?", "BtvFileAssocHeader")
    add("")
    add('This adds bemtvi to "Open with" for the file types it highlights,', "BtvFileAssocDim")
    add("and gives the files you open with it bemtvi's file icon. Windows", "BtvFileAssocDim")
    add("asks once per type before making bemtvi its default.", "BtvFileAssocDim")
    add("")
    action("<CR>", "Register bemtvi")
    action("<Esc>", "Not now — :RegisterFileTypes does it any time")
    return { lines = lines, decor = decor }
  end,
})

-- `M._offer()` → a promise of the user's answer (`true`: register).
function M._offer()
  return btv.promise.new(function(resolve)
    Offer.mount({
      name = "btv-file-associations",
      filetype = "btvfileassoc",
      float = {
        width = 72,
        height = 11,
        align = "center",
        border = "rounded",
        title = "  Open files with bemtvi  ",
        grab = true,
      },
      props = { on_done = resolve },
    })
  end)
end

-- What startup does: renew a registration whose exe moved, or (no answer yet) make the
-- offer — after the plugin welcome, when that is showing, so the two don't stack.
function M._startup()
  return btv.async(function()
    if vim.fn.has("win32") ~= 1 then
      return
    end
    local saved = btv.await(read_answer())
    if saved then
      local exe = btv._file_assoc("exe")
      -- Renew only a registration whose exe is gone: another copy that still exists (a
      -- dev build, a second install) keeps its keys, and a path spelled in another case
      -- is the same file on Windows.
      if
        saved.answer == "registered"
        and saved.exe:lower() ~= exe:lower()
        and not (saved.exe ~= "" and btv.await(lfs.exists(saved.exe)))
      then
        -- A renewal that got nowhere keeps the old answer, so the next start tries again;
        -- one that got part of the way names this exe, which its keys now do.
        local ok, r = pcall(btv.await, M.register())
        if not ok then
          btv.notify("file associations: " .. message(r), vim.log.levels.ERROR)
          if partly_done(r) then
            btv.await(save_answer("registered", r.exe))
          end
          return
        end
        btv.await(save_answer("registered", r.exe))
        btv.notify("file associations now open files with " .. r.exe, vim.log.levels.INFO)
      end
      return
    end
    local welcome = btv.plugins and btv.plugins._bootstrapping
    if welcome then
      pcall(btv.await, welcome)
      -- Accepting the welcome opens the plugin manager (a grabbing float too) and runs
      -- the install there; the welcome's promise settles without waiting for it, so wait
      -- for the manager to close as well rather than stack on top of it.
      pcall(
        btv.await,
        btv.wait_for(function()
          local ui = btv.plugins.ui
          local inst = ui and ui._instance
          return not inst or inst._closed
        end, { tries = math.huge, interval = 250 })
      )
      -- Mount on the NEXT tick: a grabbing float mounted in the tick the welcome's
      -- closes races its layer teardown, and loses the first key pressed in it.
      btv.await(btv.promise.new(function(resolve)
        btv.on_next_tick(resolve)
      end))
    end
    local yes = btv.await(M._offer())
    if yes then
      btv.await(register_and_save(true))
    else
      btv.await(save_answer("declined"))
    end
  end)()
end

-- Mark this session as `bemtvi-gui`'s, which lets it register, and arm the offer: it
-- runs at the first client attach (`UIEnter`), when there is a window to show it in.
-- Called by the GUI's launch before `init.lua`.
function M._arm()
  if is_gui then
    return
  end
  is_gui = true
  local id
  id = btv.on("UIEnter", function()
    btv.off(id)
    M._startup():catch(function(e)
      btv.notify("file associations: " .. message(e), vim.log.levels.ERROR)
    end)
  end)
end

btv.command("RegisterFileTypes", function()
  register_and_save(false)
end, {
  desc = 'Add bemtvi to Explorer\'s "Open with" for the file types it recognizes, with its file icon (Windows).',
})

-- Records `declined` once the pass removed anything, even part of the way: the user asked
-- to be out, and the offer mustn't come back. One that removed nothing (or couldn't run
-- at all) leaves the answer, so a `registered` one whose keys are all still there keeps
-- them renewed when the program moves.
btv.command("UnregisterFileTypes", function()
  M.unregister()
    :next(function(r)
      btv.notify(
        'bemtvi is out of "Open with" for ' .. r.extensions .. " file types.",
        vim.log.levels.INFO
      )
      return save_answer("declined")
    end, function(e)
      btv.notify("file associations: " .. message(e), vim.log.levels.ERROR)
      if partly_done(e) then
        return save_answer("declined")
      end
    end)
    :catch(function(e)
      btv.notify("file associations: can't save the answer: " .. message(e), vim.log.levels.ERROR)
    end)
end, {
  desc = "Remove what :RegisterFileTypes added to Explorer (Windows).",
})

return M
