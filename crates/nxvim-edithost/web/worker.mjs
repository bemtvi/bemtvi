// The edit-host Web Worker (Phase 5). This is the single `!Send` thread that owns
// nxvim's core + the PUC Lua 5.1 VM: it loads the emscripten module (`dist/eh.mjs`),
// constructs the real `EditHost` (`eh_new`), and drives the production tick through the
// `extern "C"` FFI. The UI thread holds no editor/Lua state — it ferries input and
// renders the redraw frame the worker posts back.
//
// Two input transports:
//   * slice 5d (SAB): when the page is cross-origin isolated, the UI hands over two
//     SharedArrayBuffers and the worker enters a blocking run loop that parks on
//     `Atomics.wait` against the input channel. The SAME wait's timeout is the next-due
//     timer deadline, so one mechanism is both the input wait and the timer wheel
//     (`vim.defer_fn` / `nx.timer`) that `evloop.rs` can't provide in-Worker. No Asyncify.
//   * slice 5c (postMessage): the fallback when SAB is unavailable (no cross-origin
//     isolation) — request/response messages, correlated by `id`. Timers don't fire in
//     this mode (no run loop to wake them); input still works.
import createModule from "../dist/eh.mjs";
import { dialDaemon } from "./rpc.mjs";

const M = await createModule();

const eh_new = M.cwrap("eh_new", "number", []);
const eh_input = M.cwrap("eh_input", null, ["number", "string"]);
const eh_input_mouse = M.cwrap("eh_input_mouse", null, ["number", "string", "string", "string", "number", "number"]);
const eh_source_lua = M.cwrap("eh_source_lua", "number", ["number", "string"]);
const eh_boot_finish = M.cwrap("eh_boot_finish", null, ["number"]);
const eh_attach = M.cwrap("eh_attach", null, ["number", "number", "number"]);
const eh_set_clock = M.cwrap("eh_set_clock", null, ["number", "number"]);
const eh_next_deadline = M.cwrap("eh_next_deadline", "number", ["number"]);
const eh_tick_timers = M.cwrap("eh_tick_timers", "number", ["number", "number"]);
const eh_exec_lua = M.cwrap("eh_exec_lua", "number", ["number", "string"]);
const eh_redraw_json = M.cwrap("eh_redraw_json", "number", ["number"]);
const eh_lines = M.cwrap("eh_lines", "number", ["number"]);
const eh_free_string = M.cwrap("eh_free_string", null, ["number"]);
// Off-tick OPFS fs (Phase 6): the editor enqueues `:e`/`:w` off-tick; the Worker drains
// the requests, runs the async OPFS op, and lands the result back through these.
const eh_take_fs_requests = M.cwrap("eh_take_fs_requests", "number", ["number"]);
const eh_save_bytes = M.cwrap("eh_save_bytes", "number", ["number", "number"]);
const eh_save_len = M.cwrap("eh_save_len", "number", ["number", "number"]);
const eh_fs_read_complete = M.cwrap("eh_fs_read_complete", null, ["number", "number", "string", "number", "string"]);
const eh_fs_write_complete = M.cwrap("eh_fs_write_complete", null, ["number", "number", "number", "number", "number", "string"]);
// Remote watch leg (Phase 6): the editor arms one watch per file-backed buffer off-tick; the
// Worker forwards each to the daemon and lands its `fs_changed` pushes back into the tick.
const eh_take_watch_requests = M.cwrap("eh_take_watch_requests", "number", ["number"]);
const eh_remote_file_changed = M.cwrap("eh_remote_file_changed", null, ["number", "string", "number", "number", "number"]);
// Remote proc leg (Phase 6d): the editor enqueues async `vim.system` / `jobstart` spawns
// off-tick (only when a daemon is connected — `eh_set_daemon_connected` gates it); the Worker
// forwards each to the daemon and lands its `proc_spawned`/`proc_exited` pushes back into the
// tick. `eh_proc_exited` takes stdout/stderr as pointer+length (process output is raw bytes).
const eh_set_daemon_connected = M.cwrap("eh_set_daemon_connected", null, ["number", "number"]);
const eh_take_proc_requests = M.cwrap("eh_take_proc_requests", "number", ["number"]);
const eh_proc_spawned = M.cwrap("eh_proc_spawned", null, ["number", "number", "number"]);
const eh_proc_exited = M.cwrap("eh_proc_exited", null, ["number", "number", "number", "number", "number", "number", "number"]);
// Treesitter `:TSInstall` leg: the editor enqueues each install off-tick; the Worker
// forwards it to the UI thread (web-tree-sitter lives there), which fetches/caches/registers
// the grammar and lands the outcome back via `eh_ts_install_complete`. `eh_ts_seed_installed`
// tells the core, at boot, which grammars are already available (bundle + OPFS cache).
const eh_take_ts_requests = M.cwrap("eh_take_ts_requests", "number", ["number"]);
const eh_ts_install_complete = M.cwrap("eh_ts_install_complete", null, ["number", "string", "number", "string"]);
const eh_ts_seed_installed = M.cwrap("eh_ts_seed_installed", null, ["number", "string"]);
// Clipboard (`"+`/`"*`) leg: the editor enqueues each `"+`/`"*` yank/delete off-tick; the
// Worker drains it and forwards to the UI thread (only the UI thread can reach
// `navigator.clipboard`) to write out. The UI pushes the OS clipboard back in via
// `eh_clipboard_push` so a `"+p` sees an external copy.
const eh_take_clipboard_writes = M.cwrap("eh_take_clipboard_writes", "number", ["number"]);
const eh_clipboard_push = M.cwrap("eh_clipboard_push", null, ["number", "string"]);
// Persistence (shada): the editor's cross-session snapshot ↔ a JSON blob the Worker keeps
// in OPFS. `eh_export_shada(include_exit)` serializes it; `eh_load_shada(json)` restores it.
const eh_export_shada = M.cwrap("eh_export_shada", "number", ["number", "number"]);
const eh_load_shada = M.cwrap("eh_load_shada", "number", ["number", "string"]);

function readStr(ptr) {
  const s = M.UTF8ToString(ptr);
  eh_free_string(ptr);
  return s;
}

const h = eh_new();
if (h === 0) {
  postMessage({ type: "fatal", error: "eh_new returned null (Lua VM failed to init in wasm)" });
  throw new Error("eh_new returned null");
}

