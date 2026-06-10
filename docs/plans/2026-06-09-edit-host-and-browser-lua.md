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
run-to-completion); the **blocking spawn path `vim._system`**
(`nxvim-lua/src/install.rs`) — a *fourth* spawn site the original three-site list
missed, which must route to the daemon and block on the round-trip, because a
`root_dir` shell-out like `cargo metadata` must run *where the project files are*
(the blocking-bridge mechanism of Open Decision #5's residual note);
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

**Still to do on the fs leg:** `:edit` / `:read` and the **save** path over the wire
(both still use the sync `host_fs`, i.e. local disk, in a remote session today), remote
**directory/explorer** listing (`read_dir` over the wire — currently a loud error), and
`FileChangedShell` from a daemon `watch`. The async seam + replica pattern this slice
established is what those extend.

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

— is what the **remaining daemon-bound spawn sites** need and is best resolved
against the actual wire: `lsp/manager.rs` (a language server *is* "spawn + pipe
stdio" — no special LSP protocol needed, but it does need raw bidirectional pipe
handles, i.e. the `write_stdin` + stdout-as-events shape, not run-to-completion)
and the blocking `vim._system` (a sync request/response over the same wire, via
the blocking-bridge mechanism — see the *Still to do* note under Phase 3c).
`clipboard.rs` stays **local-by-topology** and is *not* daemon-routed (same
note). The in-process impl wraps today's `tokio::process`; the remote impl
forwards to the daemon. **LSP needs no special protocol** — it collapses into
`HostProc`.

**Cross-cutting semantics this phase must define:**

- **Buffers are local replicas** (Monaco-style). Open = off-tick fetch via the
  async `HostFsAsync` seam (Open Decision #5, resolved) → populate the rope via
  `Editor::load_str`; save = push bytes back. The rope is authoritative for open
  buffers; core sees a normal local buffer. (Landed for the *initial* open in
  Phase 3d; `:edit` / save / explorer listing are the remaining fs-leg slices.)
- **Lua-visible filesystem semantics — the hardest one.** The Lua VM is local
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
  one: a bearer token minted at daemon launch and presented on connect (or mTLS).
  The same requirement applies to Open Decision #2's native QUIC listener; only
  the ssh-stdio path inherits its auth (from ssh) for free.
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

1. **`HostServices` granularity** — **RESOLVED (2026-06-10): split, by the shipped
   code.** `HostFs` lives in core (sync, Phase 1) and `HostProc` in the server
   (async + event-routed, Phase 3b) — separate traits, separate homes, and the
   daemon wire (3c) is per-class; `HostWatch` follows the same pattern. The split
   is also the prerequisite for per-class stream multiplexing (distinct traits →
   distinct logical channels → distinct transport streams, so a `HostProc` flood
   can't HOL-block an `HostFs` save; see *Transport & stream multiplexing* under
   Phase 6).
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
   the value *now*) and will still need the **blocking bridge**: a request over
   a channel to the daemon link, editor thread parked until the reply, with the
   link's RPC tasks on their **own** thread/runtime so the parked thread can't
   starve the reader carrying its reply (the deadlock trap — see *Still to do
   in Phase 3* under Phase 3a), plus the short-TTL stat/exists cache to damp
   per-call round-trips.
```
