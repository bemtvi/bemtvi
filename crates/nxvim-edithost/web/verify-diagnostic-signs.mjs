// Repro/regression for the bug "web editor doesn't show the diagnostics gutter":
// on the wasm build the server projected an all-`Nil` `diagnostics_signs` payload
// (the LSP diagnostic-sign merge was gated `#[cfg(feature = "native")]`), so the
// gutter sign column never carried the E/W/I/H glyph. Client-set diagnostics
// (`nx.diagnostic.set`) run the same render store on every build, so this drives one
// through the REAL EditHost tick in wasm and asserts the diagnostic sign reaches the
// `redraw` frame the web renderer paints.
//
//   node verify-diagnostic-signs.mjs   (needs a built dist/eh.mjs — run build.sh first)
import createModule from "../dist/eh.mjs";

const M = await createModule();

const eh_new = M.cwrap("eh_new", "number", []);
const eh_boot_finish = M.cwrap("eh_boot_finish", null, ["number"]);
const eh_input = M.cwrap("eh_input", null, ["number", "string"]);
const eh_exec_lua = M.cwrap("eh_exec_lua", "number", ["number", "string"]);
const eh_redraw_json = M.cwrap("eh_redraw_json", "number", ["number"]);
const eh_free_string = M.cwrap("eh_free_string", null, ["number"]);
const eh_free = M.cwrap("eh_free", null, ["number"]);

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
eh_boot_finish(h);

// A buffer with text so line 1 exists and the diagnostic anchors to a real row.
eh_input(h, "ihello world<Esc>");

// Drive a client-set diagnostic (severity 1 = Error) on line 1 (lnum is 0-based) via
// the real Lua → effects path — exactly what `vim.diagnostic.set` does. exec_lua drains
// the queued LSP op into the render store and reprojects the redraw, so the next
// `eh_redraw_json` reflects it.
const set = readStr(
  eh_exec_lua(
    h,
    'local ns = nx.ns.create("diag-test")\n' +
      'nx.diagnostic.set(ns, 0, { { lnum = 0, col = 0, severity = 1, message = "boom" } })\n' +
      "return 1",
  ),
);
check("lua: nx.diagnostic.set ran without error", set.startsWith("ok"), `got ${JSON.stringify(set)}`);

// Find every `diagnostics_signs` payload in the redraw frame (one per window). Each is a
// per-row array; a row with a sign is `[glyph, severity, style_id]`, else null.
const frame = JSON.parse(readStr(eh_redraw_json(h)));
const signRows = [];
(function walk(node) {
  if (Array.isArray(node)) return node.forEach(walk);
  if (node && typeof node === "object") {
    if (Array.isArray(node.diagnostics_signs)) signRows.push(node.diagnostics_signs);
    for (const k of Object.keys(node)) walk(node[k]);
  }
})(frame);

check("redraw: a window carries a diagnostics_signs payload", signRows.length > 0, `frame=${JSON.stringify(frame).slice(0, 400)}`);

// The Error sign (default glyph "E", severity code 1) must reach some row of some window.
const errSign = signRows
  .flat()
  .find((c) => Array.isArray(c) && c[0] === "E" && c[1] === 1);
check(
  "redraw: the diagnostic Error sign (glyph 'E', sev 1) reached the gutter payload",
  !!errSign,
  `diagnostics_signs payloads: ${JSON.stringify(signRows)}`,
);

eh_free(h);

console.log(
  failures === 0
    ? "\nALL PASS — client-set diagnostic signs project into the wasm redraw gutter"
    : `\n${failures} FAILED`,
);
process.exit(failures === 0 ? 0 : 1);
