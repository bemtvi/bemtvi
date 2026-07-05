-- nxvim Lua prelude — the `nx.*` namespace, nxvim's own config/plugin API.
--
-- This chunk loads LAST (see PRELUDE_MODULES in runtime.rs). Per ADR 0002 the
-- break is: `nx.*` is the canonical editor API, and the bounded `vim.*` whitelist
-- is *aliases onto it* — the same objects, the same semantics, two names. The
-- variable / option / dispatch / keymap surfaces are now *authored as `nx.*`* in
-- their home prelude chunks (stdlib / timer / nvim_api / keymap, plus `nx.cmd`
-- seeded by the Rust bridge), each setting the matching `vim.*` name to the same
-- object right after. So those nouns are already on `nx` by the time this chunk
-- runs — it does not re-bind them. What lives here is the rest of the config
-- surface a typical `init.lua` targets that has no `vim.*` twin or needs an
-- nxvim-native shape: event/command registration and the callback-shaped async.

nx = nx or {}

-- Events — structured autocmd subscriptions. `nx.on(event, opts, fn)`: the
-- canonical verb. `fn` (when given) is the handler; otherwise `opts.callback` /
-- `opts.command` apply, exactly as the underlying registry expects. Returns the
-- subscription id (droppable with `nx.off`).
function nx.on(event, opts, fn)
  opts = opts or {}
  if fn ~= nil then
    -- Don't mutate the caller's table; layer the handler on a shallow copy.
    local merged = {}
    for k, v in pairs(opts) do
      merged[k] = v
    end
    merged.callback = fn
    opts = merged
  end
  return nx.autocmd.create(event, opts)
end

-- Drop a subscription created by `nx.on`.
function nx.off(id)
  return nx.autocmd.del(id)
end

-- User commands — `nx.command(name, fn, opts)` defines `:Name`; `fn` is a
-- function or an ex-command string.
function nx.command(name, fn, opts)
  return nx.user_command.create(name, fn, opts)
end

-- nx.uuid() -> a fresh random (version-4) UUID as a canonical 8-4-4-4-12 lowercase-hex
-- string, e.g. "f47ac10b-58cc-4372-a567-0e02b2c3d479". Bytes come from the OS CSPRNG,
-- so each call is unique; handy for a session id, a temp-file name, or any unique key.
-- Available on every build (native and browser/wasm).
function nx.uuid()
  return nx._uuid()
end

-- ----- Rust-backed utilities ------------------------------------------------
-- The utilities below are implemented natively (the nx._* bridges installed by the
-- Rust runtime); these are the thin, documented Lua wrappers that surface them — so
-- the book's API generator, which reads this prelude, lists them. Each forwards
-- verbatim to its bridge. (The sub-namespace tables are seeded up front so every doc
-- comment sits directly above the function it documents.)
nx.layer = {}
nx.terminal = {}
nx.workspace = {}

-- nx.echo(msg) -> nil. Append `msg` (a string) to the message line — the programmatic
-- echo, the canonical form of vim.api.nvim_echo. For a transient, separately-styled
-- notification prefer nx.notify.
function nx.echo(msg)
  return nx._echo(msg)
end

-- nx.argv() -> the list of positional file arguments this process was launched with
-- (strings; empty when none). A launcher / wrapper reads them to forward to a
-- relaunched editor; carried through the NXVIM_ARGV environment variable, so the
-- binary stays the single source of truth.
function nx.argv()
  return nx._argv()
end

-- nx.reexec(args) -> does not return on success. Replace THIS process with a fresh
-- `nxvim <args…>` of the current executable — a launcher relaunches the editor with
-- chosen flags this way (e.g. { "--shada-namespace", ns, "--restore-session" }). On
-- Unix this execv()s (never returns on success); elsewhere it spawns and exits with
-- the child's status. Raises if the exec / spawn itself fails.
function nx.reexec(args)
  return nx._reexec(args)
end

-- nx.now_ms() -> a monotonic timestamp in milliseconds (a number) for timing and
-- scheduling math. Unlike os.clock (CPU time, ≈0 across an awaited tick) it advances
-- with real wall-clock time, so it measures durations that span async work.
function nx.now_ms()
  return nx._now_ms()
end

