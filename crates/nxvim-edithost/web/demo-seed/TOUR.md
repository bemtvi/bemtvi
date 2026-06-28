# Welcome to nxvim — running in your browser

This is the full nxvim editor compiled to WebAssembly, with a real Python
toolchain running **entirely client-side**. No server, nothing installed —
the interpreter, the language server, the syntax highlighting and the
plugins all run in this tab.

## Try it

- `:terminal`                 — a minimal shell: `ls`, `cat main.py`, `cd`, pipes
  (`cat geometry.py | python -c "import sys; print(len(sys.stdin.read()))"`),
  redirects (`echo hi > note.txt`); run the project with `python main.py`
- `:terminal python`          — an interactive REPL (`Ctrl-C` interrupts a loop)
- open `main.py` / `geometry.py` — `K` hovers, `gd` goes to a definition,
  `grn` renames; basedpyright type-checks as you type
- start typing in insert mode — autocomplete pops up (`<C-n>`/`<Tab>` to move,
  `<CR>` to accept, `<C-e>` to dismiss), with LSP suggestions and docs
- `<space>` (leader), then wait — which-key shows what each key does
- `<leader>e`                 — toggle the file-tree sidebar
- `<C-w><C-w>j`               — drop into the bottom panel (it autohides, so it
  collapses to a `▸PANEL` chip when you leave; click the chip to bring it back)
- open `shapes.py`, then `:NxDiffConflict` — it carries a real git merge conflict;
  see it side-by-side as a 3-way **ours | base | theirs** diff. Step conflicts with
  `]c`/`[c`; resolve the one under the cursor with `co` (ours), `ct` (theirs) or
  `cb` (both). After you resolve it, `:NxDiffGit` diffs your edits against HEAD

## What's here

- `main.py`      — the entry point; imports the library below
- `geometry.py`  — a small typed module (a `Circle` dataclass + helpers)
- `shapes.py`    — a `Triangle` with an unresolved merge conflict (`:NxDiffConflict`)
- `init.lua`     — this editor's config (catppuccin, the plugins, the LSP)

Edit anything — your changes persist in the browser across reloads.
