# The local edit-host, the remote daemon, and Lua in the browser — implementation plan

## Why this document exists

Remote nxvim is **laggy**, and the lag is structural, not tunable. Today's
client–server split (`docs/architecture.md` → *Embedded vs. remote*) puts the
**whole editor on the far side of the wire**:

```
UI client (local)  ──NETWORK──▶  server = core + Lua + LSP + treesitter + fs  (remote)
```

Every keystroke round-trips to the remote before the cursor moves, because the
editing state machine lives on the server. No amount of protocol tuning fixes
one-round-trip-per-keystroke — it's where the boundary *is*.

This plan moves the network boundary **below** the editing engine instead of
above it — the same trick VS Code Remote uses (the Monaco editor is local; only
the lag-tolerant work — fs, LSP, terminals — is remote):

```
UI client (local) ──cheap──▶ EDIT HOST = core + Lua + treesitter (local) ──NETWORK──▶ DAEMON = fs + process + watch (remote)
```

The keystroke → core → redraw path becomes entirely local: zero round-trips for
typing, motions, operators, undo. The wire only carries things that were always
going to feel like a spinner.

The **same edit-host concept** then unlocks a long-standing goal: **running the
real editor — Lua plugins and all — entirely in the browser**. The browser is
just the edit-host compiled to wasm in a Web Worker, with the daemon either
absent (serverless) or reached over WebSocket. `nxvim-web` today is core+view
only (no Lua); this plan brings the Lua-bearing edit-host to wasm.

### What this changes about the thesis

Principle #3 ("Client-server, always; thin clients, headless server") bends — but
only in *topology*, not semantics:

- **"Identical editing behavior everywhere"** — *kept*. `nxvim-core` is unchanged
  in its editing logic; we only swap its I/O dependency.
- **"Thin clients, headless server"** — becomes **"thin daemon, thick clients."**
  The laptop/browser runs the full edit-host; the remote runs only fs + process +
  watch. That's the deliberate trade VS Code makes, and it's the right one for
  latency.

---

## What's already de-risked (feasibility spikes, 2026-06-09)

The three "will it even work" unknowns for the browser path were spiked **and run**
before this plan was written. All green. (Throwaway crates at
`~/work/lua-{wasm,interop,worker}-spike`; results captured in the project memory
`puc-lua-compiles-to-wasm-emscripten`.)

| # | Question | Result |
| --- | --- | --- |
| 1 | Does PUC Lua 5.1 compile **and run** in wasm? | ✅ `wasm32-unknown-emscripten`, `_VERSION=Lua 5.1`, `pcall(error)` caught (**setjmp/longjmp survives wasm** — the real risk), coroutines work. ~387 KB release wasm. |
| 2 | Can one wasm module hold the VM **and** do JS interop without wasm-bindgen? | ✅ JS→Rust via `ccall`/`cwrap`, Rust→JS via `EM_JS`/`emscripten_run_script`, a `thread_local` VM keeps **state persistent across calls** (buffer survives between keystrokes). |
| 3 | Can a **synchronous blocking read** work without freezing the page? | ✅ Lua `getcharstr()` parks on `Atomics.wait` against a `SharedArrayBuffer` in a Worker, fed by the UI thread; wakes in ~0–1 ms, no spin. **Confirms Worker+SAB → no Asyncify needed.** |

Two facts these pin down, both load-bearing for the plan:

- **LuaJIT is permanently out in wasm** (mlua: WASM supports all versions *excluding
  JIT*). The browser is forever on the `lua51` backend (PUC Lua 5.1), so it
  inherits the `lua51` plugin ceiling — see Phase 2.
- **The emscripten EH gotcha:** rust 1.96 links the emscripten target with new
  wasm exceptions (`-fwasm-exceptions`) but `cc` compiles vendored Lua with the
  legacy EH → `undefined symbol: __cxa_find_matching_catch_3`. Fix:
  `EMCC_CFLAGS=-fwasm-exceptions` so both halves agree.

