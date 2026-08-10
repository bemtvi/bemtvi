-- nx.cmdline_complete: command-line completion — the unified float-list widget's
-- **fifth orchestration** (docs/specs/2026-06-14-nx-ui-float-widget.md;
-- docs/plans/2026-06-16-cmdline-completion.md). Pressing `<Tab>` on an ex command
-- line (`:`) offers a fuzzy list of matching command names with a synopsis/help
-- preview pane. The candidate set is **policy** owned here — the curated built-in
-- catalog merged with `nx.user_command.get()`, so a plugin command appears exactly
-- as a built-in does — and core stays out of "what commands exist": it extracts the
-- token, fuzzy-ranks + renders the menu, and applies the accept.
--
-- Phase 1 shipped command NAMES; Phase 3 added the docs pane (each entry carries a
-- synopsis + one-line description); Phase 4 folded in the user-command merge.
-- `:set <opt>` completes option names inline; likewise `:buffer <name>` (buffers),
-- `:colorscheme <name>` (schemes), and `:highlight <group>` (highlight groups).
--
-- A **file argument** (`:e <Tab>`, `:split`, `:cd`, …) is different: instead of the
-- inline wildmenu it hands off to the full `nx.picker` overlay (a fuzzy finder with
-- a file preview pane). The source seeds the picker with the path typed so far,
-- lists one directory at a time with the **same-level** entries ranked first, and
-- on confirm splices the chosen path back into the command and runs it (`nx.cmd`),
-- so every file-taking command works — modifiers and all. See
-- `docs/plans/2026-06-26-cmdline-file-path-picker.md`.

nx.cmdline_complete = nx.cmdline_complete or {}

