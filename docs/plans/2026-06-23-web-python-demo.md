# Plan: full in-browser python demo (Pyodide terminal + LSP, static site)

**Status:** ✅ COMPLETE — all phases (0–8) done; the demo is feature-complete
**Date:** 2026-06-23
**Owner thread:** web/edithost

## Goal

A self-contained, **static** web demo of bemtvi (no backend, deployable to Netlify) that
ships:

- a real, multi-file **python project** with a guided tour,
- a working **python interpreter** spawnable via `:terminal` (`python`, `python main.py`),
- a working **python LSP** (diagnostics / hover / completion / go-to-def),
- python **tree-sitter highlighting** out of the box,
- the recommended **first-party plugin set** (which-key, nvim-tree, lualine, lspconfig,
  diff) + **catppuccin**, pre-configured.

Everything runs **in the browser**:
- the **interpreter** is **Pyodide** (CPython 3.12 in wasm), driven by `:terminal python`;
- the **LSP** is **basedpyright** — a *JavaScript* language server (pyright fork). It runs
  as a plain JS bundle in a Web Worker with a virtual-filesystem shim — **no Pyodide
  needed for the LSP**. Pyodide is only the interpreter.

## Why this is tractable: the seams already exist

The wasm core already drives `:terminal`, async proc (`vim.system`/`jobstart`), and LSP
through uniform **off-tick request/reply seams**, today fulfilled by a daemon over
WebTransport:

| Leg      | drain export              | land exports |
|----------|---------------------------|--------------|
| terminal | `eh_take_terminal_requests` | `eh_terminal_data` / `eh_terminal_flush` / `eh_terminal_exit` |
| proc     | `eh_take_proc_requests`     | `eh_proc_spawned` / `eh_proc_stdout` / `eh_proc_exited` |
| lsp      | `eh_take_lsp_requests`      | `eh_lsp_stdout` / `eh_lsp_stderr` / `eh_lsp_exited` |

All three gate on one core flag, `Sink::daemon_connected` (`has_remote_proc` /
`has_remote_lsp` at `src/lib.rs:503,636`; terminal likewise). Critically,
`has_remote_fs()` is hardcoded `true` (`src/lib.rs:468`) — fs is **always** the OPFS
off-tick seam and does **not** depend on `daemon_connected`. So we can open the
proc/term/LSP gates for a *local* host without redirecting files to a (nonexistent)
daemon: the core only needs to know "a process host exists"; the **Worker** decides
*how* to fulfill each request (Pyodide instead of the wire).

**Therefore the core barely changes.** The bulk of the work is JS-side in the Worker: a
new **local process host** that fulfills the three seams using Pyodide, plus a
multi-file plugin-loading story, plus bundling/seeding/deploy.

## Spike results (2026-06-23) — both GO

**Pyodide interpreter — GREEN, no blockers.** Verified against Pyodide **314.0.0**
(CPython 3.14.2): streaming stdout via `setStdout({batched})` (line-by-line during a run),
interactive REPL via `pyodide.console.PyodideConsole` (`push(line)` →
incomplete/syntax-error/complete; `input()` via `stdin_callback`), interrupt via
`setInterruptBuffer(SAB)` (a `while True:` broke ~3 ms after writing SIGINT from the main
thread — needs cross-origin isolation, which we already have), and an OPFS-live FS via
`mountNativeFS('/project', dirHandle)` + `syncfs()` write-back. **Fully self-hostable**
(5 files under one `indexURL`, no CDN). Download ≈ **6.3 MB gzip** core (the
`python_stdlib.zip` is the incompressible floor) — lazy-load on first spawn, cache immutably.