Everything past the spikes is **engineering with known shapes**, not feasibility.

---

## The one constraint that shapes everything

**`nxvim-core` and the Lua VM are `!Send` and live on a single thread** (same as
neovim; concurrency comes from async I/O, not parallel mutation —
`docs/architecture.md` → *Async design*). The plan never violates this:

- Native: the edit-host keeps its own OS thread + single-threaded runtime, exactly
  as the embedded server does today.
- Browser: the edit-host owns the **Web Worker** thread. The Worker *is* the
  single thread; the UI thread never touches editor/Lua state — it only ferries
  input over the SAB and renders redraws. This maps the existing model onto the
  browser with no change in shape.

So the keystroke path is sync-and-local in both worlds; only the I/O dependency
(`HostServices`, Phase 1) ever reaches async/remote, and never on the edit tick.

---

## Status legend

- ✅ done   🚧 in progress   ⬜ not started

| phase | title | depends on | status |
| --- | --- | --- | --- |
| 0 | Feasibility spikes (compile / interop / blocking read) | — | ✅ |
| 1 | The `HostFs` I/O seam in core (dependency inversion) | 0 | ✅ |
| 2 | Opt-in yieldable `pcall` primitive (`vim.co_pcall`) | — | ✅ |
| 3 | Native edit-host / daemon split + the `HostProc` seam | 1 | 🚧 |
| 4 | wasm edit-host: compile (gate `nxvim-ts`, emscripten build) | 1, 2 | ⬜ |
| 5 | wasm edit-host: Worker + blocking input + JS interop | 4 | ⬜ |
| 6 | Browser fs/process: daemon over WebSocket (or serverless OPFS) | 3, 5 | ⬜ |

Phases 1 and 2 are independent and small; tackle either first. Phase 3 is the
native latency payoff. Phases 4–5 are the browser payoff. Phase 6 unifies them on
the one daemon. Each phase is sized to be picked up in a focused session with only
its dependencies loaded.

---

## The keystone: the `HostFs` seam (Phase 1) — ✅ DONE

Every later phase pivots on **dependency-inverting `nxvim-core`'s I/O**. Core
defines the interface it needs; the default implementation wraps the local disk,
and Phase 3 swaps in a daemon-backed one — the editing logic never knows which.

**Scope decision (2026-06-10): Phase 1 is the filesystem seam only.** Process
spawning (`HostProc`) is *already* isolated server-side in an async actor
(`evloop.rs`) and its trait shape is coupled to Phase 3's daemon wire protocol
(it's async + event-routing — stdout/exit come back as loop events, not a return
value). Guessing that shape ahead of the daemon invites rework, so it moves to
**Phase 3**. The high-value, core-touching half — the part that made the "pure
core" thesis real — is the fs seam, and it landed here.

Shipped (`crates/nxvim-core/src/host.rs`), a **synchronous** trait + a real-disk
default:

```rust
pub trait HostFs {
    fn exists(&self, path: &Path) -> bool;
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read>>; // streaming → rope at ~1× size
    fn stat(&self, path: &Path) -> Option<FileStat>;               // mtime + size, for disk_changed
    fn write_atomic(&self, path: &Path, contents: &[u8]) -> io::Result<()>;
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<DirEntry>>;   // explorer listing
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
}
pub struct StdHostFs; // the default; owns the atomic-write / owner-preserve logic
```

`Buffer::{from_file,from_dir,write,disk_changed}` take `&dyn HostFs` instead of
calling `std::fs`. `Editor` holds an `Rc<dyn HostFs>` (Rc so a `&mut`-borrowing
buffer write can still lend it without aliasing `self`; core is single-threaded)
with `set_host_fs` for Phase 3's remote injection.

Two points that keep this honest against `nxvim-core stays pure and synchronous`:

