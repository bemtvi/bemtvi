// The edit-host Web Worker (Phase 5). This is the single `!Send` thread that owns
// nxvim's core + the PUC Lua 5.4 VM: it loads the emscripten module (`dist/eh.mjs`),
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
import { dialDaemon } from "./rpc.mjs";
// Build-time feature flags. The standard editor build ships `localHost: false`; only the
// python-demo build (build-demo.sh) flips it true, which makes this Worker install the local
// in-browser process host (see the boot tail). The demo module is dynamic-imported, so the
// standard build never even loads it.
import { BUILD } from "./build-config.js";
// The `nx.fs` reply is re-encoded to msgpack bytes (faithful binary for a `read` result)
// before crossing into Rust, which decodes it through the shared `fswire` codec — so JS
// stays a near-dumb pipe (no per-variant format knowledge). Same vendored lib as rpc.mjs.
import { encode } from "./vendor/msgpack/index.mjs";

const M = await createModule();

const eh_new = M.cwrap("eh_new", "number", []);
const eh_input = M.cwrap("eh_input", null, ["number", "string"]);
const eh_input_mouse = M.cwrap("eh_input_mouse", null, ["number", "string", "string", "string", "number", "number"]);
const eh_source_lua = M.cwrap("eh_source_lua", "number", ["number", "string"]);
const eh_boot_finish = M.cwrap("eh_boot_finish", null, ["number"]);
const eh_attach = M.cwrap("eh_attach", null, ["number", "number", "number"]);
const eh_set_clock = M.cwrap("eh_set_clock", null, ["number", "number"]);
const eh_next_deadline = M.cwrap("eh_next_deadline", "number", ["number"]);
const eh_tick_timers = M.cwrap("eh_tick_timers", "number", ["number", "number"]);
const eh_exec_lua = M.cwrap("eh_exec_lua", "number", ["number", "string"]);
const eh_redraw_json = M.cwrap("eh_redraw_json", "number", ["number"]);
const eh_lines = M.cwrap("eh_lines", "number", ["number"]);
const eh_aux_lines = M.cwrap("eh_aux_lines", "number", ["number"]);
const eh_free_string = M.cwrap("eh_free_string", null, ["number"]);
// Off-tick OPFS fs (Phase 6): the editor enqueues `:e`/`:w` off-tick; the Worker drains
// the requests, runs the async OPFS op, and lands the result back through these.
const eh_take_fs_requests = M.cwrap("eh_take_fs_requests", "number", ["number"]);
const eh_save_bytes = M.cwrap("eh_save_bytes", "number", ["number", "number"]);
const eh_save_len = M.cwrap("eh_save_len", "number", ["number", "number"]);
// (h, buffer, path, kind, contents, data, len) — a file's raw bytes cross as the data/len
// pair (so non-UTF-8 reaches Rust intact for the encoding seam); contents carries only the
// dir JSON / error message. See `landFsRead`.
const eh_fs_read_complete = M.cwrap("eh_fs_read_complete", null, ["number", "number", "string", "number", "string", "number", "number"]);
const eh_fs_write_complete = M.cwrap("eh_fs_write_complete", null, ["number", "number", "number", "number", "number", "string"]);
// Remote watch leg (Phase 6): the editor arms one watch per file-backed buffer off-tick; the
// Worker forwards each to the daemon and lands its `fs_changed` pushes back into the tick.
const eh_take_watch_requests = M.cwrap("eh_take_watch_requests", "number", ["number"]);
const eh_remote_file_changed = M.cwrap("eh_remote_file_changed", null, ["number", "string", "number", "number", "number"]);
// Remote proc leg (Phase 6d): the editor enqueues async `vim.system` / `jobstart` spawns
// off-tick (only when a process host — a daemon OR a local in-browser Worker host — is present;
// `eh_set_proc_host` gates it); the Worker forwards each to the host and lands its
// `proc_spawned`/`proc_exited` pushes back into the tick. `eh_proc_exited` takes stdout/stderr
// as pointer+length (process output is raw bytes).
const eh_set_proc_host = M.cwrap("eh_set_proc_host", null, ["number", "number"]);
const eh_take_proc_requests = M.cwrap("eh_take_proc_requests", "number", ["number"]);
const eh_proc_spawned = M.cwrap("eh_proc_spawned", null, ["number", "number", "number"]);
// Streaming stdout (`nx.run_stream`'s batches): the daemon pushes `proc_stdout` batches; the
// lines ride as a JSON string array (newline-stripped) into the Lua callback.
const eh_proc_stdout = M.cwrap("eh_proc_stdout", null, ["number", "number", "string"]);
const eh_proc_exited = M.cwrap("eh_proc_exited", null, ["number", "number", "number", "number", "number", "number", "number"]);
// Remote `nx.fs` leg (Phase 2 of the off-tick plan): the editor enqueues each high-level
// `nx.fs.*` op off-tick (only with a daemon connected); the Worker forwards each as one
// `luafs_op` request over WebTransport and lands the reply via `eh_fs_op_result`. The reply
// rides as pointer+length (re-encoded msgpack — a `read` result is raw bytes Rust decodes).
const eh_take_fs_op_requests = M.cwrap("eh_take_fs_op_requests", "number", ["number"]);
const eh_fs_op_result = M.cwrap("eh_fs_op_result", null, ["number", "number", "number", "number"]);
// Streaming `nx.fs.watch` over the daemon (Phase 3b): the editor arms/disarms a recursive watch
// off-tick (only with a daemon — serverless has no change source); the Worker forwards each over
// WebTransport (`luafs_watch`/`luafs_unwatch`) and lands the daemon's `luafs_change`/`luafs_watch_err`
// pushes back into the tick. `eh_fs_watch_change` takes the changed paths as a JSON string array.
const eh_take_fs_watch_requests = M.cwrap("eh_take_fs_watch_requests", "number", ["number"]);
const eh_fs_watch_change = M.cwrap("eh_fs_watch_change", null, ["number", "number", "string", "string"]);
const eh_fs_watch_err = M.cwrap("eh_fs_watch_err", null, ["number", "number", "string"]);
// Terminal leg (the web `:terminal` — Phase 7): the editor enqueues PTY ops off-tick (only
// with a daemon connected); the Worker forwards each to the daemon and lands its
// `term_data`/`term_exit` pushes back into the tick. `eh_terminal_data` takes the child's
// output as pointer+length (raw PTY bytes); the Worker feeds each push then `eh_terminal_flush`
// projects once per drain (one repaint, never per chunk).
const eh_take_terminal_requests = M.cwrap("eh_take_terminal_requests", "number", ["number"]);
const eh_terminal_data = M.cwrap("eh_terminal_data", null, ["number", "number", "number", "number"]);
const eh_terminal_flush = M.cwrap("eh_terminal_flush", null, ["number"]);
const eh_terminal_exit = M.cwrap("eh_terminal_exit", null, ["number", "number", "number"]);
// LSP leg (Phase 6e): the editor's `SyncLspClient` enqueues raw `lsp_spawn`/`lsp_stdin`/
// `lsp_kill` ops off-tick (only with a daemon connected — language servers run on the daemon,
// the same wire the native `RemoteLspTransport` uses); the Worker forwards each and lands the
// daemon's `lsp_stdout`/`lsp_stderr`/`lsp_exited` pushes back into the client. `eh_lsp_stdout` /
// `eh_lsp_stderr` take the server's output as pointer+length (JSON-RPC framing may carry NULs).
const eh_take_lsp_requests = M.cwrap("eh_take_lsp_requests", "number", ["number"]);
const eh_lsp_stdout = M.cwrap("eh_lsp_stdout", null, ["number", "number", "number", "number"]);
const eh_lsp_stderr = M.cwrap("eh_lsp_stderr", null, ["number", "number", "number", "number"]);
const eh_lsp_exited = M.cwrap("eh_lsp_exited", null, ["number", "number", "number", "number"]);
// Treesitter `:TSInstall` leg: the editor enqueues each install off-tick; the Worker
// forwards it to the UI thread (web-tree-sitter lives there), which fetches/caches/registers
// the grammar and lands the outcome back via `eh_ts_install_complete`. `eh_ts_seed_installed`
// tells the core, at boot, which grammars are already available (bundle + OPFS cache).
const eh_take_ts_requests = M.cwrap("eh_take_ts_requests", "number", ["number"]);
const eh_ts_install_complete = M.cwrap("eh_ts_install_complete", null, ["number", "string", "number", "string"]);
const eh_ts_seed_installed = M.cwrap("eh_ts_seed_installed", null, ["number", "string"]);
// Clipboard (`"+`/`"*`) leg: the editor enqueues each `"+`/`"*` yank/delete off-tick; the
// Worker drains it and forwards to the UI thread (only the UI thread can reach
// `navigator.clipboard`) to write out. The UI pushes the OS clipboard back in via
// `eh_clipboard_push` so a `"+p` sees an external copy.
const eh_take_clipboard_writes = M.cwrap("eh_take_clipboard_writes", "number", ["number"]);
const eh_clipboard_push = M.cwrap("eh_clipboard_push", null, ["number", "string"]);
// Persistence (shada): the editor's cross-session snapshot ↔ a JSON blob the Worker keeps
// in OPFS. `eh_export_shada(include_exit)` serializes it; `eh_load_shada(json)` restores it.
const eh_export_shada = M.cwrap("eh_export_shada", "number", ["number", "number"]);
const eh_load_shada = M.cwrap("eh_load_shada", "number", ["number", "string"]);

function readStr(ptr) {
  const s = M.UTF8ToString(ptr);
  eh_free_string(ptr);
  return s;
}

// Tree-sitter INDENTATION (web/ts-indent.js). Unlike highlighting — a UI-thread paint
// overlay that may repaint late — the core decides indentation *synchronously, inside the
// tick* (on `o`/`O`/`<CR>`/`=`), so it must run HERE in the worker. The indenter loads
// grammars + indents.scm (offline bundle / OPFS install cache) and answers `indent()`
// synchronously; the Rust tick reaches it through the `eh_js_ts_*` FFI bridge
// (web/eh-lib.js), which forwards to these globals.
//
// Loaded *dynamically and guarded*: the indenter is optional (it pulls in web-tree-sitter),
// so a load failure must degrade to "no ts indent" (the core falls back to copy-previous /
// column 0), never abort the worker and hang the editor. The grammar loads are async too, so
// a keystroke that beats the first load just falls back that once.
let indenter = null;
let indenterSettled = false; // the dynamic import has resolved or rejected (not still loading the module)
import("./ts-indent.js")
  .then(({ createIndenter }) => {
    indenter = createIndenter();
    globalThis.__nxvimTsIndent = (lang, text, line, sw, ts) => indenter.indent(lang, text, line, sw, ts);
    globalThis.__nxvimTsAvailable = (lang) => indenter.available(lang);
    globalThis.__nxvimTsReload = (lang) => indenter.reload(lang);
  })
  .catch((e) => postMessage({ type: "config_error", error: "ts-indent unavailable: " + (e && e.stack ? e.stack : e) }))
  .finally(() => { indenterSettled = true; });

// Whether the indenter has async work the SAB run loop must stay event-loop-live for: the
// module import itself, then its init (web-tree-sitter + manifests) and any in-flight grammar
// load. A thread blocked in `Atomics.wait` can't run those promises, so the loop parks
// non-blockingly (`Atomics.waitAsync`) while this is true — the same treatment daemon watches
// get — and only blocks once the indenter is fully idle.
const indenterBusy = () => !indenterSettled || (indenter !== null && indenter.pendingLoads() > 0);

const h = eh_new();
if (h === 0) {
  postMessage({ type: "fatal", error: "eh_new returned null (Lua VM failed to init in wasm)" });
  throw new Error("eh_new returned null");
}

