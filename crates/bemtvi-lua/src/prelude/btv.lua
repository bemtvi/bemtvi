-- bemtvi Lua prelude — the `btv.*` namespace, bemtvi's own config/plugin API.
--
-- This chunk loads LAST (see PRELUDE_MODULES in runtime.rs). Per ADR 0002 the
-- break is: `btv.*` is the canonical editor API, and the bounded `vim.*` whitelist
-- is *aliases onto it* — the same objects, the same semantics, two names. The
-- variable / option / dispatch / keymap surfaces are now *authored as `btv.*`* in
-- their home prelude chunks (stdlib / timer / nvim_api / keymap, plus `btv.cmd`
-- seeded by the Rust bridge), each setting the matching `vim.*` name to the same
-- object right after. So those nouns are already on `btv` by the time this chunk
-- runs — it does not re-bind them. What lives here is the rest of the config
-- surface a typical `init.lua` targets that has no `vim.*` twin or needs an
-- bemtvi-native shape: event/command registration and the callback-shaped async.

btv = btv or {}

-- Events — structured autocmd subscriptions. `btv.on(event, opts, fn)`: the
-- canonical verb. `fn` (when given) is the handler; otherwise `opts.callback` /
-- `opts.command` apply, exactly as the underlying registry expects. Returns the
-- subscription id (droppable with `btv.off`).
--
-- `btv.on(event, fn)` is the same thing with no options — the spelling to reach for
-- when there is nothing to configure:
--
-- ```lua
-- btv.on("FileType", { pattern = "lua" }, function(ev) … end)  -- with options
-- btv.on("BufWritePost", function(ev) … end)                   -- without
-- ```
function btv.on(event, opts, fn)
  -- The two-argument form, normalized here rather than left to fail downstream. It
  -- has to be accepted, because the failure when it is not is out of all proportion
  -- to the mistake: the handler lands in `opts`, `btv.autocmd.create` raises
  -- `attempt to index a function value` from inside the prelude, and — since a
  -- config is one chunk — every line after the registration silently never runs.
  if type(opts) == "function" and fn == nil then
    opts, fn = {}, opts
  end
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
  return btv.autocmd.create(event, opts)
end

-- Drop a subscription created by `btv.on`.
function btv.off(id)
  return btv.autocmd.del(id)
end

-- User commands — `btv.command(name, fn, opts)` defines `:Name`; `fn` is a
-- function or an ex-command string.
function btv.command(name, fn, opts)
  return btv.user_command.create(name, fn, opts)
end

-- `btv.uuid()` -> a fresh random (version-4) UUID as a canonical 8-4-4-4-12 lowercase-hex
-- string, e.g. `"f47ac10b-58cc-4372-a567-0e02b2c3d479"`. Bytes come from the OS CSPRNG,
-- so each call is unique; handy for a session id, a temp-file name, or any unique key.
-- Available on every build (native and browser/wasm).
function btv.uuid()
  return btv._uuid()
end

-- ----- Rust-backed utilities ------------------------------------------------
-- The utilities below are implemented natively (the btv._* bridges installed by the
-- Rust runtime); these are the thin, documented Lua wrappers that surface them — so
-- the book's API generator, which reads this prelude, lists them. Each forwards
-- verbatim to its bridge. (The sub-namespace tables are seeded up front so every doc
-- comment sits directly above the function it documents.)
btv.layer = {}
btv.terminal = {}
btv.workspace = {}

-- `btv.echo(msg)` -> nil. Append `msg` (a string) to the message line — the programmatic
-- echo, the canonical form of `vim.api.nvim_echo`. For a transient, separately-styled
-- notification prefer `btv.notify`.
function btv.echo(msg)
  return btv._echo(msg)
end

-- `btv.argv()` -> the list of positional file arguments this process was launched with
-- (strings; empty when none). A launcher / wrapper reads them to forward to a
-- relaunched editor; carried through the `BEMTVI_ARGV` environment variable, so the
-- binary stays the single source of truth.
function btv.argv()
  return btv._argv()
end

