// pyodide-worker.mjs — the in-browser python interpreter that backs `:terminal python …`
// when there is no daemon (the *local* process host; see web/local-host.mjs and
// docs/plans/2026-06-23-web-python-demo.md). It runs in its **own** Web Worker, separate from
// the editor Worker, so a long-running (or `while True:`) script can't freeze the editor's run
// loop. Pyodide is CPython 3.x compiled to wasm, self-hosted from web/vendor/pyodide/ so COEP
// `require-corp` is satisfied — no CDN. Loaded **lazily** on the first `open`.
//
// Two modes, both driven through the terminal seam:
//   - `python <file>`  → run a script to completion (Phase 1).
//   - `python` (bare)  → an interactive REPL (Phase 2): keystrokes (`write`) drive a line
//                         editor; completed statements run synchronously via Python's `codeop`
//                         (multiline detection, displayhook echo), and Ctrl-C interrupts a
//                         running computation via a SharedArrayBuffer.
//
// Protocol (postMessage, ferried by the editor Worker to/from the terminal seam):
//   in : {type:'open',  buf, argv, cwd}   — `:terminal <argv>`
//        {type:'write', buf, bytes}       — keystrokes to the child (REPL line editing / stdin)
//        {type:'kill',  buf}              — the terminal closed
//   out: {type:'data',  buf, bytes}       — child output (raw bytes; \n already → \r\n)
//        {type:'exit',  buf, code}        — the child exited with `code`
//        {type:'interrupt-buffer', buffer}— the SAB the editor writes 2 (SIGINT) into for Ctrl-C
//        {type:'error', error}            — a host-level failure (fail loud, per CLAUDE.md)
//
// The editor Worker lands `data`/`exit` as `term_data`/`term_exit` (the daemon `:terminal` leg's
// landing) so the wasm vt100 emulator renders them. Ctrl-C can't ride `write` while python is in
// a tight loop (the worker's JS event loop is blocked in the wasm call), so the editor writes the
// SIGINT into the shared `interrupt-buffer` instead, which CPython polls at bytecode boundaries.

let pyodide = null; // the loaded runtime (null until the first `open`)
let loading = null; // the in-flight load promise (so concurrent opens share one load)
let curBuf = null; // the script-mode terminal buffer stdout streams to (one run at a time)
let interruptBuffer = null; // Uint8Array over a SAB; the editor writes 2 to request a SIGINT

const enc = new TextEncoder();
const dec = new TextDecoder();

// Stream a chunk of the child's output to the editor Worker. The vt100 emulator needs a carriage
// return to return to column 0 (a bare \n only line-feeds, which would stair-step the text — a
// real PTY's ONLCR does this for us), so translate every \n to \r\n. Callers pass plain \n.
function emit(buf, text) {
  if (buf == null || text === "") return;
  const bytes = enc.encode(text.replace(/\n/g, "\r\n"));
  postMessage({ type: "data", buf, bytes }, [bytes.buffer]);
}

// Pyodide's `batched` stdout/stderr callback delivers a complete line with its trailing newline
// stripped (one callback ≈ one line), so re-add the line terminator before streaming it.
function emitLine(buf, s) {
  emit(buf, s + "\n");
}