// Serverless config: source a single-file `/init.lua` from OPFS (if present) between the
// two halves of boot (`eh_new` did `boot_begin`; we finish with `eh_boot_finish`), so a
// config's options / keymaps / autocmds — including the startup buffer's `BufEnter` —
// apply to the very first frame (native ordering: config first). `require` of further
// modules won't resolve (the browser build's runtimepath is empty), so this is one file.
// A broken config is surfaced (non-fatal) — the editor still finishes booting.
async function bootWithConfig() {
  // Demo build only: on FIRST boot, seed OPFS with the demo project + tour + init.lua
  // (web/demo-seed/, fetched as static assets). A sentinel (/.nxvim/.demo-seeded) makes this
  // a one-time action, so a user's later edits persist across reloads. Runs BEFORE the
  // init.lua read below so the seeded config applies on this very boot. Non-fatal on failure.
  if (BUILD.demoSeed) {
    try {
      const seeded = await opfsRead("/.nxvim/.demo-seeded"); // kind 1 = absent (not seeded yet)
      if (seeded.kind !== 0) {
        const res = await fetch(new URL("demo-seed/manifest.json", import.meta.url));
        if (!res.ok) throw new Error(`manifest HTTP ${res.status}`);
        const files = (await res.json()).files || [];
        const enc = new TextEncoder();
        for (const rel of files) {
          const f = await fetch(new URL(`demo-seed/${rel}`, import.meta.url));
          if (!f.ok) throw new Error(`${rel} HTTP ${f.status}`);
          const w = await opfsWrite(`/${rel}`, enc.encode(await f.text()));
          if (!w.ok) throw new Error(`${rel}: ${w.error}`);
        }
        await opfsWrite("/.nxvim/.demo-seeded", enc.encode("1"));
      }
    } catch (e) {
      postMessage({ type: "config_error", error: "demo seed: " + String(e) });
    }
  }
  // Demo build only: source the vendored first-party plugin bundle (build-plugins.sh →
  // web/vendor/plugins/plugins-bundle.lua, an immutable build asset fetched fresh per
  // deploy). The standard editor ships no plugins (BUILD.plugins=false) and skips this. It
  // runs BEFORE the OPFS bundle / init.lua so a user override and config `require(...)` both
  // resolve. A broken/missing bundle is surfaced non-fatally; the editor still boots.
  if (BUILD.plugins) {
    try {
      const res = await fetch(new URL("vendor/plugins/plugins-bundle.lua", import.meta.url));
      if (res.ok) {
        const err = readStr(eh_source_lua(h, await res.text()));
        if (err) postMessage({ type: "config_error", error: "plugin bundle: " + err });
      } else {
        postMessage({ type: "config_error", error: `plugin bundle: HTTP ${res.status}` });
      }
    } catch (e) {
      postMessage({ type: "config_error", error: "plugin bundle: " + String(e) });
    }
  }
  // Source an amalgamated plugin bundle seeded into OPFS (a user-supplied bundle, or the
  // Phase-5 test fixture) — same package.preload mechanism, so an `init.lua` that
  // `require("nxvim-line")`-class resolves it. Absent → skipped, exactly like an absent
  // init.lua. A broken bundle is surfaced (non-fatal); the editor still boots.
  try {
    const bundle = await opfsRead("/plugins-bundle.lua"); // kind 0 = file (raw `bytes`), 1 = absent
    const bundleText = bundle.kind === 0 && bundle.bytes ? utf8(bundle.bytes) : "";
    if (bundleText.length) {
      const err = readStr(eh_source_lua(h, bundleText));
      if (err) postMessage({ type: "config_error", error: "plugin bundle: " + err });
    }
  } catch (e) {
    postMessage({ type: "config_error", error: "plugin bundle: " + String(e) });
  }
  try {
    const cfg = await opfsRead("/init.lua"); // kind 0 = file (raw `bytes`), 1 = absent, 2 = dir, 3 = error
    // A file read returns RAW bytes (`opfsRead` keeps them undecoded for the binary-safe
    // encoding seam); init.lua is UTF-8 source, so decode it here before sourcing.
    const cfgText = cfg.kind === 0 && cfg.bytes ? utf8(cfg.bytes) : "";
    if (cfgText.length) {
      const err = readStr(eh_source_lua(h, cfgText));
      if (err) postMessage({ type: "config_error", error: err });
    }
  } catch (e) {
    postMessage({ type: "config_error", error: String(e) });
  }
  // Restore cross-session state (registers/marks/history/jumplist) from OPFS *after* the
  // config (so a config can't clobber a restored mark) and *before* `eh_boot_finish` fires
  // the startup lifecycle — so a restored `` `" `` / registers / history are live for the
  // first frame, matching native's load ordering.
  try {
    const sh = await opfsRead(SHADA_PATH); // kind 0 = present (raw `bytes`), 1 = none yet
    const shText = sh.kind === 0 && sh.bytes ? utf8(sh.bytes) : ""; // shada is UTF-8 JSON; decode the raw bytes
    if (shText.length) {
      const err = readStr(eh_load_shada(h, shText));
      if (err) postMessage({ type: "config_error", error: err });
      else shadaBaseline = readStr(eh_export_shada(h, 0)); // seed baseline so the first checkpoint is a no-op
    }
  } catch (e) {
    postMessage({ type: "config_error", error: "shada load: " + e });
  }
  // Seed the core's set of available treesitter grammars (for `:TSInstallInfo`): the offline
  // bundle (vendor/manifest.json) ∪ whatever a previous session installed (OPFS). The UI
  // highlighter reads the same two manifests itself; this only mirrors the list into the core.
  try {
    const avail = new Set();
    try {
      const res = await fetch(new URL("vendor/manifest.json", import.meta.url));
      if (res.ok) for (const l of (await res.json()).languages || []) avail.add(l);
    } catch { /* highlighter assets not built — plain rendering, empty bundle */ }
    const cache = await opfsRead("/.nxvim/treesitter/manifest.json"); // kind 0 = present (raw `bytes`)
    const cacheText = cache.kind === 0 && cache.bytes ? utf8(cache.bytes) : ""; // UTF-8 JSON; decode raw bytes
    if (cacheText.length) for (const l of JSON.parse(cacheText)) avail.add(l);
    eh_ts_seed_installed(h, JSON.stringify([...avail]));
  } catch (e) {
    postMessage({ type: "config_error", error: "treesitter seed: " + e });
  }
  eh_boot_finish(h);
}
// NB: invoked at the very END of the module (see below), not here — the OPFS helpers it
// reaches (`splitPath` et al.) are `const` arrows still in their temporal dead zone at
// this point in module evaluation.

// `eh_redraw_json` returns the `redraw` notification's params array `[viewMap]` (or
// "null" before the first frame); the renderable frame is the single view map.
function currentFrame() {
  try {
    const parsed = JSON.parse(readStr(eh_redraw_json(h)));
    const frame = Array.isArray(parsed) ? (parsed[0] ?? null) : parsed;
    // Warm the indenter's grammars for whatever's on screen, so ts-indent is ready before
    // the user types `o`/`<CR>` (the grammar load is async; a keystroke that beats it just
    // falls back this once). Cheap + idempotent; the indenter may not be loaded yet.
    if (indenter) indenter.ensureForFrame(frame);
    return frame;
  } catch (e) {
    postMessage({ type: "fatal", error: `redraw JSON parse failed: ${e}` });
    return null;
  }
}

const nowMs = () => (typeof performance !== "undefined" ? performance.now() : Date.now());
const utf8 = (bytes) => new TextDecoder().decode(bytes);

// The full buffer text the UI's JS highlighter needs (`eh_lines` = the whole current
// buffer joined). It's `O(buffer size)` to build + structured-clone over postMessage, so
// ship it only when it'll actually be used and has actually changed:
//   * never for a terminal — its colors come from the server vt100 palette, not the JS
//     highlighter, and its scrollback can be thousands of lines (this was the terminal
//     "slowness": every keystroke echo / output burst re-shipped the entire scrollback);
//   * otherwise only when `(bufnr, changedtick)` moved, so a cursor-only or
//     terminal-driven redraw doesn't re-ship an unchanged code buffer.
// `null` tells the UI to keep its cached `bufferText` (see `setFrame`).
let lastLinesKey = null;
function linesForFrame(frame) {
  if (!frame) return null;
  const wins = frame.windows || [];
  const focused = wins.find((w) => w.focused) || wins[0];
  if (focused && focused.terminal) return null;
  const key = `${frame.bufnr ?? -1}:${frame.changedtick ?? -1}`;
  if (key === lastLinesKey) return null;
  lastLinesKey = key;
  return readStr(eh_lines(h));
}

// The full text of every visible *background* (non-focused) buffer, as `{ file_name:
// text }` — `eh_lines` ships only the focused buffer, so a window that never held focus
// (the file beneath a grabbing float opened at startup) would have no text for the JS
// highlighter and render dark. Only consulted when more than one window is on screen
// (the single-window common case has no background buffer, so this is skipped and the
// editor view isn't rebuilt). The readout reflects the live buffers, so a background
// buffer changing — e.g. its content arriving from an async OPFS open *after* the float
// grabbed focus — re-ships; an unchanged background keeps the UI's cache (`null`).
let lastAux = "{}";
function auxLinesForFrame(frame) {
  if (!frame) return null;
  const wins = frame.windows || [];
  if (wins.length < 2) { lastAux = "{}"; return null; }
  const json = readStr(eh_aux_lines(h));
  if (json === lastAux) return null;
  lastAux = json;
  const obj = JSON.parse(json);
  return Object.keys(obj).length ? obj : null;
}

// Build a `redraw` postMessage: the frame once (so `linesForFrame` reads the same frame
// it ships), plus any per-site extras (`acks`/`results`/`id`/…).
function redrawMsg(extra) {
  const frame = currentFrame();
  return { type: "redraw", frame, lines: linesForFrame(frame), aux: auxLinesForFrame(frame), ...extra };
}

// =============================================================================
// Real local filesystem (File System Access API) — the `:eo` / `:wo` / … picker family.
//
// Unlike OPFS (the sandboxed default fs), these commands open *real* local files the user
// grants through the browser's native picker. The picker must run on the UI thread (it
// needs a window + a user gesture) and yields a `FileSystemFileHandle`, not a path — so a
// picker-bound file's bytes live behind a handle the UI holds, and its read/write can only
// be done by the UI (async `getFile()` / `createWritable()`). The Worker therefore *routes*
// the editor's off-tick read/write for a bound path UI-ward instead of fulfilling it
// against OPFS, exactly as a daemon session would route it over the wire — only here the
// "wire" is a postMessage to the UI thread and back.
//
// `boundPaths` is the set of editor paths that resolve to a real handle. The UI sends a
// `bind` (a ring frame under SAB, a message under 5c) *before* the `:e`/`:w` it issues, so
// the routing decision is already known when `eh_take_fs_requests` drains the request.
// Everything not bound stays on OPFS.
const boundPaths = new Set();
// Off-tick realfs ops dispatched to the UI and awaiting its reply. While this is > 0 the
// SAB run loop stays event-loop-live (it `await`s a JS promise rather than parking on
// `Atomics.wait`) so the UI's reply postMessage can actually be received — a thread parked
// in `Atomics.wait` can't process its message queue.
let pendingRealFs = 0;
let fsReplyWaiter = null; // resolve() to wake the SAB loop when a realfs reply lands
let sabMode = false; // true once the SAB run loop owns the tick (vs the 5c postMessage path)

// Hand the in-flight write `seq`'s snapshot bytes to the UI to write to the bound handle.
// Copy them out of wasm memory first (`slice`, not `subarray`) — the copy is detach-safe
// across `ALLOW_MEMORY_GROWTH` and can be transferred (zero-copy) to the UI thread.
function dispatchRealFsWrite(seq, path) {
  const ptr = eh_save_bytes(h, seq);
  const len = eh_save_len(h, seq);
  const bytes = ptr ? M.HEAPU8.slice(ptr, ptr + len) : new Uint8Array(0);
  pendingRealFs++;
  postMessage({ type: "fs_write", seq, path, bytes }, [bytes.buffer]);
}
// Ask the UI to read a bound path's real file (its handle is UI-side); the reply lands via
// the `fs_read_result` message.
function dispatchRealFsRead(buffer, path) {
  pendingRealFs++;
  postMessage({ type: "fs_read", buffer, path });
}
// A realfs reply (read content / write result) was just applied into the tick. Under SAB,
// wake the run loop (it reposts the frame and drains any cascade the completion enqueued);
// under 5c there's no loop, so drain + repaint inline.
async function landRealFsReply() {
  pendingRealFs = Math.max(0, pendingRealFs - 1);
  if (sabMode) {
    if (fsReplyWaiter) {
      const r = fsReplyWaiter;
      fsReplyWaiter = null;
      r();
    }
  } else {
    await fulfillFsRequests();
    postMessage(redrawMsg());
  }
}

// =============================================================================
// Off-tick OPFS filesystem (Phase 6 — serverless).
//
// The editor runs in off-tick fs mode (`has_remote_fs() == true`) because OPFS handle
// acquisition is asynchronous — only a `FileSystemSyncAccessHandle`'s *operations* are
// synchronous, so a synchronous `HostFs` read/write on the editor thread is impossible
// without Asyncify (which this build avoids). So `:e` / `:w` defer to a request the
// editor enqueues; the Worker fulfills it against OPFS *between* ticks (when it isn't
// parked, so the event loop runs and the OPFS promises resolve), exactly as a daemon
// session fulfills the same request over the wire. The OPFS analogue of the native
// `select!` arms (`apply_open` / `apply_save_done`).
// =============================================================================

// Path → OPFS handle. OPFS has one root directory (`getDirectory`); a path descends it
// component by component. Leading `/` and `.` segments are dropped (the editor's paths
// are absolute-looking but OPFS has no real root prefix).
const splitPath = (path) => String(path).split("/").filter((s) => s.length && s !== ".");

async function opfsDir(parts, create) {
  let dir = await navigator.storage.getDirectory();
  for (const p of parts) dir = await dir.getDirectoryHandle(p, { create });
  return dir;
}

