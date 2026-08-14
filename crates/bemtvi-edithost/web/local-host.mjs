// web/local-host.mjs — the LOCAL in-browser process host (python-demo build ONLY).
//
// Loaded and installed by worker.mjs **only** when build-config.js says `localHost: true`
// (the build-demo.sh build) AND the session is serverless (no `?daemon=`). The standard
// editor build never imports this file. It fulfils the proc / terminal / LSP off-tick seams
// locally — in Phase 1 the terminal leg via the Pyodide interpreter (web/pyodide-worker.mjs),
// running in a sibling Web Worker — instead of forwarding them to a daemon over the wire.
// See docs/plans/2026-06-23-web-python-demo.md.
//
// `installLocalHost(ctx)` opens the process-host gate and returns the three drain handlers
// worker.mjs calls when serverless. `ctx` hands over exactly the Worker internals the host
// needs, so all the demo-specific code stays in this file:
//   { setProcHost(on), landHostPush(method, params), toU8(v), liveTerms, liveProcs, reportError(msg) }
// - setProcHost(on):   flip the core's `proc_host` gate (eh_set_proc_host) so the editor's
//                      terminal / async-spawn / LSP branches open.
// - landHostPush:      land an async host push onto the run loop's notification queue + wake it
//                      (the local-host twin of the daemon `onDaemonNotify`); the host's
//                      `term_data`/`term_exit` + `proc_*` reuse the daemon legs' landings.
// - toU8:              normalize transferred bytes to a Uint8Array.
// - liveTerms/liveProcs/liveLsp: the Sets the run loop's async-park condition reads (a live
//                      terminal, in-flight child, or running language server keeps the loop on the
//                      non-blocking park so the Worker's messages are received).
// - reportError:       surface a host-level failure loud (→ the page's config_error channel).
//
// LSP leg (Phase 4): the editor's `SyncLspClient` speaks raw `Content-Length`-framed JSON-RPC
// bytes over `lsp_spawn`/`lsp_stdin`/`lsp_kill`, but basedpyright's browser worker
// (web/vendor/basedpyright/pyright.worker.js, built by build-basedpyright.sh) speaks postMessage'd
// JSON objects via `BrowserMessageReader/Writer`. So the host runs a thin **framing bridge**: it
// de-frames the editor's bytes into JSON objects for the worker, frames the worker's objects back
// into `lsp_stdout` bytes, and facilitates basedpyright's background-analysis worker (it can't nest
// workers, so it asks the host to create them via `browser/newWorker` + a transferred MessagePort).

const LSP_ENC = new TextEncoder();
const LSP_DEC = new TextDecoder();

// Frame one JSON-RPC object as the `Content-Length: N\r\n\r\n<json>` bytes the editor's
// `SyncLspClient` parses (the wire shape a real stdio language server emits).
function frameLsp(obj) {
  const body = LSP_ENC.encode(JSON.stringify(obj));
  const header = LSP_ENC.encode(`Content-Length: ${body.length}\r\n\r\n`);
  const out = new Uint8Array(header.length + body.length);
  out.set(header, 0);
  out.set(body, header.length);
  return out;
}

// Pull the next complete `Content-Length`-framed JSON-RPC message out of `st.buf` (the editor's
// stdin bytes accumulate there across `lsp_stdin` pushes), advancing the buffer. Returns the
// parsed object, or null when a full frame isn't buffered yet.
function takeLspFrame(st) {
  const sep = "\r\n\r\n";
  // The header is ASCII, so decoding the buffer to find the separator is safe (the JSON body may
  // be multibyte, but we slice it by byte length, not by the decoded string).
  const text = LSP_DEC.decode(st.buf);
  const headerEnd = text.indexOf(sep);
  if (headerEnd < 0) return null;
  const header = text.slice(0, headerEnd);
  const m = /content-length:\s*(\d+)/i.exec(header);
  if (!m) {
    // Unparseable header — drop up to and past the separator so we don't wedge (fail-soft, but the
    // dropped bytes are surfaced by the resulting parse gap rather than silently swallowed).
    st.buf = st.buf.subarray(byteLen(text.slice(0, headerEnd + sep.length)));
    return null;
  }
  const len = Number(m[1]);
  const bodyStart = byteLen(text.slice(0, headerEnd + sep.length));
  if (st.buf.length - bodyStart < len) return null; // body not fully arrived yet
  const body = st.buf.subarray(bodyStart, bodyStart + len);
  st.buf = st.buf.slice(bodyStart + len);
  try {
    return JSON.parse(LSP_DEC.decode(body));
  } catch {
    return null;
  }
}