// Load Pyodide once: set up the SAB interrupt buffer, the script-mode stdout routing, the OPFS
// mount at /project (so `python /foo.py` sees the editor's files), and the script runner.
async function ensurePyodide() {
  if (pyodide) return pyodide;
  if (loading) return loading;
  loading = (async () => {
    const { loadPyodide } = await import("./vendor/pyodide/pyodide.mjs");
    const py = await loadPyodide({ indexURL: new URL("./vendor/pyodide/", import.meta.url).href });
    // Ctrl-C: CPython polls this shared buffer at bytecode boundaries, so a `while True:` can be
    // interrupted by the editor Worker writing 2 (SIGINT) into it from another thread — the only
    // path that works while this worker's JS event loop is blocked inside the wasm call.
    interruptBuffer = new Uint8Array(new SharedArrayBuffer(1));
    py.setInterruptBuffer(interruptBuffer);
    postMessage({ type: "interrupt-buffer", buffer: interruptBuffer.buffer });
    // Script-mode stdout/stderr → the active run's buffer.
    py.setStdout({ batched: (s) => emitLine(curBuf, s) });
    py.setStderr({ batched: (s) => emitLine(curBuf, s) });
    // Mount the origin's OPFS root at /project (NATIVEFS_ASYNC; no prompt for OPFS). The editor's
    // files live at the OPFS root, so an editor path `/main.py` maps to `/project/main.py`.
    const root = await navigator.storage.getDirectory();
    py.FS.mkdirTree("/project");
    await py.mountNativeFS("/project", root);
    // Line-buffer stdout/stderr so each print flushes immediately (without a TTY python would
    // block-buffer and the tail would flush only at teardown, after we post the exit). The runner
    // runs a file as __main__, translating SystemExit / exceptions into an exit code.
    py.runPython(`
import sys, runpy, traceback, codeop
try:
    sys.stdout.reconfigure(line_buffering=True)
    sys.stderr.reconfigure(line_buffering=True)
except Exception:
    pass
def __nx_run(path):
    sys.argv = [path]
    try:
        runpy.run_path(path, run_name="__main__")
        return 0
    except SystemExit as e:
        if e.code is None: return 0
        return e.code if isinstance(e.code, int) else 1
    except BaseException:
        traceback.print_exc()
        return 1
    finally:
        sys.stdout.flush()
        sys.stderr.flush()

# Interactive REPL, run SYNCHRONOUSLY (so a busy loop blocks this worker and the SAB interrupt
# raises a *catchable* KeyboardInterrupt — async execution surfaces it out-of-band). 'single'
# compile mode routes an expression statement's value through sys.displayhook → repr to stdout,
# exactly like the real REPL. Returns (status, text): incomplete / error(+traceback) / exit / ok.
__nx_repl_compile = codeop.CommandCompiler()
def __nx_repl_feed(ns, src):
    try:
        code = __nx_repl_compile(src, "<console>", "single")
    except (SyntaxError, OverflowError, ValueError):
        return ("error", traceback.format_exc())
    if code is None:
        return ("incomplete", "")
    try:
        exec(code, ns)
    except SystemExit:
        return ("exit", "")
    except BaseException as e:
        # Drop this function's own exec() frame so the traceback reads like the real REPL
        # (just the user's <console> frames), then format it.
        tb = e.__traceback__.tb_next if e.__traceback__ else None
        return ("error", "".join(traceback.format_exception(type(e), e, tb)))
    finally:
        sys.stdout.flush(); sys.stderr.flush()
    return ("ok", "")
`);
    pyodide = py;
    return py;
  })();
  return loading;
}

// Map an editor/OPFS path to its mount path under /project. Absolute OPFS paths (`/main.py`)
// rebase onto the mount; a bare/relative name resolves against the project root too.
function projectPath(p) {
  if (p.startsWith("/project/") || p === "/project") return p;
  return "/project/" + p.replace(/^\/+/, "");
}

// ── script mode: `python <file>` ─────────────────────────────────────────────────────────────
async function runScript(buf, argv) {
  curBuf = buf;
  let py;
  try {
    emit(buf, "loading python (first run)…\n");
    py = await ensurePyodide();
  } catch (e) {
    postMessage({ type: "error", error: `pyodide load failed: ${e && e.message ? e.message : e}` });
    postMessage({ type: "exit", buf, code: 1 });
    return;
  }
  curBuf = buf;
  const path = projectPath(String(argv[1]));
  let code = 0;
  try {
    code = await py.runPythonAsync(`__nx_run(${JSON.stringify(path)})`);
  } catch (e) {
    emit(buf, `${e && e.message ? e.message : e}\n`);
    code = 1;
  }
  postMessage({ type: "exit", buf, code: Number(code) | 0 });
  if (curBuf === buf) curBuf = null;
}

// ── REPL mode: bare `python` ─────────────────────────────────────────────────────────────────
// A minimal line discipline (the host is the terminal driver here): echo printable keys, handle
// Backspace / Enter / Ctrl-C / Ctrl-D, accumulate a (possibly multiline) statement, and feed it
// to the synchronous `__nx_repl_feed` runner once `codeop` says it's complete.
let repl = null; // { buf, feed, ns, src, line: number[], cont, running } when a REPL is open

async function startRepl(buf) {
  let py;
  try {
    emit(buf, "loading python (first run)…\n");
    py = await ensurePyodide();
  } catch (e) {
    postMessage({ type: "error", error: `pyodide load failed: ${e && e.message ? e.message : e}` });
    postMessage({ type: "exit", buf, code: 1 });
    return;
  }
  curBuf = buf; // the REPL's stdout/stderr (print, the displayhook repr) route here
  repl = {
    buf,
    feed: py.globals.get("__nx_repl_feed"),
    ns: py.runPython("dict(__name__='__main__')"),
    src: "",
    line: [],
    cont: false,
    running: false,
  };
  emit(buf, `Python ${py.runPython("__import__('sys').version.split()[0]")} (Pyodide) — Ctrl-C interrupts, Ctrl-D exits\n`);
  prompt();
}

