# Closing the `:TSInstall` gaps

**Status:** done (2026-11-25). All four phases landed on branch `tsinstall-gaps`, along
with three things found on the way:

- nvim-treesitter and nvim-treesitter-textobjects bumped to their latest commits
  (`52773f0d`, `5c7b0263`), checked by installing 36 grammars and compiling every query.
- On macOS 26 the fetched Zig linked no libc into a grammar (Zig 0.15 doesn't recognise
  the host), so every grammar with a scanner failed to install; Zig now gets an explicit
  `<arch>-macos` target.
- The browser's `:TSInstallInfo` shows the version a grammar follows, as natively.

One deviation from the design below: the browser fetches markdown at the last commit that
changed `treesitter/prebuilt/`, not at a `v<version>` tag, because no release tag exists
yet and a commit that changed the files is always on GitHub once pushed.

**Decisions** (David, 2026-11-25):

- **Generate in-process.** A grammar repo that commits `grammar.js` but no generated
  `src/parser.c` is generated inside bemtvi: an embedded QuickJS evaluates
  `grammar.js` and `tree-sitter-generate` writes the parser. No `tree-sitter` CLI and no
  `node`. `$BEMTVI_TREE_SITTER_CLI` stays as an override.
- **`lang@ref`, remembered.** `:TSInstall lua@HEAD` (or any branch, tag or sha) builds a
  catalog grammar at that ref instead of nvim-treesitter's pinned revision. The ref is
  recorded per language, so `:TSUpdate lua` follows it again; a plain `:TSInstall lua`
  goes back to the pinned revision and forgets it.
- **The browser fetches bemtvi's own markdown wasm.** `:TSInstall markdown` fetches the
  committed prebuilt parsers through jsDelivr's GitHub endpoint and caches them in OPFS
  like every other grammar, instead of reporting that nothing can be fetched.

**Goal.** Remove the three entries under *`:TSInstall` approximations* in
[known approximations](../known-approximations.md). That entry is also out of date:
it says grammars needing `tree-sitter generate` "fail loud rather than generating",
but `ensure_parser_c` already shells out to a `tree-sitter` CLI when one is on `PATH`,
and `owner/repo` installs already default to `HEAD`. What is really missing:

1. Generating needs a `tree-sitter` CLI the user installed themselves (and, for the CLI's
   default runtime, `node`). Eight catalog grammars at the pinned nvim-treesitter ref ship
   no `parser.c` and say `generate = true`: latex, mlir, ocamllex, perl, pod, swift, teal,
   unison. Without the CLI they can't be installed.
2. A catalog language can only be built at its pinned revision. `$BEMTVI_TS_REF` moves the
   whole catalog at once, and only from the environment.
3. In the browser, `:TSInstall markdown` can't fetch anything, since no npm package
   publishes a markdown `.wasm`.

## Background

- `bemtvi_ts::install` (`crates/bemtvi-ts/src/install.rs`) takes `lang` or
  `owner/repo[@ref]`. The catalog path reads nvim-treesitter's `parsers.lua` at
  `NVIM_TS_REF`, downloads the grammar tarball at the pinned revision, makes sure there
  is a `src/parser.c` (`ensure_parser_c`), compiles it with the pinned Zig (or
  `$BEMTVI_CC`), and copies the queries from the same nvim-treesitter ref.
- `:TSInstall` and `:TSUpdate` are the same command (`excmd.rs::ts_install`): both
  reinstall. Nothing about an install is recorded beyond the files it writes.
- The browser (`crates/bemtvi-edithost/web/highlight.js::install`) fetches prebuilt `.wasm`
  grammars from npm through jsDelivr (`CDN_BASE`) and queries from nvim-treesitter through
  jsDelivr's GitHub mirror (`GH_BASE`). Markdown's two parsers are `prebuilt` in
  `web/grammars.js`: compiled from C by `scripts/build-prebuilt-wasm.sh` and committed
  under `crates/bemtvi-edithost/treesitter/prebuilt/`.

## Design

### 1. In-process generation

`tree-sitter-generate` 0.26 can evaluate `grammar.js` with a bundled QuickJS (its `qjs-rt`
feature, the CLI's `--js-runtime native`). That feature turns on `rquickjs/bindgen`,
which needs libclang on every build machine, and nothing in the tree uses bindgen today.
rquickjs ships pregenerated bindings for all five release targets (x86_64/aarch64
Linux gnu, x86_64/aarch64 macOS, x86_64 Windows msvc), so:

- Depend on `tree-sitter-generate` with `default-features = false, features = ["load"]`
  (the directory API; `load` only adds `semver`).
- Depend on `rquickjs` directly, without `bindgen`.
- Evaluate `grammar.js` in our own small QuickJS host (`bemtvi-ts/src/generate.rs`),
  ported from `tree-sitter-generate`'s `quickjs.rs` with its `dsl.js` (MIT, credited in
  `scripts/gen-licenses.sh`). It writes `src/grammar.json`; then
  `generate_parser_in_directory` writes `parser.c`, `node-types.json` and the
  `tree_sitter/*.h` headers a scanner includes.
