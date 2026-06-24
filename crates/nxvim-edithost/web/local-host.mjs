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
// - liveTerms/liveProcs: the Sets the run loop's async-park condition reads (a live terminal or
//                      in-flight child keeps the loop on the non-blocking park so the Worker's
//                      messages are received; the proc one is cleared on `proc_exited`).
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
    // exit, or streaming stdout lines for `nx.run_stream`); the Pyodide Worker answers with
    // `proc-spawned`/`proc-stdout`/`proc-exited` → the daemon leg's `proc_*` landings. `liveProcs`
    // keeps the run loop on its non-blocking park so those pushes are received off the event loop.
    proc(reqs) {
      const w = ensureWorker();
      for (const s of reqs.spawn) {
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