// Enumerate OPFS directory handle `dh` → a sorted-agnostic JSON array the editor's
// explorer builder turns into a listing: [{ is_dir, name }, …]. The core listing builder
// sorts (dirs first, case-insensitive) and prepends `../`, so order here is irrelevant.
async function opfsDirEntries(dh) {
  const entries = [];
  for await (const [name, handle] of dh.entries()) {
    entries.push({ is_dir: handle.kind === "directory", name });
  }
  return entries;
}

// Read `path` from OPFS → { kind, text, path? }: kind 0 file (text = its UTF-8 contents),
// 1 a not-yet-existing path (new-file buffer), 2 a directory (text = its entries as JSON,
// `path` = the canonical dir for the explorer), 3 an error (text = message). A missing
// parent dir or missing file is "new" (the editor opens an empty buffer bound to the name,
// savable later) — not an error.
async function opfsRead(path) {
  const parts = splitPath(path);
  const canonicalDir = "/" + parts.join("/"); // "/" for the root
  // The OPFS root itself (`:e /`) is a directory — enumerate it directly.
  if (parts.length === 0) {
    try {
      const root = await navigator.storage.getDirectory();
      return { kind: 2, text: JSON.stringify(await opfsDirEntries(root)), path: "/" };
    } catch (e) {
      return { kind: 3, text: String(e) };
    }
  }
  const name = parts[parts.length - 1];
  let dir;
  try {
    dir = await opfsDir(parts.slice(0, -1), false);
  } catch (e) {
    return e.name === "NotFoundError" ? { kind: 1, text: "" } : { kind: 3, text: String(e) };
  }
  let fh;
  try {
    fh = await dir.getFileHandle(name, { create: false });
  } catch (e) {
    if (e.name === "NotFoundError") return { kind: 1, text: "" };
    if (e.name === "TypeMismatchError") {
      // It's a directory — enumerate it into an explorer listing.
      try {
        const dh = await dir.getDirectoryHandle(name, { create: false });
        return { kind: 2, text: JSON.stringify(await opfsDirEntries(dh)), path: canonicalDir };
      } catch (e2) {
        return { kind: 3, text: String(e2) };
      }
    }
    return { kind: 3, text: String(e) };
  }
  try {
    const ah = await fh.createSyncAccessHandle();
    const buf = new Uint8Array(ah.getSize());
    ah.read(buf, { at: 0 });
    ah.close();
    // Keep the RAW bytes — the encoding seam in Rust (`decode_to_rope`) detects the charset
    // and handles invalid UTF-8; decoding to text here (TextDecoder) would mangle them.
    return { kind: 0, bytes: buf };
  } catch (e) {
    return { kind: 3, text: String(e) };
  }
}

// Write `bytes` to `path` in OPFS (creating parent dirs), atomically truncating first →
// { ok, size, error }. The editor is the sole writer, so a plain truncate+write is the
// "atomic write" here (no concurrent reader to tear).
async function opfsWrite(path, bytes) {
  const parts = splitPath(path);
  if (parts.length === 0) return { ok: false, error: "empty path" };
  const name = parts[parts.length - 1];
  try {
    const dir = await opfsDir(parts.slice(0, -1), true);
    const fh = await dir.getFileHandle(name, { create: true });
    const ah = await fh.createSyncAccessHandle();
    ah.truncate(0);
    ah.write(bytes, { at: 0 });
    ah.flush();
    const size = ah.getSize();
    ah.close();
    return { ok: true, size };
  } catch (e) {
    return { ok: false, error: String(e) };
  }
}

// =============================================================================
// Serverless `nx.fs` over OPFS (Phase 3 of the off-tick plan). The JS twin of the daemon's
// `run_fs_job`: when no daemon is connected, a high-level `nx.fs.*` op runs against OPFS here
// (the same sandbox `:e`/`:w` use), producing the `["ok", <fs-value>] | ["err", code, message]`
// envelope the `fswire` codec decodes in Rust. OPFS has no synchronous path-based fs (handle
// acquisition is async), so this can't reuse `run_fs_job` — the op set is reimplemented in JS.
// OPFS divergences from POSIX, documented rather than faked: no symlinks (so `lstat` == `stat`,
// never a "link" kind), no `mode`/`ino`/`uid`/`gid`/`nlink`/`dev` (reported 0), and `mtime` is
// best-effort (a file's `lastModified`; a directory has none, reported nil). `realpath` just
// canonicalizes the path string (OPFS has no symlinks to resolve). Errors map DOMException names
// to libuv-style errno codes (`errCode`), so a reject's `err.code` matches the daemon path's.
// =============================================================================

// The tagged-array fs-value forms `fswire::fs_value_from_value` decodes (kept in lock-step with
// the Rust encoder). Bytes ride as a `Uint8Array` (re-encoded to msgpack `bin` by `landFsOpResult`).
const fsNil = () => ["nil"];
const fsBool = (b) => ["bool", !!b];
const fsBytes = (u8) => ["bytes", u8];
const fsText = (s) => ["text", String(s)];
const fsStat = (arr) => ["stat", arr];
const fsDir = (rows) => ["dir", rows];

// Join an editor-style path with a child component (paths are absolute-looking; OPFS strips the
// leading `/`). Used by recursive copy.
const joinPath = (a, b) => `${String(a).replace(/\/+$/, "")}/${b}`;

// Map a thrown error to a libuv-style errno code for the reject envelope. Our helpers throw
// `Error`s carrying an explicit `.code` for the semantic cases (EISDIR / ENOTEMPTY / EILSEQ / …);
// a raw OPFS `DOMException` is mapped by name. Anything unrecognized is `EIO` (never silent).
function errCode(e) {
  if (e && e.code) return e.code;
  switch (e && e.name) {
    case "NotFoundError": return "ENOENT";
    case "TypeMismatchError": return "ENOTDIR";
    case "InvalidModificationError": return "ENOTEMPTY";
    case "NoModificationAllowedError": return "EACCES";
    case "QuotaExceededError": return "ENOSPC";
    default: return "EIO";
  }
}

// An error carrying an explicit errno code (for the cases OPFS doesn't signal by DOMException name).
function fsErr(code, message) {
  const e = new Error(message);
  e.code = code;
  return e;
}

// Resolve a path's parent directory handle + final component name. `create` makes the parents.
// An empty path (the OPFS root) has no parent — it's a directory, so a file op on it is EISDIR.
async function opfsParent(path, create) {
  const parts = splitPath(path);
  if (parts.length === 0) throw fsErr("EISDIR", `'${path}' is a directory`);
  const dir = await opfsDir(parts.slice(0, -1), !!create); // missing parent → NotFoundError → ENOENT
  return { dir, name: parts[parts.length - 1] };
}

// Resolve a path to its file OR directory handle (the OPFS root is a directory). ENOENT if absent.
async function opfsResolveHandle(path) {
  const parts = splitPath(path);
  if (parts.length === 0) return await navigator.storage.getDirectory();
  const dir = await opfsDir(parts.slice(0, -1), false);
  const name = parts[parts.length - 1];
  try {
    return await dir.getFileHandle(name, { create: false });
  } catch (e) {
    if (e.name !== "NotFoundError" && e.name !== "TypeMismatchError") throw e;
  }
  return await dir.getDirectoryHandle(name, { create: false }); // missing → NotFoundError → ENOENT
}

// Build the positional stat array `fswire::decode_stat` reads: kind string, then size/mode and
// the `(secs, nsecs)` mtime/atime (nil secs = unknown), then the unix `st_*` extras (0 on OPFS).
function statArr(kind, size, mtimeMs) {
  let secs = null;
  let nsecs = 0;
  if (mtimeMs >= 0) {
    secs = Math.floor(mtimeMs / 1000);
    nsecs = Math.floor((mtimeMs % 1000) * 1e6);
  }
  // atime mirrors mtime (OPFS exposes only lastModified).
  return [kind, size, 0, secs, nsecs, secs, nsecs, 0, 0, 0, 0, 0];
}

async function opfsStat(path) {
  const parts = splitPath(path);
  if (parts.length === 0) return statArr("directory", 0, -1); // the root
  const dir = await opfsDir(parts.slice(0, -1), false); // missing parent → ENOENT
  const name = parts[parts.length - 1];
  try {
    const fh = await dir.getFileHandle(name, { create: false });
    const file = await fh.getFile();
    return statArr("file", file.size, file.lastModified);
  } catch (e) {
    if (e.name !== "NotFoundError" && e.name !== "TypeMismatchError") throw e;
    // Not a file — try a directory (a real ENOENT rethrows from here).
    await dir.getDirectoryHandle(name, { create: false });
    return statArr("directory", 0, -1);
  }
}

// Read a file's raw bytes. A directory is EISDIR, a missing entry ENOENT (mapped from the OPFS
// DOMExceptions, which don't distinguish the two on `getFileHandle`).
async function opfsReadBytes(path) {
  const { dir, name } = await opfsParent(path);
  let fh;
  try {
    fh = await dir.getFileHandle(name, { create: false });
  } catch (e) {
    if (e.name === "TypeMismatchError") throw fsErr("EISDIR", `'${path}' is a directory`);
    if (e.name === "NotFoundError") throw fsErr("ENOENT", `no such file '${path}'`);
    throw e;
  }
  const ah = await fh.createSyncAccessHandle();
  try {
    const buf = new Uint8Array(ah.getSize());
    ah.read(buf, { at: 0 });
    return buf;
  } finally {
    ah.close();
  }
}

// Decode a file's bytes through `encoding` (default UTF-8), failing loud like the daemon's
// `run_fs_job`: an unknown label is EINVAL, invalid bytes EILSEQ (TextDecoder `fatal`) — never
// lossy replacement text (use `nx.fs.read` for raw bytes).
async function opfsReadText(path, encoding) {
  const bytes = await opfsReadBytes(path);
  const label = encoding || "utf-8";
  let dec;
  try {
    dec = new TextDecoder(label, { fatal: true });
  } catch {
    throw fsErr("EINVAL", `unknown encoding '${label}'`);
  }
  try {
    return dec.decode(bytes);
  } catch {
    throw fsErr("EILSEQ", `invalid ${label} byte sequence in '${path}'`);
  }
}

// Write (truncate) or append `data` (a Uint8Array) to a file, creating parent dirs.
async function opfsWriteBytes(path, data, append) {
  const { dir, name } = await opfsParent(path, true);
  const fh = await dir.getFileHandle(name, { create: true });
  const ah = await fh.createSyncAccessHandle();
  try {
    if (append) {
      ah.write(data, { at: ah.getSize() });
    } else {
      ah.truncate(0);
      ah.write(data, { at: 0 });
    }
    ah.flush();
  } finally {
    ah.close();
  }
}

// readdir → the `[[kind, name], …]` rows `fswire` decodes (kind is the libuv string).
async function opfsReaddir(path) {
  const parts = splitPath(path);
  const dh = parts.length === 0
    ? await navigator.storage.getDirectory()
    : await opfsDir(parts, false); // a file component → TypeMismatchError → ENOTDIR; missing → ENOENT
  const rows = [];
  for await (const [name, handle] of dh.entries()) {
    rows.push([handle.kind === "directory" ? "directory" : "file", name]);
  }
  return rows;
}

// mkdir: recursive creates all parents (idempotent, like `mkdir -p`); non-recursive requires the
// parent to exist (else ENOENT) and the target to be absent (else EEXIST — OPFS itself is
// idempotent, so we check first to surface the POSIX error).
async function opfsMkdir(path, recursive) {
  const parts = splitPath(path);
  if (parts.length === 0) throw fsErr("EEXIST", "'/' exists");
  if (recursive) {
    await opfsDir(parts, true);
    return;
  }
  const parent = await opfsDir(parts.slice(0, -1), false); // missing parent → ENOENT
  const name = parts[parts.length - 1];
  let exists = false;
  try {
    await parent.getDirectoryHandle(name, { create: false });
    exists = true;
  } catch {
    try {
      await parent.getFileHandle(name, { create: false });
      exists = true;
    } catch {
      // genuinely absent
    }
  }
  if (exists) throw fsErr("EEXIST", `'${path}' exists`);
  await parent.getDirectoryHandle(name, { create: true });
}

// rename via FileSystemHandle.move (Chromium); falls back to copy+remove for a file where move
// is unavailable. The destination's parent dirs are created.
async function opfsRename(from, to) {
  const handle = await opfsResolveHandle(from); // missing → ENOENT
  const { dir: toDir, name: toName } = await opfsParent(to, true);
  if (typeof handle.move === "function") {
    await handle.move(toDir, toName);
    return;
  }
  if (handle.kind === "file") {
    await opfsWriteBytes(to, await opfsReadBytes(from), false);
    const { dir, name } = await opfsParent(from);
    await dir.removeEntry(name);
    return;
  }
  throw fsErr("ENOSYS", "rename of a directory needs FileSystemHandle.move (unavailable here)");
}

// remove: a file is unlinked, a directory removed (emptied first when recursive — else a
// non-empty dir is ENOTEMPTY, mapped from OPFS's InvalidModificationError).
async function opfsRemove(path, recursive) {
  const { dir, name } = await opfsParent(path); // root → EISDIR (can't remove the root)
  await dir.removeEntry(name, { recursive: !!recursive }); // missing → ENOENT
}