// Serverless config: source a single-file `/init.lua` from OPFS (if present) between the
// two halves of boot (`eh_new` did `boot_begin`; we finish with `eh_boot_finish`), so a
// config's options / keymaps / autocmds — including the startup buffer's `BufEnter` —
// apply to the very first frame (native ordering: config first). `require` of further
// modules won't resolve (the browser build's runtimepath is empty), so this is one file.
// A broken config is surfaced (non-fatal) — the editor still finishes booting.
async function bootWithConfig() {
  try {
    const cfg = await opfsRead("/init.lua"); // kind 0 = file, 1 = absent, 2 = dir, 3 = error
    if (cfg.kind === 0 && cfg.text.length) {
      const err = readStr(eh_source_lua(h, cfg.text));
      if (err) postMessage({ type: "config_error", error: err });
    }
  } catch (e) {
    postMessage({ type: "config_error", error: String(e) });
  }
  // Restore cross-session state (registers/marks/history/jumplist) from OPFS *after* the
  // config (so a config can't clobber a restored mark) and *before* `eh_boot_finish` fires
  // the startup lifecycle — so a restored `` `" `` / registers / history are live for the
  // first frame, matching native's load ordering.
  try {
    const sh = await opfsRead(SHADA_PATH); // kind 0 = present, 1 = none yet
    if (sh.kind === 0 && sh.text.length) {
      const err = readStr(eh_load_shada(h, sh.text));
      if (err) postMessage({ type: "config_error", error: err });
      else shadaBaseline = readStr(eh_export_shada(h, 0)); // seed baseline so the first checkpoint is a no-op
    }
  } catch (e) {
    postMessage({ type: "config_error", error: "shada load: " + e });
  }
  // Seed the core's set of available treesitter grammars (for `:TSInstallInfo`): the offline
  // bundle (vendor/manifest.json) ∪ whatever a previous session installed (OPFS). The UI
  // highlighter reads the same two manifests itself; this only mirrors the list into the core.
  try {
    const avail = new Set();
    try {
      const res = await fetch(new URL("vendor/manifest.json", import.meta.url));
      if (res.ok) for (const l of (await res.json()).languages || []) avail.add(l);
    } catch { /* highlighter assets not built — plain rendering, empty bundle */ }
    const cache = await opfsRead("/.nxvim/treesitter/manifest.json"); // kind 0 = present
    if (cache.kind === 0 && cache.text.length) for (const l of JSON.parse(cache.text)) avail.add(l);
    eh_ts_seed_installed(h, JSON.stringify([...avail]));
  } catch (e) {
    postMessage({ type: "config_error", error: "treesitter seed: " + e });
  }
  eh_boot_finish(h);
}
// NB: invoked at the very END of the module (see below), not here — the OPFS helpers it
// reaches (`splitPath` et al.) are `const` arrows still in their temporal dead zone at
// this point in module evaluation.

// `eh_redraw_json` returns the `redraw` notification's params array `[viewMap]` (or
// "null" before the first frame); the renderable frame is the single view map.
function currentFrame() {
  try {
    const parsed = JSON.parse(readStr(eh_redraw_json(h)));
    return Array.isArray(parsed) ? (parsed[0] ?? null) : parsed;
  } catch (e) {
    postMessage({ type: "fatal", error: `redraw JSON parse failed: ${e}` });
    return null;
  }
}

const nowMs = () => (typeof performance !== "undefined" ? performance.now() : Date.now());
const utf8 = (bytes) => new TextDecoder().decode(bytes);

// =============================================================================
// Real local filesystem (File System Access API) — the `:eo` / `:wo` / … picker family.
//
// Unlike OPFS (the sandboxed default fs), these commands open *real* local files the user
// grants through the browser's native picker. The picker must run on the UI thread (it
// needs a window + a user gesture) and yields a `FileSystemFileHandle`, not a path — so a
// picker-bound file's bytes live behind a handle the UI holds, and its read/write can only
// be done by the UI (async `getFile()` / `createWritable()`). The Worker therefore *routes*
// the editor's off-tick read/write for a bound path UI-ward instead of fulfilling it
// against OPFS, exactly as a daemon session would route it over the wire — only here the
// "wire" is a postMessage to the UI thread and back.
//
// `boundPaths` is the set of editor paths that resolve to a real handle. The UI sends a
// `bind` (a ring frame under SAB, a message under 5c) *before* the `:e`/`:w` it issues, so
// the routing decision is already known when `eh_take_fs_requests` drains the request.
// Everything not bound stays on OPFS.
const boundPaths = new Set();
// Off-tick realfs ops dispatched to the UI and awaiting its reply. While this is > 0 the
// SAB run loop stays event-loop-live (it `await`s a JS promise rather than parking on
// `Atomics.wait`) so the UI's reply postMessage can actually be received — a thread parked
// in `Atomics.wait` can't process its message queue.
let pendingRealFs = 0;
let fsReplyWaiter = null; // resolve() to wake the SAB loop when a realfs reply lands
let sabMode = false; // true once the SAB run loop owns the tick (vs the 5c postMessage path)

// Hand the in-flight write `seq`'s snapshot bytes to the UI to write to the bound handle.
// Copy them out of wasm memory first (`slice`, not `subarray`) — the copy is detach-safe
// across `ALLOW_MEMORY_GROWTH` and can be transferred (zero-copy) to the UI thread.
function dispatchRealFsWrite(seq, path) {
  const ptr = eh_save_bytes(h, seq);
  const len = eh_save_len(h, seq);
  const bytes = ptr ? M.HEAPU8.slice(ptr, ptr + len) : new Uint8Array(0);
  pendingRealFs++;
  postMessage({ type: "fs_write", seq, path, bytes }, [bytes.buffer]);
}
// Ask the UI to read a bound path's real file (its handle is UI-side); the reply lands via
// the `fs_read_result` message.
function dispatchRealFsRead(buffer, path) {
  pendingRealFs++;
  postMessage({ type: "fs_read", buffer, path });
}
// A realfs reply (read content / write result) was just applied into the tick. Under SAB,
// wake the run loop (it reposts the frame and drains any cascade the completion enqueued);
// under 5c there's no loop, so drain + repaint inline.
async function landRealFsReply() {
  pendingRealFs = Math.max(0, pendingRealFs - 1);
  if (sabMode) {
    if (fsReplyWaiter) {
      const r = fsReplyWaiter;
      fsReplyWaiter = null;
      r();
    }
  } else {
    await fulfillFsRequests();
    postMessage({ type: "redraw", frame: currentFrame(), lines: readStr(eh_lines(h)) });
  }
}

// =============================================================================
// Off-tick OPFS filesystem (Phase 6 — serverless).
//
// The editor runs in off-tick fs mode (`has_remote_fs() == true`) because OPFS handle
// acquisition is asynchronous — only a `FileSystemSyncAccessHandle`'s *operations* are
// synchronous, so a synchronous `HostFs` read/write on the editor thread is impossible
// without Asyncify (which this build avoids). So `:e` / `:w` defer to a request the
// editor enqueues; the Worker fulfills it against OPFS *between* ticks (when it isn't
// parked, so the event loop runs and the OPFS promises resolve), exactly as a daemon
// session fulfills the same request over the wire. The OPFS analogue of the native
// `select!` arms (`apply_open` / `apply_save_done`).
// =============================================================================

