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
let procRun = null; // PyProxy of __btv_proc_run, the async-proc (`vim.system`/`jobstart`) runner
let shExec = null; // PyProxy of __btv_sh_exec, the minimal-shell line executor (bare `:terminal`)
let nativeFs = null; // mountNativeFS handle; nativeFs.syncfs() persists shell FS writes back to OPFS
const procKills = new Set(); // proc ids asked to stop before their run could begin

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
    // Keep the mount handle: nativeFs.syncfs() writes the shell's FS mutations back to OPFS
    // (reads through the mount are already live; writes need an explicit sync).
    nativeFs = await py.mountNativeFS("/project", root);
    // Line-buffer stdout/stderr so each print flushes immediately (without a TTY python would
    // block-buffer and the tail would flush only at teardown, after we post the exit). The runner
    // runs a file as __main__, translating SystemExit / exceptions into an exit code.
    py.runPython(`
import io, os, sys, runpy, traceback, codeop, contextlib
try:
    sys.stdout.reconfigure(line_buffering=True)
    sys.stderr.reconfigure(line_buffering=True)
except Exception:
    pass
def __btv_run(path):
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
__btv_repl_compile = codeop.CommandCompiler()
def __btv_repl_feed(ns, src):
    try:
        code = __btv_repl_compile(src, "<console>", "single")
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

# Async-proc leg (vim.system / jobstart). Unlike the terminal — one merged PTY stream — a proc
# run captures stdout and stderr SEPARATELY and reports an exit code with both, mirroring the
# daemon's host.rs contract. A streaming run (btv.run_stream) instead pushes newline-stripped
# stdout lines through 'emit' as they are produced, and returns empty stdout with the exit (the
# output was already streamed) — exactly as stream_local_process does.
class _BtvProcOut:
    def __init__(self, emit):
        self._emit = emit        # callback(list[str]) per completed-line flush, or None to capture
        self._parts = []         # full capture (non-streaming) joined at the end
        self._pending = ""       # partial trailing line carried until its newline arrives (streaming)
    def write(self, s):
        s = str(s)
        if self._emit is None:
            self._parts.append(s)
        elif s:
            self._pending += s
            if "\\n" in self._pending:
                *lines, self._pending = self._pending.split("\\n")
                self._emit([ln.rstrip("\\r") for ln in lines])
        return len(s)
    def flush(self):
        pass
    def getvalue(self):
        return "".join(self._parts)
    def finish(self):  # streaming: flush a final unterminated line so its bytes aren't lost
        if self._emit is not None and self._pending:
            self._emit([self._pending.rstrip("\\r")])
            self._pending = ""

def __btv_proc_run(kind, payload, sys_argv, stdin_text, env_pairs, stream, emit, cwd):
    out = _BtvProcOut(emit if stream else None)
    err = io.StringIO()
    old_argv, old_stdin = sys.argv, sys.stdin
    old_cwd = None
    saved_env = []  # (key, prior value or None) to restore after the run, so env doesn't leak
    code = 0
    try:
        for pair in (env_pairs or []):
            k, v = str(pair[0]), str(pair[1])
            saved_env.append((k, os.environ.get(k)))
            os.environ[k] = v
        try:
            old_cwd = os.getcwd()
            os.chdir(cwd)
        except Exception:
            old_cwd = None
        sys.argv = list(sys_argv)
        sys.stdin = io.StringIO(stdin_text or "")
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                if kind == "file":
                    runpy.run_path(payload, run_name="__main__")
                else:
                    exec(compile(payload, "<string>", "exec"), {"__name__": "__main__"})
            except SystemExit as e:
                c = e.code
                code = 0 if c is None else (c if isinstance(c, int) else 1)
            except KeyboardInterrupt:
                err.write("\\nKeyboardInterrupt\\n")
                code = 130
            except BaseException:
                traceback.print_exc(file=err)
                code = 1
        out.finish()
    finally:
        sys.argv, sys.stdin = old_argv, old_stdin
        for k, old in saved_env:
            if old is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = old
        if old_cwd is not None:
            try:
                os.chdir(old_cwd)
            except Exception:
                pass
    # Non-streaming returns the full captured stdout; streaming already pushed it (empty here).
    stdout_bytes = b"" if stream else out.getvalue().encode("utf-8", "replace")
    return (code, stdout_bytes, err.getvalue().encode("utf-8", "replace"))
`);
    // The minimal POSIX-ish shell that backs a bare `:terminal`. Implemented in python so its
    // builtins share the interpreter's exact FS view (the /project mount). The JS side does only
    // line editing + syncfs; __btv_sh_exec(line) parses + runs one command line and returns
    // [stdout, stderr, code, exit_flag, cwd_display].
    py.runPython(`
import shlex as _shlex, glob as _glob, shutil as _shutil

class _Shell:
    def __init__(self):
        self.cwd = "/project"
        self.env = {"HOME": "/project", "PWD": "/project"}

_sh = _Shell()

def _disp(p):
    # mount path -> editor path for display ("/project/x" -> "/x", "/project" -> "/")
    if p == "/project":
        return "/"
    if p.startswith("/project/"):
        return p[len("/project"):]
    return p

def _resolve(arg, cwd):
    # editor/relative path -> normalized mount path, clamped so it can never escape /project
    if not arg:
        return cwd
    if arg.startswith("/"):
        p = os.path.normpath("/project" + arg)
    else:
        p = os.path.normpath(os.path.join(cwd, arg))
    if p != "/project" and not p.startswith("/project/"):
        return "/project"
    return p

def _tok(line):
    lex = _shlex.shlex(line, posix=True, punctuation_chars=True)
    lex.whitespace_split = True
    return list(lex)

def _expand(tokens, cwd):
    out = []
    for t in tokens:
        # expand dollar-VAR and curly-brace forms from the shell env
        s = ""
        i = 0
        while i < len(t):
            if t[i] == "$" and i + 1 < len(t):
                j = i + 1
                if t[j] == "{":
                    k = t.find("}", j)
                    if k != -1:
                        s += _sh.env.get(t[j + 1:k], "")
                        i = k + 1
                        continue
                k = j
                while k < len(t) and (t[k].isalnum() or t[k] == "_"):
                    k += 1
                s += _sh.env.get(t[j:k], "")
                i = k
                continue
            s += t[i]
            i += 1
        if any(c in s for c in "*?["):
            pat = s if s.startswith("/") else os.path.join(cwd, s)
            matches = _glob.glob(pat)
            if matches:
                out.extend(sorted(_disp(m) if s.startswith("/") else os.path.relpath(m, cwd) for m in matches))
                continue
        out.append(s)
    return out

def _read_inputs(files, sin, cwd):
    if not files:
        return sin
    txt = ""
    for x in files:
        with open(_resolve(x, cwd)) as f:
            txt += f.read()
    return txt

def _bi_pwd(a, sin, cwd):
    return (_disp(cwd) + "\\n", "", 0)

def _bi_cd(a, sin, cwd):
    target = a[0] if a else "/"
    p = _resolve(target, cwd)
    if os.path.isdir(p):
        _sh.cwd = p
        _sh.env["PWD"] = p
        return ("", "", 0)
    return ("", "cd: " + target + ": No such directory\\n", 1)

def _bi_echo(a, sin, cwd):
    nl = "\\n"
    if a and a[0] == "-n":
        nl = ""
        a = a[1:]
    return (" ".join(a) + nl, "", 0)

def _bi_ls(a, sin, cwd):
    show_all = False
    paths = []
    for x in a:
        if x.startswith("-"):
            if "a" in x:
                show_all = True
        else:
            paths.append(x)
    if not paths:
        paths = ["."]
    out = ""
    err = ""
    code = 0
    for x in paths:
        p = _resolve(x, cwd)
        try:
            if os.path.isdir(p):
                for n in sorted(os.listdir(p)):
                    if not show_all and n.startswith("."):
                        continue
                    out += n + ("/" if os.path.isdir(os.path.join(p, n)) else "") + "\\n"
            elif os.path.exists(p):
                out += x + "\\n"
            else:
                err += "ls: " + x + ": No such file or directory\\n"
                code = 1
        except Exception as e:
            err += "ls: " + str(e) + "\\n"
            code = 1
    return (out, err, code)

def _bi_cat(a, sin, cwd):
    if not a:
        return (sin, "", 0)
    out = ""
    err = ""
    code = 0
    for x in a:
        try:
            with open(_resolve(x, cwd)) as f:
                out += f.read()
        except Exception:
            err += "cat: " + x + ": No such file or directory\\n"
            code = 1
    return (out, err, code)

def _bi_mkdir(a, sin, cwd):
    parents = False
    targets = []
    for x in a:
        if x == "-p":
            parents = True
        elif not x.startswith("-"):
            targets.append(x)
    err = ""
    code = 0
    for x in targets:
        p = _resolve(x, cwd)
        try:
            if parents:
                os.makedirs(p, exist_ok=True)
            else:
                os.mkdir(p)
        except FileExistsError:
            err += "mkdir: " + x + ": File exists\\n"
            code = 1
        except Exception as e:
            err += "mkdir: " + x + ": " + str(e) + "\\n"
            code = 1
    return ("", err, code)

def _bi_rm(a, sin, cwd):
    rec = False
    force = False
    targets = []
    for x in a:
        if x.startswith("-"):
            if "r" in x or "R" in x:
                rec = True
            if "f" in x:
                force = True
        else:
            targets.append(x)
    err = ""
    code = 0
    for x in targets:
        p = _resolve(x, cwd)
        try:
            if os.path.isdir(p):
                if rec:
                    _shutil.rmtree(p)
                else:
                    err += "rm: " + x + ": is a directory\\n"
                    code = 1
            elif os.path.exists(p):
                os.remove(p)
            elif not force:
                err += "rm: " + x + ": No such file or directory\\n"
                code = 1
        except Exception as e:
            err += "rm: " + x + ": " + str(e) + "\\n"
            code = 1
    return ("", err, code)

def _bi_mv(a, sin, cwd):
    rest = [x for x in a if not x.startswith("-")]
    if len(rest) < 2:
        return ("", "mv: missing operand\\n", 1)
    dst = _resolve(rest[-1], cwd)
    err = ""
    code = 0
    for s in rest[:-1]:
        try:
            _shutil.move(_resolve(s, cwd), dst)
        except Exception as e:
            err += "mv: " + s + ": " + str(e) + "\\n"
            code = 1
    return ("", err, code)

def _bi_cp(a, sin, cwd):
    rec = False
    rest = []
    for x in a:
        if x.startswith("-"):
            if "r" in x or "R" in x:
                rec = True
        else:
            rest.append(x)
    if len(rest) < 2:
        return ("", "cp: missing operand\\n", 1)
    dst = _resolve(rest[-1], cwd)
    err = ""
    code = 0
    for s in rest[:-1]:
        sp = _resolve(s, cwd)
        try:
            if os.path.isdir(sp):
                if rec:
                    target = os.path.join(dst, os.path.basename(sp)) if os.path.isdir(dst) else dst
                    _shutil.copytree(sp, target)
                else:
                    err += "cp: " + s + ": is a directory\\n"
                    code = 1
            else:
                _shutil.copy(sp, dst)
        except Exception as e:
            err += "cp: " + s + ": " + str(e) + "\\n"
            code = 1
    return ("", err, code)

def _bi_touch(a, sin, cwd):
    err = ""
    code = 0
    for x in a:
        if x.startswith("-"):
            continue
        p = _resolve(x, cwd)
        try:
            with open(p, "a"):
                os.utime(p, None)
        except Exception as e:
            err += "touch: " + x + ": " + str(e) + "\\n"
            code = 1
    return ("", err, code)

def _bi_head(a, sin, cwd):
    n = 10
    files = []
    i = 0
    while i < len(a):
        if a[i] == "-n" and i + 1 < len(a):
            n = int(a[i + 1])
            i += 2
            continue
        if a[i].startswith("-") and a[i][1:].isdigit():
            n = int(a[i][1:])
            i += 1
            continue
        files.append(a[i])
        i += 1
    try:
        txt = _read_inputs(files, sin, cwd)
    except Exception:
        return ("", "head: cannot open input\\n", 1)
    lines = txt.splitlines()[:n]
    return (("\\n".join(lines) + "\\n") if lines else "", "", 0)

def _bi_tail(a, sin, cwd):
    n = 10
    files = []
    i = 0
    while i < len(a):
        if a[i] == "-n" and i + 1 < len(a):
            n = int(a[i + 1])
            i += 2
            continue
        if a[i].startswith("-") and a[i][1:].isdigit():
            n = int(a[i][1:])
            i += 1
            continue
        files.append(a[i])
        i += 1
    try:
        txt = _read_inputs(files, sin, cwd)
    except Exception:
        return ("", "tail: cannot open input\\n", 1)
    lines = txt.splitlines()[-n:]
    return (("\\n".join(lines) + "\\n") if lines else "", "", 0)

def _bi_wc(a, sin, cwd):
    flags = [x for x in a if x.startswith("-")]
    files = [x for x in a if not x.startswith("-")]
    try:
        txt = _read_inputs(files, sin, cwd)
    except Exception:
        return ("", "wc: cannot open input\\n", 1)
    lc = len(txt.splitlines())
    wc = len(txt.split())
    cc = len(txt)
    if "-l" in flags:
        return (str(lc) + "\\n", "", 0)
    if "-w" in flags:
        return (str(wc) + "\\n", "", 0)
    if "-c" in flags:
        return (str(cc) + "\\n", "", 0)
    return ("%d %d %d\\n" % (lc, wc, cc), "", 0)

def _bi_env(a, sin, cwd):
    return ("".join(k + "=" + v + "\\n" for k, v in sorted(_sh.env.items())), "", 0)

def _bi_clear(a, sin, cwd):
    return ("\\x1b[2J\\x1b[H", "", 0)

def _bi_which(a, sin, cwd):
    out = ""
    code = 0
    for x in a:
        if x == "python":
            out += "python\\n"
        elif x in _BUILTINS or x in ("exit", "export", "cd"):
            out += x + ": shell builtin\\n"
        else:
            code = 1
    return (out, "", code)

_BUILTINS = {
    "pwd": _bi_pwd, "cd": _bi_cd, "echo": _bi_echo, "ls": _bi_ls, "cat": _bi_cat,
    "mkdir": _bi_mkdir, "rm": _bi_rm, "mv": _bi_mv, "cp": _bi_cp, "touch": _bi_touch,
    "head": _bi_head, "tail": _bi_tail, "wc": _bi_wc, "env": _bi_env, "clear": _bi_clear,
    "which": _bi_which,
}

def _run_python(a, sin, cwd):
    out = io.StringIO()
    err = io.StringIO()
    old_argv, old_stdin, old_cwd = sys.argv, sys.stdin, os.getcwd()
    code = 0
    try:
        os.chdir(cwd)
        sys.stdin = io.StringIO(sin)
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            if len(a) >= 3 and a[1] == "-c":
                sys.argv = ["-c"] + list(a[3:])
                exec(compile(a[2], "<string>", "exec"), {"__name__": "__main__"})
            elif len(a) >= 2 and a[1] == "-":
                exec(compile(sin, "<stdin>", "exec"), {"__name__": "__main__"})
            elif len(a) >= 2:
                sys.argv = [a[1]] + list(a[2:])
                runpy.run_path(_resolve(a[1], cwd), run_name="__main__")
            else:
                err.write("python: no REPL inside the shell — use \\\`:terminal python\\\`, or python <file>/-c\\n")
                code = 127
    except SystemExit as e:
        code = 0 if e.code is None else (e.code if isinstance(e.code, int) else 1)
    except BaseException:
        traceback.print_exc(file=err)
        code = 1
    finally:
        sys.argv, sys.stdin = old_argv, old_stdin
        try:
            os.chdir(old_cwd)
        except Exception:
            pass
    return (out.getvalue(), err.getvalue(), code)

def _run_simple(cmd, sin):
    # leading VAR=val assignments (command-scoped, unless bare → persisted)
    assigns = {}
    while cmd and ("=" in cmd[0]) and not cmd[0].startswith("=") and cmd[0].split("=", 1)[0].replace("_", "").isalnum():
        k, v = cmd[0].split("=", 1)
        assigns[k] = v
        cmd = cmd[1:]
    if not cmd:
        for k, v in assigns.items():
            _sh.env[k] = v
        return ("", "", 0, False)
    saved = {k: _sh.env.get(k) for k in assigns}
    _sh.env.update(assigns)
    try:
        args = _expand(cmd, _sh.cwd)
    finally:
        for k, old in saved.items():
            if old is None:
                _sh.env.pop(k, None)
            else:
                _sh.env[k] = old
    name = args[0]
    rest = args[1:]
    if name == "exit":
        return ("", "", int(rest[0]) if rest and rest[0].lstrip("-").isdigit() else 0, True)
    if name == "export":
        for x in rest:
            if "=" in x:
                k, v = x.split("=", 1)
                _sh.env[k] = v
        return ("", "", 0, False)
    bi = _BUILTINS.get(name)
    if bi:
        try:
            o, e, c = bi(rest, sin, _sh.cwd)
            return (o, e, c, False)
        except Exception as ex:
            return ("", name + ": " + str(ex) + "\\n", 1, False)
    if name == "python":
        o, e, c = _run_python(args, sin, _sh.cwd)
        return (o, e, c, False)
    return ("", name + ": command not found\\n", 127, False)

def _split_redir(st):
    cmd = []
    rin = rout = None
    app = False
    i = 0
    while i < len(st):
        t = st[i]
        if t == ">" and i + 1 < len(st):
            rout = st[i + 1]
            app = False
            i += 2
        elif t == ">>" and i + 1 < len(st):
            rout = st[i + 1]
            app = True
            i += 2
        elif t == "<" and i + 1 < len(st):
            rin = st[i + 1]
            i += 2
        else:
            cmd.append(t)
            i += 1
    return cmd, rin, rout, app

def _run_pipeline(toks):
    stages = []
    cur = []
    for t in toks:
        if t == "|":
            stages.append(cur)
            cur = []
        else:
            cur.append(t)
    stages.append(cur)
    data = ""
    err = ""
    code = 0
    for st in stages:
        cmd, rin, rout, app = _split_redir(st)
        if not cmd:
            continue
        sin = data
        if rin is not None:
            try:
                with open(_resolve(rin, _sh.cwd)) as f:
                    sin = f.read()
            except Exception:
                return ("", "shell: " + rin + ": No such file\\n", 1, False)
        o, e, code, ex = _run_simple(cmd, sin)
        err += e
        if ex:
            return (o, err, code, True)
        if rout is not None:
            try:
                with open(_resolve(rout, _sh.cwd), "a" if app else "w") as f:
                    f.write(o)
                o = ""
            except Exception as ee:
                err += "shell: " + rout + ": " + str(ee) + "\\n"
                code = 1
        data = o
    return (data, err, code, False)

def __btv_sh_exec(line):
    line = line.strip()
    if not line:
        return ["", "", 0, False, _disp(_sh.cwd)]
    try:
        toks = _tok(line)
    except ValueError as e:
        return ["", "shell: " + str(e) + "\\n", 2, False, _disp(_sh.cwd)]
    segs = []
    cur = []
    conn = ";"
    for t in toks:
        if t in (";", "&&", "||"):
            segs.append((conn, cur))
            cur = []
            conn = t
        else:
            cur.append(t)
    segs.append((conn, cur))
    out = ""
    err = ""
    last = 0
    do_exit = False
    for conn, stoks in segs:
        if not stoks:
            continue
        if conn == "&&" and last != 0:
            continue
        if conn == "||" and last == 0:
            continue
        o, e, c, ex = _run_pipeline(stoks)
        out += o
        err += e
        last = c
        if ex:
            do_exit = True
            break
    return [out, err, int(last), bool(do_exit), _disp(_sh.cwd)]
`);
    procRun = py.globals.get("__btv_proc_run");
    shExec = py.globals.get("__btv_sh_exec");
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
    code = await py.runPythonAsync(`__btv_run(${JSON.stringify(path)})`);
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
// to the synchronous `__btv_repl_feed` runner once `codeop` says it's complete.
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
    feed: py.globals.get("__btv_repl_feed"),
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

