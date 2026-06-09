//! SSH remote transport for the GUI client.
//!
//! `nxvim-gui [user@]host[:port][/file]` runs the editor on a remote host: this
//! module parses that target ([`RemoteSpec`]), spawns `ssh … nxvim --server` to
//! launch a headless server there ([`connect`]), and exposes the ssh child's
//! stdio as an RPC transport ([`SshTransport`]) the GUI drives exactly like the
//! in-process duplex. The whole point of the client/server split: the editor
//! (buffers, Lua, LSP, treesitter) runs remote; only this thin client is local.
//!
//! See `docs/plans/2026-06-09-remote-ssh-client.md`.

use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};

use anyhow::{Context as _, Result};
use tokio::io::{AsyncRead, AsyncWrite, Join, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// A parsed `[user@]host[:port]` SSH target, plus an optional file to open on the
/// remote host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSpec {
    pub user: Option<String>,
    pub host: String,
    /// The **SSH** port (`ssh -p`), not a port a server is already listening on.
    pub port: Option<u16>,
    pub file: Option<String>,
}

impl RemoteSpec {
    /// Parse a *CLI* argument as an SSH target, or `None` if it isn't one.
    ///
    /// Disambiguation heuristic (the CLI's first positional could be a local
    /// file): a remote target has an explicit user (a literal `@`) and does not
    /// name an existing local path — so a real file named `git@notes` on disk
    /// still opens locally, while `david@host:5022` is recognized as remote. The
    /// `:connect` command, whose intent is unambiguous, parses with
    /// [`parse_target`](Self::parse_target) instead (no heuristic, user optional).
    pub fn parse(arg: &str) -> Option<RemoteSpec> {
        if !arg.contains('@') || Path::new(arg).exists() {
            return None;
        }
        Self::parse_target(arg)
    }

    /// Parse `[user@]host[:port][/file]` with no heuristics — the user is optional
    /// (ssh defaults it), a trailing `:digits` is the SSH port, and the first `/`
    /// after the host begins the remote file path (its leading `/` is kept, so an
    /// absolute path stays absolute). A colon followed by non-digits is treated as
    /// part of the host (we don't parse bracketed IPv6 literals). `None` if there's
    /// no host.
    pub fn parse_target(s: &str) -> Option<RemoteSpec> {
        let (user, rest) = match s.split_once('@') {
            Some((u, r)) if !u.is_empty() && !r.is_empty() => (Some(u.to_string()), r),
            // A stray `@` with an empty user or host is malformed.
            Some(_) => return None,
            None => (None, s),
        };
        let (hostport, file) = match rest.find('/') {
            Some(i) => (&rest[..i], Some(rest[i..].to_string())),
            None => (rest, None),
        };
        if hostport.is_empty() {
            return None;
        }
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p))
                if !h.is_empty() && !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) =>
            {
                match p.parse::<u16>() {
                    Ok(port) => (h.to_string(), Some(port)),
                    // A numeric run that overflows u16 isn't a usable port; keep the
                    // whole thing as the host rather than silently dropping it.
                    Err(_) => (hostport.to_string(), None),
                }
            }
            _ => (hostport.to_string(), None),
        };
        // Reject a `user`/`host` beginning with `-`: it would be smuggled to `ssh`
        // as an option (e.g. `-oProxyCommand=…`, a remote-code-execution vector)
        // rather than a destination. (`file` can't begin with `-` here — it keeps
        // the leading `/` from the split — and `connect` shell-quotes it anyway.)
        if host.starts_with('-') || user.as_deref().is_some_and(|u| u.starts_with('-')) {
            return None;
        }
        Some(RemoteSpec {
            user,
            host,
            port,
            file,
        })
    }

    /// Override the remote file with the CLI's second positional, if given. A
    /// `None` keeps any file already embedded in the target (`host:port/file`).
    pub fn with_file(mut self, file: Option<String>) -> Self {
        if file.is_some() {
            self.file = file;
        }
        self
    }

    /// The target string ssh wants: `user@host`, or just `host` when no user.
    fn ssh_target(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }
}

