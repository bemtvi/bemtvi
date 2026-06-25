// Regression for the wasm half of Phase 2b: a picker preview of an UN-loaded file on the
// serverless build used to be stuck "<path>: loading…" because the preview read went
// through the synchronous (off-tick) host FS. Now `ensure_preview` issues an async
// `fs_fetch` tagged with a reserved buffer id (PREVIEW_FETCH_BUF = 2^48), and the OPFS
// read lands into the preview cache via `complete_fs_read`. This drives that whole loop in
// node against the REAL wasm tick: open a custom file-preview picker, drain the queued fs
// request (asserting the reserved id round-trips through the FFI's f64 buffer id), hand
// back content, and assert it reaches the preview pane in the redraw frame.
//
//   node verify-preview-fetch.mjs   (needs a built dist/eh.mjs — run build.sh first)
import createModule from "../dist/eh.mjs";

const M = await createModule();
const PREVIEW_FETCH_BUF = 2 ** 48; // must match crate::redraw::PREVIEW_FETCH_BUF

const eh_new = M.cwrap("eh_new", "number", []);
const eh_boot_finish = M.cwrap("eh_boot_finish", null, ["number"]);
const eh_exec_lua = M.cwrap("eh_exec_lua", "number", ["number", "string"]);
const eh_redraw_json = M.cwrap("eh_redraw_json", "number", ["number"]);
const eh_take_fs_requests = M.cwrap("eh_take_fs_requests", "number", ["number"]);
const eh_fs_read_complete = M.cwrap("eh_fs_read_complete", null,
  ["number", "number", "string", "number", "string", "number", "number"]);
const eh_free_string = M.cwrap("eh_free_string", null, ["number"]);
const eh_free = M.cwrap("eh_free", null, ["number"]);

function readStr(ptr) { const s = M.UTF8ToString(ptr); eh_free_string(ptr); return s; }
let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) { if (detail !== undefined) console.log(`        ${detail}`); failures++; }
}

const h = eh_new();
if (h === 0) { console.error("FATAL: eh_new returned null"); process.exit(1); }
eh_boot_finish(h);

// A custom file-preview source pointing at an un-loaded path. Opening it projects a
// preview pane, which (off-tick) issues a preview fetch for the path.
readStr(eh_exec_lua(h,
  `nx.picker.source {\n` +
  `  name = "preview_test",\n` +
  `  preview = "file",\n` +
  `  items = function(ctx) ctx.push { text = "target", path = "/virtual/target.txt" } end,\n` +
  `  confirm = function() end,\n` +
  `}\n` +
  `nx.picker.open('preview_test')`));

// Drain the queued fs requests: the preview fetch must be there, tagged with the reserved
// buffer id (proving it round-trips through the FFI's f64 buffer id unscathed).
const reqs = JSON.parse(readStr(eh_take_fs_requests(h)));
const read = reqs.reads.find((r) => r.path === "/virtual/target.txt");
check("preview issues an off-tick fs_fetch for the un-loaded path", !!read, `reads=${JSON.stringify(reqs.reads)}`);
check("the fetch is tagged with the reserved PREVIEW_FETCH_BUF id (round-trips exactly)",
  read && read.buffer === PREVIEW_FETCH_BUF, `buffer=${read && read.buffer}, want=${PREVIEW_FETCH_BUF}`);

// Hand back the file's bytes through wasm memory (kind 0), exactly as the Worker's
// landFsRead does, and let the tick land + repaint.
const bytes = new TextEncoder().encode("PREVIEW CONTENT\nsecond line");
const ptr = M._malloc(bytes.length);
M.HEAPU8.set(bytes, ptr);
try {
  eh_fs_read_complete(h, PREVIEW_FETCH_BUF, "/virtual/target.txt", 0, "", ptr, bytes.length);
} finally {
  M._free(ptr);
}

// The fetched content must reach the preview pane in the redraw frame (no longer "loading…").
const frame = readStr(eh_redraw_json(h));
check("fetched content reaches the preview pane (not a loading stub)",
  /PREVIEW CONTENT/.test(frame) && !/loading…/.test(frame),
  `frame has loading=${/loading…/.test(frame)} content=${/PREVIEW CONTENT/.test(frame)}`);

eh_free(h);
console.log(failures === 0
  ? "\nALL PASS — wasm picker preview fetches an un-loaded file over the off-tick fs seam"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