// ── shell mode: bare `:terminal` ─────────────────────────────────────────────────────────────
// A minimal POSIX-ish shell. Same cooked-mode line discipline as the REPL, but each completed
// line is run by the python executor `__btv_sh_exec` (builtins + pipelines + `python` stages), and
// FS mutations are flushed back to OPFS with syncfs after every line.
let shell = null; // { buf, line: number[], running, cwd } when a shell is open

async function startShell(buf) {
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
  shell = { buf, line: [], running: false, cwd: "/" };
  emit(buf, "bemtvi shell — builtins (ls cat cd echo mkdir rm …), pipes/redirects, `python <file>`; Ctrl-D exits\n");
  shellPrompt();
}

function shellPrompt() {
  if (shell) emit(shell.buf, `${shell.cwd} $ `);
}

// Run one accumulated command line: a SYNC python call (so a runaway `python` stage is Ctrl-C-able
// via the SAB), then syncfs to persist any file writes back to OPFS.
async function shellSubmit(line) {
  if (interruptBuffer) Atomics.store(interruptBuffer, 0, 0); // clear any stale SIGINT
  shell.running = true;
  let out = "", err = "", code = 0, ex = false, cwd = shell.cwd;
  try {
    const r = shExec(line);
    [out, err, code, ex, cwd] = r.toJs();
    r.destroy();
  } catch (e) {
    err = `${e && e.message ? e.message : e}\n`;
    code = 1;
  }
  shell.running = false;
  shell.cwd = cwd;
  if (out) emit(shell.buf, out);
  if (err) emit(shell.buf, err);
  try {
    if (nativeFs) await nativeFs.syncfs();
  } catch (e) {
    emit(shell.buf, `shell: syncfs failed: ${e && e.message ? e.message : e}\n`);
  }
  if (!shell) return; // a kill arrived mid-run
  if (ex) {
    const b = shell.buf;
    shell = null;
    postMessage({ type: "exit", buf: b, code });
    return;
  }
  shellPrompt();
}