// copy: a file via read+write (overwriting); a directory tree when recursive (else EINVAL, like
// the daemon's `copy_path`).
async function opfsCopy(src, dst, recursive) {
  const st = await opfsStat(src); // missing → ENOENT
  if (st[0] === "directory") {
    if (!recursive) throw fsErr("EINVAL", `'${src}' is a directory (pass { recursive = true })`);
    await opfsMkdir(dst, true);
    for (const [, name] of await opfsReaddir(src)) {
      await opfsCopy(joinPath(src, name), joinPath(dst, name), true);
    }
    return;
  }
  await opfsWriteBytes(dst, await opfsReadBytes(src), false);
}

// Run one high-level `nx.fs` op against OPFS, returning the `["ok"|"err", …]` reply envelope.
// The serverless twin of the daemon's `serve_fs_op` → `run_fs_job`.
async function opfsFsOp(req) {
  try {
    switch (req.op) {
      // OPFS has no symlinks, so lstat is stat (never a "link" kind).
      case "stat":
      case "lstat":
        return ["ok", fsStat(await opfsStat(req.path))];
      case "exists":
        try {
          await opfsStat(req.path);
          return ["ok", fsBool(true)];
        } catch {
          return ["ok", fsBool(false)]; // the one op that never rejects
        }
      case "readdir":
        return ["ok", fsDir(await opfsReaddir(req.path))];
      case "read":
        return ["ok", fsBytes(await opfsReadBytes(req.path))];
      case "read_text":
        return ["ok", fsText(await opfsReadText(req.path, req.encoding))];
      case "write":
        await opfsWriteBytes(req.path, req.data || new Uint8Array(0), false);
        return ["ok", fsNil()];
      case "append":
        await opfsWriteBytes(req.path, req.data || new Uint8Array(0), true);
        return ["ok", fsNil()];
      case "mkdir":
        await opfsMkdir(req.path, !!req.recursive);
        return ["ok", fsNil()];
      case "rename":
        await opfsRename(req.from, req.to);
        return ["ok", fsNil()];
      case "remove":
        await opfsRemove(req.path, !!req.recursive);
        return ["ok", fsNil()];
      case "copy":
        await opfsCopy(req.src, req.dst, !!req.recursive);
        return ["ok", fsNil()];
      case "realpath":
        return ["ok", fsText("/" + splitPath(req.path).join("/"))];
      default:
        return ["err", "EINVAL", `unknown nx.fs op '${req.op}'`];
    }
  } catch (e) {
    return ["err", errCode(e), String(e && e.message ? e.message : e)];
  }
}

// =============================================================================
// Off-tick daemon filesystem (Phase 6b) — the WebTransport/QUIC remote.
//
// When the page is opened with `?daemon=nxvim://HOST:PORT/TOKEN?cert=HASH`, the same
// off-tick fs seam that OPFS satisfies serverless-ly is instead fulfilled over a real
// WebTransport connection to a remote `nxvim --daemon --listen` (rpc.mjs). The editor is
// unchanged — `has_remote_fs()` is already true, so `:e`/`:w` defer the same way; only the
// transport that answers `eh_take_fs_requests` differs. This is the browser twin of the
// native `connect_quic` fs leg (Phase 3d/3e): editing stays in the Worker, only fs crosses
// the wire. The watch (Phase 6c), async-proc (6d), and terminal (Phase 7) legs ride this
// same `RpcClient` too; lsp/sys_run/luafs are later slices. Config + shada stay LOCAL
// (OPFS) even in daemon mode — the thesis.
//
// `daemonUri` rides the Worker's own URL (`?daemon=…`), so the Worker self-configures with
// no boot-message race. In daemon mode fs NEVER silently falls back to OPFS: a dial failure
// surfaces loudly and every fs request errors with that reason (CLAUDE.md "fail loud").
// =============================================================================
// `let`, not `const`: the boot value is the `?daemon=` page param (if any), but a runtime
// `:connect nxvim://…` (the browser twin of nxvim-gui's `:connect`) re-points it after boot —
// see `runtimeConnect`. Its truthiness is what switches the off-tick fs/watch seam from the
// serverless OPFS sandbox onto the wire (`fulfillFsRequests` / `drainWatchRequests`).
let daemonUri = new URL(self.location.href).searchParams.get("daemon");
let daemon = null; // an RpcClient once connected
let daemonError = null; // a dial failure reason (daemon mode requested but not connected)

// The local in-browser process host, installed only in the python-demo build (build-config
// `localHost: true`) and only when serverless — see the boot tail. `null` in the standard
// editor build (and in daemon mode), where processes ride the wire or fail loud. When set, it
// fulfils the proc / terminal / LSP off-tick seams locally (Pyodide); see web/local-host.mjs.
let localHost = null;

async function connectDaemon() {
  if (!daemonUri) return;
  try {
    daemon = await dialDaemon(daemonUri);
    // The watch leg's `fs_changed` pushes and the proc leg's `proc_spawned`/`proc_exited`
    // pushes arrive here.
    daemon.onNotify = onDaemonNotify;
    // A process host is now reachable: let the editor tick take its async-spawn branch
    // (`vim.system` / `jobstart`) instead of failing loud.
    eh_set_proc_host(h, 1);
  } catch (e) {
    daemonError = String(e && e.message ? e.message : e);
    postMessage({ type: "config_error", error: `daemon connection failed: ${daemonError}` });
  }
}

// Runtime `:connect nxvim://…`: dial a daemon *after* boot and re-point the off-tick fs/watch
// seam from OPFS onto the wire — the browser twin of nxvim-gui's client-side `:connect`, which
// the editor core knows nothing about (the UI intercepts it on `<CR>` and routes the URI here).
// Replaces any existing link (a prior `?daemon=` boot or an earlier `:connect`): the old wire
// is torn down, its watches forgotten (the new daemon knows none of them), and future `:e`/`:w`
// route to the new daemon. Posts a `connected` status the UI flashes. Config + shada stay LOCAL
// (OPFS) regardless, exactly as in `?daemon=` mode.
async function runtimeConnect(uri) {
  if (daemon) {
    daemon.close();
    daemon = null;
  }
  // The old wire is gone: no process host until the new dial succeeds, and the new daemon
  // knows none of the old watches or in-flight children.
  eh_set_proc_host(h, 0);
  armedWatches.clear();
  liveProcs.clear();
  liveLsp.clear();
  daemonError = null;
  daemonUri = uri;
  await connectDaemon();
  postMessage({ type: "connected", ok: !!daemon, uri, error: daemonError });
}

// Fulfil one off-tick read over the daemon, projecting `fs_read`'s reply onto the same
// `{ kind, bytes?, text?, path? }` shape `opfsRead` returns (so the applier path is
// identical): a file carries raw `bytes`, a dir/new/error carries `text`.
async function daemonRead(path) {
  if (!daemon) return { kind: 3, text: daemonError || "daemon not connected" };
  try {
    const reply = await daemon.request("fs_read", [path]);
    const tag = Array.isArray(reply) ? reply[0] : null;
    // Keep the RAW bytes (`reply[1]` is a Uint8Array off the msgpack bin) for the encoding
    // seam — don't `utf8()`-decode here, that would mangle non-UTF-8 content.
    if (tag === "file") return { kind: 0, bytes: reply[1] };
    if (tag === "new") return { kind: 1, text: "" };
    if (tag === "dir") {
      // Daemon entries are `[[is_dir, name], …]`; the wasm dir applier wants `[{is_dir,name}]`.
      const entries = (reply[2] || []).map(([is_dir, name]) => ({ is_dir, name }));
      return { kind: 2, text: JSON.stringify(entries), path: reply[1] };
    }
    return { kind: 3, text: `unexpected fs_read reply: ${JSON.stringify(reply)}` };
  } catch (e) {
    return { kind: 3, text: String(e && e.message ? e.message : e) };
  }
}

// Fulfil one off-tick write over the daemon, projecting `fs_write`'s `["ok", stat?]` reply
// onto the `{ ok, size, mtimeMs, error }` the write applier needs. An RPC error rejects →
// a loud `{ ok: false }` (the editor's saved-state clears only on a real ack).
async function daemonWrite(path, bytes) {
  if (!daemon) return { ok: false, error: daemonError || "daemon not connected" };
  try {
    const reply = await daemon.request("fs_write", [path, bytes]);
    const stat = Array.isArray(reply) ? reply[1] : null; // [secs, nanos, size] | null
    if (Array.isArray(stat)) {
      const [secs, nanos, size] = stat;
      const mtimeMs = secs != null ? secs * 1000 + Math.floor((nanos || 0) / 1e6) : -1;
      return { ok: true, size: size ?? bytes.length, mtimeMs };
    }
    return { ok: true, size: bytes.length, mtimeMs: -1 };
  } catch (e) {
    return { ok: false, error: String(e && e.message ? e.message : e) };
  }
}

// =============================================================================
// Off-tick daemon watch leg (Phase 6 — the `HostWatch` push direction).
//
// The fs leg (above) is request/response, always initiated by the Worker during a tick. The
// watch leg adds the *other* direction: the daemon pushes `fs_changed [path, stat?]` whenever
// a watched file changes on its disk, and the editor reconciles it (autoreload / W11 / W12 /
// `FileChangedShell`) — the browser twin of the native `watch_rx` arm. Two halves:
//   * outbound — the editor arms one watch per file-backed buffer off-tick (`fs_watch` /
//     `fs_unwatch` effects → `eh_take_watch_requests`); the Worker forwards each to the daemon.
//   * inbound — a `fs_changed` push arrives on `RpcClient.onNotify`; we queue it and apply it
//     on the run loop (`eh_remote_file_changed`), which may enqueue an off-tick reload the fs
//     leg then re-fetches over the wire.
//
// The crux is *receiving* a push: a thread parked in blocking `Atomics.wait` freezes the
// WebTransport reader, so an unsolicited push would sit until the next keystroke. So in a
// daemon session with watches armed the run loop parks on `Atomics.waitAsync` instead (it
// stays event-loop-live, the reader delivers pushes), waking immediately on a push via the
// `daemonWake()` race or on input via the SEQ notify. Serverless OPFS keeps the (cheaper)
// blocking park — it has no pushes. Pushes are queued and applied on the run loop only, so the
// wasm tick has a single consumer (no reentrancy with the loop's own drain).
// =============================================================================
const armedWatches = new Set(); // paths currently watched on the daemon (gates the async park)
const liveProcs = new Set(); // spawn ids in flight on the daemon (also gates the async park)
const liveTerms = new Set(); // terminal buffer ids open on the daemon (also gates the async park)
const liveFsWatches = new Set(); // nx.fs.watch stream ids armed on the daemon (also gates the async park)
const liveLsp = new Set(); // LSP server wire ids spawned on the daemon (also gates the async park)
const daemonNotifications = []; // [method, params] pushes received but not yet applied
let daemonWaker = null; // resolve() to wake the async park the instant a push arrives
let sabCtrl = null; // the SAB control array (set by runLoopSAB) so a push can wake the futex park
let pendingConnectUri = null; // a runtime `:connect nxvim://…` to dial on the next run-loop pass
const DAEMON_PUSH_POLL_MS = 1000; // async-park cap: clears a dangling waitAsync + backstops a push
const INDENT_LOAD_POLL_MS = 25;   // async-park cap while the indenter loads a grammar (resolve its promises promptly)
// Backpressure window for daemon pushes the apply side hasn't consumed yet. When the queue
// reaches HIGH the RPC reader is parked (see `onDaemonNotify`) so it stops pulling the
// WebTransport stream → QUIC backpressures the daemon → the terminal child is throttled at the
// PTY (the browser end of the end-to-end terminal backpressure). Released once the run loop
// drains it below LOW. Without this the browser would pull a flood into an unbounded queue
// faster than it can render, the daemon would never feel backpressure, and a `^C` couldn't stop
// the output. Sized well above a normal burst so steady output never parks the reader.
const NOTIF_HIGH = 64;
const NOTIF_LOW = 16;
let backpressureWaiter = null; // resolve() to un-park the RPC reader once the queue drains below LOW

// Terminal projection (the `eh_terminal_flush` that mirrors the vt100 grid into the buffer +
// repaints) is `O(scrollback)` per call, so doing it once per PTY chunk makes a flood crawl —
// and a `^C` then can't drain the in-flight backlog fast. Decouple it from the cheap feed:
// `term_data` only *feeds* the emulator (fast); the project/repaint is throttled to ~frame rate
// (`maybeFlushTerminals`), and a trailing flush fires when output pauses. So the consumer keeps
// up with the child (no backlog builds) and the post-`^C` backlog drains in a couple of frames.
const TERM_FLUSH_MS = 16;
let termFlushDue = false; // a `term_data` feed needs projecting; deferred until the throttle allows
let lastTermFlushMs = 0;

