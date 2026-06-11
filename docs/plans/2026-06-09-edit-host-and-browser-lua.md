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
above it — the same direction VS Code Remote takes (the Monaco editor and its
text model are local; fs, LSP, terminals are remote). One honest divergence from
that precedent, owned rather than glossed: VS Code runs its *extension host* on
the **remote**, while nxvim keeps the plugin runtime (Lua) **local** —
deliberately, because nvim plugins are latency-sensitive UI (statusline per
keystroke, which-key popups, telescope sorters) and running them remote would
reintroduce the very lag this plan removes. The cost is that the Lua VM's
*native* view of the filesystem is the local machine's — see the *Lua-visible
filesystem semantics* bullet under Phase 3, the hardest semantic the full split
must define:

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
| 4 | wasm edit-host: compile (gate `nxvim-ts`, emscripten build) | 1, 2 | 🚧 |
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

**Still to do in Phase 3:** the daemon wire protocol + `nxvim --daemon`, the local
edit-host as a `HostServices` client over ssh stdio, and the buffer-replica /
`FileChangedShell` / remote-path / clipboard semantics. The remote `HostFs` impl is
*not* a drop-in here — core's `HostFs` is **sync** — and it had two candidate
shapes: a **blocking bridge** (a sync impl that blocks the editor thread on a
channel until the daemon replies — keeps the Phase-1 seam at every call site,
and blocking on an explicit `:e`/`:w` is what vim itself does on slow storage)
vs. the **off-tick fetch** (the server fetches async and hands core a populated
buffer via `Editor::load_str`, the sync trait reserved for local disk — keeps
the editor thread live, but re-plumbs each fs-touching call site individually).
**Resolved (2026-06-10): Phase 3d chose the off-tick fetch** for buffer opens
(the `HostFsAsync` seam); see Open Decision #5 for the trade and the residual
blocking-bridge need on *sync* surfaces. Two corollaries that still stand
regardless of shape: a blocking bridge, wherever one is later needed (sync
`vim._system`, sync Lua fs calls), requires the daemon link's RPC tasks to live
on their **own** thread/runtime, *not* the server's single-threaded one — a
blocked editor thread would otherwise starve the very reader task carrying its
reply (deadlock); and **don't stat-poll over the wire** — `disk_changed`
checks against a remote should be driven by `HostWatch` pushes (or a short-TTL
stat cache they invalidate), never a per-check network round-trip. The
injection seam this slice built is the anchor the remote fs plugs into.

### Phase 3b — the `HostProc` seam (process spawning) — ✅ DONE (2026-06-10)

The process-side companion to Phase 3a's fs consumption, kept to the **one spawn
site whose shape already matches the daemon's event-routed contract** — the same
"don't guess the wire ahead of need" discipline that scoped Phase 1 to fs-only.
Of the three spawn sites the full-split note lists, only the event loop's
`run_process` (the async, one-shot `vim.system` / `jobstart` / `:!` path) is
run-to-completion with pid + exit reported as loop events; the **clipboard** is a
*synchronous* `Clipboard` provider returning a value, and the **LSP** servers are
long-lived bidirectional raw-pipe transports living in `nxvim-lsp` — both diverge
from the sketch, so folding them in is a later slice matched to the wire rather
than guessed now. (Scope confirmed with the requester, 2026-06-10.)

Shipped (`crates/nxvim-server/src/host.rs` — server-side, **not** core, because the
trait is async + event-routing and is consumed by the async server, never by the
pure-sync core):
- **`trait HostProc { fn run(&self, spec, kill, events) -> Pin<Box<dyn Future + Send>> }`**
  — one method owning a child's whole lifecycle. It returns a boxed future (not
  `async fn`) to stay object-safe for `dyn HostProc` and match the existing
  `Box<dyn HostFs>` DI style **without** pulling in an `async-trait` dependency (the
  codebase hand-rolls its async actors). `Send + Sync`; the future is `Send +
  'static` so the event-loop actor can `tokio::spawn` it.
- **`ProcSpec`** (argv / cwd / env / stdin) and **`ProcEvents`** — a handle that
  hides the crate-internal `LoopEvent` enum behind `spawned(pid)` then `exited(code,
  stdout, stderr)`; `exited` consumes the handle so the exactly-one-exit contract is
  enforced by the type. `StdHostProc` (the default) is today's `run_process` verbatim.
- **`ServerInit::host_proc: Option<Box<dyn HostProc + Send>>`** — `Send` so it rides
  `ServerInit` onto the server thread, where `run_io` rebuilds it into the shared
  `Arc<dyn HostProc>` the `EventLoop` actor holds (`Arc::from` → drop `Send` by
  unsize coercion, mirroring the `host_fs` rebuild). `None` = real local processes.

**Exit criteria — met.** `crates/nxvim-server/tests/host_proc.rs`: an in-memory fake
`HostProc` (shared `Arc<Mutex<…>>`) both **records** the argv it is asked to run and
**serves** a result the editor's `on_exit` observes. Faithful, not a no-op — the
fake echoes the *actual* argv back as stdout for a program on no PATH, so the
observed `code = 0` + echoed argv proves the injected host intercepted the spawn (a
real spawn would be `code = -1`); a second test proves each `vim.system` reaches the
host with its own argv (reacts to input, not a canned constant). Regression-clean —
full `nxvim-server` suite (17 binaries incl. `editing` 536, `uv_process`,
`async_runtime`, `blockers` 34), `nxvim` crate, fmt + clippy all green; the local
binaries (`nxvim`, `nxvim-gui`) pass `host_proc: None` and are unchanged.

### Phase 3c — the daemon wire protocol (process half) — ✅ DONE (2026-06-10)

The first slice of the *full split* to actually **carry traffic over a wire**, kept
to the process seam for the same "don't guess the wire ahead of need" reason 3b was:
`HostProc` is already async + event-routed (pid then exit arrive as separate events,
not a return value), so it maps onto a wire with **no impedance mismatch**. Core's
*synchronous* `HostFs` does not — its remote backing has to become an off-tick fetch
(buffer-as-replica), which is a later slice deliberately not guessed here. (Next-slice
direction confirmed with the requester, 2026-06-10.)

Shipped (`crates/nxvim-server/src/daemon.rs`, re-exported from the crate root):
- **The wire** — four `nxvim-rpc` (msgpack) **notifications** correlated by a
  per-spawn `id`: edit-host → daemon `proc_spawn [id, argv, cwd?, env, stdin]` /
  `proc_kill [id]`; daemon → edit-host `proc_spawned [id, pid?]` / `proc_exited [id,
  code, stdout, stderr]`. Notifications (not request/response) because a child's life
  is two events at different times, which a single reply can't model. Transport is any
  `AsyncRead`/`AsyncWrite` pair — an in-process `tokio::io::duplex` today, ssh stdio to
  `nxvim --daemon` in the full split.
- **`RemoteHostProc` (edit-host side, a `HostProc`)** — `connect(reader, writer)` wires
  the RPC link and a **demux task** that fans the daemon's replies out to per-spawn
  channels (an `Inflight` map, `id` → sender). Each `run` mints a wire `id`, registers
  *before* sending `proc_spawn` (so a reply can't race ahead of its receiver), then
  relays `proc_spawned`/`proc_exited` back to the editor's `ProcEvents`. The editor's
  callback id never crosses the wire — routing is purely by the minted id. A dropped
  daemon connection clears `Inflight`, so every in-flight `run` sees EOF and reports a
  `-1` exit rather than leaking its one-shot `on_exit` (the same exactly-one-exit
  contract `StdHostProc` upholds). A drop-in for `StdHostProc` on the local side.
- **`serve_daemon` (daemon side)** — runs each requested child through the **same
  `StdHostProc`** the local server uses today, relaying that machinery's `LoopEvent`s
  straight onto the wire, so a process behaves identically remote and local. Holds a
  per-child kill map mirroring the event-loop actor's `procs`.

**Exit criteria — met.** `crates/nxvim-server/tests/daemon_proc.rs` drives a real
editor whose `host_proc` is a `RemoteHostProc` talking to a `serve_daemon` over an
in-process duplex (the ssh-stdio stand-in): an async `vim.system` runs a **real** `sh`
on the daemon and `on_exit` sees its *actual* stdout (`hello-from-daemon`) — output a
stub can't invent; two concurrent spawns each see their own result (`AAA`/`BBB` —
proving the per-`id` demux, not a shared constant); a non-zero `exit 7` round-trips
faithfully; and `handle:kill()` on a `sleep 30` child fires `on_exit` with `code = -1`
in well under a second (proving `proc_kill` crosses the wire and terminates the child,
not that the sleep elapsed). Regression-clean — full `nxvim-server` suite (now 18
binaries incl. `editing` 536, `async_runtime`, `uv_process`, `host_proc`, `blockers`
34), fmt + clippy `-D warnings` all green; the duplex+daemon and the remote host's RPC
tasks live on the test runtime while the server keeps its own thread, exactly the split
the harness already makes for its client connection.

**Still to do in the full split:** `lsp/manager.rs` (long-lived bidirectional
raw-pipe transport: needs the `write_stdin` + stdout-as-events shape, not
run-to-completion); ~~the **blocking spawn path `vim._system`**~~ ✅ DONE — Phase 3n
below (the *fourth* spawn site the original three-site list missed; it now routes to
the daemon over the `sys_run` wire and blocks on the round-trip via the blocking
bridge, because a `root_dir` shell-out like `cargo metadata` must run *where the
project files are*);
`HostWatch` (the daemon side of `FsEventStart`, today local-only via `notify` —
`serve_daemon` currently drops `LoopEvent::FsEvent` on the floor); the `nxvim
--daemon` binary; and the local edit-host as a client over ssh stdio. (The
`HostFs` half landed next — Phase 3d, via the async `HostFsAsync` seam.)
**`clipboard.rs` is struck from this list** — slating it for daemon-folding
contradicted the plan's own clipboard semantics bullet: with the edit-host
local, `pbcopy`/`xclip` already run on the right machine, so routing them
through the daemon would actively move the clipboard to the *wrong* host. (The
browser needs a different clipboard seam entirely — `navigator.clipboard` via
JS interop, not `HostProc`.) This slice is the wire the rest plug into; the
trait the wire satisfies (`HostProc`) and its routing are now proven end-to-end.

### Phase 3d — the daemon wire protocol (filesystem half, initial open) — ✅ DONE (2026-06-10)

The symmetric companion to 3c, and the first time a *buffer* — not just a process —
crosses the wire. Scoped to the **initial open only**, mirroring how 3a scoped the
`HostFs` *consumption* to the startup file: prove the off-tick fetch end-to-end on the
simplest path, leave `:edit` / `:write` / the explorer for later sub-slices. (Slice
direction confirmed with the requester, 2026-06-10.)

The shape differs from 3c by necessity. Core's [`HostFs`] is **synchronous**, and the
plan is explicit that a daemon-backed read must *not* block the single editor thread on
the network — so the remote fs is **not** a `HostFs` impl. It is a new *async* seam the
**server** consumes off the editor tick (the sync `HostFs` stays reserved for local
disk). A file read is also naturally request/response (one reply, not a two-event
lifecycle), so unlike the process leg it needs no `id`/demux.

Shipped (alongside 3c in `crates/nxvim-server/src/daemon.rs`; the module now carries
*both* legs):
- **The wire** — one `nxvim-rpc` **request**: `fs_read [path]` → `["file", bytes]`,
  `["new"]` (path doesn't exist → a new-file buffer), or a loud RPC **error** (a
  directory — remote explorer is a later slice — or a transport/permission failure;
  never a silent empty buffer). `nxvim_rpc::request` routes the reply by msgid, so the
  edit-host side has no demux.
- **`HostFsAsync` (server-side async seam)** + **`RemoteHostFs`** (its over-the-wire
  impl: `read` issues `fs_read` and awaits the reply). `ServerInit::host_fs_async:
  Option<Box<dyn HostFsAsync + Send>>` rides onto the server thread and is rebuilt into
  an `Arc<dyn HostFsAsync>`, mirroring the `host_proc` rebuild. `None` = today's
  behavior (open the startup file synchronously through the sync `host_fs`).
