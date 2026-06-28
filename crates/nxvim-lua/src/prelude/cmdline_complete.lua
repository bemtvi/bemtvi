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
-- `:set <opt>` completes option names inline.
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
-- including the last "/" (kept verbatim so the spliced command stays in the user's
-- relative/absolute/`~` style), `leaf` is the partial name after it. No "/" ⇒ the
-- whole token is the leaf and the base is empty (list the cwd).
local function split_path(token)
  local base, leaf = token:match("^(.*/)([^/]*)$")
  if base then
    return base, leaf
  end
  return "", token
end

-- Resolve the directory to LIST from the typed `base` and the picker `cwd`. A
-- leading "~" / "~/" expands to $HOME (when set); an absolute base lists itself; a
-- relative base joins onto cwd. Always returns an absolute path for `nx.fs`.
local function resolve_dir(base, cwd)
  if base == "" then
    return cwd
  end
  local p = base
  if p == "~" or p:sub(1, 2) == "~/" then
    local home = os.getenv("HOME")
    if home then
      p = home .. p:sub(2)
    end
  end
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
-- the leaf itself starts with "." (vim's wildignore-ish default). `dirs_only` keeps
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
-- Crossing a "/" re-roots; confirming a directory descends (file mode) or selects
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

-- nx._cmdline_complete_run(line, col): the candidate source the server calls
-- synchronously per `<Tab>` (and each edit while the wildmenu is open). It returns
-- the candidate list — a `{ {label, insert[, doc]}, ... }` array — and core
-- fuzzy-ranks it against the token it extracted. Core completes either the leading
-- command name or, once whitespace separates it, the current argument word; this
-- source picks the candidate set from `line` / `col`:
--   * still in the command name  → the command catalog (built-ins + user commands);
--   * in a `:set` argument        → option names (with docs);
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
  if before:match("%S+%s") then
    local cmd = line:match("(%a%w*)") -- the first word — the command name
    if cmd and SET_COMMANDS[cmd] then
      return option_candidates()
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
