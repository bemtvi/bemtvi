# Plan: full in-browser python demo (Pyodide terminal + LSP, static site)

**Status:** proposed — awaiting sign-off
**Date:** 2026-06-23
**Owner thread:** web/edithost

## Goal

A self-contained, **static** web demo of nxvim (no backend, deployable to Netlify) that
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
wasm and benefit both builds. Serve the demo with `NXVIM_SERVE_ROOT=demo-site node web/serve.mjs`;
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
- `web/pyodide-worker.mjs` `__nx_proc_run` mirrors the daemon `host.rs` contract: stdout/stderr
  captured **separately** (via `contextlib.redirect_stdout/redirect_stderr`, isolated from the
  terminal's `curBuf` routing) with an exit code, run in a fresh `__main__` namespace with
  `sys.argv`/`sys.stdin`/`cwd`/`env` set + restored. A **streaming** spawn (`nx.run_stream`)
  pushes newline-stripped stdout lines through `proc-stdout` as they're produced and returns
  empty stdout with the exit (already streamed); a plain spawn returns the whole capture.
- Invocation forms: `python -c CODE`, a script `FILE` (path rebased onto the `/project` OPFS
  mount), and source-from-stdin (`python -`). Only `python` is available — any other binary is
  command-not-found (exit 127, stderr names it), exactly as a shell reports a missing binary (a
  localized failure, not a host crash). Kill = SIGINT via the shared interrupt buffer (best-effort
  — one buffer, single-threaded interpreter).
- **Verified:** `verify-pyodide-proc.mjs` — serverless `nx.run{python -c …}` computes
  `sum(0..100)=5050` on stdout with a distinct line on stderr (captured separately), `sys.exit(3)`
  → code 3, an uncaught exception → code 1 + traceback on stderr, a non-python binary → 127,
  piped stdin read back, and `nx.run_stream` delivers all five streamed lines. No regressions:
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
  - **Synthesize `workspaceFolders`:** nxvim sends only `rootUri`; browser-basedpyright keys its
    workspace off `workspaceFolders`, so without one it falls to an empty `<default>` workspace.
  - **`pyright/createFile` before `didOpen`:** the server only analyzes a file once it exists on
    its FS; the didOpen overlay then supplies live text. Also: guarantee `initializationOptions.files`
    is an object (it destructures it) and drop `rootPath`.
- **Verified:** `verify-basedpyright-lsp.mjs` — serverless `nx.run`-free: a python type error
  (`add("x", 1)`) yields a real basedpyright diagnostic (`"Literal['x']" is not assignable to
  "int"`) in `nx.diagnostic.get()` (proving typeshed loaded — `int` resolves) and a hover request
  returns the inferred signature `def add(a: int, b: int) -> int`. No regressions: terminal / repl /
  proc / core verifies green.
- *Deferred:* the cursor-anchored hover **float UI** drops the reply on a `cursor_moved`/`buffer_changed`
  staleness check in the serverless async round-trip (the protocol round-trip itself is correct — the
  verify issues hover via `nx.lsp.request`); completion/go-to-def UX and multi-file project seeding
  (cross-file analysis) are later slices. The demo's `init.lua` LSP config lands in Phase 6.

### Phase 5 — single-file plugin bundle (amalgamation) — *medium (spike #2)*
- Build step: amalgamate each plugin's Lua tree into ONE `package.preload`-registering Lua
  file; the existing single-file `init.lua` boot path sources it, then `require`s modules
  from the preload table (no filesystem, no runtimepath).
- **Verify:** `require('which-key')`-class resolves from the bundle; a plugin `setup()` runs.

### Phase 6 — bundle the first-party plugins + catppuccin + demo init.lua — *medium*
- Wire which-key, nvim-tree, lualine, lspconfig, diff + catppuccin; a demo `init.lua`
  configuring the python LSP, highlighting, and keymaps.
- **Verify:** plugins load, theme applies, statusline renders.

### Phase 7 — pre-loaded demo project + tour + python grammar — *medium*
- Seed OPFS on first boot with a small but **real** multi-file python project + a
  README/guided-tour buffer. Pre-bundle the python tree-sitter grammar so highlighting
  works with no `:TSInstall`.
- **Verify:** project present after boot, highlight colors, tour buffer opens.

### Phase 8 — deployment / packaging — *medium* — ✅ DONE (deploy wiring; revisit for plugins)
- Two separate Netlify sites from one repo: **nxvim** (standard editor) via the root
  `netlify.toml` → `netlify-build.sh` → `_site/`; **nxvim-demo** (python demo) via
  `netlify-build-demo.sh` → `_site-demo/` (configured in the dashboard, documented in that
  script's header + the root `netlify.toml`). Shared toolchain provisioning extracted to
  `netlify-provision.sh`; the publish layout is `package-site.sh` (now copies `build-config.js`
  + writes `_redirects`; `--demo` flavor adds `local-host.mjs` + `pyodide-worker.mjs` + Pyodide
  + flips the flag). Pyodide self-hosted (COEP `require-corp` satisfied); `_headers` gives
  cross-origin isolation (the editor's SAB *and* Pyodide's interrupt). Lazy-load keeps the
  ~6 MB off the first paint.
- **Verified:** the packaged **standard** `_site/` boots green (`verify.mjs` via
  `NXVIM_SERVE_ROOT=_site` — also proving the `build-config.js` packaging fix), and the
  packaged **demo** site (`build-demo.sh` → `package-site --demo`) runs `:terminal python`
  (`verify-pyodide-terminal.mjs`). *(Will need the plugin bundle + project-tour assets added to
  the `--demo` packaging in later phases.)*

## Non-goals (first cut)
- A full POSIX shell in `:terminal` (only enough to launch python + the project's commands).
- Pip-installing arbitrary packages at runtime (only the bundled wheels).
- Multiple concurrent interpreters beyond what the demo needs.

## Conventions
- Fail loud: any unimplemented shell command / LSP capability errors by name — no silent
  no-op (CLAUDE.md).
- Each phase ends green with a headless Playwright `verify-*.mjs` (web testing convention).