-- nx.runtime_file(name[, all]) -> full paths of runtimepath files matching `name` (a
-- runtimepath-relative path whose final component may be globbed with `*`), as a list.
-- With `all` falsey it returns just the first match (a one- or zero-element list).
-- Reads the LIVE runtimepath, so a plugin installed mid-session contributes its files
-- immediately. The lsp/<server>.lua config-discovery primitive.
function nx.runtime_file(name, all)
  return nx._runtime_file(name, all)
end

-- nx.open(path[, opts]) -> nil. Open a file (or a directory, which opens the file
-- explorer) in the editing area. `opts` is an optional table:
--
--   * `reuse` (boolean, default true) — "open or jump". How to handle the file when
--     it is already up:
--       * shown in a window → focus that window (across tabs under the default
--         `'switchbuf'` = `usetab`); the file is NOT reloaded and no split is made.
--       * loaded but hidden → show that existing buffer in the current window,
--         preserving its edits and cursor (no re-read, no duplicate buffer).
--       * not open at all → read it fresh into the current window.
--
--     This is what a file explorer, a "go to file", or a jump-to-source wants:
--     click a file that's already on screen and you land on it rather than getting
--     a second copy. Set `reuse = false` for plain `:edit` semantics — always load
--     into the current window even when another window already shows the file, e.g.
--     to deliberately place a buffer into a split you just created.
--   * `where` ("main" | nil) — with `where = "main"` the open first crosses to the
--     main editor layer, so an open fired from a dock / sidebar keymap lands in the
--     main area instead of inside the dock. Omitted, it opens in the current window.
--
-- Note: only `reuse` (the default) consults `'switchbuf'`, and `'switchbuf'` only
-- ever redirects to a window already DISPLAYING the buffer — a hidden buffer has no
-- such window, hence the "loaded but hidden" case above.
function nx.open(path, opts)
  if opts and opts.reuse == false then
    return nx._open(path, opts)
  end
  return nx._open_switchbuf(path, opts and opts.where == "main")
end

-- nx.layer.focus(target) -> nil. Move keyboard focus across the layout's layers:
-- `target` is "main" (the main editing area) or a dock's name.
function nx.layer.focus(target)
  return nx._layer.focus(target)
end

-- nx.layer.main() -> nil. Shorthand for nx.layer.focus("main") — focus the main
-- editor area.
function nx.layer.main()
  return nx._layer.main()
end

-- nx.terminal.open([opts]) -> nil. Open a terminal job programmatically — the API twin
-- of `:terminal`. `opts.cmd` is a string (whitespace-split into argv, no shell) or a
-- list (argv verbatim, so an argument may contain spaces); omitted runs the default
-- shell. `opts.cwd` defaults to the editor's working directory.
function nx.terminal.open(opts)
  return nx._terminal.open(opts)
end

-- nx.workspace.dir() -> the absolute workspace root (a string), or nil when this is not
-- a `--workspace` launch. Read-only — nxvim chooses the workspace from the command
-- line, not from Lua. For a daemon session this is the daemon's directory.
function nx.workspace.dir()
  return nx._workspace.dir()
end

-- nx.workspace.active() -> true if this launch is a `--workspace` directory session,
-- false otherwise.
function nx.workspace.active()
  return nx._workspace.active()
end

