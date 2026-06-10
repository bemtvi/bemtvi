// ⚠ TEMPORARY DEMO harness — see Cargo.toml / src/lib.rs banners. Delete with the crate.
//
// Headless proof that the real editor core + the Lua VM run together in wasm: load
// the emscripten module, then (1) feed vim keys and read the buffer, (2) execute Lua
// and read its result, (3) let Lua drive an :-command into the buffer. Exits non-zero
// on any failed assertion so it can gate CI / a manual check.
import createModule from "./dist/eh.mjs";

const M = await createModule();

const eh_new = M.cwrap("eh_new", "number", []);
const eh_input = M.cwrap("eh_input", null, ["number", "string"]);
const eh_exec_lua = M.cwrap("eh_exec_lua", "number", ["number", "string"]);
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
function check(label, got, want) {
  const ok = got === want;
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) {
    console.log(`        got:  ${JSON.stringify(got)}`);
    console.log(`        want: ${JSON.stringify(want)}`);
    failures++;
  }
}

const h = eh_new();
if (h === 0) {
  console.error("FATAL: eh_new returned null (Lua VM failed to init in wasm)");
  process.exit(1);
}

// 1. The editor core runs in wasm: insert text via vim keys, read it back.
eh_input(h, "ihello world<Esc>");
check("editor: insert via vim keys", readStr(eh_lines(h)), "hello world");

// 2. The Lua VM runs in wasm: evaluate an expression (incl. a vim.* stdlib call).
check("lua: arithmetic eval", readStr(eh_exec_lua(h, "return 1 + 41")), "ok:42");
// A real vim.* function (not bare Lua): vim.split runs the prelude in wasm.
check("lua: vim.* stdlib in wasm", readStr(eh_exec_lua(h, 'return #vim.split("a,b,c", ",")')), "ok:3");

// 3. Lua drives the editor: a queued :-command mutates the buffer (the one wired effect).
eh_exec_lua(h, 'vim.cmd("%s/hello/LUA/")');
check("lua → editor: :substitute applied", readStr(eh_lines(h)), "LUA world");

eh_free(h);

console.log(failures === 0 ? "\nALL PASS — core+Lua validated in wasm" : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
