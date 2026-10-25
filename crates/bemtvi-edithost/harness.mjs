// Headless proof of slice 5b: load the emscripten edit-host module, feed vim keys
// through the REAL EditHost tick, and read back (1) the buffer lines and (2) a real
// `redraw` frame projected by the server's view projection — the demo's `eh_lines`
// proof upgraded to a redraw through the production tick. Exits non-zero on any failed
// assertion so it can gate a manual check / CI.
import createModule from "./dist/eh.mjs";

const M = await createModule();

const eh_new = M.cwrap("eh_new", "number", []);
const eh_boot_finish = M.cwrap("eh_boot_finish", null, ["number"]);
const eh_input = M.cwrap("eh_input", null, ["number", "string"]);
const eh_exec_lua = M.cwrap("eh_exec_lua", "number", ["number", "string"]);
const eh_redraw_json = M.cwrap("eh_redraw_json", "number", ["number"]);
const eh_lines = M.cwrap("eh_lines", "number", ["number"]);
const eh_free_string = M.cwrap("eh_free_string", null, ["number"]);
const eh_free = M.cwrap("eh_free", null, ["number"]);
const eh_set_clock = M.cwrap("eh_set_clock", null, ["number", "number"]);
const eh_next_deadline = M.cwrap("eh_next_deadline", "number", ["number"]);
const eh_tick_timers = M.cwrap("eh_tick_timers", "number", ["number", "number"]);

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
// `eh_new` only runs the first half of boot (so a browser can source `init.lua` from
// OPFS in between); this node smoke test has no config, so finish boot straight away.
eh_boot_finish(h);

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

// 4. `:sleep` parks the editor on the JS clock, as natively: keys typed meanwhile are
//    held (not applied, not lost) and replayed when the Worker wakes at its deadline;
//    `<C-c>` ends it early.
eh_set_clock(h, 1000);
eh_input(h, "<Esc>:sleep 500m<CR>");
eh_set_clock(h, 1100);
eh_input(h, "A!<Esc>");
let held = readStr(eh_lines(h));
check(":sleep holds input while it runs", held === "world", `got ${JSON.stringify(held)}`);
check(":sleep's end is the Worker's next wake", eh_next_deadline(h) === 1500, `got ${eh_next_deadline(h)}`);
eh_tick_timers(h, 1400);
held = readStr(eh_lines(h));
check(":sleep still holds before its end", held === "world", `got ${JSON.stringify(held)}`);
eh_tick_timers(h, 1500);
held = readStr(eh_lines(h));
check(":sleep replays held input when it ends", held === "world!", `got ${JSON.stringify(held)}`);
eh_set_clock(h, 2000);
eh_input(h, ":sleep 60<CR>");
eh_set_clock(h, 2010);
eh_input(h, "A?<Esc>");
eh_input(h, "<C-c>");
held = readStr(eh_lines(h));
check("<C-c> ends a :sleep early and replays what it held", held === "world!?", `got ${JSON.stringify(held)}`);

eh_free(h);

console.log(failures === 0 ? "\nALL PASS — real EditHost tick + redraw validated in wasm" : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