/// Parse a `:connect [user@]host[:port][/file]` command line — the text *after*
/// the `:` (so the leading word is `connect`) — into its SSH target, or `None`
/// for any other command line. The client intercepts this on `<CR>` (the current
/// server knows nothing of `:connect`).
pub fn connect_command(cmdline: &str) -> Option<RemoteSpec> {
    let mut parts = cmdline.split_whitespace();
    match parts.next() {
        Some("connect") | Some("Connect") => {}
        _ => return None,
    }
    RemoteSpec::parse_target(parts.next()?)
}

/// The remote command run over ssh — `$NXVIM_REMOTE_CMD`, else `nxvim --server`
/// (the single binary's headless role; assumed on the remote `PATH`). Split on
/// whitespace into argv pieces, each shell-quoted by [`connect`].
fn remote_cmd() -> String {
    std::env::var("NXVIM_REMOTE_CMD").unwrap_or_else(|_| "nxvim --server".to_string())
}

/// The local ssh program — `$NXVIM_SSH`, else `ssh`.
fn ssh_program() -> String {
    std::env::var("NXVIM_SSH").unwrap_or_else(|_| "ssh".to_string())
}

/// POSIX single-quote a string so the *remote* shell (ssh joins the command args
/// and runs them through it) treats it as one literal word — so a file path with
/// spaces or shell metacharacters (`;`, `|`, `$(…)`, …) can't inject a command.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    // A literal `'` ends the quote, escapes one apostrophe, and reopens: `'\''`.
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Spawn `ssh [-p PORT] -- TARGET <remote command>` and connect its stdio as the
/// RPC transport. The remote command is `nxvim --server [file]` ([`remote_cmd`]),
/// each piece shell-quoted so the remote shell can't be tricked by a crafted file
/// path; `--` stops ssh parsing the destination as an option (belt-and-suspenders
/// with the `-`-leading rejection in [`RemoteSpec::parse_target`]).
///
/// stderr is **inherited**, so ssh's own diagnostics (auth failures, host-key
/// prompts, a "nxvim: command not found" from the remote shell) reach the user's
/// terminal — without them a failed connection would just look like an instant
/// disconnect. The returned [`SshTransport`] owns the child with `kill_on_drop`,
/// so closing the window tears the remote process down.
///
/// Must be called from within a tokio runtime — the child's pipes are bound to
/// the runtime that polls them (the GUI's IO thread).
pub async fn connect(spec: &RemoteSpec) -> Result<SshTransport> {
    let ssh = ssh_program();
    let mut cmd = Command::new(&ssh);
    if let Some(port) = spec.port {
        cmd.arg("-p").arg(port.to_string());
    }
    // Build the remote command as one shell-safe string: each `remote_cmd` token
    // plus the optional file, individually quoted, joined by spaces. ssh would
    // otherwise concatenate separate args with no quoting, breaking a path with
    // spaces and (worse) letting metacharacters reach the remote shell.
    let mut remote: Vec<String> = remote_cmd().split_whitespace().map(shell_quote).collect();
    if let Some(file) = &spec.file {
        remote.push(shell_quote(file));
    }
    cmd.arg("--").arg(spec.ssh_target()).arg(remote.join(" "));
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {ssh} (is it installed and on PATH?)"))?;
    let stdout = child.stdout.take().expect("ssh stdout was piped");
    let stdin = child.stdin.take().expect("ssh stdin was piped");
    Ok(SshTransport {
        inner: tokio::io::join(stdout, stdin),
        _child: child,
    })
}

/// RPC transport over an ssh child's stdio. Owns the [`Child`] so the connection
/// and the remote process share a lifetime; all fields are `Unpin`, so the poll
/// impls forward straight to the joined `stdout`(read)+`stdin`(write) stream.
pub struct SshTransport {
    inner: Join<ChildStdout, ChildStdin>,
    _child: Child,
}

impl AsyncRead for SshTransport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for SshTransport {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
