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
//   { setProcHost(on), landHostPush(method, params), toU8(v), liveTerms, reportError(msg) }
// - setProcHost(on):   flip the core's `proc_host` gate (eh_set_proc_host) so the editor's
//                      terminal / async-spawn / LSP branches open.
// - landHostPush:      land an async host push onto the run loop's notification queue + wake it
//                      (the local-host twin of the daemon `onDaemonNotify`); the host's
//                      `term_data`/`term_exit` reuse the daemon leg's landing + vt100 emulation.
// - toU8:              normalize transferred bytes to a Uint8Array.
// - liveTerms:         the Set the run loop's async-park condition reads (a live terminal keeps
//                      the loop on the non-blocking park so the Worker's messages are received).
// - reportError:       surface a host-level failure loud (→ the page's config_error channel).
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

    // The proc leg (`vim.system` / `jobstart`) isn't wired to Pyodide yet (a later phase) —
    // fail loud rather than silently dropping the spawn (CLAUDE.md).
    proc(reqs) {
      if (reqs.spawn.length) {
        ctx.reportError(
          "local process host: vim.system / jobstart is not available yet (lands in a later phase)",
        );
      }
    },

    // The LSP leg (basedpyright) isn't wired yet (a later phase) — fail loud, not a silent drop.
    lsp(reqs) {
      if (reqs.spawn.length) {
        ctx.reportError(
          "local process host: LSP (basedpyright) is not available yet (lands in a later phase)",
        );
      }
    },
  };
}