// Path → OPFS handle. OPFS has one root directory (`getDirectory`); a path descends it
// component by component. Leading `/` and `.` segments are dropped (the editor's paths
// are absolute-looking but OPFS has no real root prefix).
const splitPath = (path) => String(path).split("/").filter((s) => s.length && s !== ".");

async function opfsDir(parts, create) {
  let dir = await navigator.storage.getDirectory();
  for (const p of parts) dir = await dir.getDirectoryHandle(p, { create });
  return dir;
}

// Enumerate OPFS directory handle `dh` → a sorted-agnostic JSON array the editor's
// explorer builder turns into a listing: [{ is_dir, name }, …]. The core listing builder
// sorts (dirs first, case-insensitive) and prepends `../`, so order here is irrelevant.
async function opfsDirEntries(dh) {
  const entries = [];
  for await (const [name, handle] of dh.entries()) {
    entries.push({ is_dir: handle.kind === "directory", name });
  }
  return entries;
}

// Read `path` from OPFS → { kind, text, path? }: kind 0 file (text = its UTF-8 contents),
// 1 a not-yet-existing path (new-file buffer), 2 a directory (text = its entries as JSON,
// `path` = the canonical dir for the explorer), 3 an error (text = message). A missing
// parent dir or missing file is "new" (the editor opens an empty buffer bound to the name,
// savable later) — not an error.
async function opfsRead(path) {
  const parts = splitPath(path);
  const canonicalDir = "/" + parts.join("/"); // "/" for the root
  // The OPFS root itself (`:e /`) is a directory — enumerate it directly.
  if (parts.length === 0) {
    try {
      const root = await navigator.storage.getDirectory();
      return { kind: 2, text: JSON.stringify(await opfsDirEntries(root)), path: "/" };
    } catch (e) {
      return { kind: 3, text: String(e) };
    }
  }
  const name = parts[parts.length - 1];
  let dir;
  try {
    dir = await opfsDir(parts.slice(0, -1), false);
  } catch (e) {
    return e.name === "NotFoundError" ? { kind: 1, text: "" } : { kind: 3, text: String(e) };
  }
  let fh;
  try {
    fh = await dir.getFileHandle(name, { create: false });
  } catch (e) {
    if (e.name === "NotFoundError") return { kind: 1, text: "" };
    if (e.name === "TypeMismatchError") {
      // It's a directory — enumerate it into an explorer listing.
      try {
        const dh = await dir.getDirectoryHandle(name, { create: false });
        return { kind: 2, text: JSON.stringify(await opfsDirEntries(dh)), path: canonicalDir };
      } catch (e2) {
        return { kind: 3, text: String(e2) };
      }
    }
    return { kind: 3, text: String(e) };
  }
  try {
    const ah = await fh.createSyncAccessHandle();
    const buf = new Uint8Array(ah.getSize());
    ah.read(buf, { at: 0 });
    ah.close();
    return { kind: 0, text: new TextDecoder().decode(buf) };
  } catch (e) {
    return { kind: 3, text: String(e) };
  }
}

// Write `bytes` to `path` in OPFS (creating parent dirs), atomically truncating first →
// { ok, size, error }. The editor is the sole writer, so a plain truncate+write is the
// "atomic write" here (no concurrent reader to tear).
async function opfsWrite(path, bytes) {
  const parts = splitPath(path);
  if (parts.length === 0) return { ok: false, error: "empty path" };
  const name = parts[parts.length - 1];
  try {
    const dir = await opfsDir(parts.slice(0, -1), true);
    const fh = await dir.getFileHandle(name, { create: true });
    const ah = await fh.createSyncAccessHandle();
    ah.truncate(0);
    ah.write(bytes, { at: 0 });
    ah.flush();
    const size = ah.getSize();
    ah.close();
    return { ok: true, size };
  } catch (e) {
    return { ok: false, error: String(e) };
  }
}

// =============================================================================
// Off-tick daemon filesystem (Phase 6b) — the WebTransport/QUIC remote.
//
// When the page is opened with `?daemon=nxvim://HOST:PORT/TOKEN?cert=HASH`, the same
// off-tick fs seam that OPFS satisfies serverless-ly is instead fulfilled over a real
// WebTransport connection to a remote `nxvim --daemon --listen` (rpc.mjs). The editor is
// unchanged — `has_remote_fs()` is already true, so `:e`/`:w` defer the same way; only the
// transport that answers `eh_take_fs_requests` differs. This is the browser twin of the
// native `connect_quic` fs leg (Phase 3d/3e): editing stays in the Worker, only fs crosses
// the wire. The other five legs (proc/watch/lsp/sys_run/luafs) are later slices on this
// same `RpcClient`. Config + shada stay LOCAL (OPFS) even in daemon mode — the thesis.
//
// `daemonUri` rides the Worker's own URL (`?daemon=…`), so the Worker self-configures with
// no boot-message race. In daemon mode fs NEVER silently falls back to OPFS: a dial failure
// surfaces loudly and every fs request errors with that reason (CLAUDE.md "fail loud").
// =============================================================================
// `let`, not `const`: the boot value is the `?daemon=` page param (if any), but a runtime
// `:connect nxvim://…` (the browser twin of nxvim-gui's `:connect`) re-points it after boot —
// see `runtimeConnect`. Its truthiness is what switches the off-tick fs/watch seam from the
// serverless OPFS sandbox onto the wire (`fulfillFsRequests` / `drainWatchRequests`).
let daemonUri = new URL(self.location.href).searchParams.get("daemon");
let daemon = null; // an RpcClient once connected
let daemonError = null; // a dial failure reason (daemon mode requested but not connected)

async function connectDaemon() {
  if (!daemonUri) return;
  try {
    daemon = await dialDaemon(daemonUri);
    // The watch leg's `fs_changed` pushes and the proc leg's `proc_spawned`/`proc_exited`
    // pushes arrive here.
    daemon.onNotify = onDaemonNotify;
    // A process host is now reachable: let the editor tick take its async-spawn branch
    // (`vim.system` / `jobstart`) instead of failing loud.
    eh_set_daemon_connected(h, 1);
  } catch (e) {
    daemonError = String(e && e.message ? e.message : e);
    postMessage({ type: "config_error", error: `daemon connection failed: ${daemonError}` });
  }
}