-- `btv.reexec(args)` -> does not return on success. Replace THIS process with a fresh
-- `bemtvi <args…>` of the current executable — a launcher relaunches the editor with
-- chosen flags this way (e.g. { `"--shada-namespace"`, ns, `"--restore-session"` }). On
-- Unix this `execv()`s (never returns on success); elsewhere it spawns and exits with
-- the child's status. Raises if the exec / spawn itself fails.
function btv.reexec(args)
  return btv._reexec(args)
end

-- `btv.now_ms()` -> a monotonic timestamp in milliseconds (a number) for timing and
-- scheduling math. Unlike `os.clock` (CPU time, ≈0 across an awaited tick) it advances
-- with real wall-clock time, so it measures durations that span async work.
function btv.now_ms()
  return btv._now_ms()
end

-- `btv.runtime_file(name[, all])` -> full paths of runtimepath files matching `name` (a
-- runtimepath-relative path whose final component may be globbed with `*`), as a list.
-- With `all` falsey it returns just the first match (a one- or zero-element list).
-- Reads the LIVE runtimepath, so a plugin installed mid-session contributes its files
-- immediately. The lsp/<server>.lua config-discovery primitive.
function btv.runtime_file(name, all)
  return btv._runtime_file(name, all)
end

-- `btv.open(path[, opts])` -> nil. Open a file (or a directory, which opens the file
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
--   * `where` (`"main"` | nil) — with `where = "main"` the open first crosses to the
--     main editor layer, so an open fired from a dock / sidebar keymap lands in the
--     main area instead of inside the dock. Omitted, it opens in the current window.
--
-- Note: only `reuse` (the default) consults `'switchbuf'`, and `'switchbuf'` only
-- ever redirects to a window already DISPLAYING the buffer — a hidden buffer has no
-- such window, hence the "loaded but hidden" case above.
function btv.open(path, opts)
  if opts and opts.reuse == false then
    return btv._open(path, opts)
  end
  return btv._open_switchbuf(path, opts and opts.where == "main")
end

-- `btv.layer.focus(target)` -> nil. Move keyboard focus across the layout's layers:
-- `target` is `"main"` (the main editing area) or a dock's name.
function btv.layer.focus(target)
  return btv._layer.focus(target)
end

-- `btv.layer.main()` -> nil. Shorthand for `btv.layer.focus("main")` — focus the main
-- editor area.
function btv.layer.main()
  return btv._layer.main()
end

-- `btv.terminal.open([opts])` -> nil. Open a terminal job programmatically — the API twin
-- of `:terminal`. `opts.cmd` is a string (whitespace-split into argv, no shell) or a
-- list (argv verbatim, so an argument may contain spaces); omitted runs the default
-- shell. `opts.cwd` defaults to the editor's working directory.
function btv.terminal.open(opts)
  return btv._terminal.open(opts)
end

-- `btv.workspace.dir()` -> the absolute workspace root (a string), or nil when this
-- session has no workspace. Read-only — bemtvi chooses the workspace, not Lua. Natively
-- that means a `--workspace` launch; for a daemon session it is the daemon's directory.
-- In the browser every session is a workspace (see `btv.workspace.active()`) and this is
-- the session root — the OPFS root serverless, the daemon's directory in a daemon session.
function btv.workspace.dir()
  return btv._workspace.dir()
end

-- `btv.workspace.active()` -> true when this session is workspace-scoped, false otherwise.
-- Natively that means a `--workspace` directory launch. In the browser it is always true:
-- the page's ORIGIN is the workspace, since the shada blob lives in that origin's OPFS and
-- there is exactly one session per origin — which is what `--workspace` names natively. So
-- a plugin keying per-workspace state off this gets a real workspace in every build rather
-- than silently skipping persistence in a browser.
function btv.workspace.active()
  return btv._workspace.active()
end

-- Dock-scoped options (the dock scope, alongside btv.bo/btv.wo/btv.o). Set via
-- `btv.dock.opt(side).<name> = <value>` or inline in `btv.dock.open{...}`; read back
-- through the same proxy. `btv._dock_opts` is a write-through cache keyed by side,
-- and `btv.dock._set_opt` (Rust) queues the change to the core. Known options:
-- `showtabline` (0/1/2), `laststatus` (0/1/2/3 — the per-dock statusline override),
-- `size`, `title`, `winhighlight`, `autohide` (collapse the dock when focus leaves).
btv._dock_opts = btv._dock_opts or {}
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
    return btv.notify("btv.dock.opt: unknown option '" .. tostring(name) .. "'", 4)
  end
  btv._dock_opts[side] = btv._dock_opts[side] or {}
  btv._dock_opts[side][name] = value
  btv.dock._set_opt(side, name, value)
end

-- `btv.dock.opt(side)` — an options proxy for one dock, mirroring `btv.wo`/`btv.bo`:
-- reads return the cached value (or the default), writes queue the change.
btv.dock.opt = function(side)
  return setmetatable({}, {
    __index = function(_, k)
      local cached = btv._dock_opts[side]
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

-- Wrap `btv.dock.open` so it accepts the dock options inline (`showtabline`,
-- `title`, `winhighlight`) alongside `side`/`size`/`buf`, applying them through the
-- same path so the read cache stays in sync.
local _dock_open_raw = btv.dock.open
btv.dock.open = function(o)
  _dock_open_raw({ side = o.side, size = o.size, buf = o.buf })
  if o.size ~= nil then
    btv._dock_opts[o.side] = btv._dock_opts[o.side] or {}
    btv._dock_opts[o.side].size = o.size
  end
  for _, name in ipairs({ "showtabline", "laststatus", "title", "winhighlight", "autohide" }) do
    if o[name] ~= nil then
      dock_set_opt(o.side, name, o[name])
    end
  end
end

-- Wrap `btv.panel.open` (the Rust bridge) so its geometry rides the shared
-- `btv._geom` vocabulary like every other surface: `height` accepts cells or a
-- viewport fraction (`"30vh"` / `"50%"`), and `margin` accepts a number / {v,h} /
-- {t,r,b,l} / {top=, …} — all normalized to the wire shape the bridge expects
-- (a height string, a `[top, right, bottom, left]` margin array). The panel stays
-- bottom-anchored; `margin` is a gap from the screen edges (top is ignored).
local _panel_open_raw = btv.panel.open
btv.panel.open = function(opts)
  opts = opts or {}
  local o = {}
  for k, v in pairs(opts) do
    o[k] = v
  end
  o.height = btv._geom.size(opts.height)
  o.margin = btv._geom.margin(opts.margin)
  return _panel_open_raw(o)
end

-- Dock ex-commands — thin wrappers over the Rust-backed `btv.dock.*` surface
-- (installed before the prelude), dogfooding the btv API. `:DockOpen {side} [size]`
-- opens/focuses a permanent edge panel; `:DockClose`/`:DockFocus {side}` address it.
-- Each carries a `desc`, so it appears in the `:`-completion wildmenu with helpful
-- docs (the user-command merge surfaces `desc` exactly like a built-in's synopsis).
btv.command("DockOpen", function(o)
  local side = o.fargs[1]
  if not side then
    return btv.notify("usage: :DockOpen {left|right|top|bottom} [size]", 4)
  end
  btv.dock.open({ side = side, size = tonumber(o.fargs[2]) })
end, { desc = "Open or focus an edge dock — :DockOpen {left|right|top|bottom} [size]." })
btv.command("DockClose", function(o)
  if o.fargs[1] then
    btv.dock.close(o.fargs[1])
  end
end, { desc = "Close the dock on {side}, discarding its window and content." })
btv.command("DockFocus", function(o)
  if o.fargs[1] then
    btv.dock.focus(o.fargs[1])
  end
end, { desc = "Move focus to the dock on {side}." })
-- `:DockToggle`/`:DockHide`/`:DockShow {side}` — collapse a dock from view (keeping
-- its content) and bring it back, distinct from `:DockClose` (which drops it).
btv.command("DockToggle", function(o)
  if o.fargs[1] then
    btv.dock.toggle(o.fargs[1])
  end
end, { desc = "Toggle the dock on {side} — hide it if shown, show it if hidden." })
btv.command("DockHide", function(o)
  if o.fargs[1] then
    btv.dock.hide(o.fargs[1])
  end
end, { desc = "Hide the dock on {side} from view, keeping its content for :DockShow." })
btv.command("DockShow", function(o)
  if o.fargs[1] then
    btv.dock.show(o.fargs[1])
  end
end, { desc = "Re-show a dock on {side} that was hidden with :DockHide." })

-- Restore the cursor to its last position when a file is reopened — the editor
-- equivalent of neovim's common `BufReadPost` recipe. Opt in with
-- `btv.o.restorecursor = true` (`vim.o.restorecursor = true`); off by default, so
-- the out-of-the-box behavior matches vim/neovim (open at the top unless the user
-- asks otherwise). The `"` mark is the last-cursor position shada persists per
-- file; ``g`"`` jumps there without touching the jumplist, and is a no-op when
-- there is no saved position (a brand-new file, or restore left off). The mark is
-- already seeded onto the buffer by the time `BufReadPost` fires.
btv.on("BufReadPost", {}, function()
  if btv.o.restorecursor then
    btv.cmd([[normal! g`"]])
  end
end)

-- (`btv.notify` / `btv.schedule` — the callback-shaped async — are authored as
-- `btv.*` in prelude/runtime.lua, with `vim.*` aliased onto them there.)
--
-- Treesitter highlighting is controlled declaratively through buffer options
-- (`btv.bo.filetype` + `btv.bo.ts_highlight`), part of the options surface in
-- prelude/state.lua. The one verb surface is `btv.treesitter.foldexpr`, the
-- foldmethod=expr fold source.
--
-- `btv.treesitter.foldexpr` is the canonical tree-sitter foldexpr, set as a string
-- reference into `'foldexpr'`:
--
--     btv.bo.foldmethod = "expr"
--     btv.bo.foldexpr   = "v:lua.btv.treesitter.foldexpr()"
--
-- bemtvi recognizes that exact reference and computes the folds **natively** (the
-- engine's `folds.scm` over the parse — see crates/bemtvi-core/src/editor/fold.rs),
-- so this function is a marker, never evaluated per line. Calling it directly is a
-- usage error (per-line Lua foldexpr evaluation is Phase 5): fail loud rather than
-- silently return a wrong fold level.
btv.treesitter = btv.treesitter or {}
function btv.treesitter.foldexpr(_lnum)
  error(
    "btv.treesitter.foldexpr is a native marker for 'foldmethod=expr' — set it as the "
      .. "'foldexpr' string ('v:lua.btv.treesitter.foldexpr()'), don't call it; per-line "
      .. "Lua foldexpr evaluation is Phase 5",
    2
  )
end

-- `btv.treesitter.highlight(lang, text)` -> promise of the tree-sitter highlight
-- spans for the off-buffer snippet `text` in language `lang` — the same stateless
-- highlighter (injections included) the picker preview uses, exposed so a plugin can
-- token-colour an arbitrary snippet without opening a buffer (the help window's
-- `>lua` code blocks are the motivating case). Resolves with an array of
-- `{ line = <0-based row>, col_start = <byte>, col_end = <byte>, group = <capture> }`
-- (`col_end` exclusive); the columns are byte offsets within each snippet line, which
-- a caller maps to extmark columns. Resolve-only: a language with no installed grammar
-- (and the wasm serverless build, whose highlighter is JS-side) settles with an empty
-- array, so the caller simply paints nothing.
function btv.treesitter.highlight(lang, text)
  return btv.promise.new(function(resolve)
    local id = btv._next_cb_id()
    btv._cb_fns[id] = function(_err, spans)
      resolve(spans or {})
    end
    btv._bridge(id, function()
      btv._ts_highlight(lang or "", text or "", id)
    end)
  end)
end

-- The shipped framings, applied at the bottom of this section. Ordered per language
-- by how a doc block usually arrives, most likely first — the first framing that
-- parses cleanly wins, so ordering is the whole tuning knob. A framing that never
-- fits is inert (it simply never parses cleanly), so the cost of a speculative entry
-- is one throwaway parse, never a wrong colour.
--
-- A `%s` that follows only indentation on its line puts the fragment in an INDENTED
-- block: every line of it is indented to match, which is what an
-- indentation-sensitive language needs to be framed at all.
--
-- The rust, python, go, javascript, json and lua entries were each checked against
-- the real grammar on the hover shapes their servers actually send; the rest follow
-- the same shapes and cost nothing when they don't fit.
local FRAGMENT_CONTEXTS = {
  rust = {
    "struct __btv {\n%s\n}",
    "fn __btv() {\n%s\n}",
    "trait __btv {\n%s\n}",
    "impl __btv {\n%s\n}",
  },
  -- A python hover is very often a header with no body (`def f(a: int) -> bool`,
  -- `class Foo(Base)`, `if x > 1`): giving it a colon and a `pass` is what makes it
  -- a statement. The middle rungs indent a flush block into a class/function; the
  -- last supplies the `def` a *bare* signature drops (`join(self, x: str) -> str`,
  -- what a pyright method hover is once its `(method)` label is peeled off).
  python = {
    "%s:\n    pass\n",
    "class __btv:\n%s",
    "def __btv():\n%s",
    "class __btv:\n    %s",
    "def __btv():\n    %s",
    "def %s:\n    pass\n",
  },
  -- Go's `source_file` wants a package clause, so every framing carries one.
  go = {
    "package __btv\ntype __btv struct {\n%s\n}",
    "package __btv\ntype __btv interface {\n%s\n}",
    "package __btv\nfunc __btv() {\n%s\n}",
    "package __btv\n%s",
  },
  lua = { "local __btv = {\n%s\n}", "function __btv()\n%s\nend" },
  -- `%s {}` gives a body to the bodyless method signature a hover shows.
  javascript = {
    "class __btv {\n%s {}\n}",
    "class __btv {\n%s\n}",
    "const __btv = {%s}",
    "function __btv() {\n%s\n}",
  },
  typescript = {
    "interface __btv {\n%s\n}",
    "class __btv {\n%s {}\n}",
    "class __btv {\n%s\n}",
    "declare %s",
    "type __btv = %s",
    "function __btv() {\n%s\n}",
  },
  tsx = {
    "interface __btv {\n%s\n}",
    "class __btv {\n%s {}\n}",
    "class __btv {\n%s\n}",
    "declare %s",
    "type __btv = %s",
    "function __btv() {\n%s\n}",
  },
  c = { "struct __btv {\n%s\n};", "void __btv() {\n%s\n}" },
  cpp = { "class __btv {\n%s\n};", "struct __btv {\n%s\n};", "void __btv() {\n%s\n}" },
  java = { "class __btv {\n%s\n}", "class __btv {\nvoid __m() {\n%s\n}\n}" },
  c_sharp = { "class __btv {\n%s\n}", "class __btv {\nvoid __m() {\n%s\n}\n}" },
  kotlin = { "class __btv {\n%s\n}", "fun __btv() {\n%s\n}" },
  swift = { "struct __btv {\n%s\n}", "func __btv() {\n%s\n}" },
  scala = { "object __btv {\n%s\n}", "def __btv = {\n%s\n}" },
  dart = { "class __btv {\n%s\n}", "void __btv() {\n%s\n}" },
  zig = { "const __btv = struct {\n%s\n};", "fn __btv() void {\n%s\n}" },
  ruby = { "class __Btv\n%s\nend", "def __btv\n%s\nend" },
  php = { "<?php\nclass __btv {\n%s\n}", "<?php\nfunction __btv() {\n%s\n}", "<?php\n%s" },
  elixir = { "defmodule __Btv do\n%s\nend", "def __btv do\n%s\nend" },
  -- A declaration (`color: red`) is not a stylesheet; a JSON member is not a document.
  css = { "__btv {\n%s\n}" },
  scss = { "__btv {\n%s\n}" },
  json = { "{%s}", "[%s]" },
}

-- `btv.treesitter.fragment_context(lang, templates)` — teach the **fragment**
-- highlighter how to make sense of a code block that is not a whole program.
--
-- The code blocks inside LSP documentation (hover, completion docs) are usually
-- *fragments*: a struct field, a bare statement, a body-less signature. Parsed as a
-- whole file they land in tree-sitter's error recovery, which doesn't merely
-- under-highlight them — it names constructs that aren't in the text (`Vec` in
-- `field: Vec<String>` comes out `@constructor`). bemtvi's fragment highlighter drops
-- those guesses; a *framing* is how it gets the real structure back instead.
--
-- `templates` is an ordered list of framings, each a string with one `%s` marking
-- where the snippet goes. A snippet that doesn't parse on its own is tried inside
-- each in turn, and the **first framing that parses cleanly** wins — its highlight
-- spans are mapped back onto the snippet's own lines and columns:
--
-- ```lua
-- btv.treesitter.fragment_context("rust", {
--   "struct __btv {\n%s\n}",   -- a field hover: `field: Vec<String>`
--   "fn __btv() {\n%s\n}",     -- a statement or expression
-- })
-- ```
--
-- Only a clean parse is accepted, so a framing that doesn't fit costs one throwaway
-- parse and nothing else — the snippet falls through to the conservative repaint
-- (keywords, strings, numbers, comments, punctuation; no guessed constructs), which
-- is also where an annotation dialect ends up: `lua_ls` writes
-- `function f(t: table)` into a ` ```lua ` fence, and that is not a fragment of any
-- Lua program.
--
-- **Indentation-sensitive languages.** When a template's `%s` follows only
-- whitespace on its line, that whitespace is the block level the fragment sits at,
-- not just an opener the first line continues — so *every* line of the fragment is
-- indented to match:
--
-- ```lua
-- btv.treesitter.fragment_context("python", { "class __btv:\n    %s" })
-- ```
--
-- Without that a multi-line python fragment would be framed as a header, one
-- indented line, and then a dedent — a syntax error rather than a block. The spans
-- come back with the indent taken off every line, so the caller still sees the
-- fragment's own columns.
--
-- A same-line framing works too (`"fn __btv() { return %s }"`) — there the prefix's
-- width comes off the first line only. The wrapped text always ends in a newline,
-- because some grammars (go) treat a missing final terminator as a parse defect.
--
-- **Two shapes the ladder is run for you on.** A hover often carries the server's
-- own display label in front of the code — `pyright` sends
-- `(method) def join(self, x: str) -> str`, `tsserver` `(property) Foo.bar: number`
-- — and that label is what stops an otherwise framable signature from framing. It
-- is peeled off, the ladder runs on the rest, and the label itself is painted as a
-- `comment`. And a block that is a *list* rather than one fragment — `ty` sends
-- every overload of a function as its own signature line — is resolved line by
-- line, each through its own ladder. Both are all-or-nothing: a peel or a split
-- that doesn't end in a clean parse leaves no trace, and the block falls to the
-- repaint whole.
--
-- Calling this replaces the language's list; passing `{}` turns the ladder off for
-- it. bemtvi ships defaults for rust, python, go, lua, javascript, typescript, tsx,
-- c, cpp, java, c_sharp, kotlin, swift, scala, dart, zig, ruby, php, elixir, css,
-- scss and json.
function btv.treesitter.fragment_context(lang, templates)
  btv._ts_fragment_context(lang or "", templates or {})
end

for lang, templates in pairs(FRAGMENT_CONTEXTS) do
  btv.treesitter.fragment_context(lang, templates)
end

-- vim.* muscle-memory alias (ADR 0002 §4 whitelist): neovim's canonical spelling
-- `v:lua.vim.treesitter.foldexpr()`. Same native marker — bemtvi recognizes both
-- the `vim.` and `btv.` references.
vim.treesitter = vim.treesitter or {}
vim.treesitter.foldexpr = btv.treesitter.foldexpr

-- `btv.textobject` — user-defined tree-sitter text objects.
--
-- Bind a full `i`/`a` + object-key sequence to an exact `textobjects.scm` capture,
-- so operators and visual mode can select it. After
-- `btv.textobject.map("il", "@loop.inner")`, `vil` selects inside the enclosing loop
-- (and `dil` deletes it); add `btv.textobject.map("al", "@loop.outer")` for `val`.
--
-- The four built-ins (`f` function, `a` argument, `c` comment, `t` type) need no
-- registration. Use this to add MORE objects — `@loop`, `@call`, `@block`,
-- `@conditional`, `@return`, `@assignment`, … that queries already capture — or to
-- override a built-in key.
--
-- The capture is used **verbatim**, so you pick the convention: bemtvi's own
-- `.inner`/`.outer`, or Helix's `.inside`/`.around` if you drop Helix's
-- `textobjects.scm` on your runtimepath, or any custom capture your query defines. A
-- leading `@` is optional (`"@loop.inner"` and `"loop.inner"` are equivalent).
btv.textobject = btv.textobject or {}

-- `btv.textobject.map(lhs, capture)` binds one sequence; `btv.textobject.map(tbl)`
-- binds many from an `lhs -> capture` table, e.g.
-- `btv.textobject.map({ il = "@loop.inner", al = "@loop.outer" })`.
function btv.textobject.map(lhs, capture)
  if type(lhs) == "table" then
    for k, v in pairs(lhs) do
      btv.textobject.map(k, v)
    end
    return
  end
  assert(
    type(lhs) == "string" and #lhs == 2 and (lhs:sub(1, 1) == "i" or lhs:sub(1, 1) == "a"),
    "btv.textobject.map: lhs must be a 2-char sequence starting with 'i' or 'a' (e.g. 'il', 'af')"
  )
  assert(
    type(capture) == "string" and #capture > 0,
    "btv.textobject.map: capture must be a non-empty string (e.g. '@loop.inner')"
  )
  btv._textobject_map(lhs, capture)
end

-- `btv.textobject.unmap(lhs)` removes a binding; a previously-overridden built-in key
-- reverts to its built-in behavior.
function btv.textobject.unmap(lhs)
  assert(
    type(lhs) == "string" and #lhs == 2,
    "btv.textobject.unmap: lhs must be a 2-char sequence (e.g. 'il')"
  )
  btv._textobject_map(lhs, nil)
end

-- btv.daemon.* — the reconnecting remote-daemon link's connection status, surfaced so a
-- plugin (e.g. a statusline component) can show it. A daemon session runs the editor
-- locally and reaches the remote only through the link; when it drops, the supervisor
-- auto-retries a few times and then parks Disconnected until `:reconnect`. The server
-- pushes the current phase here (and fires `User DaemonStatusChanged`) on every change.
btv.daemon = btv.daemon or {}
-- The current phase, mirrored from the server: `"connected"` | `"reconnecting"` |
-- `"disconnected"`, or nil for a local (non-daemon) session.
btv._daemon_status = nil

-- `btv.daemon.status()` -> `"connected"`|`"reconnecting"`|`"disconnected"`|nil
-- The live daemon connection phase, or nil when this session has no daemon link (local).
-- A statusline component renders connected green / reconnecting yellow / disconnected red,
-- and hides itself on nil.
function btv.daemon.status()
  return btv._daemon_status
end

-- Server-internal: set the phase and fire `User DaemonStatusChanged` so a statusline /
-- plugin re-renders. Called from the run loop's daemon-status arm on every change.
function btv._set_daemon_status(phase)
  btv._daemon_status = phase
  btv.autocmd.exec("User", { pattern = "DaemonStatusChanged" })
end

-- btv.session.* — the client-persistent session swap ("reload window"): tear down the
-- server/VM behind this window and bring up a new one against a different backend, keeping
-- the window alive. A plugin (e.g. a remote connector) calls this from inside the running
-- VM once it has resolved a transport; the client (TUI/GUI) performs the actual reload. See
-- docs/plans/2026-07-05-remote-connectors-and-system-plugins.md → §B.
btv.session = btv.session or {}

-- `btv.session.reconnect(spec)` — swap the current client onto `spec`. `spec` is:
--   {
--     transport = { kind = "spawn", argv = { "ssh", "host", "bemtvi", "--daemon" } }
--                                                              -- structured, no shell
--              or { kind = "spawn", cmd = "ssh host bemtvi --daemon" }  -- `sh -c` line
--              or { kind = "quic",  addr = "bemtvi://host:port/token?cert=…" },
--     config_source = "remote" | "local",   -- optional; default "remote" (§D)
--     keep_buffers  = true | false,         -- optional; default false
--   }
-- `config_source` (§D) picks whose config the swapped session runs, independent of the
-- transport: `"remote"` (default) materializes the daemon's config + plugins locally and
-- keeps shada on the daemon; `"local"` keeps THIS machine's `init.lua` / plugins and the
-- daemon backs only the fs / process / LSP seams (the dev-container shape — local editor
-- settings, the container's toolchain). Either way the client-owned system-plugin tier (§A)
-- is re-seeded, so a connector persists across the swap regardless. `"merged"` (local UI
-- config layered over the remote's project config) is RESERVED but not implemented yet — it
-- fails loud rather than silently picking one side.
--
-- Prefer `argv` (a list run WITHOUT a shell, so nothing can be smuggled through shell
-- metacharacters); `cmd` is the `sh -c` convenience for ssh/docker one-liners (as safe as
-- its origin — this runs in the LOCAL VM, which already has arbitrary execution). The client
-- carries the system-plugin tier (§A) forward across the swap and feeds the command into the
-- reconnecting dialer so a dropped link re-runs it. Fails LOUD on a malformed spec (a bad
-- transport is a bug, not a silent no-op); a provisioning / spawn FAILURE surfaces later and
-- leaves the current session intact (the client resolves fully, then swaps). Returns nothing.
function btv.session.reconnect(spec)
  if type(spec) ~= "table" then
    error("btv.session.reconnect: spec must be a table", 2)
  end
  local t = spec.transport
  if type(t) ~= "table" then
    error("btv.session.reconnect: spec.transport must be a table", 2)
  end
  if t.kind == "spawn" then
    local has_argv = type(t.argv) == "table" and #t.argv > 0
    local has_cmd = type(t.cmd) == "string" and t.cmd ~= ""
    if not has_argv and not has_cmd then
      error(
        'btv.session.reconnect: a "spawn" transport needs a non-empty `argv` list or `cmd` string',
        2
      )
    end
    if type(t.argv) == "table" then
      for _, a in ipairs(t.argv) do
        if type(a) ~= "string" then
          error("btv.session.reconnect: spawn `argv` entries must be strings", 2)
        end
      end
    end
  elseif t.kind == "quic" then
    if type(t.addr) ~= "string" or t.addr == "" then
      error('btv.session.reconnect: a "quic" transport needs a non-empty `addr` string', 2)
    end
  else
    error(
      'btv.session.reconnect: transport.kind must be "spawn" or "quic", got ' .. tostring(t.kind),
      2
    )
  end
  local cs = spec.config_source
  if cs == "merged" then
    -- Reserved (§D): local UI config layered over the remote's project config. Not built
    -- yet — fail loud rather than silently falling back to "remote" or "local".
    error(
      'btv.session.reconnect: config_source "merged" is not implemented yet — use "remote" or "local"',
      2
    )
  elseif cs ~= nil and cs ~= "remote" and cs ~= "local" then
    error(
      'btv.session.reconnect: config_source must be "remote" or "local", got ' .. tostring(cs),
      2
    )
  end
  -- Normalize into the wire form the client parses (only the known fields, defaults filled),
  -- so a stray key in the caller's table never reaches the transport builder.
  btv._session_reconnect({
    transport = { kind = t.kind, argv = t.argv, cmd = t.cmd, addr = t.addr },
    config_source = cs or "remote",
    keep_buffers = spec.keep_buffers == true,
  })
end

return btv
