// Repro/regression for the bug "diff3 panes show [No Name] in the web statusline":
// a Lua statusline plugin (nxvim-line) labels each window from `nx.buf.name(bufnr)`,
// but the buffer-name mirror that getter reads was sourced from the path-only
// `buffer_name`, which is empty for an `nx.view` (it has no file path). So every named
// view — a diff pane, a file tree — read back as `""` → `[No Name]`. The fix surfaces
// the view's create name through `display_name` (the same precedence the rendered
// statusline `%f` / tab label already used). This drives the REAL wasm tick and asserts
// `nx.buf.name` returns the name both by handle and for the focused buffer.
//
//   node verify-view-name.mjs   (needs a built dist/eh.mjs — run build.sh first)
import createModule from "../dist/eh.mjs";

const M = await createModule();
const eh_new = M.cwrap("eh_new", "number", []);
const eh_boot_finish = M.cwrap("eh_boot_finish", null, ["number"]);
const eh_input = M.cwrap("eh_input", null, ["number", "string"]);
const eh_exec_lua = M.cwrap("eh_exec_lua", "number", ["number", "string"]);
const eh_free_string = M.cwrap("eh_free_string", null, ["number"]);
const eh_free = M.cwrap("eh_free", null, ["number"]);

function readStr(ptr) {
  const s = M.UTF8ToString(ptr);
  eh_free_string(ptr);
  return s;
}
// `eh_exec_lua` renders the Lua result as a string prefixed `ok:` / `err:`.
function execLua(h, code) {
  const raw = readStr(eh_exec_lua(h, code));
  if (raw.startsWith("err:")) throw new Error(raw);
  return raw.slice(3); // strip "ok:"
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) { if (detail !== undefined) console.log(`        ${detail}`); failures++; }
}

const h = eh_new();
if (h === 0) { console.error("FATAL: eh_new returned null"); process.exit(1); }
eh_boot_finish(h);

// A real buffer so the view mounts beside something.
eh_input(h, "imain<Esc>");
// Create + mount a named view (the diff-pane shape).
execLua(h, `vw = nx.view.create{ name = "ours" }
            vw:set_lines{ "x" }
            vw:mount{ split = "vsplit" }`);
// A barrier tick so the bufnr mirror + buffer mirror settle.
eh_input(h, "<Esc>");

// By handle — the path a statusline plugin uses to label a window it doesn't focus.
const byHandle = execLua(h, `return nx.buf.name(vw:bufnr())`);
check("nx.buf.name(view bufnr) is the create name, not [No Name]",
  byHandle.includes("\"ours\""), `result=${JSON.stringify(byHandle)}`);

// Focused-buffer fast path (`nx.buf.name(0)` / `expand("%")` via the _cur_buf snapshot).
execLua(h, `vw:focus()`);
eh_input(h, "<Esc>");
const current = execLua(h, `return nx.buf.name(0)`);
check("nx.buf.name(0) for the focused view is the create name",
  current.includes("\"ours\""), `result=${JSON.stringify(current)}`);

eh_free(h);
console.log(failures === 0
  ? "\nALL PASS — an nx.view's name reaches nx.buf.name on the web build"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