// Runtime `:connect nxvim://…`: dial a daemon *after* boot and re-point the off-tick fs/watch
// seam from OPFS onto the wire — the browser twin of nxvim-gui's client-side `:connect`, which
// the editor core knows nothing about (the UI intercepts it on `<CR>` and routes the URI here).
// Replaces any existing link (a prior `?daemon=` boot or an earlier `:connect`): the old wire
// is torn down, its watches forgotten (the new daemon knows none of them), and future `:e`/`:w`
// route to the new daemon. Posts a `connected` status the UI flashes. Config + shada stay LOCAL
// (OPFS) regardless, exactly as in `?daemon=` mode.
async function runtimeConnect(uri) {
  if (daemon) {
    daemon.close();
    daemon = null;
  }
  // The old wire is gone: no process host until the new dial succeeds, and the new daemon
  // knows none of the old watches or in-flight children.
  eh_set_daemon_connected(h, 0);
  armedWatches.clear();
  liveProcs.clear();
  daemonError = null;
  daemonUri = uri;
  await connectDaemon();
  postMessage({ type: "connected", ok: !!daemon, uri, error: daemonError });
}

// Fulfil one off-tick read over the daemon, projecting `fs_read`'s reply onto the same
// `{ kind, text, path? }` shape `opfsRead` returns (so the applier path is identical).
async function daemonRead(path) {
  if (!daemon) return { kind: 3, text: daemonError || "daemon not connected" };
  try {
    const reply = await daemon.request("fs_read", [path]);
    const tag = Array.isArray(reply) ? reply[0] : null;
    if (tag === "file") return { kind: 0, text: utf8(reply[1]) };
    if (tag === "new") return { kind: 1, text: "" };
    if (tag === "dir") {
      // Daemon entries are `[[is_dir, name], …]`; the wasm dir applier wants `[{is_dir,name}]`.
      const entries = (reply[2] || []).map(([is_dir, name]) => ({ is_dir, name }));
      return { kind: 2, text: JSON.stringify(entries), path: reply[1] };
    }
    return { kind: 3, text: `unexpected fs_read reply: ${JSON.stringify(reply)}` };
  } catch (e) {
    return { kind: 3, text: String(e && e.message ? e.message : e) };
  }
}

// Fulfil one off-tick write over the daemon, projecting `fs_write`'s `["ok", stat?]` reply
// onto the `{ ok, size, mtimeMs, error }` the write applier needs. An RPC error rejects →
// a loud `{ ok: false }` (the editor's saved-state clears only on a real ack).
async function daemonWrite(path, bytes) {
  if (!daemon) return { ok: false, error: daemonError || "daemon not connected" };
  try {
    const reply = await daemon.request("fs_write", [path, bytes]);
    const stat = Array.isArray(reply) ? reply[1] : null; // [secs, nanos, size] | null
    if (Array.isArray(stat)) {
      const [secs, nanos, size] = stat;
      const mtimeMs = secs != null ? secs * 1000 + Math.floor((nanos || 0) / 1e6) : -1;
      return { ok: true, size: size ?? bytes.length, mtimeMs };
    }
    return { ok: true, size: bytes.length, mtimeMs: -1 };
  } catch (e) {
    return { ok: false, error: String(e && e.message ? e.message : e) };
  }
}

// =============================================================================
// Off-tick daemon watch leg (Phase 6 — the `HostWatch` push direction).
//
// The fs leg (above) is request/response, always initiated by the Worker during a tick. The
// watch leg adds the *other* direction: the daemon pushes `fs_changed [path, stat?]` whenever
// a watched file changes on its disk, and the editor reconciles it (autoreload / W11 / W12 /
// `FileChangedShell`) — the browser twin of the native `watch_rx` arm. Two halves:
//   * outbound — the editor arms one watch per file-backed buffer off-tick (`fs_watch` /
//     `fs_unwatch` effects → `eh_take_watch_requests`); the Worker forwards each to the daemon.
//   * inbound — a `fs_changed` push arrives on `RpcClient.onNotify`; we queue it and apply it
//     on the run loop (`eh_remote_file_changed`), which may enqueue an off-tick reload the fs
//     leg then re-fetches over the wire.
//
// The crux is *receiving* a push: a thread parked in blocking `Atomics.wait` freezes the
// WebTransport reader, so an unsolicited push would sit until the next keystroke. So in a
// daemon session with watches armed the run loop parks on `Atomics.waitAsync` instead (it
// stays event-loop-live, the reader delivers pushes), waking immediately on a push via the
// `daemonWake()` race or on input via the SEQ notify. Serverless OPFS keeps the (cheaper)
// blocking park — it has no pushes. Pushes are queued and applied on the run loop only, so the
// wasm tick has a single consumer (no reentrancy with the loop's own drain).
// =============================================================================
const armedWatches = new Set(); // paths currently watched on the daemon (gates the async park)
const liveProcs = new Set(); // spawn ids in flight on the daemon (also gates the async park)
const daemonNotifications = []; // [method, params] pushes received but not yet applied
let daemonWaker = null; // resolve() to wake the async park the instant a push arrives
let pendingConnectUri = null; // a runtime `:connect nxvim://…` to dial on the next run-loop pass
const DAEMON_PUSH_POLL_MS = 1000; // async-park cap: clears a dangling waitAsync + backstops a push

// A daemon→edit-host push landed on the RPC reader. Queue it (the run loop applies it — single
// consumer of the wasm tick) and wake the loop. Under 5c (no run loop) apply it inline.
function onDaemonNotify(method, params) {
  daemonNotifications.push([method, params]);
  if (sabMode) {
    if (daemonWaker) { const r = daemonWaker; daemonWaker = null; r(); }
  } else {
    pump5cDaemon();
  }
}

// Apply every queued daemon push through the real tick (run-loop side): the watch leg's
// `fs_changed` and the proc leg's `proc_spawned`/`proc_exited` (Phase 6d). An unknown push is
// surfaced loudly (it can only arrive for something we subscribed to — fail loud, per CLAUDE.md).
function applyDaemonNotifications() {
  if (daemonNotifications.length === 0) return false;
  let any = false;
  for (let n; (n = daemonNotifications.shift()); ) {
    const [method, params] = n;
    if (method === "fs_changed") {
      // params = [path, stat?] where stat = [secs, nanos, size] | nil (nil = vanished).
      const path = params[0];
      const stat = params[1];
      const hasStat = Array.isArray(stat) ? 1 : 0;
      const size = hasStat ? (stat[2] ?? 0) : 0;
      const mtimeMs = hasStat ? (stat[0] ?? 0) * 1000 + Math.floor((stat[1] ?? 0) / 1e6) : -1;
      eh_remote_file_changed(h, String(path), hasStat, size, mtimeMs);
      any = true;
    } else if (method === "proc_spawned") {
      // params = [id, pid?] — nil pid = the child failed to spawn (passed as -1).
      const id = params[0];
      const pid = params[1];
      eh_proc_spawned(h, Number(id), pid == null ? -1 : Number(pid));
      any = true;
    } else if (method === "proc_exited") {
      // params = [id, code, stdout(bin), stderr(bin)] — a killed child arrives as code -1.
      const id = params[0];
      const code = params[1];
      callProcExited(Number(id), Number(code), toU8(params[2]), toU8(params[3]));
      liveProcs.delete(id);
      any = true;
    } else {
      postMessage({ type: "config_error", error: `unhandled daemon push: ${method}` });
    }
  }
  return any;
}