-- Dock-scoped options (the dock scope, alongside nx.bo/nx.wo/nx.o). Set via
-- `nx.dock.opt(side).<name> = <value>` or inline in `nx.dock.open{...}`; read back
-- through the same proxy. `nx._dock_opts` is a write-through cache keyed by side,
-- and `nx.dock._set_opt` (Rust) queues the change to the core. Known options:
-- `showtabline` (0/1/2), `laststatus` (0/1/2/3 — the per-dock statusline override),
-- `size`, `title`, `winhighlight`, `autohide` (collapse the dock when focus leaves).
nx._dock_opts = nx._dock_opts or {}
local DOCK_OPT_DEFAULT = {
  showtabline = nil,
  laststatus = nil,
  size = 0,
  title = "",
  winhighlight = "",
  autohide = false,
}
-- Recognized names (a set, since `showtabline`/`laststatus` default to nil and so
-- can't be detected via `DOCK_OPT_DEFAULT[name] == nil`).
local DOCK_OPT_KNOWN = {
  showtabline = true,
  laststatus = true,
  size = true,
  title = true,
  winhighlight = true,
  autohide = true,
}

-- Apply one dock option: write-through the cache, then queue it to the core.
local function dock_set_opt(side, name, value)
  if not DOCK_OPT_KNOWN[name] then
    return nx.notify("nx.dock.opt: unknown option '" .. tostring(name) .. "'", 4)
  end
  nx._dock_opts[side] = nx._dock_opts[side] or {}
  nx._dock_opts[side][name] = value
  nx.dock._set_opt(side, name, value)
end

-- `nx.dock.opt(side)` — an options proxy for one dock, mirroring nx.wo/nx.bo:
-- reads return the cached value (or the default), writes queue the change.
nx.dock.opt = function(side)
  return setmetatable({}, {
    __index = function(_, k)
      local cached = nx._dock_opts[side]
      if cached and cached[k] ~= nil then
        return cached[k]
      end
      return DOCK_OPT_DEFAULT[k]
    end,
    __newindex = function(_, k, v)
      dock_set_opt(side, k, v)
    end,
  })
end

-- Wrap `nx.dock.open` so it accepts the dock options inline (`showtabline`,
-- `title`, `winhighlight`) alongside `side`/`size`/`buf`, applying them through the
-- same path so the read cache stays in sync.
local _dock_open_raw = nx.dock.open
nx.dock.open = function(o)
  _dock_open_raw({ side = o.side, size = o.size, buf = o.buf })
  if o.size ~= nil then
    nx._dock_opts[o.side] = nx._dock_opts[o.side] or {}
    nx._dock_opts[o.side].size = o.size
  end
  for _, name in ipairs({ "showtabline", "laststatus", "title", "winhighlight", "autohide" }) do
    if o[name] ~= nil then
      dock_set_opt(o.side, name, o[name])
    end
  end
end

-- Wrap `nx.panel.open` (the Rust bridge) so its geometry rides the shared
-- `nx._geom` vocabulary like every other surface: `height` accepts cells or a
-- viewport fraction ("30vh" / "50%"), and `margin` accepts a number / {v,h} /
-- {t,r,b,l} / {top=, …} — all normalized to the wire shape the bridge expects
-- (a height string, a `[top, right, bottom, left]` margin array). The panel stays
-- bottom-anchored; `margin` is a gap from the screen edges (top is ignored).
local _panel_open_raw = nx.panel.open
nx.panel.open = function(opts)
  opts = opts or {}
  local o = {}
  for k, v in pairs(opts) do
    o[k] = v
  end
  o.height = nx._geom.size(opts.height)
  o.margin = nx._geom.margin(opts.margin)
  return _panel_open_raw(o)
end

-- Dock ex-commands — thin wrappers over the Rust-backed `nx.dock.*` surface
-- (installed before the prelude), dogfooding the nx API. `:DockOpen {side} [size]`
-- opens/focuses a permanent edge panel; `:DockClose`/`:DockFocus {side}` address it.
-- Each carries a `desc`, so it appears in the `:`-completion wildmenu with helpful
-- docs (the user-command merge surfaces `desc` exactly like a built-in's synopsis).
nx.command("DockOpen", function(o)
  local side = o.fargs[1]
  if not side then
    return nx.notify("usage: :DockOpen {left|right|top|bottom} [size]", 4)
  end
  nx.dock.open({ side = side, size = tonumber(o.fargs[2]) })
end, { desc = "Open or focus an edge dock — :DockOpen {left|right|top|bottom} [size]." })
nx.command("DockClose", function(o)
  if o.fargs[1] then
    nx.dock.close(o.fargs[1])
  end
end, { desc = "Close the dock on {side}, discarding its window and content." })
nx.command("DockFocus", function(o)
  if o.fargs[1] then
    nx.dock.focus(o.fargs[1])
  end
end, { desc = "Move focus to the dock on {side}." })
-- `:DockToggle`/`:DockHide`/`:DockShow {side}` — collapse a dock from view (keeping
-- its content) and bring it back, distinct from `:DockClose` (which drops it).
nx.command("DockToggle", function(o)
  if o.fargs[1] then
    nx.dock.toggle(o.fargs[1])
  end
end, { desc = "Toggle the dock on {side} — hide it if shown, show it if hidden." })
nx.command("DockHide", function(o)
  if o.fargs[1] then
    nx.dock.hide(o.fargs[1])
  end
end, { desc = "Hide the dock on {side} from view, keeping its content for :DockShow." })
nx.command("DockShow", function(o)
  if o.fargs[1] then
    nx.dock.show(o.fargs[1])
  end
end, { desc = "Re-show a dock on {side} that was hidden with :DockHide." })

-- Restore the cursor to its last position when a file is reopened — the editor
-- equivalent of neovim's common `BufReadPost` recipe. Opt in with
-- `nx.o.restorecursor = true` (`vim.o.restorecursor = true`); off by default, so
-- the out-of-the-box behavior matches vim/neovim (open at the top unless the user
-- asks otherwise). The `"` mark is the last-cursor position shada persists per
-- file; ``g`"`` jumps there without touching the jumplist, and is a no-op when
-- there is no saved position (a brand-new file, or restore left off). The mark is
-- already seeded onto the buffer by the time `BufReadPost` fires.
nx.on("BufReadPost", {}, function()
  if nx.o.restorecursor then
    nx.cmd([[normal! g`"]])
  end
end)

-- (`nx.notify` / `nx.schedule` — the callback-shaped async — are authored as
-- `nx.*` in prelude/runtime.lua, with `vim.*` aliased onto them there.)
--
-- Treesitter highlighting is controlled declaratively through buffer options
-- (nx.bo.filetype + nx.bo.ts_highlight), part of the options surface in
-- prelude/state.lua. The one verb surface is `nx.treesitter.foldexpr`, the
-- foldmethod=expr fold source.
--
-- `nx.treesitter.foldexpr` is the canonical tree-sitter foldexpr, set as a string
-- reference into 'foldexpr':
--
--     nx.bo.foldmethod = "expr"
--     nx.bo.foldexpr   = "v:lua.nx.treesitter.foldexpr()"
--
-- nxvim recognizes that exact reference and computes the folds **natively** (the
-- engine's `folds.scm` over the parse — see crates/nxvim-core/src/editor/fold.rs),
-- so this function is a marker, never evaluated per line. Calling it directly is a
-- usage error (per-line Lua foldexpr evaluation is Phase 5): fail loud rather than
-- silently return a wrong fold level.
nx.treesitter = nx.treesitter or {}
function nx.treesitter.foldexpr(_lnum)
  error(
    "nx.treesitter.foldexpr is a native marker for 'foldmethod=expr' — set it as the "
      .. "'foldexpr' string ('v:lua.nx.treesitter.foldexpr()'), don't call it; per-line "
      .. "Lua foldexpr evaluation is Phase 5",
    2
  )
end

-- vim.* muscle-memory alias (ADR 0002 §4 whitelist): neovim's canonical spelling
-- `v:lua.vim.treesitter.foldexpr()`. Same native marker — nxvim recognizes both
-- the `vim.` and `nx.` references.
vim.treesitter = vim.treesitter or {}
vim.treesitter.foldexpr = nx.treesitter.foldexpr

-- nx.daemon.* — the reconnecting remote-daemon link's connection status, surfaced so a
-- plugin (e.g. a statusline component) can show it. A daemon session runs the editor
-- locally and reaches the remote only through the link; when it drops, the supervisor
-- auto-retries a few times and then parks Disconnected until `:reconnect`. The server
-- pushes the current phase here (and fires `User DaemonStatusChanged`) on every change.
nx.daemon = nx.daemon or {}
-- The current phase, mirrored from the server: "connected" | "reconnecting" |
-- "disconnected", or nil for a local (non-daemon) session.
nx._daemon_status = nil

-- nx.daemon.status() -> "connected"|"reconnecting"|"disconnected"|nil
-- The live daemon connection phase, or nil when this session has no daemon link (local).
-- A statusline component renders connected green / reconnecting yellow / disconnected red,
-- and hides itself on nil.
function nx.daemon.status()
  return nx._daemon_status
end

-- Server-internal: set the phase and fire `User DaemonStatusChanged` so a statusline /
-- plugin re-renders. Called from the run loop's daemon-status arm on every change.
function nx._set_daemon_status(phase)
  nx._daemon_status = phase
  nx.autocmd.exec("User", { pattern = "DaemonStatusChanged" })
end

return nx