const byteLen = (s) => LSP_ENC.encode(s).length;

export function installLocalHost(ctx) {
  // A process host now exists: open the gate (Pyodide loads lazily on the first `:terminal`).
  ctx.setProcHost(true);

  let pyodideWorker = null; // the Pyodide Worker, spawned lazily on the first `:terminal`
  let interruptBuffer = null; // Uint8Array over the Pyodide Worker's SAB; write 2 to SIGINT it

  function ensureWorker() {
    if (pyodideWorker) return pyodideWorker;
    pyodideWorker = new Worker(new URL("./pyodide-worker.mjs", import.meta.url), { type: "module" });
    pyodideWorker.onmessage = (ev) => {
      const m = ev.data;
      if (m.type === "data") ctx.landHostPush("term_data", [m.buf, ctx.toU8(m.bytes)]);
      else if (m.type === "exit") ctx.landHostPush("term_exit", [m.buf, m.code]);
      // The async-proc leg's pushes reuse the daemon `proc_*` landings (eh_proc_spawned /
      // eh_proc_stdout / eh_proc_exited); proc_exited clears the id from `liveProcs` editor-side.
      else if (m.type === "proc-spawned") ctx.landHostPush("proc_spawned", [m.id, m.pid]);
      else if (m.type === "proc-stdout") ctx.landHostPush("proc_stdout", [m.id, m.lines]);
      else if (m.type === "proc-exited") ctx.landHostPush("proc_exited", [m.id, m.code, ctx.toU8(m.stdout), ctx.toU8(m.stderr)]);
      else if (m.type === "interrupt-buffer") interruptBuffer = new Uint8Array(m.buffer);
      else if (m.type === "error") ctx.reportError(m.error);
    };
    pyodideWorker.onerror = (e) => ctx.reportError(`pyodide worker: ${e.message || e}`);
    return pyodideWorker;
  }

  // Ctrl-C: request a SIGINT by writing 2 into the shared interrupt buffer. This reaches the
  // running python even while the Pyodide Worker's event loop is blocked in a tight loop (a
  // `postMessage` would queue behind that block); CPython polls the buffer at bytecode
  // boundaries. A no-op until Pyodide has loaded (nothing is running to interrupt yet).
  function requestInterrupt() {
    if (interruptBuffer) Atomics.store(interruptBuffer, 0, 2);
  }

  // ── LSP (basedpyright) ────────────────────────────────────────────────────────────────────
  const lspById = new Map(); // wire id -> { fg, bgs: Set<Worker>, buf: Uint8Array }

  const newPyrightWorker = () =>
    new Worker(new URL("./vendor/basedpyright/pyright.worker.js", import.meta.url));

  // The project is mounted under a dedicated workspace root (`/w`) inside basedpyright's virtual
  // FS, kept DISJOINT from the bundled typeshed at `/typeshed`. This is load-bearing: with the
  // editor's natural root (`file:///`, the OPFS root) basedpyright would treat `/typeshed`'s ~5000
  // stub files as workspace sources and never get to the user's file. So every `file://` uri is
  // rebased `file:///…` ↔ `file:///w/…` across the bridge (the editor sees its own paths; the
  // server sees `/w/…`). Diagnostics/hover/definition uris are rebased back on the way out.
  const VROOT = "w";
  const toServer = (msg) => JSON.parse(JSON.stringify(msg).replaceAll("file:///", `file:///${VROOT}/`));
  const toEditor = (msg) => JSON.parse(JSON.stringify(msg).replaceAll(`file:///${VROOT}/`, "file:///"));

  // Boot a basedpyright foreground worker for wire `id` and bridge its output to the editor.
  function startLspServer(id) {
    const fg = newPyrightWorker();
    const st = { fg, bgs: new Set(), buf: new Uint8Array(0) };
    lspById.set(id, st);
    ctx.liveLsp.add(id);
    fg.onmessage = (ev) => {
      const m = ev.data;
      // basedpyright can't nest workers (no Safari support), so it asks us to create its
      // background-analysis worker and wire a MessagePort between the two.
      if (m && m.type === "browser/newWorker") {
        const bg = newPyrightWorker();
        st.bgs.add(bg);
        bg.onerror = (e) => ctx.reportError(`basedpyright background worker: ${e.message || e}`);
        bg.postMessage({ type: "browser/boot", mode: "background", initialData: m.initialData, port: m.port }, [m.port]);
        return;
      }
      // Any JSON-RPC message from the server → rebase its uris back to the editor's and frame it
      // as stdout bytes for the SyncLspClient.
      if (m && m.jsonrpc) ctx.landHostPush("lsp_stdout", [id, frameLsp(toEditor(m))]);
    };
    fg.onerror = (e) => {
      ctx.reportError(`basedpyright worker: ${e.message || e}`);
      stopLspServer(id);
      ctx.landHostPush("lsp_exited", [id, 1, -1]); // a crash → the client surfaces ServerExited
    };
    fg.postMessage({ type: "browser/boot", mode: "foreground" });
  }

  // Feed editor stdin bytes (one or more framed JSON-RPC messages) to the server worker, rebasing
  // uris into the `/w` workspace. Two server-specific fixups:
  //  - `initialize`: guarantee `initializationOptions.files` is an object (browser-basedpyright
  //    destructures it unconditionally), and drop the un-prefixed `rootPath` so the server can't
  //    fall back to scanning the typeshed-bearing root.
  //  - `didOpen`: basedpyright only analyzes a file once it exists on its FS, so create it first
  //    (`pyright/createFile`); the didOpen overlay then supplies the live buffer text.
  function feedLspStdin(id, bytes) {
    const st = lspById.get(id);
    if (!st) return;
    const merged = new Uint8Array(st.buf.length + bytes.length);
    merged.set(st.buf, 0);
    merged.set(bytes, st.buf.length);
    st.buf = merged;
    for (;;) {
      const raw = takeLspFrame(st);
      if (!raw) break;
      if (raw.method === "initialize") {
        const params = (raw.params = raw.params || {});
        const io = params.initializationOptions && typeof params.initializationOptions === "object" && !Array.isArray(params.initializationOptions)
          ? params.initializationOptions
          : {};
        if (typeof io.files !== "object" || io.files === null || Array.isArray(io.files)) io.files = {};
        params.initializationOptions = io;
        delete params.rootPath;
        // browser-basedpyright keys its workspace off `workspaceFolders`; bemtvi's SyncLspClient
        // sends only `rootUri`, so without this the server falls back to an empty "<default>"
        // workspace and analyzes nothing. Synthesize one folder at the (about-to-be-remapped) root.
        params.workspaceFolders = [{ uri: params.rootUri || "file:///", name: VROOT }];
      }
      const msg = toServer(raw);
      if (raw.method === "textDocument/didOpen") {
        st.fg.postMessage({ jsonrpc: "2.0", method: "pyright/createFile", params: { uri: msg.params.textDocument.uri } });
      }
      st.fg.postMessage(msg);
    }
  }

  // Tear down a server worker + its background workers (an editor `lsp_kill` or a crash).
  function stopLspServer(id) {
    const st = lspById.get(id);
    if (!st) return;
    lspById.delete(id);
    ctx.liveLsp.delete(id);
    try { st.fg.terminate(); } catch {}
    for (const bg of st.bgs) { try { bg.terminate(); } catch {} }
  }

  return {
    // Fulfil the `:terminal` ops the tick enqueued against the local Pyodide host. The Pyodide
    // Worker answers with `data`/`exit` messages → `term_data`/`term_exit` pushes. `open` (run a
    // script, or the REPL for bare `python`), `write` (REPL line editing), `kill`, and Ctrl-C
    // (a `0x03` keystroke in `write`, or the core's flood-cancel `interrupt`) → a SIGINT via the
    // shared buffer. `resize` is a no-op for now (the REPL doesn't reflow).
    terminal(reqs) {
      const w = ensureWorker();
      for (const o of reqs.open) {
        ctx.liveTerms.add(o.buf);
        w.postMessage({ type: "open", buf: o.buf, argv: o.argv, cwd: o.cwd ?? null });
      }
      for (const wr of reqs.write) {
        const bytes = new Uint8Array(wr.bytes);
        if (bytes.includes(0x03)) requestInterrupt(); // Ctrl-C: interrupt a running computation
        w.postMessage({ type: "write", buf: wr.buf, bytes });
      }
      for (const buf of reqs.interrupt || []) requestInterrupt(); // flood-cancel Ctrl-C
      for (const buf of reqs.kill) {
        ctx.liveTerms.delete(buf);
        w.postMessage({ type: "kill", buf });
      }
    },

    // Fulfil the async-proc ops (`vim.system` / `jobstart`) the tick enqueued against the local
    // Pyodide host. Each spawn runs `python …` in the interpreter (capturing stdout/stderr +
    // exit, or streaming stdout lines for `btv.run_stream`); the Pyodide Worker answers with
    // `proc-spawned`/`proc-stdout`/`proc-exited` → the daemon leg's `proc_*` landings. `liveProcs`
    // keeps the run loop on its non-blocking park so those pushes are received off the event loop.
    //
    // `python` is the ONLY program this host can run, and a spawn it cannot perform is reported
    // the way every other leg reports one: pid `-1` and exit `code = -1` (see host.rs — "`code =
    // -1` on a spawn failure or a kill"). That code is the canonical "this tool could not RUN"
    // signal callers fall back on, and getting it wrong is not cosmetic: the `files` /
    // `live_grep` picker sources walk `rg` → `find`/`grep` → an `btv.fs` walk, stepping on only
    // when the previous tool could not run — so a fabricated "ran and found nothing" status
    // (a shell's 127) settles the chain on `rg` and the picker comes up permanently empty.
    // Answered here rather than in the Worker, so a missing binary never boots CPython to say so.
    proc(reqs) {
      const runnable = [];
      for (const s of reqs.spawn) {
        if (Array.isArray(s.argv) && s.argv[0] === "python") {
          runnable.push(s);
          continue;
        }
        const cmd = (s.argv && s.argv[0]) || "";
        ctx.landHostPush("proc_spawned", [s.id, -1]);
        ctx.landHostPush("proc_exited", [
          s.id,
          -1,
          ctx.toU8([]),
          ctx.toU8(`bemtvi web demo: only \`python\` runs in the browser process host (no "${cmd}")\n`),
        ]);
      }
      if (runnable.length === 0 && reqs.kill.length === 0) return; // nothing for the interpreter
      const w = ensureWorker();
      for (const s of runnable) {
        ctx.liveProcs.add(s.id);
        w.postMessage({ type: "proc-open", id: s.id, argv: s.argv, cwd: s.cwd ?? null, env: s.env, stdin: s.stdin, stream: s.stream === true });
      }
      // Kill: SIGINT the running computation via the shared interrupt buffer (a `postMessage`
      // would queue behind a worker blocked in a tight python loop), and tell the Worker so a
      // not-yet-started spawn reports a killed exit. Best-effort — one shared interrupt buffer
      // and a single-threaded interpreter mean a kill hits whatever is currently running.
      for (const id of reqs.kill) {
        requestInterrupt();
        w.postMessage({ type: "proc-kill", id });
      }
    },

    // Fulfil the LSP wire ops against the local basedpyright worker (see the framing-bridge note
    // at the top of this file). Only basedpyright is available in this demo — any other server
    // fails loud rather than silently dropping the spawn. `liveLsp` keeps the run loop on its
    // non-blocking park so the worker's `lsp_stdout`/`lsp_exited` pushes are received.
    lsp(reqs) {
      for (const s of reqs.spawn) {
        if (!/pyright/i.test(s.program || "")) {
          ctx.reportError(`local LSP host: only basedpyright is available in this demo (got "${s.program}")`);
          continue;
        }
        startLspServer(s.id);
      }
      for (const i of reqs.stdin) feedLspStdin(i.id, ctx.toU8(i.bytes));
      for (const id of reqs.kill) stopLspServer(id);
    },
  };
}