- **Off-tick open in `run_io`** — when a daemon fs is present, the editor **starts
  empty** and the startup file is fetched by a task spawned *before* the loop; its bytes
  (or error) arrive on a new `tokio::select!` arm that loads them into a **replica**
  buffer via `Editor::load_str` (the in-memory open the web build already uses) and
  repaints. So a slow remote read never freezes startup — the keystroke/redraw path is
  serving the empty buffer the whole time.
- **`serve_fs_daemon` (daemon side)** — answers `fs_read` from an injected sync
  [`HostFs`] ([`StdHostFs`] in the binary, a fake in tests), classifying file / new /
  directory through the existing trait surface (`read_dir` probe + `open_read`), so a
  fake and the real disk behave identically.
- **Replica lifecycle** — `load_str` reuses the throwaway `[No Name]` buffer, which
  already announced its file-less `BufEnter` at startup, so the apply clears it from
  `announced` to let the now-named buffer's `BufReadPost`/`FileType` fire as a fresh
  read (`FileType` drives syntax + LSP), then refreshes the Lua snapshot/mirror.

**Exit criteria — met.** `crates/nxvim-server/tests/daemon_fs.rs`: an editor whose
`host_fs_async` is a `RemoteHostFs` talking to a `serve_fs_daemon` over an in-process
duplex opens a `/virtual/...` path — one the edit-host's *local* disk cannot read — and
its bytes (`fetched / over / the / wire`) appear in the first buffer, named for the
path; the content can only have crossed the wire (the same faithfulness argument 3a's
`host_fs.rs` makes for the sync seam). A second test proves a not-yet-existing path
opens as an empty **new-file** buffer (not an error) with its name bound for a later
`:w`. The `attach` handshake completes before the file loads — evidence the fetch did
not block startup. Regression-clean — full `nxvim-server` suite (now 19 binaries),
`nxvim`/`nxvim-gui` (which pass `host_fs_async: None`, unchanged), fmt + clippy
`-D warnings` all green.

**Still to do on the fs leg (after 3e/3f/3g/3h):** `FileChangedShell` from a daemon
`watch` (a genuinely new wire leg — server-push, the `HostWatch` traffic class). The
async seam + replica pattern these slices established is what it extends. (The **save**
path landed in Phase 3e, `:edit` in Phase 3f, the **remote explorer** — `read_dir` over
the wire — in Phase 3g, and **`:tabnew` / LSP go-to** in Phase 3h, all below. `:read`/`:r`
is *not implemented* in nxvim at all, so there is nothing to route over the wire — it
would be a new feature, not a wire slice.)

### Phase 3i — the watch leg, local behavior (`:checktime` / `'autoread'`) — ✅ DONE (2026-06-10)

The watch leg's foundation, done **local-first** exactly as the read leg was (sync-local
behavior in 3a → remote async in 3d): before the daemon can *push* "a remote file
changed under you," the editor has to know what to *do* when a file changes. That
behavior didn't exist — the only external-change detection was the `:w` clobber guard.

Shipped (all in `nxvim-core`, with the `vim.o` mirror plumbing):
- **`Buffer::disk_change` → `DiskChange { Unchanged, Changed, Vanished }`** — the richer
  form of `disk_changed` that distinguishes a modified file from a deleted one, by
  comparing a fresh stat against the read/write snapshot (the same mtime+size snapshot
  the clobber guard uses).
- **`'autoread'`** — global bool, default **on** (neovim; vim's is off), wired through
  `:set`, `set_global_option_bool`, and the `vim.o` mirror (`GoMirror.autoread`,
  `stdlib.lua` `O_GLOBAL`/`O_GLOBAL_DEFAULT`).
- **`Editor::checktime(target)`** (`:checkt[ime]`) — re-stats every loaded file-backed
  buffer (or one resolved buffer) and reconciles it the way neovim does: an
  externally-changed but *unmodified* buffer is silently reloaded when `'autoread'` is on
  (**W11** warning + no reload when off); a buffer changed on disk **and** in nxvim is a
  **W12** conflict (never clobbered); a vanished file is **E211**.
- **`Editor::reload_buffer(id)`** — the in-place disk re-read `:checktime`'s autoread path
  uses (generalizing `load_into_current` to any buffer): replaces the rope, re-roots the
  undo tree at the reloaded state, **refreshes the disk snapshot** (so the next
  `:checktime` is quiet), and clamps the cursor (live for the current buffer, saved
  otherwise) into the new extent.

**Exit criteria — met.** `crates/nxvim-server/tests/editing/core_editing.rs`: four
black-box tests drive `:checktime` after an external `std::fs::write`/`remove_file` and
assert each branch — autoreload picks up the new content, a modified buffer warns W12
without losing the in-buffer edit, `:set noautoread` warns W11 without reloading, and a
deleted file reports E211 — plus a `vim.o.autoread` default-on + round-trip test in
`options.rs`. Regression-clean; fmt + clippy `-D warnings` green.

**`:checktime` is both the user command and the watcher's entry point** — the remote
`HostWatch` push and any local auto-trigger call `Editor::checktime` / `reload_buffer`
rather than re-deriving the reconcile logic.

### Phase 3j — the watch leg's local auto-trigger (reuse the `vim.uv.fs_event` watcher) — ✅ DONE (2026-06-10)

The reconciliation with the recent `vim.uv.fs_event` work (commit `f7bae73`), built.
The server *already had* a native `notify`-backed watcher actor (`evloop.rs`:
`FsEventStart`/`FsEventStop` → `LoopEvent::FsEvent`), driven only by Lua
(`vim.uv.new_fs_event`, lualine watches `.git/HEAD`). That watcher is the **trigger**
primitive; Phase 3i's `checktime`/`autoread` is the **policy** — complementary layers,
exactly as neovim splits libuv fs_event from `FileChangedShell`, and **non-overlapping**
(3i added a stat-based reconcile, not a second watcher). So the auto-trigger **reuses** the
existing watcher rather than standing up another.

Shipped:
- **One internal (non-Lua) watch per file-backed buffer.** `Server::sync_buffer_watches`
  (called at the tail of `emit_lifecycle_events`, the per-tick chokepoint) declaratively
  reconciles a `buf_watches: HashMap<BufferId, (PathBuf, Option<FileStat>)>` against the live
  buffers: arm new file-backed buffers (`LoopCommand::FsEventStart` on the file path),
  disarm closed ones, and **re-arm on a changed key** — a reload/save re-stamps the disk
  snapshot, so the watch re-points at the (possibly new) inode after an atomic replace.
  Declarative off the buffer set, not hooked into every open/close/rename site.
- **Id space, no side table.** Buffer `b`'s watch id is `INTERNAL_WATCH_BASE (1<<48) + b.0`,
  far above any Lua callback id, so the `LoopEvent::FsEvent` arm classifies by
  `id >= INTERNAL_WATCH_BASE` and routes straight to `editor.checktime_buffer(BufferId(id - BASE))`
  (vs. `lua.run_callback`) — the change reconciles (autoreload / W11 / W12 / E211) and the
  watch re-arms.
- **`Editor::checktime_buffer` / `buffer_watch_key`** — the single-buffer reconcile entry the
  watcher fires, and the `(path, disk-stat)` key the server watches on. `checktime`'s body was
  refactored into a shared `checktime_one(id)` both the ex-command and the watcher use.
- **Local sessions only** — `sync_buffer_watches` no-ops when a daemon `host_fs_async` is set
  (remote watching is the `HostWatch` slice).

Two properties fall out for free: a buffer's own `:w` is **self-suppressed** (`Buffer::write`
updates the disk snapshot synchronously, so the watcher event it triggers makes `checktime`
see `Unchanged`), and **no debounce** is needed (`notify` may emit several events per save, but
after the first autoreload the snapshot is fresh and the rest are no-ops — idempotent by
construction).

**Exit criteria — met.** `core_editing.rs`'s `an_external_change_autoreloads_via_the_buffer_watch`
opens a file, makes an external in-place change with **no `:checktime`**, and polls until the
buffer autoreloads — proving the watch fired and reconciled on its own. Full suite green
(`nxvim-server` 559 in `editing`, all binaries; `nxvim`); fmt + clippy clean.