-- The curated built-in ex-command catalog: `{ name, synopsis, description }` per
-- command. Core fuzzy-ranks the typed prefix against `name` (abbreviations match
-- implicitly — fuzzy ranking favours the prefix, so `:e` leads with `edit`); the
-- docs pane shows `synopsis` + `description` for the highlighted row. The registered
-- user commands (`nx.user_command.get()`) are appended in `_cmdline_complete_run`.
--
-- MAINTENANCE: this list is curated by hand, so it must be kept in sync with the two
-- dispatch tables — core ex-commands in `editor/ex.rs` and the server-level commands
-- (`:Lsp*`, `:TS*`, `:cd`, `:grep`, `:make`, `:source`, `:colorscheme`, `:autocmd`,
-- `:command`, …) in `nxvim-server/src/excmd.rs`. When you add a user-facing ex-command
-- there, add it here too. The `catalog_commands_are_recognized` test guards the
-- forward direction (every entry below is a real command); the reverse — a new
-- command missing from this list — is not auto-detected, hence this note. A few rarely
-- typed quickfix-population variants (`:cgetfile`, `:caddfile`, `:cnfile`, `:cpfile`
-- and their `l`-twins) are intentionally omitted.
--
-- Argument notation follows vim help and is kept **consistent between the synopsis
-- and the description**: `{arg}` is required, `[arg]` is optional — a given command
-- spells its argument the same way in both.
local BUILTIN = {
  { "write", ":write [file]", "Write the buffer to its file (or to [file])." },
  { "quit", ":quit", "Close the current window; quit when it is the last." },
  { "wq", ":wq [file]", "Write the buffer (to [file]), then close the window." },
  { "xit", ":xit [file]", "Write the buffer if changed (to [file]), then close it." },
  { "quitall", ":quitall", "Quit nxvim, failing if any buffer has unsaved changes." },
  { "wall", ":wall", "Write every changed buffer." },
  { "wqall", ":wqall", "Write every changed buffer, then quit." },
  { "edit", ":edit [file]", "Edit [file], or reload the current buffer from disk." },
  { "enew", ":enew", "Open a new, empty, unnamed buffer in this window." },
  { "split", ":split [file]", "Split the window horizontally; optionally edit [file]." },
  { "vsplit", ":vsplit [file]", "Split the window vertically; optionally edit [file]." },
  { "new", ":new", "Horizontally split and edit a new empty buffer." },
  { "vnew", ":vnew", "Vertically split and edit a new empty buffer." },
  { "close", ":close", "Close the current window (keep the buffer loaded)." },
  { "hide", ":hide", "Close the current window without unloading the buffer." },
  { "only", ":only", "Close every window except the current one." },
  { "terminal", ":terminal [cmd]", "Open a terminal buffer running [cmd] (or the shell)." },
  { "tabnew", ":tabnew [file]", "Open a new tab page; optionally edit [file]." },
  { "tabclose", ":tabclose", "Close the current tab page." },
  { "tabonly", ":tabonly", "Close every tab page except the current one." },
  { "tabmove", ":tabmove [n]", "Move the current tab page to position [n]." },
  { "tabnext", ":tabnext [n]", "Go to the next tab page (or tab [n])." },
  { "tabprevious", ":tabprevious", "Go to the previous tab page." },
  { "tabfirst", ":tabfirst", "Go to the first tab page." },
  { "tablast", ":tablast", "Go to the last tab page." },
  { "drop", ":drop {file}", "Edit {file}, reusing a window that already shows it." },
  { "resize", ":resize [n]", "Set the current window's height to [n] rows." },
  { "vertical", ":vertical {cmd}", "Run {cmd} with a vertical split where it splits." },
  { "checktime", ":checktime", "Reload buffers changed on disk outside nxvim." },
  { "buffers", ":buffers", "List the loaded buffers (alias :ls)." },
  { "panels", ":panels", "List the named panels (the surfaces hidden from :ls)." },
  { "buffer", ":buffer {n}", "Edit buffer {n} (or by name) in this window." },
  { "bnext", ":bnext", "Go to the next buffer in the buffer list." },
  { "bprevious", ":bprevious", "Go to the previous buffer in the buffer list." },
  { "bfirst", ":bfirst", "Go to the first buffer in the buffer list." },
  { "blast", ":blast", "Go to the last buffer in the buffer list." },
  { "bdelete", ":bdelete [n]", "Unload buffer [n] and remove it from the list." },
  { "copen", ":copen", "Open the quickfix window." },
  { "cclose", ":cclose", "Close the quickfix window." },
  { "cwindow", ":cwindow", "Open the quickfix window only if it has entries." },
  { "cnext", ":cnext", "Jump to the next quickfix entry." },
  { "cprevious", ":cprevious", "Jump to the previous quickfix entry." },
  { "cfirst", ":cfirst", "Jump to the first quickfix entry." },
  { "clast", ":clast", "Jump to the last quickfix entry." },
  { "colder", ":colder", "Go to an older quickfix list." },
  { "cnewer", ":cnewer", "Go to a newer quickfix list." },
  { "lopen", ":lopen", "Open the location-list window." },
  { "lclose", ":lclose", "Close the location-list window." },
  { "lwindow", ":lwindow", "Open the location-list window only if it has entries." },
  { "lnext", ":lnext", "Jump to the next location-list entry." },
  { "lprevious", ":lprevious", "Jump to the previous location-list entry." },
  { "lfirst", ":lfirst", "Jump to the first location-list entry." },
  { "llast", ":llast", "Jump to the last location-list entry." },
  { "lolder", ":lolder", "Go to an older location list." },
  { "lnewer", ":lnewer", "Go to a newer location list." },
  { "lua", ":lua {chunk}", "Execute {chunk} as Lua in the nx.* runtime." },
  { "sleep", ":sleep [n]", "Pause for [n] seconds (or milliseconds with `m`)." },
  { "messages", ":messages [clear]", "Show recorded messages (`clear` empties the log)." },
  { "echo", ":echo {expr}", "Evaluate and display {expr}." },
  { "echomsg", ":echomsg {expr}", "Display {expr} and record it in :messages." },
  { "echoerr", ":echoerr {expr}", "Display {expr} as an error and record it." },
  { "registers", ":registers", "Display the contents of the registers." },
  { "marks", ":marks", "List the marks and their positions." },
  { "jumps", ":jumps", "List the jump list." },
  { "changes", ":changes", "List the change list." },
  { "set", ":set {option}", "Set {option} for all buffers and windows." },
  { "setlocal", ":setlocal {option}", "Set {option} for the current buffer/window only." },
  {
    "setglobal",
    ":setglobal {option}",
    "Set {option}'s global value — what a new buffer is born from — leaving this one.",
  },
  { "setfiletype", ":setfiletype {ft}", "Set 'filetype' to {ft} unless already set." },
  { "undo", ":undo [n]", "Undo changes (or jump to undo state [n])." },
  { "redo", ":redo", "Redo a change that was undone." },
  { "nohlsearch", ":nohlsearch", "Stop highlighting the last search match." },
  { "substitute", ":substitute/{pat}/{sub}/", "Replace matches of {pat} with {sub} on a range." },
  { "global", ":global/{pat}/{cmd}", "Run {cmd} on every line matching {pat}." },
  { "vglobal", ":vglobal/{pat}/{cmd}", "Run {cmd} on every line NOT matching {pat}." },
  { "delete", ":delete [x]", "Delete lines in the range (into register [x])." },
  { "print", ":print", "Print the lines in the range." },
  { "put", ":put [x]", "Insert the contents of register [x] after the line." },
  { "move", ":move {addr}", "Move the lines in the range to below {addr} (`:m0` = to the top)." },
  { "copy", ":copy {addr}", "Copy the lines in the range to below {addr} (also `:t`)." },
  { "highlight", ":highlight {group}", "Define or show highlight group {group}." },
  { "helptags", ":helptags {dir}", "Generate the help tags file for {dir}." },
  -- Working directory / sourcing / colours (server-level commands — `excmd.rs`).
  { "cd", ":cd [dir]", "Change the global working directory to [dir] (or $HOME)." },
  { "tcd", ":tcd [dir]", "Change the tab-local working directory to [dir]." },
  { "lcd", ":lcd [dir]", "Change the window-local working directory to [dir]." },
  { "pwd", ":pwd", "Print the current working directory." },
  { "source", ":source {file}", "Execute the Lua commands in {file}." },
  { "colorscheme", ":colorscheme [name]", "Load colour scheme [name] (or show the current one)." },
  -- Build / search into the quickfix or location list.
  { "make", ":make [args]", "Run 'makeprg' and load the errors into the quickfix list." },
  { "lmake", ":lmake [args]", "Like :make, but load into the location list." },
  { "grep", ":grep {args}", "Run 'grepprg' and load the matches into the quickfix list." },
  { "lgrep", ":lgrep {args}", "Like :grep, but load into the location list." },
  { "vimgrep", ":vimgrep /{pat}/ {file}", "Search files for {pat} into the quickfix list." },
  { "lvimgrep", ":lvimgrep /{pat}/ {file}", "Like :vimgrep, but load into the location list." },
  { "cc", ":cc [nr]", "Jump to quickfix error [nr] (or the current one)." },
  { "ll", ":ll [nr]", "Jump to location-list entry [nr] (or the current one)." },
  { "cfile", ":cfile [file]", "Read errors from [file] into the quickfix list." },
  { "lfile", ":lfile [file]", "Read errors from [file] into the location list." },
  { "cbuffer", ":cbuffer", "Read errors from the current buffer into the quickfix list." },
  { "lbuffer", ":lbuffer", "Read errors from the current buffer into the location list." },
  -- Autocommands / user commands / command modifiers.
  { "autocmd", ":autocmd {event} {pat} {cmd}", "Define an autocommand." },
  { "augroup", ":augroup {name}", "Define or switch to autocommand group {name}." },
  { "doautocmd", ":doautocmd {event}", "Fire the autocommands for {event}." },
  { "command", ":command {name} {repl}", "Define a user command :{name}." },
  {
    "silent",
    ":silent[!] {cmd}",
    "Run {cmd}, suppressing its messages (errors still show; `!` hides those too).",
  },
  { "tab", ":tab {cmd}", "Run {cmd} so a window it opens becomes a new tab page." },
  -- LSP (the `:Lsp*` verbs). Each request verb takes an optional `[server]` — the
  -- config name of the attached client to route to, when a buffer carries several.
  { "LspInfo", ":LspInfo", "Show the LSP clients attached to the buffer." },
  {
    "LspHover",
    ":LspHover [server]",
    "Show hover information for the symbol under the cursor (from [server]).",
  },
  {
    "LspDefinition",
    ":LspDefinition [server]",
    "Jump to the definition of the symbol under the cursor (asking [server]).",
  },
  {
    "LspDeclaration",
    ":LspDeclaration [server]",
    "Jump to the declaration of the symbol under the cursor (asking [server]).",
  },
  {
    "LspTypeDefinition",
    ":LspTypeDefinition [server]",
    "Jump to the type definition of the symbol (asking [server]).",
  },
  {
    "LspImplementation",
    ":LspImplementation [server]",
    "Jump to the implementation of the symbol (asking [server]).",
  },
  {
    "LspReferences",
    ":LspReferences [server]",
    "List references to the symbol under the cursor (from [server] alone).",
  },
  {
    "LspSignatureHelp",
    ":LspSignatureHelp [server]",
    "Show signature help for the call under the cursor (from [server]).",
  },
  {
    "LspRename",
    ":LspRename [name] [server]",
    "Rename the symbol under the cursor to [name] (through [server]).",
  },
  {
    "LspCodeAction",
    ":LspCodeAction [server]",
    "Choose a code action for the cursor position (from [server] alone).",
  },
  {
    "LspFormat",
    ":LspFormat [server]",
    "Format the current buffer with the language server [server].",
  },
  { "LspDiagnostics", ":LspDiagnostics", "List the buffer's diagnostics in the location list." },
  -- Tree-sitter parser management.
  { "TSInstall", ":TSInstall {lang}", "Install the tree-sitter parser for {lang}." },
  { "TSUpdate", ":TSUpdate [lang]", "Update installed tree-sitter parsers (all, or [lang])." },
  { "TSInstallInfo", ":TSInstallInfo", "List installed / available tree-sitter parsers." },
}

