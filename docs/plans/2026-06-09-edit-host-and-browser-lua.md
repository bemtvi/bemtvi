# The local edit-host, the remote daemon, and Lua in the browser — implementation plan

> **Update (2026-06-11): classic remote removed.** The "whole editor runs remote,
> thin client local" topology this plan was written *against* has since been deleted
> outright — `bemtvi --server`, the `bemtvi-gui` SSH client (`:connect`, askpass), and
> the Socket.IO web bridge (`bemtvi-web-bridge` + the wasm `RemoteClient`) are all
> gone. The browser is **serverless only**. References below to `--server` as
> "today's topology," and to the `RemoteClient`/Socket.IO path "retiring" into the
> edit-host (Open Decision #3), are therefore historical: those pieces no longer
> exist to retire, and the edit-host / daemon split is the *only* remote story. The
> retired `docs/plans/2026-06-09-remote-ssh-client.md` and
> `…-remote-web-client-over-socketio.md` plans were deleted with the code.

## Why this document exists

Remote bemtvi is **laggy**, and the lag is structural, not tunable. Today's
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
the **remote**, while bemtvi keeps the plugin runtime (Lua) **local** —
deliberately, because plugins are latency-sensitive UI (statusline per
keystroke, key-hint popups, fuzzy-finder sorters) and running them remote would
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
absent (serverless) or reached over WebSocket. `bemtvi-web` today is core+view
only (no Lua); this plan brings the Lua-bearing edit-host to wasm.

### What this changes about the thesis

Principle #3 ("Client-server, always; thin clients, headless server") bends — but
only in *topology*, not semantics:

- **"Identical editing behavior everywhere"** — *kept*. `bemtvi-core` is unchanged
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
| 3 | Can the Worker **wait for input synchronously** without freezing the page? | ✅ The Worker parks on `Atomics.wait` against a `SharedArrayBuffer`, fed by the UI thread; wakes in ~0–1 ms, no spin. **Confirms Worker+SAB → no Asyncify needed.** |

Two facts these pin down, both load-bearing for the plan:

- **LuaJIT is permanently out in wasm** (mlua: WASM supports all versions *excluding
  JIT*). The browser is forever on the `lua51` backend (PUC Lua 5.1) — so a config
  relying on LuaJIT-only behavior (`ffi`, `bit`) won't run there.
- **The emscripten EH gotcha:** rust 1.96 links the emscripten target with new
  wasm exceptions (`-fwasm-exceptions`) but `cc` compiles vendored Lua with the
  legacy EH → `undefined symbol: __cxa_find_matching_catch_3`. Fix:
  `EMCC_CFLAGS=-fwasm-exceptions` so both halves agree.

Everything past the spikes is **engineering with known shapes**, not feasibility.

---

## The one constraint that shapes everything

**`bemtvi-core` and the Lua VM are `!Send` and live on a single thread** (same as
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
| 0 | Feasibility spikes (compile / interop / input wait) | — | ✅ |
| 1 | The `HostFs` I/O seam in core (dependency inversion) | 0 | ✅ |
| 3 | Native edit-host / daemon split + the `HostProc` seam | 1 | ✅ (3a–3r; QUIC listener done — only path-space / `luafs` cache / per-class stream split remain as noted follow-ups) |
| 4 | wasm edit-host: compile (gate `bemtvi-ts`, emscripten build) + extract sync `EditHost` (OD#6 (a)) | 1 | ✅ (compile de-risked; `EditHost` extraction 4a–4e done) |
| 5 | wasm edit-host: Worker + input/timer loop + JS interop | 4 | ✅ (5a feature seam · 5b wasm `HostEffects`/cdylib · 5c Worker/`postMessage` redraw/`window.__bemtvi` · 5d SAB input/timer park · 5e COOP/COEP serving docs + demo deletion — all done) |
| 6 | Browser fs/process: daemon over WebTransport (or serverless OPFS) | 3, 5 | ✅ (6a serverless OPFS fs + explorer done; 6b the **WebTransport daemon fs leg** — browser `:e`/`:w`/`:e <dir>` over a real `--daemon --listen` — done; 6c the **watch leg** — daemon→browser `fs_changed` pushes autoreload / `FileChangedShell` over WebTransport — done; 6d the **proc leg** — async `vim.system`/`jobstart` over WebTransport, daemon→browser `proc_spawned`/`proc_exited` pushes — done; the **luafs legs** (`btv.fs` off-tick `luafs_op` + streaming `luafs_watch` — landed under `docs/plans/2026-06-16-btv-fs-off-tick-daemon-leg.md`) and the **terminal leg** (`term_*` PTY, Phase 7) also landed browser-side since; **6e the LSP leg** is **done** (browser `vim.lsp.start`/diagnostics/hover over a real `--daemon --listen` via the in-Worker `SyncLspClient` ↔ the daemon's `lsp_spawn`/`lsp_stdin`/`lsp_kill` wire — Stages A–F below); the **sys_run** leg is MOOT — the blocking `btv._system` vertical was **removed** entirely under "no blocking IO at all" (commit `474813f`, [[no-blocking-io-fs-async-only]]), so there is no browser sys_run leg to build. Browser edit-host fs/process/LSP/terminal are feature-complete.) |

Phase 1 is independent and small. Phase 3 is the
native latency payoff. Phases 4–5 are the browser payoff. Phase 6 unifies them on
the one daemon. Each phase is sized to be picked up in a focused session with only
its dependencies loaded.

---

## The keystone: the `HostFs` seam (Phase 1) — ✅ DONE

Every later phase pivots on **dependency-inverting `bemtvi-core`'s I/O**. Core
defines the interface it needs; the default implementation wraps the local disk,
and Phase 3 swaps in a daemon-backed one — the editing logic never knows which.

**Scope decision (2026-06-10): Phase 1 is the filesystem seam only.** Process
spawning (`HostProc`) is *already* isolated server-side in an async actor
(`evloop.rs`) and its trait shape is coupled to Phase 3's daemon wire protocol
(it's async + event-routing — stdout/exit come back as loop events, not a return
value). Guessing that shape ahead of the daemon invites rework, so it moves to
**Phase 3**. The high-value, core-touching half — the part that made the "pure
core" thesis real — is the fs seam, and it landed here.

Shipped (`crates/bemtvi-core/src/host.rs`), a **synchronous** trait + a real-disk
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

Two points that keep this honest against `bemtvi-core stays pure and synchronous`:

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

**Exit criteria — met.** `crates/bemtvi-server/tests/host_fs.rs`: an in-memory fake
`HostFs` (shared `Arc<Mutex<…>>` the test inspects) both **serves** the initial
buffer (a `/virtual/...` path that never touches disk) and **captures** `:w` — and
a bare-session `:write <path>` also lands in the fake. Faithful, not a no-op: the
fake genuinely round-trips bytes the editor read and wrote. Regression-clean —
`editing` (536), `buffers` (27), `bemtvi` crate, fmt + clippy all green; the local
binaries (`bemtvi`, `bemtvi-gui`) pass `host_fs: None` and are unchanged.

**Still to do in Phase 3:** the daemon wire protocol + `bemtvi --daemon`, the local
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
`btv._system`, sync Lua fs calls), requires the daemon link's RPC tasks to live
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
long-lived bidirectional raw-pipe transports living in `bemtvi-lsp` — both diverge
from the sketch, so folding them in is a later slice matched to the wire rather
than guessed now. (Scope confirmed with the requester, 2026-06-10.)

Shipped (`crates/bemtvi-server/src/host.rs` — server-side, **not** core, because the
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

**Exit criteria — met.** `crates/bemtvi-server/tests/host_proc.rs`: an in-memory fake
`HostProc` (shared `Arc<Mutex<…>>`) both **records** the argv it is asked to run and
**serves** a result the editor's `on_exit` observes. Faithful, not a no-op — the
fake echoes the *actual* argv back as stdout for a program on no PATH, so the
observed `code = 0` + echoed argv proves the injected host intercepted the spawn (a
real spawn would be `code = -1`); a second test proves each `vim.system` reaches the
host with its own argv (reacts to input, not a canned constant). Regression-clean —
full `bemtvi-server` suite (17 binaries incl. `editing` 536, `uv_process`,
`async_runtime`, `blockers` 34), `bemtvi` crate, fmt + clippy all green; the local
binaries (`bemtvi`, `bemtvi-gui`) pass `host_proc: None` and are unchanged.

### Phase 3c — the daemon wire protocol (process half) — ✅ DONE (2026-06-10)

The first slice of the *full split* to actually **carry traffic over a wire**, kept
to the process seam for the same "don't guess the wire ahead of need" reason 3b was:
`HostProc` is already async + event-routed (pid then exit arrive as separate events,
not a return value), so it maps onto a wire with **no impedance mismatch**. Core's
*synchronous* `HostFs` does not — its remote backing has to become an off-tick fetch
(buffer-as-replica), which is a later slice deliberately not guessed here. (Next-slice
direction confirmed with the requester, 2026-06-10.)

Shipped (`crates/bemtvi-server/src/daemon.rs`, re-exported from the crate root):
- **The wire** — four `bemtvi-rpc` (msgpack) **notifications** correlated by a
  per-spawn `id`: edit-host → daemon `proc_spawn [id, argv, cwd?, env, stdin]` /
  `proc_kill [id]`; daemon → edit-host `proc_spawned [id, pid?]` / `proc_exited [id,
  code, stdout, stderr]`. Notifications (not request/response) because a child's life
  is two events at different times, which a single reply can't model. Transport is any
  `AsyncRead`/`AsyncWrite` pair — an in-process `tokio::io::duplex` today, ssh stdio to
  `bemtvi --daemon` in the full split.
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

**Exit criteria — met.** `crates/bemtvi-server/tests/daemon_proc.rs` drives a real
editor whose `host_proc` is a `RemoteHostProc` talking to a `serve_daemon` over an
in-process duplex (the ssh-stdio stand-in): an async `vim.system` runs a **real** `sh`
on the daemon and `on_exit` sees its *actual* stdout (`hello-from-daemon`) — output a
stub can't invent; two concurrent spawns each see their own result (`AAA`/`BBB` —
proving the per-`id` demux, not a shared constant); a non-zero `exit 7` round-trips
faithfully; and `handle:kill()` on a `sleep 30` child fires `on_exit` with `code = -1`
in well under a second (proving `proc_kill` crosses the wire and terminates the child,
not that the sleep elapsed). Regression-clean — full `bemtvi-server` suite (now 18
binaries incl. `editing` 536, `async_runtime`, `uv_process`, `host_proc`, `blockers`
34), fmt + clippy `-D warnings` all green; the duplex+daemon and the remote host's RPC
tasks live on the test runtime while the server keeps its own thread, exactly the split
the harness already makes for its client connection.

**Still to do in the full split:** `lsp/manager.rs` (long-lived bidirectional
raw-pipe transport: needs the `write_stdin` + stdout-as-events shape, not
run-to-completion); ~~the **blocking spawn path `btv._system`**~~ ✅ DONE — Phase 3n
below (the *fourth* spawn site the original three-site list missed; it now routes to
the daemon over the `sys_run` wire and blocks on the round-trip via the blocking
bridge, because a `root_dir` shell-out like `cargo metadata` must run *where the
project files are*);
`HostWatch` (the daemon side of `FsEventStart`, today local-only via `notify` —
`serve_daemon` currently drops `LoopEvent::FsEvent` on the floor); the `bemtvi
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

Shipped (alongside 3c in `crates/bemtvi-server/src/daemon.rs`; the module now carries
*both* legs):
- **The wire** — one `bemtvi-rpc` **request**: `fs_read [path]` → `["file", bytes]`,
  `["new"]` (path doesn't exist → a new-file buffer), or a loud RPC **error** (a
  directory — remote explorer is a later slice — or a transport/permission failure;
  never a silent empty buffer). `bemtvi_rpc::request` routes the reply by msgid, so the
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

**Exit criteria — met.** `crates/bemtvi-server/tests/daemon_fs.rs`: an editor whose
`host_fs_async` is a `RemoteHostFs` talking to a `serve_fs_daemon` over an in-process
duplex opens a `/virtual/...` path — one the edit-host's *local* disk cannot read — and
its bytes (`fetched / over / the / wire`) appear in the first buffer, named for the
path; the content can only have crossed the wire (the same faithfulness argument 3a's
`host_fs.rs` makes for the sync seam). A second test proves a not-yet-existing path
opens as an empty **new-file** buffer (not an error) with its name bound for a later
`:w`. The `attach` handshake completes before the file loads — evidence the fetch did
not block startup. Regression-clean — full `bemtvi-server` suite (now 19 binaries),
`bemtvi`/`bemtvi-gui` (which pass `host_fs_async: None`, unchanged), fmt + clippy
`-D warnings` all green.

**Still to do on the fs leg (after 3e/3f/3g/3h):** `FileChangedShell` from a daemon
`watch` (a genuinely new wire leg — server-push, the `HostWatch` traffic class). The
async seam + replica pattern these slices established is what it extends. (The **save**
path landed in Phase 3e, `:edit` in Phase 3f, the **remote explorer** — `read_dir` over
the wire — in Phase 3g, and **`:tabnew` / LSP go-to** in Phase 3h, all below. `:read`/`:r`
is *not implemented* in bemtvi at all, so there is nothing to route over the wire — it
would be a new feature, not a wire slice.)

### Phase 3i — the watch leg, local behavior (`:checktime` / `'autoread'`) — ✅ DONE (2026-06-10)

The watch leg's foundation, done **local-first** exactly as the read leg was (sync-local
behavior in 3a → remote async in 3d): before the daemon can *push* "a remote file
changed under you," the editor has to know what to *do* when a file changes. That
behavior didn't exist — the only external-change detection was the `:w` clobber guard.

Shipped (all in `bemtvi-core`, with the `vim.o` mirror plumbing):
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
  (**W11** warning + no reload when off); a buffer changed on disk **and** in bemtvi is a
  **W12** conflict (never clobbered); a vanished file is **E211**.
- **`Editor::reload_buffer(id)`** — the in-place disk re-read `:checktime`'s autoread path
  uses (generalizing `load_into_current` to any buffer): replaces the rope, re-roots the
  undo tree at the reloaded state, **refreshes the disk snapshot** (so the next
  `:checktime` is quiet), and clamps the cursor (live for the current buffer, saved
  otherwise) into the new extent.

**Exit criteria — met.** `crates/bemtvi-server/tests/editing/core_editing.rs`: four
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
(`vim.uv.new_fs_event`, e.g. a statusline VCS segment watching `.git/HEAD`). That watcher is the **trigger**
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
(`bemtvi-server` 559 in `editing`, all binaries; `bemtvi`); fmt + clippy clean.

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
  `v:fcs_choice` reset (the new `LuaRuntime::fire_file_changed` / `fcs_choice`; `btv._fire`
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
- **The wire** — one `bemtvi-rpc` **request** added alongside 3c/3d in `daemon.rs`:
  `fs_write [path, bytes]` → `["ok", stat?]` (the post-write [`FileStat`] the edit-host
  stamps as its `disk` baseline — no remote stat round-trip) or a loud RPC **error** (a
  permission/transport failure; never a silent success). `serve_fs_daemon` does the
  atomic write through the *same* sync [`HostFs`] the local server uses, so a fake and
  the real disk behave identically.
- **The off-tick save seam in core** (`bemtvi-core`): an opt-in `host_save_offtick` flag
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
autocmds aren't emitted anywhere in bemtvi yet (the contract's snapshot-after-`BufWritePre`
point is moot until they exist); the observable saved-state is `modified` / `save_tick` /
the `written` echo, all ack-gated here.

**Exit criteria — met.** `crates/bemtvi-server/tests/daemon_save.rs`: an editor whose
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
local binaries (`bemtvi`, `bemtvi-gui`) leave off-tick mode off and write synchronously
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
- **Core** (`bemtvi-core`): the off-tick flag generalized `host_save_offtick` →
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
- **`:read`/`:r` is *not implemented* in bemtvi** (confirmed: no dispatch arm), so there
  was nothing to route — it would be a new feature, out of this slice.

**Exit criteria — met.** `crates/bemtvi-server/tests/daemon_edit.rs`: `:edit
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
fetches a *directory's entries* over the wire so bemtvi's in-window file explorer (vim's
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
  sort in one place. (`crates/bemtvi-server/src/daemon.rs`.)
- **Core, off-tick directory listing** (`bemtvi-core`): `Buffer::from_dir_entries(dir,
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
- **Rides for free:** the startup `bemtvi <remote-dir>` open (the deferred startup fetch
  hits the same `apply_open`) and `:split`/`:vsplit <remote-dir>` (delegate to `ex_edit`).
  **Still sync (documented):** `:tabnew {file}`; and remote directory **canonicalization**
  beyond what the daemon resolves on the open is not re-statted per navigation (the
  listing's trailing-slash and the daemon's canonical path carry it).

**Exit criteria — met.** `crates/bemtvi-server/tests/daemon_explorer.rs`: a server whose
`host_fs_async` is a `RemoteHostFs` over an in-process duplex, backed by a daemon-side
fake that models *directories* (a `read_dir` that succeeds only for registered dirs).
`bemtvi /virtual/proj` (startup) and `:edit /virtual/proj` both list the remote dir's
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
and worth recording: bemtvi had **four near-identical file-open paths** (`ex_edit`,
`ex_tabnew`, `jump_to`, `explorer_open_file`), each inlining its own
`find_buffer_by_path` → `Buffer::from_file` because the *load* step was never separated from
the *placement* policy (current window in place / a new tab / cursor jump / wipe the
listing). All the off-tick investment landed in `:edit`'s copy; the other three silently
stayed sync.

So this slice **extracts the shared kernel** rather than bolting a fourth and fifth copy of
the off-tick enqueue on:
- **`Editor::load_new_buffer(path) -> Option<BufferId>`** (`bemtvi-core`) — the load atom: off-tick
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

**Exit criteria — met.** `crates/bemtvi-server/tests/daemon_edit.rs` gains
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
`bemtvi-web-bridge` relay test times out identically on a clean tree, and the `bemtvi` e2e PTY
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
- **Core** (`bemtvi-core`): `enqueue_save_of(buffer, …)` (the buffer-targeted form of
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
`:w`). `BufWritePre`/`BufWritePost` still aren't emitted anywhere in bemtvi yet, so the
snapshot-after-`BufWritePre` point stays moot; the observable saved-state is `modified` /
the `written` echo, ack-gated per buffer.

**Exit criteria — met.** `crates/bemtvi-server/tests/daemon_save.rs` gains three tests over an
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

### Phase 3n — the blocking `vim.system` shell-out over the wire (`sys_run`, the blocking bridge) — ✅ DONE (2026-06-10), then **REMOVED (2026-06-17, commit `474813f`)**

> **This whole leg was later deleted** under "no blocking IO at all"
> ([[no-blocking-io-fs-async-only]]): the `BlockingSystem` trait, `btv._system`, the
> `Std`/`Wasm`/`RemoteBlockingSystem` impls, and the `sys_run` daemon wire are gone. It had
> zero production callers — `vim.system`/`btv.run`/`:make` ride the async
> `btv._system_async` path, and `vim.fn.system` as a sync function was never wired. The
> section below is kept for history; the blocking bridge it describes no longer exists.

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
- **The seam, in `bemtvi-lua`** (`system.rs`): a synchronous `trait BlockingSystem { fn
  run(&self, SystemSpec) -> SystemOutput }` with `SystemSpec` (argv / cwd / env) and
  `SystemOutput` (code / stdout / stderr / pid). `StdBlockingSystem` is today's
  `btv._system` spawn-and-wait logic **factored verbatim** behind the seam — the
  editor-side default (no daemon) *and* the daemon-side backend in the real `bemtvi
  --daemon`, where "local" *is* where the project files live. `btv._system` now builds a
  `SystemSpec` and runs it through `Shared::blocking_system` (an `Option<Rc<dyn
  BlockingSystem>>`, `None` = the `StdBlockingSystem` default — a bare/local session is
  byte-for-byte unchanged); `LuaRuntime::set_blocking_system` injects the daemon bridge.
- **The wire** (`daemon.rs`, alongside the fs/process legs): one `bemtvi-rpc` **request**
  `sys_run [argv, cwd?, env]` → `[code, stdout, stderr, pid?]` (stdout/stderr as binary so
  non-UTF-8 output survives), or a loud RPC error. Request/response, like the fs read — no
  `id`/demux.
- **`RemoteBlockingSystem` (edit-host side, a `BlockingSystem`)** — `connect(reader,
  writer)` spawns a **dedicated link thread** that owns its *own* current-thread runtime
  and the RPC link; `run` hands the spec to that thread over a plain `std::sync::mpsc`
  channel and **parks the calling (Lua) thread** on a `std` reply channel. Parking with a
  `std` recv — not a tokio primitive — is deliberate: `btv._system` runs *inside* the
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

**Exit criteria — met.** `crates/bemtvi-server/tests/daemon_system.rs`: an editor whose
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
binaries (`bemtvi`, `bemtvi-gui`) leave `blocking_system: None` and spawn locally, unchanged.

**Still to do on the process side of the full split:** ~~`lsp/manager.rs` (the long-lived
bidirectional raw-pipe transport)~~ ✅ DONE — Phase 3o below. The `bemtvi --daemon` binary and
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

The seam is **in `bemtvi-lsp`** (where the spawn lived), not the server, because the
[`LspManager`] is what spawns servers. Shipped:
- **`trait LspTransport`** (`crates/bemtvi-lsp/src/transport.rs`): `spawn(spec, root) ->
  io::Result<LspChannel>`, where an [`LspChannel`] hands back the server's `stdout`/`stdin`
  (boxed `AsyncRead`/`AsyncWrite`), its `stderr`, and an [`LspProcess`] (`start_kill` + `wait
  -> (code, signal)`). The manager drives its `async-lsp` `run_buffered` loop over whichever
  streams it gets, knowing nothing of where the server runs. **`LocalLspTransport`** is the
  default — today's `tokio::process` spawn lifted verbatim behind the seam (the inline
  `Command`/pipe-take/stderr-drain `run_server_once` did is now its `spawn`). `LspManager::new`
  uses it; `with_transport` injects another. **Zero behavior change** on the local path.
- **The wire** (`crates/bemtvi-server/src/daemon.rs`, a fifth leg): six notifications correlated
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

**Exit criteria — met.** `crates/bemtvi/tests/lsp/daemon.rs` drives the **real** `bemtvi
--__lsp-mock` server through a `RemoteLspTransport` ↔ `serve_lsp_daemon` over an in-process
duplex (the ssh-stdio stand-in): a scripted `publishDiagnostics` renders in the editor —
proving the `didOpen` crossed as `lsp_stdin` to the child *and* its reply crossed back as
`lsp_stdout` (faithful, not a stub — the diagnostic is state only a real round-trip produces);
`gd` lands the cursor on the mock's scripted definition, proving the request/reply path; and a
mock that exits after `initialize` makes the tunneled child die, `lsp_exited` round-trips, the
breaker respawns, and the editor stays fully responsive throughout. Regression-clean — the full
114-test local LSP suite passes unchanged (the `LocalLspTransport` lift didn't regress it), full
`cargo test --workspace` green, fmt + clippy `-D warnings` clean; the local binaries (`bemtvi`,
`bemtvi-gui`) leave `lsp_transport: None` and spawn servers locally, unchanged. (`clipboard.rs`
stays local-by-topology, struck under 3c.)

### Phase 3p — the Lua-visible filesystem seam (`LuaFs`, the project-facing fs surface) — ✅ DONE (2026-06-11)

The cross-cutting semantic the *Lua-visible filesystem semantics* bullet below named "the
hardest one": plugins read the *project* through `vim.uv.fs_*` and a handful of `vim.fn` fs
builtins, which bound **directly** to `std::fs` (~22 sites in `bemtvi-lua/uvfs.rs`, plus
`install.rs`/`host.rs`). In a daemon session that silently hits the *local* machine — the
wrong filesystem — so file-picker previewers, LSP `root_dir` detection, and VCS-status providers would
see the wrong tree. This slice routes that surface through a synchronous **`LuaFs` seam**
(the fs analogue of Phase 3n's `BlockingSystem`), with a daemon **blocking bridge** so a
plugin reads the *remote* project. The **split-brain routing rule was decided up front, not
plugin-by-plugin** (the bullet's demand).

**The rule (now in `architecture.md` and the `luafs.rs` header):** vim-level *project-facing*
fs APIs route through `LuaFs`; raw Lua `io.*`/`os.*`, `require`/`package.path`,
`nvim_get_runtime_file` (runtimepath = local plugins), `btv._read_file` (sources an
`lsp/<name>.lua` *config*), `vim.fn.mkdir` (overwhelmingly a `stdpath`-rooted local data/state
dir), and `stdpath` all stay **local** — plugins and their caches live on the local machine
by design (the divergence from VS Code's remote-extension-host topology).

Shipped:
- **The seam** (`bemtvi-lua/src/luafs.rs`): a synchronous object-safe `trait LuaFs` covering the
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
- **The wire** (`bemtvi-server/daemon.rs`, alongside the sys leg): one `luafs` request carrying
  `["op", args…]` → `["ok", payload] | ["err", msg]`, with `RemoteLuaFs` (the edit-host side, a
  `LuaFs`) the blocking bridge — a dedicated link thread owns the wire + its own runtime, each
  call parks the Lua thread on the reply (`std` channel, so the park can't starve the reader) —
  and `serve_luafs_daemon` running each op through the daemon's real `StdLuaFs` on `spawn_blocking`
  (it owns the fd table the tokens index).

**Scoped out (next slices, not silent gaps):** the short-TTL stat/exists cache the bullet pairs
with the routing (deferred — correctness first); the `bemtvi --daemon` binary + WebTransport/QUIC
listener transport that ties every leg together (ssh dropped — Open Decision #2); and the
*paths-are-remote-paths* concern (`getcwd` stays the local cwd —
the path-space split is its own bullet).

**Exit criteria — met.** `crates/bemtvi-server/tests/daemon_luafs.rs`: an editor whose `lua_fs` is a
`RemoteLuaFs` talking to a `serve_luafs_daemon` over an in-process duplex, backed by a virtual
in-memory fs serving `/virtual/...` content that exists on no real disk. `vim.uv.fs_stat` returns
the daemon's size + sentinel mtime (a local stat would be nil); `fs_open`+`fs_read`+`fs_close`
round-trips the remote fd token; `fs_scandir` enumerates the daemon dir; `vim.fn.readblob`/
`filereadable`/`executable`/`exepath` resolve against the daemon (the tool is not on the local
PATH); `fs_mkdir` mutates the daemon store, observable on a follow-up stat; distinct paths echo
distinct sizes (reacts to input). Two controls: dropping the injection flips every `/virtual/...`
probe to a local miss, and a local-`StdLuaFs` test round-trips write/read/stat/mkdir/scandir against
a real temp dir (the refactor is behavior-preserving). Regression-clean — full `cargo test
--workspace` green, fmt +
clippy `-D warnings` clean; local binaries leave `lua_fs: None` and hit the disk directly, unchanged.

### Phase 3q — the `bemtvi --daemon` binary + the six-leg multiplexer (one stream) — ✅ DONE — both multiplexers *and* the QUIC listener shipped (2026-06-11)

**Status (2026-06-11): the daemon half, the edit-host multiplexer (`connect_daemon`),
*and* the WebTransport/QUIC listener transport are all shipped — the native full split
runs end-to-end over real QUIC.** (ssh was the originally-planned native transport;
**dropped 2026-06-11** in favor of the non-ssh QUIC listener — see Open Decision #2.) The
listener slice is recorded as **Phase 3r** below. The scoping question at the foot of this section was
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
- **`crates/bemtvi/tests/daemon_stdio.rs`** drives the **real** `bemtvi --daemon` binary,
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
- **`crates/bemtvi/tests/daemon_stdio.rs`** gained `edit_host_drives_a_real_daemon_over_one_stream`:
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
- **`bemtvi --connect-daemon [file]`** (`main.rs`) — the manual driver / local edit-host role,
  so the split is runnable for real, not just in tests. It is the default editor+TUI role plus
  the daemon seams: it spawns the daemon child (this binary's `--daemon` by default, or whatever
  `BEMTVI_DAEMON_CMD` names — e.g. `ssh host bemtvi --daemon` — run through `sh -c`), wraps the
  child's stdio in `connect_daemon`, and injects the five seams into `ServerInit`; config /
  runtimepath / clipboard stay **local** (the thesis), only I/O crosses the wire. The daemon's
  stderr is redirected to `$TMPDIR/bemtvi-daemon.log` so it can't corrupt the TUI. **Verified by
  hand over a PTY:** the startup buffer renders `[No Name]` then fills from the daemon (off-tick
  `fs_read`) with its name bound, and an in-editor edit + `:w` lands the bytes on disk through the
  daemon's off-tick `fs_write`. (This subsumes the listener slice's "drives a local edit-host
  against a daemon" check for the *stdio* transport; the QUIC transport is what remains.)

The **listener slice landed as Phase 3r below** (2026-06-11): the non-ssh
WebTransport/QUIC listener (`wtransport` on `quinn`), the launch-minted bearer token, the
self-signed cert pinned TOFU, the `--daemon --listen` role and the `bemtvi://…`
`--connect-daemon` target. `connect_daemon`/`run_daemon_io` were ready for it exactly as
predicted — they take any `AsyncRead`/`AsyncWrite`, so the listener just feeds them a QUIC
bidi stream's halves and the stdio proof carried over verbatim (zero changes to the legs).

The slice that **ties every leg together**. Phases 3c–3p each built a wire leg and
proved it over its *own* `tokio::io::duplex`; this one stands up the actual
`bemtvi --daemon` process and carries **all six legs over one ordered stdio stream** —
the transport `ssh host bemtvi --daemon` execs. It is the daemon counterpart to
`--server` (`crates/bemtvi/src/main.rs`), but inverted: `--server` runs the *whole
editor* remotely (one round-trip per keystroke — the lag this plan exists to kill);
`--daemon` runs *only* fs + process + watch + `sys_run` + LSP + `luafs` remotely while
the editor stays local. No `Editor`, no `LuaRuntime`, no UI, and — unlike `--server` —
**no config sourcing** (`default_runtime` / `init.lua` / runtimepath all stay on the
local edit-host; the daemon is pure I/O).

**The one genuinely new mechanism: a multiplexer, needed symmetrically on both ends.**
Every `serve_*` (daemon side) *and* every `Remote*::connect` (edit-host side) currently
calls `bemtvi_rpc::connect(reader, writer)` itself and **assumes it owns the whole
transport** — which is why the per-leg tests each hand it a private duplex. A real
daemon has *one* ssh stdio stream for all six classes, so the legs must share a single
connection. Two properties (verified in the code) make that a clean *router*, not a
rework:

- **The six method namespaces are disjoint** — `fs_*`, `proc_*`, `sys_run`, `lsp_*`,
  `luafs` (and the `proc_`/`lsp_` daemon→edit-host pushes) — so an inbound stream
  demuxes unambiguously on the method string.
- **Request replies are routed by msgid *inside* `Rpc`, not by an embedded responder.**
  `Incoming::Request` carries only `{id, method, params}` (`bemtvi-rpc/src/lib.rs`); a
  handler replies via `rpc.respond(id, …)` on any clone of the shared `Rpc`, and
  request *responses* (`fs_read`/`fs_write`/`sys_run`/`luafs` results) are matched by the
  `pending` map and never surface as `Incoming` at all. **So forwarding an `Incoming`
  over an mpsc channel loses nothing**, and concurrent writes from all legs serialize
  safely through `Rpc`'s single `out_tx`. This *is* the "msgpack-RPC already frames
  concurrent requests over one ordered stream" the *Transport & stream multiplexing*
  section counts on for the native (ssh-stdio) path.

**Plan.**

1. **Split each `serve_*` into `connect()` + a connection-agnostic core**
   (`crates/bemtvi-server/src/daemon.rs`). Each grows a `serve_*_on(rpc: Rpc, incoming:
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
   `bemtvi_server::run_daemon_io(stdin, stdout)`: `connect` once, mint a per-leg
   `unbounded_channel`, `tokio::spawn` each `serve_*_on(rpc.clone(), leg_rx, deps)` —
   `StdHostFs` for `fs_*`, `StdHostProc` (internal to the proc leg), `StdBlockingSystem`
   for `sys_run`, `LocalLspTransport` (internal to the lsp leg), `StdLuaFs` for `luafs`
   — then a demux loop reading `incoming` and routing each message by method prefix
   (`fs_` / `proc_` / `sys_run` / `lsp_` / `luafs`) to the matching `leg_tx`; unknown
   methods drop (the peer is the same build). Then in `crates/bemtvi/src/main.rs`, a
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

**Testing (mirror `crates/bemtvi/tests/stdio_server.rs`).** A new
`crates/bemtvi/tests/daemon_stdio.rs` spawns the **real** `CARGO_BIN_EXE_bemtvi --daemon`
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
(`bemtvi`/`bemtvi-gui`, no `--daemon`) byte-for-byte unchanged (all `Remote*`/`serve_*`
wrappers retained). This is the concrete slice that fulfils *The full split*'s first two
exit-criteria sentences below.

**Deferred (explicitly, not stubbed):**
- ~~**The real listener hop / CLI / `:connect`**~~ ✅ DONE — **Phase 3r above** wired
  `connect_quic`/`serve_quic` onto a **WebTransport/QUIC** connection (`wtransport` on
  `quinn`, launch-minted bearer token, TOFU cert pin — Open Decision #2), the
  `--daemon --listen` role, and the `bemtvi://…` `--connect-daemon` target. (An in-editor
  `:connect` ex-command — live seam re-pointing — is the one piece deferred from that
  slice; the launch-time target shipped.) **(ssh is dropped — the earlier
  `ssh … bemtvi --daemon` + askpass + `BEMTVI_REMOTE_CMD` plan no longer applies.)**
- **Path-space** (`getcwd` / buffer names / statusline in the remote's path-space) and
  the **short-TTL stat/exists cache** for `luafs` — both already deferred by Phase 3p.
- **Transport HOL mitigation beyond app-level framing** — Phase 3r ships the QUIC listener
  but still multiplexes all six legs over **one** ordered bidi stream (the stdio-equivalent,
  so the `daemon_stdio.rs` proof carried over). The real escape from HOL blocking is the
  per-`HostServices`-class **QUIC stream split** (one stream per class + one per live
  `HostProc`); that is the remaining follow-up on this transport, built once for native +
  browser. (ssh was never going to escape HOL anyway — QUIC can't run under its single TCP
  stream — which is why it was dropped.)

**Scoping question — RESOLVED (2026-06-11): daemon side first, then the edit-host
multiplexer as its own stdio slice.** The daemon demux was tested faithfully without the
edit-host multiplexer — a raw `bemtvi_rpc` client over one stream drives three namespaces
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

### Phase 3r — the WebTransport/QUIC listener transport (the native daemon hop) — ✅ DONE (2026-06-11)

The last leg of the native full split: the real transport the Phase 3q stdio stand-in was
standing in for. As predicted, this slice touched **none** of the six wire legs — they
already take any `AsyncRead`/`AsyncWrite`, and a QUIC bidi stream's halves are exactly
that, so the listener is pure transport wiring around the unchanged `run_daemon_io` /
`connect_daemon` multiplexers. **ssh is dropped** (Open Decision #2), so the native and
browser daemon transports unify on one stack — `wtransport` on `quinn`, default features
(`ring` crypto, no aws-lc-rs/cmake; `self-signed`/`rcgen` for the dev cert).

Shipped (`crates/bemtvi-server/src/quic.rs`, re-exported from the crate root):
- **The seam reuse** — `connect_daemon`'s link-thread body was extracted into a
  `pub(crate) serve_daemon_link(rpc, incoming, client_tx)` (pure inversion; `connect_daemon`
  now calls `connect(reader, writer)` then it). `connect_quic` reuses that body verbatim,
  the QUIC `Endpoint` + `Connection` kept alive on the same dedicated link thread (the
  blocking-bridge deadlock-avoidance property from Open Decision #5 is unchanged — the wire
  is still driven off the parked editor thread).
- **`serve_quic` / `bind_quic_listener`** (daemon side, `--daemon --listen`) — mint a
  self-signed [`Identity`], bind a QUIC endpoint (`bind_quic_listener` splits bind from
  serve so a `:0` port resolves before anyone connects), then accept connections forever;
  each runs the full six-leg `run_daemon_io` multiplexer over its one client-opened bidi
  stream. One ordered QUIC stream *this* slice — the per-`HostServices`-class stream split
  (the real HOL escape) is the *Transport & stream multiplexing* follow-up; it builds on
  this, it doesn't block it.
- **Auth, the two mechanisms ssh gave for free** (Open Decision #2): a **launch-minted
  bearer token** (`mint_token`, 32 CSPRNG bytes via `getrandom`) carried on the
  WebTransport CONNECT *path* and compared constant-time — a mismatch replies **403**
  (`request.forbidden()`) so the edit-host's `connect` fails *promptly and loudly*, never a
  half-open session; and **TOFU server identity** — the daemon prints its self-signed
  cert's SHA-256 hash, the edit-host pins it (`with_server_certificate_hashes`), the
  known-hosts model, no CA. (The hash, not mTLS, because the browser passes the same hash
  to its `WebTransport` constructor — native/browser auth stays unified.)
- **The CLI** (`crates/bemtvi/src/main.rs`) — `--daemon --listen [addr]` binds the listener
  (default `127.0.0.1:8765`, loopback-only as defense-in-depth; pass `0.0.0.0:PORT` to
  accept off-host) and prints the exact `bemtvi --connect-daemon 'bemtvi://HOST:PORT/TOKEN?cert=HASH'`
  command. A `bemtvi://…` argument selects the QUIC connect path (`connect_quic` → the same
  five seams `connect_daemon` returns over stdio) over the default stdio-child split; the
  stdio and QUIC edit-host roles now share one `run_edit_host_session` helper, so they can't
  drift. Config / runtimepath / clipboard stay **local** (the thesis); only I/O crosses.

**Exit criteria — met.** `crates/bemtvi/tests/daemon_quic.rs`: a real in-process edit-host
`Server` drives a QUIC daemon (an in-process listener on its own thread+runtime — a
faithful stand-in for the separate daemon *process*, reached only over a loopback QUIC
socket) over **one real QUIC connection**, exercising four wire classes through the running
editor — the off-tick **fs read** (startup open) and **write** (`:w`, `modified` clearing
only on the daemon's ack), the blocking **`sys_run`** bridge (`vim.system():wait()` — the
deadlock case), the **watch** push (external-change autoreload, a lever only the daemon's
poller can pull), and **`luafs`** (`vim.uv.fs_stat`/`filereadable`) — each carrying bytes a
stub couldn't invent. A second test proves the **bearer token gates the connection**: a
wrong token is rejected (403, under a timeout so a regression that *hangs* fails loud) and
the right one connects. The six per-leg suites + `daemon_stdio.rs` stay green (the
`serve_daemon_link` extraction was pure inversion); full `cargo test --workspace` green
(one unrelated, load-only `mouse` redraw-race flake that passes standalone — the documented
`drain_to_latest_redraw` timing issue, not this slice); fmt + clippy `-D warnings` clean.
**Verified end-to-end as two real processes**: `bemtvi --daemon --listen 127.0.0.1:0` +
`bemtvi --connect-daemon 'bemtvi://…'` driven through a PTY — the file opens off-tick over the
wire, an edit + `:wq` lands the edited bytes on the daemon's disk over real QUIC.

**Deferred (explicitly, not stubbed):** the per-`HostServices`-class QUIC stream split (the
actual HOL escape; one ordered stream here is the stdio-equivalent); an in-editor
`:connect` ex-command (live seam re-pointing) vs. the launch-time `--connect-daemon`
target shipped here; graceful client-initiated connection close on quit (today the process
exit tears the QUIC link down, and the 30 s idle timeout + keep-alive bound any lingering
daemon-side connection); path-space (`getcwd`/buffer names in the remote's path-space) and
the short-TTL `luafs` stat/exists cache (both already deferred by Phase 3p).

### The full split

The native latency payoff. Carve today's `bemtvi-server` into two roles connected
by `HostServices` (Phase 1) over RPC:

- **Edit host** (runs *locally* in the remote case): `bemtvi-core` + `bemtvi-lua` +
  `bemtvi-ts` + redraw projection + the input/keymap/excmd/evloop machinery.
  Everything in `dispatch.rs` / `redraw.rs` / `input.rs` / `keymap.rs` /
  `excmd.rs` / `evloop.rs` / `lsp/` stays here.
- **Daemon** (runs *remotely*): fs + process + watch only — the `HostFs`/`HostProc`
  server half. Tiny.

The network boundary moves from *above* the editor (the former `bemtvi --server` over
ssh stdio, since removed) to *below* it: the GUI/TUI
client and edit-host are co-located and local; `ssh … bemtvi --daemon` runs just
the fs/process daemon on the remote, and the local edit-host is a `HostServices`
client over the ssh stdio transport (reusing the `bemtvi-rpc` plumbing and the
hardened ssh connector from `crates/bemtvi-gui/src/remote.rs`).

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
LSP **landed in Phase 3o** with its own `LspTransport` seam (in `bemtvi-lsp`, where the
spawn lives) + the `lsp_*` wire that streams the raw bidirectional pipe — *not*
`HostProc`. The blocking `btv._system` **landed in Phase 3n** — its own
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
  blocking `btv._system` (Phase 3n), the LSP leg (Phase 3o), and the Lua-visible fs surface
  (Phase 3p) are all complete**; what remains for the full split is the daemon binary / ssh
  transport and the path-space + cache follow-ups noted below.)
- **Lua-visible filesystem semantics — the hardest one. ✅ DONE — Phase 3p above** (the
  `LuaFs` seam + `luafs` wire; the short-TTL stat/exists cache and the `getcwd`/path-space
  half are deferred follow-ups). The Lua VM is local
  (the thesis), but plugins read the *project* through it, and today the bridge
  reaches the disk directly: `vim.uv.fs_*` (`uvfs.rs`, ~22 raw `std::fs` call
  sites), `vim.fn.readfile` / `readdir` / `glob` / `filereadable` /
  `executable` (`install.rs` / `host.rs` in `bemtvi-lua`), and the blocking
  `vim.fn.system`. The proposed split-brain rule: **vim-level fs/process APIs
  route through the host seams** (`HostFsAsync` / the blocking bridge /
  `HostProc` — Open Decision #5's residual note) — so file-picker previewers, root
  detection, and VCS-status providers see the *remote* project — while **raw Lua `io.*` /
  `os.*` and `require`/`package.path` stay local**: plugins and config live on
  the local machine (a feature — no remote plugin install needed), and their
  caches/state files are local. This is exactly the consequence of diverging
  from VS Code's remote-extension-host topology; it must be decided and
  documented up front, not discovered plugin-by-plugin. Two corollaries: (1)
  it's an implementation lift — `bemtvi-lua` has no `HostFs` handle today and
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

**Exit criteria.** `bemtvi --daemon` over stdio passes a black-box suite mirroring
`crates/bemtvi/tests/stdio_server.rs` but for the `HostServices` protocol; an
end-to-end test drives a local edit-host against a daemon over an in-process
duplex and asserts edit/save/reload round-trips. Manually: typing over a real ssh
hop has **no per-keystroke latency** (the whole point — verify, don't assume).

---

## Phase 4 — wasm edit-host: compile

Bring the Lua-bearing edit-host to `wasm32-unknown-emscripten` (Phase 0 proved the
VM compiles; this compiles the *real* stack).

**Scope (per Open Decision #3, resolved): one web build.** This emscripten edit-host
*replaces* the `wasm32-unknown-unknown` `bemtvi-web` — the serverless `WebEditor` and
the `RemoteClient`/Socket.IO bridge both retire into it (see Open Decision #3). The
gating below is the first slice: make `bemtvi-lua` (the C-heavy VM, the real risk)
compile under emscripten with `bemtvi-ts`/`libloading` and the process/fs hatches gated
on `target_arch = "wasm32"`. **Prerequisite:** the emsdk toolchain (`emcc`) must be
installed and sourced — the Rust `wasm32-unknown-emscripten` *target* alone can't build
the vendored Lua/regex C.

- **Gate out `bemtvi-ts` + `libloading`.** Dynamic library loading doesn't exist in
  wasm. `bemtvi-lua` pulls `bemtvi-ts` (tree-sitter + `libloading`) for the
  `vim.treesitter` binding; feature-gate that binding **out** of the wasm build.
  The browser already does highlighting in JS via web-tree-sitter (project memory
  `web-treesitter-highlighting`, `docs/architecture.md` → *The web build*), so no
  capability is lost — the redraw just omits server-side highlight spans and the
  JS layer paints them, as `bemtvi-web` does today.
- **Gate the process/fs escape hatches.** `bemtvi-lua` reaches `std::process`
  directly (the blocking `btv._system`) and `std::fs` directly (`uvfs.rs`,
  `vim.fn.readfile`/`readdir`/`glob`): there are no subprocesses in a browser,
  and the Worker's "local fs" is meaningless. Per *No silent stubs or skips*
  these must **fail loud** on wasm (until Phase 6 routes them to the daemon /
  OPFS) — not link against emscripten's stubs and quietly return junk. The
  clipboard likewise: `navigator.clipboard` via JS interop, not a shell-out.
- **Emscripten toolchain in the build.** The web build moves from
  `wasm32-unknown-unknown` (`crates/bemtvi-web`, wasm-bindgen, `build.sh`) to
  `wasm32-unknown-emscripten`. Wire `EMCC_CFLAGS=-fwasm-exceptions` and the
  emsdk-sourced `emcc` into the build script. `bemtvi-core` is pure Rust and
  compiles to the new target unchanged.
- **Backend = `lua51`** (LuaJIT excluded from wasm). The browser inherits the
  PUC 5.1 dialect ceiling — a config relying on LuaJIT-only `ffi`/`bit` won't run
  there. Known limitation, by design.

**Exit criteria.** A headless node harness loads the compiled edit-host module,
feeds a vim key sequence, and reads back buffer lines / a redraw — i.e. the *real*
editor runs in wasm, proven by behavior, not just a clean link (cf. project memory
`dont-conflate-loads-with-works`).

**Progress (2026-06-10) — concept VALIDATED via a throwaway demo.** The
risky-unknown half of Phase 4 is green, proven by behavior in a real wasm engine:

1. **`bemtvi-lua` compiles to `wasm32-unknown-emscripten` (`lua51`).** Gated
   `bemtvi-ts`/`libloading` off wasm (a `cfg(not(wasm32))` dependency + three gated
   call sites in `runtime.rs`; the browser highlights in JS). Hit and fixed a
   portability bug the plan hadn't called out: **`mlua::Integer` is `i32` on wasm32**
   (`lua_Integer` = `ptrdiff_t`), not `i64` — 11 type errors fixed with the
   `lua_int`/`lua_i64` helpers in `convert.rs` (identity on native, so host
   `clippy -D warnings` + the full `bemtvi-server` suite stay green). Project memory:
   `wasm32-mlua-integer-is-i32`.
2. **core + Lua run *together* in one wasm module.** A throwaway demo crate —
   `crates/bemtvi-edithost-demo/` (workspace-excluded, **marked TEMPORARY/DELETE-ME**
   in every file) — wires `bemtvi_core::Editor` + `bemtvi_lua::LuaRuntime` with the
   crudest sync tick (`editor.input` + `lua.eval` + drain `take_commands` →
   `editor.command`, mirroring `effects.rs`), links via `emcc` (staticlib + the
   `mlua-sys`/`bemtvi-regex` C archives) into an ES module, and a node harness asserts:
   vim-key insert → buffer; `return 1+41` → `42`; `#vim.split("a,b,c",",")` → `3`
   (the `vim.*` prelude runs in wasm); `vim.cmd("%s/hello/LUA/")` mutates the buffer
   (Lua → editor). All pass. (It also confirmed the **fail-loud** convention survives
   wasm — an unimplemented `vim.fn.abs` raised loudly rather than returning junk.)

**Still to do for Phase 4 proper** (the demo deliberately skips these — it is *not*
the edit-host): the real edit-host reuses `bemtvi-server`'s synchronous tick
(`apply_lua_effects` + the buffer/option/register **mirrors** that let Lua *read*
editor state, autocmds, redraw projection) behind an async-effect seam — which is the
larger "extract the sync edit-host" refactor (Open Decision #6 below). The throwaway
demo gets **deleted** when that lands. The fail-loud process/fs/clipboard hatches
(this phase's third bullet) are also still to wire — deferred until the wasm runtime
exists to exercise them (Phase 5).

### Phase 4-proper — extract the reusable sync `EditHost` (Open Decision #6 (a))

The "extract the sync edit-host" refactor, resolved to **option (a)** (2026-06-11):
pull the synchronous tick out of `impl Server` into a reusable `EditHost` whose
**only** reach into async/external machinery is through a `HostEffects` trait. Native
`Server` becomes the trait's implementor (today's tokio/RPC/LSP machinery, verbatim);
the wasm Worker (Phase 5) supplies a JS-interop + daemon-link implementor. The
empirical anchor (Open Decision #6): the wasm blocker is the **dependency tree**, not
`bemtvi-server`'s own source — so the work is *moving the I/O behind a seam*, not
rewriting the editor.

**The seam, sliced from the small side.** `self.editor` / `self.lua` are touched at
**~530 sites**; relocating them wholesale would be enormous churn with zero
architectural payoff. So the extraction grows from the *small* side — the **bounded set
of outbound async effects** the tick fires — not by moving the editor out of `Server`.
The full effect surface, mapped from the code, is just five classes: the **client wire**
(`rpc.notify`/`respond` — redraw + notifications + responses, ~5 sites), the
**event-loop commands** (`evloop.send` — timers/proc/watch, ~8 sites, all in
`apply_loop_op` + `sync_buffer_watches`), **off-tick fs** (`host_fs_async` / `open_tx` /
`save_done_tx`), **LSP** (the `lsp/` submodules), and **TSInstall** (1 site). Each
becomes a `HostEffects` method group in its own slice; the inbound events they generate
(a child exited, a file fetched, an LSP reply) stay owned by the run loop's `select!`
and feed editor-tick methods — that *inbound* seam is its own later slice.

#### Phase 4a — the `HostEffects` seam: wire + event-loop commands — ✅ DONE (2026-06-11)

The first brick: define `trait HostEffects` (`crates/bemtvi-server/src/edithost.rs`) and
route the two cleanest, fully self-contained **outbound** effects through it — the
client wire (`notify` / `respond`) and the event-loop command sink (`loop_command`).
`Server` no longer holds `rpc: Rpc` or `evloop: EventLoop`; it holds
`fx: Box<dyn HostEffects>`, and every tick emit site goes through `self.fx`.
`NativeEffects` (the sole implementor) owns the `Rpc` and the `EventLoop` and is today's
behavior **verbatim** — routing `loop_command` through `EventLoop::send` (not a bare
cloned sender) preserves the "no actor task until first command" laziness. The daemon
link's own `Rpc` uses (the `Remote*` structs in `daemon.rs`) are a *different* transport
and untouched.

**Exit criteria — met.** Pure indirection, zero behavior change, guarded by the existing
suite: `cargo build` + `fmt --check` + `clippy -D warnings` clean; the full
`bemtvi-server` + `bemtvi` suites (41 binaries, **1179 tests**) green, including `editing`
(570 — the redraw/respond wire path), `uv_process` (the evloop timer/process command
path), and `daemon_proc` / `daemon_watch` (the proc/watch command path) — exactly the
seam this slice routes; `bemtvi-gui` (the other `run_io` consumer) builds. The
`HostEffects` surface grows in the next slices (off-tick fs, LSP, TSInstall) before the
sync tick is hoisted off `Server` onto `EditHost` proper.

**Deferred to the next 4-proper slices (deliberately, not stubbed — only what's wired
lives on the trait):**
- **4b — off-tick fs effects** ✅ DONE — Phase 4b below.
- **4c — LSP effects** ✅ DONE — Phase 4c below.
- **4d — the inbound seam** ✅ DONE — Phase 4d below.
- **4e — hoist the tick onto `EditHost`** ✅ DONE — Phase 4e below.

#### Phase 4b — the `HostEffects` seam: off-tick fs effects — ✅ DONE (2026-06-11)

The second brick: route the editor tick's **off-tick filesystem effects** — the
request/response read leg, the write leg, and the `HostWatch` arm/disarm leg Phase 3
built — through `HostEffects`, so the only thing reaching the daemon fs is the trait.
Same "grow from the small side" discipline as 4a: the *outbound* side joins the seam;
the *inbound* deliveries (the `open_rx` / `save_done_rx` / `watch_rx` arms that fill the
buffer, finalize the save, and reconcile a remote change) stay owned by the run loop's
`select!` for the 4d inbound slice.

Shipped (`crates/bemtvi-server/src/edithost.rs`):
- **Five new `HostEffects` methods.** `fs_fetch(buffer, path)` (spawn an `fs_read`,
  deliver `(buffer, path, result)` to the open arm), `fs_save(PendingSave)` (take the
  command-time snapshot's bytes, spawn an `fs_write`, deliver the ack-gated [`SaveDone`]
  to the save arm), `fs_watch(path)` / `fs_unwatch(path)` (arm/disarm the daemon watch),
  and `has_remote_fs()` (the off-tick-mode predicate that gates the editor tick's remote
  vs. local branches). The two channel-bearing types stay *inside* the trait impl — the
  trait surface names only `BufferId` / `PendingSave` / `String`, never the senders.
- **`NativeEffects` now owns the off-tick fs.** It holds the `Option<Arc<dyn HostFsAsync>>`
  plus the `open_tx` / `save_done_tx` senders and does the `tokio::spawn` + the
  `HostFsAsync::{read,write,watch,unwatch}` calls — today's behavior **verbatim**, just
  relocated behind the trait. `Server` no longer holds `host_fs_async` / `open_tx` /
  `save_done_tx`; its `drain_pending_opens`, `dispatch_save`, and `sync_buffer_watches`
  call through `self.fx`, gating on `self.fx.has_remote_fs()` instead of an inline
  `host_fs_async.is_some()`. The run loop's inbound arms and the startup-fetch bootstrap
  (a native-only `run_io` one-shot) are untouched.

**Exit criteria — met.** Pure indirection, zero behavior change, guarded by the existing
suite: `cargo build` + `fmt --check` + `clippy -D warnings` clean; the **full workspace**
(`cargo test --workspace`, 1341 tests across 78 binaries) green — including exactly the
seam this slice routes: `daemon_fs` (the read leg), `daemon_save` (the write leg, incl.
the ack-gated `:wq` and the failing-write-cancels-the-quit cases), `daemon_watch` (the
`HostWatch` arm + push reconcile), and the local-session regressions (`editing`,
`host_fs`, `host_proc`). The `HostEffects` surface grows once more (LSP, TSInstall)
before the sync tick is hoisted off `Server` onto `EditHost` proper.

#### Phase 4c — the `HostEffects` seam: LSP effects — ✅ DONE (2026-06-11)

The third brick: route the editor tick's **LSP command surface** through `HostEffects`.
The `lsp/` submodules touch the [`LspManager`] at exactly **17 sites**, and it is purely
an *outbound command sink* — `ensure_server` / `notify` / `request`, no reads — so the
whole field moves behind the trait with no read-path entanglement. Same discipline as
4a/4b: only the outbound side joins the seam; the inbound `LspEvent` stream
(`lsp_events`, the diagnostics/reply arm) stays owned by the run loop's `select!` for the
4d inbound slice.

Shipped (`crates/bemtvi-server/src/edithost.rs`):
- **Three new `HostEffects` methods** — `lsp_ensure(key, spawn)`, `lsp_notify(key, note)`,
  `lsp_request(key, token, req)` — one per `LspManager` method the tick actually fires
  (the fourth, `shutdown`, isn't called from the tick, so it stays off the trait per
  "only what's wired lives on the seam"). `NativeEffects` now **owns the `LspManager`**
  and delegates verbatim; `Server` no longer holds `lsp`. The 17 call sites across
  `request.rs` / `sync.rs` / `inlay.rs` / `semantic.rs` / `edit.rs` / `completion.rs`
  call `self.fx.lsp_*` instead of `self.lsp.*`. The manager's inbound `lsp_events`
  receiver (created with it at startup) is untouched — it still feeds the run loop.

**Exit criteria — met.** Pure delegation, zero behavior change: `cargo build` +
`fmt --check` + `clippy -D warnings` clean; the **full workspace** (`cargo test
--workspace`, 1341 tests / 78 binaries) green, and — the faithful proof this reroutes a
*live* path, not just compiles — the **114-test `bemtvi` LSP suite** (`crates/bemtvi/tests/
lsp/`) passes unchanged: it drives a real `--__lsp-mock` server through the whole
`ensure → didOpen → request → reply` exchange (and the daemon `RemoteLspTransport` leg),
exactly the `lsp_ensure` / `lsp_notify` / `lsp_request` methods this slice introduces. Four of
the five outbound effect classes the 4a map named now ride the seam (wire + loop
commands + off-tick fs + LSP); the lone holdout is **TSInstall** (the 1-site
`:TSInstall` fetch+compile `spawn_blocking` → `install_tx`), still direct on `Server`. The
remaining 4-proper slices are the **4d inbound seam** (the `select!` arms' `on_*`
methods — `on_loop_event` / `apply_open` / `apply_save_done` / `on_lsp_event` /
`on_remote_file_changed` / `on_install_done`) and **4e** (hoist the sync tick onto a
standalone `EditHost`); the trailing TSInstall outbound site is small enough to fold into
whichever of those reaches it first rather than carrying its own slice.

#### Phase 4d — the inbound seam: the run loop as a thin translator — ✅ DONE (2026-06-11)

The mirror of 4a–4c's *outbound* `HostEffects` seam: the **inbound** events the tick
reacts to — an LSP reply, a timer firing / child exiting, a file fetched or saved over the
daemon wire, a remote file changed, a `:TSInstall` finishing, a client `nvim_*` call —
arrive on the run loop's seven `select!` transports. Before this slice each arm inlined its
own coalesce-drain + `settle_events` + (for the two quit-capable arms) a direct poke at
`server.editor.should_quit` / `server.fx`, and the LSP arm reached into `server.lsp_dirty`.
That direct reach into tick internals is exactly what the `EditHost` hoist (4e) can't have
in the loop.

Shipped (`crates/bemtvi-server/src/inbound.rs`, a new module — the inbound counterpart to
`edithost.rs`):
- **One translator method per arm** — `on_client_message` (→ `handle`, returns whether to
  quit), `on_lsp_events`, `on_loop_events`, `on_opens`, `on_installs`, `on_save_dones`
  (returns whether to quit), `on_watch_events`. Each takes the first event plus `&mut` its
  receiver, coalesces the burst (first + `try_recv` rest) through the **existing** per-event
  handler (`on_lsp_event` / `on_loop_event` / `apply_open` / `on_install_done` /
  `apply_save_done` / `on_remote_file_changed`), and settles — the LSP one keeping its
  `lsp_dirty`-gated settle, verbatim.
- **`quitting()` — the single quit funnel.** The `should_quit` check + the `bemtvi_exit`
  client notification, previously duplicated in the input and save arms, now live in one
  private method both quit-capable handlers call. The loop no longer reads `editor` or `fx`.
- **The loop body is now one line per arm.** `Some(event) = lsp_events.recv() =>
  server.on_lsp_events(event, &mut lsp_events)`, etc.; the two quit-capable arms `break` on
  the handler's `bool`. No arm touches editor / Lua / effect state directly — the property
  4e needs to lift the tick onto a standalone `EditHost`.

**Exit criteria — met.** Pure relocation, zero behavior change (the coalesce/settle/quit
logic is byte-for-byte the same, just moved off the loop): `cargo build` + `fmt --check` +
`clippy -D warnings` clean; the **full workspace** (`cargo test --workspace`, 1345 tests /
78 binaries) green — and since the run loop is the one path *all* behavior flows through,
that sweep is the proof: the quit path (`:q` / `:wq` ack-gated quit), the LSP
`lsp_dirty`-coalesced settle, the daemon open/save/watch arms, and the timer/process arm
are each exercised by their existing suites (`editing`, the 114-test `bemtvi` LSP suite,
`daemon_*`, `uv_process`). Only **4e** remains in Phase 4-proper: hoist the sync state +
tick methods off `Server` onto a standalone `EditHost` holding `&mut dyn HostEffects` (and
fold in the one trailing TSInstall outbound site).

#### Phase 4e — hoist the tick onto the standalone `EditHost` — ✅ DONE (2026-06-11)

The capstone of Phase 4-proper: the monolithic `Server` struct — which already held
`editor` / `lua` / the per-frame caches and, since 4a–4c, `fx: Box<dyn HostEffects>` — *is*
the synchronous edit-host, so the hoist is the renaming that makes that true and the
removal of the one field that contradicted it. With the outbound effects (4a–4c) and the
inbound translator (4d) both already off the struct, no tick code moved; the work was
making the type *standalone* — coupled to async only through `fx`.

Shipped:
- **The trailing TSInstall outbound site folded onto the seam.** `:TSInstall`'s
  fetch+compile was the **last** raw transport the struct still held:
  `install_tx: UnboundedSender<InstallOutcome>` plus the `tokio::task::spawn_blocking` in
  `excmd.rs`. A new `HostEffects::ts_install(lang)` method (the fifth and final outbound
  class — wire / loop commands / off-tick fs / LSP / **TSInstall**) takes both;
  `NativeEffects` now owns `install_tx` and runs the `spawn_blocking` verbatim, and the
  `:TSInstall` loop is just `self.fx.ts_install(lang)`. The finished job still returns
  *inbound* on the run loop's install arm. After this the struct holds **no tokio channel
  or socket** — every async edge is `fx` (outbound) or a loop arm feeding a tick method
  (inbound).
- **`Server` → `EditHost`.** With the last transport gone, the struct is renamed to name
  what it is: the portable synchronous tick. The run-loop binding becomes `host`, and the
  decomposition the plan named is now literal — `EditHost` (the sync tick + `fx`),
  `NativeEffects` (the native `HostEffects` impl + the outbound transports), and `run()`
  (the inbound transports + the `select!` loop). There is no `Server` struct left: a wasm
  Worker (Phase 5) will construct an `EditHost` with a wasm `HostEffects` and supply its
  own loop, reusing this exact tick.

**Exit criteria — met.** Pure relocation, zero behavior change: `cargo build` +
`fmt --check` + `clippy -D warnings` clean across the workspace (incl. `bemtvi` + `bemtvi-gui`,
the `run`/`run_io` consumers); the **full workspace** (`cargo test --workspace`, 1351 tests
/ 78 binaries) green. The rename is exercised by *every* test (all drive the renamed
`EditHost` through the loop); the TSInstall fold is proven *live* — not merely compiled — by
the hermetic `bemtvi` `ts_install` suite, which drives `:TSInstall` end to end through a local
HTTPS mirror (real gunzip → untar → C compile → `dlopen`) and asserts the freshly-installed
parser highlights/indents — exactly the `fx.ts_install` → `spawn_blocking` → install arm →
`on_install_done` → reload path this slice reroutes. Phase 4-proper is complete: the
reusable sync `EditHost` exists, coupled to the outside world only through `HostEffects`.
The throwaway `bemtvi-edithost-demo` was replaced by Phase 5's wasm cdylib (`bemtvi-edithost`) and deleted in slice 5e.

---

## Phase 5 — wasm edit-host: Worker + input/timer loop + JS interop

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
- **Input over SAB.** The Worker's run loop parks on `Atomics.wait` against a
  `SharedArrayBuffer` keyboard channel the UI fills — no Asyncify. It wakes on a
  keystroke, runs the tick, and posts the redraw back.
- **Timers in the Worker.** Native `vim.defer_fn` / `btv.timer` timers ride
  `evloop.rs` (tokio), which doesn't exist in the wasm edit-host — the plan
  needs a Worker-side analog of `LoopEvent::Timer`. The SAB park *is* the event
  loop: `Atomics.wait` takes a timeout, so set it to the next-due timer's
  deadline and the same park that wakes on input doubles as the timer wheel —
  one mechanism, no busy loop. (Timer-driven statusline refresh depends on
  timers firing.)
- **COOP/COEP serving.** `SharedArrayBuffer` needs cross-origin isolation
  (`Cross-Origin-Opener-Policy: same-origin` + `Cross-Origin-Embedder-Policy:
  require-corp`). Add to the dev server + ship docs.

**Exit criteria.** Driveable via Playwright through the `window.__bemtvi` hook
(project memory `web-client-driveable-via-playwright`): type vim commands, assert
buffer/cursor/redraw, and prove the SAB input/timer loop works end-to-end in a
real browser — keystrokes drive the tick and a timer fires a deferred callback.

### Phase 5-proper — the slices

Phase 4-proper extracted the reusable sync [`EditHost`] (core + Lua + the full tick),
coupled to the outside world only through `HostEffects`. Phase 5 carries it into the
browser. The work splits into five bricks; like Phase 4, the seam grows from the small,
verifiable side, and **nothing is faked** — a browser-unavailable feature (LSP, native
treesitter) must fail *loud* at runtime, not stub a success.

**Binding decisions (set here so the slices don't re-litigate them):**
- **Target `wasm32-unknown-emscripten`, not `wasm32-unknown-unknown`.** Forced, not
  chosen: the edit-host contains Lua (PUC 5.1, C), and only the emscripten target links
  the vendored C cleanly — exactly what `bemtvi-edithost-demo` proved (Phase 0 spike #1,
  the `-fwasm-exceptions` EH gotcha). The interop is therefore emscripten `ccall`/`cwrap`
  (JS→Rust) + `EM_JS` (Rust→JS), **not** wasm-bindgen. This is a *separate* build from
  today's `bemtvi-web` (which is `unknown-unknown` + wasm-bindgen, **core only, no Lua**);
  converging the two web builds is an explicit non-goal for now — `bemtvi-web` stays the
  lean serverless core editor, the Phase 5 build is the full Lua edit-host.
- **A `native` feature on `bemtvi-server` (default-on) is the wasm seam.** The blocker is
  the *dependency tree*, not the tick's source (Open Decision #6): `EditHost`'s crate
  hard-pulls `tokio`, `wtransport` (QUIC), `notify`, `redb`, `getrandom`, `rmp-serde`,
  plus `bemtvi-lsp` (process spawn) and `bemtvi-ts` (C/`dlopen`) — none of which target
  emscripten. `--no-default-features` (the wasm profile, `+ lua51`) must compile the tick
  subset alone. So `native` gates: `lib.rs`'s `run`/`run_io`/`run_daemon_io`, `daemon.rs`,
  `quic.rs`, `evloop.rs`, `inbound.rs`, `host.rs`, the `NativeEffects` impl in
  `edithost.rs`, and the redb `shada` store — and the deps above leave the wasm build's
  tree. What stays un-gated (wasm-eligible): the `EditHost` struct, the `HostEffects`
  trait, and `dispatch`/`input`/`excmd`/`lifecycle`/`effects`/`redraw` + `clipboard` /
  `decoration` / `extmarks` / `keymap` / `save` (snapshot side).
- **v1 excludes LSP and native treesitter.** No language servers in a serverless browser
  (that needs the Phase 6 daemon), and treesitter highlighting is already done JS-side in
  `bemtvi-web` (`web/highlight.js`). The catch the code map surfaced: LSP/TS coupling is
  **threaded through `input.rs` / `excmd.rs` / `dispatch.rs`** (the `gd`/`K` keymap
  actions, the `Lsp*` + `TSInstall` ex-commands, the `nvim_*` LSP API), not isolated to
  `lsp/` + `treesitter.rs`. So gating them out is **per-arm `#[cfg(feature = "native")]`**
  on those call sites, with the `lsp/` subtree and `treesitter.rs` gated whole. Per *no
  silent stubs*: on the wasm build those arms don't vanish silently — where a user could
  reach one (`:LspHover`, `:TSInstall`), it echoes a loud "not available in the browser
  build yet", not a no-op. (A later `wasm`-side treesitter via `web-tree-sitter` and LSP
  via the Phase 6 daemon can re-enable them; v1 is core editing + Lua + redraw.)

#### Slice 5a — the `native` feature seam (the dependency-tree cut) — ✅ DONE (2026-06-11)

Introduced `feature = "native"` (in `default`) and moved the whole async/transport surface
+ the LSP/TS coupling behind it, so `bemtvi-server` compiles with `--no-default-features
--features lua51`. The largest slice; pure feature-gating, no logic change.

Shipped:
- **The deps split** (`Cargo.toml`): `tokio` / `wtransport` / `getrandom` / `redb` /
  `rmp-serde` / `serde` / `notify` / `bemtvi-rpc` / `bemtvi-lsp` / `bemtvi-ts` are now
  `optional`, pulled in only by `native`. The wasm tree keeps `anyhow` / `rmpv` /
  `bemtvi-core` (with `vim-regex` — the C compiles under emscripten) / `bemtvi-lua`.
- **Whole native modules gated:** `daemon` / `quic` / `evloop` / `inbound` / `host` /
  `shada` / `dispatch` (the `bemtvi_rpc::Incoming` router — wasm feeds input via FFI, not
  RPC) / `clipboard` / `lsp/` / `treesitter`, plus `NativeEffects` (the trait stays).
- **Per-arm gating in the wasm-eligible tick:** the `HostEffects` `loop_command` + `lsp_*`
  methods (`LoopCommand` / `bemtvi_lsp` typed); the LSP `EditHost` struct fields + their
  `redraw` projections (empty-array fallbacks keep the wire shape stable); the `Lsp*` /
  `TSInstallInfo` ex-command arms; the LSP keymap defaults (`BuiltinAction` / `NativeDefault`
  / `MappingRhs::Native`); the completion-popup key routing; the off-tick `apply_open` /
  watch reconcile / `sync_buffer_watches`. Per *no silent stubs*, the wasm fallbacks are
  **loud**: the `vim.lsp` / `vim.treesitter` / timer-job Lua drains echo "not available in
  the browser build yet", and `:LspHover` / `:TSInstallInfo` fall to the standard
  `E492: Not an editor command`. `vim.schedule` (no event loop needed) still works.

**Exit criteria — met.**
- **Native unchanged:** `cargo build` + `clippy -D warnings` clean; `cargo test --workspace`
  green (**1360 tests / 79 binaries**) — the gating is `#[cfg]` only, `native` is default, so
  every native path is byte-identical.
- **Wasm-eligibility (host target):** `cargo build -p bemtvi-server --no-default-features
  --features lua51` compiles **warning-free** — the un-gated tick subset references no gated
  symbol.
- **Wasm (emscripten):** with `emsdk` provisioned, `EMCC_CFLAGS=-fwasm-exceptions cargo
  build -p bemtvi-server --no-default-features --features lua51 --target
  wasm32-unknown-emscripten` compiles **warning-free** — the dependency-tree blocker
  (Open Decision #6) is cut: the reusable sync `EditHost` (core + Lua + the full tick)
  now builds for the browser. The actual emcc *link* of a wasm module is slice 5b.

#### Slice 5b — the wasm `HostEffects` + the `bemtvi-edithost` cdylib — ✅ DONE (2026-06-11)

New emscripten crate `bemtvi-edithost` (the demo's successor), depending on `bemtvi-server`
(`default-features = false, features = ["lua51"]`). A `WasmEffects: HostEffects` that
captures `notify` redraw frames into a buffer the UI drains, answers `respond` likewise,
and queues `loop_command` timers for 5d (the off-tick fs / LSP / TSInstall methods
`fail!`-loud — serverless v1). `extern "C"` exports mirror the demo but drive the **real**
`EditHost` tick: construct, feed vim-notation input (→ `EditHost::input` → settle), drain
the latest redraw as msgpack/JSON. Reuse `bemtvi-edithost-demo/build.sh`'s emcc shape.

Shipped:
- **`EditHost` made publicly constructable + drivable** (`bemtvi-server`): `EditHost` and
  `EditHost::new(editor, lua, fx)` are now `pub` — the **one** construction site, shared by
  the native `run_io` (refactored onto it; it then seeds `shada` / `mouse_clock`) and the
  out-of-crate cdylib. The wasm drive surface is a small `#[cfg(not(feature = "native"))]`
  impl block — `attach_ui` (the `nvim_ui_attach` analogue; the dispatch router is gated
  off, so this sets the grid size and paints), `boot` (the serverless startup seed: buffer
  snapshot → lifecycle sets → `emit_lifecycle_events` → `run_pending` → `v:vim_did_enter`,
  the native startup minus config/plugins/shada/LSP-keymaps), `feed` (input → redraw, one
  turn of the run loop's input arm), `exec_lua` (a chunk through the **real** effects path —
  `apply_lua_effects` + `run_pending` — not the demo's hand-drained `take_commands`), and
  `lines`. The native API surface is unchanged (the block is wasm-only); `HostEffects` is
  now `pub use`-exported so the cdylib can implement it.
- **`WasmEffects` + the FFI** (`crates/bemtvi-edithost`, workspace-excluded like the demo /
  `bemtvi-web`): `notify` captures the latest `redraw` params and queues the rest
  (`bemtvi_exit` / scripted selects) for 5c; every **off-tick** effect is genuinely
  unreachable in serverless v1 (`has_remote_fs() == false` gates the fs legs; `respond`
  needs the gated RPC router) and so `unreachable!`-loud, not a silent no-op. `extern "C"`
  exports (`eh_new` / `eh_input` / `eh_exec_lua` / `eh_redraw_json` / `eh_lines` /
  `eh_free*`) drive the real tick; `eh_redraw_json` serializes the captured rmpv frame to
  JSON for the UI. `build.sh` is the demo's emcc shape with the new exports + the system
  emscripten fallback (`/usr/lib/emscripten`).
- **The `:TSInstall` gating gap (slice 5a residue) closed.** 5a gated `:TSInstallInfo` but
  left the `:TSInstall` / `:TSUpdate` ex-command arm ungated, so it reached
  `HostEffects::ts_install` on the wasm build. Now that arm is `#[cfg(feature = "native")]`
  with a `#[cfg(not(feature = "native"))]` companion that echoes a loud "treesitter is not
  available in the browser build yet" — so the user gets a runtime message *and*
  `WasmEffects::ts_install` is truly unreachable (the `unreachable!` is an invariant guard).

- **Exit (needs toolchain) — met.** `EMCC_CFLAGS=-fwasm-exceptions ./build.sh` compiles the
  cdylib to `wasm32-unknown-emscripten` and links it with emcc into `dist/eh.mjs` (system
  emscripten at `/usr/lib/emscripten`); `node harness.mjs` feeds `ihello<Esc>` and asserts
  (1) the buffer lines read back `hello`, (2) the latest **`redraw` frame** — the real
  server view projection through the real tick, not the demo's raw lines — has a grid row
  showing `hello`, and (3) a `vim.cmd("%s/hello/world/")` through the real effects path
  mutates the buffer to `world`. All three PASS. Native regression-clean: `cargo test
  --workspace` green (1360 tests / 79 binaries, unchanged from 5a — this is pure
  relocation, no new native test, and the new construction path is exercised by *every*
  test since `run_io` now builds the host via `EditHost::new`), `clippy -D
  warnings` clean on **both** the native default and the `--no-default-features --features
  lua51` wasm subset, fmt clean (incl. the excluded crate). The throwaway
  `bemtvi-edithost-demo` was deleted in slice 5e once this crate fully superseded it.

#### Slice 5c — the Web Worker + redraw transport + `window.__bemtvi` — ✅ DONE (2026-06-13)

The edit-host now runs in a **Web Worker** — the single `!Send` thread owning core + Lua,
mapping bemtvi's threading model onto the browser exactly as the native edit-host owns its
own OS thread. The UI thread holds **no** editor/Lua state: it ferries input and renders
the redraw. Transport is `postMessage` request/response (correlated by `id`); slice 5d
swaps the UI→worker input leg for the SAB park (so the same wait fires Worker-side timers).

Shipped (`crates/bemtvi-edithost/`):
- **`eh_attach` FFI export** (`src/lib.rs`) — the resize path; re-attaches the UI at a new
  `cols`×`rows` and repaints (the JS side fires it on window resize, the plan's "resize via
  a re-attach" note). `build.sh` exports it and adds the `worker` emscripten environment.
- **`web/worker.mjs`** — loads `dist/eh.mjs`, constructs the real `EditHost` (`eh_new`,
  fail-loud if the Lua VM can't init), and on each `attach`/`feed`/`exec_lua` message drives
  the production tick, then reads the latest `redraw` frame + buffer lines back out and posts
  them UI-ward. The `eh_redraw_json` return is the redraw *params array* `[viewMap]`; the
  worker unwraps the single view map.
- **`web/index.html`** — the UI: a renderer that composes the **server** `redraw` frame
  (the same projection the native TUI consumes — windows by rect offset past the tabline,
  gutters, statuslines, cmdline/message, cursor placement) into a character grid + a cursor
  overlay; a keystroke→vim-notation translator; and the `window.__bemtvi` hook (`feed` /
  `execLua` / `attach` / `lines` / `frame` / `cursor` / `cursorCell` / `mode` / `cmdline` /
  `message`, plus a `ready` promise) for automation.
- **`web/serve.mjs`** — a cross-origin-isolated dev/CI server (COOP `same-origin` + COEP
  `require-corp` + CORP `same-origin` on every response), so `crossOriginIsolated === true`
  — the SAB prerequisite slice 5d needs. (Slice 5e ships the *production* serving docs; this
  is the dev server the verifier runs against.)
- **`web/verify.mjs`** + **`web/package.json`** — the Playwright verifier (the reproducible
  form of the exit criteria) and its `playwright` devDependency.

**Exit criteria — met.** `web/verify.mjs` drives the **real** wasm edit-host in a real
headless Chromium through `window.__bemtvi` and asserts, all PASS: the page is
cross-origin isolated; `ihello world<Esc>` inserts the line (the production tick runs in
the browser); the cursor settles on the last char (col 10) where vim leaves it after
`<Esc>`; the **rendered DOM grid** (not just the FFI return — proving the Worker +
`postMessage` transport + renderer) shows the text; `0dw` deletes the first word; a
`vim.cmd("%s/world/wasm/")` through the real effects path mutates the buffer; and
command-line mode renders the `:` prompt + text on the bottom row. The node smoke test
(`harness.mjs`) and the `--no-default-features --features lua51` clippy (host + wasm
target) stay green; no workspace crate changed (the FFI lives in the workspace-excluded
`bemtvi-edithost`), so the native suite is untouched.

> **Verifier note (env-specific).** `verify.mjs` prefers an explicitly-installed Chromium
> (`PW_CHROMIUM`, else the newest `~/.cache/ms-playwright/chromium-*/chrome`) so the run
> doesn't pin this Playwright build's bundled-browser revision. With a clean
> `npm install` + `npx playwright install chromium` it uses Playwright's own browser.

#### Slice 5d — input + timers over `SharedArrayBuffer` — ✅ DONE (2026-06-13)

The Worker's run loop now parks on `Atomics.wait` against an SAB input ring the UI fills
(Phase 0 spike #3 — no Asyncify), and the **same park's timeout is the next-due timer
deadline**, so one mechanism is both the input wait and the `LoopEvent::Timer` wheel
`evloop.rs` (tokio) can't provide in-Worker. It wakes on a keystroke or the timeout, fires
due timers, runs the tick, and posts the redraw back.

Shipped:
- **The Worker-side timer wheel** (`bemtvi-server`, all `#[cfg(not(feature = "native"))]` —
  the native build is byte-identical). [`EditHost`] gains a `WasmTimer { id, due_ms,
  repeat_ms }` list + a JS clock, with `set_clock` / `next_timer_deadline` /
  `fire_due_timers` and `arm_wasm_timer` / `stop_wasm_timer`. `fire_due_timers` runs each
  due timer's Lua callback through the **real** effects path and repaints once; only timers
  due *at entry* fire per pass (so a 0-delay self-re-arming timer can't spin the wheel), and
  a repeating timer re-arms to `now + repeat_ms` *before* its callback runs.
  `apply_loop_op`'s wasm branch now **arms/stops** these from `vim.defer_fn` / `btv.timer`
  (the `LoopOp::TimerStart` / `TimerStop` it used to echo "not available" for); processes /
  fs-watch (`vim.system` / `jobstart` / `vim.uv.spawn`) still echo loud (Phase 6 daemon).
- **The cdylib FFI** (`bemtvi-edithost`): `eh_set_clock` / `eh_next_deadline` /
  `eh_tick_timers` (build.sh exports them).
- **The Worker SAB loop** (`web/worker.mjs`): when the page is cross-origin isolated it
  enters a blocking loop draining a framed byte ring (`[type:u8][reqId:u32][len:u32][payload]`;
  feed / exec_lua / attach), fires due timers on every wake (`eh_tick_timers`), posts a
  redraw out (posting out is never blocked), and parks with `eh_next_deadline - now` as the
  `Atomics.wait` timeout. The clock is set to *now* before draining so a timer armed by the
  batch dates from now. Falls back to the 5c `postMessage` path when SAB is unavailable.
- **The UI transport** (`web/index.html`): picks SAB vs. postMessage by capability
  (`crossOriginIsolated && SharedArrayBuffer`), writes framed input into the ring +
  `Atomics.notify`, and resolves the `feed` / `execLua` / `attach` promises by `reqId`
  carried back in the redraw's `acks` / `results`. `window.__bemtvi.sab` reports the mode.

**Exit criteria — met.** `web/verify.mjs` (real headless Chromium): all 5c checks still
pass; the SAB input/timer loop is active (`__bemtvi.sab === true`, cross-origin isolated); a
one-shot `vim.defer_fn` rewrites the buffer **on its own** ~150 ms later with **no further
input** (only the Worker's park timeout could have fired it); and a self-rescheduling
`defer_fn` chain fires ≥5 times unattended. Native `cargo test --workspace` green (the lone
`mouse_example_config_runs` miss under full-suite load is the documented load-sensitive
message-line redraw flake — passes in isolation; my Rust is all `cfg(not(native))`, so the
native binary is unchanged); clippy clean on native + the `--no-default-features` wasm subset.

> **Clarification (not a 5d gap):** while writing the timer test the `vim.api.nvim_buf_*`
> *mutation* surface (`nvim_buf_set_lines`, …) showed up as `nil` — but that is **by
> design and project-wide** (native *and* wasm load the same `prelude/api.lua`, whose
> header declares the mutating entity surface — `nvim_buf_set_lines`/`set_text`/`set_name`,
> `nvim_open_win`, `nvim_win_set_*`, `nvim_create_buf`, `nvim_feedkeys`, `nvim_buf_attach`
> — *intentionally absent*: bemtvi's config API is autocmds / diagnostics / keymaps /
> options, not entity mutation). The *read* getters + extmarks exist. So the 5d test
> mutates via `vim.cmd` / keystrokes, which is the supported path — there is nothing to
> "wire in." (An earlier draft of this note mis-filed it as a wasm follow-up; corrected.)

#### Slice 5e — COOP/COEP serving + docs; delete `bemtvi-edithost-demo` — ✅ DONE (2026-06-13)

The cross-origin-isolation serving requirement (SAB → `crossOriginIsolated`) is now
documented for production, and the throwaway demo is gone.

Shipped:
- **Production serving docs + a ready `_headers`** (`crates/bemtvi-edithost/web/`):
  `web/_headers` sets `Cross-Origin-Opener-Policy: same-origin` +
  `Cross-Origin-Embedder-Policy: require-corp` + `Cross-Origin-Resource-Policy:
  same-origin` for `/*` (Netlify / Cloudflare Pages format). The README's *Serving in
  production* section explains the requirement (without the headers the page degrades to
  the slow `postMessage` transport and timers never fire — `window.__bemtvi.sab` reports
  which mode is live) and gives nginx / Apache / generic recipes plus the `application/wasm`
  mime note. The dev/CI server `web/serve.mjs` already sets the same three (slice 5c).
- **`bemtvi-edithost-demo` deleted** — the Phase 4 throwaway that only proved core+Lua
  *compile and run* in wasm. `bemtvi-edithost` (the real `bemtvi-server` tick in a Worker)
  has fully superseded it, so the crate dir is removed, dropped from the workspace
  `exclude` list, and every "supersedes the demo / deleted in 5e" reference scrubbed to
  past tense across `Cargo.toml` / the crate README / `src/lib.rs`.

**Exit criteria — met.** `web/serve.mjs` + `web/_headers` make `crossOriginIsolated ===
true` (the `verify.mjs` run asserts it, and the SAB timer path depends on it — both green);
the build + serving + production-headers docs ship in the crate README; `git ls-files`
shows no `bemtvi-edithost-demo`, and `grep -r bemtvi-edithost-demo` over the tree returns only
this plan's historical narrative. **Phase 5 is complete** — the full Lua edit-host runs in
the browser (core + Lua + autocmds + redraw + Worker-side timers), driven over SAB, served
cross-origin-isolated; what remains browser-side is Phase 6 (the daemon over WebTransport
for real files/processes/LSP) and the v1 feature gaps that are genuinely *gated out* of the
wasm build (LSP + native treesitter). (The `vim.api.nvim_buf_*` *write* surface being `nil`
is **not** one of these — it is intentionally absent in every build by the btv.* config-API
design; see the slice-5d clarification above.)

> **Toolchain prerequisite (now provisioned).** Slices 5b–5e — and the wasm half of 5a —
> require the emscripten SDK (`emcc`, at `/usr/lib/emscripten`), the
> `wasm32-unknown-emscripten` rustup target, and (5c–5d) a browser + Playwright. **All are
> now installed** in the dev environment (emcc 6.0.0-git, the wasm target, Playwright 1.60
> + a Chromium under `~/.cache/ms-playwright`), so 5b's node harness and 5c's headless
> browser exit criteria run for real here — claimed green only after running against that
> toolchain, per *no silent stubs / don't conflate loads with works*.

---

## Phase 6 — Browser fs/process: the daemon over WebTransport (or serverless)

Tie the browser edit-host to actual files/processes, reusing the Phase 1 traits and
the Phase 3 daemon.

### Phase 6a — serverless OPFS filesystem (`:e` / `:w` against the browser's OPFS) — ✅ DONE (2026-06-13)

The serverless half of Phase 6, sliced first (self-contained — no daemon, no QUIC, no
auth/cert) on the same "simplest path first / prove the seam" discipline that scoped 3a
to the startup file and 3d to the initial open. Until this slice the browser edit-host
was **in-memory only**: `boot()` seeded an empty `[No Name]` buffer, `has_remote_fs()` was
`false`, and every fs effect was unreachable — there was no way to open or persist a real
file. Now `:e` / `:w` operate on the browser's **Origin Private File System (OPFS)**, so
edits survive a reload.

**The shape — reuse the off-tick seam, not the sync `HostFs`.** The plan's framing
("`HostFs` backed by OPFS") implied the synchronous Phase-1 trait, but that is
*impossible* without Asyncify (which Phase 0 deliberately avoids): OPFS handle
acquisition — `navigator.storage.getDirectory()`, `getFileHandle`,
`createSyncAccessHandle()` — is **asynchronous**; only a `FileSystemSyncAccessHandle`'s
*operations* (`read`/`write`/`truncate`/`getSize`) are synchronous. So OPFS is, from
core's view, an **off-tick fs** even though it is local — and the slice reuses the *exact*
machinery the daemon path built (Phase 3d/3e/3f): `has_remote_fs() → true`, so `:e` / `:w`
defer to a `PendingOpen` / `PendingSave`, and the **Web Worker fulfills them against OPFS
between ticks** (when it isn't parked on `Atomics.wait`, so the event loop runs and the
OPFS promises resolve). The OPFS analogue of the native `select!` arms — only the
transport is OPFS instead of the QUIC wire. No new core seam; the daemon path and the
serverless path are now the *same* off-tick design with two transports.

Shipped:
- **Core/server (the wasm-eligible tick, `bemtvi-server`):** three public methods on the
  `#[cfg(not(feature = "native"))]` `EditHost` drive surface — `enable_offtick_fs()` (turns
  on `Editor::set_host_fs_offtick`), `complete_fs_read(buffer, path, kind, contents)` (the
  read applier: kind file/new/dir/err — reuses the *ungated* subset of the native
  `load_replica`: `load_str_into` + cleared `announced` → `BufReadPost`/`FileType` +
  snapshot/mirror + `run_pending`; a directory echoes loud "not supported yet", deferring
  the OPFS explorer), and `complete_fs_write(save, ok, size, mtime, err)` (the write
  applier: reuses the shared `apply_save_done`, so the `written` echo, ack-gated `[+]`
  clear, `FileStat` baseline, deferred-`:wq` replay, and per-buffer/`:wqa` serialization
  behave **identically** to the daemon save path). The native binary is byte-identical —
  every line is inside the existing wasm-only `impl` block.
- **The cdylib (`bemtvi-edithost`):** `WasmEffects::has_remote_fs() → true`; `fs_fetch` /
  `fs_save` record the request into the `Sink` (a read list; a write queue + a
  seq→`PendingSave` map holding the snapshot bytes) instead of `unreachable!`. New FFI:
  `eh_take_fs_requests` (drains the queued reads/writes as JSON), `eh_save_bytes` /
  `eh_save_len` (hand a write's snapshot bytes to JS), `eh_fs_read_complete` /
  `eh_fs_write_complete` (land the OPFS result back through the two appliers). `eh_new`
  also injects a **`WasmBlockingSystem`**: `btv._system` (the blocking shell-out behind
  `vim.fn.system`) now fails *loud* with a named "processes are not available in the
  browser build yet" rather than `StdBlockingSystem`'s cryptic emscripten spawn errno —
  the serverless "fail loud, name what's missing" for the process half.
- **The Worker (`web/worker.mjs`):** OPFS helpers (`opfsRead` / `opfsWrite` — descend the
  OPFS root by path component, sync-access-handle read/write; a missing file/parent is a
  *new* buffer, a directory is kind 2) and `fulfillFsRequests()` — drains `eh_take_fs_requests`,
  runs each OPFS op, lands it back, and **loops until dry** (a landed read fires
  `BufReadPost` autocmds that may enqueue more opens/saves). The SAB run loop is now
  `async` with an `await fulfillFsRequests()` after each input drain (and the postMessage
  fallback fulfills before posting its frame) — so the `:e`/`:w` `feed` promise resolves
  only once the buffer/save has landed. `Atomics.wait` blocking inside the async loop is
  fine: the `await` fully settles before the park.

**Exit criteria — met.** `web/verify.mjs` (real headless Chromium, the same harness as
5c/5d) adds, all PASS alongside the existing 11 checks: `:w /bemtvi-verify/rt.txt` saves a
buffer to OPFS and `vim.bo.modified` clears **only after** the write acks; the saved bytes
are read back through the **raw OPFS API** (`navigator.storage` — a path the editor never
touches), proving they truly landed in storage; `:e!` reloads the file from OPFS,
discarding an unsaved in-memory edit (so the reloaded content can only have come from
storage — the read leg); and `btv._system({...})` returns a clear `code = -1` + a "not
available in the browser build" message (the process half fails loud). Native
`cargo test --workspace` regression-clean (my Rust is all `cfg(not(native))`, so every
native binary is byte-identical — the lone load-sensitive `mouse` flake passes in
isolation); clippy `-D warnings` clean on the native default **and** the
`--no-default-features` wasm subset; fmt clean (incl. the excluded crate); the node
`harness.mjs` smoke test still green.

**The OPFS file explorer landed next (same slice family).** `:e <dir>` now lists a real
OPFS directory (netrw), and descending / `../` / opening an entry navigate the browser's
OPFS tree — the directory analogue of the file open, mirroring how Phase 3g added the
remote explorer to the daemon read leg. The whole explorer (`enter_dir` /
`explorer_open_entry` / `explorer_open_file`) was *already* off-tick-aware in core (it
enqueues a `PendingOpen` and routes entry-is-dir off the listing's trailing `/`, no remote
stat); the one missing piece was landing the entries — so the cdylib's `opfsRead`
enumerates a directory (`FileSystemDirectoryHandle.entries()`) into
`[{ is_dir, name }, …]`, the read reply gains a directory shape (`kind == 2`, the canonical
dir + entries JSON), and `eh_fs_read_complete` routes it to a new
`EditHost::complete_fs_read_dir` → `Editor::load_dir_into` (the wasm twin of the native
`load_dir_replica`). Verified: `web/verify.mjs` lists `/xpl` (two files written via `:w`)
and opens an entry with `gg j <CR>`, reading it back from OPFS.

**Deferred to later Phase 6 slices (not stubbed):** a watch leg (OPFS has no
change-notification and the serverless editor is its sole writer, so there's nothing to
reconcile); and the **WebTransport/QUIC daemon path** below (real remote files + processes
+ LSP) — the heavy half, which reuses the *same* off-tick `fs_fetch`/`fs_save` seam this
slice exercised, only crossing a QUIC stream instead of OPFS. **The fs leg of that daemon
path landed next — Phase 6b below.**

### Phase 6b — browser edit-host ↔ daemon over WebTransport (the fs leg) — ✅ DONE (2026-06-13)

The remote half of Phase 6, sliced first to the **fs read + write leg** on the same
"simplest path first / prove the transport" discipline 6a used for OPFS. The browser
edit-host is now a `HostFs` client of a real `bemtvi --daemon --listen` over **WebTransport
(HTTP/3 / QUIC)** — the browser twin of the native `connect_quic` fs leg (Phase 3d/3e), and
the *inverse* of the deleted Socket.IO whole-editor-remote topology. Editing stays in the
Worker (zero per-keystroke round-trips); only `:e`/`:w`/`:e <dir>` cross the wire.

**The seam was already transport-agnostic** (the keystone): 6a made the wasm edit-host's
off-tick fs seam (`eh_take_fs_requests` → JS fulfils → `eh_fs_read_complete` /
`eh_fs_write_complete`) carry OPFS. This slice swaps OPFS for a WebTransport RPC client in
the *same* `fulfillFsRequests()` drain loop — **no core/wasm/Rust change at all** (the
native binary and the whole `cargo test --workspace` are byte-identical; zero `.rs` touched).
`has_remote_fs()` is already `true`, so `:e`/`:w` defer the same `PendingOpen`/`PendingSave`;
the appliers 6a proved are reused verbatim.

**The one genuinely new piece is browser-side: a JS msgpack-RPC client** (`web/rpc.mjs`), the
JS twin of `bemtvi-rpc`'s `Rpc` + reader task (the Worker has no tokio — the point of
`EditHost`). It wraps one WebTransport bidi stream: a msgid counter + pending-reply map, a
`for await (const frame of decodeMultiStream(readable))` reader loop that resolves responses
(`[1,msgid,err,result]` — reject on a non-nil `err`, fail loud) and surfaces notifications
(`[2,method,params]` via `onNotify`, unused by the fs leg, ready for the next), and
`encode([0,msgid,method,params])` writes. `dialDaemon(uri)` parses the launch-printed
`bemtvi://HOST:PORT/TOKEN?cert=HASH`, builds `https://HOST:PORT/TOKEN` (token on the CONNECT
path, the daemon reads `request.path()`) + `serverCertificateHashes` (dotted-hex →
`Uint8Array(32)`, TOFU), awaits `.ready`, opens the bidi stream. **msgpack is a real vendored
library** — `@msgpack/msgpack`, staged into `web/vendor/msgpack/` by `build.sh` from the
`web/` devDependency (gitignored/regenerated like the tree-sitter assets); its
`decodeMultiStream` solves the bidi-stream frame-splitting for free (one decoded value per
complete msgpack frame, `bin` → `Uint8Array`).

Shipped:
- **`web/rpc.mjs`** — `RpcClient` + `dialDaemon` (above); fails every in-flight request loud
  on a dropped QUIC session (`transport.closed`) or stream EOF — no silently-hung `:e`/`:w`.
- **`web/worker.mjs`** — a `?daemon=` on the Worker's own URL makes it `dialDaemon` before
  "ready" (self-configures, no boot-message race). `fulfillFsRequests()`'s OPFS-fallback
  branch forks to `daemonRead`/`daemonWrite` when in daemon mode, projecting `fs_read`'s
  `["file",bin]`/`["new"]`/`["dir",canon,[[is_dir,name]…]]` and `fs_write`'s `["ok",stat?]`
  onto the exact `{kind,text,path?}` / `{ok,size,mtimeMs,error}` shapes the appliers expect.
  In daemon mode fs **never** silently falls back to OPFS — a dial failure surfaces and every
  request errors with that reason. The picker (`boundPaths`) branch still wins first; the SAB
  park / input loop is unchanged (a daemon request is an `await` that settles before the
  park, exactly like an OPFS promise). **Config + shada stay LOCAL (OPFS)** even in daemon
  mode — the thesis (only I/O crosses the wire).
- **`web/index.html`** — forwards a page `?daemon=<uri>` onto the Worker's script URL; no
  param = serverless OPFS, unchanged.
- **The daemon needed no change** — Phase 3r's `wtransport` listener is already
  browser-compatible (the feasibility spike confirmed headless Chromium accepts its
  self-signed cert via `serverCertificateHashes` with **no launch flags and no cert tweak**).

**Exit criteria — met.** `web/verify-daemon.mjs` (real headless Chromium + a real
`--daemon --listen` on an ephemeral loopback port, the browser twin of `daemon_quic.rs`):
`:e <file>` fills the buffer with the **daemon's** bytes (a path the browser origin can't
hold — they can only have crossed the wire), the buffer binds to the remote path; an edit +
`:w` clears `[+]` **only after** the daemon acks, and the edited bytes are read back **from
the daemon's disk in Node** (proving the write truly landed remotely, not in-memory); and
`:e <dir>` lists the daemon's directory entries over the wire. Native `cargo test
--workspace` green and byte-identical (zero `.rs` changed); the existing browser suites
(`verify.mjs` serverless OPFS, `verify-fs.mjs` picker, `verify-shada.mjs`) all stay green —
serverless mode (no `?daemon=`) is untouched.

**Deferred to later Phase 6 slices (not stubbed):** the other five wire legs over
WebTransport — **proc** (`HostProc`), **sys_run** (the blocking `btv._system` — today the
browser fails it loud), **lsp**, **watch** (`fs_changed` push → the `onNotify` hook this
slice's `RpcClient` already exposes), and **luafs** (`vim.uv.fs_stat`/`filereadable`) — each
reuses the same `RpcClient`, the process legs adding the daemon→browser notification routing;
the per-`HostServices`-class **QUIC stream split** (the HOL-blocking escape, one bidi stream
here) — a shared native+browser follow-up already deferred by Phase 3r; and an in-browser
connect UI / live re-point beyond the `?daemon=` param.

### Phase 6c — browser edit-host ↔ daemon over WebTransport (the watch leg) — ✅ DONE (2026-06-13)

The second daemon leg in the browser, and the first to use the **daemon→edit-host push
direction** the fs leg (6b) never needed. Where 6b's fs reads/writes are request/response
always *initiated by the Worker during a tick*, the watch leg has the daemon **own change
detection** and *push* `fs_changed [path, stat?]` when a watched file drifts; the browser
turns each push into the same `FileChangedShell` reconcile (autoreload / W11 / W12 /
handler choice) the native `watch_rx` arm runs — the browser twin of Phase 3l, reusing
6b's `RpcClient` and 6b's off-tick re-fetch appliers. Editing stays in the Worker; only the
change notification + the reload re-fetch cross the wire. (Slice direction confirmed with
the requester, 2026-06-13.)

**The daemon needed no change** — `run_daemon_io` already routes every `fs_*` method
(including `fs_watch` / `fs_unwatch`) to `serve_fs_daemon_on`, which baselines each watched
path and pushes `fs_changed` on a `WATCH_POLL` (200 ms) drift (Phase 3l/3q/3r). This slice is
purely the **browser/edit-host** half — the inverse of 6b, which was purely browser-side too.

**The reconcile body was already there; only its *entry points* were native-gated.** The
watch policy (detection → `'autoread'` silent reload / `FileChangedShell` round-trip /
`v:fcs_choice` / `FileChangedShellPost`) is in `EditHost` and worked for the native daemon
already; the wasm build just never reached it (`on_remote_file_changed` and the watch-arm
`sync_buffer_watches` branch were `#[cfg(feature = "native")]`, and `WasmEffects::fs_watch`
was `unreachable!()`). So the Rust change is a small de-gating, not new policy.

Shipped:
- **Shared reconcile body** (`bemtvi-server` `lifecycle.rs`) — `on_remote_file_changed`'s body
  lifted into an un-gated `reconcile_remote_change(path, stat)`; the native run-loop wrapper
  (`on_remote_file_changed(WatchEvent)`) and a new wasm `EditHost::remote_file_changed`
  (decomposed `(path, has_stat, size, mtime_ms)` — the daemon wire types are native-only)
  both delegate to it. `remote_reload` un-gated; `fire_file_changed_post` made `pub(crate)`
  for the wasm applier. `load_replica_wasm` now fires the deferred `FileChangedShellPost` from
  `reload_posts` when a watch-driven re-fetch lands (mirrors the native `load_replica`).
- **The wasm watch-arm branch** — the `#[cfg(not(feature = "native"))] sync_buffer_watches`
  no-op became the **remote branch** (the native build's `has_remote_fs()` path verbatim,
  paths-only): every file-backed buffer arms one watch through `HostEffects::fs_watch`. The
  wasm `WasmEffects::fs_watch` / `fs_unwatch` enqueue into the `Sink` instead of `unreachable!`.
- **Two new FFI exports** (`bemtvi-edithost` cdylib) — `eh_take_watch_requests` (drains the
  arm/disarm queue as `{"arm":[…],"disarm":[…]}` for the Worker to forward) and
  `eh_remote_file_changed(path, has_stat, size, mtime_ms)` (the Worker calls it from
  `RpcClient.onNotify`; it builds the `FileStat`, runs `reconcile_remote_change`, and settles —
  draining any enqueued reload into the fs-request queue and repainting, exactly as the native
  `on_watch_events` tail does). Added to `build.sh`'s `EXPORTED_FUNCTIONS`.
- **Worker wiring** (`web/worker.mjs`) — `daemon.onNotify` queues each push; the run loop
  applies queued pushes (`applyDaemonNotifications`, run-loop-side so the wasm tick has one
  consumer — no reentrancy), forwards watch arms (`drainWatchRequests` → `fs_watch` /
  `fs_unwatch`), and **receives** pushes by parking on **`Atomics.waitAsync`** (not blocking
  `Atomics.wait`) whenever a daemon session has watches armed: a thread frozen in
  `Atomics.wait` can't run the WebTransport reader, so an unsolicited push would sit until the
  next keystroke. The async park stays event-loop-live; a push wakes it at once
  (`daemonWake()` race) and input still wakes it (SEQ notify), capped so a dangling wait
  clears. **Serverless OPFS keeps the cheaper blocking park** — it has no pushes (the tab is
  the sole writer, so a watch arm is dropped, not a silent stub). The 5c postMessage fallback
  applies pushes inline (no run loop to race).

**Exit criteria — met.** `web/verify-watch.mjs` (real headless Chromium + a real `--daemon
--listen`, the browser twin of `daemon_watch.rs`): `:e <file>` arms the watch, then a Node
rewrite of the file **on the daemon's disk** (a path the browser origin can't touch, with
**no** `:checktime`) autoreloads the new bytes in the browser buffer over the wire — so the
daemon detected, pushed, and the browser re-fetched on its own; and with `'noautoread'` a
`FileChangedShell` handler fires on the edit-host with `v:fcs_reason = "changed"` and its
`v:fcs_choice = "reload"` drives the off-tick re-fetch. Native `cargo test --workspace` green
(993 passing, incl. the unchanged `daemon_watch` native suite — the de-gating didn't regress
it), fmt + clippy `-D warnings` clean; the existing browser suites stay green — serverless
OPFS (`verify.mjs`) and the 6b fs leg (`verify-daemon.mjs`) are untouched (the watch-arm path
no-ops without a daemon, and the park is unchanged when no watch is armed).

**Deferred to later Phase 6 slices (not stubbed):** the remaining four wire legs over
WebTransport — **proc** (`HostProc`; the process legs add the daemon→browser
`proc_spawned`/`proc_exited` notification routing this slice's push plumbing now proves),
**sys_run** (blocking `btv._system` — still fails loud in the browser), **lsp**, and **luafs**
(`vim.uv.fs_stat` / `filereadable`); the per-`HostServices`-class **QUIC stream split** (one
bidi stream still); and an **event-driven push wakeup** to replace the async-park cap — a
second worker (or the UI thread) owning the connection and poking the edit-host Worker's SAB,
folded into the same deferred stream-split follow-up.

### Phase 6d — browser edit-host ↔ daemon over WebTransport (the proc leg) — ✅ DONE (2026-06-14)

The third daemon leg in the browser, and the first to carry a **process** over the wire:
async `vim.system` / `jobstart` (with an `on_exit`) has no local process in the browser, so
the spawn crosses to the daemon, runs there, and its pid/exit return as the daemon→browser
pushes the watch leg (6c) built the push plumbing for. The browser twin of native Phase 3c,
reusing 6b's `RpcClient` and 6c's push-routing + async-park machinery. (Slice direction
confirmed with the requester, 2026-06-14.)

**The daemon needed no change** — `run_daemon_io` already routes every `proc_*` method to
`serve_proc_daemon_on`, which runs the child through the same `StdHostProc` the local server
uses and relays its `LoopEvent`s as `proc_spawned`/`proc_exited` notifications (Phase 3c/3q/3r).
This slice is purely the **browser/edit-host** half, the inverse-direction twin of 6b.

The shape differs from the fs legs by one structural fact: processes do **not** flow through
`HostEffects::fs_*` or the off-tick replica path — they ride the **event-loop command** seam
(`LoopOp::Spawn`/`Kill`), which the native build routes to the tokio actor via
`loop_command(LoopCommand::Spawn)`. The wasm build has no event-loop actor, so the proc
spawn/kill became **new wasm-gated `HostEffects` methods** (`proc_spawn` / `proc_kill` /
`has_remote_proc`) the editor tick enqueues into the `Sink`, mirroring how `fs_fetch`/`fs_save`
work — and the child's pid/exit land back through new inbound `EditHost` methods
(`proc_spawned` / `proc_exited`), the wasm twins of the native `on_loop_event` arms.

Shipped:
- **The wasm proc seam** (`bemtvi-server` `edithost.rs` + `effects.rs`) — `HostEffects` grew
  three `#[cfg(not(feature = "native"))]` methods: `proc_spawn(id, cmd, cwd, env, stdin)`,
  `proc_kill(id)`, and `has_remote_proc()`. `apply_loop_op`'s wasm `LoopOp::Spawn`/`Kill`
  branch — which used to fail loud unconditionally ("not available in the browser build yet")
  — now gates on `has_remote_proc()`: with a daemon connected it enqueues the spawn/kill;
  serverless OPFS (no process host) still fails *loud* in the tick ("require a daemon — :connect
  to one"). `has_remote_proc` is distinct from `has_remote_fs` (always `true` on wasm — OPFS is
  an off-tick fs even with no daemon) precisely because a process has **no serverless fallback**.
- **The inbound `EditHost` methods** (`lib.rs`, wasm impl block) — `proc_spawned(id, pid)`
  records the child's pid via `set_process_pid` (the handle's `.pid`; native's `ProcessSpawned`
  arm) and `proc_exited(id, code, stdout, stderr)` runs the `on_exit` callback with the result
  table, drains its effects, and `settle_events` + repaints (native's `ProcessExit` arm plus the
  run loop's trailing settle) — a chained spawn / off-tick `:edit` the callback queues drains in
  the same convergence, exactly as `remote_file_changed` does for a watch reconcile.
- **The cdylib FFI** (`bemtvi-edithost`) — `Sink` gained `proc_spawns` / `proc_kills` /
  `daemon_connected`; `WasmEffects` implements the three seam methods over them. Four exports:
  `eh_set_daemon_connected` (the Worker flips it on `:connect` / `?daemon=` / disconnect),
  `eh_take_proc_requests` (drains the spawn/kill queue as JSON for the Worker to forward),
  `eh_proc_spawned`, and `eh_proc_exited` — the last taking stdout/stderr as **pointer+length**,
  not a C string, because process output is arbitrary bytes (NUL / non-UTF-8) a C string would
  truncate (Lua strings are byte strings, so the callback sees them faithfully).
- **Worker wiring** (`web/worker.mjs`) — `drainProcRequests` forwards each spawn/kill as a
  `proc_spawn`/`proc_kill` notification; `applyDaemonNotifications` routes `proc_spawned`/
  `proc_exited` pushes (copying the `bin` stdout/stderr into wasm memory for `eh_proc_exited`);
  a `liveProcs` set joins `armedWatches` in gating the **async park** — a daemon session with a
  child in flight parks on `Atomics.waitAsync` (not blocking `Atomics.wait`) so the WebTransport
  reader stays live to receive the unsolicited `proc_exited` push, exactly as 6c does for watch
  pushes. `eh_set_daemon_connected` is flipped on every connect/disconnect path.

**The async-spawn *public* Lua surface (`btv.spawn`, ADR 0002) is still the proposed primitive —
this slice carries the leg, not the wrapper.** When the neovim-plugin-compat runtime was ripped
out (`300cdb0` / `e9bb90c`), the public `vim.system` wrapper went with it, leaving the **funnel**
(`btv._system_async` / `btv._system_kill` / `btv._set_proc_pid` / `btv._proc_pids`) and the native
event-loop handling in place. The native proc leg is in the same funnel-only state, so 6d brings
the browser to **parity** (transport + funnel, no public wrapper yet) rather than inventing the
`btv.spawn` API — that public surface is a separate, still-proposed slice. The leg is ready for it.

**Exit criteria — met.** `web/verify-proc.mjs` (real headless Chromium + a real `--daemon
--listen`, the browser twin of the native daemon proc test): driving the genuine `btv._system_async`
funnel (the exact funnel any public wrapper calls — not a mock), (1) a `sh -c 'printf …'` child's
**stdout round-trips** to the `on_exit` callback over WebTransport with exit code 0; (2) a child
writes a **marker file on the daemon's disk** (a path the browser origin can't touch) that Node
reads back — proving the process truly executed on the daemon, not faked; and (3) a `sleep 30`
child is **killed from the browser** and its `on_exit` fires with a `-1` (killed) code in well
under a second — proving `proc_kill` crossed the wire and terminated the child, not that the sleep
elapsed. Native `cargo test --workspace` green (61 suites, zero failures — the wasm-gated changes
don't touch the native build), fmt + clippy `-D warnings` clean on `bemtvi-server` (native) and
`bemtvi-edithost` (wasm); the existing browser suites stay green — serverless OPFS (`verify.mjs`)
and the 6b fs leg (`verify-daemon.mjs`) are untouched (proc requests no-op without a daemon, and
the park is unchanged when no child is in flight). (A pre-existing, platform-specific clippy error
in the unrelated `bemtvi-gui/src/remote.rs` — `anyhow::Context` unused on Linux, used only in the
macOS/Windows SSH-dialog `cfg` blocks — is present on clean `HEAD` and out of this slice's scope.)

**Deferred to later Phase 6 slices (not stubbed):** the remaining three wire legs over
WebTransport — **sys_run** (blocking `btv._system` — still fails loud in the browser), **lsp**, and
**luafs** (`vim.uv.fs_stat` / `filereadable`); the public **`btv.spawn`** async surface over the
funnel this leg carries (a shared native+browser slice, per ADR 0002); the per-`HostServices`-class
**QUIC stream split** (one bidi stream still — a `proc_*` flood can still HOL-block an `fs_*` save);
a **connection-drop sweep** that fails every in-flight child's `on_exit` with `code -1` (the native
`RemoteHostProc` clears its `Inflight` on EOF; the browser leaves a dropped daemon's children
dangling for now); and an **event-driven push wakeup** to replace the async-park cap.

### Current state of the browser daemon legs (2026-06-17)

Since 6d, two more legs landed browser-side under their **own** plans (they extend
this Worker / `RpcClient`, so they belong on this map even though they were sliced
elsewhere): the **luafs legs** — `btv.fs`'s off-tick `luafs_op` and the streaming
`luafs_watch`/`luafs_unwatch` (`docs/plans/2026-06-16-btv-fs-off-tick-daemon-leg.md`,
project memory `btv-fs-must-route-to-daemon-in-browser`) — and the **terminal leg**,
the web `:terminal` PTY over `term_open`/`term_write`/`term_resize`/`term_kill` +
`term_data`/`term_exit` pushes (Phase 7). So the browser Worker now consumes **fs,
watch, proc, luafs (op + watch), and terminal**; the daemon (`run_daemon_io`) serves
**every** leg already (`fs_*`/`proc_*`/`term_*`/`sys_run`/`lsp_*`/`luafs*`), and the
**native** edit-host consumes all of them over QUIC (`RemoteHostFs`/`RemoteHostProc`/
`RemoteBlockingSystem`/`RemoteLspTransport`/`RemoteLuaFs`). The only legs the **browser**
still lacks are **LSP** and **sys_run** — and LSP is the substantial one (it is not a
pure Worker-forwarding slice like the others, because the LSP *client* itself —
`bemtvi-lsp` on tokio + `async-lsp` — was native-gated and had to be brought to wasm).
**Phase 6e** below is that LSP leg.

> **Phase 6e — DONE (2026-06-17).** The browser edit-host runs language servers on a
> real `bemtvi --daemon --listen` over WebTransport, verified end-to-end
> (`web/verify-lsp.mjs`): server-pushed diagnostics (`didOpen` → `publishDiagnostics`
> land in `btv.diagnostic.get()`) and a hover request/reply (`btv.lsp.hover()` opens the
> content float with the server's markup), both against the scripted mock
> (`bemtvi --__lsp-mock`) the daemon spawns. The slices:
> - **Stage A** — the synchronous `SyncLspClient` (`bemtvi-lsp/src/sync_client.rs`):
>   `bemtvi-lsp` feature-gated (`native` default) so the async manager/transport tree
>   drops on wasm; the protocol/convert/caps transforms stay always-on and are shared
>   verbatim with the async path. (commit 27ae327)
> - **Stage B** — server integration: the `lsp/` consumer de-gated for wasm, the
>   `HostEffects` LSP seam (always-on `lsp_ensure`/`lsp_notify`/`lsp_request`; wasm-only
>   `lsp_stdout`/`lsp_stderr`/`lsp_exited`/`lsp_take_events`/`has_remote_lsp`), and the
>   `EditHost` wasm inbound (`lsp_stdout` feed → `drain_lsp_events` → `on_lsp_event`).
>   (commit 2efb06e)
> - **Stage C** — the `bemtvi-edithost` cdylib: `WasmEffects` holds the `SyncLspClient`
>   and implements the seam (each call flushes the client's `WireOp`s into the `Sink`;
>   inbound feeds the client and drains events); FFI `eh_take_lsp_requests` /
>   `eh_lsp_stdout` / `eh_lsp_stderr` / `eh_lsp_exited`.
> - **Stage D** — the daemon `lsp_*` leg was **already shipped** for the native
>   `RemoteLspTransport` (`serve_one_lsp` in `daemon.rs`); the wasm client speaks the
>   identical wire, so no daemon work was needed.
> - **Stage E** — `web/worker.mjs`: forward `lsp_spawn`/`lsp_stdin`/`lsp_kill`
>   (`drainLspRequests`), land `lsp_stdout`/`lsp_stderr`/`lsp_exited` pushes
>   (`applyDaemonNotifications` + `callLspStdout`/`callLspStderr`), `liveLsp` gating the
>   async park so the reader stays live for server pushes.
> - **Stage F** — `web/verify-lsp.mjs` (above). It surfaced and fixed a real bug: the
>   `sync_lsp()` call in `redraw()` was `#[cfg(feature = "native")]`-gated (stale from
>   when wasm had no LSP), so on wasm the pending `didOpen` after a server's
>   `Initialized` never fired — diagnostics never flowed. Un-gating it (it runs on both
>   builds now) is what makes server pushes work.
>
> The remaining browser leg is **sys_run** (blocking `btv._system` over the wire), per
> the residual note in Open Decision #5.

### The WebTransport/QUIC daemon path (the remaining Phase 6 work)

- **The good path:** the browser edit-host is a `HostServices` client over
  **WebTransport (HTTP/3 / QUIC)** to a remote daemon — the browser analog of
  Phase 3, and the *inverse* of today's Socket.IO mode (`docs/architecture.md` →
  *connecting to a real server over Socket.IO*), which puts the whole server
  (editing included) remote and laggy. Here, editing is in the browser Worker;
  only fs/process cross the wire. A file picker's `rg`/`fd` run on the remote daemon
  via `HostProc`. **Why WebTransport over a single WebSocket** — see *Transport &
  stream multiplexing* below: the daemon carries three independent traffic classes
  (`HostFs` blobs, per-process `HostProc` stdio, `HostWatch` pushes), and a single
  WebSocket is one TCP stream, so a heavy `HostProc` flood (a file picker's `rg`, an
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

**One bemtvi-specific correction to the usual "remote editor" framing:** because the
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
  An earlier draft carried the daemon over `ssh … bemtvi --daemon` (a single ordered
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
- **wasm config ceiling = `lua51`, not LuaJIT.** The browser runs the PUC 5.1
  dialect; config relying on LuaJIT-only behavior (e.g. `ffi`, `bit`) won't run.
  `Luau` (faster non-JIT, 5.1-ish, wasm-capable per mlua) is a possible future
  pivot but a different dialect bet — out of scope here.
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
   move into it explicitly below. The Phase 3 deferred ssh slice (the `ssh … bemtvi
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
   `wasm32-unknown-unknown` `bemtvi-web` outright — no second no-Lua core-only demo
   build. **Both** of today's web clients fold into the single edit-host:
   - the **serverless `WebEditor`** (`crates/bemtvi-web/src/lib.rs`, core-only, no
     Lua) and its bespoke *local* paint path in `index.html` (the
     `serverStyled === false` branches) are deleted — the edit-host *is* the local
     editor now, with Lua;
   - the **`RemoteClient` + Socket.IO bridge** (`remote.rs` + `bemtvi-web-bridge`)
     is superseded too: it is the whole-editor-*remote* topology this plan exists
     to kill (one round-trip per keystroke). The editor moves *into* the browser
     Worker; only fs/process stay remote, behind the daemon (Phase 6). `remote.rs`'s
     *synchronous* msgpack framing is reusable for the new browser↔daemon link, but
     the boundary flips, and `bemtvi-web-bridge`'s per-connection `bemtvi --server`
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
   trait. **Residual:** the *synchronous* surfaces — blocking `btv._system`,
   and any Lua-visible sync fs calls routed remote (`vim.uv.fs_stat`,
   `vim.fn.filereadable`, …) — cannot use an off-tick shape (the caller needs
   the value *now*) and need the **blocking bridge**: a request over
   a channel to the daemon link, editor thread parked until the reply, with the
   link's RPC tasks on their **own** thread/runtime so the parked thread can't
   starve the reader carrying its reply (the deadlock trap — see *Still to do
   in Phase 3* under Phase 3a), plus the short-TTL stat/exists cache to damp
   per-call round-trips. **The bridge is now built and proven — Phase 3n shipped it
   for the blocking `btv._system`** (the `BlockingSystem` seam + `sys_run` wire +
   dedicated-link-thread park, exactly this mechanism); the Lua-visible sync fs
   calls reuse the same bridge when the Lua-visible fs-semantics slice lands.
6. **How the wasm edit-host gets the editor+Lua sync tick** (Phase 4/5) —
   **RESOLVED (2026-06-11): (a) — extract a reusable sync `EditHost` from `Server`,
   async effects behind a `HostEffects` trait.** The tick (`dispatch` →
   `run_pending` → `apply_lua_effects` + the mirrors) is synchronous but lived in
   `impl Server`, entangled with the async fields (`tokio` net→`mio`, `notify`,
   `bemtvi-lsp` subprocess, `bemtvi-ts`). Three shapes were on the table:
   **(a) extract a reusable sync `EditHost`** from `Server` with async effects
   behind a trait — the blessed architecture (it *is* the "full split" seam, serving
   both native latency in Phase 3 and wasm here), the largest refactor but the only
   one with **no tokio in the Worker** and **one sync core for native + wasm**;
   **(b) gate `bemtvi-server` itself to wasm** (target-off `net`/`process`, native
   deps non-wasm, current-thread tokio in the Worker) — reuses all glue but keeps
   tokio in the Worker, against this plan's grain; **(c) a minimal fresh cdylib**
   reimplementing a crude tick (the throwaway `crates/bemtvi-edithost-demo`, which
   proved core+Lua-in-wasm by behavior — the *interim* 2026-06-10 de-risking step,
   now superseded). The empirical finding that makes (a) tractable: the wasm blocker
   is the **dependency tree** (`mio`/net, `notify`, lsp, ts), not `bemtvi-server`'s own
   source, which produced *zero* errors before the build died at `mio`. **(a) chosen**
   — the slice plan is *Phase 4-proper* below. The throwaway demo gets deleted when the
   real `EditHost` lands.
```