// ^C-cancel of a flooding terminal. The trim ([`terminal_trim`]) keeps the recent tail + a
// marker, but the in-flight backlog the daemon already sent (bounded by the browser's QUIC
// receive window, which auto-tunes to ~seconds of output and can't be shrunk from our side)
// would keep arriving and bury it. So on a `^C` to a terminal that's actively flooding we
// *discard* that backlog — drop the queued `term_data` until output goes quiet — leaving the
// trimmed tail. The reader keeps draining the wire at full speed (drop is cheap), so the cancel
// takes hold in a beat instead of the ~seconds it takes to render the whole window.
const DISCARD_QUIET_MS = 250; // end the discard once no term_data has arrived for this long
const discardingTerms = new Set(); // bufs whose in-flight backlog is being dropped after a ^C
let lastDiscardActivityMs = 0;
let discardCheckScheduled = false;

// Poll for the end of a discard window: once term_data stops arriving for `DISCARD_QUIET_MS`
// (the backlog has drained and the killed child is silent), stop discarding and wake the run
// loop to repaint the settled (trimmed) buffer.
function scheduleDiscardCheck() {
  if (discardCheckScheduled) return;
  discardCheckScheduled = true;
  const check = () => {
    if (discardingTerms.size === 0) { discardCheckScheduled = false; return; }
    if (nowMs() - lastDiscardActivityMs >= DISCARD_QUIET_MS) {
      discardingTerms.clear();
      discardCheckScheduled = false;
      if (sabCtrl) { Atomics.add(sabCtrl, SEQ, 1); Atomics.notify(sabCtrl, SEQ, 1); }
    } else {
      setTimeout(check, DISCARD_QUIET_MS);
    }
  };
  setTimeout(check, DISCARD_QUIET_MS);
}

// Project + repaint all live terminals if a feed is pending and either forced (a non-`term_data`
// event needs an immediate frame) or the frame-rate throttle has elapsed. Returns whether it flushed.
function maybeFlushTerminals(now, force) {
  if (!termFlushDue) return false;
  if (!force && now - lastTermFlushMs < TERM_FLUSH_MS) return false;
  eh_terminal_flush(h);
  lastTermFlushMs = now;
  termFlushDue = false;
  return true;
}

// A daemon→edit-host push landed on the RPC reader. Queue it (the run loop applies it — single
// consumer of the wasm tick) and wake the loop. Under 5c (no run loop) apply it inline. Returns
// a promise when the queue is over HIGH so the RPC reader parks (backpressure); else undefined.
function onDaemonNotify(method, params) {
  // Draining a cancelled flood: drop this terminal's in-flight backlog (the ^C already trimmed
  // the buffer to the tail + marker). Drop = no queue, no feed, no repaint, no backpressure —
  // so the reader keeps pulling the wire at full speed and the window drains fast.
  if (method === "term_data" && discardingTerms.has(Number(params[0]))) {
    lastDiscardActivityMs = nowMs();
    return undefined;
  }
  // Wake only on the empty→non-empty transition. The run loop never parks with a non-empty
  // queue (`if (daemonNotifications.length > 0) continue`), so once it's draining, further
  // pushes don't need a wake — they're picked up by the same drain. Gating on `wasEmpty`
  // coalesces a flood into a few big batches (one `eh_terminal_flush` each, like the native
  // 256 KiB budget) instead of one flush per PTY chunk, while a lone echo (queue was empty)
  // still wakes instantly — so typing stays snappy AND a flood drains cheaply.
  const wasEmpty = daemonNotifications.length === 0;
  daemonNotifications.push([method, params]);
  if (sabMode) {
    // Wake the run loop's async park. `daemonWaker` is a fast microtask path, but it races —
    // a push can land in a window where the resolver being awaited isn't the current one — so
    // ALSO bump the SEQ futex the park's `Atomics.waitAsync` watches (the same reliable wake UI
    // input uses). Without this backstop an *isolated* push slept until the poll cap (~1s): a
    // stream of pushes hid the bug by re-waking each other, but a one-shot echo (a single
    // keystroke into a terminal) did not — that was the typing lag.
    if (wasEmpty) {
      if (daemonWaker) { const r = daemonWaker; daemonWaker = null; r(); }
      if (sabCtrl) { Atomics.add(sabCtrl, SEQ, 1); Atomics.notify(sabCtrl, SEQ, 1); }
    }
  } else {
    pump5cDaemon();
  }
  // Apply side falling behind: park the RPC reader until the run loop drains below LOW.
  if (daemonNotifications.length >= NOTIF_HIGH && !backpressureWaiter) {
    return new Promise((res) => { backpressureWaiter = res; });
  }
  return undefined;
}

// Release a parked RPC reader once the queue has drained below the low-water mark. Called after
// the run loop applies pushes, so the reader resumes pulling the stream (lifting QUIC backpressure).
function releaseBackpressure() {
  if (backpressureWaiter && daemonNotifications.length < NOTIF_LOW) {
    const r = backpressureWaiter;
    backpressureWaiter = null;
    r();
  }
}

// Apply every queued daemon push through the real tick (run-loop side): the watch leg's
// `fs_changed`, the proc leg's `proc_spawned`/`proc_exited` (Phase 6d), and the terminal leg's
// `term_data`/`term_exit` (Phase 7). An unknown push is surfaced loudly (it can only arrive for
// something we subscribed to — fail loud, per CLAUDE.md).
//
// Terminal output is doubly batched: each `term_data` push only *feeds* the vt100 emulator (a
// cheap parse, `eh_terminal_data`) and marks a flush pending; the `O(scrollback)` project +
// repaint is throttled to ~frame rate by `maybeFlushTerminals` (called by the run loop), never
// per PTY chunk. That keeps the consumer fast enough to stay with the child (so no backlog
// builds and a `^C` drains in a couple of frames) — the wire-crossing twin of the native leg's
// 256 KiB budget. `any` (return) excludes bare `term_data` so the run loop doesn't post a
// redraw for a feed whose projection is still pending — the flush posts its own.
function applyDaemonNotifications() {
  if (daemonNotifications.length === 0) return false;
  let any = false;
  for (let n; (n = daemonNotifications.shift()); ) {
    const [method, params] = n;
    if (method === "term_data") {
      // params = [buf, bytes(bin)] — the child's raw PTY output. Feed only; project later.
      const buf = Number(params[0]);
      const bytes = toU8(params[1]);
      callTerminalData(buf, bytes);
      termFlushDue = true;
    } else if (method === "term_exit") {
      // params = [buf, code] — the child exited (a killed child arrives as code -1).
      const buf = Number(params[0]);
      const code = Number(params[1]);
      eh_terminal_exit(h, buf, code);
      liveTerms.delete(params[0]);
      termFlushDue = true; // the `[Process exited]` notice needs projecting
      any = true; // exit forces an immediate flush + frame (see the run loop)
    } else if (method === "fs_changed") {
      // params = [path, stat?] where stat = [secs, nanos, size] | nil (nil = vanished).
      const path = params[0];
      const stat = params[1];
      const hasStat = Array.isArray(stat) ? 1 : 0;
      const size = hasStat ? (stat[2] ?? 0) : 0;
      const mtimeMs = hasStat ? (stat[0] ?? 0) * 1000 + Math.floor((stat[1] ?? 0) / 1e6) : -1;
      eh_remote_file_changed(h, String(path), hasStat, size, mtimeMs);
      any = true;
    } else if (method === "proc_spawned") {
      // params = [id, pid?] — nil pid = the child failed to spawn (passed as -1).
      const id = params[0];
      const pid = params[1];
      eh_proc_spawned(h, Number(id), pid == null ? -1 : Number(pid));
      any = true;
    } else if (method === "proc_stdout") {
      // params = [id, lines(array of str)] — a streaming child's stdout batch
      // (`nx.run_stream`). Hand the lines to the Lua callback as JSON.
      const id = params[0];
      const lines = Array.isArray(params[1]) ? params[1].map(String) : [];
      eh_proc_stdout(h, Number(id), JSON.stringify(lines));
      any = true;
    } else if (method === "proc_exited") {
      // params = [id, code, stdout(bin), stderr(bin)] — a killed child arrives as code -1.
      const id = params[0];
      const code = params[1];
      callProcExited(Number(id), Number(code), toU8(params[2]), toU8(params[3]));
      liveProcs.delete(id);
      any = true;
    } else if (method === "luafs_change") {
      // params = [id, kind, [path, …]] — a coalesced nx.fs.watch change batch (Phase 3b).
      const id = params[0];
      const kind = String(params[1] ?? "modify");
      const paths = Array.isArray(params[2]) ? params[2].map(String) : [];
      eh_fs_watch_change(h, Number(id), kind, JSON.stringify(paths));
      any = true;
    } else if (method === "luafs_watch_err") {
      // params = [id, message] — the watch failed (bad path / limit) or its backend errored.
      const id = params[0];
      eh_fs_watch_err(h, Number(id), String(params[1] ?? "watch error"));
      liveFsWatches.delete(id);
      any = true;
    } else if (method === "lsp_stdout") {
      // params = [id, bytes(bin)] — a framed JSON-RPC chunk from the server (wire id). Feed it
      // into the SyncLspClient, which parses complete frames and emits events + outbound ops.
      callLspStdout(Number(params[0]), toU8(params[1]));
      any = true;
    } else if (method === "lsp_stderr") {
      // params = [id, bytes(bin)] — the server's diagnostic output. The client drops it (no
      // browser log file), but feed it so the wire method has a sink rather than being ignored.
      callLspStderr(Number(params[0]), toU8(params[1]));
    } else if (method === "lsp_exited") {
      // params = [id, code?, signal?] — the server exited / its pipe closed. A nil code/signal
      // means "not collected" (passed as -1, the proc-leg convention); the client surfaces a
      // ServerExited and forgets it (the editor re-ensures on the next FileType).
      const id = params[0];
      const code = params[1];
      const signal = params[2];
      eh_lsp_exited(h, Number(id), code == null ? -1 : Number(code), signal == null ? -1 : Number(signal));
      liveLsp.delete(id);
      any = true;
    } else {
      postMessage({ type: "config_error", error: `unhandled daemon push: ${method}` });
    }
  }
  // The terminal projection is deferred to `maybeFlushTerminals` (throttled); see above.
  // Drained: if the RPC reader parked on backpressure, let it resume pulling the stream.
  releaseBackpressure();
  return any;
}

// Land an async host push onto the run loop's notification queue and wake its park — the local
// host's twin of `onDaemonNotify`'s push+wake (a one-shot push must bump the SEQ futex the async
// park watches, not just the microtask waker, or an isolated `term_exit` could sleep to the poll
// cap; see the daemon-push wake-race note above and in CLAUDE.md/memory). Generic infra: the
// demo's local host (web/local-host.mjs) receives the Pyodide Worker's `data`/`exit` and calls
// this so they reuse the daemon leg's `term_data`/`term_exit` landing. Only wired in the demo
// build (passed into `installLocalHost`); inert in the standard build.
function landHostPush(method, params) {
  const wasEmpty = daemonNotifications.length === 0;
  daemonNotifications.push([method, params]);
  if (sabMode) {
    if (wasEmpty) {
      if (daemonWaker) { const r = daemonWaker; daemonWaker = null; r(); }
      if (sabCtrl) { Atomics.add(sabCtrl, SEQ, 1); Atomics.notify(sabCtrl, SEQ, 1); }
    }
  } else {
    pump5cDaemon();
  }
}

// Feed a `term_data` push's bytes into the vt100 emulator. The bytes are copied into wasm
// memory (PTY output is binary — NULs / invalid UTF-8 can't ride the cwrap "string"
// marshalling) and `eh_terminal_data` feeds them; free after the call. Feed only — the caller
// flushes once per drain.
function callTerminalData(buf, bytes) {
  const ptr = bytes.length ? M._malloc(bytes.length) : 0;
  if (ptr) M.HEAPU8.set(bytes, ptr);
  eh_terminal_data(h, buf, ptr, bytes.length);
  if (ptr) M._free(ptr);
}

// Normalize a msgpack value to bytes: `bin` decodes to a Uint8Array, but guard against a
// `str`/array/nil shape too so a daemon quirk degrades to bytes rather than throwing.
function toU8(v) {
  if (v == null) return new Uint8Array(0);
  if (v instanceof Uint8Array) return v;
  if (ArrayBuffer.isView(v)) return new Uint8Array(v.buffer, v.byteOffset, v.byteLength);
  if (Array.isArray(v)) return new Uint8Array(v);
  if (typeof v === "string") return new TextEncoder().encode(v);
  return new Uint8Array(0);
}

// Run a child's `on_exit` with its raw stdout/stderr bytes. The bytes are copied into wasm
// memory (process output is binary — it can't ride the cwrap "string" marshalling, which
// stops at a NUL) and `eh_proc_exited` hands them to the Lua callback; free after the call.
function callProcExited(id, code, stdout, stderr) {
  const oPtr = stdout.length ? M._malloc(stdout.length) : 0;
  const ePtr = stderr.length ? M._malloc(stderr.length) : 0;
  if (oPtr) M.HEAPU8.set(stdout, oPtr);
  if (ePtr) M.HEAPU8.set(stderr, ePtr);
  eh_proc_exited(h, id, code, oPtr, stdout.length, ePtr, stderr.length);
  if (oPtr) M._free(oPtr);
  if (ePtr) M._free(ePtr);
}