function prompt() {
  if (repl) emit(repl.buf, repl.cont ? "... " : ">>> ");
}

// Submit the accumulated statement. Runs SYNCHRONOUSLY (blocking this worker), so a `while True:`
// is interruptible by the SAB SIGINT the editor writes, and an expression's value auto-displays
// via the displayhook (→ stdout → the terminal). incomplete → keep buffering (continuation).
function evalStatement() {
  if (interruptBuffer) Atomics.store(interruptBuffer, 0, 0); // clear any stale SIGINT before running
  repl.running = true;
  let status = "ok";
  let text = "";
  try {
    const r = repl.feed(repl.ns, repl.src);
    [status, text] = r.toJs();
    r.destroy();
  } catch (e) {
    // A KeyboardInterrupt from a sync run can still escape here; treat it as one.
    text = `${e && e.message ? e.message : e}\n`;
    status = "error";
  }
  repl.running = false;
  if (status === "incomplete") {
    repl.cont = true;
    prompt();
    return;
  }
  if (text) emit(repl.buf, text); // a traceback (already newline-terminated)
  repl.src = "";
  repl.cont = false;
  if (status === "exit") {
    const buf = repl.buf;
    repl = null;
    postMessage({ type: "exit", buf, code: 0 });
    return;
  }
  prompt();
}

// REPL keystroke handling (cooked-mode line editing the host performs in lieu of a PTY).
function replWrite(bytes) {
  for (let i = 0; i < bytes.length; i++) {
    const b = bytes[i];
    if (b === 0x0d || b === 0x0a) {
      // Enter: echo the newline, append the line to the pending statement, and try to run it.
      emit(repl.buf, "\n");
      repl.src += dec.decode(new Uint8Array(repl.line)) + "\n";
      repl.line = [];
      evalStatement();
    } else if (b === 0x7f || b === 0x08) {
      // Backspace / Delete: erase the last char on the screen and in the buffer.
      if (repl.line.length) {
        repl.line.pop();
        emit(repl.buf, "\b \b");
      }
    } else if (b === 0x03) {
      // Ctrl-C: a running computation was already interrupted via the SAB by the editor Worker;
      // here (at the prompt) just cancel the current line / pending block and start fresh.
      if (!repl.running) {
        repl.line = [];
        repl.src = "";
        repl.cont = false;
        emit(repl.buf, "^C\n");
        prompt();
      }
    } else if (b === 0x04) {
      // Ctrl-D: EOF on an empty line ends the REPL.
      if (repl.line.length === 0 && repl.src === "" && !repl.running) {
        emit(repl.buf, "\n");
        const buf = repl.buf;
        repl = null;
        postMessage({ type: "exit", buf, code: 0 });
        return;
      }
    } else if (b >= 0x20) {
      // Printable: buffer it and echo (bytes are accumulated and UTF-8-decoded on submit; the
      // echo is per-byte, fine for ASCII — the common REPL case).
      repl.line.push(b);
      emit(repl.buf, String.fromCharCode(b));
    }
  }
}

// `:terminal <argv>`. `python <file>` runs a script; bare `python` opens the REPL; anything else
// fails loud rather than silently doing nothing.
async function open(buf, argv) {
  if (!Array.isArray(argv) || argv.length === 0 || argv[0] !== "python") {
    emit(buf, `nxvim web demo: the in-browser terminal runs \`python [file]\` (got: ${(argv || []).join(" ") || "<shell>"})\n`);
    postMessage({ type: "exit", buf, code: 127 });
    return;
  }
  if (argv.length >= 2 && argv[1] !== "-") {
    await runScript(buf, argv);
  } else {
    await startRepl(buf);
  }
}

onmessage = (ev) => {
  const m = ev.data;
  if (m.type === "open") {
    open(m.buf, m.argv).catch((e) =>
      postMessage({ type: "error", error: `pyodide open: ${e && e.message ? e.message : e}` }),
    );
  } else if (m.type === "write") {
    // Keystrokes to the child. Meaningful in REPL mode (line editing); a running script ignores
    // stdin for now (input() lands in a later phase).
    if (repl && repl.buf === m.buf) replWrite(new Uint8Array(m.bytes));
  } else if (m.type === "kill") {
    if (repl && repl.buf === m.buf) repl = null;
    if (curBuf === m.buf) curBuf = null;
  }
};
