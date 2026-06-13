// The edit-host Web Worker (Phase 5, slice 5c). This is the single `!Send` thread that
// owns nxvim's core + the PUC Lua 5.1 VM: it loads the emscripten module (`dist/eh.mjs`),
// constructs the real `EditHost` (`eh_new`), and drives the production tick through the
// `extern "C"` FFI. Input arrives from the UI thread as `postMessage`s; after each tick
// the worker reads the latest `redraw` frame (the real server view projection) plus the
// buffer lines back out and posts them UI-ward to render. The UI thread never touches
// editor/Lua state — it only ferries input and renders frames, mapping nxvim's
// single-threaded model onto the browser exactly as the native edit-host owns its own OS
// thread.
//
// Slice 5c transport is `postMessage` (request/response, correlated by `id`). Slice 5d
// replaces the UI→worker leg with a `SharedArrayBuffer` + `Atomics.wait` park so the same
// wait that blocks on input also fires Worker-side timers (`vim.defer_fn` / `nx.timer`).
import createModule from "../dist/eh.mjs";

const M = await createModule();

// emscripten ccall/cwrap over the exports (mirrors harness.mjs).
const eh_new = M.cwrap("eh_new", "number", []);
const eh_input = M.cwrap("eh_input", null, ["number", "string"]);
const eh_attach = M.cwrap("eh_attach", null, ["number", "number", "number"]);
const eh_exec_lua = M.cwrap("eh_exec_lua", "number", ["number", "string"]);
const eh_redraw_json = M.cwrap("eh_redraw_json", "number", ["number"]);
const eh_lines = M.cwrap("eh_lines", "number", ["number"]);
const eh_free_string = M.cwrap("eh_free_string", null, ["number"]);

// Read an owned char* back as a JS string, then free it Rust-side (the harness pattern).
function readStr(ptr) {
  const s = M.UTF8ToString(ptr);
  eh_free_string(ptr);
  return s;
}

const h = eh_new();
if (h === 0) {
  // Fail loud — the Lua VM could not initialize in wasm (no silent stub).
  postMessage({ type: "fatal", error: "eh_new returned null (Lua VM failed to init in wasm)" });
  throw new Error("eh_new returned null");
}

// Read the current frame + lines off the host and ship them to the UI. Called after
// every tick; `id`/`result` ride along so a UI request can await its own reply.
function postFrame(id, result) {
  const frameJson = readStr(eh_redraw_json(h));
  const lines = readStr(eh_lines(h));
  let frame = null;
  try {
    const parsed = JSON.parse(frameJson);
    // `eh_redraw_json` returns the `redraw` notification's params array `[viewMap]`
    // (or "null" before the first frame). The renderable frame is the single view map.
    frame = Array.isArray(parsed) ? (parsed[0] ?? null) : parsed;
  } catch (e) {
    postMessage({ type: "fatal", error: `redraw JSON parse failed: ${e}` });
    return;
  }
  postMessage({ type: "redraw", id, result, frame, lines });
}

onmessage = (ev) => {
  const msg = ev.data;
  switch (msg.type) {
    case "attach":
      eh_attach(h, msg.cols | 0, msg.rows | 0);
      postFrame(msg.id);
      break;
    case "feed":
      eh_input(h, String(msg.notation));
      postFrame(msg.id);
      break;
    case "exec_lua": {
      const result = readStr(eh_exec_lua(h, String(msg.code)));
      postFrame(msg.id, result);
      break;
    }
    default:
      postMessage({ type: "fatal", error: `unknown worker message: ${msg.type}` });
  }
};

// Tell the UI the host is up; it will attach at the real grid size and render.
postMessage({ type: "ready" });
