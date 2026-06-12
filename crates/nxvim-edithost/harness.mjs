// Headless proof of slice 5b: load the emscripten edit-host module, feed vim keys
// through the REAL EditHost tick, and read back (1) the buffer lines and (2) a real
// `redraw` frame projected by the server's view projection — the demo's `eh_lines`
// proof upgraded to a redraw through the production tick. Exits non-zero on any failed
// assertion so it can gate a manual check / CI.
import createModule from "./dist/eh.mjs";

const M = await createModule();

const eh_new = M.cwrap("eh_new", "number", []);
const eh_input = M.cwrap("eh_input", null, ["number", "string"]);
const eh_exec_lua = M.cwrap("eh_exec_lua", "number", ["number", "string"]);
const eh_redraw_json = M.cwrap("eh_redraw_json", "number", ["number"]);
const eh_lines = M.cwrap("eh_lines", "number", ["number"]);
const eh_free_string = M.cwrap("eh_free_string", null, ["number"]);
const eh_free = M.cwrap("eh_free", null, ["number"]);

// Read an owned char* back as a JS string, then free it Rust-side.
function readStr(ptr) {
  const s = M.UTF8ToString(ptr);
  eh_free_string(ptr);
  return s;
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) {
    if (detail !== undefined) console.log(`        ${detail}`);
    failures++;
  }
}

const h = eh_new();
if (h === 0) {
  console.error("FATAL: eh_new returned null (Lua VM failed to init in wasm)");
  process.exit(1);
}

// 1. The real tick runs in wasm: insert text via vim keys, read it back.
eh_input(h, "ihello<Esc>");
const lines = readStr(eh_lines(h));
check("editor: insert via vim keys → lines", lines === "hello", `got ${JSON.stringify(lines)}`);

// 2. The REAL redraw projection runs through the tick: the latest `redraw` frame is a
//    server view map (not the demo's raw lines), and its grid shows "hello".
const frame = JSON.parse(readStr(eh_redraw_json(h)));
// The frame is the `redraw` notification's params array: [ <view-map> ]. The view map
// carries a `windows` array; each window has the rendered text rows. Search the whole
// frame for a row whose text is "hello" rather than hard-coding the (evolving) shape.
function gridText(node, out) {
  if (typeof node === "string") { out.push(node); return; }
  if (Array.isArray(node)) { for (const x of node) gridText(x, out); return; }
  if (node && typeof node === "object") { for (const k of Object.keys(node)) gridText(node[k], out); }
}
const strings = [];
gridText(frame, strings);
const showsHello = strings.some((s) => s.includes("hello"));
check(
  "redraw: real view projection grid shows 'hello'",
  frame !== null && showsHello,
  `frame strings: ${JSON.stringify(strings.slice(0, 12))}`,
);

// 3. Lua drives the editor through the real effects path: a queued :-command mutates
//    the buffer, exactly as a :lua from the keystroke tick would.
readStr(eh_exec_lua(h, 'vim.cmd("%s/hello/world/")'));
const after = readStr(eh_lines(h));
check("lua → editor: :substitute via real effects", after === "world", `got ${JSON.stringify(after)}`);

eh_free(h);

console.log(failures === 0 ? "\nALL PASS — real EditHost tick + redraw validated in wasm" : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