**basedpyright LSP — GO, but build from source (not the npm bundle).** Node baseline
proved the checker itself is correct (real `publishDiagnostics` on a type error, hover,
196-item completion). BUT the published `dist/pyright-langserver.js` is a **closed webpack
IIFE that self-runs on Node stdio with zero exports** — unusable directly in a browser, and
it **reads exclusively through a filesystem** (won't type-check buffer text alone). The
working path (proven by prior art: monaco-pyright-lsp, Eric Traut's pyright-playground):
vendor **basedpyright git source** (`packages/pyright-internal`, *not* on npm), and bundle a
Worker entry that (1) uses `BrowserMessageReader/Writer` instead of the `--stdio` transport,
(2) backs pyright's `FileSystem` interface with an **in-memory FS** (memfs/ZenFS) holding
project files + **typeshed stdlib stubs** (≈0.7 MB gz), (3) ships a ~5-line `process` shim
+ no-op `child_process`/`net`/`worker_threads` shims, (4) subclasses `LanguageServerBase`
and overrides `createBackgroundAnalysis()→undefined` (single-threaded; disables
`worker_threads`). Bundle+stubs ≈ **1.6 MB gz**. *Effort is build-tooling/integration, not a
research unknown.* **Consequence for Phase 4:** it gets its own from-source TS build
pipeline, and the LSP Worker's in-memory FS must be fed buffer text on `didOpen`/`didChange`
(the editor already sends these) plus seeded with typeshed at boot.

## Build layout — the demo is a SEPARATE build from the standard editor

The python demo and the standard web editor are **separate build outputs**, not one build
with the demo baked in:
- `build.sh` → the **standard editor** in `web/` + `dist/`. No Pyodide;
  `web/build-config.js` ships `localHost: false`, so the Worker never installs the local
  process host (`:terminal` fails loud serverless, as before). The standard build is pristine.
- `build-demo.sh` → the **python demo** assembled into its own self-contained `demo-site/`
  (gitignored): the standard build PLUS Pyodide vendored in and a `build-config.js` flipped to
  `localHost: true`. Deployable side-by-side with the standard editor.

Shared source, one set of files; the only differences are the `build-config.js` flag and the
presence of `web/vendor/pyodide/`. Demo-only code lives in `web/local-host.mjs` (the local
process-host coordinator) + `web/pyodide-worker.mjs`; `worker.mjs` carries only a **generic**
hook (`let localHost = null`; dynamic-`import`s `local-host.mjs` and installs it iff
`BUILD.localHost && serverless`), so the standard build never even loads the demo module. The
shared-core fixes (the `proc_host` gate rename, the `terminal_exit` projection) live in the
wasm and benefit both builds. Serve the demo with `BEMTVI_SERVE_ROOT=demo-site node web/serve.mjs`;
`verify-pyodide-terminal.mjs` runs against `demo-site/`, the standard verifies against `web/`
(with a guard asserting `localHost=false`).

## Architecture

```
UI thread (index.html)  ── DOM renderer, keystrokes ─┐
                                                     │ postMessage / SAB
Editor Worker (worker.mjs)  ── owns eh.wasm + Lua ───┤
  off-tick fulfillment:                              │
    fs   → OPFS                                      │
    proc/term → PYODIDE WORKER ──── postMessage ─────┤   (interpreter / REPL / shell)
    lsp       → BASEDPYRIGHT WORKER  postMessage ────┤   (JS langserver, virtual FS)
                                                     │
Pyodide Worker  ── CPython 3.12 in wasm ─────────────┤
basedpyright Worker  ── pyright JS bundle ───────────┘
```

The interpreter (Pyodide) and the LSP (basedpyright JS) each run in **their own Worker**,
never the editor Worker — a `while True:` in the REPL must not freeze the editor's run
loop. Pyodide's interrupt buffer (a `SharedArrayBuffer`) makes `<C-c>` real even
mid-execution. The editor Worker posts proc/term/lsp requests to the relevant Worker and
lands replies via the existing `eh_*` completion exports — the daemon path's exact shape,
only the transport is `postMessage`.

**FS bridge:** the project files live in OPFS (the editor's fs). Pyodide gets the same
view via `mountNativeFS`/OPFS mount (or copy-in + write-back on save) so `python main.py`
sees what the editor shows and edits are visible to the interpreter.

## Open risks / de-risking spikes (RUN THESE FIRST, before committing to a phase)

1. **basedpyright in the browser.** basedpyright's langserver is bundled JS (Node-targeted:
   it uses `fs`, `path`, `worker_threads`, `vscode-jsonrpc`). Running it in a Web Worker
   needs (a) a browser/worker build or shims for those Node APIs, and (b) a **virtual
   filesystem** holding the project sources + the bundled typeshed stubs. Spike: stand the
   langserver up under Node first (trivial — it's a Node program), then assess the browser
   port: which Node APIs it touches, whether a published browser build exists, and how to
   back its FS with our project files. **Biggest unknown — spike first.**
2. **Lua amalgamation (single-file plugin bundle).** Instead of a sync multi-file `require`
   mechanism, concatenate every plugin module into ONE Lua file that registers each module
   in `package.preload["mod"] = function() … end`; `require` then resolves from the preload
   table with **no filesystem**. `StdLib::ALL_SAFE` (runtime.rs:803) includes Lua's
   `package` lib, so `package.preload` exists in the wasm VM. Spike: `exec_lua` a
   preload-registered module + `require` it; confirm it resolves. Low risk, confirm cheaply.
3. **Pyodide interpreter in a Worker.** Confirm Pyodide loads + runs python in a Worker,
   with an OPFS-backed FS view of the project and a working interrupt buffer. eh.wasm and
   Pyodide coexist (separate Worker instances; COOP/COEP already satisfied). Pyodide is
   ~6–10 MB+; lazy-load on first process need; self-host (not CDN) so COEP `require-corp`
   is satisfied.

## Phases (commit + pause for review between each)

### Phase 0 — capability decoupling (core seam prep) — *small* — ✅ DONE
- Renamed the wasm gate `Sink::daemon_connected` → `Sink::proc_host` and the FFI setter
  `eh_set_daemon_connected` → `eh_set_proc_host`, so the gate honestly means "a process
  host exists (daemon **or** local in-browser Worker host)". `has_remote_proc` /
  `has_remote_lsp` read it; `has_remote_fs` stays hardcoded `true` (fs unaffected). Updated
  `build.sh` export list + `worker.mjs` cwrap + the two daemon connect/disconnect callsites.
  No behavior change — the local host that *also* flips this gate lands in Phase 1.
- **Verified:** wasm rebuilt clean; `node harness.mjs` + `web/verify.mjs` (worker boot,
  editing, OPFS, SAB timers) green — the `eh_set_proc_host` cwrap resolves at boot. Daemon
  proc-gate regression (`verify-proc.mjs`) re-run against the renamed export.
- *(Moved to Phase 1: the Worker `localHost` abstraction — it belongs where it's first
  exercised rather than landing as unused scaffold.)*

### Phase 1 — Pyodide worker + `:terminal python file.py` — *large* — ✅ DONE
- New `web/pyodide-worker.mjs`: a dedicated **Pyodide Worker** (lazy-loaded on first
  `:terminal`), self-hosted from `web/vendor/pyodide/` (vendored by `build.sh`, ~6 MB,
  fetched only on first use). Mounts OPFS at `/project` for a live file view; runs
  `python <file>` via `runpy.run_path`, line-buffered stdout/stderr streamed back as
  `term_data`. Bare `python`/non-python fail loud (REPL is Phase 2).
- `web/worker.mjs` local-host coordinator: serverless boot flips `eh_set_proc_host(1)`;
  `drainTerminalRequests` routes to the Pyodide Worker; `data`/`exit` land on the existing
  `daemonNotifications` queue as `term_data`/`term_exit` (reusing the daemon leg's landing).
  Proc/LSP serverless drains now **fail loud** (gate is open but those legs aren't wired).
- **Two real bugs found + fixed along the way:**
  - **Serverless async-park:** the run loop's `waitAsync` (non-blocking) park was gated on
    `daemon &&`, so a serverless local-host terminal parked on *blocking* `Atomics.wait` and
    could never receive the Pyodide Worker's `postMessage`s. Widened to `(daemon ||
    localHostEnabled)`.
  - **`terminal_exit` lost the tail:** the wasm `terminal_exit` dropped the emulator without a
    final `terminal_project`, so a fast-exiting child's last output burst (fed in the same pass
    as the exit) was never mirrored. Mirrored the native `on_term_event` Exit arm (project
    before freeze). Latent for the daemon path too; the local interpreter exposed it.
- **Verified:** `verify-pyodide-terminal.mjs` — serverless `:terminal python /demo.py` runs
  CPython in-browser, computes `sqrt(1764)=42`, streams multi-line stdout, clean exit. No
  regressions: `verify.mjs` (serverless) + `verify-terminal.mjs` (daemon) + `harness.mjs`.

### Phase 2 — interactive REPL + interrupt — *medium* — ✅ DONE
- Bare `:terminal python` opens an interactive REPL: a host-side line editor (echo, Backspace,
  Enter, Ctrl-D) accumulates input and runs completed statements **synchronously** via Python's
  `codeop` (single-mode compile → `exec`), so an expression's value auto-displays via the
  displayhook and multiline blocks get a `...` continuation. *Synchronous* execution is the key
  choice: it makes `<C-c>` a **catchable** `KeyboardInterrupt` — the editor Worker writes a SIGINT
  into a SharedArrayBuffer (`setInterruptBuffer`) which CPython polls mid-loop, the only path that
  works while the Pyodide Worker is blocked in a tight loop. (PyodideConsole's async run surfaces
  the interrupt out-of-band — unrecoverable — so it was dropped.)
- **Verified:** `verify-pyodide-repl.mjs` — `6*7`→`42`, persistent state across lines, a
  multiline `def` block, and a genuine `<C-c>` interrupt of `while True: pass` (→ clean
  `KeyboardInterrupt`, REPL recovers). No regression: `verify-pyodide-terminal.mjs` (script mode).
- *Deferred:* python's own `input()`/stdin inside a running program (the REPL's line reading is
  separate and works) — needs a blocking SAB stdin read; a later slice.

### Phase 3 — async proc leg locally (`vim.system`/`jobstart`) — *medium* — ✅ DONE
- `web/local-host.mjs` `proc(reqs)` now fulfils the off-tick async-proc seam against the same
  Pyodide Worker (one shared interpreter for `:terminal` + proc). Each spawn runs `python …` and
  the Worker answers with `proc-spawned`/`proc-stdout`/`proc-exited` → the existing daemon
  `proc_*` landings (`eh_proc_spawned`/`eh_proc_stdout`/`eh_proc_exited`); `liveProcs` keeps the
  run loop on its non-blocking park so the pushes are received. The ctx gains `liveProcs`.
- `web/pyodide-worker.mjs` `__btv_proc_run` mirrors the daemon `host.rs` contract: stdout/stderr
  captured **separately** (via `contextlib.redirect_stdout/redirect_stderr`, isolated from the
  terminal's `curBuf` routing) with an exit code, run in a fresh `__main__` namespace with
  `sys.argv`/`sys.stdin`/`cwd`/`env` set + restored. A **streaming** spawn (`btv.run_stream`)
  pushes newline-stripped stdout lines through `proc-stdout` as they're produced and returns
  empty stdout with the exit (already streamed); a plain spawn returns the whole capture.
- Invocation forms: `python -c CODE`, a script `FILE` (path rebased onto the `/project` OPFS
  mount), and source-from-stdin (`python -`). Only `python` is available — any other binary is
  command-not-found (exit 127, stderr names it), exactly as a shell reports a missing binary (a
  localized failure, not a host crash). Kill = SIGINT via the shared interrupt buffer (best-effort
  — one buffer, single-threaded interpreter).
- **Verified:** `verify-pyodide-proc.mjs` — serverless `btv.run{python -c …}` computes
  `sum(0..100)=5050` on stdout with a distinct line on stderr (captured separately), `sys.exit(3)`
  → code 3, an uncaught exception → code 1 + traceback on stderr, a non-python binary → 127,
  piped stdin read back, and `btv.run_stream` delivers all five streamed lines. No regressions:
  `verify-pyodide-terminal.mjs` (script) + `verify-pyodide-repl.mjs` (REPL) — the shared Pyodide
  Worker is unaffected.

### Phase 4 — basedpyright LSP in a Worker — *large; has its own build pipeline* — ✅ DONE
- **Key discovery (much easier than the spike feared):** basedpyright's monorepo ships an
  **official browser target**, `packages/browser-pyright` ("browser-basedpyright"), that already
  uses `BrowserMessageReader/Writer`, bundles typeshed into a virtual FS, and solves the
  background-analysis-worker problem — so none of the spike's hand-written entry / Node shims /
  memfs / `createBackgroundAnalysis` override is needed. (We also evaluated ruff — lint/format
  only, no hover/completion/goto — and ty's `ty_wasm` — full-featured but alpha + a Rust-wasm build
  + a protocol adapter; basedpyright won.)
- **4a — build pipeline (`build-basedpyright.sh`):** clone basedpyright at a pinned tag, `npm
  install` the monorepo, symlink `typeshed-fallback` → `docstubs` (real docstubs need extra Python
  tooling; the plain typeshed type-checks identically), `rspack build` the browser package, and
  vendor the one ~16 MB `pyright.worker.js` into `web/vendor/basedpyright/` (gitignored;
  `package-site.sh --demo` builds + copies it, like Pyodide). Idempotent (`--force` to rebuild).
- **4b — framing bridge (`web/local-host.mjs` `lsp(reqs)`):** the editor's `SyncLspClient` speaks
  `Content-Length`-framed JSON-RPC over `lsp_spawn`/`lsp_stdin`/`lsp_kill`; the worker speaks
  postMessage'd JSON objects. The bridge de-frames/re-frames across that boundary, lands replies via
  the existing daemon `lsp_*` paths (`eh_lsp_stdout`/`exited`), and facilitates basedpyright's
  `browser/newWorker` background-analysis worker (creating it + transferring the MessagePort).
  `liveLsp` (new ctx field) keeps the run loop on its non-blocking park. **Three load-bearing
  fixups** found while wiring (the bulk of the effort):
  - **Workspace under `/w`, disjoint from `/typeshed`:** with the editor's natural root
    (`file:///`) basedpyright treats typeshed's ~5000 stubs as workspace sources and never analyzes
    the user's file. Every `file://` uri is rebased `file:///…` ↔ `file:///w/…` across the bridge.
  - **Synthesize `workspaceFolders`:** bemtvi sends only `rootUri`; browser-basedpyright keys its
    workspace off `workspaceFolders`, so without one it falls to an empty `<default>` workspace.
  - **`pyright/createFile` before `didOpen`:** the server only analyzes a file once it exists on
    its FS; the didOpen overlay then supplies live text. Also: guarantee `initializationOptions.files`
    is an object (it destructures it) and drop `rootPath`.
- **Verified:** `verify-basedpyright-lsp.mjs` — serverless `btv.run`-free: a python type error
  (`add("x", 1)`) yields a real basedpyright diagnostic (`"Literal['x']" is not assignable to
  "int"`) in `btv.diagnostic.get()` (proving typeshed loaded — `int` resolves) and a hover request
  returns the inferred signature `def add(a: int, b: int) -> int`. No regressions: terminal / repl /
  proc / core verifies green.
- *Deferred:* the cursor-anchored hover **float UI** drops the reply on a `cursor_moved`/`buffer_changed`
  staleness check in the serverless async round-trip (the protocol round-trip itself is correct — the
  verify issues hover via `btv.lsp.request`); completion/go-to-def UX and multi-file project seeding
  (cross-file analysis) are later slices. The demo's `init.lua` LSP config lands in Phase 6.

### Phase 5 — single-file plugin bundle (amalgamation) — *medium (spike #2)* — ✅ DONE
- **Spike #2 confirmed GREEN:** `package.preload` + the stock `require` resolve a multi-module
  bundle (including a submodule that itself `require`s another submodule) in the wasm VM — the
  preload searcher is `require`'s first searcher and `StdLib::ALL_SAFE` loads `package`.
- **`web/amalgamate-plugins.mjs`** — the build step. Walks each plugin's `lua/` tree, maps each
  `.lua` to its module name following the default `package.path` order (`foo/init.lua`→`foo`,
  `foo/bar.lua`→`foo.bar`), and concatenates every module's source into ONE chunk that registers
  `package.preload["mod"] = function(...) <body> end`. A trailing newline guards a final
  `-- comment`; duplicate module names across plugins error (no silent clobber). Both a CLI
  (`-o OUT.lua PLUGIN_DIR…`, or stdout) and an exported `amalgamate(dirs)` the verifier calls.
- **Boot wiring (`web/worker.mjs`):** `bootWithConfig` sources `/plugins-bundle.lua` from OPFS
  **before** `/init.lua`, so an `init.lua` that `require("bemtvi-line")`-class resolves it from the
  preload table. Absent (the standard editor seeds none) → skipped, exactly like an absent
  init.lua; a broken bundle is surfaced non-fatally. The wrapper is sound for any module (a valid
  Lua chunk is a valid function body → top-level locals stay module-scoped, the file's `return`
  becomes the module value). *(Seeding the actual first-party bundle into OPFS rides the
  project-seeding in Phase 6/7; Phase 5 is the mechanism + a general amalgamator.)*
- **Verified:** `verify-plugin-bundle.mjs` — a multi-file fixture plugin ("which-key": init +
  config + util, init requiring BOTH submodules) is amalgamated, seeded to OPFS, and
  `require("which-key").setup{ delay=50 }` runs at boot; its composed label `[which-key@50]`
  (default name + overridden delay, util-formatted) proves nested require + defaults-merge +
  return-value threading. Also: a submodule resolves via `require` post-boot (preload persists),
  a missing module fails loud, and with the bundle absent `require("which-key")` fails (no ambient
  leak) yet the editor still boots + edits. No regressions: `verify.mjs` + `verify-config.mjs`.

### Phase 6 — bundle the first-party plugins + catppuccin + demo init.lua — *medium* — ✅ DONE
- **Load-bearing shared-core fix (`build.sh` `-sSTACK_SIZE=8MB`):** emscripten's `cwrap`
  marshals a `"string"` arg onto the C stack (`stringToUTF8OnStack`), and the default stack is
  64 KB — so `eh_source_lua`/`eh_exec_lua` of any Lua chunk >64 KB trapped (`memory access out
  of bounds`). The 352 KB plugin bundle hit it; a large user `init.lua` would have too. Bumping
  the stack to 8 MB fixes it for both builds (verified: the full bundle sources + all 6 plugins
  `setup()` cleanly).
- **`build-plugins.sh`** — clones the recommended set (`bemtvi/{bemtvi-keys-helper,bemtvi-tree,
  bemtvi-line,bemtvi-lspconfig,bemtvi-diff}`) + `bemtvi/catppuccin-bemtvi` at **pinned commits**
  (full clone + checkout SHA, so an arbitrary pin resolves), then runs `amalgamate-plugins.mjs`
  over all six `lua/` trees → `web/vendor/plugins/plugins-bundle.lua`. Idempotent (`--force`),
  clones cached in `.plugins-src` (gitignored), `BEMTVI_PLUGINS_BASE` overrides the host.
- **Boot wiring:** `web/build-config.js` gains `plugins:false`; `package-site.sh --demo` flips it
  true, runs `build-plugins.sh`, and ships the bundle (standard flavor strips `web/vendor/plugins`).
  `worker.mjs` (demo build only, `BUILD.plugins`) fetches + sources the vendored bundle BEFORE the
  OPFS bundle / init.lua, so the config's `require("bemtvi-line")`-class resolves from
  `package.preload`. Missing/broken → non-fatal.
- **`web/demo-init.lua`** — the demo config: catppuccin mocha (`require("catppuccin").load()` —
  the colorscheme path a runtimepath-less browser can't source), which-key (keys-helper), the
  tree sidebar (`<leader>e`), the lualine-style statusline (`theme="auto"`), the LSP keymaps
  (lspconfig), the diff commands, and the python LSP via `btv.lsp.config/enable("basedpyright")`
  (the Phase-4 path; the local host routes any spawn to the bundled basedpyright worker).
  *(Auto-seeding this into OPFS on first boot rides Phase 7; the verify seeds it directly.)*
- **Verified:** `verify-plugin-demo.mjs` (against `demo-site/`) — all six modules load from the
  bundle (`package.loaded`), catppuccin mocha applied (`Normal` = `#cdd6f4`/`#1e1e2e` from the
  real hl registry), the bemtvi-line statusline renders (the `NORMAL` mode segment is in the
  redraw frame the client paints), and basedpyright is configured + enabled. No regressions:
  `verify.mjs` / `verify-config.mjs` / `verify-plugin-bundle.mjs` / `verify-pyodide-terminal.mjs`.

### Phase 7 — pre-loaded demo project + tour + python grammar — *medium* — ✅ DONE
- **Python tree-sitter grammar was already bundled offline:** `python` is in `web/grammars.js`
  `BUNDLED` (→ `web/vendor/grammars/python.wasm` + manifest, shipped by the blanket vendor copy),
  so `.py` highlights with no `:TSInstall`. Phase 7 only had to add the project + tour seeding.
- **First-boot OPFS seeding (`web/demo-seed/` + `worker.mjs`):** the seed dir holds the editable
  config (`init.lua`, moved here from `demo-init.lua`), a small real typed python project
  (`main.py` + `geometry.py`), the guided tour (`TOUR.md`), and a `manifest.json`. On first boot
  (demo build, `BUILD.demoSeed`) `bootWithConfig` fetches the manifest + each file and writes them
  into OPFS, guarded by a sentinel (`/.bemtvi/.demo-seeded`) so it runs **once** — a user's later
  edits persist across reloads, never clobbered. Runs before the init.lua read so the seeded
  config applies on that same boot. `package-site.sh --demo` ships `web/demo-seed/` + flips the flag.
- **Tour opens on boot:** `init.lua` ends with `edit /TOUR.md` (harmless if absent → empty buffer).
- **Verified:** `verify-demo-seed.mjs` — from a cleared OPFS, first boot seeds the project + tour +
  config (read straight back from storage), the tour opens as the startup buffer, `main.py`
  highlights offline (colored spans, no `:TSInstall` — python grammar pre-bundled), and seeding is
  one-time (a user edit survives a reload). No regressions across the whole web suite: standard
  (`verify.mjs`), plugin demo (`verify-plugin-demo.mjs`), and the python legs
  (`verify-pyodide-{terminal,repl,proc}.mjs`, `verify-basedpyright-lsp.mjs`) — the terminal verify
  was made seed-robust by typing into a fresh `:enew` buffer (the async `/TOUR.md` read can't land
  in it) instead of the seeded startup buffer.

### Phase 8 — deployment / packaging — *medium* — ✅ DONE (deploy wiring; revisit for plugins)
- Two separate Netlify sites from one repo: **bemtvi** (standard editor) via the root
  `netlify.toml` → `netlify-build.sh` → `_site/`; **bemtvi-demo** (python demo) via
  `netlify-build-demo.sh` → `_site-demo/` (configured in the dashboard, documented in that
  script's header + the root `netlify.toml`). Shared toolchain provisioning extracted to
  `netlify-provision.sh`; the publish layout is `package-site.sh` (now copies `build-config.js`
  + writes `_redirects`; `--demo` flavor adds `local-host.mjs` + `pyodide-worker.mjs` + Pyodide
  + flips the flag). Pyodide self-hosted (COEP `require-corp` satisfied); `_headers` gives
  cross-origin isolation (the editor's SAB *and* Pyodide's interrupt). Lazy-load keeps the
  ~6 MB off the first paint.
- **Plugin bundle + project-tour assets now in `--demo` packaging (Phases 6–7):**
  `package-site.sh --demo` runs `build-plugins.sh` and ships `web/vendor/plugins/plugins-bundle.lua`
  + `web/demo-seed/`, and flips `build-config` `plugins`/`demoSeed` true; the standard flavor strips
  `web/vendor/plugins` and ships neither. So the `netlify-build-demo.sh` deploy is complete.
- **Verified:** the packaged **standard** `_site/` boots green (`verify.mjs` via
  `BEMTVI_SERVE_ROOT=_site` — also proving the `build-config.js` packaging fix), and the
  packaged **demo** site (`build-demo.sh` → `package-site --demo`) runs `:terminal python`
  (`verify-pyodide-terminal.mjs`), loads the plugin set (`verify-plugin-demo.mjs`), and seeds the
  project/tour (`verify-demo-seed.mjs`).

### Phase 9 — minimal POSIX shell in `:terminal` — *medium* — ✅ DONE
- **Bare `:terminal` now opens a minimal shell** (was: fail-loud 127). `:terminal python [file]`
  stays the REPL/script path; the core passes an **empty argv** for bare `:terminal`
  (`bemtvi-core/.../terminal.rs` — empty argv = default shell), which `pyodide-worker.mjs open()`
  routes to the shell. Reuses the REPL's cooked-mode line editor (echo/Backspace/Enter/Ctrl-C/D),
  the SAB interrupt (a runaway `python` stage is `Ctrl-C`-able), and the Pyodide `/project` mount.
- **The shell executor is python (`__btv_sh_exec`), so builtins share python's exact FS view.** A
  line is tokenized with `shlex(posix, punctuation_chars)` (quotes + operators), split on
  `;`/`&&`/`||`, then per statement into a `|` pipeline with `>`/`>>`/`<` redirects; each stage
  threads stdout→stdin as a string. Builtins (`cd pwd ls cat echo mkdir rm mv cp touch head tail
  wc which env export clear exit`) run over the mounted FS; `$VAR`/`${ }` expansion, `VAR=val`
  (command-scoped) + `export`, and `*?[` globbing are supported; `python [file|-c|-]` runs
  in-process (stdin-aware); anything else → `command not found` (127).
- **Write-back to OPFS only (no live editor-buffer refresh, per the decision):** the
  `mountNativeFS` handle is captured and `nativeFs.syncfs()` runs after each command line, so file
  mutations (`echo hi > f`, `mkdir`, `rm`) persist to OPFS. `bemtvi-tree`'s `btv.fs` watch shows new
  files; an already-open editor buffer is not auto-reloaded (a `:e!` re-reads it).
- **Verified:** `verify-pyodide-shell.mjs` — `pwd`/`ls` (sees the seeded project), `echo … > f` +
  `cat f` (and the bytes land in OPFS), a `cat … | python -c …` pipe (stdin into a python stage),
  `cd`/`mkdir` + persisted dir, `$VAR`/`export`, and `command not found`. The tour (TOUR.md) now
  leads with the shell. No regressions: the terminal/repl/proc/lsp/plugin/seed verifies stay green.

## Non-goals (first cut)
- ~~A full POSIX shell in `:terminal`~~ — a minimal one landed (Phase 9). Still out: job control,
  subshells `()`, command substitution `$( )`, here-docs, signals beyond Ctrl-C, non-`python` externals.
- Pip-installing arbitrary packages at runtime (only the bundled wheels).
- Multiple concurrent interpreters beyond what the demo needs.

## Conventions
- Fail loud: any unimplemented shell command / LSP capability errors by name — no silent
  no-op (CLAUDE.md).
- Each phase ends green with a headless Playwright `verify-*.mjs` (web testing convention).
