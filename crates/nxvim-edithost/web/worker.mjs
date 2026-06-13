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
const eh_attach = M.cwrap("eh_attach", null, ["number", "number", "number"]);
const eh_set_clock = M.cwrap("eh_set_clock", null, ["number", "number"]);
const eh_next_deadline = M.cwrap("eh_next_deadline", "number", ["number"]);
const eh_tick_timers = M.cwrap("eh_tick_timers", "number", ["number", "number"]);
const eh_exec_lua = M.cwrap("eh_exec_lua", "number", ["number", "string"]);
const eh_redraw_json = M.cwrap("eh_redraw_json", "number", ["number"]);
const eh_lines = M.cwrap("eh_lines", "number", ["number"]);
const eh_free_string = M.cwrap("eh_free_string", null, ["number"]);

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
// Slice 5d — the SharedArrayBuffer run loop.
//
// ctrl: Int32Array(4) — [SEQ, WRITE, READ, _]. SEQ is the wake counter the UI bumps +
//   notifies; WRITE/READ are monotonic byte cursors (mod 2^32) into the data ring.
// data: Uint8Array ring. Frames are [type:u8][reqId:u32][len:u32][payload:len]:
//   type 0 = feed (payload = vim notation), 1 = exec_lua (payload = code),
//   2 = attach (payload = cols:u32, rows:u32).
// =============================================================================
const SEQ = 0, WRITE = 1, READ = 2;

function runLoopSAB(ctrl, data) {
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
    if (fired || acks.length) {
      postMessage({ type: "redraw", frame: currentFrame(), lines: readStr(eh_lines(h)), acks, results });
    }
    // Park until the UI notifies (SEQ changes) or the next timer is due. If input
    // arrived while we processed (SEQ already moved off seqBefore), `Atomics.wait`
    // returns "not-equal" at once — no missed wakeup.
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
onmessage = (ev) => {
  const msg = ev.data;
  if (msg.type === "init") {
    // SAB handover (5d): enter the blocking run loop and never return — all further
    // input arrives over the SAB, redraws post out (posting out is never blocked).
    if (started) return;
    started = true;
    eh_attach(h, msg.cols | 0, msg.rows | 0);
    runLoopSAB(new Int32Array(msg.ctrl), new Uint8Array(msg.data));
    return;
  }
  // postMessage fallback (5c).
  switch (msg.type) {
    case "attach":
      eh_attach(h, msg.cols | 0, msg.rows | 0);
      postFrame(msg.id);
      break;
    case "feed":
      eh_input(h, String(msg.notation));
      postFrame(msg.id);
      break;
    case "exec_lua":
      postFrame(msg.id, readStr(eh_exec_lua(h, String(msg.code))));
      break;
    default:
      postMessage({ type: "fatal", error: `unknown worker message: ${msg.type}` });
  }
};

postMessage({ type: "ready" });
