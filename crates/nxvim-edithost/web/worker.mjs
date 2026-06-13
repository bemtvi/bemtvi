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

// Drain every off-tick fs request the editor enqueued, run its OPFS op, and land the
// result back into the tick. Loops until the queue is dry, because landing a read fires
// `BufReadPost` autocmds (and `run_pending`) that may enqueue further opens/saves.
// Returns whether any request was handled (so the caller posts a fresh redraw).
async function fulfillFsRequests() {
  let didWork = false;
  for (;;) {
    const reqs = JSON.parse(readStr(eh_take_fs_requests(h)));
    if (reqs.reads.length === 0 && reqs.writes.length === 0) return didWork;
    didWork = true;
    for (const r of reqs.reads) {
      const res = await opfsRead(r.path);
      // A directory read carries its canonical dir in res.path (the explorer navigates
      // from it); a file/new/err keeps the requested path.
      eh_fs_read_complete(h, r.buffer, res.path ?? r.path, res.kind, res.text);
    }
    for (const w of reqs.writes) {
      // Copy the snapshot bytes out of wasm memory *before* the await — ALLOW_MEMORY_GROWTH
      // can detach HEAPU8 across a later wasm call, and the pointer is only valid until
      // `eh_fs_write_complete` drops the save.
      const ptr = eh_save_bytes(h, w.seq);
      const len = eh_save_len(h, w.seq);
      const bytes = ptr ? new Uint8Array(M.HEAPU8.subarray(ptr, ptr + len)) : new Uint8Array(0);
      const res = await opfsWrite(w.path, bytes);
      // mtime_ms = -1: OPFS sync handles don't expose mtime and there's no watch leg, so
      // the disk baseline doesn't need it.
      eh_fs_write_complete(h, w.seq, res.ok ? 1 : 0, res.ok ? res.size : 0, -1, res.ok ? "" : res.error);
    }
  }
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
  const cap = data.length;
  const rdByte = (pos) => data[((pos % cap) + cap) % cap];
  const rdU32 = (pos) =>
    (rdByte(pos) | (rdByte(pos + 1) << 8) | (rdByte(pos + 2) << 16) | (rdByte(pos + 3) << 24)) >>> 0;

  // Drain every queued frame (READ → WRITE), apply it, and return the reqIds processed
  // plus any exec_lua results, so the UI can resolve the matching promises.
  function drain() {
    const acks = [];
    const results = [];
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
      }
      acks.push(reqId);
    }
    Atomics.store(ctrl, READ, rp);
    return { acks, results };
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
    const { acks, results } = drain();
    // Fulfill any `:e`/`:w` the tick (or a fired timer) deferred against OPFS. We're not
    // parked here, so the event loop runs and the OPFS promises resolve; the completions
    // land the buffer/save back into the tick before we post the ack, so the UI sees the
    // opened/saved frame when its `feed` promise resolves.
    const fsWork = await fulfillFsRequests();
    if (fired || acks.length || fsWork) {
      postMessage({ type: "redraw", frame: currentFrame(), lines: readStr(eh_lines(h)), acks, results });
    }
    // Park until the UI notifies (SEQ changes) or the next timer is due. If input
    // arrived while we processed (SEQ already moved off seqBefore), `Atomics.wait`
    // returns "not-equal" at once — no missed wakeup. Blocking inside an async function
    // is fine: the `await` above fully settled before we reach here, so nothing is
    // pending on the microtask queue while the thread is parked.
    const deadline = eh_next_deadline(h);
    const timeout = deadline < 0 ? undefined : Math.max(0, deadline - nowMs());
    Atomics.wait(ctrl, SEQ, seqBefore, timeout);
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
      postFrame(msg.id);
      break;
    case "input_mouse":
      // Stamp from the JS clock so multi-click timing matches the keystroke tick.
      eh_set_clock(h, nowMs());
      eh_input_mouse(h, String(msg.button), String(msg.action), String(msg.modifier), msg.row | 0, msg.col | 0);
      await fulfillFsRequests();
      postFrame(msg.id);
      break;
    case "exec_lua": {
      const result = readStr(eh_exec_lua(h, String(msg.code)));
      await fulfillFsRequests();
      postFrame(msg.id, result);
      break;
    }
    default:
      postMessage({ type: "fatal", error: `unknown worker message: ${msg.type}` });
  }
};

// Source the optional /init.lua + finish boot before announcing "ready", so the config
// is fully applied before the UI attaches and the first frame paints. Run here (module
// tail) so the `const` OPFS helpers it reaches are past their temporal dead zone.
await bootWithConfig();

postMessage({ type: "ready" });