1. **Core stays sync.** The methods are called at *buffer open* / *save*, never on
   the keystroke path. When Phase 3 needs the actual wait-on-the-network to be
   async, that happens in the edit-host *orchestration* layer (the server, already
   async via `evloop.rs`), which fetches bytes off-tick and hands core a populated
   buffer — `Editor::load_str` / `mark_saved` already do exactly this in-memory
   open/save for the web build, so the pattern exists.
2. **The trait stayed synchronous on purpose** — see the module docs in `host.rs`.

**Exit criteria — met.** Full `cargo test --workspace --no-fail-fast` green
(incl. the 536-test editing suite, buffers, windows, tabs, explorer); fmt +
clippy clean. The only failures are two pre-existing tree-sitter-grammar tests
that fail identically on a stashed clean tree (a sandbox limitation — the grammar
worker can't compile). Zero behavior change; pure inversion. Server unchanged
(default `StdHostFs` = today's behavior). Committed as `a378c16`.

---

## Phase 2 — An opt-in yieldable `pcall` primitive (`vim.co_pcall`) — ✅ DONE

**Independent of everything else; smallest, testable today.**

PUC Lua 5.1 can't `coroutine.yield` across a C-call boundary, and `pcall` is a C
function — so `pcall(vim.fn.getcharstr)` (which-key's live-popup loop) raises
*"attempt to yield across metamethod/C-call boundary"* instead of reading a key
(project memory `pcall-yield-blocks-on-puc-lua51`,
`whichkey-needs-vim-split-and-str2list`). LuaJIT (today's default) has a yieldable
pcall, so this only bites `lua51` — including the browser, which is permanently
`lua51` (Phase 0).

**Decision (2026-06-10): expose the fix as a named, opt-in primitive — do NOT
replace the global `pcall`.** Globally swapping `pcall` would impose a per-call
coroutine allocation on *every* protected call and risk subtle fidelity
regressions across unrelated code, for the benefit of the handful of plugins that
block-read inside `pcall`. Instead we ship `vim.co_pcall` (and `vim.co_xpcall` /
`vim.co_wrap`) that plugin authors targeting nxvim call explicitly when they need
a yieldable protected call. This fits nxvim's existing posture — plugins run
through a compat layer (`prelude/compat.lua`), not byte-for-byte unmodified.

**The trade we are accepting:** plugins that wrap a blocking read in the *global*
`pcall` — foremost **which-key's live popup** — will **not** read keys on
`lua51`/browser builds unless they switch to `vim.co_pcall`. This is a deliberate
carve-out from principle #1's "run the ecosystem unmodified," scoped to the narrow
blocking-read-in-pcall pattern. (On the default LuaJIT native build the global
pcall is already yieldable, so nothing regresses there.) See *Known limitations*.

**The implementation.** Run the protected fn in its own coroutine and *relay*
yields through a pure-Lua path (no C frame in the way) — exposed under
`vim.co_pcall`, not as a global:

```lua
function vim.co_pcall(f, ...)
  local co = coroutine.create(f)
  local function step(ok, ...)
    if not ok then return false, ... end
    if coroutine.status(co) == 'dead' then return true, ... end
    return step(coroutine.resume(co, coroutine.yield(...)))
  end
  return step(coroutine.resume(co, ...))
end
```

It composes with nxvim's pump-coroutine model: the inner yield is caught by
`coroutine.resume`, re-emitted by the relay to park the pumped coroutine, and the
server's key resumes the whole chain (verified logically; Phase 0 spike #1
confirmed coroutines work on the wasm build, and `lua51` natively already runs
them).

**Scope.** Ship `vim.co_pcall`, `vim.co_xpcall`, and `vim.co_wrap` in the prelude.
Match varargs, non-string error values, `error` level, and `xpcall`'s
message-handler semantics as closely as possible. Because it's opt-in, the
per-call coroutine cost falls only on calls a plugin author deliberately routes
through it. **Does not** cover yields from genuinely C-level callbacks
(`table.sort` comparator, `string.gsub` replacer, raw metamethods) — rare;
document the gap. Available on both backends (harmless on LuaJIT, where the global
`pcall` is already yieldable) so plugin authors can target a single name.

**Files.** `crates/nxvim-lua/.../prelude/` (alongside `stdlib.lua` /
`runtime.lua`); document `vim.co_pcall` for plugin authors.

**The relay needed one more piece than the sketch above (found in
implementation).** nxvim's blocking reads do *not* "bubble a yield up to the pump
which resumes the top coroutine" — the model the plan's relay was written for.
Instead `fs.lua`'s `await_prompt` (the single funnel for `getcharstr` / `input` /
`confirm`) registers a server callback that resumes **`coroutine.running()`
directly** and then yields. Under a `vim.co_pcall` the running coroutine is the
*inner* protected one, so a direct resume bypasses the relay and the relay's
coroutine — which holds the protected call's continuation — never wakes; the
sketch's relay alone hangs. The fix is a small, backend-shared change at that one
chokepoint: a `vim._co_driver` map (coroutine → its driver, weak keys) records
the resume chain, and `await_prompt` walks it to resume the **outermost** driver
so the relay chain forwards the resume value back down to the blocked inner
coroutine. With no `co_pcall` on the stack the map is empty, the root *is* the
running coroutine, and `await_prompt` is byte-for-byte its old self — zero
regression (the 18 `editing::prompts` + existing `getcharstr` tests stay green on
both backends). Shipped in `prelude/copcall.lua` (the `vim.co_pcall` /
`co_xpcall` / `co_wrap` family + the driver map) and the `await_prompt` edit.

**Exit criteria — met.** A black-box test (`crates/nxvim-server/tests/blockers.rs`,
the existing which-key regression home) drives a Lua snippet that wraps
`vim.fn.getcharstr` in `vim.co_pcall` through several keystrokes on a
`--features lua51` build and asserts it reads them — proving the relay reads input,
not merely that it doesn't error. Six tests landed: a single protected read, a
loop relaying several reads then feeding a resolved sequence (the which-key
shape), error/args/return passthrough, and `co_xpcall` / `co_wrap`. A throwaway
negative control confirmed the *global* `pcall(getcharstr)` still raises *"attempt
to yield across metamethod/C-call boundary"* on `lua51` where `co_pcall` succeeds —
so the test proves the relay does real work, not a no-op. Full `blockers` suite
green on **both** backends (34 tests, `luajit` + `lua51`); fmt + clippy clean. The
memory's "unmodified which-key live popup" case stays a documented limitation,
distinct from this passing opt-in test.

---

## Phase 3 — Native edit-host / daemon split (invert the SSH topology)

### Phase 3a — the server *consumes* the `HostFs` seam — ✅ DONE (2026-06-10)

The first slice, kept deliberately free of any daemon-protocol guesswork (the same
discipline that scoped Phase 1 to the fs seam alone). Phase 1 defined `HostFs` in
core but the **server never drove it** — `Editor::open_or_named` baked in
`StdHostFs` and opened the startup file in its constructor, so no injected fs
could ever reach the *first* buffer (the gap flagged in `set_host_fs`'s own doc).

Shipped:
- **`Editor::open_or_named_with(path, fs)`** (core) — loads the initial buffer
  *through* `fs` and installs it as the editor's `HostFs`, so the first buffer is
  fetched the same way every later `:edit`/`:write` is. `open_or_named` is now just
  this with `StdHostFs` (zero behavior change for existing callers). Directory
  detection still uses `std::path::Path::is_dir` — a type-bearing remote stat is a
  later daemon-wire concern; only the file read/write crosses the seam now.
- **`ServerInit::host_fs: Option<Box<dyn HostFs + Send>>`** — `Send` so it rides
  `ServerInit` onto the server's own thread, where `run_io` rebuilds it into the
  single-threaded `Rc<dyn HostFs>` the editor holds (`Rc::from` → drop `Send` by
  unsize coercion). `None` = today's local disk. The startup open lifts to *after*
  injection, exactly as the Phase-3 note prescribed.

**Exit criteria — met.** `crates/nxvim-server/tests/host_fs.rs`: an in-memory fake
`HostFs` (shared `Arc<Mutex<…>>` the test inspects) both **serves** the initial
buffer (a `/virtual/...` path that never touches disk) and **captures** `:w` — and
a bare-session `:write <path>` also lands in the fake. Faithful, not a no-op: the
fake genuinely round-trips bytes the editor read and wrote. Regression-clean —
`editing` (536), `buffers` (27), `nxvim` crate, fmt + clippy all green; the local
binaries (`nxvim`, `nxvim-gui`) pass `host_fs: None` and are unchanged.

**Still to do in Phase 3:** the `HostProc` seam (below), the daemon wire protocol +
`nxvim --daemon`, the local edit-host as a `HostServices` client over ssh stdio,
and the buffer-replica / `FileChangedShell` / remote-path / clipboard semantics.
The remote `HostFs` impl is *not* a drop-in here: core's `HostFs` is **sync**, so a
daemon-backed read can't block the single editor thread on the network — the open
must become an async off-tick fetch that hands core populated bytes (the
"buffers are local replicas" note below), with the sync trait reserved for local
disk. The injection seam this slice built is the anchor that lands plugs into.

### The full split

The native latency payoff. Carve today's `nxvim-server` into two roles connected
by `HostServices` (Phase 1) over RPC:

- **Edit host** (runs *locally* in the remote case): `nxvim-core` + `nxvim-lua` +
  `nxvim-ts` + redraw projection + the input/keymap/excmd/evloop machinery.
  Everything in `dispatch.rs` / `redraw.rs` / `input.rs` / `keymap.rs` /
  `excmd.rs` / `evloop.rs` / `lsp/` stays here.
- **Daemon** (runs *remotely*): fs + process + watch only — the `HostFs`/`HostProc`
  server half. Tiny.

The network boundary moves from *above* the editor (today's `nxvim --server` over
ssh stdio, `docs/plans/2026-06-09-remote-ssh-client.md`) to *below* it: the GUI/TUI
client and edit-host are co-located and local; `ssh … nxvim --daemon` runs just
the fs/process daemon on the remote, and the local edit-host is a `HostServices`
client over the ssh stdio transport (reusing the `nxvim-rpc` plumbing and the
hardened ssh connector from `crates/nxvim-gui/src/remote.rs`).

**The `HostProc` seam (folded in from Phase 1).** Define the process-spawning
trait here, where its shape can match the daemon wire protocol rather than being
guessed ahead of it. Unlike `HostFs` it is **async + event-routing** — a spawn
returns an id, and stdout/exit arrive as loop events (exactly as `vim.system`
already works via `evloop.rs`):

```rust
// the daemon's other half; consumed by the async server, not by core.
trait HostProc {
    async fn spawn(&self, cmd: &Command) -> io::Result<ProcId>; // jobstart / vim.system / LSP / :!
    async fn write_stdin(&self, id: ProcId, bytes: &[u8]) -> io::Result<()>;
    async fn signal(&self, id: ProcId, sig: Signal) -> io::Result<()>;
    // stdout/exit → loop events on the existing evloop channel.
}
```

Re-point the three spawn sites at it: `evloop.rs::run_process`, `lsp/manager.rs`
(a language server *is* "spawn + pipe stdio" — no special LSP protocol needed),
and `clipboard.rs`. The in-process impl wraps today's `tokio::process`; the remote
impl forwards to the daemon. **LSP needs no special protocol** — it collapses into
`HostProc`.

**Cross-cutting semantics this phase must define:**

- **Buffers are local replicas** (Monaco-style). Open = async-fetch bytes via
  `HostFs` → populate the rope; save = push bytes back. The rope is authoritative
  for open buffers; core sees a normal local buffer. (Lift the initial-file open
  in `lib.rs` to *after* `set_host_fs`, per the Phase 3 note on
  `Editor::open_or_named` — so the first buffer is fetched through the injected fs
  too, not the default `StdHostFs`.)
- **`FileChangedShell`** — the daemon's `watch` reports a remote file changed
  under us; surface neovim's reload/conflict prompt.
- **LSP buffer sync** — the server runs remotely (via `HostProc`); feed it
  incremental `didChange` over the wire (lag-tolerant, already async in `lsp/`).
- **Paths are remote paths** — buffer names, cwd, statusline operate in the
  remote's path-space (as VS Code Remote does).
- **Clipboard** — supersedes the remote-ssh v1 limitation: with the edit-host
  local, `"+`/`"*` can target the *local* OS clipboard directly.

**Exit criteria.** `nxvim --daemon` over stdio passes a black-box suite mirroring
`crates/nxvim/tests/stdio_server.rs` but for the `HostServices` protocol; an
end-to-end test drives a local edit-host against a daemon over an in-process
duplex and asserts edit/save/reload round-trips. Manually: typing over a real ssh
hop has **no per-keystroke latency** (the whole point — verify, don't assume).

---

## Phase 4 — wasm edit-host: compile

Bring the Lua-bearing edit-host to `wasm32-unknown-emscripten` (Phase 0 proved the
VM compiles; this compiles the *real* stack).

- **Gate out `nxvim-ts` + `libloading`.** Dynamic library loading doesn't exist in
  wasm. `nxvim-lua` pulls `nxvim-ts` (tree-sitter + `libloading`) for the
  `vim.treesitter` binding; feature-gate that binding **out** of the wasm build.
  The browser already does highlighting in JS via web-tree-sitter (project memory
  `web-treesitter-highlighting`, `docs/architecture.md` → *The web build*), so no
  capability is lost — the redraw just omits server-side highlight spans and the
  JS layer paints them, as `nxvim-web` does today.
- **Emscripten toolchain in the build.** The web build moves from
  `wasm32-unknown-unknown` (`crates/nxvim-web`, wasm-bindgen, `build.sh`) to
  `wasm32-unknown-emscripten`. Wire `EMCC_CFLAGS=-fwasm-exceptions` and the
  emsdk-sourced `emcc` into the build script. `nxvim-core` is pure Rust and
  compiles to the new target unchanged.
- **Backend = `lua51`** (LuaJIT excluded from wasm). Phase 2's `vim.co_pcall` ships
  here, but the global `pcall` is *not* swapped — so plugins that block-read inside
  the global `pcall` (unmodified which-key live popup) don't read keys unless they
  opt in. Known limitation, by design.

**Exit criteria.** A headless node harness loads the compiled edit-host module,
feeds a vim key sequence, and reads back buffer lines / a redraw — i.e. the *real*
editor runs in wasm, proven by behavior, not just a clean link (cf. project memory
`dont-conflate-loads-with-works`).

---

## Phase 5 — wasm edit-host: Worker + blocking input + JS interop

Make it a real browser editor (Phase 0 spikes #2/#3 proved the mechanisms; this
integrates them).

- **Interop replaces wasm-bindgen glue.** JS→Rust via `ccall`/`cwrap` on
  `#[no_mangle] extern "C"` exports (feed input, query lines); Rust→JS via a
  `--js-library` / `EM_JS` binding (push redraws) — the production form of the
  spike's `emscripten_run_script`. Required link flags (from the spikes):
  `-sMODULARIZE=1 -sEXIT_RUNTIME=0 -sALLOW_MEMORY_GROWTH=1
  -sEXPORTED_RUNTIME_METHODS=ccall,cwrap,UTF8ToString` plus the explicit
  `EXPORTED_FUNCTIONS`.
- **Edit-host in a Web Worker.** UI thread renders + ferries input; Worker owns
  core+Lua (the single `!Send` thread). Redraws post back to the UI.
- **Blocking input over SAB.** `getcharstr` / the pump-coroutine park on
  `Atomics.wait` against a `SharedArrayBuffer` keyboard channel the UI fills — no
  Asyncify. Plugins that opt into Phase 2's `vim.co_pcall` get yieldable blocking
  reads on top of this; unmodified global-`pcall` block-readers stay blocked (see
  *Known limitations*).
- **COOP/COEP serving.** `SharedArrayBuffer` needs cross-origin isolation
  (`Cross-Origin-Opener-Policy: same-origin` + `Cross-Origin-Embedder-Policy:
  require-corp`). Add to the dev server + ship docs.

**Exit criteria.** Driveable via Playwright through the `window.__nxvim` hook
(project memory `web-client-driveable-via-playwright`): type vim commands, assert
buffer/cursor/redraw, and drive a `vim.co_pcall(vim.fn.getcharstr)`-based snippet
(the opt-in blocking-read path) to prove SAB-backed blocking reads work end-to-end
in a real browser.

---

## Phase 6 — Browser fs/process: the daemon over WebTransport (or serverless)

Tie the browser edit-host to actual files/processes, reusing the Phase 1 traits and
the Phase 3 daemon.

- **The good path:** the browser edit-host is a `HostServices` client over
  **WebTransport (HTTP/3 / QUIC)** to a remote daemon — the browser analog of
  Phase 3, and the *inverse* of today's Socket.IO mode (`docs/architecture.md` →
  *connecting to a real server over Socket.IO*), which puts the whole server
  (editing included) remote and laggy. Here, editing is in the browser Worker;
  only fs/process cross the wire. Telescope's `rg`/`fd` run on the remote daemon
  via `HostProc`. **Why WebTransport over a single WebSocket** — see *Transport &
  stream multiplexing* below: the daemon carries three independent traffic classes
  (`HostFs` blobs, per-process `HostProc` stdio, `HostWatch` pushes), and a single
  WebSocket is one TCP stream, so a heavy `HostProc` flood (telescope's `rg`, an
  `npm install`'s output) head-of-line-blocks an LSP `didChange` or a file save
  queued behind it. QUIC's independent streams remove that coupling at the protocol
  level. The Rust daemon uses `wtransport` (on `quinn`); WebTransport mandates TLS
  even on localhost, so dev uses a self-signed cert (`wtransport`'s generator) with
  its hash passed to the browser `WebTransport` constructor.
- **The serverless path:** `HostFs` backed by OPFS / the File System Access API;
  in-memory rope authoritative. `HostProc` has no processes — per
  `No silent stubs or skips`, `system()`/`jobstart` must **fail loud**, not fake
  success. So serverless = real editing + plugins that don't shell out. (No
  transport at all — `HostServices` is satisfied in-Worker.)

**Exit criteria.** Browser edit-host opens/edits/saves a file served by a daemon
over WebTransport (each `HostServices` class on its own QUIC stream); a shell-out
plugin path runs the process on the daemon and its stdout streams back without
stalling a concurrent fs save. Serverless mode edits an OPFS file and raises a
clear error on a `system()` call.

### Transport & stream multiplexing (Phase 3 native + Phase 6 browser)

The daemon wire carries **three independent traffic classes**, and cramming them
down one ordered byte stream couples them through head-of-line (HOL) blocking:

| class | shape | hazard if shared |
| --- | --- | --- |
| `HostFs` | request/response, bursty binary blobs | a large read/write stalls behind other traffic |
| `HostProc` | long-lived stdio, **one pipe per process** (LSP servers, `rg`/`fd`, `:!`, future PTY) | a flood (`npm install`, `rg` over a huge tree) is the worst offender |
| `HostWatch` | server-*push* file-change events (`FileChangedShell`) | small, but must not wait behind a flood to surface a conflict |

This is the *application-layer* case for splitting `HostServices` into
`HostFs`/`HostProc`/`HostWatch` (Open Decision #1): distinct traits → distinct
logical channels → distinct streams.

**One nxvim-specific correction to the usual "remote editor" framing:** because the
edit-host moved the editor *local* (the whole thesis), the latency-critical
keystroke → core → redraw path **never crosses the wire**. So HOL blocking here can
delay *completion results*, *saves*, and *diagnostics* — all already-async,
spinner-tolerant surfaces — but it **cannot** stall typing/motions/operators/undo.
That makes stream-splitting a *responsiveness* win on async paths, not a fix for
typing lag (unlike the Monaco-remote topology, where the editor itself round-trips).

**The native (Phase 3) vs browser (Phase 6) transport asymmetry — they differ:**

- **Browser (Phase 6):** WebTransport/QUIC gives independent reliable streams over
  one authenticated connection — map one stream per `HostServices` class (and one
  per live `HostProc`). This is the right tool and the reason Phase 6 moved off the
  single WebSocket above.
- **Native (Phase 3):** `ssh … nxvim --daemon` is a **single ordered stdio stream** —
  QUIC can't go under it, so HOL blocking is intrinsic at the transport. Mitigate by
  multiplexing logical channels in the framing (nxvim's msgpack-RPC already frames
  concurrent requests) and/or opening separate ssh channels per `HostProc`; a true
  QUIC escape would require the non-ssh listener of Open Decision #2.

---

## Non-goals / known limitations

- **No protocol negotiation** (inherited): edit-host and daemon must be the same
  build; mismatch surfaces as a dropped connection, not a clean version error.
- **wasm plugin ceiling = `lua51`, not LuaJIT.** The browser runs the PUC 5.1
  dialect; plugins relying on LuaJIT-only behavior (e.g. `ffi`, `bit`) won't run.
  `Luau` (faster non-JIT, 5.1-ish, yieldable pcall, wasm-capable per mlua) is a
  possible future pivot but a different dialect bet — out of scope here.
- **Blocking reads inside the global `pcall` need opt-in.** `vim.co_pcall` is
  available but the global `pcall` is *not* replaced (Phase 2 decision). So plugins
  that wrap a blocking read in the global `pcall` — the canonical case is
  **which-key's live popup** — do not read keys on `lua51`/browser builds unless
  they switch to `vim.co_pcall`. A plugin author targeting nxvim opts in; we do not
  emulate LuaJIT's yieldable global pcall. (Native LuaJIT builds are unaffected —
  their global pcall is already yieldable.)
- **Yields from C-level callbacks** (sort/gsub/metamethods) remain un-yieldable on
  `lua51` even via `vim.co_pcall` (Phase 2 scope note).
- **Vimscript** stays a non-goal (`docs/architecture.md` → principle #2).

---

## Open decisions (resolve before the phase that needs them)

1. **`HostServices` granularity** (Phase 1): one combined trait vs. split
   `HostFs`/`HostProc`/`HostWatch`. Leaning split — smaller daemon surface, easier
   to stub the serverless `HostProc`, **and** the prerequisite for per-class stream
   multiplexing (distinct traits → distinct logical channels → distinct transport
   streams, so a `HostProc` flood can't HOL-block an `HostFs` save; see *Transport &
   stream multiplexing* under Phase 6).
2. **Daemon discovery/launch** (Phase 3): `ssh … nxvim --daemon` mirrors today's
   `--server`; do we also want a standalone `--daemon --listen` for non-ssh? (The
   remote-ssh plan deferred generic TCP; same call here.) This is also the only
   native path that could adopt **WebTransport/QUIC** — over ssh stdio (a single
   ordered stream) HOL blocking is intrinsic; a non-ssh QUIC listener would give
   native the same independent-stream story Phase 6's browser path gets. Decide
   whether that's worth a second native transport or whether app-level framing over
   ssh stdio is enough.
3. **One web build or two** (Phase 4): does the emscripten edit-host *replace*
   today's `wasm32-unknown-unknown` `nxvim-web`, or do both ship (a tiny
   no-Lua core-only build + a full Lua build)? Replacing is simpler; keeping both
   preserves the smallest-possible demo.
4. **Redraw transport in the browser** (Phase 5): pull (redraw = return value of
   the input call) vs. push (`EM_JS` notification). Pull is simpler and matches
   the single-threaded Worker; push matches the existing notification model.
```
