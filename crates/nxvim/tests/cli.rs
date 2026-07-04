//! Black-box CLI tests: spawn the real `nxvim` binary in its headless roles
//! (`--lua`, flag validation) and assert on exit codes, timing, and on-disk side
//! effects. Unlike `tests/e2e.rs`, nothing here starts the terminal UI, so no PTY
//! is needed and the tests run in CI. Hermetic: every spawn gets a throwaway empty
//! `NXVIM_CONFIG` and its own temp working dir, and the `--lua` role persists no
//! shada, so a run never touches the developer's real config or state.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A fresh, unique temp directory (created) for one test's config / workspace / cwd.
fn fresh_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("nxvim_cli_{tag}_{}_{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A `Command` for the real binary, hermetic: empty config, no runtimepath override,
/// stdio captured (the headless roles never need a terminal).
fn nxvim(cfg: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_nxvim"));
    c.env("NXVIM_CONFIG", cfg);
    c.env_remove("NXVIM_RUNTIMEPATH");
    c.stdin(Stdio::null());
    c.stdout(Stdio::piped());
    c.stderr(Stdio::piped());
    c
}

/// Run to completion with a hard timeout (kill + panic on expiry — a hung role must
/// fail the test, not wedge the suite). Returns (status, stderr, elapsed).
fn run(mut cmd: Command, timeout: Duration) -> (ExitStatus, String, Duration) {
    let start = Instant::now();
    let mut child = cmd.spawn().expect("spawn nxvim");
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            panic!("nxvim did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    (status, stderr, start.elapsed())
}

/// Like [`run`], but also captures stdout — for the `--lua` output-routing tests that
/// assert a script's `print` reaches the process's real stdout. Returns
/// (status, stdout, stderr, elapsed).
fn run_io(mut cmd: Command, timeout: Duration) -> (ExitStatus, String, String, Duration) {
    let start = Instant::now();
    let mut child = cmd.spawn().expect("spawn nxvim");
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            panic!("nxvim did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    (status, stdout, stderr, start.elapsed())
}

/// A `--lua` script's `print(...)` must reach the process's real **stdout** — the
/// headless one-shot has no UI, so output routed only to the message line would vanish.
/// A shell/CI running `nxvim --lua '...print(x)...'` must be able to read it back.
#[test]
fn lua_oneshot_print_goes_to_stdout() {
    let cfg = fresh_dir("cfg");
    let mut cmd = nxvim(&cfg);
    cmd.arg("--lua").arg("print('hello_stdout_marker')");
    let (status, stdout, stderr, _) = run_io(cmd, Duration::from_secs(45));
    assert!(
        status.success(),
        "a printing --lua CODE must exit 0: {status:?} (stderr: {stderr})"
    );
    assert!(
        stdout.contains("hello_stdout_marker"),
        "print output must reach stdout, got stdout: {stdout:?} stderr: {stderr:?}"
    );
    let _ = std::fs::remove_dir_all(&cfg);
}

/// A `--lua` script's error-writer output (`nx.err_writeln`) must reach **stderr**,
/// kept off stdout so a caller consuming the script's stdout as data isn't polluted by
/// diagnostics. (Plain `print` → stdout is covered above.)
#[test]
fn lua_oneshot_err_write_goes_to_stderr() {
    let cfg = fresh_dir("cfg");
    let mut cmd = nxvim(&cfg);
    cmd.arg("--lua")
        .arg("nx.err_writeln('err_stderr_marker'); print('out_stdout_marker')");
    let (status, stdout, stderr, _) = run_io(cmd, Duration::from_secs(45));
    assert!(
        status.success(),
        "the script must exit 0: {status:?} (stderr: {stderr})"
    );
    assert!(
        stderr.contains("err_stderr_marker"),
        "nx.err_writeln output must reach stderr, got stderr: {stderr:?}"
    );
    assert!(
        !stdout.contains("err_stderr_marker"),
        "error output must NOT leak onto stdout, got stdout: {stdout:?}"
    );
    assert!(
        stdout.contains("out_stdout_marker"),
        "plain print must still reach stdout, got stdout: {stdout:?}"
    );
    let _ = std::fs::remove_dir_all(&cfg);
}

/// A `--lua` CODE with a **compile error** must exit non-zero promptly, with the Lua
/// error on stderr — not swallow the failed `nvim_exec_lua`, spin out the full 30s
/// completion poll (the flag-setting chunk never ran), and exit 0 as if it succeeded.
#[test]
fn lua_oneshot_compile_error_exits_nonzero_without_hanging() {
    let cfg = fresh_dir("cfg");
    let mut cmd = nxvim(&cfg);
    cmd.arg("--lua").arg("(");
    let (status, stderr, elapsed) = run(cmd, Duration::from_secs(45));
    assert!(
        !status.success(),
        "invalid --lua CODE must exit non-zero, got {status:?} (stderr: {stderr})"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "invalid --lua CODE must fail fast, not sit out the completion poll ({elapsed:?})"
    );
    assert!(
        !stderr.trim().is_empty(),
        "the Lua load error must be reported on stderr"
    );
    let _ = std::fs::remove_dir_all(&cfg);
}

/// A `--lua` CODE whose evaluation **throws** (or whose promise rejects) must exit
/// non-zero with the error on stderr — a headless one-shot that fails must not report
/// success to the shell (the old path only `nx.notify`d into a UI nobody attached).
#[test]
fn lua_oneshot_runtime_error_exits_nonzero() {
    let cfg = fresh_dir("cfg");
    let mut cmd = nxvim(&cfg);
    cmd.arg("--lua").arg("error('boom_marker')");
    let (status, stderr, _) = run(cmd, Duration::from_secs(45));
    assert!(
        !status.success(),
        "a throwing --lua CODE must exit non-zero (stderr: {stderr})"
    );
    assert!(
        stderr.contains("boom_marker"),
        "the Lua error must reach stderr, got: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&cfg);
}

/// `--lua` accepts statement-form CODE (`return …`, multi-statement chunks) — the
/// natural thing to type — by falling back to compiling CODE as a chunk body when it
/// is not a valid expression. The returned promise is still awaited before exit.
#[test]
fn lua_oneshot_statement_code_works() {
    let cfg = fresh_dir("cfg");
    let dir = fresh_dir("stmt");
    let marker = dir.join("stmt_marker.txt");
    let mut cmd = nxvim(&cfg);
    cmd.arg("--lua").arg(format!(
        "return nx.fs.write('{}', 'ok')",
        marker.to_str().unwrap()
    ));
    let (status, stderr, elapsed) = run(cmd, Duration::from_secs(45));
    assert!(
        status.success(),
        "statement-form --lua CODE must run: {status:?} (stderr: {stderr})"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "statement-form CODE must settle promptly, not poll out the deadline ({elapsed:?})"
    );
    assert!(
        marker.is_file(),
        "the awaited nx.fs.write must have landed before exit"
    );
    let _ = std::fs::remove_dir_all(&cfg);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Guard: expression-form CODE (the documented shape) keeps working, and a promise
/// result is awaited before the process exits.
#[test]
fn lua_oneshot_expression_code_still_works() {
    let cfg = fresh_dir("cfg");
    let dir = fresh_dir("expr");
    let marker = dir.join("expr_marker.txt");
    let mut cmd = nxvim(&cfg);
    cmd.arg("--lua")
        .arg(format!("nx.fs.write('{}', 'ok')", marker.to_str().unwrap()));
    let (status, stderr, _) = run(cmd, Duration::from_secs(45));
    assert!(
        status.success(),
        "expression --lua CODE must run: {status:?} (stderr: {stderr})"
    );
    assert!(
        marker.is_file(),
        "the awaited nx.fs.write must have landed before exit"
    );
    let _ = std::fs::remove_dir_all(&cfg);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--restore-session` must be accepted alongside `--workspace`: a workspace launch
/// derives a shada namespace from the directory, which satisfies the flag's
/// namespace requirement (the old clap `requires = "shada_namespace"` rejected the
/// combination with a usage error even though it is perfectly meaningful).
#[test]
fn restore_session_with_workspace_is_accepted() {
    let cfg = fresh_dir("cfg");
    let ws = fresh_dir("ws");
    let mut cmd = nxvim(&cfg);
    cmd.arg("--workspace")
        .arg(&ws)
        .arg("--restore-session")
        .arg("--lua")
        .arg("1");
    let (status, stderr, _) = run(cmd, Duration::from_secs(45));
    assert!(
        status.success(),
        "--restore-session with --workspace must be accepted: {status:?} (stderr: {stderr})"
    );
    let _ = std::fs::remove_dir_all(&cfg);
    let _ = std::fs::remove_dir_all(&ws);
}

/// Guard: `--restore-session` with neither `--shada-namespace` nor `--workspace` has
/// no namespace to restore from and must still be rejected loudly.
#[test]
fn restore_session_alone_is_still_rejected() {
    let cfg = fresh_dir("cfg");
    let mut cmd = nxvim(&cfg);
    cmd.arg("--restore-session").arg("--lua").arg("1");
    let (status, stderr, elapsed) = run(cmd, Duration::from_secs(45));
    assert!(
        !status.success(),
        "--restore-session without a namespace source must be rejected (stderr: {stderr})"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "the rejection must be immediate ({elapsed:?})"
    );
    let _ = std::fs::remove_dir_all(&cfg);
}

/// `--workspace DIR --lua CODE` runs CODE with the workspace as the working
/// directory, matching the flag's documented "cd into it" and the interactive
/// launch — a relative path in CODE resolves against the workspace root, not
/// wherever the wrapper happened to be started from.
#[test]
fn workspace_lua_oneshot_runs_in_the_workspace_dir() {
    let cfg = fresh_dir("cfg");
    let launch = fresh_dir("launch");
    let ws = fresh_dir("wsdir");
    let mut cmd = nxvim(&cfg);
    cmd.current_dir(&launch)
        .arg("--workspace")
        .arg(&ws)
        .arg("--lua")
        .arg("nx.fs.write('ws_marker.txt', 'hi')");
    let (status, stderr, _) = run(cmd, Duration::from_secs(45));
    assert!(
        status.success(),
        "the workspace one-shot must run: {status:?} (stderr: {stderr})"
    );
    assert!(
        ws.join("ws_marker.txt").is_file(),
        "the relative write must land in the workspace dir"
    );
    assert!(
        !launch.join("ws_marker.txt").exists(),
        "the relative write must NOT land in the launch cwd"
    );
    let _ = std::fs::remove_dir_all(&cfg);
    let _ = std::fs::remove_dir_all(&launch);
    let _ = std::fs::remove_dir_all(&ws);
}

/// R10's surviving guard, PTY-free: internal-looking flags that no longer exist
/// (`--__ts-worker`, removed with the in-process treesitter move) are rejected
/// loudly as unknown options — never silently absorbed into a headless mode that
/// reads stdin as RPC and renders nothing.
#[test]
fn unknown_internal_flag_is_rejected_loudly() {
    let cfg = fresh_dir("cfg");
    let file = cfg.join("plain.txt");
    std::fs::write(&file, "gamma\n").unwrap();
    let mut cmd = nxvim(&cfg);
    cmd.arg(&file).arg("--__ts-worker");
    let (status, stderr, elapsed) = run(cmd, Duration::from_secs(45));
    assert!(
        !status.success(),
        "an unknown --__ flag must be a loud usage error (stderr: {stderr})"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "the rejection must be immediate, not a hung headless worker ({elapsed:?})"
    );
    let _ = std::fs::remove_dir_all(&cfg);
}

/// `--extract-lua-runtime DIR` dumps the embedded `nx.*`/`vim.*` prelude to DIR as real
/// `.lua` files (so a Lua language server can index the API via `workspace.library`),
/// exits 0, and echoes the `.luarc.json` snippet. Each file must be the actual prelude
/// source — here we assert the `nx` namespace file landed and defines the global.
#[test]
fn extract_lua_runtime_writes_prelude_to_explicit_dir() {
    let cfg = fresh_dir("cfg");
    let out = fresh_dir("rt");
    let mut cmd = nxvim(&cfg);
    cmd.env_remove("NXVIM_RUNTIME");
    cmd.arg("--extract-lua-runtime").arg(&out);
    let (status, stdout, stderr, _) = run_io(cmd, Duration::from_secs(45));
    assert!(
        status.success(),
        "--extract-lua-runtime must exit 0: {status:?} (stderr: {stderr})"
    );
    let nx = out.join("prelude").join("nx.lua");
    assert!(
        nx.exists(),
        "the nx prelude file must be written, missing {}",
        nx.display()
    );
    let src = std::fs::read_to_string(&nx).unwrap();
    assert!(
        src.contains("nx = nx or {}"),
        "the extracted file must be the real prelude source that defines `nx`"
    );
    // The generated globals stub gives the Lua language server a definition site for the
    // Rust-injected `vim` global (else every `vim.*` alias reads as an undefined global).
    let stub = out.join("nxvim-globals.lua");
    assert!(
        stub.exists(),
        "the globals stub must be written, missing {}",
        stub.display()
    );
    assert!(
        std::fs::read_to_string(&stub)
            .unwrap()
            .contains("vim = vim"),
        "the globals stub must declare the Rust-injected `vim` global"
    );
    assert!(
        stdout.contains("workspace.library"),
        "the .luarc.json hint must be printed, got stdout: {stdout:?}"
    );
    let _ = std::fs::remove_dir_all(&cfg);
    let _ = std::fs::remove_dir_all(&out);
}

/// With no DIR argument, extraction targets `$NXVIM_RUNTIME` when it is set — the
/// directory a user's `.luarc.json` `["$NXVIM_RUNTIME"]` resolves to. Since the var is
/// already exported, the tool must NOT re-print the `export NXVIM_RUNTIME=…` hint.
#[test]
fn extract_lua_runtime_honors_nxvim_runtime_env() {
    let cfg = fresh_dir("cfg");
    let out = fresh_dir("rt_env");
    let mut cmd = nxvim(&cfg);
    cmd.env("NXVIM_RUNTIME", &out);
    cmd.arg("--extract-lua-runtime");
    let (status, stdout, stderr, _) = run_io(cmd, Duration::from_secs(45));
    assert!(
        status.success(),
        "--extract-lua-runtime must exit 0: {status:?} (stderr: {stderr})"
    );
    assert!(
        out.join("prelude").join("nx.lua").exists(),
        "extraction must target $NXVIM_RUNTIME when no DIR is given"
    );
    assert!(
        !stdout.contains("export NXVIM_RUNTIME"),
        "no export hint when the var is already set, got stdout: {stdout:?}"
    );
    let _ = std::fs::remove_dir_all(&cfg);
    let _ = std::fs::remove_dir_all(&out);
}

/// With neither a DIR argument nor `$NXVIM_RUNTIME`, extraction falls back to the
/// standard data dir (`stdpath("data")/runtime`, driven here by `XDG_DATA_HOME`) and
/// prints the `export NXVIM_RUNTIME=…` line the user must add to their shell.
#[test]
fn extract_lua_runtime_defaults_to_data_dir_and_prints_export() {
    let cfg = fresh_dir("cfg");
    let data = fresh_dir("xdg_data");
    let mut cmd = nxvim(&cfg);
    cmd.env_remove("NXVIM_RUNTIME");
    cmd.env("XDG_DATA_HOME", &data);
    cmd.arg("--extract-lua-runtime");
    let (status, stdout, stderr, _) = run_io(cmd, Duration::from_secs(45));
    assert!(
        status.success(),
        "--extract-lua-runtime must exit 0: {status:?} (stderr: {stderr})"
    );
    let nx = data
        .join("nxvim")
        .join("runtime")
        .join("prelude")
        .join("nx.lua");
    assert!(
        nx.exists(),
        "the default must be <data-dir>/nxvim/runtime, missing {}",
        nx.display()
    );
    assert!(
        stdout.contains("export NXVIM_RUNTIME"),
        "the export hint must be printed when the var is unset, got stdout: {stdout:?}"
    );
    let _ = std::fs::remove_dir_all(&cfg);
    let _ = std::fs::remove_dir_all(&data);
}