function shellWrite(bytes) {
  for (let i = 0; i < bytes.length; i++) {
    const b = bytes[i];
    if (b === 0x0d || b === 0x0a) {
      emit(shell.buf, "\n");
      if (shell.running) continue; // ignore Enter while a command runs
      const line = dec.decode(new Uint8Array(shell.line));
      shell.line = [];
      shellSubmit(line);
    } else if (b === 0x7f || b === 0x08) {
      if (shell.line.length) {
        shell.line.pop();
        emit(shell.buf, "\b \b");
      }
    } else if (b === 0x03) {
      // Ctrl-C: a running stage was already SIGINT'd via the SAB; at the prompt, cancel the line.
      if (!shell.running) {
        shell.line = [];
        emit(shell.buf, "^C\n");
        shellPrompt();
      }
    } else if (b === 0x04) {
      // Ctrl-D on an empty line ends the shell.
      if (shell.line.length === 0 && !shell.running) {
        emit(shell.buf, "\n");
        const buf = shell.buf;
        shell = null;
        postMessage({ type: "exit", buf, code: 0 });
        return;
      }
    } else if (b >= 0x20) {
      shell.line.push(b);
      emit(shell.buf, String.fromCharCode(b));
    }
  }
}

// `:terminal <argv>`. Bare `:terminal` (empty argv) opens the shell; `python <file>` runs a script;
// bare `python` opens the REPL; anything else fails loud rather than silently doing nothing.
async function open(buf, argv) {
  if (!Array.isArray(argv) || argv.length === 0) {
    await startShell(buf);
    return;
  }
  if (argv[0] !== "python") {
    emit(buf, `${argv.join(" ")}: command not found (this terminal runs the built-in shell or \`python\`)\n`);
    postMessage({ type: "exit", buf, code: 127 });
    return;
  }
  if (argv.length >= 2 && argv[1] !== "-") {
    await runScript(buf, argv);
  } else {
    await startRepl(buf);
  }
}