// Normalize a msgpack value to bytes: `bin` decodes to a Uint8Array, but guard against a
// `str`/array/nil shape too so a daemon quirk degrades to bytes rather than throwing.
function toU8(v) {
  if (v == null) return new Uint8Array(0);
  if (v instanceof Uint8Array) return v;
  if (ArrayBuffer.isView(v)) return new Uint8Array(v.buffer, v.byteOffset, v.byteLength);
  if (Array.isArray(v)) return new Uint8Array(v);
  if (typeof v === "string") return new TextEncoder().encode(v);
  return new Uint8Array(0);
}

// Run a child's `on_exit` with its raw stdout/stderr bytes. The bytes are copied into wasm
// memory (process output is binary — it can't ride the cwrap "string" marshalling, which
// stops at a NUL) and `eh_proc_exited` hands them to the Lua callback; free after the call.
function callProcExited(id, code, stdout, stderr) {
  const oPtr = stdout.length ? M._malloc(stdout.length) : 0;
  const ePtr = stderr.length ? M._malloc(stderr.length) : 0;
  if (oPtr) M.HEAPU8.set(stdout, oPtr);
  if (ePtr) M.HEAPU8.set(stderr, ePtr);
  eh_proc_exited(h, id, code, oPtr, stdout.length, ePtr, stderr.length);
  if (oPtr) M._free(oPtr);
  if (ePtr) M._free(ePtr);
}

// Forward the async process spawns/kills the tick enqueued to the daemon (`proc_spawn` /
// `proc_kill`). The daemon answers with `proc_spawned`/`proc_exited` pushes (`onDaemonNotify`).
// Only reached in a daemon session — the tick gates proc spawns on a connected daemon
// (`has_remote_proc`), so a serverless `vim.system` fails loud in the core and never enqueues.
async function drainProcRequests() {
  const reqs = JSON.parse(readStr(eh_take_proc_requests(h)));
  if (reqs.spawn.length === 0 && reqs.kill.length === 0) return;
  if (!daemon) return; // defensive: the tick shouldn't enqueue a spawn without a daemon
  for (const s of reqs.spawn) {
    liveProcs.add(s.id);
    await daemon.notify("proc_spawn", [s.id, s.argv, s.cwd ?? null, s.env, new Uint8Array(s.stdin)]);
  }
  for (const id of reqs.kill) await daemon.notify("proc_kill", [id]);
}

// Forward the watch arm/disarm requests the editor enqueued to the daemon (`fs_watch` /
// `fs_unwatch`). Serverless OPFS has no change source, so a watch is dropped — not a silent
// stub: the tab is the sole writer, there is genuinely nothing external to watch.
async function drainWatchRequests() {
  const reqs = JSON.parse(readStr(eh_take_watch_requests(h)));
  if (reqs.arm.length === 0 && reqs.disarm.length === 0) return;
  if (!daemon) return; // serverless OPFS — no remote to watch
  for (const path of reqs.arm) {
    armedWatches.add(path);
    await daemon.notify("fs_watch", [path]);
  }
  for (const path of reqs.disarm) {
    armedWatches.delete(path);
    await daemon.notify("fs_unwatch", [path]);
  }
}

// Forward any `:TSInstall <lang>` the tick enqueued to the UI thread, where the
// web-tree-sitter highlighter lives. Fire-and-forget: the UI fetches the prebuilt grammar
// (offline bundle / OPFS / jsDelivr), caches + registers it, and posts the outcome back as a
// `ts_install_result` (a ring type-6 frame under SAB, a message under 5c) which lands via
// `eh_ts_install_complete`. The "installing…" echo already painted with the `:TSInstall`
// keystroke, so no extra repaint is needed here. Returns whether any were sent.
function drainTsRequests() {
  const reqs = JSON.parse(readStr(eh_take_ts_requests(h)));
  for (const lang of reqs) postMessage({ type: "ts_install", lang });
  return reqs.length > 0;
}

// Forward any `"+`/`"*` yanks/deletes the tick enqueued to the UI thread, which writes the
// text to `navigator.clipboard` (a Worker has no clipboard access). Fire-and-forget, exactly
// like `drainTsRequests` — the editor already painted the yank, so no extra repaint here.
function drainClipboardWrites() {
  const writes = JSON.parse(readStr(eh_take_clipboard_writes(h)));
  for (const text of writes) postMessage({ type: "clipboard_write", text });
}

// 5c (postMessage, no run loop): apply a push + fulfil any reload re-fetch + repaint inline.
// Safe because 5c is never parked (no single-consumer run loop to race).
async function pump5cDaemon() {
  applyDaemonNotifications();
  await fulfillFsRequests();
  await drainProcRequests();
  postMessage({ type: "redraw", frame: currentFrame(), lines: readStr(eh_lines(h)) });
}

