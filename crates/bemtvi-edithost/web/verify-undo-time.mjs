// The undo timeline in the wasm edit-host: `:undolist`'s age column and
// `:earlier`/`:later {N}s` read node timestamps, which are seconds on the editor's
// monotonic base. The browser's base is the Worker's `performance.now()` clock, handed
// in by `eh_set_clock` before each tick — unstamped, every state is the same age and
// every timed travel runs off the end of the history.
//
// Pure node (no browser): this drives the real EditHost tick through the emscripten
// module directly, so the clock seam is exercised without a Worker.
//
// Prereqs: ../build.sh (dist/eh.mjs + eh.wasm). Run: node verify-undo-time.mjs
import createModule from "../dist/eh.mjs";

const M = await createModule();

const eh_new = M.cwrap("eh_new", "number", []);
const eh_boot_finish = M.cwrap("eh_boot_finish", null, ["number"]);
const eh_set_clock = M.cwrap("eh_set_clock", null, ["number", "number"]);
const eh_input = M.cwrap("eh_input", null, ["number", "string"]);
const eh_exec_lua = M.cwrap("eh_exec_lua", "number", ["number", "string"]);
const eh_lines = M.cwrap("eh_lines", "number", ["number"]);
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

// Two change groups, thirty seconds apart on the Worker's clock. Each commits with the
// second its group *began* — so state 1 is stamped 10, state 2 stamped 40.
eh_set_clock(h, 10_000);
eh_input(h, "ialpha<Esc>");
eh_set_clock(h, 40_000);
eh_input(h, "obravo<Esc>");
eh_set_clock(h, 70_000);
eh_input(h, "ocharlie<Esc>");
eh_set_clock(h, 100_000);

// 1. The nodes carry distinct timestamps rather than all sitting at 0.
const times = readStr(
  eh_exec_lua(
    h,
    `local t = btv.undotree.get(0)
     local out = {}
     local function walk(es) for _, e in ipairs(es) do out[#out+1] = e.seq .. '@' .. e.time walk(e.alt or {}) end end
     walk(t.entries)
     return table.concat(out, ',')`,
  ),
);
check(
  "undo nodes are stamped from the Worker clock",
  times.includes("1@10,2@40,3@70"),
  `got ${JSON.stringify(times)} (want 1@10,2@40,3@70 — an all-zero run means the ` +
    `monotonic base is never stamped on this leg)`,
);

// 2. `:undolist` renders a real age, not "0 seconds ago" for everything.
eh_input(h, ":undolist<CR>");
const listing = readStr(eh_lines(h));
check(
  "`:undolist` ages read off the same base",
  listing.includes("30 seconds ago"),
  `got ${JSON.stringify(listing)} (the newest state is stamped 70, the clock is at 100)`,
);
eh_input(h, "q");

// 3. `:earlier {N}s` measures from the *current* state's timestamp: 40 seconds back
//    from state 3 (stamped 70) is 30, and the newest state at or before that is state 1.
//    An unstamped base makes every state age 0, so this would run off the end instead.
eh_input(h, ":earlier 40s<CR>");
const back = readStr(eh_lines(h));
check(
  "`:earlier 40s` lands on the state that far back",
  back === "alpha",
  `got ${JSON.stringify(back)}`,
);

// 4. …and `:later 40s` from state 1 (stamped 10) reaches state 3 (70), the oldest
//    state at or after 50.
eh_input(h, ":later 40s<CR>");
const fwd = readStr(eh_lines(h));
check(
  "`:later 40s` comes forward again",
  fwd === "alpha\nbravo\ncharlie",
  `got ${JSON.stringify(fwd)}`,
);

eh_free(h);

console.log(
  failures === 0
    ? "\nALL PASS — the undo timeline is stamped and travelled in the wasm edit-host"
    : `\n${failures} FAILED`,
);
process.exit(failures === 0 ? 0 : 1);
