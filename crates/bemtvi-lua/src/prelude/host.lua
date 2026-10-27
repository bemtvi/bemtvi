-- bemtvi Lua prelude — `btv.host`: the app embedding this editor, and what a config or
-- plugin says to it. An app (a mitsuami app mounting `bemtvi_gui::embed::Editor`) names
-- itself as it starts the editor's session; `btv.host.send` then reaches it as an
-- `EditorEvent::Host`, through a `bemtvi_host` notification the server drains from the
-- `btv._host_send` queue. In the other direction an app needs nothing new: it runs Lua or
-- ex commands in the editor through its handle.
--
-- Loads after btv.lua (needs `btv.on` / `btv.off`) and utils.lua (`btv.utils.debounce`).

btv = btv or {}

btv.host = btv.host or {}

-- `btv.host.name()` -> the id of the app embedding this editor (what it named itself
-- with, `SessionOptions::host`), or `nil` when nothing does: the TUI, `bemtvi-gui`, the
-- browser. It's set before any config runs, so an `init.lua` can branch on it.
function btv.host.name()
  return btv._host_name()
end

-- Why msgpack can't carry `value` (the payload, or a part of it at `where`), or `nil`
-- when it can: a function, a coroutine or userdata would reach the app as `nil`, and a
-- table holding itself has no end.
local function payload_problem(value, where, open)
  local kind = type(value)
  if kind == "table" then
    if open[value] then
      return where .. " holds itself"
    end
    open[value] = true
    for k, v in pairs(value) do
      local key = type(k) == "string" and ("." .. k) or ("[" .. tostring(k) .. "]")
      local problem = payload_problem(k, where .. key .. " (its key)", open)
        or payload_problem(v, where .. key, open)
      if problem then
        return problem
      end
    end
    open[value] = nil
  elseif kind ~= "nil" and kind ~= "boolean" and kind ~= "number" and kind ~= "string" then
    return where .. " is a " .. kind .. ", which a message can't carry"
  end
  return nil
end

-- `btv.host.send(name, payload)`: send the app embedding this editor a message, which it
-- gets as `EditorEvent::Host { name, payload }`. `name` is a non-empty string the app and
-- the config agree on; `payload` is anything msgpack carries: `nil`, booleans, numbers,
-- strings, and tables of them (a table whose keys are exactly `1..n` goes as a list).
-- Anything else in it (a function, a coroutine) raises.
--
-- Raises when no app embeds the editor (`btv.host.name()` is `nil`), so a config meant to
-- run both ways checks that first:
--
-- ```lua
-- if btv.host.name() then
--   btv.keymap.set("n", "<leader>p", function()
--     btv.host.send("preview", { file = btv.buf.name(0) })
--   end)
-- end
-- ```
function btv.host.send(name, payload)
  if type(name) ~= "string" or name == "" then
    error("btv.host.send: name must be a non-empty string", 2)
  end
  if btv._host_name() == nil then
    error("btv.host.send: no app embeds this editor (btv.host.name() is nil)", 2)
  end
  local problem = payload_problem(payload, "payload", {})
  if problem then
    error("btv.host.send: " .. problem, 2)
  end
  btv._host_send(name, payload)
end

-- What stops what `btv.host.forward` set up, to call when it's called again.
local forwarded = {}

-- What each forwardable event subscribes to, and the message it sends; each returns
-- the functions that stop it.
local forwarders = {
  changed = function()
    -- One message per quiet stretch of typing: the last buffer changed wins.
    local send = btv.utils.debounce(function(buf)
      if btv.buf.is_valid(buf) then
        btv.host.send("changed", {
          buf = buf,
          name = btv.buf.name(buf),
          modified = btv.bo[buf].modified == true,
        })
      end
    end, 200)
    local function changed(ev)
      send(ev.buf)
    end
    local ids = { btv.on("TextChanged", changed), btv.on("TextChangedI", changed) }
    return {
      function()
        btv.off(ids[1])
        btv.off(ids[2])
      end,
      -- A change still waiting out the pause isn't sent once the app asked for no more.
      function()
        send:cancel()
      end,
    }
  end,
  wrote = function()
    local id = btv.on("BufWritePost", function(ev)
      btv.host.send("wrote", { buf = ev.buf, name = btv.buf.name(ev.buf) })
    end)
    return {
      function()
        btv.off(id)
      end,
    }
  end,
}

-- `btv.host.forward(events)`: tell the app embedding this editor about edits as they
-- happen, as `btv.host.send` messages. `events` is a list of:
--
-- ```
-- "changed"  a buffer's text changed: { buf, name, modified }, once typing pauses
--            for 200 ms (the last buffer changed wins)
-- "wrote"    a buffer was written: { buf, name }
-- ```
--
-- `name` is the buffer's file (`""` for one with none). The text isn't sent — a large
-- buffer on every keystroke would stall the editor; the app asks for what it needs
-- (`btv.buf.lines(buf, 0, -1)` through its handle). Calling it again replaces what it
-- forwards (`btv.host.forward({})` stops). Raises when no app embeds the editor, or for
-- an event it doesn't know.
function btv.host.forward(events)
  if type(events) ~= "table" then
    error("btv.host.forward: events must be a list of event names", 2)
  end
  if btv._host_name() == nil then
    error("btv.host.forward: no app embeds this editor (btv.host.name() is nil)", 2)
  end
  for _, event in ipairs(events) do
    if forwarders[event] == nil then
      error("btv.host.forward: unknown event " .. tostring(event) .. ' (try "changed", "wrote")', 2)
    end
  end
  for _, stop in ipairs(forwarded) do
    stop()
  end
  forwarded = {}
  for _, event in ipairs(events) do
    for _, stop in ipairs(forwarders[event]()) do
      table.insert(forwarded, stop)
    end
  end
end