// Drain every off-tick fs request the editor enqueued, run its op (OPFS serverless, or the
// daemon over WebTransport when `?daemon=` is set), and land the result back into the tick.
// Loops until the queue is dry, because landing a read fires `BufReadPost` autocmds (and
// `run_pending`) that may enqueue further opens/saves. Returns whether any request was
// handled (so the caller posts a fresh redraw).
async function fulfillFsRequests() {
  let didWork = false;
  for (;;) {
    const reqs = JSON.parse(readStr(eh_take_fs_requests(h)));
    if (reqs.reads.length === 0 && reqs.writes.length === 0) return didWork;
    didWork = true;
    for (const r of reqs.reads) {
      // A picker-bound path's bytes live behind a UI-held handle — route the read there;
      // the `fs_read_result` reply lands it (`landRealFsReply`). Else fulfill from OPFS.
      if (boundPaths.has(r.path)) {
        dispatchRealFsRead(r.buffer, r.path);
        continue;
      }
      const res = daemonUri ? await daemonRead(r.path) : await opfsRead(r.path);
      // A directory read carries its canonical dir in res.path (the explorer navigates
      // from it); a file/new/err keeps the requested path.
      eh_fs_read_complete(h, r.buffer, res.path ?? r.path, res.kind, res.text);
    }
    for (const w of reqs.writes) {
      // A picker-bound path is written by the UI against its handle; route it there and
      // leave the save in-flight until the `fs_write_result` reply finalizes it.
      if (boundPaths.has(w.path)) {
        dispatchRealFsWrite(w.seq, w.path);
        continue;
      }
      // Copy the snapshot bytes out of wasm memory *before* the await — ALLOW_MEMORY_GROWTH
      // can detach HEAPU8 across a later wasm call, and the pointer is only valid until
      // `eh_fs_write_complete` drops the save.
      const ptr = eh_save_bytes(h, w.seq);
      const len = eh_save_len(h, w.seq);
      const bytes = ptr ? new Uint8Array(M.HEAPU8.subarray(ptr, ptr + len)) : new Uint8Array(0);
      const res = daemonUri ? await daemonWrite(w.path, bytes) : await opfsWrite(w.path, bytes);
      // mtime_ms: OPFS has no watch leg (so -1 is fine); the daemon returns a real stat
      // (`res.mtimeMs`) the editor stamps as its disk baseline.
      eh_fs_write_complete(h, w.seq, res.ok ? 1 : 0, res.ok ? res.size : 0, res.mtimeMs ?? -1, res.ok ? "" : res.error);
    }
  }
}

// =============================================================================
// Persistence (shada) — serverless OPFS.
//
// The editor's cross-session state (registers, marks, search/ex history, jumplist,
// changelist) is the pure snapshot the core hands out; we serialize it to a single JSON
// blob at SHADA_PATH in OPFS and restore it at boot (`bootWithConfig`). This is the
// browser analogue of the native redb store — minus the multi-instance merge a single tab
// doesn't need. Durability is the *debounced checkpoint* (write after a quiet period),
// exactly as native treats it as the primary mechanism; a flush-with-exit-cursor on tab
// hide seeds `'0` for next launch (best-effort — the debounce already captured the rest).
// =============================================================================
const SHADA_PATH = "/.nxvim/shada";
const SHADA_DEBOUNCE_MS = 1200;
let shadaBaseline = null; // last-written no-exit JSON, for change detection (skip no-op writes)
let shadaDirty = false; // input arrived since the last checkpoint
let shadaDueMs = 0; // earliest time the debounced checkpoint may fire
let shadaFlushRequested = false; // a flush-with-exit was asked for (tab hidden / test hook)

// Write the shada snapshot to OPFS when it changed (or `force`). The no-exit form is the
// change-detection baseline (pure cursor moves don't churn it); `includeExit` additionally
// persists the clean-exit cursor so `'0` seeds next launch. Posts `shada_written` on a real
// write (the UI's flush hook awaits it). Returns whether it wrote.
async function checkpointShada(includeExit, force) {
  shadaDirty = false;
  const canonical = readStr(eh_export_shada(h, 0));
  if (!canonical) return false; // serialization failed — write nothing rather than a bad blob
  if (!force && canonical === shadaBaseline) return false;
  shadaBaseline = canonical;
  const json = includeExit ? readStr(eh_export_shada(h, 1)) : canonical;
  const res = await opfsWrite(SHADA_PATH, new TextEncoder().encode(json));
  if (res.ok) postMessage({ type: "shada_written", bytes: json.length });
  return res.ok;
}

// 5c-only throttled checkpoint: the postMessage path has no run loop to debounce against,
// so write at most once per SHADA_DEBOUNCE_MS of activity (leading-edge). The UI's hide
// flush covers the trailing gap. (Under SAB the run loop does the proper debounce instead.)
let shada5cLast = 0;
async function maybeCheckpoint5c() {
  const now = nowMs();
  if (now - shada5cLast < SHADA_DEBOUNCE_MS) return;
  shada5cLast = now;
  await checkpointShada(false, false);
}

// =============================================================================
// Slice 5d — the SharedArrayBuffer run loop.
//
// ctrl: Int32Array(4) — [SEQ, WRITE, READ, _]. SEQ is the wake counter the UI bumps +
//   notifies; WRITE/READ are monotonic byte cursors (mod 2^32) into the data ring.
// data: Uint8Array ring. Frames are [type:u8][reqId:u32][len:u32][payload:len]:
//   type 0 = feed (payload = vim notation), 1 = exec_lua (payload = code),
//   2 = attach (payload = cols:u32, rows:u32),
//   3 = mouse (payload = JSON {b:button, a:action, m:modifier, r:row, c:col}).
// =============================================================================
const SEQ = 0, WRITE = 1, READ = 2;

