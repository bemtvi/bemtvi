# Prebuilt grammar `.wasm` — the one committed binary

Everything else under `treesitter/` is generated: `node_modules/` and `vendor/` are
gitignored and rebuilt from the pinned `package.json` by `build.sh`. These two files are
the deliberate exception — **committed binaries** — because the grammar they hold has no
prebuilt WebAssembly anywhere upstream.

`@tree-sitter-grammars/tree-sitter-markdown` ships `src/parser.c` + `src/scanner.c` and
native Node addons, but **no `.wasm`** (contrast `tree-sitter-rust`, which ships
`tree-sitter-rust.wasm` — that is what every other bundled language copies). The old
`tree-sitter-wasms` bundle has no markdown either. So the registry's normal path —
copy the package's prebuilt wasm at build time, fetch the same file from jsDelivr for
`:TSInstall` — cannot reach markdown at all. Compiling it needs emscripten *and* the
tree-sitter CLI, which the ordinary `npm ci` build must not require; committing the
output is what keeps markdown in the offline bundle.

Markdown is two grammars, not one: `markdown` parses block structure and hands every
`(inline)` node to `markdown_inline` through an injection (see `web/highlight.js`), so
both parsers are needed for prose to highlight.

| file                    | grammar dir in the npm package |
| ----------------------- | ------------------------------ |
| `markdown.wasm`         | `tree-sitter-markdown`         |
| `markdown_inline.wasm`  | `tree-sitter-markdown-inline`  |

## Rebuilding

Only needed when the pinned `@tree-sitter-grammars/tree-sitter-markdown` version in
`../package.json` (and `VERSIONS` in `web/grammars.js`) changes:

```sh
cd ..                      # crates/bemtvi-edithost/treesitter
npm ci                     # unpack the pinned grammar sources
./scripts/build-prebuilt-wasm.sh
```

Needs `emcc` (emsdk or the system emscripten package) and the `tree-sitter` CLI on PATH —
`cargo install tree-sitter-cli` or `npx tree-sitter-cli@0.26.3`. Commit the changed
`.wasm` files, then regenerate the vendored assets (`rm -rf vendor && npm run
build:treesitter`) so the queries stay matched to the parser.
