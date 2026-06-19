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
-- Argument completion (`:e <file>`, `:set <opt>`, …) is a later, pure-Lua extension
-- — the source already receives the whole line + cursor.

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
  { "messages", ":messages", "Show recorded messages." },
  { "echo", ":echo {expr}", "Evaluate and display {expr}." },
  { "echomsg", ":echomsg {expr}", "Display {expr} and record it in :messages." },
  { "echoerr", ":echoerr {expr}", "Display {expr} as an error and record it." },
  { "registers", ":registers", "Display the contents of the registers." },
  { "marks", ":marks", "List the marks and their positions." },
  { "jumps", ":jumps", "List the jump list." },
  { "changes", ":changes", "List the change list." },
  { "set", ":set {option}", "Set {option} for all buffers and windows." },
  { "setlocal", ":setlocal {option}", "Set {option} for the current buffer/window only." },
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
  { "silent", ":silent {cmd}", "Run {cmd}, suppressing its messages." },
  { "tab", ":tab {cmd}", "Run {cmd} so a window it opens becomes a new tab page." },
  -- LSP (the `:Lsp*` verbs).
  { "LspInfo", ":LspInfo", "Show the LSP clients attached to the buffer." },
  { "LspHover", ":LspHover", "Show hover information for the symbol under the cursor." },
  { "LspDefinition", ":LspDefinition", "Jump to the definition of the symbol under the cursor." },
  {
    "LspDeclaration",
    ":LspDeclaration",
    "Jump to the declaration of the symbol under the cursor.",
  },
  { "LspTypeDefinition", ":LspTypeDefinition", "Jump to the type definition of the symbol." },
  { "LspImplementation", ":LspImplementation", "Jump to the implementation of the symbol." },
  { "LspReferences", ":LspReferences", "List references to the symbol under the cursor." },
  { "LspSignatureHelp", ":LspSignatureHelp", "Show signature help for the call under the cursor." },
  { "LspRename", ":LspRename [name]", "Rename the symbol under the cursor to [name]." },
  { "LspCodeAction", ":LspCodeAction", "Choose a code action for the cursor position." },
  { "LspFormat", ":LspFormat", "Format the current buffer with the language server." },
  { "LspDiagnostics", ":LspDiagnostics", "List the buffer's diagnostics in the location list." },
  -- Tree-sitter parser management.
  { "TSInstall", ":TSInstall {lang}", "Install the tree-sitter parser for {lang}." },
  { "TSUpdate", ":TSUpdate [lang]", "Update installed tree-sitter parsers (all, or [lang])." },
  { "TSInstallInfo", ":TSInstallInfo", "List installed / available tree-sitter parsers." },
}

-- nx.cmdline_complete.setup{ docs = true }: enable the engine. `docs` toggles the
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
-- synopsis; a command with no `desc` shows the synopsis alone.
local function append_user_commands(out, seen)
  local function add(commands)
    for name, record in pairs(commands) do
      if not seen[name] then
        seen[name] = true
        local desc = record.desc
        local doc = ":" .. name
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

-- The `:set`-family commands whose arguments are option names. All four share the
-- same `ex_set` handler in core (`editor/ex.rs`); `:setfiletype` completes filetypes,
-- not options, so it is deliberately excluded.
local SET_COMMANDS = { set = true, se = true, setlocal = true, setl = true }

-- Friendlier spellings for the docs pane's metadata line.
local KIND_LABEL = { bool = "boolean", number = "number", string = "string" }
local SCOPE_LABEL = { global = "global", window = "window-local", buffer = "buffer-local" }

-- The candidate set for an option name (the argument of `:set` / `:setlocal`): every
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

-- nx._cmdline_complete_run(line, col): the candidate source the server calls
-- synchronously per `<Tab>` (and each edit while the wildmenu is open). It returns
-- the candidate list — a `{ {label, insert[, doc]}, ... }` array — and core
-- fuzzy-ranks it against the token it extracted. Core completes either the leading
-- command name or, once whitespace separates it, the current argument word; this
-- source picks the candidate set from `line` / `col`:
--   * still in the command name  → the command catalog (built-ins + user commands);
--   * in a `:set` argument        → option names (with docs);
--   * any other command's args    → nothing yet (core closes the menu).
function nx._cmdline_complete_run(line, col)
  -- Argument region iff a complete word is followed by whitespace before the cursor.
  -- The command word and its trailing space are ASCII, so this byte scan of the
  -- char-offset-truncated prefix is exact for the only structure we test.
  local before = line:sub(1, col)
  if before:match("%S+%s") then
    local cmd = line:match("(%a%w*)") -- the first word — the command name
    if cmd and SET_COMMANDS[cmd] then
      return option_candidates()
    end
    return {} -- no argument completer for this command yet
  end
  return command_candidates()
end