// Feed a `lsp_stdout` push's bytes into the SyncLspClient. The bytes are copied into wasm
// memory (LSP framing is UTF-8 but a payload may carry NULs — it can't ride the cwrap
// "string" marshalling) and `eh_lsp_stdout` parses every complete frame; free after the call.
// The feed can complete a handshake / answer a config pull, leaving fresh wire ops the run
// loop drains with `drainLspRequests` right after applying pushes.
function callLspStdout(id, bytes) {
  const ptr = bytes.length ? M._malloc(bytes.length) : 0;
  if (ptr) M.HEAPU8.set(bytes, ptr);
  eh_lsp_stdout(h, id, ptr, bytes.length);
  if (ptr) M._free(ptr);
}

// Feed a `lsp_stderr` push's bytes (the server's diagnostic output) into the SyncLspClient,
// which drops them (no browser log file). Bytes copied into wasm memory like `callLspStdout`.
function callLspStderr(id, bytes) {
  const ptr = bytes.length ? M._malloc(bytes.length) : 0;
  if (ptr) M.HEAPU8.set(bytes, ptr);
  eh_lsp_stderr(h, id, ptr, bytes.length);
  if (ptr) M._free(ptr);
}

// Forward the async process spawns/kills the tick enqueued to the daemon (`proc_spawn` /
// `proc_kill`). The daemon answers with `proc_spawned`/`proc_exited` pushes (`onDaemonNotify`).
// Only reached in a daemon session — the tick gates proc spawns on a connected daemon
// (`has_remote_proc`), so a serverless `vim.system` fails loud in the core and never enqueues.
async function drainProcRequests() {
  const reqs = JSON.parse(readStr(eh_take_proc_requests(h)));
  if (reqs.spawn.length === 0 && reqs.kill.length === 0) return;
  if (!daemon) {
    // Serverless: route to the local host (demo build) if present — it fails loud for legs it
    // hasn't wired yet. Standard build: no local host, no enqueued spawns (the gate is closed).
    if (localHost) localHost.proc(reqs);
    return;
  }
  for (const s of reqs.spawn) {
    liveProcs.add(s.id);
    // The 6th param is the stream flag (the daemon's `decode_spawn` reads it):
    // a streaming spawn (`nx.run_stream`) gets `proc_stdout` batches back.
    await daemon.notify("proc_spawn", [
      s.id, s.argv, s.cwd ?? null, s.env, new Uint8Array(s.stdin), s.stream === true,
    ]);
  }
  for (const id of reqs.kill) await daemon.notify("proc_kill", [id]);
}

// Forward the off-tick `nx.fs` ops the tick enqueued, routed to the daemon when connected (the
// `luafs_op` leg — Phase 2) else to OPFS (Phase 3, serverless) — the same daemon-or-OPFS split
// `fulfillFsRequests` uses for `:e`/`:w`. Each op produces the `["ok", <fs-value>] | ["err",
// code, message]` envelope, which `landFsOpResult` re-encodes to msgpack and lands via
// `eh_fs_op_result` (resolving the op's promise). Drained sequentially within the tick (we're
// not parked, so the awaits resolve), looping until dry — landing a result can fire a chained
// `nx.fs` in the promise continuation. Returns whether any op was handled (so the caller
// repaints / marks shada dirty).
async function drainFsOpRequests() {
  let didWork = false;
  for (;;) {
    const ops = JSON.parse(readStr(eh_take_fs_op_requests(h)));
    if (ops.length === 0) return didWork;
    didWork = true;
    for (const op of ops) {
      const id = op.id;
      // The request map mirrors the JSON object the editor emitted (minus the JS-only `id`);
      // `data` (write/append) becomes a byte buffer so it crosses as msgpack `bin` (daemon) /
      // is written verbatim (OPFS).
      const req = { ...op };
      delete req.id;
      if (Array.isArray(req.data)) req.data = new Uint8Array(req.data);
      let reply;
      try {
        reply = daemonUri ? await daemonFsOp(req) : await opfsFsOp(req);
      } catch (e) {
        reply = ["err", "EIO", String(e && e.message ? e.message : e)];
      }
      landFsOpResult(id, reply);
    }
  }
}

// Run one `nx.fs` op against the daemon (the `luafs_op` leg). In daemon mode fs NEVER silently
// falls back to OPFS (the thesis) — a dropped link returns a loud error, exactly as `daemonRead`.
async function daemonFsOp(req) {
  if (!daemon) return ["err", "ENOTCONN", daemonError || "daemon not connected"];
  try {
    return await daemon.request("luafs_op", [req]);
  } catch (e) {
    return ["err", "EIO", String(e && e.message ? e.message : e)];
  }
}

// Forward the streaming `nx.fs.watch` arms/disarms the tick enqueued to the daemon (the
// `luafs_watch` leg — Phase 3b). The daemon answers with `luafs_change`/`luafs_watch_err` pushes
// (`onDaemonNotify`). Only reached in a daemon session — the tick gates the watch on a connected
// daemon (`has_remote_proc`), so a serverless `nx.fs.watch` fails loud in the core and never
// enqueues. `liveFsWatches` gates the async park so the reader keeps running to receive pushes.
async function drainFsWatchRequests() {
  const reqs = JSON.parse(readStr(eh_take_fs_watch_requests(h)));
  if (reqs.arm.length === 0 && reqs.disarm.length === 0) return;
  if (!daemon) return; // defensive: the tick shouldn't enqueue a watch without a daemon
  for (const w of reqs.arm) {
    liveFsWatches.add(w.id);
    await daemon.notify("luafs_watch", [w.id, w.path, w.recursive === true]);
  }
  for (const id of reqs.disarm) {
    liveFsWatches.delete(id);
    await daemon.notify("luafs_unwatch", [id]);
  }
}

// Land one `nx.fs` op reply into the tick: re-encode the daemon's `["ok"|"err", …]` envelope to
// msgpack bytes (a `read` result's raw bytes survive the round-trip as `bin`) and hand them to
// `eh_fs_op_result`, which decodes them through the shared `fswire` codec and resolves/rejects
// the op's promise. The bytes are copied into a malloc'd buffer for the synchronous call, then
// freed — no `await` between `HEAPU8.set` and the call, so growth can't detach the heap.
function landFsOpResult(id, reply) {
  const bytes = encode(reply);
  const ptr = bytes.length ? M._malloc(bytes.length) : 0;
  if (ptr) M.HEAPU8.set(bytes, ptr);
  try {
    eh_fs_op_result(h, id, ptr, bytes.length);
  } finally {
    if (ptr) M._free(ptr);
  }
}

// Forward the terminal ops the tick enqueued to the daemon (`term_open` / `term_write` /
// `term_resize` / `term_kill` — the web `:terminal`, Phase 7). The daemon runs the real PTY
// and answers with `term_data`/`term_exit` pushes (`onDaemonNotify`). Only reached in a daemon
// session — the dispatch gates terminal opens on a connected daemon (`has_remote_proc`), so a
// serverless `:terminal` fails loud in the core and never enqueues. Order matters within a
// drain (open before its writes/resizes), so the queues are forwarded open → write → resize →
// kill — the order the editor enqueues them.
async function drainTerminalRequests() {
  const reqs = JSON.parse(readStr(eh_take_terminal_requests(h)));
  const interrupt = reqs.interrupt || [];
  if (
    reqs.open.length === 0 &&
    reqs.write.length === 0 &&
    reqs.resize.length === 0 &&
    reqs.kill.length === 0 &&
    interrupt.length === 0
  ) {
    return;
  }
  if (!daemon) {
    // Serverless: fulfil `:terminal` against the local in-browser host (demo build) if present.
    if (localHost) localHost.terminal(reqs);
    return;
  }
  for (const o of reqs.open) {
    liveTerms.add(o.buf);
    await daemon.notify("term_open", [o.buf, o.argv, o.cwd ?? null, o.rows, o.cols]);
  }
  for (const w of reqs.write) {
    await daemon.notify("term_write", [w.buf, new Uint8Array(w.bytes)]);
  }
  // A `^C` trimmed a flooding terminal (the core decided it was a flood-cancel): discard the
  // child's in-flight backlog so the cancel takes hold promptly instead of the browser rendering
  // the seconds of output the daemon already put on the wire (bounded only by the QUIC window).
  for (const buf of interrupt) {
    discardingTerms.add(Number(buf));
    lastDiscardActivityMs = nowMs();
    scheduleDiscardCheck();
  }
  for (const r of reqs.resize) await daemon.notify("term_resize", [r.buf, r.rows, r.cols]);
  for (const buf of reqs.kill) {
    liveTerms.delete(buf);
    await daemon.notify("term_kill", [buf]);
  }
}

// Forward the LSP wire ops the SyncLspClient enqueued to the daemon (`lsp_spawn` / `lsp_stdin` /
// `lsp_kill` — the LSP leg, Phase 6e). The daemon runs the real language server (`serve_one_lsp`,
// the same wire the native `RemoteLspTransport` uses) and answers with `lsp_stdout`/`lsp_stderr`/
// `lsp_exited` pushes (`onDaemonNotify`). Only reached in a daemon session — the editor gates
// `vim.lsp.start` on a connected daemon (`has_remote_lsp`), so a serverless session fails it loud
// in the core and never enqueues. `spawn` is forwarded before `stdin` so the daemon processes
// `lsp_spawn` before the `initialize` `lsp_stdin` that follows on the same ordered stream;
// `liveLsp` gates the async park so the reader keeps running to receive the server's pushes.
async function drainLspRequests() {
  const reqs = JSON.parse(readStr(eh_take_lsp_requests(h)));
  if (reqs.spawn.length === 0 && reqs.stdin.length === 0 && reqs.kill.length === 0) return;
  if (!daemon) {
    // Serverless: route to the local host (demo build) if present — it fails loud for legs it
    // hasn't wired yet. Standard build: no local host, no enqueued LSP ops (the gate is closed).
    if (localHost) localHost.lsp(reqs);
    return;
  }
  for (const s of reqs.spawn) {
    liveLsp.add(s.id);
    await daemon.notify("lsp_spawn", [s.id, s.program, s.args, s.cwd]);
  }
  for (const i of reqs.stdin) {
    await daemon.notify("lsp_stdin", [i.id, new Uint8Array(i.bytes)]);
  }
  for (const id of reqs.kill) {
    liveLsp.delete(id);
    await daemon.notify("lsp_kill", [id]);
  }
}

// Forward the watch arm/disarm requests the editor enqueued to the daemon (`fs_watch` /
// `fs_unwatch`). Serverless OPFS has no change source, so a watch is dropped — not a silent
// stub: the tab is the sole writer, there is genuinely nothing external to watch.
async function drainWatchRequests() {
  const reqs = JSON.parse(readStr(eh_take_watch_requests(h)));
  if (reqs.arm.length === 0 && reqs.disarm.length === 0) return;
  if (!daemon) return; // serverless OPFS — no remote to watch
  for (const path of reqs.arm) {
    armedWatches.add(path);
    await daemon.notify("fs_watch", [path]);
  }
  for (const path of reqs.disarm) {
    armedWatches.delete(path);
    await daemon.notify("fs_unwatch", [path]);
  }
}

// Forward any `:TSInstall <lang>` the tick enqueued to the UI thread, where the
// web-tree-sitter highlighter lives. Fire-and-forget: the UI fetches the prebuilt grammar
// (offline bundle / OPFS / jsDelivr), caches + registers it, and posts the outcome back as a
// `ts_install_result` (a ring type-6 frame under SAB, a message under 5c) which lands via
// `eh_ts_install_complete`. The "installing…" echo already painted with the `:TSInstall`
// keystroke, so no extra repaint is needed here. Returns whether any were sent.
function drainTsRequests() {
  const reqs = JSON.parse(readStr(eh_take_ts_requests(h)));
  for (const lang of reqs) postMessage({ type: "ts_install", lang });
  return reqs.length > 0;
}

// Forward any `"+`/`"*` yanks/deletes the tick enqueued to the UI thread, which writes the
// text to `navigator.clipboard` (a Worker has no clipboard access). Fire-and-forget, exactly
// like `drainTsRequests` — the editor already painted the yank, so no extra repaint here.
function drainClipboardWrites() {
  const writes = JSON.parse(readStr(eh_take_clipboard_writes(h)));
  for (const text of writes) postMessage({ type: "clipboard_write", text });
}

// 5c (postMessage, no run loop): apply a push + fulfil any reload re-fetch + repaint inline.
// Safe because 5c is never parked (no single-consumer run loop to race).
async function pump5cDaemon() {
  applyDaemonNotifications();
  await fulfillFsRequests();
  await drainFsOpRequests();
  await drainProcRequests();
  await drainFsWatchRequests();
  await drainTerminalRequests();
  await drainLspRequests();
  postMessage(redrawMsg());
}