// ── async-proc leg: `vim.system` / `jobstart` ────────────────────────────────────────────────
// Run one off-tick process spawn against the Pyodide interpreter (the proc twin of `runScript`,
// but with stdout/stderr captured separately and an exit code, per the daemon `host.rs` contract).
// Only `python` is available in this single-interpreter demo; any other argv could not be spawned
// at all, so it reports the canonical spawn failure (`code = -1`, per the daemon contract) rather
// than a shell's command-not-found status — `-1` is what tells a caller the tool never RAN, which
// is how the picker sources know to fall back. Normally unreachable: local-host.mjs answers such a
// spawn itself so a missing binary never boots CPython (this is the same answer, one layer in).
// A streaming spawn (`stream: true`) pushes `proc-stdout` line batches as the run produces them
// and reports empty stdout with the exit; a plain spawn returns the whole captured stdout.
async function runProc(req) {
  const { id, argv, stream } = req;
  // Synthetic pid (there is no OS process) — a positive value so `vim.system().pid` looks spawned.
  postMessage({ type: "proc-spawned", id, pid: 100000 + (Number(id) & 0xffff) });
  if (!Array.isArray(argv) || argv.length === 0 || argv[0] !== "python") {
    const cmd = (argv && argv[0]) || "";
    postMessage({ type: "proc-exited", id, code: -1, stdout: new Uint8Array(0),
      stderr: enc.encode(`bemtvi web demo: only \`python\` runs in the browser process host (no "${cmd}")\n`) });
    return;
  }
  let py;
  try {
    py = await ensurePyodide();
  } catch (e) {
    postMessage({ type: "proc-exited", id, code: 1, stdout: new Uint8Array(0),
      stderr: enc.encode(`pyodide load failed: ${e && e.message ? e.message : e}\n`) });
    return;
  }
  if (procKills.delete(id)) { // killed before its run could begin (-1 = killed, per the leg)
    postMessage({ type: "proc-exited", id, code: -1, stdout: new Uint8Array(0), stderr: new Uint8Array(0) });
    return;
  }
  // Interpret the python invocation: `-c CODE`, a script `FILE`, or source-from-stdin (`python -`).
  const rest = argv.slice(1);
  const stdinText = req.stdin && req.stdin.length ? dec.decode(new Uint8Array(req.stdin)) : "";
  let kind, payload, sysArgv, progStdin;
  if (rest[0] === "-c") {
    kind = "code"; payload = rest[1] ?? ""; sysArgv = ["-c", ...rest.slice(2)]; progStdin = stdinText;
  } else if (rest.length && rest[0] !== "-" && rest[0] !== "") {
    kind = "file"; payload = projectPath(rest[0]); sysArgv = [rest[0], ...rest.slice(1)]; progStdin = stdinText;
  } else {
    kind = "code"; payload = stdinText; sysArgv = ["-"]; progStdin = ""; // the source IS stdin
  }
  const cwd = projectPath(req.cwd || "/");
  const emit = stream
    ? (lines) => {
        const arr = lines.toJs ? lines.toJs() : lines;
        if (lines.destroy) lines.destroy();
        postMessage({ type: "proc-stdout", id, lines: arr });
      }
    : null;
  if (interruptBuffer) Atomics.store(interruptBuffer, 0, 0); // clear a stale SIGINT before running
  let code = 0;
  let stdout = new Uint8Array(0);
  let stderr = new Uint8Array(0);
  try {
    const r = procRun(kind, payload, sysArgv, progStdin, req.env || [], Boolean(stream), emit, cwd);
    const [c, o, e] = r.toJs();
    r.destroy();
    code = Number(c) | 0;
    if (o instanceof Uint8Array) stdout = o;
    if (e instanceof Uint8Array) stderr = e;
  } catch (e) {
    // A KeyboardInterrupt (a Ctrl-C / proc-kill SIGINT) can still escape the python guard.
    stderr = enc.encode(`${e && e.message ? e.message : e}\n`);
    code = 130;
  }
  const transfer = [stdout.buffer, stderr.buffer].filter((b) => b.byteLength);
  postMessage({ type: "proc-exited", id, code, stdout, stderr }, transfer);
}