- `ensure_parser_c` order: a committed `parser.c` wins; then `$BEMTVI_TREE_SITTER_CLI` if
  set; then the in-process generator. A committed `src/grammar.json` with no `parser.c` is
  generated from directly, skipping QuickJS.
- Bounds: QuickJS gets a memory limit and a stack limit (as upstream). A grammar whose
  `grammar.js` `require`s an npm dependency that isn't in the tarball fails loud with the
  module name.

The ABI version passed is `tree_sitter::LANGUAGE_VERSION`, the one the linked runtime
loads.

### 2. `lang@ref`

- `install()` dispatches `owner/repo[@ref]` first (unchanged), then `lang@ref` (no `/`,
  one `@`), then `lang`.
- `lang@ref` resolves the catalog entry as usual and downloads the grammar at `ref`
  instead of `entry.revision`. Queries still come from the pinned nvim-treesitter ref, so
  the report says they may not match.
- The built commit: GitHub's archives are made by `git archive`, which writes the commit
  id into a pax global header. The installer reads it, so the report and the record show
  the commit `HEAD` resolved to; if it's missing, the ref is shown.
- The record: `<data>/parser-info/<lang>.json`, `{ "ref": "HEAD", "commit": "<sha>" }`,
  written only for a `lang@ref` install. A plain `lang` install deletes it.
- `:TSUpdate lang` (and a bare `:TSUpdate` on the current filetype) reads the record and
  reinstalls at the recorded ref. `:TSInstallInfo` shows it.
- The record lives beside the parser in the data dir, which is where the engine loads
  from in every session kind (a daemon session installs locally too), so no remote work.

### 3. Browser

- **Markdown.** A `prebuilt` grammar is fetched from
  `${GH_BASE}/bemtvi/bemtvi@v<version>/crates/bemtvi-edithost/treesitter/prebuilt/<file>`,
  where `<version>` is the edit-host's version, stamped into the JS at build. Between
  releases that is the last released tag, which exists. The bytes are checked by loading
  them as a `Language` and running the highlight query (the same check every install
  does), then cached in OPFS, where they win over the bundled copy. `:TSInstall markdown`
  installs both halves (`markdown_inline` is its injection).
- **`lang@ref`.** The browser installs published npm packages, not git refs, so there
  `@ref` names an npm version or dist-tag (`lua@latest`, `lua@0.4.0`); `@HEAD` is taken as
  `latest`, the closest equivalent. A ref that isn't a version or tag is rejected with
  that reason. The ref is recorded in the OPFS install manifest so `:TSUpdate` follows it.
  Queries for a refreshed grammar come from the same package version.

## Phases

1. **In-process generation** (native). Generator module, deps, licenses, `ensure_parser_c`
   order. Tests in `crates/bemtvi/tests/ts_install.rs`: the existing grammar.js test runs
   without a CLI (it no longer skips); a grammar.js that `require`s a missing module fails
   loud naming it; a repo with only `src/grammar.json` installs; a catalog `generate = true`
   entry installs from the mirror. Check by hand against real grammars (latex, swift,
   perl, teal) over the network.
2. **`lang@ref`** (native). Spec parsing, the pax commit, the record, `:TSUpdate` and
   `:TSInstallInfo`. Tests: `lua@HEAD`-style install builds the ref's tarball and records
   it; `:TSUpdate` follows the record; plain `:TSInstall` clears it; a bad ref fails loud.
3. **Browser.** Markdown fetch and `lang@ref` over npm. Version stamp in `build.sh`. A
   `verify-*.mjs` check against a local mirror (`__BEMTVI_TS_BASE` / `__BEMTVI_TS_GH_BASE`).
4. **Docs.** Rewrite the known-approximations entry, update the `:TSInstall` feature docs
   and architecture.md's treesitter-control paragraph.

## Risks

- **Binary size.** QuickJS and the generator add roughly 1–2 MB.
- **Generator time.** Big grammars (swift, latex) take seconds to generate; this runs on
  the install worker, off the editor thread, like the compile.
- **Grammars that need `npm install`.** A grammar that imports a base grammar from
  `node_modules` can't be evaluated from the tarball alone. Fails loud.
- **Queries vs a HEAD grammar.** A grammar at `HEAD` can rename nodes the pinned queries
  use; a query compile error then names the node. That's the cost of following `HEAD`.