// Drain every off-tick fs request the editor enqueued, run its op (OPFS serverless, or the
// daemon over WebTransport when `?daemon=` is set), and land the result back into the tick.
// Loops until the queue is dry, because landing a read fires `BufReadPost` autocmds (and
// `run_pending`) that may enqueue further opens/saves. Returns whether any request was
// handled (so the caller posts a fresh redraw).
// Land one fs read into the editor tick via the FFI. A file (kind 0) passes its raw bytes
// (`res.bytes`) through wasm memory so Rust decodes them with the encoding seam; a dir (2) /
// new (1) / error (3) passes `res.text` (JSON / "" / message). The bytes are copied into a
// malloc'd buffer for the synchronous call, then freed — there's no `await` between the
// `HEAPU8.set` and the call, so ALLOW_MEMORY_GROWTH can't detach the heap mid-sequence.
function landFsRead(buffer, path, kind, res) {
  if (kind === 0) {
    const src = res.bytes;
    const bytes = src instanceof Uint8Array ? src : src ? new Uint8Array(src) : new Uint8Array(0);
    const len = bytes.length;
    const ptr = len ? M._malloc(len) : 0;
    if (ptr) M.HEAPU8.set(bytes, ptr);
    try {
      eh_fs_read_complete(h, buffer, path, kind, "", ptr, len);
    } finally {
      if (ptr) M._free(ptr);
    }
  } else {
    eh_fs_read_complete(h, buffer, path, kind, res.text ?? "", 0, 0);
  }
}

async function fulfillFsRequests() {
  let didWork = false;
  for (;;) {
    const reqs = JSON.parse(readStr(eh_take_fs_requests(h)));
    if (reqs.reads.length === 0 && reqs.writes.length === 0) return didWork;
    didWork = true;
    for (const r of reqs.reads) {
      // A picker-bound path's bytes live behind a UI-held handle — route the read there;
      // the `fs_read_result` reply lands it (`landRealFsReply`). Else fulfill from OPFS.
      if (boundPaths.has(r.path)) {
        dispatchRealFsRead(r.buffer, r.path);
        continue;
      }
      const res = daemonUri ? await daemonRead(r.path) : await opfsRead(r.path);
      // A directory read carries its canonical dir in res.path (the explorer navigates
      // from it); a file/new/err keeps the requested path.
      landFsRead(r.buffer, res.path ?? r.path, res.kind, res);
    }
    for (const w of reqs.writes) {
      // A picker-bound path is written by the UI against its handle; route it there and
      // leave the save in-flight until the `fs_write_result` reply finalizes it.
      if (boundPaths.has(w.path)) {
        dispatchRealFsWrite(w.seq, w.path);
        continue;
      }
      // Copy the snapshot bytes out of wasm memory *before* the await — ALLOW_MEMORY_GROWTH
      // can detach HEAPU8 across a later wasm call, and the pointer is only valid until
      // `eh_fs_write_complete` drops the save.
      const ptr = eh_save_bytes(h, w.seq);
      const len = eh_save_len(h, w.seq);
      const bytes = ptr ? new Uint8Array(M.HEAPU8.subarray(ptr, ptr + len)) : new Uint8Array(0);
      const res = daemonUri ? await daemonWrite(w.path, bytes) : await opfsWrite(w.path, bytes);
      // mtime_ms: OPFS has no watch leg (so -1 is fine); the daemon returns a real stat
      // (`res.mtimeMs`) the editor stamps as its disk baseline.
      eh_fs_write_complete(h, w.seq, res.ok ? 1 : 0, res.ok ? res.size : 0, res.mtimeMs ?? -1, res.ok ? "" : res.error);
    }
  }
}

// =============================================================================
// Persistence (shada) — serverless OPFS.
//
// The editor's cross-session state (registers, marks, search/ex history, jumplist,
// changelist) is the pure snapshot the core hands out; we serialize it to a single JSON
// blob at SHADA_PATH in OPFS and restore it at boot (`bootWithConfig`). This is the
// browser analogue of the native redb store — minus the multi-instance merge a single tab
// doesn't need. Durability is the *debounced checkpoint* (write after a quiet period),
// exactly as native treats it as the primary mechanism; a flush-with-exit-cursor on tab
// hide seeds `'0` for next launch (best-effort — the debounce already captured the rest).
// =============================================================================
const SHADA_PATH = "/.nxvim/shada";
const SHADA_DEBOUNCE_MS = 1200;
let shadaBaseline = null; // last-written no-exit JSON, for change detection (skip no-op writes)
let shadaDirty = false; // input arrived since the last checkpoint
let shadaDueMs = 0; // earliest time the debounced checkpoint may fire
let shadaFlushRequested = false; // a flush-with-exit was asked for (tab hidden / test hook)

// Write the shada snapshot to OPFS when it changed (or `force`). The no-exit form is the
// change-detection baseline (pure cursor moves don't churn it); `includeExit` additionally
// persists the clean-exit cursor so `'0` seeds next launch. Posts `shada_written` on a real
// write (the UI's flush hook awaits it). Returns whether it wrote.
async function checkpointShada(includeExit, force) {
  shadaDirty = false;
  const canonical = readStr(eh_export_shada(h, 0));
  if (!canonical) return false; // serialization failed — write nothing rather than a bad blob
  if (!force && canonical === shadaBaseline) return false;
  shadaBaseline = canonical;
  const json = includeExit ? readStr(eh_export_shada(h, 1)) : canonical;
  const res = await opfsWrite(SHADA_PATH, new TextEncoder().encode(json));
  if (res.ok) postMessage({ type: "shada_written", bytes: json.length });
  return res.ok;
}

// 5c-only throttled checkpoint: the postMessage path has no run loop to debounce against,
// so write at most once per SHADA_DEBOUNCE_MS of activity (leading-edge). The UI's hide
// flush covers the trailing gap. (Under SAB the run loop does the proper debounce instead.)
let shada5cLast = 0;
async function maybeCheckpoint5c() {
  const now = nowMs();
  if (now - shada5cLast < SHADA_DEBOUNCE_MS) return;
  shada5cLast = now;
  await checkpointShada(false, false);
}

// =============================================================================
// Slice 5d — the SharedArrayBuffer run loop.
//
// ctrl: Int32Array(4) — [SEQ, WRITE, READ, _]. SEQ is the wake counter the UI bumps +
//   notifies; WRITE/READ are monotonic byte cursors (mod 2^32) into the data ring.
// data: Uint8Array ring. Frames are [type:u8][reqId:u32][len:u32][payload:len]:
//   type 0 = feed (payload = vim notation), 1 = exec_lua (payload = code),
//   2 = attach (payload = cols:u32, rows:u32),
//   3 = mouse (payload = JSON {b:button, a:action, m:modifier, r:row, c:col}).
// =============================================================================
const SEQ = 0, WRITE = 1, READ = 2;

async function runLoopSAB(ctrl, data) {
  sabMode = true;
  sabCtrl = ctrl; // expose to `onDaemonNotify` so a daemon push can wake the futex park
  const cap = data.length;
  const rdByte = (pos) => data[((pos % cap) + cap) % cap];
  const rdU32 = (pos) =>
    (rdByte(pos) | (rdByte(pos + 1) << 8) | (rdByte(pos + 2) << 16) | (rdByte(pos + 3) << 24)) >>> 0;

  // Drain every queued frame (READ → WRITE), apply it, and return the reqIds processed
  // plus any exec_lua results, so the UI can resolve the matching promises.
  function drain() {
    const acks = [];
    const results = [];
    let tsLanded = false;
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
      } else if (type === 3) {
        // The clock was set to now before this drain, so the mouse stamp drives
        // 'mousetime' multi-click detection off the same JS clock as input.
        const m = JSON.parse(utf8(payload));
        eh_input_mouse(h, m.b, m.a, m.m, m.r | 0, m.c | 0);
      } else if (type === 4) {
        // bind: the path resolves to a real-FS handle the UI holds. Sent *before* the
        // `:e`/`:w` that references it, so the routing decision is set when the request
        // drains. (Bulk content never rides the ring — it's small enough to overflow it —
        // so only this marker does; the file bytes flow over postMessage.)
        boundPaths.add(utf8(payload));
      } else if (type === 5) {
        // shada_flush: persist cross-session state now, with the exit cursor (tab hidden).
        shadaFlushRequested = true;
      } else if (type === 6) {
        // ts_install_result: the UI finished a `:TSInstall` ({lang, ok, msg} JSON). Land the
        // outcome (echo + record installed). Not a user request, so it earns a repaint but
        // no ack / shada churn — skip the `acks.push` below.
        const r = JSON.parse(utf8(payload));
        eh_ts_install_complete(h, String(r.lang), r.ok ? 1 : 0, String(r.msg || ""));
        tsLanded = true;
        continue;
      } else if (type === 7) {
        // connect: a runtime `:connect nxvim://…`. Record the URI; the run loop dials it
        // after this drain (the dial is async, so it can't run here in the sync drain).
        pendingConnectUri = utf8(payload);
      } else if (type === 8) {
        // clipboard_push: the UI read `navigator.clipboard` (on focus / paste) and handed us
        // its text — update the mirror a `"+`/`"*` paste reads. Not a user keystroke, so it
        // earns neither an ack nor a repaint (the cache update is invisible until a paste).
        eh_clipboard_push(h, utf8(payload));
        continue;
      }
      acks.push(reqId);
    }
    Atomics.store(ctrl, READ, rp);
    return { acks, results, tsLanded };
  }

  // Initial paint (the attach the UI requested rode in as the first frame, but paint a
  // baseline regardless so the UI has a frame even before any input).
  postMessage(redrawMsg({ acks: [], results: [] }));

  for (;;) {
    const seqBefore = Atomics.load(ctrl, SEQ);
    const fired = eh_tick_timers(h, nowMs()) === 1;
    // Set the clock to *now* before draining input, so a timer armed by this batch's
    // `vim.defer_fn` / `nx.timer` (feed or exec_lua) computes its deadline from now —
    // not a stale clock that would make it instantly due.
    eh_set_clock(h, nowMs());
    const { acks, results, tsLanded } = drain();
    // A runtime `:connect nxvim://…` arrived this pass (`<Esc>` already dismissed the command
    // line in the drain above): dial the daemon now, while the loop is event-loop-live — a
    // blocking `Atomics.wait` park would freeze the WebTransport handshake. This re-points the
    // off-tick fs/watch seam onto the wire; re-loop so subsequent input hits the new backend.
    if (pendingConnectUri !== null) {
      const uri = pendingConnectUri;
      pendingConnectUri = null;
      await runtimeConnect(uri);
      postMessage(redrawMsg({ acks, results }));
      continue;
    }
    // Apply any daemon pushes received since the last pass (the watch leg's `fs_changed`):
    // a reconcile may enqueue an off-tick reload, which `fulfillFsRequests` then re-fetches
    // over the wire in this same pass. Done before `fulfillFsRequests` so the reload lands now.
    const notified = applyDaemonNotifications();
    // Project the terminal: forced (so an exit / fs / proc push paints this pass) when
    // `notified`, otherwise throttled to ~frame rate so a `term_data` flood paints at 60fps
    // while feeding far faster. A trailing flush before the park (below) catches the last batch.
    const termFlushed = maybeFlushTerminals(nowMs(), notified);
    // Fulfill any `:e`/`:w` the tick (or a fired timer / a push reconcile) deferred against
    // OPFS or the daemon. We're not parked here, so the event loop runs and the promises
    // resolve; the completions land the buffer/save back into the tick before we post the ack,
    // so the UI sees the opened/saved frame when its `feed` promise resolves.
    const fsWork = await fulfillFsRequests();
    // Forward any off-tick `nx.fs` ops the tick enqueued to the daemon (the `luafs_op` leg).
    // Request/response within this pass (we're not parked), so each reply lands before the
    // repaint below — the op's promise resolves and the UI sees its effect.
    const fsOpWork = await drainFsOpRequests();
    // Forward any watches the tick armed/disarmed (file-backed buffers opened/closed) to the
    // daemon, so it begins/stops pushing `fs_changed` for them.
    await drainWatchRequests();
    // Forward any streaming `nx.fs.watch` arms/disarms the tick enqueued to the daemon (its
    // `luafs_change`/`luafs_watch_err` pushes return on `onDaemonNotify`).
    await drainFsWatchRequests();
    // Forward any async `vim.system` / `jobstart` the tick enqueued to the daemon (its
    // `proc_spawned`/`proc_exited` pushes return on `onDaemonNotify`).
    await drainProcRequests();
    // Forward any `:terminal` PTY ops the tick enqueued to the daemon (its `term_data`/
    // `term_exit` pushes return on `onDaemonNotify`).
    await drainTerminalRequests();
    // Forward any LSP wire ops the tick (or a just-applied `lsp_stdout`) enqueued to the daemon
    // (its `lsp_stdout`/`lsp_stderr`/`lsp_exited` pushes return on `onDaemonNotify`).
    await drainLspRequests();
    // Forward any `:TSInstall` the tick enqueued to the UI thread (fire-and-forget).
    drainTsRequests();
    // Forward any `"+`/`"*` yanks/deletes to the UI thread to write to navigator.clipboard.
    drainClipboardWrites();
    if (fired || acks.length || fsWork || fsOpWork || notified || tsLanded || termFlushed) {
      postMessage(redrawMsg({ acks, results }));
    }
    // Persistence (shada): any input this pass arms the debounced checkpoint; a requested
    // flush (tab hidden) writes immediately with the exit cursor. The checkpoint write is
    // async (OPFS) — awaiting it here is fine (the thread isn't parked yet).
    if (acks.length || results.length || fsWork || fsOpWork) {
      shadaDirty = true;
      shadaDueMs = nowMs() + SHADA_DEBOUNCE_MS;
    }
    if (shadaFlushRequested) {
      shadaFlushRequested = false;
      await checkpointShada(true, true);
    } else if (shadaDirty && nowMs() >= shadaDueMs) {
      await checkpointShada(false, false);
    }
    // A picker-bound `:e`/`:w` dispatched a read/write to the UI this pass. Don't park on
    // `Atomics.wait` — a parked thread can't receive the UI's reply postMessage. Instead
    // stay in the event loop awaiting the reply (`onmessage` → `landRealFsReply` resolves
    // it), repaint with the landed result, then loop to drain any cascade + any input that
    // arrived meanwhile (it's queued in the ring; the SEQ notify was a no-op while unparked,
    // but the next `drain()` picks it up).
    if (pendingRealFs > 0) {
      await new Promise((res) => {
        fsReplyWaiter = res;
      });
      postMessage(redrawMsg({ acks: [], results: [] }));
      continue;
    }
    // A push that arrived during this pass (e.g. while awaiting `fulfillFsRequests`) is
    // queued — re-process it next iteration rather than parking on a now-stale wait.
    if (daemonNotifications.length > 0) continue;
    // The queue drained: do the throttled terminal flush if its frame interval has elapsed
    // (a momentary empty mid-flood respects the throttle — it won't fire every gap), and
    // re-loop so the repaint goes out this pass.
    if (maybeFlushTerminals(nowMs(), false)) {
      postMessage(redrawMsg({ acks: [], results: [] }));
      continue;
    }
    // Park until the UI notifies (SEQ changes) or the next timer is due. If input
    // arrived while we processed (SEQ already moved off seqBefore), the wait
    // returns "not-equal" at once — no missed wakeup.
    let deadline = eh_next_deadline(h);
    // A terminal feed is pending but the throttle hasn't elapsed: wake by then to paint it,
    // so the last batch of a flood isn't stranded unprojected until the next push.
    if (termFlushDue) {
      const flushDue = lastTermFlushMs + TERM_FLUSH_MS;
      deadline = deadline < 0 ? flushDue : Math.min(deadline, flushDue);
    }
    // Fold the shada debounce deadline into the same park, so the one wait that wakes on a
    // keystroke / timer also wakes to flush cross-session state after a quiet period.
    if (shadaDirty) deadline = deadline < 0 ? shadaDueMs : Math.min(deadline, shadaDueMs);
    let timeout = deadline < 0 ? undefined : Math.max(0, deadline - nowMs());
    // Any in-flight host work means a push could land asynchronously and must be received off
    // the event loop — a daemon over the wire OR the local in-browser host (the Pyodide
    // `:terminal`, whose `term_data`/`term_exit` arrive via the sibling Worker's postMessage).
    // A thread frozen in blocking `Atomics.wait` can't process those messages, so park async
    // while a live terminal / child / watch / server exists, in either transport.
    const hostPushing =
      (daemon || localHost) &&
      (armedWatches.size > 0 ||
        liveProcs.size > 0 ||
        liveTerms.size > 0 ||
        liveFsWatches.size > 0 ||
        liveLsp.size > 0);
    if (hostPushing || indenterBusy()) {
      // Don't *block* — a thread frozen in `Atomics.wait` can't run the event loop, so any
      // pending promise stalls. Cases that need it live: a daemon session expecting pushes
      // (a watch armed, or a child in flight — its WebTransport reader must keep running to
      // receive a `fs_changed` / `proc_exited` push), and the treesitter indenter while it
      // loads a grammar (its fetch / `Language.load` promises must resolve between
      // keystrokes). Park on the non-blocking `Atomics.waitAsync` so the event loop (hence
      // the reader) keeps running; wake on input (SEQ notify resolves `w.value`), a daemon
      // push (`daemonWaker`), or the poll cap. The cap is short while a grammar is loading so
      // the load completes promptly, then the loop falls back to blocking once idle.
      const cap = indenterBusy() ? INDENT_LOAD_POLL_MS : DAEMON_PUSH_POLL_MS;
      timeout = timeout === undefined ? cap : Math.min(timeout, cap);
      const w = Atomics.waitAsync(ctrl, SEQ, seqBefore, timeout);
      if (w.async) await Promise.race([w.value, new Promise((res) => { daemonWaker = res; })]);
      // (`!w.async` ⇒ SEQ already moved / timeout 0 — loop immediately, no missed wakeup.)
    } else {
      // Serverless OPFS (or daemon with nothing pending) and the indenter idle: no pushes to
      // receive and nothing pending on the microtask queue, so block — it's cheaper.
      Atomics.wait(ctrl, SEQ, seqBefore, timeout);
    }
  }
}