onmessage = (ev) => {
  const m = ev.data;
  if (m.type === "proc-open") {
    runProc(m).catch((e) =>
      postMessage({ type: "proc-exited", id: m.id, code: 1, stdout: new Uint8Array(0),
        stderr: enc.encode(`pyodide proc: ${e && e.message ? e.message : e}\n`) }),
    );
  } else if (m.type === "proc-kill") {
    // The running computation (if any) was already SIGINT'd via the shared interrupt buffer by the
    // editor Worker; record the id so a not-yet-started proc reports a killed exit when its turn comes.
    procKills.add(m.id);
  } else if (m.type === "open") {
    open(m.buf, m.argv).catch((e) =>
      postMessage({ type: "error", error: `pyodide open: ${e && e.message ? e.message : e}` }),
    );
  } else if (m.type === "write") {
    // Keystrokes to the child. Meaningful in REPL / shell mode (line editing); a running script
    // ignores stdin for now (input() lands in a later phase).
    if (repl && repl.buf === m.buf) replWrite(new Uint8Array(m.bytes));
    else if (shell && shell.buf === m.buf) shellWrite(new Uint8Array(m.bytes));
  } else if (m.type === "kill") {
    if (repl && repl.buf === m.buf) repl = null;
    if (shell && shell.buf === m.buf) shell = null;
    if (curBuf === m.buf) curBuf = null;
  }
};