-- `nx.cmdline_complete.setup{ docs = true }`: enable the engine. `docs` toggles the
-- params/help preview pane (Phase 3; default true). A bare call enables it with
-- defaults. Failing loud on a non-table argument (no silent stub).
function nx.cmdline_complete.setup(opts)
  opts = opts or {}
  if type(opts) ~= "table" then
    error("nx.cmdline_complete.setup: expected a table, got " .. type(opts))
  end
  local docs = opts.docs
  if docs == nil then
    docs = true
  end
  nx._cmdline_complete_setup(docs == true)
end

-- Append the registered user commands (`nx.user_command.create`) to `out` so a
-- plugin command appears in the wildmenu exactly as a built-in does — the unified
-- payoff. Both the global registry and the current buffer's buffer-local commands
-- are merged (a buffer-local shadows a global of the same name, matching dispatch);
-- `seen` dedups so a buffer-local doesn't list twice and a plugin can't double a
-- built-in. The command's `desc` (when given) becomes its docs, headed by a `:Name`
-- synopsis; a command with no `desc` shows the synopsis alone. A declared `usage`
-- argument signature is appended to the synopsis (`:Name [arg]`), like a built-in's.
local function append_user_commands(out, seen)
  local function add(commands)
    for name, record in pairs(commands) do
      if not seen[name] then
        seen[name] = true
        -- The synopsis heads the docs float, like a built-in's. A declared `usage`
        -- argument signature follows the name (`:Name [arg]`); otherwise it's bare.
        local synopsis = ":" .. name
        if record.usage and record.usage ~= "" then
          synopsis = synopsis .. " " .. record.usage
        end
        local desc = record.desc
        local doc = synopsis
        if desc and desc ~= "" then
          doc = doc .. "\n\n" .. desc
        end
        out[#out + 1] = { label = name, insert = name, doc = doc }
      end
    end
  end
  -- Buffer-local first so it wins the `seen` dedup over a same-named global.
  add(nx.user_command.buf_get(0))
  add(nx.user_command.get())
end

-- The candidate set for the **command name** itself: the curated built-in catalog
-- plus the registered user commands.
local function command_candidates()
  local out = {}
  local seen = {}
  for _, cmd in ipairs(BUILTIN) do
    local name, synopsis, description = cmd[1], cmd[2], cmd[3]
    seen[name] = true
    out[#out + 1] = {
      label = name,
      insert = name,
      -- The docs pane (Phase 3): the synopsis heads the float, the description
      -- follows a blank line. Core skips a leading blank, so a command with no
      -- description still renders cleanly.
      doc = synopsis .. "\n\n" .. description,
    }
  end
  append_user_commands(out, seen)
  return out
end

-- The `:set`-family commands whose arguments are option names. They share the same
-- `ex_set` handler in core (`editor/ex.rs`), differing only in which tier of a
-- buffer/window-local option the write lands on; `:setfiletype` completes filetypes,
-- not options, so it is excluded here and has its own completer (`SETFILETYPE_COMMANDS`).
local SET_COMMANDS = {
  set = true,
  se = true,
  setlocal = true,
  setl = true,
  setglobal = true,
  setg = true,
}

-- Friendlier spellings for the docs pane's metadata line.
local KIND_LABEL = { bool = "boolean", number = "number", string = "string" }
local SCOPE_LABEL = { global = "global", window = "window-local", buffer = "buffer-local" }

-- The candidate set for an option name (the argument of any `SET_COMMANDS` entry): every
-- option from core's injected catalog (`nx._options_catalog`, the single source of
-- truth — it can never drift from what `:set` accepts). The canonical name is the
-- label/insert (vim completes `:set nu` to `number`; the abbreviation still matches
-- via fuzzy ranking and is shown in the docs). The docs pane heads with the name +
-- abbreviation, then a `scope, kind` line, then the one-line description.
local function option_candidates()
  local out = {}
  for _, opt in ipairs(nx._options_catalog or {}) do
    local header = opt.name
    if opt.abbrev then
      header = header .. " (" .. opt.abbrev .. ")"
    end
    local meta = (SCOPE_LABEL[opt.scope] or opt.scope) .. ", " .. (KIND_LABEL[opt.kind] or opt.kind)
    out[#out + 1] = {
      label = opt.name,
      insert = opt.name,
      doc = header .. "\n" .. meta .. "\n\n" .. opt.doc,
    }
  end
  return out
end

-- ----- buffer / colorscheme / highlight argument completion (inline wildmenu) ----
-- Three commands whose argument is a name the editor already knows: a buffer, a
-- color scheme, or a highlight group. Each returns candidates straight to the inline
-- wildmenu (like `:set`), read from an authoritative in-memory source — never a
-- filesystem or name heuristic — so it can never drift from what the command accepts.

