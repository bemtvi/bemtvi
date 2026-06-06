# LSP end-to-end suite

This directory installs the **real** language-server binaries that
[`crates/nxvim/tests/lsp_e2e.rs`](../../crates/nxvim/tests/lsp_e2e.rs) drives. That
test answers a specific question — *does nxvim's nvim-lspconfig support actually
work with real servers?* — by, for each of the ten most popular servers:

1. laying down a real mini-project with a **deliberate** error
   (`crates/nxvim/tests/fixtures/lsp-e2e/<server>/`),
2. configuring the server **only** through the vendored
   [`vendor/nvim-lspconfig`](../../vendor/nvim-lspconfig) — an `init.lua` that does
   nothing but `vim.lsp.enable('<server>')` (+ optional `settings`), so the
   vendored `lsp/<server>.lua` resolves the `cmd`, `filetypes`, and root, and
3. opening the file with the real server and asserting the real
   `textDocument/publishDiagnostics` surfaces in nxvim.

A diagnostic arriving is end-to-end proof of the whole round trip: `initialize` →
`didOpen` → the server analysing real code → `publishDiagnostics` → nxvim
projecting it into the view.

## The ten servers

| config (nvim-lspconfig) | binary | source | hash verification |
|---|---|---|---|
| `pyright` | `pyright-langserver` | npm | `npm ci` integrity (package-lock.json) |
| `ts_ls` | `typescript-language-server` | npm | `npm ci` integrity |
| `lua_ls` | `lua-language-server` | GitHub release | pinned sha256 |
| `rust_analyzer` | `rust-analyzer` | GitHub release | pinned sha256 |
| `gopls` | `gopls` | `go install` | pinned module checksum (`Sum`) |
| `clangd` | `clangd` | GitHub release | pinned sha256 |
| `bashls` | `bash-language-server` | npm | `npm ci` integrity |
| `jsonls` | `vscode-json-language-server` | npm (vscode-langservers-extracted) | `npm ci` integrity |
| `yamlls` | `yaml-language-server` | npm | `npm ci` integrity |
| `zls` | `zls` | GitHub release | pinned sha256 |

> **Why zls and not eslint?** The original target list's tenth server was
> `eslint`. `vscode-eslint-language-server` only ever validates after a
> VS Code-specific server→client handshake (dynamic registration plus bespoke
> `eslint/*` requests) that nxvim's deliberately-minimal LSP client doesn't
> implement — it initializes and accepts `didOpen` but never lints headlessly.
> Rather than ship a permanently-red case, the suite drives **zls** (the Zig
> language server), another extremely popular, single-binary server that reports
> diagnostics over plain LSP. The eslint gap is a real, known limitation of
> nxvim's client.

Exact versions live in [`manifest.json`](manifest.json) (single binaries + gopls)
and [`npm/package.json`](npm/package.json) (node servers). The sha256s in
`manifest.json` are pinned for the two host arches the suite supports:
**darwin-arm64** and **linux-x86_64**.

## Host requirements

The installer downloads what it can and hash-verifies everything, but a few
toolchains must already be present (the "true top-10" includes node- and
go-based servers):

- `curl`, `jq`, `tar` (with `xz` support), `unzip`, `gzip` — for the
  GitHub-release binaries (`zls` ships as `.tar.xz`).
- **node + npm** — to run the five npm-based servers (pyright, ts, bash, json,
  yaml).
- **go** — `gopls` is built with `go install` at the pinned version.
- **shellcheck** on `$PATH` — `bash-language-server` produces diagnostics through
  it; without it the bashls case has nothing to report.

## Usage

```sh
# 1. Download + hash-verify + install all ten servers into ./.lsp-e2e (git-ignored)
scripts/lsp-e2e/lsp-e2e.sh install

# 2. Put them on PATH (the script prints this exact line)
export PATH="$PWD/.lsp-e2e/bin:$PATH"

# 3. Run the gated suite (serialized; --nocapture shows per-server progress)
NXVIM_LSP_E2E=1 cargo test -p nxvim --test lsp_e2e -- --nocapture --test-threads=1
```

Without `NXVIM_LSP_E2E=1` every case is a passing no-op, so a normal
`cargo test --workspace` never touches a real server.

Other subcommands:

```sh
scripts/lsp-e2e/lsp-e2e.sh doctor      # report which servers are installed
scripts/lsp-e2e/lsp-e2e.sh print-bin   # print the install bin dir
scripts/lsp-e2e/lsp-e2e.sh install <name>...   # install a subset (e.g. clangd gopls)
```

## Bumping a version (maintainers)

1. Edit the version in `manifest.json` (single binaries / gopls) and/or
   `npm/package.json` (node servers).
2. Re-derive the hashes + lockfiles:
   ```sh
   scripts/lsp-e2e/lsp-e2e.sh lock
   ```
   This downloads each single-binary asset for **both** darwin-arm64 and
   linux-x86_64, rewrites their sha256 in `manifest.json`, refreshes the gopls
   module sum, and regenerates the npm lockfiles.
3. Review the `manifest.json` / `package-lock.json` diff, then commit.

## No treesitter grammars required

The suite asserts on LSP diagnostics, never on syntax highlighting. nxvim's
filetype detection is a pure file-extension table
(`nxvim_server::filetype_of`) with no treesitter involvement, and the syntax
worker degrades gracefully when a grammar is missing. So a CI host with zero
grammars installed runs this suite fine.
