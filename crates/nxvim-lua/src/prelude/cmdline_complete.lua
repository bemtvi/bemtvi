-- nx.cmdline_complete: command-line completion — the unified float-list widget's
-- **fifth orchestration** (docs/specs/2026-06-14-nx-ui-float-widget.md;
-- docs/plans/2026-06-16-cmdline-completion.md). Pressing `<Tab>` on an ex command
-- line (`:`) offers a fuzzy list of matching command names (with a params/help
-- preview pane in a later phase). The candidate set is **policy** owned here — the
-- curated command catalog, merged with `nx.user_command.get()` (Phase 4) — so core
-- stays out of "what commands exist": it extracts the token, fuzzy-ranks + renders
-- the menu, and applies the accept.
--
-- This phase ships command NAMES only. Argument completion (`:e <file>`, `:set
-- <opt>`, …) is a later, pure-Lua extension — the source already receives the whole
-- line + cursor.

nx.cmdline_complete = nx.cmdline_complete or {}

-- The curated built-in ex-command catalog: the canonical name of each command core
-- fuzzy-ranks against the typed prefix. (Abbreviations match implicitly — fuzzy
-- ranking favours the prefix, so `:e` leads with `edit`.) Synopsis / help text (the
-- docs pane) and the `nx.user_command.get()` merge land in later phases.
local BUILTIN = {
  "write",
  "quit",
  "wq",
  "xit",
  "quitall",
  "wall",
  "wqall",
  "edit",
  "enew",
  "split",
  "vsplit",
  "new",
  "vnew",
  "close",
  "hide",
  "only",
  "terminal",
  "tabnew",
  "tabclose",
  "tabonly",
  "tabmove",
  "tabnext",
  "tabprevious",
  "tabfirst",
  "tablast",
  "drop",
  "resize",
  "vertical",
  "checktime",
  "buffers",
  "buffer",
  "bnext",
  "bprevious",
  "bfirst",
  "blast",
  "bdelete",
  "copen",
  "cclose",
  "cwindow",
  "cnext",
  "cprevious",
  "cfirst",
  "clast",
  "colder",
  "cnewer",
  "lopen",
  "lclose",
  "lwindow",
  "lnext",
  "lprevious",
  "lua",
  "sleep",
  "messages",
  "echo",
  "echomsg",
  "echoerr",
  "registers",
  "marks",
  "jumps",
  "changes",
  "set",
  "setlocal",
  "setfiletype",
  "undo",
  "redo",
  "nohlsearch",
  "substitute",
  "global",
  "vglobal",
  "delete",
  "print",
  "put",
  "highlight",
  "helptags",
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

-- nx._cmdline_complete_run(line, col): the candidate source the server calls
-- synchronously per `<Tab>` (and each edit while the wildmenu is open). It returns
-- the candidate list — a `{ {label, insert[, doc]}, ... }` array — and core
-- fuzzy-ranks it against the command-name token it extracted (core only calls this
-- when the cursor is within the command name, never in its arguments). This phase
-- returns the whole command catalog (names); `line` / `col` drive argument
-- completion in a later phase (`_line` / `_col` are unused until then).
function nx._cmdline_complete_run(_line, _col)
  local out = {}
  for _, name in ipairs(BUILTIN) do
    out[#out + 1] = { label = name, insert = name }
  end
  return out
end