**Test-determinism note (recorded so it isn't rediscovered):** an always-on watch fires
`checktime` *asynchronously* around the test's own actions, so three existing disk-change
tests needed a synchronizing round-trip to make their precondition (a modified buffer /
`noautoread`) land server-side **before** the external write — otherwise the watch reconciles a
still-unmodified buffer first (a fire-and-forget `feed` race; real edits precede an external
write). And the clobber test now matches the `:w` frame by predicate, because the watch's W12
conflict competes with the clobber message on the line and which is *latest* is timing-dependent.
The product behavior is correct in every case; these are test-ordering fixes.

**Deferred to the next watch-leg slices (deliberately, not stubbed):**
- **The `FileChangedShell`/`FileChangedShellPost` autocmds + `v:fcs_reason`/`v:fcs_choice`.**
  ✅ DONE — Phase 3k below.
- **The remote `HostWatch` push wire.** ✅ DONE — Phase 3l below.
- **Cursor preservation across reload** is line/col-clamped, not view-stable (vim keeps the
  exact screen position); acceptable and consistent with `:e!`. (Still deferred.)

### Phase 3k — the `FileChangedShell` round-trip + `v:fcs_choice` — ✅ DONE (2026-06-10)

Honoring the choice contract needs core to *defer* the decision to the server (the
synchronous Lua round-trip the pure core can't drive), so the reconcile moved one layer up
while detection/reload stayed in core — mirroring neovim's `buf_check_timestamp`:

- **Core defers, doesn't decide.** `:checktime` / `checktime_buffer` now enqueue a
  `pending_checktime` queue (the watch-leg analogue of `pending_saves`/`pending_opens`)
  instead of reconciling inline. `Editor::begin_file_change(id)` does detection plus the
  one part needing no autocmd — the silent `'autoread'` reload of an unmodified buffer
  (neovim reloads *before*, and *without*, firing `FileChangedShell`) — returning a
  `FileChangeAction` (`None` / `Reloaded` / `Autocmd(FileChangeReason)`).
  `Editor::warn_file_change` echoes the default W11/W12/E211; `reload_buffer` is now `pub`.
- **The server owns the round-trip.** `Server::reconcile_file_change` (drained in
  `run_pending`'s fixpoint) fires `FileChangedShell` with `v:fcs_reason` set and
  `v:fcs_choice` reset (the new `LuaRuntime::fire_file_changed` / `fcs_choice`; `vim._fire`
  now returns whether any handler ran), then dispatches: `"reload"`/`"edit"` →
  `reload_buffer` (`"reload"` refused for a deleted file), `"ask"` → the default warning,
  an empty choice → the handler took over (neovim's `return 2`: no post). Every handled
  change that isn't the empty-choice case fires `FileChangedShellPost`.
- **Watch arm routes through the queue.** The internal-watch `FsEvent` arm enqueues via
  `checktime_buffer`; the reconcile + watch re-arm happen in `run_pending` (one place).

**Exit criteria — met.** `core_editing.rs` adds three tests: `FileChangedShell` redirects a
*conflict* to a reload via `v:fcs_choice` (and the handler sees `v:fcs_reason = "conflict"`),
an empty-choice handler suppresses the default W11, and `FileChangedShellPost` fires after an
autoread reload — plus the four existing `:checktime` tests stay green. Found + fixed a latent
bug along the way: `load_str_into` / `reload_buffer` left a freshly-read replica marked
*modified* (`mark_resync` re-set the flag after `mark_clean`), which the reload-vs-conflict
decision newly depends on.

### Phase 3l — the remote `HostWatch` push wire — ✅ DONE (2026-06-10)

Only the daemon can watch a remote file, so it **owns change detection** and the edit-host
reacts to a push (it never stats the remote disk itself):

- **The wire** (`daemon.rs`, the fs leg): `fs_watch [path]` / `fs_unwatch [path]`
  (edit-host → daemon notifications) and `fs_changed [path, stat?]` (the one daemon → edit-host
  *push*). `serve_fs_daemon` baselines each watched path at arm time and re-stats on a coarse
  `WATCH_POLL` interval (the daemon is the lag-tolerant leg), pushing on a drift; a successful
  `fs_write` refreshes the baseline so the edit-host's **own** save can't echo back as an
  external change (self-suppression). `HostFsAsync` grew `watch`/`unwatch`/`take_watch_events`;
  `RemoteHostFs` routes each `fs_changed` into a `WatchEvent` channel the server's new
  `watch_rx` `select!` arm drains.
- **Off-tick reconcile** (`Server::on_remote_file_changed`): the same `FileChangedShell`
  round-trip as the local path, but the reason comes from the push (vanished ⇒ deleted, an
  unsaved buffer ⇒ conflict, else changed) — no local stat — and a reload can't be synchronous,
  so it re-fetches over `fs_read` (`enqueue_reload`) with `FileChangedShellPost` deferred to the
  fetch landing in `apply_open` (`reload_posts`). `sync_buffer_watches` arms the watches on the
  daemon (`remote_watches`) in a daemon session instead of the local `notify`. `:checktime` is a
  no-op in a daemon session now (the always-on watch covers it), not the old "not wired" echo.

**Exit criteria — met.** `daemon_watch.rs`: an external change to an unmodified buffer
autoreloads **over the wire** (a `/virtual/...` path the edit-host's local disk can't hold, with
no `:checktime`), and a `FileChangedShell` handler fires on the edit-host with `v:fcs_reason`
set and its `v:fcs_choice = "reload"` drives the off-tick re-fetch. Full `cargo test --workspace`
green (1255 tests, 60 binaries); fmt + clippy clean.

**The save slice must define its semantics up front — write is the hard half of
off-tick** (read got done first for a reason: it's idempotent and cancelable;
write is neither). The contract for an async `:w`:

- **Snapshot at command time.** `BufWritePre` runs first (it may mutate the
  buffer), then the rope bytes are snapshotted and *those* bytes cross the wire —
  edits made while the write is in flight can never tear into it.
- **Ack-gated state.** The `modified` flag clears, the new `FileStat` is stamped
  (so `disk_changed` doesn't false-positive on our own write), and
  `BufWritePost` fires only when the daemon acks the atomic write — never
  optimistically at send time.
- **Quit waits for the ack.** `:wq` / `:x` / `ZZ` defer their quit effect until
  the write acks; a failure or timeout cancels the quit and surfaces loudly. An
  unflushed write must never be silently abandoned by an exiting editor.
- **Per-buffer serialization.** Overlapping `:w`s on one buffer queue in order
  (snapshot order = wire order); a failed earlier write fails the queue loudly
  rather than letting a later snapshot paper over it.

### Phase 3e — the daemon wire protocol (filesystem half, the save path) — ✅ DONE (2026-06-10)

The symmetric companion to 3d's off-tick *read*, and the **hard half** the contract
above warned about: read is idempotent and cancelable, write is neither. Scoped to the
**single-buffer save** (`:w` / `:w {name}` / `:wq` / `:x` / `:exit`) — the same "prove
the path, defer the rest" discipline 3d used for the initial open. (Slice direction
confirmed with the requester, 2026-06-10.)

The shape mirrors 3d's read: core's [`HostFs`] is **synchronous** and a daemon-backed
write must *not* block the single editor thread on the network (Open Decision #5 —
exactly the frozen-screen-on-`:w` failure this plan exists to kill). So the write goes
**off-tick** too: core does not write through the sync `HostFs` in a daemon session — it
**snapshots the buffer at command time** into a [`PendingSave`] and the server pushes
those bytes over the wire, finalizing the buffer's saved-state only on the daemon's ack.

Shipped:
- **The wire** — one `nxvim-rpc` **request** added alongside 3c/3d in `daemon.rs`:
  `fs_write [path, bytes]` → `["ok", stat?]` (the post-write [`FileStat`] the edit-host
  stamps as its `disk` baseline — no remote stat round-trip) or a loud RPC **error** (a
  permission/transport failure; never a silent success). `serve_fs_daemon` does the
  atomic write through the *same* sync [`HostFs`] the local server uses, so a fake and
  the real disk behave identically.
- **The off-tick save seam in core** (`nxvim-core`): an opt-in `host_save_offtick` flag
  (the server sets it whenever a daemon fs is present), a `PendingSave` queue the server
  drains with `take_pending_saves`, and `finalize_save(buffer, path, stat)` — the
  deferred half of a synchronous `:w` (bind name, stamp `disk`, clear `[+]`, bump
  `save_tick`, record the saved undo node), run only on the ack. `ex_write` enqueues a
  snapshot instead of writing when the flag is set; the disk-change guard is skipped on
  this path (it needs a remote stat we've sworn off the tick — a `HostWatch`-driven
  check is a later slice). `Buffer::to_save_bytes` / `Buffer::mark_written` keep the
  buffer invariants encapsulated.
- **The server orchestration** (`save.rs`): `drain_pending_saves` (at the tail of
  `run_pending`, so a `:w` from a keystroke, `vim.cmd('w')`, or a user command is all
  caught) dispatches each snapshot over `fs_write` on a spawned task, **serialized per
  buffer** (at most one write in flight per buffer; the rest queue in snapshot order). A
  new `select!` arm applies each ack: `finalize_save` + the `"{name}" {lines}L, {bytes}B
  written` echo + dispatch the buffer's next queued write. On failure it surfaces loudly
  and **fails the buffer's whole queue** rather than letting a later snapshot paper over
  the gap.
- **Quit waits for the ack.** `:wq` / `:x` carry a `then_quit` on the pending save; core
  does *not* run the synchronous quit in off-tick mode. The server **replays** the quit
  (`:q` / `:q!`) only after the write finalizes — a clean buffer, so no spurious E37 — and
  a **failed** write *cancels* the quit (the buffer stays modified, the editor stays up).
  An unflushed write is never silently abandoned by an exiting editor.

**Scoped out (fail loud, not silent) — now done in Phase 3m below:** multi-buffer
`:wall` / `:wqa` / `:xa` echoed `E5555: :wall over the daemon is not supported yet` in
off-tick mode rather than silently writing every modified buffer to the *local* disk
(the wrong machine). `BufWritePre`/`BufWritePost`
autocmds aren't emitted anywhere in nxvim yet (the contract's snapshot-after-`BufWritePre`
point is moot until they exist); the observable saved-state is `modified` / `save_tick` /
the `written` echo, all ack-gated here.

**Exit criteria — met.** `crates/nxvim-server/tests/daemon_save.rs`: an editor whose
`host_fs_async` is a `RemoteHostFs` talking to a `serve_fs_daemon` over an in-process
duplex edits a `/virtual/...` buffer (a path its *local* disk can't hold) and `:w`s it —
the **edited** bytes appear in the daemon fake, so they can only have crossed the wire
(the faithfulness argument 3d makes), and `vim.bo.modified` clears **only after** the
ack. A second test proves `:wq` defers its quit until the write acks, then exits with the
bytes already on the daemon. A third proves a **failing** daemon write *cancels* the
`:wq` quit (the editor stays running, the buffer stays modified, the daemon gets nothing)
and surfaces the failure **loudly** on the message line — proving the quit is gated on a
*successful* ack, not fired optimistically. Regression-clean — full `cargo test
--workspace` green (now 20 server test binaries), fmt + clippy `-D warnings` clean; the
local binaries (`nxvim`, `nxvim-gui`) leave off-tick mode off and write synchronously
through the sync `host_fs`, unchanged.

### Phase 3f — the daemon wire protocol (filesystem half, `:edit` over the wire) — ✅ DONE (2026-06-10)

The runtime-open companion to 3d's *initial* open: where 3d fetched the startup file
off-tick, this routes `:edit {file}` through the **same async `HostFsAsync` + replica
path** so opening a second file at runtime crosses the wire instead of reading the
edit-host's local disk. Reuses 3d's read leg verbatim — **no new wire** — and unifies the
two open paths onto one channel and one applier.

The off-tick shape is the read mirror of 3e's save: core does **not** read through the
synchronous [`HostFs`] in a daemon session (that would block the one editor thread on the
network). `:edit` instead **creates an empty buffer named for the file, switches to it,
and enqueues a [`PendingOpen`]**; the server fetches the bytes over `HostFsAsync` off the
editor tick and fills that buffer with [`Editor::load_str_into`]. The keystroke path keeps
serving the (briefly empty) buffer the whole time — a slow remote `:e` never freezes
typing, the exact failure mode Open Decision #5 ruled out for buffer opens.

Shipped:
- **Core** (`nxvim-core`): the off-tick flag generalized `host_save_offtick` →
  `host_fs_offtick` (it now gates reads *and* writes). A `PendingOpen { buffer, path }`
  queue drained with `take_pending_opens`, `enqueue_open`, and **`load_str_into(buffer,
  name, contents)`** — the buffer-*targeted* form of `load_str` (replaces the named
  buffer's text in place, binds the name, marks clean, flags a syntax re-sync, roots a
  fresh undo tree; resets the window's cursor/scroll only when that buffer is current, so
  a mid-fetch buffer switch can't disturb the live window). `ex_edit`'s file cases
  (reload-current, new-file via throwaway reuse or a fresh switched-to buffer) enqueue an
  off-tick open instead of `Buffer::from_file` when the flag is set; `:e dir` (explorer)
  stays sync.
- **Server**: the startup and `:edit` opens were **unified** onto one channel
  (`(BufferId, String, io::Result<FsRead>)`) and one applier — `apply_initial_open` →
  buffer-targeted **`apply_open`** + a shared `load_replica` (now keyed by buffer id, the
  filetype taken from the path so it works whether or not the buffer is current).
  `drain_pending_opens` (tail of `run_pending`, beside `drain_pending_saves`) spawns a
  `HostFsAsync::read` per `PendingOpen` and delivers the result to the `open_rx` arm.
- **Rides for free:** `:split {file}` / `:vsplit {file}` delegate to `ex_edit`, so they
  cross the wire too. **Still sync (documented):** `:tabnew {file}` (its own
  `from_file`), LSP go-to / `jump_to`, and the explorer — later micro-slices on the same
  pattern.
- **`:read`/`:r` is *not implemented* in nxvim** (confirmed: no dispatch arm), so there
  was nothing to route — it would be a new feature, out of this slice.

**Exit criteria — met.** `crates/nxvim-server/tests/daemon_edit.rs`: `:edit
/virtual/other.txt` fills a new buffer with a *second* file's bytes fetched over the wire
(a `/virtual/...` path the local disk can't hold — the 3d/3e faithfulness argument);
`:edit` of a not-yet-existing path opens an empty new-file buffer named for it; and `:e!`
reload-in-place **refetches** over the wire (a content change made on the daemon after
the open shows up after the reload — proving a real re-read, not just a local-edit
discard). Regression-clean — full `cargo test --workspace` green (now 21 server test
binaries, incl. the unchanged `daemon_fs` initial-open suite proving the unification
didn't regress 3d), fmt + clippy `-D warnings` clean; local binaries leave off-tick mode
off and open synchronously, unchanged.

### Phase 3g — the daemon wire protocol (filesystem half, the remote explorer) — ✅ DONE (2026-06-10)

The listing companion to 3d/3f's file open: where those fetch a *file's bytes*, this
fetches a *directory's entries* over the wire so nxvim's in-window file explorer (vim's
netrw) shows the **remote** project tree instead of the edit-host's local disk. Until
this slice a remote directory came back as a loud `fs_read` error ("remote directory open
not yet supported", the placeholder 3d left); now it lists, navigates, and opens entries.
Reuses 3d/3f's `HostFsAsync` + `PendingOpen` + replica machinery — **no new seam, no new
channel** — by adding a third reply shape to the existing `fs_read` request.

The off-tick shape is forced by the same constraint as the rest of the fs leg: core's
[`HostFs`] is **synchronous** and reading a directory through it would block the one
editor thread on the network. So a directory open is *also* an off-tick `PendingOpen`,
and the **reply type** — not a local `is_dir()` stat — decides file vs. directory. That
inverts a synchronous assumption the explorer baked in: `ex_edit`'s `path.is_dir()` and
the explorer's `target.is_dir()` both stat *local* disk, which is meaningless for a
remote path. In a daemon session the decision moves to the daemon (it has the files) and
comes back on the wire.

Shipped:
- **The wire** — a third `fs_read` reply variant alongside `["file", bytes]` / `["new"]`:
  `["dir", canonical_path, [[is_dir, name], …]]`. The daemon's `classify` now returns
  `FsRead::Dir { path, entries }` for a readable directory (canonicalizing the path so the
  edit-host's `../`/descend navigation is unambiguous) instead of the old loud error; the
  entries ride raw and **unsorted** — the edit-host sorts/renders them, keeping the netrw
  sort in one place. (`crates/nxvim-server/src/daemon.rs`.)
- **Core, off-tick directory listing** (`nxvim-core`): `Buffer::from_dir_entries(dir,
  entries)` factors the [`HostFs`]-free sort-and-render core out of `Buffer::from_dir`
  (which now just `read_dir`s then calls it), and `Editor::load_dir_into(buffer, dir,
  entries)` is the directory analogue of `load_str_into` — it builds the listing into an
  already-created buffer (preserving its id, rooting a fresh undo tree, resetting the
  window to the top when current). The explorer's three entry points learned an off-tick
  branch: `enter_dir` (and so `:e dir`, descend, `-`-up) sets up the destination buffer
  and `enqueue_open`s instead of reading sync; `explorer_open_entry` decides dir-vs-file
  from the **listing's trailing `/`** (already authoritative — no remote stat round-trip)
  rather than `target.is_dir()`; `explorer_open_file` opens an empty named buffer and
  enqueues the fetch. `ex_edit` needed no change — a remote dir already isn't a *local*
  `is_dir()`, so it flowed to the enqueue path; only the reply handler is new.
- **Server** (`lifecycle.rs`): `apply_open` gained a `Dir` arm → `load_dir_replica`, the
  directory sibling of `load_replica` — `load_dir_into` + clear `announced` (so the
  now-named buffer's `BufReadPost` fires) + refresh the Lua snapshot/mirror + drive the
  queued autocmds. A directory has no filetype, so no `FileType`/LSP work.
- **Rides for free:** the startup `nxvim <remote-dir>` open (the deferred startup fetch
  hits the same `apply_open`) and `:split`/`:vsplit <remote-dir>` (delegate to `ex_edit`).
  **Still sync (documented):** `:tabnew {file}`; and remote directory **canonicalization**
  beyond what the daemon resolves on the open is not re-statted per navigation (the
  listing's trailing-slash and the daemon's canonical path carry it).

**Exit criteria — met.** `crates/nxvim-server/tests/daemon_explorer.rs`: a server whose
`host_fs_async` is a `RemoteHostFs` over an in-process duplex, backed by a daemon-side
fake that models *directories* (a `read_dir` that succeeds only for registered dirs).
`nxvim /virtual/proj` (startup) and `:edit /virtual/proj` both list the remote dir's
entries — dirs-first, then files by name — a `/virtual/...` tree the edit-host's local
disk can't hold, so the listing crossed the wire (the 3d/3f faithfulness argument);
`<CR>` on `src/` descends into the remote sub-directory and `-` lists the remote parent
again (two more remote `read_dir`s); and `<CR>` on a file row opens that remote file's
bytes over the wire (destroying the listing, as netrw does). Regression-clean — the
unchanged local-disk `editing::explorer` suite (10 tests) proves the sync path didn't
regress; full `cargo test --workspace` green (now 22 server test binaries), fmt + clippy
`-D warnings` clean; local binaries leave off-tick mode off and list synchronously.

### Phase 3h — finish the buffer-open leg (`:tabnew` / LSP go-to over the wire) — ✅ DONE (2026-06-10)

The remaining sync `from_file` sites. 3a/3d/3f/3g routed `:edit`, the startup open, and the
explorer onto the off-tick wire, but **`:tabnew {file}`** and **`jump_to`** (LSP go-to /
diagnostics / the location-list panel) still read the edit-host's *local* disk — so in a
daemon session they'd open the wrong machine's files. The reason they lagged is structural,
and worth recording: nxvim had **four near-identical file-open paths** (`ex_edit`,
`ex_tabnew`, `jump_to`, `explorer_open_file`), each inlining its own
`find_buffer_by_path` → `Buffer::from_file` because the *load* step was never separated from
the *placement* policy (current window in place / a new tab / cursor jump / wipe the
listing). All the off-tick investment landed in `:edit`'s copy; the other three silently
stayed sync.

So this slice **extracts the shared kernel** rather than bolting a fourth and fifth copy of
the off-tick enqueue on:
- **`Editor::load_new_buffer(path) -> Option<BufferId>`** (`nxvim-core`) — the load atom: off-tick
  it creates an empty named buffer and enqueues a `PendingOpen` the server fills over the
  wire; locally it reads `Buffer::from_file`. No find, no placement. `None` = a *synchronous*
  load failed (echoed); off-tick never fails here (errors surface later in `apply_open`).
- **`Editor::open_buffer(path)`** = find-or-`load_new_buffer` (no placement) — for `:tabnew`
  (hands the id to `new_tab`) and `explorer_open_file` (switch, then wipe the listing).
- **`Editor::edit_in_current_window(path)`** = find-or-switch / throwaway-reuse-in-place /
  load-and-switch, off-tick aware throughout — the `:e file` / go-to core, for `ex_edit` and
  `jump_to`.

All four callers now route through this one kernel: `ex_edit`'s open-or-switch tail →
`edit_in_current_window`; `ex_tabnew` → `open_buffer`; `jump_to` → `edit_in_current_window`
(bailing if a sync load fails, so the cursor never lands in a phantom buffer);
`explorer_open_file` → `open_buffer` (replacing the bespoke off-tick branch 3g had added).
`ex_edit`'s **reload-current** (`:e!` re-read) stays its own case — it must re-read even when
the path is already current, which the find-or-switch kernel deliberately does not. The
explorer's dir-navigation (`enter_dir`) keeps its own placement (descend reuses the listing
window — distinct from the four file-open callers) and was left as-is.

**Rides for free:** off-tick **`:tabnew <remote-dir>`** now lists the directory as the
explorer in a new tab (the unified kernel composes with 3g's `FsRead::Dir`); `:split`/
`:vsplit` already delegate to `ex_edit`. **Still sync (local-only, documented):** the
synchronous `:tabnew <dir>` fallback (a local dir errors → empty buffer, unchanged — only
the off-tick path gets the listing).

**Exit criteria — met.** `crates/nxvim-server/tests/daemon_edit.rs` gains
`tabnew_fetches_a_file_over_the_wire`: `:tabnew /virtual/other.txt` fills the *new tab's*
buffer with a `/virtual/...` file the edit-host's local disk can't hold (so it crossed the
wire — the 3d/3f faithfulness argument), and `nvim_list_tabpages` confirms a *second* tab
really opened (not an in-place `:edit`). `daemon_explorer.rs` gains
`tabnew_lists_a_remote_directory_in_a_new_tab`, proving the kernel composes with the remote
explorer. `jump_to` (LSP go-to) routes through the **same** `edit_in_current_window` /
`load_new_buffer` atom these tests exercise over the wire end-to-end; driving it *itself*
over the wire needs a live LSP server or a populated cross-file location list, which the
daemon harness doesn't stand up, so it's proven off-tick **by construction** (shared atom)
with its unchanged cursor/placement logic covered by the local `editing::marks` /
`editing::panel` / LSP suites — a precise weaker claim, not a behavior test asserted but not
run. Regression-clean: the four-way unification left every local suite green —
`editing::explorer` (10), `tabs` (35), `buffers` (27), `host_fs` (3), and the marks/panel/
LSP suites that exercise `jump_to`; the lib/server/core run is 1040 green; fmt + clippy
`-D warnings` clean. (The only red anywhere is pre-existing and environmental — the
`nxvim-web-bridge` relay test times out identically on a clean tree, and the `nxvim` e2e PTY
tests flake under the full-`--workspace` parallel storm but pass in isolation.)

### Phase 3m — the daemon wire protocol (filesystem half, multi-buffer `:wall` / `:wqa` / `:xa`) — ✅ DONE (2026-06-10)

The last fs-leg write slice, and the one 3e explicitly deferred behind an `E5555` stub:
where 3e proved the **single-buffer** off-tick save (`:w` / `:wq` / `:x`), this generalizes
it to the **all-modified-buffers** forms. In a daemon session `:wall` / `:wqa` / `:xa` had
been failing loud (`E5555: :wall over the daemon is not supported yet`) rather than
silently scattering every modified buffer onto the edit-host's *local* disk (the wrong
machine). Now they write every modified buffer over the wire, reusing 3e's per-buffer save
machinery wholesale — **no new wire, no new seam**; the only new concept is the
*all-buffers-ack-then-quit* gate the save contract named.

The write half is trivial once 3e exists: `:wall` snapshots **each** modified file-backed
buffer into its own [`PendingSave`] (the disk-change/conflict guard skipped for the same
reason `:w` skips it off-tick — it needs a remote stat sworn off the tick), and the server's
existing `drain_pending_saves` dispatches them. The buffers are distinct, so they ride the
wire concurrently (the per-buffer serialization in `save.rs` only orders *overlapping* writes
to the *same* buffer); each acks independently with its own `written` echo. No summary echo —
the per-ack echoes carry it.

The **quit** half is the real content. The single-buffer `:wq` rides `PendingSave::then_quit`
(one save → one `:q`); a `:wqa` can't, because it must wait for *every* write of the batch
before quitting — quit too early and the still-`[+]` buffers make `:qa` report `E37` and the
editor never exits. So core hands the server a new [`PendingQuitAll`] (`take_pending_quit_all`):
the `:qa!` bang plus the [`PendingSave::seq`]s of every write the batch enqueued. The server
holds it as a `QuitAllGate`, ticks each seq off as its write acks, and replays `:qa` only when
the set drains — the *all-buffers-ack-then-quit* contract. A **failed** write in the batch
**cancels** the gate (drops it, surfaces loudly), so a failing `:wqa` keeps the editor up with
the unsaved buffer intact — the multi-buffer form of 3e's failing-`:wq`-cancels-the-quit. A
`:wqa` with nothing to save quits inline (no write to wait on), exactly as `:qa` would; a
modified *no-name* buffer still makes the replayed `:qa` report `E37` (the gate watches no
write for it), matching vim.

Shipped:
- **Core** (`nxvim-core`): `enqueue_save_of(buffer, …)` (the buffer-targeted form of
  `enqueue_save`, returning the minted seq); `ex_write_all` grew an off-tick branch that
  enqueues each modified file-backed buffer and returns the seqs; `ex_write_quit_all` (the
  new `:wqa` / `:xa` entry) sets `pending_quit_all` from those seqs off-tick (or quits inline
  when nothing was enqueued), and is plain `:wall` + `:qall` locally. `PendingQuitAll` +
  `take_pending_quit_all`.
- **Server** (`save.rs`): `QuitAllGate` + `drain_pending_quit_all` (records the gate right
  after `drain_pending_saves`, in `run_pending`); `advance_quit_all_gate` (fires `:qa` when
  the seq-set empties) and `cancel_quit_all_gate` (drops it on a batch write failure), woven
  into `apply_save_done`'s success/failure arms.

**Scoped out (unchanged, fail loud where relevant):** the local-disk `:wall` keeps its
disk-change conflict guard + `"{n} buffer(s) written"` summary (off-tick can't stat the
remote on-tick, so it skips the guard and emits the per-ack echoes instead, consistent with
`:w`). `BufWritePre`/`BufWritePost` still aren't emitted anywhere in nxvim yet, so the
snapshot-after-`BufWritePre` point stays moot; the observable saved-state is `modified` /
the `written` echo, ack-gated per buffer.

**Exit criteria — met.** `crates/nxvim-server/tests/daemon_save.rs` gains three tests over an
in-process duplex daemon (a `RemoteHostFs` ↔ `serve_fs_daemon`): two `/virtual/...` buffers
edited then `:wall`ed both land their *distinct* edited bodies on the daemon (content a stub
couldn't invent and the local disk can't hold — the 3d/3e faithfulness argument) and both read
clean only on their acks; `:wqa` writes both **then quits**, with both files already on the
daemon before the exit (the gate waited for the whole batch, not just the first); and a
**failing** `:wqa` does *not* quit, leaves both buffers modified, leaves the daemon empty, and
surfaces the failure loudly. Two negative controls confirmed the tests do real work (stubbing
the off-tick `:wall` enqueue fails the write test; disabling `drain_pending_quit_all` fails the
quit test). Regression-clean — full `cargo test --workspace` green (1019+ tests, the
`daemon_save` suite now 6), fmt + clippy `-D warnings` clean; local binaries leave off-tick
mode off and `:wall` / `:wqa` write synchronously, unchanged.

### Phase 3n — the blocking `vim.system` shell-out over the wire (`sys_run`, the blocking bridge) — ✅ DONE (2026-06-10)

The *fourth* spawn site the original three-site list missed (called out under Phase 3c's
*Still to do*), and the first wire leg whose shape is **neither** off-tick fetch (the fs
leg) **nor** event-routed notifications (the process leg). The **synchronous**
`vim.system(...):wait()` — the form an `lsp/<server>.lua` `root_dir` uses to run `cargo
metadata` / `rustc --print sysroot` during `vim.lsp.enable` — runs *inline on the Lua
tick*: the caller needs the value **now** and has nothing to hand back on a later tick, so
it can't go off-tick like a buffer open. But it must still run **where the project files
are** (the daemon), not the local machine where `Cargo.toml` is meaningless. That forces
the **blocking bridge** Open Decision #5's residual note reserved for exactly the sync
surfaces: a request/response over the wire on which the edit-host **parks its Lua thread**,
with the wire's RPC tasks on their **own** OS thread so the parked thread can't starve the
reader carrying its reply (the deadlock trap). (The *async* `vim.system` with `on_exit`
already rides the Phase 3c `HostProc` wire off-tick — unchanged; only the blocking
`:wait()` form is this slice.)

The shape, kept to the one spawn site whose contract matches (the same "don't guess ahead
of need" discipline as 3b/3c):
- **The seam, in `nxvim-lua`** (`system.rs`): a synchronous `trait BlockingSystem { fn
  run(&self, SystemSpec) -> SystemOutput }` with `SystemSpec` (argv / cwd / env) and
  `SystemOutput` (code / stdout / stderr / pid). `StdBlockingSystem` is today's
  `vim._system` spawn-and-wait logic **factored verbatim** behind the seam — the
  editor-side default (no daemon) *and* the daemon-side backend in the real `nxvim
  --daemon`, where "local" *is* where the project files live. `vim._system` now builds a
  `SystemSpec` and runs it through `Shared::blocking_system` (an `Option<Rc<dyn
  BlockingSystem>>`, `None` = the `StdBlockingSystem` default — a bare/local session is
  byte-for-byte unchanged); `LuaRuntime::set_blocking_system` injects the daemon bridge.
- **The wire** (`daemon.rs`, alongside the fs/process legs): one `nxvim-rpc` **request**
  `sys_run [argv, cwd?, env]` → `[code, stdout, stderr, pid?]` (stdout/stderr as binary so
  non-UTF-8 output survives), or a loud RPC error. Request/response, like the fs read — no
  `id`/demux.
- **`RemoteBlockingSystem` (edit-host side, a `BlockingSystem`)** — `connect(reader,
  writer)` spawns a **dedicated link thread** that owns its *own* current-thread runtime
  and the RPC link; `run` hands the spec to that thread over a plain `std::sync::mpsc`
  channel and **parks the calling (Lua) thread** on a `std` reply channel. Parking with a
  `std` recv — not a tokio primitive — is deliberate: `vim._system` runs *inside* the
  server's tokio runtime, where a tokio `blocking_recv` would panic; a `std` recv just
  parks the OS thread, and the link thread (a different thread) is free to drive the wire
  that delivers the reply. `Send` (it holds only the channel sender) so it rides
  `ServerInit` onto the server thread and is rebuilt into the Lua runtime's `Rc<dyn
  BlockingSystem>`.
- **`serve_sys_daemon` (daemon side)** — runs each `sys_run` through the *same*
  `StdBlockingSystem` the local editor uses, on a `spawn_blocking` pool thread so a long
  shell-out can't stall the reader, so a process behaves identically run here or across the
  wire. `ServerInit::blocking_system: Option<Box<dyn BlockingSystem + Send>>` is the
  injection point; `None` = today's local spawn.

**Exit criteria — met.** `crates/nxvim-server/tests/daemon_system.rs`: an editor whose
`blocking_system` is a `RemoteBlockingSystem` talking to a `serve_sys_daemon` over an
in-process duplex runs `vim.system({...}):wait()` and sees the daemon's result inline — a
tool name **not on the edit-host's `PATH`** comes back `code = 0` with the daemon fake's
echoed argv and a sentinel pid, a result a real *local* spawn could not produce (it would
be `-1`, "failed to spawn"), so the spawn was intercepted across the wire (the
`/virtual/...` faithfulness argument the rest of the suite makes); a second test proves
`cwd`/`env` cross intact; a third proves two distinct calls echo distinctly (reacts to
input, not a canned constant). A **negative control** — dropping the daemon injection so
the local `StdBlockingSystem` runs — flips the first test to `code = -1` (the missing tool),
confirming it genuinely depends on the wire. Regression-clean — full `cargo test
--workspace` green (the two `mouse` flakes are the documented test-shuffle race and pass in
isolation), the `daemon_system` suite is 3, fmt + clippy `-D warnings` clean; the local
binaries (`nxvim`, `nxvim-gui`) leave `blocking_system: None` and spawn locally, unchanged.

**Still to do on the process side of the full split:** ~~`lsp/manager.rs` (the long-lived
bidirectional raw-pipe transport)~~ ✅ DONE — Phase 3o below. The `nxvim --daemon` binary and
the ssh-stdio transport tie all the legs together remain. (`clipboard.rs` stays
local-by-topology, struck from this list under 3c.)

### Phase 3o — LSP over the wire (the long-lived bidirectional-pipe leg) — ✅ DONE (2026-06-10)

The **last** process-shaped seam, and the one whose shape diverges most from every leg
before it. A language server is neither run-to-completion (the `proc_*` leg's pid-then-exit)
nor request/response (`fs_*`/`sys_run`): it is a *long-lived child whose stdio is a raw
bidirectional pipe* — JSON-RPC flowing both ways for the server's whole life, stdout consumed
incrementally, never buffered to an exit. So it does **not** collapse into [`HostProc`] as the
original sketch guessed (`HostProc`'s `run`-to-`exited(stdout)` contract can't model a stream);
it gets its own seam matched to that shape, the same "don't fold a mismatched shape in" discipline
that kept the clipboard local and gave the blocking `vim.system` its own bridge (3n).

The seam is **in `nxvim-lsp`** (where the spawn lived), not the server, because the
[`LspManager`] is what spawns servers. Shipped:
- **`trait LspTransport`** (`crates/nxvim-lsp/src/transport.rs`): `spawn(spec, root) ->
  io::Result<LspChannel>`, where an [`LspChannel`] hands back the server's `stdout`/`stdin`
  (boxed `AsyncRead`/`AsyncWrite`), its `stderr`, and an [`LspProcess`] (`start_kill` + `wait
  -> (code, signal)`). The manager drives its `async-lsp` `run_buffered` loop over whichever
  streams it gets, knowing nothing of where the server runs. **`LocalLspTransport`** is the
  default — today's `tokio::process` spawn lifted verbatim behind the seam (the inline
  `Command`/pipe-take/stderr-drain `run_server_once` did is now its `spawn`). `LspManager::new`
  uses it; `with_transport` injects another. **Zero behavior change** on the local path.
- **The wire** (`crates/nxvim-server/src/daemon.rs`, a fifth leg): six notifications correlated
  by a per-spawn `id` — edit-host → daemon `lsp_spawn [id, program, args, cwd]` / `lsp_stdin
  [id, bytes]` / `lsp_kill [id]`; daemon → edit-host `lsp_stdout [id, bytes]` / `lsp_stderr [id,
  bytes]` / `lsp_exited [id, code?, signal?]`. It streams the *pipe itself* (raw chunks), not a
  result — the structural difference from every prior leg.
- **`RemoteLspTransport`** (edit-host side, an `LspTransport`): each `spawn` mints an `id`,
  registers per-server sinks, and hands the manager an `LspChannel` whose stdout/stderr are a
  `ChannelReader` (an `AsyncRead` fed by demuxed `lsp_stdout`/`lsp_stderr` chunks) and whose
  stdin is a duplex pumped onto the wire as `lsp_stdin`. A demux task fans the daemon's replies
  to the right server by `id`; a dropped link EOFs every reader and resolves every `wait` to
  `(None, None)` (no leaked server). **`serve_lsp_daemon`** (daemon side) spawns the actual
  child through the *same* `tokio::process` machinery the local transport uses and streams its
  pipes back — **joining the stdout/stderr pumps before signaling `lsp_exited`**, so the
  edit-host (which EOFs its reader on exit) never loses trailing output.
- **`ServerInit::lsp_transport`** rides onto the server thread and is rebuilt into the
  `Arc<dyn LspTransport>` the manager holds (mirroring `host_proc`); `None` = local children.

**Exit criteria — met.** `crates/nxvim/tests/lsp/daemon.rs` drives the **real** `nxvim
--__lsp-mock` server through a `RemoteLspTransport` ↔ `serve_lsp_daemon` over an in-process
duplex (the ssh-stdio stand-in): a scripted `publishDiagnostics` renders in the editor —
proving the `didOpen` crossed as `lsp_stdin` to the child *and* its reply crossed back as
`lsp_stdout` (faithful, not a stub — the diagnostic is state only a real round-trip produces);
`gd` lands the cursor on the mock's scripted definition, proving the request/reply path; and a
mock that exits after `initialize` makes the tunneled child die, `lsp_exited` round-trips, the
breaker respawns, and the editor stays fully responsive throughout. Regression-clean — the full
114-test local LSP suite passes unchanged (the `LocalLspTransport` lift didn't regress it), full
`cargo test --workspace` green, fmt + clippy `-D warnings` clean; the local binaries (`nxvim`,
`nxvim-gui`) leave `lsp_transport: None` and spawn servers locally, unchanged. (`clipboard.rs`
stays local-by-topology, struck under 3c.)

### Phase 3p — the Lua-visible filesystem seam (`LuaFs`, the project-facing fs surface) — ✅ DONE (2026-06-11)

The cross-cutting semantic the *Lua-visible filesystem semantics* bullet below named "the
hardest one": plugins read the *project* through `vim.uv.fs_*` and a handful of `vim.fn` fs
builtins, which bound **directly** to `std::fs` (~22 sites in `nxvim-lua/uvfs.rs`, plus
`install.rs`/`host.rs`). In a daemon session that silently hits the *local* machine — the
wrong filesystem — so telescope previewers, LSP `root_dir` detection, and gitsigns would
see the wrong tree. This slice routes that surface through a synchronous **`LuaFs` seam**
(the fs analogue of Phase 3n's `BlockingSystem`), with a daemon **blocking bridge** so a
plugin reads the *remote* project. The **split-brain routing rule was decided up front, not
plugin-by-plugin** (the bullet's demand).

**The rule (now in `architecture.md` and the `luafs.rs` header):** vim-level *project-facing*
fs APIs route through `LuaFs`; raw Lua `io.*`/`os.*`, `require`/`package.path`,
`nvim_get_runtime_file` (runtimepath = local plugins), `vim._read_file` (sources an
`lsp/<name>.lua` *config*), `vim.fn.mkdir` (overwhelmingly a `stdpath`-rooted local data/state
dir), and `stdpath` all stay **local** — plugins and their caches live on the local machine
by design (the divergence from VS Code's remote-extension-host topology).

Shipped:
- **The seam** (`nxvim-lua/src/luafs.rs`): a synchronous object-safe `trait LuaFs` covering the
  whole surface — fd-level `open`/`read`/`write`/`close`/`fstat` (open files are opaque `i64`
  **fd tokens** the impl mints and owns, so the *daemon* holds the real `File`), `stat`/`lstat`/
  `scandir` (materialized in one call — the libuv iterator handle is reconstructed locally over
  the `Vec`, one round-trip per dir), every mutation (`mkdir`/`rmdir`/`unlink`/`rename`/
  `copyfile`/`utime`), `access`/`realpath`/`read_file`/`which`. `StdLuaFs` is today's `std::fs`
  logic factored verbatim behind the seam (the fd table moved from a `uvfs.rs` `thread_local!`
  into a `Mutex`-guarded instance field, so it is `Send + Sync` and doubles as the daemon-side
  backend); a bare/local session is byte-for-byte unchanged.
- **Wiring** (mirroring `blocking_system`): `Shared::lua_fs: Option<Rc<dyn LuaFs>>` resolved by
  `resolve_lua_fs` (lazily installs the persistent local default so fd state outlives a call),
  `LuaRuntime::set_lua_fs`, `ServerInit::lua_fs: Option<Box<dyn LuaFs + Send>>` rebuilt `Box → Rc`
  on the server thread. `uvfs.rs` / `install.rs` / `host.rs` route their project-facing closures
  through it.
- **The wire** (`nxvim-server/daemon.rs`, alongside the sys leg): one `luafs` request carrying
  `["op", args…]` → `["ok", payload] | ["err", msg]`, with `RemoteLuaFs` (the edit-host side, a
  `LuaFs`) the blocking bridge — a dedicated link thread owns the wire + its own runtime, each
  call parks the Lua thread on the reply (`std` channel, so the park can't starve the reader) —
  and `serve_luafs_daemon` running each op through the daemon's real `StdLuaFs` on `spawn_blocking`
  (it owns the fd table the tokens index).

**Scoped out (next slices, not silent gaps):** the short-TTL stat/exists cache the bullet pairs
with the routing (deferred — correctness first); the `nxvim --daemon` binary + WebTransport/QUIC
listener transport that ties every leg together (ssh dropped — Open Decision #2); and the
*paths-are-remote-paths* concern (`getcwd` stays the local cwd —
the path-space split is its own bullet).

**Exit criteria — met.** `crates/nxvim-server/tests/daemon_luafs.rs`: an editor whose `lua_fs` is a
`RemoteLuaFs` talking to a `serve_luafs_daemon` over an in-process duplex, backed by a virtual
in-memory fs serving `/virtual/...` content that exists on no real disk. `vim.uv.fs_stat` returns
the daemon's size + sentinel mtime (a local stat would be nil); `fs_open`+`fs_read`+`fs_close`
round-trips the remote fd token; `fs_scandir` enumerates the daemon dir; `vim.fn.readblob`/
`filereadable`/`executable`/`exepath` resolve against the daemon (the tool is not on the local
PATH); `fs_mkdir` mutates the daemon store, observable on a follow-up stat; distinct paths echo
distinct sizes (reacts to input). Two controls: dropping the injection flips every `/virtual/...`
probe to a local miss, and a local-`StdLuaFs` test round-trips write/read/stat/mkdir/scandir against
a real temp dir (the refactor is behavior-preserving). Regression-clean — full `cargo test
--workspace` green (the plenary-heavy `plugin_compat`/`telescope_e2e` suites included), fmt +
clippy `-D warnings` clean; local binaries leave `lua_fs: None` and hit the disk directly, unchanged.

### Phase 3q — the `nxvim --daemon` binary + the six-leg multiplexer (one stream) — 🚧 both multiplexers DONE; only the QUIC listener remains (2026-06-11)

**Status (2026-06-11): the daemon half *and* the edit-host multiplexer (`connect_daemon`)
both shipped; only the WebTransport/QUIC listener transport remains.** (ssh was the
originally-planned native transport; **dropped 2026-06-11** in favor of the non-ssh QUIC
listener — see Open Decision #2.) The scoping question at the foot of this section was
resolved *daemon-side first*; the edit-host-side multiplexer then landed as its own
focused slice (2026-06-11, **multiplexer-over-stdio**), proven by driving a real
in-process edit-host `Server` against the **real `--daemon` binary** over stdio — the
transport-agnostic stand-in the listener slice will swap for QUIC. What shipped in the
**daemon half** (2026-06-11):
- **The `serve_*_on` extraction** (`daemon.rs`): each `serve_*` is now `connect()` +
  a connection-agnostic `serve_*_on(rpc, incoming, deps)` (its loop verbatim); the
  `serve_*(reader, writer, deps)` wrappers stay, so the six per-leg suites (30 tests)
  pass **unchanged** — the extraction is pure inversion.
- **`run_daemon_io`** (`lib.rs`): `connect` once, fan each inbound message to its leg's
  `*_on` by method namespace (`fs_*` / `proc_*` / `sys_run` / `lsp_*` / `luafs` —
  disjoint), every leg backed by the same `Std*` impl the local server uses; EOF winds
  the legs down and reaps children.
- **`--daemon`** in `main.rs` → `run_daemon()` (a current-thread runtime over
  stdin/stdout, no `ServerInit`, no config), checked before `--server`.
- **`crates/nxvim/tests/daemon_stdio.rs`** drives the **real** `nxvim --daemon` binary,
  exercising three namespaces over one connection (an `fs_read`/`fs_write` round-trip
  issued *while a `proc_spawn` is in flight*, plus a `luafs read_file`) — proving the
  classes coexist demuxed without cross-talk, with byte/stdout round-trips a stub
  couldn't invent. Full `cargo test --workspace` green (83 binaries); fmt + clippy
  `-D warnings` clean; local binaries unchanged.

What shipped in the **edit-host multiplexer** (`connect_daemon`, 2026-06-11):
- **The blocking bridges collapsed onto one shared link** (`daemon.rs`): `RemoteBlockingSystem`
  / `RemoteLuaFs` each owned a *private* link thread + runtime in their single-leg `connect`;
  their job-server loops were extracted into `run_sys_jobs` / `run_luafs_jobs` (the request
  channel flipped `std::sync::mpsc` → tokio `unbounded` so the server can `await` it on a
  shared runtime; the **reply** channel stays `std` — the editor thread still parks on an
  OS-thread recv from inside the server runtime, the deadlock-avoidance property). The
  single-leg `connect`s now just run those helpers, so the per-leg suites pass **unchanged**.
- **`route_lsp_notification`** factored out of `run_lsp_demux` so one demux can route the LSP
  pushes alongside the proc/fs ones.
- **`connect_daemon(reader, writer) -> DaemonClient`**: one dedicated link thread + current-thread
  runtime, `connect` **once**, then `run_sys_jobs` + `run_luafs_jobs` as tasks and a single
  `run_client_demux` that fans every daemon→edit-host *notification* (`proc_spawned`/`proc_exited`
  → the inflight spawn, `fs_changed` → the watch channel, `lsp_*` → `route_lsp_notification`) to
  its leg. Request *responses* (`fs_read`/`fs_write`/`sys_run`/`luafs`) are msgid-routed inside
  `Rpc` and never surface, so the one demux suffices. `DaemonClient`'s five fields drop straight
  into the matching `ServerInit` slots — one connection populates every seam. The async legs hold
  clones of the shared `Rpc` and issue from the server runtime; only wire I/O touches the link
  thread. Re-exported from the crate root.
- **`crates/nxvim/tests/daemon_stdio.rs`** gained `edit_host_drives_a_real_daemon_over_one_stream`:
  it wraps the real `--daemon` child in `connect_daemon` and hands the five seams to a real
  in-process edit-host `Server`, then exercises four classes through the running editor over the
  **one** stdio stream — the off-tick **fs read** (startup open) and **write** (`:w`, with
  `modified` clearing only on the daemon's ack), the **blocking `sys_run`** bridge
  (`vim.system():wait()` — the case that would deadlock if the parked editor thread drove the
  wire), the **watch** push (an external change autoreloads, a lever only the daemon's poller can
  pull), and **`luafs`** (`vim.uv.fs_stat` / `vim.fn.filereadable`). The proc leg is wired
  identically and covered by the daemon-side test above. Full `cargo test --workspace` green
  (0 failed); fmt + clippy `-D warnings` clean; local binaries (`ServerInit::default()` =
  every seam `None`) unchanged.

Still to do (the **listener slice**): the non-ssh **WebTransport/QUIC listener** wiring
(`wtransport` on `quinn`, launch-minted bearer token, self-signed cert pinned TOFU — Open
Decision #2; **ssh is dropped**), the `--daemon --listen` role + a `:connect` CLI, and the
manual no-per-keystroke-latency check. The design below stands and `connect_daemon` is ready
for it: it takes any `AsyncRead`/`AsyncWrite`, so the listener just feeds it a QUIC bidi
stream instead of the `--daemon` child's stdio — the stdio proof carries over verbatim.

The slice that **ties every leg together**. Phases 3c–3p each built a wire leg and
proved it over its *own* `tokio::io::duplex`; this one stands up the actual
`nxvim --daemon` process and carries **all six legs over one ordered stdio stream** —
the transport `ssh host nxvim --daemon` execs. It is the daemon counterpart to
`--server` (`crates/nxvim/src/main.rs`), but inverted: `--server` runs the *whole
editor* remotely (one round-trip per keystroke — the lag this plan exists to kill);
`--daemon` runs *only* fs + process + watch + `sys_run` + LSP + `luafs` remotely while
the editor stays local. No `Editor`, no `LuaRuntime`, no UI, and — unlike `--server` —
**no config sourcing** (`default_runtime` / `init.lua` / runtimepath all stay on the
local edit-host; the daemon is pure I/O).

**The one genuinely new mechanism: a multiplexer, needed symmetrically on both ends.**
Every `serve_*` (daemon side) *and* every `Remote*::connect` (edit-host side) currently
calls `nxvim_rpc::connect(reader, writer)` itself and **assumes it owns the whole
transport** — which is why the per-leg tests each hand it a private duplex. A real
daemon has *one* ssh stdio stream for all six classes, so the legs must share a single
connection. Two properties (verified in the code) make that a clean *router*, not a
rework:

- **The six method namespaces are disjoint** — `fs_*`, `proc_*`, `sys_run`, `lsp_*`,
  `luafs` (and the `proc_`/`lsp_` daemon→edit-host pushes) — so an inbound stream
  demuxes unambiguously on the method string.
- **Request replies are routed by msgid *inside* `Rpc`, not by an embedded responder.**
  `Incoming::Request` carries only `{id, method, params}` (`nxvim-rpc/src/lib.rs`); a
  handler replies via `rpc.respond(id, …)` on any clone of the shared `Rpc`, and
  request *responses* (`fs_read`/`fs_write`/`sys_run`/`luafs` results) are matched by the
  `pending` map and never surface as `Incoming` at all. **So forwarding an `Incoming`
  over an mpsc channel loses nothing**, and concurrent writes from all legs serialize
  safely through `Rpc`'s single `out_tx`. This *is* the "msgpack-RPC already frames
  concurrent requests over one ordered stream" the *Transport & stream multiplexing*
  section counts on for the native (ssh-stdio) path.

**Plan.**

1. **Split each `serve_*` into `connect()` + a connection-agnostic core**
   (`crates/nxvim-server/src/daemon.rs`). Each grows a `serve_*_on(rpc: Rpc, incoming:
   UnboundedReceiver<Incoming>, deps…)` that is its existing loop *minus* the leading
   `let (rpc, incoming) = connect(...)`. The current `serve_*(reader, writer, deps…)`
   stay as thin wrappers (`let (rpc, incoming) = connect(reader, writer);
   serve_*_on(rpc, incoming, deps…)`), so the six per-leg suites
   (`daemon_proc`/`daemon_fs`/`daemon_save`/`daemon_watch`/`daemon_explorer`/
   `daemon_system`/`daemon_luafs`) compile and pass **unchanged** — proving the
   extraction is pure inversion. The watch-leg `fs_changed` push and the proc/lsp event
   forwarders already write through a cloned `Rpc`, so they work identically off a
   shared one.

2. **The daemon-side multiplexer + the binary role.** A new
   `nxvim_server::run_daemon_io(stdin, stdout)`: `connect` once, mint a per-leg
   `unbounded_channel`, `tokio::spawn` each `serve_*_on(rpc.clone(), leg_rx, deps)` —
   `StdHostFs` for `fs_*`, `StdHostProc` (internal to the proc leg), `StdBlockingSystem`
   for `sys_run`, `LocalLspTransport` (internal to the lsp leg), `StdLuaFs` for `luafs`
   — then a demux loop reading `incoming` and routing each message by method prefix
   (`fs_` / `proc_` / `sys_run` / `lsp_` / `luafs`) to the matching `leg_tx`; unknown
   methods drop (the peer is the same build). Then in `crates/nxvim/src/main.rs`, a
   `const DAEMON_FLAG = "--daemon"` early branch → a `run_daemon()` mirroring
   `run_headless` (a `current_thread` runtime, `enable_io().enable_time()`,
   `block_on(run_daemon_io(tokio::io::stdin(), tokio::io::stdout()))`) but with **no
   `ServerInit`, no `default_runtime`** — LSP/process discovery (program/args/cwd) and
   the project tree arrive *on the wire* and resolve against the remote's real
   PATH/filesystem.

3. **The symmetric edit-host-side multiplexer.** A matching `Remote*::on(rpc,
   incoming_rx)` split plus a `connect_daemon(reader, writer) -> (RemoteHostFs,
   RemoteHostProc, RemoteBlockingSystem, RemoteLspTransport, RemoteLuaFs)` that does the
   single `connect()` + a demux loop routing the daemon→edit-host **notifications**
   (`proc_spawned`, `proc_exited`, `fs_changed`, `lsp_stdout`, `lsp_stderr`,
   `lsp_exited`) to the right leg. The request/response legs (`fs_read`/`fs_write`/
   `sys_run`/`luafs`) need no routing here — their replies are msgid-matched inside
   `Rpc`. This is the piece that lets *one* `ServerInit` populate
   `host_fs_async`/`host_proc`/`blocking_system`/`lsp_transport`/`lua_fs` from a *single*
   connection to one `--daemon` child. (The `sys_run`/`luafs` blocking bridges keep
   their dedicated link thread + own runtime so a parked Lua/editor thread can't starve
   the reader carrying its reply — the deadlock trap from Phase 3a's note; that thread
   now drives the *shared* demux rather than a private connection.)

**Lifecycle / shutdown.** EOF on the daemon's stdin (the local editor quit, or ssh
dropped) ends the demux loop (`incoming.recv()` → `None`); the per-leg senders drop,
each leg's loop ends, the runtime is dropped, and the `tokio::process` children (procs +
language servers, spawned `kill_on_drop`) are reaped — no orphaned `rust-analyzer`. On
the edit-host side the ssh child is already `kill_on_drop` (the v1 `SshTransport`), so
closing the window tears the remote down. (Abrupt parent-loss reaping of grandchildren —
ssh `-tt` / process-group behavior — is the clean path tested and the abrupt path
eyeballed.)

**Testing (mirror `crates/nxvim/tests/stdio_server.rs`).** A new
`crates/nxvim/tests/daemon_stdio.rs` spawns the **real** `CARGO_BIN_EXE_nxvim --daemon`
with piped stdio, wraps the child in `connect_daemon`, hands those `Remote*` seams to a
real in-process edit-host `Server`, and asserts a faithful round-trip over the *one*
stream: a temp file the **daemon** holds (a path the edit-host's own disk can't serve)
opens into the buffer; `:w` of an edit lands those bytes on the daemon and `modified`
clears only after the ack; `vim.system{…}:wait()` on a tool *not on the edit-host PATH*
returns the daemon's result; an external write autoreloads over the wire (the watch
push); and `vim.uv.fs_stat`/`vim.fn.filereadable` resolve against the daemon (`luafs`).
The point the per-leg duplex suites can't make: **all six classes coexist on one ordered
stdio stream without cross-talk or head-of-line deadlock.**

**Exit criteria.** `daemon_stdio.rs` green against the real `--daemon` binary; the six
per-leg suites still green (the `connect` → `*_on` extraction was pure inversion); full
`cargo test --workspace` green; fmt + clippy `-D warnings` clean; the local binaries
(`nxvim`/`nxvim-gui`, no `--daemon`) byte-for-byte unchanged (all `Remote*`/`serve_*`
wrappers retained). This is the concrete slice that fulfils *The full split*'s first two
exit-criteria sentences below.

**Deferred (explicitly, not stubbed):**
- **The real listener hop / CLI / `:connect`** — wiring `connect_daemon` onto a
  **WebTransport/QUIC** connection to a `nxvim --daemon --listen` process (`wtransport`
  client, bearer token, TOFU cert pin — Open Decision #2). The *next* slice; this one
  proves the protocol over real-binary stdio (the duplex/stdio stand-in is transport-
  agnostic, so the proof carries to the QUIC wire). **(ssh is dropped — the earlier
  `ssh … nxvim --daemon` + askpass + `NXVIM_REMOTE_CMD` plan no longer applies.)**
- **Path-space** (`getcwd` / buffer names / statusline in the remote's path-space) and
  the **short-TTL stat/exists cache** for `luafs` — both already deferred by Phase 3p.
- **Transport HOL mitigation beyond app-level framing** — the shipped daemon-side
  multiplexer is one ordered stream with msgpack framing (stdio, as `daemon_stdio.rs`
  drives it); the real escape from HOL blocking is the **WebTransport/QUIC listener**,
  now resolved as the native transport (Open Decision #2, RESOLVED 2026-06-11) and built
  once for native + browser. (ssh is dropped — it was never going to escape HOL anyway,
  QUIC can't run under its single TCP stream.)

**Scoping question — RESOLVED (2026-06-11): daemon side first, then the edit-host
multiplexer as its own stdio slice.** The daemon demux was tested faithfully without the
edit-host multiplexer — a raw `nxvim_rpc` client over one stream drives three namespaces
(request/response replies are msgid-routed inside `Rpc`, proc notifications arrive on
`incoming`) — which is what `daemon_stdio.rs`'s first test does against the real binary.
The edit-host-side multiplexer (`connect_daemon`) then landed as a **second focused slice
over the same stdio transport** (not deferred to the QUIC listener): it entailed the real
change to the blocking-bridge threading model (collapsing the `sys_run`/`luafs` dedicated
link threads onto one shared connection), and that change is *exercised by a live editor*
against the real `--daemon` binary in `daemon_stdio.rs`'s second test — fulfilling *The
full split*'s "drives a local edit-host against a daemon" criterion **without** waiting on
the listener, because `connect_daemon` is transport-agnostic and the stdio proof carries
to QUIC verbatim. Only the listener transport itself (QUIC wiring + auth + cert) remains.

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

**The `HostProc` seam (folded in from Phase 1).** Phase 3b **landed the trait and
its in-process default** for the one-shot spawn path (see above) — async +
event-routing, a `run` per child that reports pid + exit as loop events (exactly as
`vim.system` already works via `evloop.rs`). The shape there is run-to-completion
(`ProcSpec` → `ProcEvents::spawned`/`exited`); the daemon-bound, *interactive* shape
the original sketch reached for —

```rust
// the daemon's other half; consumed by the async server, not by core.
trait HostProc {
    async fn spawn(&self, cmd: &Command) -> io::Result<ProcId>; // jobstart / vim.system / LSP / :!
    async fn write_stdin(&self, id: ProcId, bytes: &[u8]) -> io::Result<()>;
    async fn signal(&self, id: ProcId, sig: Signal) -> io::Result<()>;
    // stdout/exit → loop events on the existing evloop channel.
}
```

— is the shape the original sketch reached for, and `lsp/manager.rs` was where it
seemed to fit. **In practice it did not fold into `HostProc`** (resolved in Phase 3o):
a language server's pipe stays open for its whole life with stdout consumed
incrementally, which `HostProc`'s run-to-`exited(stdout)` contract cannot model. So
LSP **landed in Phase 3o** with its own `LspTransport` seam (in `nxvim-lsp`, where the
spawn lives) + the `lsp_*` wire that streams the raw bidirectional pipe — *not*
`HostProc`. The blocking `vim._system` **landed in Phase 3n** — its own
`BlockingSystem` seam + `sys_run` request/response wire (a blocking bridge, *not*
`HostProc`: it's synchronous, the caller parks on the reply rather than routing
pid/exit as loop events). `clipboard.rs` stays **local-by-topology** and is *not*
daemon-routed (same note). The lesson across 3n/3o: each daemon-bound spawn site got
the seam matched to *its* shape (run-to-completion `HostProc`, the blocking
`sys_run` bridge, the streaming `lsp_*` pipe) rather than one trait stretched over
all three.

**Cross-cutting semantics this phase must define:**

- **Buffers are local replicas** (Monaco-style). Open = off-tick fetch via the
  async `HostFsAsync` seam (Open Decision #5, resolved) → populate the rope via
  `Editor::load_str`; save = snapshot the rope and push the bytes back off-tick,
  finalizing the saved-state on the ack. The rope is authoritative for open
  buffers; core sees a normal local buffer. (Initial open landed in Phase 3d, the
  single-buffer **save** in Phase 3e, **`:edit`** in Phase 3f, the **remote explorer**
  listing in Phase 3g, **`:tabnew` / LSP go-to** in Phase 3h, and the multi-buffer
  **`:wall` / `:wqa` / `:xa`** in Phase 3m — every buffer-open path *and* every write path
  is now off-tick, behind one shared kernel. **The fs leg, the watch leg (3i–3l), the
  blocking `vim._system` (Phase 3n), the LSP leg (Phase 3o), and the Lua-visible fs surface
  (Phase 3p) are all complete**; what remains for the full split is the daemon binary / ssh
  transport and the path-space + cache follow-ups noted below.)
- **Lua-visible filesystem semantics — the hardest one. ✅ DONE — Phase 3p above** (the
  `LuaFs` seam + `luafs` wire; the short-TTL stat/exists cache and the `getcwd`/path-space
  half are deferred follow-ups). The Lua VM is local
  (the thesis), but plugins read the *project* through it, and today the bridge
  reaches the disk directly: `vim.uv.fs_*` (`uvfs.rs`, ~22 raw `std::fs` call
  sites), `vim.fn.readfile` / `readdir` / `glob` / `filereadable` /
  `executable` (`install.rs` / `host.rs` in `nxvim-lua`), and the blocking
  `vim.fn.system`. The proposed split-brain rule: **vim-level fs/process APIs
  route through the host seams** (`HostFsAsync` / the blocking bridge /
  `HostProc` — Open Decision #5's residual note) — so telescope previewers, root
  detection, and gitsigns see the *remote* project — while **raw Lua `io.*` /
  `os.*` and `require`/`package.path` stay local**: plugins and config live on
  the local machine (a feature — no remote plugin install needed), and their
  caches/state files are local. This is exactly the consequence of diverging
  from VS Code's remote-extension-host topology; it must be decided and
  documented up front, not discovered plugin-by-plugin. Two corollaries: (1)
  it's an implementation lift — `nxvim-lua` has no `HostFs` handle today and
  needs one threaded in; (2) per-call round-trips amplify (a root detector
  stats a dozen ancestors), so pair the routing with a short-TTL stat/exists
  cache invalidated by `HostWatch`.
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

**Scope (per Open Decision #3, resolved): one web build.** This emscripten edit-host
*replaces* the `wasm32-unknown-unknown` `nxvim-web` — the serverless `WebEditor` and
the `RemoteClient`/Socket.IO bridge both retire into it (see Open Decision #3). The
gating below is the first slice: make `nxvim-lua` (the C-heavy VM, the real risk)
compile under emscripten with `nxvim-ts`/`libloading` and the process/fs hatches gated
on `target_arch = "wasm32"`. **Prerequisite:** the emsdk toolchain (`emcc`) must be
installed and sourced — the Rust `wasm32-unknown-emscripten` *target* alone can't build
the vendored Lua/regex C.

- **Gate out `nxvim-ts` + `libloading`.** Dynamic library loading doesn't exist in
  wasm. `nxvim-lua` pulls `nxvim-ts` (tree-sitter + `libloading`) for the
  `vim.treesitter` binding; feature-gate that binding **out** of the wasm build.
  The browser already does highlighting in JS via web-tree-sitter (project memory
  `web-treesitter-highlighting`, `docs/architecture.md` → *The web build*), so no
  capability is lost — the redraw just omits server-side highlight spans and the
  JS layer paints them, as `nxvim-web` does today.
- **Gate the process/fs escape hatches.** `nxvim-lua` reaches `std::process`
  directly (the blocking `vim._system`) and `std::fs` directly (`uvfs.rs`,
  `vim.fn.readfile`/`readdir`/`glob`): there are no subprocesses in a browser,
  and the Worker's "local fs" is meaningless. Per *No silent stubs or skips*
  these must **fail loud** on wasm (until Phase 6 routes them to the daemon /
  OPFS) — not link against emscripten's stubs and quietly return junk. The
  clipboard likewise: `navigator.clipboard` via JS interop, not a shell-out.
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

**Progress (2026-06-10) — concept VALIDATED via a throwaway demo.** The
risky-unknown half of Phase 4 is green, proven by behavior in a real wasm engine:

1. **`nxvim-lua` compiles to `wasm32-unknown-emscripten` (`lua51`).** Gated
   `nxvim-ts`/`libloading` off wasm (a `cfg(not(wasm32))` dependency + three gated
   call sites in `runtime.rs`; the browser highlights in JS). Hit and fixed a
   portability bug the plan hadn't called out: **`mlua::Integer` is `i32` on wasm32**
   (`lua_Integer` = `ptrdiff_t`), not `i64` — 11 type errors fixed with the
   `lua_int`/`lua_i64` helpers in `convert.rs` (identity on native, so host
   `clippy -D warnings` + the full `nxvim-server` suite stay green). Project memory:
   `wasm32-mlua-integer-is-i32`.
2. **core + Lua run *together* in one wasm module.** A throwaway demo crate —
   `crates/nxvim-edithost-demo/` (workspace-excluded, **marked TEMPORARY/DELETE-ME**
   in every file) — wires `nxvim_core::Editor` + `nxvim_lua::LuaRuntime` with the
   crudest sync tick (`editor.input` + `lua.eval` + drain `take_commands` →
   `editor.command`, mirroring `effects.rs`), links via `emcc` (staticlib + the
   `mlua-sys`/`nxvim-regex` C archives) into an ES module, and a node harness asserts:
   vim-key insert → buffer; `return 1+41` → `42`; `#vim.split("a,b,c",",")` → `3`
   (the `vim.*` prelude runs in wasm); `vim.cmd("%s/hello/LUA/")` mutates the buffer
   (Lua → editor). All pass. (It also confirmed the **fail-loud** convention survives
   wasm — an unimplemented `vim.fn.abs` raised loudly rather than returning junk.)

**Still to do for Phase 4 proper** (the demo deliberately skips these — it is *not*
the edit-host): the real edit-host reuses `nxvim-server`'s synchronous tick
(`apply_lua_effects` + the buffer/option/register **mirrors** that let Lua *read*
editor state, autocmds, redraw projection) behind an async-effect seam — which is the
larger "extract the sync edit-host" refactor (Open Decision #6 below). The throwaway
demo gets **deleted** when that lands. The fail-loud process/fs/clipboard hatches
(this phase's third bullet) are also still to wire — deferred until the wasm runtime
exists to exercise them (Phase 5).

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
- **Timers in the Worker.** Native `vim.defer_fn` / `vim.uv` timers ride
  `evloop.rs` (tokio), which doesn't exist in the wasm edit-host — the plan
  needs a Worker-side analog of `LoopEvent::Timer`. The SAB park *is* the event
  loop: `Atomics.wait` takes a timeout, so set it to the next-due timer's
  deadline and the same park that wakes on input doubles as the timer wheel —
  one mechanism, no busy loop. (Statusline refresh à la lualine depends on
  timers firing.)
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
  its hash passed to the browser `WebTransport` constructor. **The cert buys
  encryption, not authorization** — a daemon executes arbitrary processes, so an
  unauthenticated listener is remote code execution by design. Ship auth from day
  one: a bearer token minted at daemon launch and presented on connect. The same
  requirement applies to Open Decision #2's native QUIC listener — and since **ssh is
  dropped** (no transport inherits auth for free anymore), the bearer token is the
  single auth mechanism for both native and browser. (mTLS was considered and rejected:
  the browser `WebTransport` client-cert story is awkward and would split native vs
  browser auth.)
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
- **Native (Phase 3):** the native transport is the **same WebTransport/QUIC listener**
  (Open Decision #2, RESOLVED 2026-06-11) — **ssh is dropped**, not kept as a fallback.
  An earlier draft carried the daemon over `ssh … nxvim --daemon` (a single ordered
  stdio stream), but QUIC can't go under ssh's one TCP connection, so its HOL blocking
  is intrinsic and app-level framing can't escape it. Rather than ship a second,
  weaker native transport, native and browser unify on the QUIC listener — one stream
  per `HostServices` class plus one per live `HostProc`. The cost ssh covered for free
  (auth, server identity) moves into the listener: a launch-minted bearer token and a
  self-signed cert pinned TOFU (see Open Decision #2).

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

1. **`HostServices` granularity** — **RESOLVED (2026-06-10): split, by the shipped
   code.** `HostFs` lives in core (sync, Phase 1) and `HostProc` in the server
   (async + event-routed, Phase 3b) — separate traits, separate homes, and the
   daemon wire (3c) is per-class; `HostWatch` follows the same pattern. The split
   is also the prerequisite for per-class stream multiplexing (distinct traits →
   distinct logical channels → distinct transport streams, so a `HostProc` flood
   can't HOL-block an `HostFs` save; see *Transport & stream multiplexing* under
   Phase 6).
2. **Daemon discovery/launch** (Phase 3) — **RESOLVED (2026-06-11): yes, the native
   transport is the non-ssh `--daemon --listen` WebTransport/QUIC listener — the same
   transport Phase 6's browser path uses.** ssh stdio carries every leg over a single
   ordered stream, so HOL blocking is intrinsic there (a `HostProc` flood stalls an
   `HostFs` save queued behind it); app-level framing can't escape it because the bytes
   are already committed to one socket's buffer. A non-ssh listener on **`wtransport`
   (on `quinn`)** gives native the same independent-stream story as the browser — one
   QUIC stream per `HostServices` class (`HostFs`/`HostProc`/`HostWatch`) plus one per
   live `HostProc` — so the native and browser daemon transports **unify on one
   stack** instead of diverging (ssh-stdio for native, WebTransport for browser).
   **ssh is dropped** — not kept as a fallback. The single ordered stdio stream it
   carries has intrinsic HOL blocking QUIC can't escape (QUIC can't run under ssh's one
   TCP connection), so keeping it would mean shipping a second, strictly-weaker native
   transport and splitting auth (ssh's vs the listener's). Instead there is one native
   transport, the QUIC listener, and ssh's free conveniences (auth, identity, launch)
   move into it explicitly below. The Phase 3 deferred ssh slice (the `ssh … nxvim
   --daemon` connector, `:connect`, askpass) is therefore **dropped**, not deferred.

   **Forced sub-decisions (ssh gave these for free; the listener must provide them):**
   - **Auth — launch-minted bearer token.** Per Phase 6, "the cert buys encryption,
     not authorization"; an unauthenticated daemon listener is RCE by design. The
     listener mints a bearer token at `--daemon --listen` startup and requires it on
     the WebTransport CONNECT; native and browser present it identically (token over
     mTLS specifically because the browser `WebTransport` client-cert story is awkward
     and would split native vs browser auth). A network boundary (WireGuard/Tailscale,
     or binding a private interface) is **optional defense-in-depth, not a substitute**
     — "network-trusted" alone contradicts this day-one requirement, and no off-the-
     shelf reverse proxy (nginx et al.) can gate raw QUIC-carrying-msgpack anyway, so
     the gate lives in the daemon.
   - **Server identity — self-signed cert + TOFU pinning.** The daemon generates a
     self-signed cert (`wtransport`'s generator); the client pins its hash on first
     connect (the browser passes the hash to the `WebTransport` constructor, the known-
     hosts model) and warns on change. No CA infrastructure.
3. **One web build or two** (Phase 4) — **RESOLVED (2026-06-10): one build,
   emscripten only.** The emscripten edit-host *replaces* today's
   `wasm32-unknown-unknown` `nxvim-web` outright — no second no-Lua core-only demo
   build. **Both** of today's web clients fold into the single edit-host:
   - the **serverless `WebEditor`** (`crates/nxvim-web/src/lib.rs`, core-only, no
     Lua) and its bespoke *local* paint path in `index.html` (the
     `serverStyled === false` branches) are deleted — the edit-host *is* the local
     editor now, with Lua;
   - the **`RemoteClient` + Socket.IO bridge** (`remote.rs` + `nxvim-web-bridge`)
     is superseded too: it is the whole-editor-*remote* topology this plan exists
     to kill (one round-trip per keystroke). The editor moves *into* the browser
     Worker; only fs/process stay remote, behind the daemon (Phase 6). `remote.rs`'s
     *synchronous* msgpack framing is reusable for the new browser↔daemon link, but
     the boundary flips, and `nxvim-web-bridge`'s per-connection `nxvim --server`
     relay retires with the Socket.IO client.

   The trade: we give up the smallest-possible no-Lua demo for **one** web client
   to maintain and a single feature ceiling (`lua51`). The size cost is accepted
   (the Lua VM alone is ~387 KB, Phase 0; the full edit-host is larger but still a
   one-time download). Resolving this *down* to one build is the whole point of the
   "I don't need two web clients" simplification that prompted this decision.
4. **Redraw transport in the browser** (Phase 5): pull (redraw = return value of
   the input call) vs. push (`EM_JS` notification). Pull is simpler and matches
   the single-threaded Worker; push matches the existing notification model.
5. **Remote `HostFs` shape** — **RESOLVED (2026-06-10): off-tick fetch, by Phase
   3d; affirmed on review.** Buffer opens go through the async `HostFsAsync`
   seam (server-side, request/response, `Editor::load_str` replica) — the editor
   thread never blocks on the network; the sync `HostFs` stays reserved for
   local disk. The blocking-bridge alternative was rejected for buffer I/O
   because a sync call parks the server thread — the thread that processes
   input *and emits redraws* — so a slow remote `:e` would be a frozen screen
   with queued keystrokes (not even a spinner is paintable), on the most common
   operation in a remote session: exactly the failure mode this plan exists to
   eliminate. The bridge survives only where semantics force it (the sync
   surfaces in the residual note below). The
   accepted cost: each remaining fs-touching call site (`:edit`/`:read`, save,
   explorer `read_dir` — Phase 3d's *Still to do on the fs leg*) is re-plumbed
   onto the async seam individually rather than swapped behind the Phase-1
   trait. **Residual:** the *synchronous* surfaces — blocking `vim._system`,
   and any Lua-visible sync fs calls routed remote (`vim.uv.fs_stat`,
   `vim.fn.filereadable`, …) — cannot use an off-tick shape (the caller needs
   the value *now*) and need the **blocking bridge**: a request over
   a channel to the daemon link, editor thread parked until the reply, with the
   link's RPC tasks on their **own** thread/runtime so the parked thread can't
   starve the reader carrying its reply (the deadlock trap — see *Still to do
   in Phase 3* under Phase 3a), plus the short-TTL stat/exists cache to damp
   per-call round-trips. **The bridge is now built and proven — Phase 3n shipped it
   for the blocking `vim._system`** (the `BlockingSystem` seam + `sys_run` wire +
   dedicated-link-thread park, exactly this mechanism); the Lua-visible sync fs
   calls reuse the same bridge when the Lua-visible fs-semantics slice lands.
6. **How the wasm edit-host gets the editor+Lua sync tick** (Phase 4/5) — the
   tick (`dispatch` → `run_pending` → `apply_lua_effects` + the mirrors) is
   synchronous but lives in `impl Server`, entangled with the async fields
   (`tokio` net→`mio`, `notify`, `nxvim-lsp` subprocess, `nxvim-ts`). Three shapes:
   **(a) extract a reusable sync `EditHost`** from `Server` with async effects
   behind a trait — the blessed architecture (it *is* the "full split" seam, serving
   both native latency in Phase 3 and wasm here), but the largest refactor;
   **(b) gate `nxvim-server` itself to wasm** (target-off `net`/`process`, native
   deps non-wasm, current-thread tokio in the Worker) — reuses all glue but keeps
   tokio in the Worker, against this plan's grain; **(c) a minimal fresh cdylib**
   reimplementing a crude tick. **Interim decision (2026-06-10): (c), as a
   throwaway, to de-risk first** — `crates/nxvim-edithost-demo` (above) proved
   core+Lua-in-wasm by behavior. The empirical finding that makes (a)/(b) tractable:
   the wasm blocker is the **dependency tree** (`mio`/net, `notify`, lsp, ts), not
   `nxvim-server`'s own source, which produced *zero* errors before the build died
   at `mio`. **Still to resolve: (a) vs (b) for the real edit-host.** (a) is
   recommended — no tokio in the Worker, one sync core for native + wasm.
```
