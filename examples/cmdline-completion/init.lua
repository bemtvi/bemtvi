-- ~~~ nxvim nx.cmdline_complete playground: command-line completion ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/cmdline-completion \
--       cargo run -p nxvim -- examples/cmdline-completion/sample.txt
--
-- `nx.cmdline_complete` is the wildmenu on the unified float-list widget — its
-- FIFTH orchestration (picker / nx.ui.select / insert-completion / the docs
-- sidebar being the prior four; docs/specs/2026-06-14-nx-ui-float-widget.md,
-- docs/plans/2026-06-16-cmdline-completion.md). Press <Tab> on an ex command line
-- (`:`) and a fuzzy list of matching command NAMES floats just above the line,
-- anchored under the command-name token.
--
-- Engine in Rust core, policy in Lua: core extracts the token, fuzzy-ranks the
-- catalog (matched chars are highlighted), renders the menu, and applies the
-- accept; the bundled `nx.cmdline_complete` plugin owns the curated command
-- catalog. No input loop runs in Lua (ADR 0002 rule 4).
--
-- WHAT TO TRY (open the command line with `:` first):
--   :e<Tab>            float a list of commands matching `e` (edit, enew, …)
--   :tab<Tab>          narrow to the tab-* family
--   keep typing        the open list narrows LIVE against what you type
--                      (`:e<Tab>` then `d` → just `edit`)
--   <Tab> / <C-n> / <Down>   cycle the highlight forward — a docs float showing the
--                            highlighted command's synopsis + help floats beside it
--   <S-Tab> / <C-p> / <Up>   cycle the highlight backward
--   <CR>               accept the highlighted command (rewriting the token) and
--                      RUN it; with nothing highlighted, run the typed line as-is
--   <Esc>              dismiss the wildmenu but keep the command line open
--                      (a second <Esc> then cancels the line)
--
-- The popup opens with NOTHING highlighted (noselect) — so the first <Tab>
-- highlights the top match and the first <S-Tab> the bottom one, and <CR> keeps
-- running the typed line until you actually pick a row.
--
-- A worked example against this buffer:
--   :ene<Tab>          float the list — `enew` is the top fuzzy match for `ene`
--   <Tab>              highlight `enew`
--   <CR>               accept + run :enew → a new empty buffer replaces this one
--
-- THE UNIFIED PAYOFF — a plugin command appears like a built-in:
--   :Greet<Tab>        the `:Greet` command registered below shows in the list, and
--                      its `desc` is the docs float text — no extra wiring
--
-- Command NAMES + docs (built-ins and user commands). Argument completion (`:e
-- <file>`, `:set <opt>`, … — a later, pure-Lua extension; the source already
-- receives the whole line + cursor) is not in this phase.

-- Enable the engine. A bare `setup {}` turns command-line completion on; the
-- engine is OFF until this call, so an editor with no `setup` behaves exactly as
-- before. (`docs = true` is the default — the synopsis/help float beside the
-- highlighted row; pass `docs = false` for a names-only wildmenu.)
nx.cmdline_complete.setup {}

-- A plugin command with a `desc`: it joins the wildmenu catalog automatically (via
-- nx.user_command.get()), ranked and previewed exactly like a built-in.
nx.user_command.create("Greet", function()
  nx.notify("Hello from a plugin command!")
end, { desc = "Print a friendly greeting" })
