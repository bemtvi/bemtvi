//! The transport seam: how the manager obtains a language server's bidirectional
//! byte streams.
//!
//! A language server is a long-lived child speaking JSON-RPC over its stdio — a
//! *bidirectional raw pipe*, unlike the run-to-completion `vim.system` /
//! `jobstart` path (which the [`HostProc`] seam owns server-side). The two are
//! genuinely different contracts: a one-shot spawn reports pid + a single exit with
//! captured output; a server's pipes stay open for its whole life, with bytes
//! flowing both ways the entire time and stdout consumed incrementally, not
//! buffered to an exit. So this seam is its own trait rather than folded into
//! `HostProc`.
//!
//! [`LocalLspTransport`] is the default: it spawns the server as a real local child
//! (today's behavior, verbatim). The edit-host split injects a daemon-backed
//! transport (`nxvim-server`'s `RemoteLspTransport`) that tunnels the server's
//! stdio over the wire to a daemon that holds the actual child — so a language
//! server runs *where the project files are* while editing stays local
//! (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` → Phase 3, the long-lived
//! bidirectional-pipe shape). The manager drives the same `async-lsp` loop over
//! whichever streams the transport hands back; it never knows which it is talking
//! to.
//!
//! [`HostProc`]: nxvim_server (the server-side process seam)

use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;

use std::process::Stdio;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{Child, Command};

use crate::client::exit_code_signal;
use crate::protocol::ServerSpawn;

/// One spawned language server's I/O: the byte streams the JSON-RPC loop drives and
/// a handle to terminate it and reap its exit status. `stdout` is the server→client
/// direction (the server's standard output), `stdin` the client→server direction;
/// `stderr` (when present) is drained to the LSP log. The streams are boxed trait
/// objects so a local child's pipes and a daemon-tunneled pipe satisfy one type.
pub struct LspChannel {
    /// server → client: the JSON-RPC the manager reads (the server's stdout).
    pub stdout: Pin<Box<dyn AsyncRead + Send>>,
    /// client → server: the JSON-RPC the manager writes (the server's stdin).
    pub stdin: Pin<Box<dyn AsyncWrite + Send>>,
    /// The server's stderr, drained to the log a line at a time. `None` when the
    /// transport captures it elsewhere.
    pub stderr: Option<Pin<Box<dyn AsyncRead + Send>>>,
    /// Terminate the server and await its exit status.
    pub process: Box<dyn LspProcess>,
}

/// The future a [`LspProcess::wait`] resolves to: the server's `(exit code,
/// terminating signal)`. Aliased so both the trait and its implementations (here and
/// `nxvim-server`'s remote one) name one type rather than repeating the boxed shape.
pub type ExitFuture = Pin<Box<dyn Future<Output = (Option<i32>, Option<i32>)> + Send>>;

/// A spawned server's lifetime handle: kill it and reap its `(code, signal)`. The
/// counterpart to a [`tokio::process::Child`] for the local case, abstracted so the
/// remote case (a daemon-held child whose exit arrives over the wire) satisfies the
/// same contract.
pub trait LspProcess: Send {
    /// Begin terminating the server (idempotent; harmless if it already exited).
    fn start_kill(&mut self);
    /// Await the server's exit and report `(exit code, terminating signal)` — both
    /// `None` if the status couldn't be collected (a kill, a dropped daemon link).
    /// Consumes the handle, so it is awaited exactly once.
    fn wait(self: Box<Self>) -> ExitFuture;
}

/// How the manager spawns a language server. The default [`LocalLspTransport`]
/// launches a real local child; the edit-host split swaps in a daemon-backed
/// transport so the server runs on the remote. `Send + Sync` so one transport is
/// shared across every server the manager supervises.
pub trait LspTransport: Send + Sync {
    /// Launch the server described by `spec`, with `root` as its working directory,
    /// and hand back its byte streams + lifetime handle. An `Err` here is a spawn
    /// failure the supervisor reports loudly (and the breaker counts).
    fn spawn(
        &self,
        spec: &ServerSpawn,
        root: &Path,
    ) -> Pin<Box<dyn Future<Output = io::Result<LspChannel>> + Send>>;
}

/// The default [`LspTransport`]: spawn the server as a real local child via
/// `tokio::process`, exactly as the manager did inline before the seam existed.
/// Every front end uses this today; the daemon-backed transport arrives with the
/// edit-host split.
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalLspTransport;

impl LspTransport for LocalLspTransport {
    fn spawn(
        &self,
        spec: &ServerSpawn,
        root: &Path,
    ) -> Pin<Box<dyn Future<Output = io::Result<LspChannel>> + Send>> {
        let program = spec.program.clone();
        let args = spec.args.clone();
        let env = spec.env.clone();
        let root = root.to_path_buf();
        Box::pin(async move {
            let mut child = Command::new(&program)
                .args(&args)
                // Layered on top of the inherited environment (`envs`, not
                // `env_clear` + `envs`): a language server needs `$PATH` to find its
                // own toolchain, so `cmd_env` adds to the environment, never replaces it.
                .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .current_dir(&root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                // Captured (not null'd) so the server's stderr — panics, RA_LOG
                // output — reaches the log; the manager drains it so the pipe never
                // blocks.
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            let (Some(stdout), Some(stdin)) = (child.stdout.take(), child.stdin.take()) else {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(io::Error::other("server stdio pipes unexpectedly missing"));
            };
            let stderr = child.stderr.take();
            Ok(LspChannel {
                stdout: Box::pin(stdout),
                stdin: Box::pin(stdin),
                stderr: stderr.map(|e| Box::pin(e) as Pin<Box<dyn AsyncRead + Send>>),
                process: Box::new(LocalLspProcess { child }),
            })
        })
    }
}

/// The local [`LspProcess`]: a [`tokio::process::Child`] with its stdio already
/// taken. `kill_on_drop` means even an un-`wait`ed drop terminates the child.
struct LocalLspProcess {
    child: Child,
}

impl LspProcess for LocalLspProcess {
    fn start_kill(&mut self) {
        let _ = self.child.start_kill();
    }

    fn wait(mut self: Box<Self>) -> ExitFuture {
        Box::pin(async move { exit_code_signal(self.child.wait().await.ok()) })
    }
}
