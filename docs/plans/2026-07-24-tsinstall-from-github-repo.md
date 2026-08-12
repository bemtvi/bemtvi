# `:TSInstall owner/repo` — install arbitrary GitHub grammars + honor their declared file-types

## Motivation

Two linked asks:

1. `:TSInstall user/repo` should install a tree-sitter grammar straight from a
   GitHub repo, not only from nvim-treesitter's curated `parsers.lua` catalog.
2. Grammars declare which file extensions they apply to (modern grammars ship a
   `tree-sitter.json` with `grammars[].file-types`, e.g. `tree-sitter-ruby →
   ["rb"]`; older ones use a `tree-sitter` array in `package.json`). bemtvi
   currently ignores these — extension→language detection comes *only* from the
   static `EXT_FILETYPE` const in `bemtvi-core`. A grammar installed from an
   arbitrary repo has no catalog entry and no `EXT_FILETYPE` row, so bemtvi would
   compile it but never auto-detect the files it highlights.

The canonical fix for (2) is what makes (1) actually useful: read the grammar's
declared `file-types` on install and register them, persisted, so opening a file
of that type detects the filetype and highlights it.

## Decisions (confirmed with the requester)

- **File-type wiring: auto-register + persist.** On install, read the grammar's
  declared `file-types` and register them ext→lang in a persisted data-dir
  manifest so it survives restart. A grammar that declares *no* file-types fails
  loud with guidance (pass `:set ft=` / a future override), never silently.
- **Repo syntax: `owner/repo` + `owner/repo@ref`.** `@ref` pins a branch/tag/sha;
  default ref is `HEAD` (GitHub archive resolves it to the default branch — no
  extra API call). Grammar name and monorepo subdir are auto-detected from
  `tree-sitter.json`.

## Constraints (fail loud, no silent stubs)

- We cannot run `tree-sitter generate`; the repo must ship a pre-generated
  `src/parser.c` (same rule the catalog path already enforces).
- The browser/wasm build installs *prebuilt wasm* grammars from a CDN and has no
  C compiler — an arbitrary repo can't be compiled there. The wasm `:TSInstall`
  arm must reject a repo spec loudly ("not available in the browser build"),
  not silently no-op.

## Phases

### Phase 1 — repo install mechanics (native) + read declared file-types

- Parse the spec in `bemtvi_ts::install::install`: a token containing `/` is a
  repo spec (`owner/repo[@ref]`), else today's catalog language.
- New `install_from_repo`:
  - Fetch `https://github.com/{o}/{r}/archive/{ref}.tar.gz` (ref default `HEAD`),
    unpack, `single_subdir` (reuse existing helpers + `$BEMTVI_TS_MIRROR` seam).
  - Read grammar metadata: `tree-sitter.json` (`grammars[]`: `name`, `file-types`,
    `path`), falling back to `package.json`'s `tree-sitter` array. Derive the
    language from `name`, else the repo name minus a `tree-sitter-` prefix
    (`-`→`_`). Locate the grammar dir via the entry's `path` (default `.`).
  - Compile `<path>/src/parser.c` (+ scanner) → `<data>/parser/<lang>.so`
    (reuse `resolve_compiler` + `compile`).
  - Copy the repo's *own* `<path>/queries/*.scm` (QUERY_FILES basenames) into
    `<data>/queries/<lang>/`. Custom-repo inherits are not chased (note it).
  - Return `InstallReport` with a new `file_types: Vec<String>` (declared) and
    `revision = ref`.
- `InstallReport` gains `file_types`; the catalog path sets it empty. The install
  echo reports the declared file-types.
- wasm `:TSInstall` arm rejects a repo spec loudly.
- Tests (hermetic, black-box via `$BEMTVI_TS_MIRROR`): a fixture repo tarball
  (reusing tree-sitter-rust's `src/` for a real `parser.c`) plus a
  `tree-sitter.json` declaring `name`/`file-types`; assert the parser compiles,
  the grammar loads, and the report carries the declared file-types. A parse-shape
  test for the spec (`owner/repo`, `@ref`) and the metadata reader.

**Pause for review.** After Phase 1 a repo grammar installs and highlights via
`:set ft=<lang>`; auto-detection lands in Phase 2.

### Phase 2 — persisted dynamic filetype registry (auto-detection)

- New per-data-dir manifest of ext→lang registrations harvested from installs
  (the `file_types` field). Loaded at boot, extended on each completed install.
- `bemtvi-core` `language_of_path` / `buffer_filetype` consult this dynamic layer
  *over* the static `EXT_FILETYPE` const (const remains the built-in base; the
  registry adds custom grammars). Single new read seam; no read-site rewrite
  beyond threading the registry into the core.
- `on_install_done` registers `report.file_types` → `report.lang` and persists;
  a grammar with no declared file-types fails loud per the decision.
- Tier-1 parity: works over the daemon and (for the *registration* half; wasm
  can't compile repos) mirror the persisted state through the existing shada /
  OPFS split as needed.

### Phase 3 — surfacing & polish

- `:TSInstallInfo` lists custom grammars and their registered file-types.
- Consider an explicit `btv.filetype.add` / `vim.filetype.add`-style API so config
  can register ext→lang without an install (the same registry, public surface).
- Docs (the `btv` book page for treesitter) + an `examples/` walkthrough.

## Key files

- `crates/bemtvi-ts/src/install.rs` — install pipeline; add `install_from_repo`,
  spec parsing, `tree-sitter.json`/`package.json` reader, `InstallReport.file_types`.
- `crates/bemtvi-server/src/excmd.rs` — `ts_install` arms; wasm repo-spec guard;
  echo declared file-types; (Phase 2) register + persist in `on_install_done`.
- `crates/bemtvi-core/src/editor/mod.rs` — (Phase 2) dynamic ext→lang registry over
  `EXT_FILETYPE`, consulted by `language_of_path` / `buffer_filetype`.
- `crates/bemtvi/tests/ts_install.rs` — fixtures + black-box coverage.