-- The buffer-taking commands whose argument is a buffer: `:buffer` and its unload
-- twins (`:bdelete`/`:bwipeout`). Core's `resolve_buffer` accepts a bufnr or any
-- unique substring of a buffer's path, so completing the full buffer name resolves.
local BUFFER_COMMANDS = {}
for _, c in ipairs({
  "b",
  "bu",
  "buf",
  "buffer",
  "bd",
  "bdel",
  "bdelete",
  "bw",
  "bwipe",
  "bwipeout",
}) do
  BUFFER_COMMANDS[c] = true
end

-- The candidate set for a buffer argument: every named buffer, by name, from the
-- authoritative buffer mirror (`nx._bufs`). The name is both the shown label and the
-- inserted token (`:buffer {name}` resolves it by substring), and the docs pane shows
-- the buffer number. Unnamed buffers are skipped — there is no name to complete to,
-- so `:b {nr}` (or the `buffers` picker, which does list them as `[No Name]`) is how
-- you reach one.
local function buffer_candidates()
  local out = {}
  local bufs = nx._bufs or {}
  for _, b in ipairs(nx.buf.list()) do
    local entry = bufs[b]
    local name = (entry and entry.name) or nx.buf.name(b)
    if name and name ~= "" then
      out[#out + 1] = { label = name, insert = name, doc = ":buffer " .. b .. "\n\n" .. name }
    end
  end
  return out
end

-- `:colorscheme` (and its `:colo` abbreviation — the only two spellings core
-- dispatches, see `excmd.rs`) completes color scheme names.
local COLORSCHEME_COMMANDS = { colo = true, colorscheme = true }

-- The candidate set for a `:colorscheme` argument: every `colors/<name>.lua` on the
-- live runtimepath (found synchronously via `nx.runtime_file`, so a plugin-installed
-- scheme shows the instant it lands on disk) plus the schemes bundled in the binary
-- (`nx._builtin_colorschemes`, injected by the server — the embedded `runtime/colors/`
-- tree is not a real runtimepath directory, so the glob cannot see it). Sorted, deduped.
local function colorscheme_candidates()
  local names, seen = {}, {}
  local function add(name)
    if name and name ~= "" and not seen[name] then
      seen[name] = true
      names[#names + 1] = name
    end
  end
  for _, name in ipairs(nx._builtin_colorschemes or {}) do
    add(name)
  end
  for _, path in ipairs(nx.runtime_file("colors/*.lua", true)) do
    add(path:match("([^/]+)%.lua$"))
  end
  table.sort(names)
  local out = {}
  for _, name in ipairs(names) do
    out[#out + 1] = { label = name, insert = name }
  end
  return out
end

-- `:highlight` / `:hi` completes highlight group names.
local HIGHLIGHT_COMMANDS = { hi = true, highlight = true }

-- The candidate set for a `:highlight` argument: every defined highlight group, by
-- name, from the highlight-registry mirror (`nx._hl_defs`, the same table `nx.hl.get`
-- reads). Sorted for a stable menu.
local function highlight_candidates()
  local names = {}
  for name in pairs(nx._hl_defs or {}) do
    names[#names + 1] = name
  end
  table.sort(names)
  local out = {}
  for _, name in ipairs(names) do
    out[#out + 1] = { label = name, insert = name }
  end
  return out
end

-- `:setfiletype` completes a filetype name. (It is deliberately NOT in `SET_COMMANDS`
-- — its argument is a filetype, not an option.)
local SETFILETYPE_COMMANDS = {}
for _, c in ipairs({
  "setf",
  "setfi",
  "setfil",
  "setfile",
  "setfilet",
  "setfilety",
  "setfiletyp",
  "setfiletype",
}) do
  SETFILETYPE_COMMANDS[c] = true
end

-- The candidate set for a `:setfiletype` argument: the filetype names nxvim recognizes
-- (`nx._filetypes`, injected by the server from core's extension-detection table — the
-- single source of truth). A buffer can still be forced to any string; this is the
-- known/highlighting-capable set offered for convenience.
local function filetype_candidates()
  local out = {}
  for _, ft in ipairs(nx._filetypes or {}) do
    out[#out + 1] = { label = ft, insert = ft }
  end
  return out
end

-- `:TSInstall` / `:TSUpdate` install / update tree-sitter parsers by LANGUAGE name.
-- The full installable catalog is nvim-treesitter's (~hundreds, behind a network
-- fetch), so it can't be listed synchronously — but a filetype name IS the tree-sitter
-- language name in nxvim (`language_of_path`), and those are exactly the languages
-- nxvim highlights, so `nx._filetypes` is the meaningful, offline candidate set. Both
-- commands take MULTIPLE space-separated languages, so the completer is NOT first-arg
-- gated — every argument word completes a language.
local TS_COMMANDS = { TSInstall = true, TSUpdate = true }

-- ----- `:Lsp*` server routing argument ---------------------------------------
-- Every `:Lsp*` request verb takes an optional trailing `[server]` — the config name
-- of the attached client to route to. The names come from the buffer's own clients,
-- so the completion is exactly the set the command can accept.
--
-- The slot differs by command: `:LspRename` takes the new identifier first, so its
-- server is the SECOND argument; the rest take it as the first.
local LSP_SERVER_COMMANDS = {
  LspHover = 1,
  LspDefinition = 1,
  LspDeclaration = 1,
  LspTypeDefinition = 1,
  LspImplementation = 1,
  LspReferences = 1,
  LspSignatureHelp = 1,
  LspCodeAction = 1,
  LspFormat = 1,
  LspRename = 2,
}

-- The candidate set for a `[server]` argument: the config names of the clients
-- attached to the current buffer, deduplicated and sorted. Empty (so the wildmenu
-- stays closed) when no server is attached — there is nothing to route to.
local function lsp_server_candidates()
  local names, seen = {}, {}
  for _, client in ipairs(nx.lsp.clients({ bufnr = 0 })) do
    if client.name and not seen[client.name] then
      seen[client.name] = true
      names[#names + 1] = client.name
    end
  end
  table.sort(names)
  local out = {}
  for _, name in ipairs(names) do
    out[#out + 1] = { label = name, insert = name }
  end
  return out
end

-- ----- autocmd events / augroups, registers, addresses (first-argument completers) --
-- These commands complete a name in their FIRST argument slot only — an event, an
-- augroup, a register, or an address landmark — so `on_first_arg` gates them (a later
-- word, e.g. an `:autocmd` pattern or command, has no completer and stays closed).

-- Whether the cursor sits on the FIRST argument of the command — no completed
-- argument word precedes the partial being typed. Skips a leading range (the command
-- word is the first letter-run), so `:'<,'>move <Tab>` still counts as first-argument.
local function on_first_arg(before)
  local after = before:match("%a%w*%s+(.*)$")
  return after ~= nil and not after:match("%S%s")
end

-- `:autocmd` / `:doautocmd` complete an event name in their first argument
-- (`:autocmd BufWrite<Tab>`); `:augroup` completes a defined group name.
local AUTOCMD_EVENT_COMMANDS = {}
for _, c in ipairs({
  "au",
  "aut",
  "auto",
  "autoc",
  "autocm",
  "autocmd",
  "doau",
  "doaut",
  "doauto",
  "doautoc",
  "doautocm",
  "doautocmd",
}) do
  AUTOCMD_EVENT_COMMANDS[c] = true
end
local AUGROUP_COMMANDS = {}
for _, c in ipairs({ "aug", "augr", "augro", "augrou", "augroup" }) do
  AUGROUP_COMMANDS[c] = true
end

-- The autocmd events nxvim emits, plus the accepted aliases (canonicalized on
-- registration). Hand-curated to mirror `docs/autocmd-events.md` — the authoritative
-- catalog, since nxvim has no runtime registry of the *emitted* set to source from —
-- so keep the two in sync when an event is added (like the `BUILTIN` command catalog).
local AUTOCMD_EVENTS = {
  -- Buffer lifecycle
  "BufAdd",
  "BufDelete",
  "BufEnter",
  "BufLeave",
  "BufNewFile",
  "BufReadCmd",
  "BufReadPost",
  "BufWinEnter",
  "FileType",
  -- Writing
  "BufWritePost",
  "BufWritePre",
  -- Window & tab
  "TabClosed",
  "TabEnter",
  "TabLeave",
  "TabNew",
  "WinClosed",
  "WinEnter",
  "WinLeave",
  "WinNew",
  "WinResized",
  "WinScrolled",
  -- Mode
  "InsertEnter",
  "InsertLeave",
  "ModeChanged",
  -- Editing & cursor
  "CursorMoved",
  "CursorMovedI",
  "TextChanged",
  "TextChangedI",
  -- LSP
  "LspAttach",
  "LspDetach",
  "LspProgress",
  -- Files & environment
  "ColorScheme",
  "DirChanged",
  "EncodingChanged",
  "FileChangedShell",
  "FileChangedShellPost",
  -- Startup & plugins
  "PluginLoaded",
  "PluginsLoaded",
  "UIEnter",
  "VimEnter",
  -- Accepted aliases (each canonicalizes to a real emitted event)
  "BufCreate",
  "BufRead",
  "BufWrite",
  "FileEncoding",
}

local function autocmd_event_candidates()
  local out = {}
  for _, name in ipairs(AUTOCMD_EVENTS) do
    out[#out + 1] = { label = name, insert = name }
  end
  return out
end

-- The candidate set for `:augroup`: every currently-defined autocommand group, from
-- the live `nx._augroups` registry (name -> id). Sorted for a stable menu.
local function augroup_candidates()
  local names = {}
  for name in pairs(nx._augroups or {}) do
    names[#names + 1] = name
  end
  table.sort(names)
  local out = {}
  for _, name in ipairs(names) do
    out[#out + 1] = { label = name, insert = name }
  end
  return out
end

-- `:put` READS a register, so its argument completes to the registers that hold
-- content. `:delete`/`:yank` WRITE their register, so completing existing-content
-- registers there would mislead — they are deliberately excluded.
local REGISTER_COMMANDS = { pu = true, put = true }

-- The candidate set for a `:put` register argument: every register that holds content,
-- from the register mirror (`nx._registers`, name -> { text, type }). The single-char
-- name is the label/insert; the docs pane previews the register's first line.
local function register_candidates()
  local names = {}
  for name in pairs(nx._registers or {}) do
    names[#names + 1] = name
  end
  table.sort(names)
  local out = {}
  for _, name in ipairs(names) do
    local reg = nx._registers[name]
    local first = ((reg and reg.text) or ""):match("^[^\n]*") or ""
    if #first > 60 then
      first = first:sub(1, 57) .. "..."
    end
    out[#out + 1] = { label = name, insert = name, doc = 'register "' .. name .. '"\n\n' .. first }
  end
  return out
end

-- `:move` / `:copy` (`:m` / `:t`) relocate the range below an address; complete the
-- addressable landmarks — the specials `.`/`$`/`0` and the marks set in the CURRENT
-- buffer (as `'x` addresses).
local ADDRESS_COMMANDS = {}
for _, c in ipairs({ "m", "mo", "mov", "move", "t", "co", "cop", "copy" }) do
  ADDRESS_COMMANDS[c] = true
end

local function address_candidates()
  local out = {
    { label = ".", insert = ".", doc = "the current line" },
    { label = "$", insert = "$", doc = "the last line" },
    { label = "0", insert = "0", doc = "above the first line" },
  }
  local cur = nx.buf.current()
  for _, m in ipairs(nx.mark.list()) do
    -- A `'x` address references a line in the CURRENT buffer, so skip marks pointing
    -- elsewhere (an out-of-buffer global mark, an unset one). `line` is 0-based here.
    if m.bufnr == cur and m.name:match("^%a$") then
      out[#out + 1] = {
        label = "'" .. m.name,
        insert = "'" .. m.name,
        doc = "mark " .. m.name .. " — line " .. (m.line + 1) .. "\n\n" .. (m.text or ""),
      }
    end
  end
  return out
end

-- ----- command modifiers that wrap a nested command --------------------------------
-- `:vertical` / `:tab` / `:silent` take a nested command as their argument. Once the
-- modifier word is complete, `nx._cmdline_complete_run` strips it and completes the
-- REMAINDER — so the nested command's name and arguments complete as if typed bare,
-- and chained modifiers recurse (`:tab vertical split …`). These are the only three
-- the core / server dispatch (nxvim has no `:aboveleft`/`:keepjumps`/… modifiers).
local WRAPPER_COMMANDS = {}
for _, c in ipairs({ "ver", "vert", "vertical", "tab", "sil", "sile", "silen", "silent" }) do
  WRAPPER_COMMANDS[c] = true
end

-- ----- file-path completion via the picker --------------------------------------
-- The file-taking ex commands (and their standard abbreviations) whose argument is
-- a path: `<Tab>` in the argument region opens the file picker rather than the
-- wildmenu. Kept generous but conservative — every entry opens or writes a file.
local FILE_COMMANDS = {}
for _, c in ipairs({
  "e",
  "ed",
  "edi",
  "edit",
  "sp",
  "spl",
  "spli",
  "split",
  "vs",
  "vsp",
  "vspl",
  "vsplit",
  "new",
  "vne",
  "vnew",
  "tabe",
  "tabed",
  "tabedi",
  "tabedit",
  "tabnew",
  "vie",
  "view",
  "r",
  "re",
  "rea",
  "read",
  "w",
  "wr",
  "wri",
  "writ",
  "write",
  "wq",
  "x",
  "xi",
  "xit",
  "sav",
  "save",
  "savea",
  "saveas",
  "so",
  "sou",
  "sour",
  "sourc",
  "source",
  "badd",
  "drop",
  "ped",
  "pedit",
  "cf",
  "cfi",
  "cfile",
  "lf",
  "lfi",
  "lfile",
  "diffs",
  "diffsp",
  "diffsplit",
}) do
  FILE_COMMANDS[c] = true
end

-- Directory-taking commands: the picker lists directories only and confirming one
-- runs the command on it (`:cd dir`) rather than descending into it.
local DIR_COMMANDS = {}
for _, c in ipairs({ "cd", "chd", "chdir", "tcd", "lcd", "lch", "lchdir" }) do
  DIR_COMMANDS[c] = true
end

-- The command context captured at handoff, read by the `cmdline_files` source on
-- confirm. `dirs_only` lists / selects directories (the `:cd` family). The command
-- line stays open under the picker, so confirm just pastes the chosen path into the
-- argument token (`nx._cmdline_set_arg`) — the user runs the filled line with <CR>.
local pending = nil

-- The pending promise for an *async* function completer (a user command's
-- `complete = function(args)` that returned a promise), read by the
-- `cmdline_complete_fn` picker source. Only one cmdline picker is open at a time, so a
-- single module-level handoff slot is enough (the same pattern as `pending`).
local pending_fn = nil

-- Split a typed path token into `(base, leaf)`: `base` is everything up to and
-- including the last `"/"` (kept verbatim so the spliced command stays in the user's
-- relative/absolute/`~` style), `leaf` is the partial name after it. No `"/"` ⇒ the
-- whole token is the leaf and the base is empty (list the cwd).
local function split_path(token)
  local base, leaf = token:match("^(.*/)([^/]*)$")
  if base then
    return base, leaf
  end
  return "", token
end

-- Resolve the directory to LIST from the typed `base` and the picker `cwd`. A
-- leading `"~"` / `"~/"` expands to `$HOME` (when set); an absolute base lists itself; a
-- relative base joins onto cwd. Always returns an absolute path for `nx.fs`.
local function resolve_dir(base, cwd)
  if base == "" then
    return cwd
  end
  local p = nx.utils.expanduser(base)
  if p:sub(1, 1) ~= "/" then
    p = (cwd:gsub("/$", "")) .. "/" .. p
  end
  -- Strip the trailing "/" the typed base carries (`sub/`) so `nx.fs.readdir` gets a
  -- bare directory path; keep a lone root "/".
  return (p:gsub("(.)/+$", "%1"))
end

-- A case-insensitive subsequence test (`pat`'s chars appear in order in `s`) — the
-- fuzzy tier below the exact-prefix tier.
local function subsequence(s, pat)
  local i = 1
  for j = 1, #pat do
    local found = s:find(pat:sub(j, j), i, true)
    if not found then
      return false
    end
    i = found + 1
  end
  return true
end

-- Rank a directory's entries against the partial `leaf`, returning the matches in
-- display order: an exact case-insensitive prefix tier first, then a subsequence
-- (fuzzy) tier, and within each tier **directories before files**, alphabetically.
-- These are all immediate children of the listed directory, so ranking them first
-- is what "prioritise same-level candidates" means here. Dotfiles are hidden unless
-- the leaf itself starts with `"."` (vim's wildignore-ish default). `dirs_only` keeps
-- only directories (and symlinks, which may point at one).
local function rank_entries(entries, leaf, dirs_only)
  local lleaf = leaf:lower()
  local want_hidden = leaf:sub(1, 1) == "."
  local matches = {}
  for _, e in ipairs(entries) do
    local is_dir = e.type == "directory"
    local kind_ok = (not dirs_only) or is_dir or e.type == "link"
    local hidden = e.name:sub(1, 1) == "."
    if kind_ok and (want_hidden or not hidden) then
      local lname = e.name:lower()
      local tier
      if leaf == "" then
        tier = 2
      elseif lname:sub(1, #lleaf) == lleaf then
        tier = 0
      elseif subsequence(lname, lleaf) then
        tier = 1
      end
      if tier then
        matches[#matches + 1] = { name = e.name, is_dir = is_dir, tier = tier }
      end
    end
  end
  table.sort(matches, function(a, b)
    if a.tier ~= b.tier then
      return a.tier < b.tier
    end
    if a.is_dir ~= b.is_dir then
      return a.is_dir -- directories first
    end
    return a.name:lower() < b.name:lower()
  end)
  return matches
end

-- The cmdline file picker: a DYNAMIC source (so it controls its own ordering —
-- same-level first — and re-lists as the query is edited). Per query it lists ONE
-- directory (the one the typed base points at) via `nx.fs.readdir` (async, so it
-- works native / over the daemon / on wasm-OPFS), pushing the same-level matches.
-- Crossing a `"/"` re-roots; confirming a directory descends (file mode) or selects
-- it (`dirs_only`). Confirming a file splices its path into the captured command
-- line and runs it.
nx.picker.source({
  name = "cmdline_files",
  layer = "main", -- a picked file opens in the main editor, never a focused dock
  resumable = false, -- transient: confirm pastes into the open cmdline; `<leader>fr` skips it
  dynamic = true,
  debounce = 0, -- a local readdir is instant, so re-list on every keystroke
  multiselect = false, -- a single path is chosen; `<Tab>` marking makes no sense

  items = nx.async(function(ctx)
    local base, leaf = split_path(ctx.query)
    local dir = resolve_dir(base, ctx.cwd)
    local ok, entries = pcall(nx.await, nx.fs.readdir(dir))
    if not ok or type(entries) ~= "table" then
      return -- an unreadable / non-existent directory simply lists nothing
    end
    -- Inside a sub-directory (a non-empty base) and not yet filtering: offer a first
    -- "<select directory>" row that USES this directory as the path rather than
    -- descending into it — `:cd src/<select directory>` selects `src/`, and a file
    -- command pastes the directory path. It carries `is_dir = false` so confirm
    -- pastes it (no descend); typing a leaf hides it so it never shadows a match.
    if base ~= "" and leaf == "" then
      ctx.push({ text = "<select directory>", path = base, is_dir = false })
    end
    local dirs_only = pending ~= nil and pending.dirs_only
    for _, m in ipairs(rank_entries(entries, leaf, dirs_only)) do
      -- A directory shows (and splices) with a trailing "/"; the path keeps the
      -- typed base so the reconstructed command stays in the user's style.
      local shown = m.is_dir and (m.name .. "/") or m.name
      ctx.push({ text = shown, path = base .. shown, is_dir = m.is_dir })
    end
  end),
  confirm = function(item)
    local dirs_only = pending ~= nil and pending.dirs_only
    if item.is_dir and not dirs_only then
      -- Descend: re-open the picker one level deeper, seeded with this directory.
      -- Deferred a tick so the current picker has fully closed first (the confirm
      -- runs with `nx._picker` already cleared — re-opening inline would race). The
      -- command line stays open underneath the whole time.
      nx.on_next_tick(function()
        nx.picker.open(
          "cmdline_files",
          { query = item.path, preview = "file", title = "Select file" }
        )
      end)
      return
    end
    -- Paste the chosen path into the still-open command line's argument token — NO
    -- execute. The user runs the now-filled line (`:e src/foo.rs`) with <CR>.
    nx._cmdline_set_arg(item.path)
  end,
})

-- Normalize one candidate from a function completer's returned list to the shared
-- `{ label, insert, doc }` shape. A bare string is both the shown label and the
-- inserted text; a table may set `label` (shown), `insert` (pasted, defaults to the
-- label/text), and `doc` (the wildmenu docs float). Anything else is dropped.
local function normalize_candidate(c)
  if type(c) == "string" then
    return { label = c, insert = c }
  elseif type(c) == "table" then
    local insert = c.insert or c.label or c.text or ""
    return { label = c.label or c.text or insert, insert = insert, doc = c.doc }
  end
  return nil
end

local function normalize_candidates(list)
  local out = {}
  for _, c in ipairs(list or {}) do
    local n = normalize_candidate(c)
    if n then
      out[#out + 1] = n
    end
  end
  return out
end

-- The picker source for an ASYNC function completer: it awaits the promise the
-- completer returned (captured in `pending_fn`) and pushes its candidates. A sync
-- function completer never reaches here — it returns its list straight to the inline
-- wildmenu (see `nx._cmdline_complete_run`); only a promise-returning one routes
-- through the picker, since the wildmenu path is synchronous. Confirm pastes the
-- chosen value into the still-open command line's argument token.
nx.picker.source({
  name = "cmdline_complete_fn",
  resumable = false, -- transient: confirm pastes into the open cmdline
  multiselect = false, -- a single value is chosen
  items = nx.async(function(ctx)
    local spec = pending_fn
    if not spec or not spec.promise then
      return
    end
    -- Await the completer's promise; a rejection propagates and the picker reports it.
    local cands = nx.await(spec.promise)
    for _, c in ipairs(cands or {}) do
      local n = normalize_candidate(c)
      if n then
        ctx.push({ text = n.label, insert = n.insert })
      end
    end
  end),
  confirm = function(item)
    nx._cmdline_set_arg(item.insert or item.text)
  end,
})

-- The whitespace-separated argument words typed before the cursor (the command name
-- dropped), the last of which is the partial word being completed. This is what a
-- function completer receives: `:Cmd<Tab>`/`:Cmd <Tab>` → `{}`, `:Cmd a<Tab>` →
-- `{ "a" }`, `:Cmd a b<Tab>` → `{ "a", "b" }`.
local function arg_list(before)
  local args = {}
  local first = true
  for word in before:gmatch("%S+") do
    if first then
      first = false -- drop the command name itself
    else
      args[#args + 1] = word
    end
  end
  return args
end

-- Which argument slot the cursor sits in, 1-based: the number of *completed* argument
-- words before it, plus one. `:LspHover p|` and `:LspHover |` are both slot 1;
-- `:LspRename Foo p|` is slot 2. The gate for a command whose routing argument is not
-- the first (`on_first_arg` covers only the first).
local function arg_slot(before)
  local n = #arg_list(before)
  return before:match("%s$") and n + 1 or math.max(n, 1)
end

-- `nx._cmdline_complete_run(line, col)`: the candidate source the server calls
-- synchronously per `<Tab>` (and each edit while the wildmenu is open). It returns
-- the candidate list — a `{ {label, insert[, doc]}, ... }` array — and core
-- fuzzy-ranks it against the token it extracted. Core completes either the leading
-- command name or, once whitespace separates it, the current argument word; this
-- source picks the candidate set from `line` / `col`:
--   * still in the command name  → the command catalog (built-ins + user commands);
--   * behind a `:vertical`/`:tab`/`:silent` modifier → strip it and complete the rest;
--   * in a `:set` argument        → option names (with docs);
--   * a `:buffer`/`:colorscheme`/`:highlight` argument → that name set;
--   * an `:autocmd`/`:augroup`/`:put`/`:move` first argument → event/group/register/addr;
--   * in a file/dir argument      → launch the file picker, return the sentinel;
--   * any other command's args    → nothing yet (core closes the menu).
--
-- The file-picker case is a SIDE EFFECT (it queues `nx.picker.open`) and returns
-- the `{ __picker = true }` sentinel: the server recognises it, dismisses the
-- command line, and lets the queued picker take over (`CmdlineComplete::PickerLaunched`).
-- Launch the file/dir picker for a path argument and return the `{ __picker }`
-- sentinel. The path typed so far (`before`'s trailing non-whitespace run) seeds the
-- picker; since the command line stays open, confirm pastes the choice back into this
-- same argument token via `nx._cmdline_set_arg`. `dirs_only` lists directories only.
local function launch_path_picker(before, dirs_only)
  local prefix = before:match("(%S*)$") or ""
  pending = { dirs_only = dirs_only }
  local popts = { query = prefix, title = dirs_only and "Select directory" or "Select file" }
  if not dirs_only then
    popts.preview = "file" -- a file preview pane (directories list their contents)
  end
  nx.picker.open("cmdline_files", popts)
  return { __picker = true }
end

-- The argument completer a user command declared via `create(... { complete = })`
-- (a buffer-local command shadows a global of the same name) — `"dir"` / `"file"`, a
-- function `fn(args)`, or nil. Lets a registered command (e.g. the GUI's `:workspace`)
-- get the same path completion the built-in `:cd` / `:edit` get, or generate its own.
local function user_command_complete(cmd)
  local rec = nx.user_command.buf_get(0)[cmd] or nx.user_command.get()[cmd]
  return rec and rec.complete or nil
end

-- Whether `v` is a thenable (a promise) — what an *async* completer returns.
local function is_promise(v)
  return type(v) == "table" and type(v.next) == "function"
end

-- Launch the picker for an async function completer: it awaits `promise` (the value
-- the completer returned) and lists its candidates, seeded/filtered by the partial
-- argument word. Returns the `{ __picker }` sentinel like the path picker.
local function launch_fn_picker(before, cmd, promise)
  local prefix = before:match("(%S*)$") or ""
  pending_fn = { promise = promise }
  nx.picker.open("cmdline_complete_fn", { query = prefix, title = ":" .. cmd })
  return { __picker = true }
end

function nx._cmdline_complete_run(line, col)
  -- Argument region iff a complete word is followed by whitespace before the cursor.
  -- The command word and its trailing space are ASCII, so this byte scan of the
  -- char-offset-truncated prefix is exact for the only structure we test.
  local before = line:sub(1, col)
  -- A command modifier (`:vertical`/`:tab`/`:silent`, optional `!`) wraps a nested
  -- command: once past the modifier word, strip it (and its trailing whitespace) and
  -- complete the REMAINDER, so the nested command completes as if typed bare. The
  -- prefix is leading ASCII, so removing `#prefix` bytes from both `line` and `col`
  -- is exact; chained modifiers recurse (`:tab vertical split …`).
  local wrap_prefix = before:match("^(%a%w*!?%s+)")
  if wrap_prefix and WRAPPER_COMMANDS[wrap_prefix:match("^(%a%w*)")] then
    local n = #wrap_prefix
    return nx._cmdline_complete_run(line:sub(n + 1), col - n)
  end
  if before:match("%S+%s") then
    local cmd = line:match("(%a%w*)") -- the first word — the command name
    if cmd and SET_COMMANDS[cmd] then
      return option_candidates()
    end
    if cmd and BUFFER_COMMANDS[cmd] then
      return buffer_candidates()
    end
    if cmd and COLORSCHEME_COMMANDS[cmd] then
      return colorscheme_candidates()
    end
    if cmd and HIGHLIGHT_COMMANDS[cmd] then
      return highlight_candidates()
    end
    if cmd and SETFILETYPE_COMMANDS[cmd] then
      return on_first_arg(before) and filetype_candidates() or {}
    end
    -- `:TSInstall`/`:TSUpdate` complete a tree-sitter language in EVERY argument (they
    -- take several), sharing the filetype names (a filetype IS the language name here).
    if cmd and TS_COMMANDS[cmd] then
      return filetype_candidates()
    end
    -- The `:Lsp*` verbs' optional `[server]` route, in its own slot (the second for
    -- `:LspRename`, whose first argument is the new identifier).
    local lsp_slot = cmd and LSP_SERVER_COMMANDS[cmd]
    if lsp_slot then
      return arg_slot(before) == lsp_slot and lsp_server_candidates() or {}
    end
    -- First-argument name completers (event / group / register / address). Gated on
    -- `on_first_arg` so they fire only for their own slot, not a later word.
    if cmd and AUTOCMD_EVENT_COMMANDS[cmd] then
      return on_first_arg(before) and autocmd_event_candidates() or {}
    end
    if cmd and AUGROUP_COMMANDS[cmd] then
      return on_first_arg(before) and augroup_candidates() or {}
    end
    if cmd and REGISTER_COMMANDS[cmd] then
      return on_first_arg(before) and register_candidates() or {}
    end
    if cmd and ADDRESS_COMMANDS[cmd] then
      return on_first_arg(before) and address_candidates() or {}
    end
    if cmd and (FILE_COMMANDS[cmd] or DIR_COMMANDS[cmd]) then
      return launch_path_picker(before, DIR_COMMANDS[cmd] == true)
    end
    -- A user command's declared argument completer: `"dir"`/`"file"` reuse the path
    -- picker; a function generates candidates from the args typed so far.
    local uc = cmd and user_command_complete(cmd)
    if uc == "dir" or uc == "file" then
      return launch_path_picker(before, uc == "dir")
    end
    if type(uc) == "function" then
      -- Call the completer with the args so far. A throw yields no candidates rather
      -- than breaking the command line. A SYNC completer returns its list straight to
      -- the inline wildmenu (core fuzzy-ranks it against the partial word, and re-runs
      -- this per keystroke so it tracks the growing args); an ASYNC one returns a
      -- promise, which can't be awaited synchronously here, so it routes through the
      -- picker instead.
      local ok, result = pcall(uc, arg_list(before))
      if not ok then
        return {}
      end
      if is_promise(result) then
        return launch_fn_picker(before, cmd, result)
      end
      return normalize_candidates(result)
    end
    return {} -- no argument completer for this command yet
  end
  return command_candidates()
end
