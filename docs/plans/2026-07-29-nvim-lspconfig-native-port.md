# Porting nvim-lspconfig to nxvim — the real, total, async port

**Repo under port:** `~/work/nxvim-plugins/nxvim-lspconfig` (a fork of
`neovim/nvim-lspconfig` @ `b7b92094`), which now claims the `nxvim/nxvim-lspconfig`
name. The earlier 22-server hand-written port has been renamed to
`nxvim/nxvim-lspconfig-base` (`~/work/nxvim-plugins/nxvim-lspconfig-base`) and is
**superseded** by this work.

**Goal:** every one of the 407 upstream server configs runs on nxvim's own
surfaces — `nx.lsp` / `nx.fs` / `nx.run` / `nx.utils` / `nx.command` / `nx.ui` —
with **no blocking I/O anywhere** and no `vim.*` beyond the ADR 0002 whitelist.
Not a compat shim: nvim-lspconfig is treated as a *behavioral spec to
reimplement* (design principle: "There is no neovim compatibility — port
behavior natively").

---

## 1. What upstream actually is, measured

`lsp/*.lua` — 407 files, each returning one `vim.lsp.Config` table. Split by shape:

| shape | count | port cost |
| --- | ---: | --- |
| pure data (`cmd`/`filetypes`/`root_markers`/`settings`/`init_options` only) | 284 | mechanical |
| carries code (a `cmd` builder, `root_dir` fn, `on_attach`, `on_init`, …) | 114 | by hand |
| code but no `vim.*` (self-contained helpers) | 9 | near-mechanical |

Top-level keys in use, by frequency:

```
cmd 399   filetypes 392   root_markers 337   settings 83   root_dir 60
init_options 52   on_attach 25   workspace_required 16   capabilities 16
on_init 10   get_language_id 8   before_init 7   handlers 6   cmd_env 5(7 files)
reuse_client 3   commands 2   offset_encoding 2   name 5
```

`lua/lspconfig/` (the deprecated `require('lspconfig').xxx.setup{}` "framework",
`configs/`, `manager.lua`, `async.lua`, `health.lua`, `ui/windows.lua`) and
`plugin/lspconfig.lua` are **dropped wholesale** — they are neovim-internals
plumbing (`vim.lsp.config._configs`, `vim.uv.new_timer`, `vim.deprecate`,
`:checkhealth`) with no nxvim analogue and no reason to exist here. The user
commands they define are re-implemented natively.

## 2. The translation table (upstream API → nxvim native)

| upstream | nxvim | status |
| --- | --- | --- |
| `vim.fs.root(bufnr, markers)` | `util.root(bufnr, markers)` → **promise** | new (plugin), async |
| `vim.fs.find(n, {upward})` | `util.find_upward(bufnr, names)` → promise | new (plugin) |
| `vim.fs.dirname` / `basename` | `nx.utils.dirname` / `basename` | ✅ exists |
| `vim.fs.parents` | `nx.utils.ancestors` | ✅ exists |
| `vim.fs.joinpath` | `nx.utils.joinpath` | **new (core prelude)** |
| `vim.fs.normalize` | `nx.utils.normalize` | **new (core prelude)** |
| `vim.fs.relpath` | `nx.utils.relpath` | **new (core prelude)** |
| `vim.fn.executable` / `exepath` | `nx.fs.which(name)` → promise | **new (core, cross-crate)** |
| `vim.uv.fs_stat` | `nx.fs.stat` / `nx.fs.exists` | ✅ exists, async |
| `vim.system(cmd)` | `nx.run{}` → promise | ✅ exists |
| `vim.lsp.rpc.start(argv, d)` | `cmd` fn just **returns the argv** | drop the wrapper |
| `vim.api.nvim_buf_get_name` | `nx.buf.name(bufnr)` | ✅ exists |
| `nvim_buf_create_user_command` | `nx.user_command.buf_create` | ✅ exists |
| `vim.notify` | `nx.notify` | ✅ exists |
| `vim.lsp.get_clients` | `nx.lsp.clients` | ✅ exists |
| `vim.json.decode` | `nx.json.decode` | ✅ exists |
| `vim.ui.select` / `input` | `nx.ui.select` / `nx.ui.input` (promise) | ✅ exists |
| `vim.keymap.set` | `nx.keymap.set` | ✅ exists |
| `vim.cmd.edit` | `nx.cmd("edit …")` | ✅ exists |
| `vim.uri_from_bufnr` / `uri_to_fname` | `nx.uri.*` | **new (plugin util)** |
| `vim.fn.has('nvim-0.11.x')` | deleted — nxvim is not versioned by nvim | n/a |
| `vim.deprecate` | deleted | n/a |

## 3. Gaps in `nx.lsp` this port must close (Phase 1)

`nx.lsp`'s resolved-config reader (`crates/nxvim-lua/src/prelude/lsp.lua`)
consumes `cmd` / `filetypes` / `root_markers` / `root_dir` / `init_options` /
`settings` / `capabilities` / `on_attach` / `on_init` / `on_exit`. Everything
else a config writes today is **silently ignored** — which for
`workspace_required` means a server that must have a project root starts
rootlessly instead. Silent-wrong, so these are core fixes, not plugin
workarounds (CLAUDE.md: *fix at the canonical layer*).

1. **Nested `root_markers` priority tiers.** `{ {a, b}, {'.git'} }` means "try
   a/b at every level first, only then `.git`". `find_root` currently does
   `present[m]` with `m` a *table* → never matches → the whole tier silently
   never fires. 13 configs use the form.
2. **`workspace_required`** (16 configs) — no root ⇒ do not start. Lua-side in
   `start_for`.
3. **`single_file_support`** — the inverse default. Lua-side.
4. **`get_language_id(bufnr, ft)`** (8 configs) — the `didOpen` `languageId`.
   The filetype already crosses `nx._lsp_start` as `ft`; apply the hook to it
   Lua-side. No Rust change.
5. **`before_init(init_params, config)`** (7 configs) — mutates
   `initializationOptions` before `initialize`. `nx.lsp` builds `init_options`
   in Lua, so this is emulable Lua-side against a synthetic `init_params`.
6. **`cmd_env`** (7 configs) — environment for the spawned server. **Rust,
   cross-crate**: `LspOp::Start { env }` → `ServerSpawn.env` →
   `Command::envs()` in `nxvim-lsp/src/transport.rs`, and the daemon leg.
7. **`name`** — a config whose registry key differs from its client name.
8. **`reuse_client`** / **`offset_encoding`** / per-config **`handlers`** —
   assessed in Phase 1; anything not modellable in this pass is **rejected loud
   at config-load time** rather than silently dropped (CLAUDE.md: no silent
   stubs). A config carrying an unsupported key must say so.

Plus `nx.fs.which` (Rust, cross-crate: `FsJob::Which` through `install.rs`
native + `fswire.rs` daemon leg + the wasm Worker — the tier-1 remote rule), and
`nx.utils.joinpath` / `normalize` / `relpath` (pure Lua, prelude — CLAUDE.md
puts path munging in public `nx.utils.*`).

Phase 4 surfaced five more, closed in `128eb1e2`: `nx.cwd()`, `nx.stdpath(what)`,
`nx.pid()`, `nx.version()` — editor and host facts previously reachable only
through `vim.fn`, which ADR 0002 keeps off the whitelist — and the two JSON values
Lua cannot express, `nx.json.null` and `nx.json.empty_object()`. The JSON pair is
the load-bearing one: an `init_options` that means `{}` or `null` and encodes as
`[]` or a missing key is a *different message* to the server, and the failure looks
like "the server started but does nothing" rather than an error.

## 4. Phases

Each phase is committed separately and paused for review.

- **Phase 1 — close the canonical gaps.** `nx.utils` path helpers; `nx.fs.which`
  end-to-end (native + daemon + wasm); `nx.lsp` config-key support per §3 with a
  loud rejection for anything still unmodelled. Harness tests in
  `crates/nxvim-server/tests/`. *No plugin code yet.*
- **Phase 2 — the plugin skeleton.** Strip `lua/lspconfig/`, `plugin/`, `test/`,
  `scripts/`, the rockspec and CI. **(Overreached — see Phase 5: `scripts/docgen.lua`
  and the `doc/configs.*` it builds are the per-server *docs*, not plumbing, and
  went out with the plumbing.)** Write `lua/nxvim-lspconfig/util.lua` (the
  async native helpers of §2) and `lua/nxvim-lspconfig/init.lua` (`setup()`,
  carried forward from `-base` as an additive convenience over the primary
  `nx.lsp.enable` path). Native `:LspStart` / `:LspStop` / `:LspRestart` /
  `:LspInfo` / `:LspLog` via `nx.command`.
- **Phase 3 — the 284 pure-data configs.** Scripted rewrite: strip the
  `---@type vim.lsp.Config` / `lspconfig.settings.*` annotations, rewrite
  `vim.lsp.config(…)` doc examples to `nx.lsp.config(…)`, verify every file
  still loads and returns a table with the same data.
- **Phase 4 — the 114 code-bearing configs.** Hand-ported in batches, grouped by
  the pattern they share (`node_modules/.bin` cmd resolvers; `vim.system`
  root probes; `on_attach` buffer commands; `handlers`). Each batch commits.
  - *Pass 1* (`7e0614da`) — the mechanical renames, the 14 nvim-version gates, the
    15 `node_modules/.bin` cmd builders. 387/407 configs loading.
  - *Pass 2* (`2c918aa2`) — everything that could not be renamed because upstream
    wrote it against blocking I/O: the ~50 `vim.fs.root`/`find` upward probes, the
    `vim.system(…):wait()` tool queries, `vim.fn.executable`/`exepath`,
    `io.open`/`io.lines`, the 9 `vim.lsp.rpc.start` wrappers, gitlab_duo's `curl`
    OAuth flow. **407/407 loading, and every `root_dir` / `cmd` / `before_init` in
    the repo runs to completion.** `util.root_pattern` gained upstream's
    pattern-order priority; four load-time-frozen values moved to `before_init` /
    a `cmd` builder.
  - *Pass 3* (core `b2cda7cd`) — the **client-side LSP helper surface**, and the eight
    configs that needed it (texlab, clangd, ccls, ocamllsp, ts_ls, denols, eslint,
    stylelint_lsp). Measured, it was larger than "nine configs carrying `vim.*`":
    **21** configs called `client:exec_cmd` / `client:supports_method` /
    `client.offset_encoding`, none of which existed on the handle, and three called the
    BLOCKING `client:request_sync`.
    - Core (`nx.lsp`): `client.offset_encoding` — readable AND writable, the write
      re-negotiating the live client through `LspOp::SetOffsetEncoding` so clangd's
      non-standard top-level `offsetEncoding` actually takes effect; `supports_method`
      (a method no capability describes ⇒ true, since a server's own extension is
      exactly what these configs call); `exec_cmd`, sharing one precedence
      implementation with `_dispatch_command`; `client_by_id`; `position_params` /
      `text_document_params`; `locations_to_items` → a **promise** of quickfix items
      (an item quotes its source line, so a location into an unopened file is I/O);
      and `nx.utils.uri_from_path` / `uri_from_buf` / `uri_to_path`. The config key
      `offset_encoding` moved off the unsupported list.
    - Plugin: `open_floating_preview` → `nx.ui.float` (a non-persistent float already
      IS "transient, unfocusable, sized to its contents"); `locations_to_items` +
      `setqflist` → `nx.lsp.locations_to_items` + `nx.qf`; `vim.cmd.edit(uri_to_path)`
      → `nx.lsp.show_document` (reuses a buffer opened under a relative name).
      `:LspEslintFixAll` / `:LspStylelintFixAll` became the STANDARD
      `source.fixAll.<server>` code action instead of each server's private command +
      a hand-carried document version over `request_sync` — same operation, over the
      path nxvim already implements. ts_ls' source-action command likewise asks for the
      kind family (`only = { "source" }`, hierarchical) rather than enumerating the
      server's advertised kinds. Two configs' `handlers` were **deleted** rather than
      left as code that cannot run (`nx.lsp` does not route server-initiated messages
      into Lua), each documented in its `---@brief`: denols' `deno:` virtual documents
      and ts_ls' `_typescript.rename` follow-up.
    - Also fixed: three pass-2 call sites passing the encoding **positionally** to
      `nx.lsp.show_document` / `apply_workspace_edit` (neovim's signature, not nxvim's),
      which silently defaulted to utf-16.
    - Tests: `crates/nxvim/tests/lsp_client_api.rs` (11, over the mock server — the
      encoding write-back is asserted on the wire, via a `didChange` range past a
      2-byte character), and the plugin's own `test/` suite run by
      `nxvim --test-plugin` (15): every config loads, every `cmd` / `root_dir` /
      `before_init` / `get_language_id` runs with the arguments `nx.lsp` really passes,
      and every reworked `on_attach` is entered and its commands invoked against a
      recording client.
    - **407/407 loading, and zero `vim.*` left in the repo outside doc comments.**
- **Phase 5 — verification & ship.** Harness tests over a representative slice
  (data config loads, root-tier resolution, `which`-based local-cmd resolution,
  a `workspace_required` server declining to start); `examples/`; README +
  vimdoc regeneration; bump the `nxvim/nxvim-lspconfig` pin in
  `crates/nxvim-edithost/build-plugins.sh` (**currently pinned at `e9d13fff`,
  a SHA that now lives only in `nxvim-lspconfig-base` — the rename broke it**).
  - Measured against what already existed, the *core* half of the slice was
    covered: `lsp_config_keys.rs` (12) has the root-marker tiers and
    `workspace_required`, `utils_paths.rs` has `nx.fs.which`, and the plugin's
    `configs_spec.lua` loads and runs all 407. The gap was the **plugin's own
    helper surface** — `configs_spec` runs the configs against whatever tree the
    test sits in, where nothing has a `node_modules`, a lockfile, or two competing
    markers at different depths, so every probe takes its "found nothing" path and
    the helpers never make a *decision*. `test/util_spec.lua` (22) builds the trees
    those decisions need: `node_cmd` preferring the project's own binary (and
    falling back when it isn't executable), `root_pattern`/`root_of_path` priority
    proven **both ways round** on one tree, `find_upward_all`'s nearest-first order
    with `limit`/`stop`, the manifest probes, `get_typescript_server_path`,
    percent-encoded URIs, `tabsize`. Mutation-tested: swapping the two priority
    loops, dropping the `local_bin` preference, and ignoring `opts.stop` each fail
    exactly their own test and nothing else.
  - `examples/lspconfig/` (`init.lua` + `sample.lua`), verified end-to-end against a
    real `lua-language-server`: attach, the `undefined_global` diagnostic firing
    while `nx` does **not** (the `diagnostics.globals` override taking), the nine
    plugin keymaps installed buffer-local, `:LspInfo` showing the `.git`-tier root,
    407 available, `for_filetype("lua")`, and `:LspStop`.
  - Doc pipeline per `WRITING-VIMDOCS.md`: authored `doc/nxvim-lspconfig.md`,
    generated `doc/nxvim-lspconfig.txt` (23 tags), `scripts/gen-vimdoc.sh` +
    `check-vimdoc.sh` + the pre-push hook, README slimmed to intro/install/pointer.
    Reproducible (regenerating is byte-identical) and verified in the editor:
    `:help nxvim-lspconfig` opens the page, `:help nxvim-lspconfig-writing-a-config`
    lands on its heading.
  - **Corrections the verification forced.** (a) `single_file_support` was listed in
    §3 as a Phase 1 item and in `configs_spec`'s known-key set, but `nx.lsp` never
    wired it and **no config uses it** — removed from the spec rather than
    documented, so a config carrying it fails loud like any unknown key.
    (b) Pass 3's "zero `vim.*` outside doc comments" was not true of `plugin/`:
    five `vim.log.levels` → `nx.log.levels`. (c) The doc comments themselves were
    excluded from that claim, but 15 configs' `---@brief` blocks are **copy-paste
    snippets** telling users to run `vim.uv.fs_stat` (blocking), `vim.fn.stdpath`,
    `vim.filetype.add`, `vim.lsp.protocol.make_client_capabilities` and a
    `lua/lspconfig` path this port deleted. All rewritten natively — the
    `snippetSupport` trio (html/cssls/jsonls) as a capability *delta* (config
    `capabilities` deep-merge over the base, and nxvim does **not** advertise
    `snippetSupport`, so that advice is load-bearing here too), and lua_ls/emmylua_ls
    retargeted at nxvim: **PUC 5.4, not LuaJIT**, and `globals = { 'nx', 'vim' }`.
    Only three `vim.*` mentions remain, all in our own prose quoting what upstream
    spelled.
  - **The per-server reference, restored.** Phase 2's wholesale `scripts/` strip took
    `scripts/docgen.lua` with it, and `doc/configs.md` (15,918 lines) +
    `doc/configs.txt` (11,990) went with `doc/`. Nothing noticed because `doc/` did
    not exist again until this phase — with the effect that every config's
    `---@brief` (install notes, what its settings mean, what it assumes) was
    reachable *only* by opening `lsp/<name>.lua`. The port had been curating prose
    that rendered nowhere, including the fifteen blocks rewritten above.
    `scripts/docgen.lua` is now a native port running under `nxvim --lua` against
    `nx.fs`/`nx.cwd`/`nx.await`, with two deliberate departures from upstream: a
    **tag per server** (`:help lspconfig-clangd` lands on clangd; upstream tagged
    only the page and left `gO` to find the rest), and **our own table renderer**
    rather than `inspect` — which prints a list as `{ 1 = "x" }`, not valid Lua on a
    page people copy from, and promises no key order. Keys are sorted, so output is
    byte-identical run to run. Determinism needed upstream's hack in nxvim spelling:
    the table is rendered by *loading* the config, and a handful read host facts at
    load time (`nx.stdpath`, `nx.version`, `nx.env.get`, `~`), so those are frozen to
    placeholders across the load — otherwise the committed page carries the
    generating machine's home directory. Verified: 433 help tags, and
    `lspconfig-clangd` / `lspconfig-rust_analyzer` / `lspconfig-all` each land on
    their section. `check-vimdoc.sh` covers both generators.
  - **A core bug the example surfaced** (`crates/nxvim/tests/lsp_lifecycle.rs`, 2):
    an explicit `:LspStop` left `nx.lsp.clients()` listing a server `:LspInfo` said
    was not running. `stop_lsp_servers`/`restart_lsp_servers` removed the record
    from `lsp_servers` *before* the asynchronous `ServerExited` arrived, and that
    event handler was the only place the exit path lived — so it found nothing to
    retire and the config's `on_exit` never ran, `LspDetach` never fired, and the
    Lua handle leaked. `:LspStop` with no argument derives its list from
    `nx.lsp.clients()`, so the phantom made the *next* `:LspStop` claim it stopped
    something already gone. Fixed at the canonical layer: one `retire_lsp_server`
    owns the whole exit path, is idempotent, and is run by *both* the event and the
    deliberate stop/restart (`drop_docs` distinguishing a stop that isn't coming
    back from a crash the breaker may respawn).

## 5. Non-goals

- No `require('lspconfig').server.setup{}` framework — deleted, not aliased.
- No `:checkhealth` port.
- No neovim-version gating anywhere.