async function runLoopSAB(ctrl, data) {
  sabMode = true;
  const cap = data.length;
  const rdByte = (pos) => data[((pos % cap) + cap) % cap];
  const rdU32 = (pos) =>
    (rdByte(pos) | (rdByte(pos + 1) << 8) | (rdByte(pos + 2) << 16) | (rdByte(pos + 3) << 24)) >>> 0;

  // Drain every queued frame (READ → WRITE), apply it, and return the reqIds processed
  // plus any exec_lua results, so the UI can resolve the matching promises.
  function drain() {
    const acks = [];
    const results = [];
    let tsLanded = false;
    let rp = Atomics.load(ctrl, READ) >>> 0;
    const wp = Atomics.load(ctrl, WRITE) >>> 0;
    while (rp !== wp) {
      const type = rdByte(rp);
      const reqId = rdU32(rp + 1);
      const len = rdU32(rp + 5);
      const payload = new Uint8Array(len);
      for (let i = 0; i < len; i++) payload[i] = rdByte(rp + 9 + i);
      rp = (rp + 9 + len) >>> 0;
      if (type === 0) {
        eh_input(h, utf8(payload));
      } else if (type === 1) {
        results.push([reqId, readStr(eh_exec_lua(h, utf8(payload)))]);
      } else if (type === 2) {
        eh_attach(h, rdU32Bytes(payload, 0), rdU32Bytes(payload, 4));
      } else if (type === 3) {
        // The clock was set to now before this drain, so the mouse stamp drives
        // 'mousetime' multi-click detection off the same JS clock as input.
        const m = JSON.parse(utf8(payload));
        eh_input_mouse(h, m.b, m.a, m.m, m.r | 0, m.c | 0);
      } else if (type === 4) {
        // bind: the path resolves to a real-FS handle the UI holds. Sent *before* the
        // `:e`/`:w` that references it, so the routing decision is set when the request
        // drains. (Bulk content never rides the ring — it's small enough to overflow it —
        // so only this marker does; the file bytes flow over postMessage.)
        boundPaths.add(utf8(payload));
      } else if (type === 5) {
        // shada_flush: persist cross-session state now, with the exit cursor (tab hidden).
        shadaFlushRequested = true;
      } else if (type === 6) {
        // ts_install_result: the UI finished a `:TSInstall` ({lang, ok, msg} JSON). Land the
        // outcome (echo + record installed). Not a user request, so it earns a repaint but
        // no ack / shada churn — skip the `acks.push` below.
        const r = JSON.parse(utf8(payload));
        eh_ts_install_complete(h, String(r.lang), r.ok ? 1 : 0, String(r.msg || ""));
        tsLanded = true;
        continue;
      } else if (type === 7) {
        // connect: a runtime `:connect nxvim://…`. Record the URI; the run loop dials it
        // after this drain (the dial is async, so it can't run here in the sync drain).
        pendingConnectUri = utf8(payload);
      } else if (type === 8) {
        // clipboard_push: the UI read `navigator.clipboard` (on focus / paste) and handed us
        // its text — update the mirror a `"+`/`"*` paste reads. Not a user keystroke, so it
        // earns neither an ack nor a repaint (the cache update is invisible until a paste).
        eh_clipboard_push(h, utf8(payload));
        continue;
      }
      acks.push(reqId);
    }
    Atomics.store(ctrl, READ, rp);
    return { acks, results, tsLanded };
  }

  // Initial paint (the attach the UI requested rode in as the first frame, but paint a
  // baseline regardless so the UI has a frame even before any input).
  postMessage({ type: "redraw", frame: currentFrame(), lines: readStr(eh_lines(h)), acks: [], results: [] });

  for (;;) {
    const seqBefore = Atomics.load(ctrl, SEQ);
    const fired = eh_tick_timers(h, nowMs()) === 1;
    // Set the clock to *now* before draining input, so a timer armed by this batch's
    // `vim.defer_fn` / `nx.timer` (feed or exec_lua) computes its deadline from now —
    // not a stale clock that would make it instantly due.
    eh_set_clock(h, nowMs());
    const { acks, results, tsLanded } = drain();
    // A runtime `:connect nxvim://…` arrived this pass (`<Esc>` already dismissed the command
    // line in the drain above): dial the daemon now, while the loop is event-loop-live — a
    // blocking `Atomics.wait` park would freeze the WebTransport handshake. This re-points the
    // off-tick fs/watch seam onto the wire; re-loop so subsequent input hits the new backend.
    if (pendingConnectUri !== null) {
      const uri = pendingConnectUri;
      pendingConnectUri = null;
      await runtimeConnect(uri);
      postMessage({ type: "redraw", frame: currentFrame(), lines: readStr(eh_lines(h)), acks, results });
      continue;
    }
    // Apply any daemon pushes received since the last pass (the watch leg's `fs_changed`):
    // a reconcile may enqueue an off-tick reload, which `fulfillFsRequests` then re-fetches
    // over the wire in this same pass. Done before `fulfillFsRequests` so the reload lands now.
    const notified = applyDaemonNotifications();
    // Fulfill any `:e`/`:w` the tick (or a fired timer / a push reconcile) deferred against
    // OPFS or the daemon. We're not parked here, so the event loop runs and the promises
    // resolve; the completions land the buffer/save back into the tick before we post the ack,
    // so the UI sees the opened/saved frame when its `feed` promise resolves.
    const fsWork = await fulfillFsRequests();
    // Forward any watches the tick armed/disarmed (file-backed buffers opened/closed) to the
    // daemon, so it begins/stops pushing `fs_changed` for them.
    await drainWatchRequests();
    // Forward any async `vim.system` / `jobstart` the tick enqueued to the daemon (its
    // `proc_spawned`/`proc_exited` pushes return on `onDaemonNotify`).
    await drainProcRequests();
    // Forward any `:TSInstall` the tick enqueued to the UI thread (fire-and-forget).
    drainTsRequests();
    // Forward any `"+`/`"*` yanks/deletes to the UI thread to write to navigator.clipboard.
    drainClipboardWrites();
    if (fired || acks.length || fsWork || notified || tsLanded) {
      postMessage({ type: "redraw", frame: currentFrame(), lines: readStr(eh_lines(h)), acks, results });
    }
    // Persistence (shada): any input this pass arms the debounced checkpoint; a requested
    // flush (tab hidden) writes immediately with the exit cursor. The checkpoint write is
    // async (OPFS) — awaiting it here is fine (the thread isn't parked yet).
    if (acks.length || results.length || fsWork) {
      shadaDirty = true;
      shadaDueMs = nowMs() + SHADA_DEBOUNCE_MS;
    }
    if (shadaFlushRequested) {
      shadaFlushRequested = false;
      await checkpointShada(true, true);
    } else if (shadaDirty && nowMs() >= shadaDueMs) {
      await checkpointShada(false, false);
    }
    // A picker-bound `:e`/`:w` dispatched a read/write to the UI this pass. Don't park on
    // `Atomics.wait` — a parked thread can't receive the UI's reply postMessage. Instead
    // stay in the event loop awaiting the reply (`onmessage` → `landRealFsReply` resolves
    // it), repaint with the landed result, then loop to drain any cascade + any input that
    // arrived meanwhile (it's queued in the ring; the SEQ notify was a no-op while unparked,
    // but the next `drain()` picks it up).
    if (pendingRealFs > 0) {
      await new Promise((res) => {
        fsReplyWaiter = res;
      });
      postMessage({ type: "redraw", frame: currentFrame(), lines: readStr(eh_lines(h)), acks: [], results: [] });
      continue;
    }
    // A push that arrived during this pass (e.g. while awaiting `fulfillFsRequests`) is
    // queued — re-process it next iteration rather than parking on a now-stale wait.
    if (daemonNotifications.length > 0) continue;
    // Park until the UI notifies (SEQ changes) or the next timer is due. If input
    // arrived while we processed (SEQ already moved off seqBefore), the wait
    // returns "not-equal" at once — no missed wakeup.
    let deadline = eh_next_deadline(h);
    // Fold the shada debounce deadline into the same park, so the one wait that wakes on a
    // keystroke / timer also wakes to flush cross-session state after a quiet period.
    if (shadaDirty) deadline = deadline < 0 ? shadaDueMs : Math.min(deadline, shadaDueMs);
    let timeout = deadline < 0 ? undefined : Math.max(0, deadline - nowMs());
    if (daemon && (armedWatches.size > 0 || liveProcs.size > 0)) {
      // A daemon session expecting pushes — a watch armed, or a child in flight — must not
      // *block*: a thread frozen in `Atomics.wait` can't run the WebTransport reader, so a
      // `fs_changed` / `proc_exited` push would sit until the next keystroke. Park on the
      // non-blocking `Atomics.waitAsync` so the event loop (hence the reader) keeps running;
      // wake the instant a push lands (`daemonWake`) or input arrives (SEQ notify resolves
      // `w.value`), capped so a dangling wait clears.
      timeout = timeout === undefined ? DAEMON_PUSH_POLL_MS : Math.min(timeout, DAEMON_PUSH_POLL_MS);
      const w = Atomics.waitAsync(ctrl, SEQ, seqBefore, timeout);
      if (w.async) await Promise.race([w.value, new Promise((res) => { daemonWaker = res; })]);
      // (`!w.async` ⇒ SEQ already moved / timeout 0 — loop immediately, no missed wakeup.)
    } else {
      // Serverless OPFS (or daemon with no watches): no pushes to receive, so block — it's
      // cheaper. Blocking inside an async function is fine: the `await`s above fully settled,
      // so nothing is pending on the microtask queue while the thread is parked.
      Atomics.wait(ctrl, SEQ, seqBefore, timeout);
    }
  }
}

