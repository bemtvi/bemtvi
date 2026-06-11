//! The blocking shell-out seam `vim._system` runs through.
//!
//! `vim.system(...):wait()` (the synchronous form, no `on_exit`) shells out **on the
//! editor/Lua thread** and blocks the input tick to completion — an `lsp/<server>.lua`
//! `root_dir` that runs `cargo metadata` / `rustc --print sysroot` resolves this way
//! during `vim.lsp.enable`. By default the process is spawned locally
//! ([`StdBlockingSystem`]).
//!
//! In the edit-host split (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` →
//! Phase 3, Open Decision #5's *residual* blocking-bridge note) the shell-out must run
//! **where the project files are** — the remote daemon — because a `root_dir` detector
//! inspecting `Cargo.toml` is meaningless on the local machine. Unlike the async
//! `vim.system` (which rides the off-tick `HostProc` wire), the blocking form needs
//! its value *now*, so it can't go off-tick: the daemon impl is a *blocking bridge*
//! that parks the editor thread on the daemon's reply (with the link's RPC tasks on
//! their own thread, so the parked thread can't starve the reader carrying that reply).
//!
//! This module is the seam both impls satisfy. The default local spawn lives here; the
//! daemon impl (`RemoteBlockingSystem`) lives in `nxvim-server`'s daemon wire, where the
//! transport does.

use std::process::{Command, Stdio};

/// A blocking shell-out request: an argv list (no shell), an optional working
/// directory, and explicit environment additions. Mirrors what `vim.system` forwards
/// (its `text` flag is irrelevant here — `stdout`/`stderr` are returned as raw bytes
/// either way).
pub struct SystemSpec {
    /// The argv. The first element is the program, the rest its arguments; spawned
    /// directly (no shell), so it must be non-empty (an empty list degrades loudly).
    pub cmd: Vec<String>,
    /// The working directory to run in (`None` inherits the caller's).
    pub cwd: Option<String>,
    /// Environment variables to set on the child, layered over the inherited env.
    pub env: Vec<(String, String)>,
}

/// The result of a blocking shell-out — the shape `vim.system(...):wait()` returns:
/// the exit `code`, raw `stdout`/`stderr` bytes (so non-UTF-8 output survives), and
/// the child's `pid` when it actually spawned (`None` on a spawn failure).
pub struct SystemOutput {
    /// The process exit code, or `-1` for a spawn/transport failure (never a panic).
    pub code: i32,
    /// Raw standard output.
    pub stdout: Vec<u8>,
    /// Raw standard error (or the failure message, for a `code = -1` degrade).
    pub stderr: Vec<u8>,
    /// The OS pid, present once the child actually spawned.
    pub pid: Option<u32>,
}

impl SystemOutput {
    /// A degraded result for a failure that never produced an exit code — a missing
    /// tool, a dropped daemon link: `code = -1`, `msg` on stderr, no pid. Matches how
    /// the local `vim._system` reports a spawn failure (it never raises, so a config
    /// `root_dir` shell-out on a machine that lacks the toolchain degrades rather than
    /// breaking `vim.lsp.enable`).
    pub fn failed(msg: impl Into<String>) -> Self {
        SystemOutput {
            code: -1,
            stdout: Vec::new(),
            stderr: msg.into().into_bytes(),
            pid: None,
        }
    }
}

/// The seam `vim._system` runs its shell-out through. **Synchronous** — the caller
/// (LSP `root_dir` detection) needs the value inline on the editor tick. The default
/// ([`StdBlockingSystem`]) spawns a real local process; a daemon session injects a
/// blocking bridge that runs the process on the remote.
pub trait BlockingSystem {
    /// Run `spec` to completion and return its output. Must never panic or raise — a
    /// failure degrades to [`SystemOutput::failed`], because `vim.system` callers rely
    /// on a value, not an error.
    fn run(&self, spec: SystemSpec) -> SystemOutput;
}

/// The default [`BlockingSystem`]: spawn the process on the *local* machine and wait —
/// today's `vim._system` behavior verbatim, factored behind the seam. It serves both
/// as the editor-side default (no daemon) and as the daemon-side backend in the real
/// `nxvim --daemon`, where "local" *is* where the project files live.
pub struct StdBlockingSystem;

impl BlockingSystem for StdBlockingSystem {
    fn run(&self, spec: SystemSpec) -> SystemOutput {
        let Some((program, args)) = spec.cmd.split_first() else {
            return SystemOutput::failed("vim.system: cmd must be a non-empty list");
        };
        let mut command = Command::new(program);
        command.args(args).stdin(Stdio::null());
        if let Some(dir) = spec.cwd {
            command.current_dir(dir);
        }
        for (k, v) in spec.env {
            command.env(k, v);
        }
        // Spawn (capturing the real pid) then wait, rather than `output()`, so the
        // result's `pid` is a real pid — parity with the async path. The wait is short
        // by construction (a `root_dir` shell-out), so blocking here is acceptable.
        match command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => {
                let pid = Some(child.id());
                match child.wait_with_output() {
                    Ok(output) => SystemOutput {
                        code: output.status.code().unwrap_or(-1),
                        stdout: output.stdout,
                        stderr: output.stderr,
                        pid,
                    },
                    // A wait failure keeps the (real) pid but has no exit code.
                    Err(e) => SystemOutput {
                        code: -1,
                        stdout: Vec::new(),
                        stderr: format!("vim.system: wait failed for {program}: {e}").into_bytes(),
                        pid,
                    },
                }
            }
            Err(e) => SystemOutput::failed(format!("vim.system: failed to spawn {program}: {e}")),
        }
    }
}
