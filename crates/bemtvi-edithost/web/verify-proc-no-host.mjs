// Regression for the silent-hang half of the picker bug: on the serverless build (NO process
// host) an async spawn (`btv.run` / `btv.run_stream`) used to be dropped with only an `echo`,
// leaving its promise/stream pending forever — so the file/grep pickers, which stream `rg`,
// hung silently with no results and no way to fall back. The fix completes the callback LOUD
// with a spawn-failure exit (code -1, the same shape a missing binary yields). This drives a
// real `btv.run` through the wasm tick (no proc host set) and asserts it RESOLVES with code -1.
//
//   node verify-proc-no-host.mjs   (needs a built dist/eh.mjs — run build.sh first)
import createModule from "../dist/eh.mjs";

const M = await createModule();
const eh_new = M.cwrap("eh_new", "number", []);
const eh_boot_finish = M.cwrap("eh_boot_finish", null, ["number"]);
const eh_exec_lua = M.cwrap("eh_exec_lua", "number", ["number", "string"]);
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
  if (!ok) { if (detail !== undefined) console.log(`        ${detail}`); failures++; }
}

const h = eh_new();
if (h === 0) { console.error("FATAL: eh_new returned null"); process.exit(1); }
eh_boot_finish(h);

// No `eh_set_proc_host` is ever called here, so `has_remote_proc()` is false — the serverless
// "no host" path. `btv.run` queues a spawn; the tick (exec_lua drains loop_ops + apply effects)
// must complete it with code -1 and run the `:next` continuation, all within this call.
readStr(eh_exec_lua(h,
  `_G.code = "pending"\n` +
  `btv.run({ cmd = "rg", args = { "--files" } }):next(function(r) _G.code = r.code end)`));

const code = readStr(eh_exec_lua(h, "return _G.code"));
check("hostless btv.run RESOLVES (no silent hang)", !/pending/.test(code), `code=${JSON.stringify(code)}`);
check("hostless btv.run resolves with spawn-failure code -1", /(^|\D)-1\b/.test(code) || / -1/.test(code), `code=${JSON.stringify(code)}`);

eh_free(h);
console.log(failures === 0
  ? "\nALL PASS — a hostless spawn fails loud (code -1) instead of hanging"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