function rdU32Bytes(arr, off) {
  return (arr[off] | (arr[off + 1] << 8) | (arr[off + 2] << 16) | (arr[off + 3] << 24)) >>> 0;
}

// =============================================================================
// Slice 5c — the postMessage fallback (no SAB / not cross-origin isolated).
// =============================================================================
function postFrame(id, result) {
  postMessage({ type: "redraw", id, result, frame: currentFrame(), lines: readStr(eh_lines(h)) });
}

let started = false;
onmessage = async (ev) => {
  const msg = ev.data;
  if (msg.type === "init") {
    // SAB handover (5d): enter the blocking run loop and never return — all further
    // input arrives over the SAB, redraws post out (posting out is never blocked).
    if (started) return;
    started = true;
    eh_attach(h, msg.cols | 0, msg.rows | 0);
    runLoopSAB(new Int32Array(msg.ctrl), new Uint8Array(msg.data)).catch((e) =>
      postMessage({ type: "fatal", error: `SAB run loop failed: ${e}` }),
    );
    return;
  }
  // Real-FS picker plumbing (both transports). The UI fulfills a bound path's read/write
  // against its `FileSystemFileHandle` and reports back here; land it into the tick. Under
  // SAB these are received while the run loop is event-loop-live (pendingRealFs > 0); under
  // 5c the Worker isn't parked, so they're received normally.
  if (msg.type === "fs_read_result") {
    eh_fs_read_complete(h, msg.buffer, String(msg.path), msg.kind | 0, String(msg.text ?? ""));
    await landRealFsReply();
    return;
  }
  if (msg.type === "fs_write_result") {
    eh_fs_write_complete(
      h,
      msg.seq,
      msg.ok ? 1 : 0,
      msg.ok ? msg.size : 0,
      -1,
      msg.ok ? "" : String(msg.error ?? "write failed"),
    );
    await landRealFsReply();
    return;
  }
  // 5c bind (the SAB transport binds via a ring frame in `drain()` instead). Marks a path
  // as resolving to a real handle; must precede the `:e`/`:w` the UI sends after it.
  if (msg.type === "bind") {
    boundPaths.add(String(msg.path));
    return;
  }
  // Shada flush (SAB requests it via a ring frame). Persist now, with the exit cursor.
  if (msg.type === "shada_flush") {
    await checkpointShada(true, true);
    return;
  }
  // `:TSInstall` outcome from the UI (SAB sends it as a ring type-6 frame instead). Land
  // the echo + record, then repaint so the status line shows it.
  if (msg.type === "ts_install_result") {
    eh_ts_install_complete(h, String(msg.lang), msg.ok ? 1 : 0, String(msg.msg ?? ""));
    postMessage({ type: "redraw", frame: currentFrame(), lines: readStr(eh_lines(h)) });
    return;
  }
  // Runtime `:connect nxvim://…` (5c; SAB routes it via a ring frame in `drain()`). Dial the
  // daemon and re-point the off-tick fs/watch seam onto the wire, then repaint.
  if (msg.type === "connect") {
    await runtimeConnect(String(msg.uri));
    postMessage({ type: "redraw", frame: currentFrame(), lines: readStr(eh_lines(h)) });
    return;
  }
  // Clipboard mirror push (5c; SAB routes it via a ring type-8 frame in `drain()`). The UI
  // read `navigator.clipboard` and handed us its text — update what a `"+`/`"*` paste reads.
  // No repaint: the cache change is invisible until a paste consumes it.
  if (msg.type === "clipboard_push") {
    eh_clipboard_push(h, String(msg.text ?? ""));
    return;
  }
  // postMessage fallback (5c). `:e`/`:w` fulfill against OPFS before the frame posts, so
  // the resolved frame reflects the open/save (the SAB loop does the same inline).
  switch (msg.type) {
    case "attach":
      eh_attach(h, msg.cols | 0, msg.rows | 0);
      postFrame(msg.id);
      break;
    case "feed":
      eh_input(h, String(msg.notation));
      await fulfillFsRequests();
      await drainWatchRequests();
      await drainProcRequests();
      drainTsRequests();
      drainClipboardWrites();
      postFrame(msg.id);
      await maybeCheckpoint5c();
      break;
    case "input_mouse":
      // Stamp from the JS clock so multi-click timing matches the keystroke tick.
      eh_set_clock(h, nowMs());
      eh_input_mouse(h, String(msg.button), String(msg.action), String(msg.modifier), msg.row | 0, msg.col | 0);
      await fulfillFsRequests();
      await drainWatchRequests();
      await drainProcRequests();
      drainTsRequests();
      drainClipboardWrites();
      postFrame(msg.id);
      await maybeCheckpoint5c();
      break;
    case "exec_lua": {
      const result = readStr(eh_exec_lua(h, String(msg.code)));
      await fulfillFsRequests();
      await drainWatchRequests();
      await drainProcRequests();
      drainTsRequests();
      drainClipboardWrites();
      postFrame(msg.id, result);
      await maybeCheckpoint5c();
      break;
    }
    default:
      postMessage({ type: "fatal", error: `unknown worker message: ${msg.type}` });
  }
};

// Connect the daemon (if `?daemon=` was passed) *before* boot finishes, so the link is
// live for the first user `:e` — and a dial failure surfaces before "ready". A no-op in
// serverless (OPFS) mode.
await connectDaemon();

// Source the optional /init.lua + finish boot before announcing "ready", so the config
// is fully applied before the UI attaches and the first frame paints. Run here (module
// tail) so the `const` OPFS helpers it reaches are past their temporal dead zone.
await bootWithConfig();

postMessage({ type: "ready" });
