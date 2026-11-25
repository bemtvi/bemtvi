# Installing treesitter grammars

`:TSInstall` makes a language's treesitter grammar available: its parser plus the
queries that drive highlighting, indentation, folds and text objects. A bare
`:TSInstall` installs the grammar for the file you're editing.

```vim
:TSInstall rust               " the catalog grammar, at its pinned revision
:TSInstall rust@HEAD          " the same grammar repo at a ref, remembered
:TSInstall acme/tree-sitter-x " a grammar straight from a GitHub repo (owner/repo[@ref])
:TSUpdate rust                " reinstall: at the remembered ref if there is one
:TSInstallInfo                " list what's installed, and the ref each follows
```

## Where a grammar comes from

The catalog is [nvim-treesitter](https://github.com/nvim-treesitter/nvim-treesitter),
pinned to one commit. That commit names every grammar's repository and the exact
revision to build, and holds the queries that match those revisions, so an install is
reproducible. Text objects come from nvim-treesitter-textobjects, pinned alongside it.

The native build downloads the grammar's source and compiles it into the data dir
(`stdpath("data")`, or `$BEMTVI_DATA_DIR`), off the editor thread. You don't need a
toolchain: bemtvi fetches a pinned, checksum-verified Zig the first time it needs a C
compiler (`$BEMTVI_CC` names your own instead).

Some grammar repositories commit only `grammar.js`, not the generated parser
(nvim-treesitter marks them `generate = true`: latex, perl, swift, teal, …). bemtvi
generates those itself, with no `tree-sitter` CLI and no `node`: an embedded JavaScript
engine evaluates `grammar.js`, and tree-sitter's own generator writes the parser.
`grammar.js` can `require` files from its own repository, which is how a monorepo
grammar shares rules, but nothing outside it: a grammar that needs an npm package or a
node built-in fails, naming the module, and one that runs longer than 60 seconds is
stopped. To use a `tree-sitter` CLI anyway, set `$BEMTVI_TREE_SITTER_CLI` to it.

## Following upstream: `lang@ref`

`:TSInstall lua@HEAD` builds the catalog grammar's repository at `HEAD` (or any branch,
tag or sha) instead of the pinned revision. The message names the commit it built, and
the ref is remembered, so `:TSUpdate lua` builds `HEAD` again. A plain `:TSInstall lua`
goes back to the pinned revision and forgets the ref.

A sha is fetched from GitHub's archive of the grammar's repository, and GitHub serves
commits pushed to any fork of it there too. Install a sha only from a source you trust.

The queries stay nvim-treesitter's pinned set. A grammar ahead of them can rename a
node a query uses, and then that query fails to compile with an error naming the node.
That is the cost of following `HEAD`.

## In the browser

The browser build can't compile C, so it installs **prebuilt** `.wasm` grammars from
their npm packages (through jsDelivr) and caches them in the browser's private storage.
A few grammars are bundled for offline use.

- `:TSInstall markdown` fetches markdown's two parsers (`markdown` and the
  `markdown_inline` it injects) from bemtvi's own repository, since no npm package
  publishes them.
- `@ref` names an npm version or dist-tag: `:TSInstall lua@latest`, `:TSInstall
  lua@0.4.0`. `@HEAD` means `latest`, the nearest thing npm has to following upstream; a
  git sha is refused, since a published package has no commits to pick from. The version
  is remembered for `:TSUpdate`, as natively.
- `owner/repo` installs need a C compiler, so the browser refuses them.
