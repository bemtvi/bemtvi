-- ~~~ nxvim nx.snippet playground: the native snippet engine ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/snippets \
--       cargo run -p nxvim -- examples/snippets/sample.lua
--
-- `nx.snippet` is the native snippet engine (docs/plans/2026-06-15-nx-snippet-engine.md,
-- docs/specs/2026-06-11-native-plugin-api.md §4). The SERVER owns the LSP snippet
-- grammar, the expansion, the tabstop session, and mirrored placeholders — no input
-- loop runs in Lua. Snippet bodies are LSP snippet syntax:
--
--     $1 / ${1:default}    a tabstop, optionally with default text
--     $0                   the final cursor stop (jumped to last)
--     ${1|a,b,c|}          a choice (renders its first alternative for now)
--     $1 … $1              a mirror — repeat a number, every copy stays in sync
--
-- Unsupported constructs (variables like $TM_FILENAME, regex transforms) fail loud
-- rather than inserting raw `$1` text — the project's no-silent-stubs rule.

-- Jump keys (the defaults shown; change them here if you like):
nx.snippet.setup { jump_next = "<Tab>", jump_prev = "<S-Tab>" }

-- Register snippets per filetype. `body` is a string (function bodies are a later
-- phase). The sample file is `sample.lua`, so these are registered for `lua`.
nx.snippet.add("lua", {
  { trigger = "fn", body = "function ${1:name}(${2})\n\t$0\nend" },
  { trigger = "for", body = "for ${1:i} = ${2:1}, ${3:n} do\n\t$0\nend" },
  { trigger = "if", body = "if ${1:cond} then\n\t$0\nend" },
  -- A mirror: the local name is echoed in the assignment.
  { trigger = "loc", body = "local ${1:x} = ${1:x}" },
})

-- Offer the snippets as completion candidates (alongside buffer words). Type a
-- trigger, pick the row with <C-n>/<C-p>, accept with <C-y> — the body expands and
-- the cursor lands on the first tabstop. <Tab>/<S-Tab> move between tabstops.
nx.complete.setup {
  sources = {
    { "snippets" },
    { "buffer", min_chars = 2 },
  },
}

-- You can also expand a body directly, e.g. from an insert-mode mapping:
nx.keymap.set("i", "<C-s>", function()
  nx.snippet.expand("print(${1:value})$0")
end, { desc = "expand a print() snippet" })