function rdU32Bytes(arr, off) {
  return (arr[off] | (arr[off + 1] << 8) | (arr[off + 2] << 16) | (arr[off + 3] << 24)) >>> 0;
}

// =============================================================================
// Slice 5c — the postMessage fallback (no SAB / not cross-origin isolated).
// =============================================================================
function postFrame(id, result) {
  postMessage(redrawMsg({ id, result }));
}

let started = false;
onmessage = async (ev) => {
  const msg = ev.data;
  if (msg.type === "init") {
    // SAB handover (5d): enter the blocking run loop and never return — all further
    // input arrives over the SAB, redraws post out (posting out is never blocked).
    if (started) return;
    started = true;
    eh_attach(h, msg.cols | 0, msg.rows | 0);
    runLoopSAB(new Int32Array(msg.ctrl), new Uint8Array(msg.data)).catch((e) =>
      postMessage({ type: "fatal", error: `SAB run loop failed: ${e}` }),
    );
    return;
  }
  // Real-FS picker plumbing (both transports). The UI fulfills a bound path's read/write
  // against its `FileSystemFileHandle` and reports back here; land it into the tick. Under
  // SAB these are received while the run loop is event-loop-live (pendingRealFs > 0); under
  // 5c the Worker isn't parked, so they're received normally.
  if (msg.type === "fs_read_result") {
    // A real-FS file read (kind 0) carries raw bytes (`msg.bytes`) so the encoding seam sees
    // them intact; a dir/new/error carries `msg.text`.
    landFsRead(msg.buffer, String(msg.path), msg.kind | 0, { bytes: msg.bytes, text: msg.text });
    await landRealFsReply();
    return;
  }
  if (msg.type === "fs_write_result") {
    eh_fs_write_complete(
      h,
      msg.seq,
      msg.ok ? 1 : 0,
      msg.ok ? msg.size : 0,
      -1,
      msg.ok ? "" : String(msg.error ?? "write failed"),
    );
    await landRealFsReply();
    return;
  }
  // 5c bind (the SAB transport binds via a ring frame in `drain()` instead). Marks a path
  // as resolving to a real handle; must precede the `:e`/`:w` the UI sends after it.
  if (msg.type === "bind") {
    boundPaths.add(String(msg.path));
    return;
  }
  // Shada flush (SAB requests it via a ring frame). Persist now, with the exit cursor.
  if (msg.type === "shada_flush") {
    await checkpointShada(true, true);
    return;
  }
  // `:TSInstall` outcome from the UI (SAB sends it as a ring type-6 frame instead). Land
  // the echo + record, then repaint so the status line shows it.
  if (msg.type === "ts_install_result") {
    eh_ts_install_complete(h, String(msg.lang), msg.ok ? 1 : 0, String(msg.msg ?? ""));
    postMessage(redrawMsg());
    return;
  }
  // Runtime `:connect nxvim://…` (5c; SAB routes it via a ring frame in `drain()`). Dial the
  // daemon and re-point the off-tick fs/watch seam onto the wire, then repaint.
  if (msg.type === "connect") {
    await runtimeConnect(String(msg.uri));
    postMessage(redrawMsg());
    return;
  }
  // Clipboard mirror push (5c; SAB routes it via a ring type-8 frame in `drain()`). The UI
  // read `navigator.clipboard` and handed us its text — update what a `"+`/`"*` paste reads.
  // No repaint: the cache change is invisible until a paste consumes it.
  if (msg.type === "clipboard_push") {
    eh_clipboard_push(h, String(msg.text ?? ""));
    return;
  }
  // Out-of-band image-preview byte fetch (`'imagepreview'`): the editor core never reads an
  // image buffer's bytes (it stays an inert preview — the never-freeze invariant), so the UI
  // fetches them here to paint the `<img>`. The UI reads OPFS and picker-bound real files
  // itself (those are UI-reachable); it only routes here for a *daemon* path, whose bytes live
  // over the wire. Reply with the raw bytes (transferable, zero-copy) or an error.
  if (msg.type === "image_read") {
    const res = daemon ? await daemonRead(String(msg.path)) : await opfsRead(String(msg.path));
    const ok = res.kind === 0 && !!res.bytes;
    postMessage(
      {
        type: "image_read_result",
        reqId: msg.reqId,
        ok,
        bytes: ok ? res.bytes : null,
        error: ok ? "" : res.text || "not a readable file",
      },
      ok ? [res.bytes.buffer] : [],
    );
    return;
  }
  // postMessage fallback (5c). `:e`/`:w` fulfill against OPFS before the frame posts, so
  // the resolved frame reflects the open/save (the SAB loop does the same inline).
  switch (msg.type) {
    case "attach":
      eh_attach(h, msg.cols | 0, msg.rows | 0);
      postFrame(msg.id);
      break;
    case "feed":
      eh_input(h, String(msg.notation));
      await fulfillFsRequests();
      await drainFsOpRequests();
      await drainWatchRequests();
      await drainFsWatchRequests();
      await drainProcRequests();
      await drainTerminalRequests();
      await drainLspRequests();
      drainTsRequests();
      drainClipboardWrites();
      postFrame(msg.id);
      await maybeCheckpoint5c();
      break;
    case "input_mouse":
      // Stamp from the JS clock so multi-click timing matches the keystroke tick.
      eh_set_clock(h, nowMs());
      eh_input_mouse(h, String(msg.button), String(msg.action), String(msg.modifier), msg.row | 0, msg.col | 0);
      await fulfillFsRequests();
      await drainFsOpRequests();
      await drainWatchRequests();
      await drainFsWatchRequests();
      await drainProcRequests();
      await drainTerminalRequests();
      await drainLspRequests();
      drainTsRequests();
      drainClipboardWrites();
      postFrame(msg.id);
      await maybeCheckpoint5c();
      break;
    case "exec_lua": {
      const result = readStr(eh_exec_lua(h, String(msg.code)));
      await fulfillFsRequests();
      await drainFsOpRequests();
      await drainWatchRequests();
      await drainFsWatchRequests();
      await drainProcRequests();
      await drainTerminalRequests();
      await drainLspRequests();
      drainTsRequests();
      drainClipboardWrites();
      postFrame(msg.id, result);
      await maybeCheckpoint5c();
      break;
    }
    default:
      postMessage({ type: "fatal", error: `unknown worker message: ${msg.type}` });
  }
};

// Connect the daemon (if `?daemon=` was passed) *before* boot finishes, so the link is
// live for the first user `:e` — and a dial failure surfaces before "ready". A no-op in
// serverless (OPFS) mode.
await connectDaemon();

// Python-demo build, serverless: install the local in-browser process host (the Pyodide
// interpreter backs `:terminal python …`). It opens the `proc_host` gate; Pyodide itself loads
// lazily on the first `:terminal`, so this costs nothing until used. The module is
// dynamic-imported so the standard editor build never loads it. In daemon mode `connectDaemon`
// already flipped the gate and processes ride the wire, so the local host stays off.
if (BUILD.localHost && !daemonUri) {
  const { installLocalHost } = await import("./local-host.mjs");
  localHost = installLocalHost({
    setProcHost: (on) => eh_set_proc_host(h, on ? 1 : 0),
    landHostPush,
    toU8,
    liveTerms,
    liveProcs,
    liveLsp,
    reportError: (msg) => postMessage({ type: "config_error", error: msg }),
  });
}

// Source the optional /init.lua + finish boot before announcing "ready", so the config
// is fully applied before the UI attaches and the first frame paints. Run here (module
// tail) so the `const` OPFS helpers it reaches are past their temporal dead zone.
await bootWithConfig();

postMessage({ type: "ready" });
