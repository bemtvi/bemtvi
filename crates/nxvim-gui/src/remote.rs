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
/// Interactive prompts (host-key acceptance, password / key passphrase) are
/// routed to a GUI dialog via `SSH_ASKPASS` (see [`run_askpass_if_invoked`]), so
/// auth works from a desktop launch with no terminal. stderr is **inherited**, so
/// ssh's non-interactive diagnostics (a "Permission denied", a "nxvim: command not
/// found" from the remote shell) still reach the user's terminal if there is one.
/// The returned [`SshTransport`] owns the child with `kill_on_drop`, so closing
/// the window tears the remote process down.
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

    // Route ssh's interactive prompts — host-key acceptance, password / key
    // passphrase — to a GUI dialog by re-invoking *this* binary as ssh's askpass
    // helper (see [`run_askpass_if_invoked`]). Without this they go to the
    // controlling terminal, so a desktop launch (no tty) couldn't authenticate or
    // accept a new host key at all. `SSH_ASKPASS_REQUIRE=force` (OpenSSH 8.4+) uses
    // the helper even when a tty exists, so the dialog is consistent either way.
    // (Key-agent auth with a known host needs no prompt, so nothing pops then.)
    if let Ok(exe) = std::env::current_exe() {
        cmd.env("SSH_ASKPASS", &exe)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env(ASKPASS_ENV, "1");
    }

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

// ---------------------------------------------------------------------------
// SSH askpass helper
// ---------------------------------------------------------------------------
//
// `connect` points `$SSH_ASKPASS` at this very binary (plus `ASKPASS_ENV` as the
// "you are the helper" marker, inherited by the program ssh execs). ssh runs the
// helper once per prompt with the prompt text as `argv[1]`, reads the answer from
// its stdout, and treats a non-zero exit as cancel. So `main` checks
// `run_askpass_if_invoked` first thing: in helper mode it pops a native dialog and
// exits, never starting the editor. This makes host-key acceptance and
// password/passphrase entry work from a desktop launch with no controlling
// terminal — the dialogs shell out to the platform's prompt tool (no winit), so
// the helper process needs no GPU/window.

/// Env var `connect` sets on the spawned `ssh`, inherited by the askpass program
/// it execs, marking a re-invoked `nxvim-gui` as the askpass helper.
const ASKPASS_ENV: &str = "NXVIM_GUI_ASKPASS";

/// If this process was re-invoked by ssh as its `SSH_ASKPASS` helper, pop a dialog
/// for the prompt in `argv[1]`, print the answer to stdout, and return
/// `Some(result)` so `main` exits without starting the editor. `None` on a normal
/// launch. A cancelled/declined dialog returns `Err`, so `main` exits non-zero and
/// ssh aborts the connection rather than retrying with an empty answer.
pub fn run_askpass_if_invoked() -> Option<Result<()>> {
    std::env::var_os(ASKPASS_ENV)?;
    let prompt = std::env::args().nth(1).unwrap_or_default();
    Some(answer_askpass(&prompt))
}

/// Show the right dialog for `prompt` and write the answer (one line) to stdout.
fn answer_askpass(prompt: &str) -> Result<()> {
    let answer = if is_confirmation(prompt) {
        confirm_dialog(prompt)?
    } else {
        secret_dialog(prompt)?
    };
    println!("{answer}");
    Ok(())
}

/// Whether `prompt` is ssh's host-key *confirmation* (a yes/no question) rather
/// than a secret to type — keyed off the stable phrasing OpenSSH uses. Exposed for
/// testing (the dialogs themselves need a display).
pub fn is_confirmation(prompt: &str) -> bool {
    let p = prompt.to_ascii_lowercase();
    p.contains("continue connecting")
        || p.contains("authenticity of host")
        || p.contains("(yes/no")
        || p.contains("yes/no/")
}

// --- macOS: osascript -------------------------------------------------------

#[cfg(target_os = "macos")]
fn secret_dialog(prompt: &str) -> Result<String> {
    osascript(
        "on run argv\n\
         return text returned of (display dialog (item 1 of argv) default answer \"\" \
         with hidden answer with title \"nxvim - SSH\")\n\
         end run",
        prompt,
    )
}

#[cfg(target_os = "macos")]
fn confirm_dialog(prompt: &str) -> Result<String> {
    // Only "Connect" succeeds; "Cancel" raises osascript's user-cancelled error,
    // which `osascript` maps to an abort (so ssh gets a non-zero exit = decline).
    let button = osascript(
        "on run argv\n\
         return button returned of (display dialog (item 1 of argv) \
         buttons {\"Cancel\", \"Connect\"} default button \"Connect\" \
         with title \"nxvim - SSH\" with icon caution)\n\
         end run",
        prompt,
    )?;
    Ok(if button.eq_ignore_ascii_case("connect") {
        "yes".into()
    } else {
        "no".into()
    })
}

#[cfg(target_os = "macos")]
fn osascript(body: &str, prompt: &str) -> Result<String> {
    // The prompt is passed as an argv item (not interpolated into the script), so
    // an attacker-controlled prompt can't inject AppleScript.
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(body)
        .arg(prompt)
        .output()
        .context("run osascript for the SSH dialog")?;
    if !output.status.success() {
        anyhow::bail!("SSH dialog cancelled");
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

// --- Linux/BSD: zenity or kdialog -------------------------------------------

#[cfg(all(unix, not(target_os = "macos")))]
fn secret_dialog(prompt: &str) -> Result<String> {
    if let Some(answer) = dialog_stdout("zenity", &["--password", "--title=nxvim - SSH"])? {
        return Ok(answer);
    }
    if let Some(answer) = dialog_stdout("kdialog", &["--title=nxvim - SSH", "--password", prompt])?
    {
        return Ok(answer);
    }
    anyhow::bail!(
        "no GUI password helper found — install `zenity` or `kdialog`, or use \
         key-based auth with a loaded ssh-agent"
    )
}

#[cfg(all(unix, not(target_os = "macos")))]
fn confirm_dialog(prompt: &str) -> Result<String> {
    let text = format!("--text={prompt}");
    if let Some(yes) = dialog_status(
        "zenity",
        &["--question", "--title=nxvim - SSH", text.as_str()],
    )? {
        return Ok(if yes { "yes".into() } else { "no".into() });
    }
    if let Some(yes) = dialog_status("kdialog", &["--title=nxvim - SSH", "--yesno", prompt])? {
        return Ok(if yes { "yes".into() } else { "no".into() });
    }
    anyhow::bail!("no GUI dialog helper found — install `zenity` or `kdialog`")
}

/// Run a dialog tool, returning its stdout on success, `Ok(None)` if the tool
/// isn't installed (try the next), or `Err` if the user cancelled.
#[cfg(all(unix, not(target_os = "macos")))]
fn dialog_stdout(prog: &str, args: &[&str]) -> Result<Option<String>> {
    match std::process::Command::new(prog).args(args).output() {
        Ok(o) if o.status.success() => Ok(Some(
            String::from_utf8_lossy(&o.stdout).trim_end().to_string(),
        )),
        Ok(_) => anyhow::bail!("SSH dialog cancelled"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context(format!("run {prog}"))),
    }
}

/// Like [`dialog_stdout`] but the answer is the exit status (yes/no question):
/// `Some(true)` accepted, `Some(false)` declined, `None` tool not installed.
#[cfg(all(unix, not(target_os = "macos")))]
fn dialog_status(prog: &str, args: &[&str]) -> Result<Option<bool>> {
    match std::process::Command::new(prog).args(args).status() {
        Ok(s) if s.success() => Ok(Some(true)),
        Ok(_) => Ok(Some(false)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context(format!("run {prog}"))),
    }
}

// --- Windows: PowerShell + WinForms -----------------------------------------

#[cfg(windows)]
fn secret_dialog(prompt: &str) -> Result<String> {
    // A masked WinForms text box; the prompt arrives via env (no script injection).
    powershell(
        "Add-Type -AssemblyName System.Windows.Forms,System.Drawing;\
         $f=New-Object System.Windows.Forms.Form;\
         $f.Text='nxvim - SSH';$f.Width=440;$f.Height=170;$f.TopMost=$true;\
         $l=New-Object System.Windows.Forms.Label;$l.Text=$env:NXVIM_ASKPASS_PROMPT;\
         $l.Left=10;$l.Top=10;$l.Width=410;$l.Height=50;\
         $t=New-Object System.Windows.Forms.TextBox;$t.UseSystemPasswordChar=$true;\
         $t.Left=10;$t.Top=70;$t.Width=410;\
         $b=New-Object System.Windows.Forms.Button;$b.Text='OK';$b.Left=345;$b.Top=100;\
         $b.DialogResult=[System.Windows.Forms.DialogResult]::OK;\
         $f.Controls.AddRange(@($l,$t,$b));$f.AcceptButton=$b;\
         if($f.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK){[Console]::Out.Write($t.Text)}else{exit 1}",
        prompt,
    )
}

#[cfg(windows)]
fn confirm_dialog(prompt: &str) -> Result<String> {
    powershell(
        "Add-Type -AssemblyName System.Windows.Forms;\
         $r=[System.Windows.Forms.MessageBox]::Show($env:NXVIM_ASKPASS_PROMPT,'nxvim - SSH',\
         [System.Windows.Forms.MessageBoxButtons]::YesNo,[System.Windows.Forms.MessageBoxIcon]::Warning);\
         if($r -eq [System.Windows.Forms.DialogResult]::Yes){[Console]::Out.Write('yes')}else{[Console]::Out.Write('no')}",
        prompt,
    )
}

#[cfg(windows)]
fn powershell(script: &str, prompt: &str) -> Result<String> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .env("NXVIM_ASKPASS_PROMPT", prompt)
        .output()
        .context("run powershell for the SSH dialog")?;
    if !output.status.success() {
        anyhow::bail!("SSH dialog cancelled");
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

// --- Other targets: no GUI prompt -------------------------------------------

#[cfg(not(any(unix, windows)))]
fn secret_dialog(_prompt: &str) -> Result<String> {
    anyhow::bail!("no GUI askpass on this platform; use key-based auth with an ssh-agent")
}

#[cfg(not(any(unix, windows)))]
fn confirm_dialog(_prompt: &str) -> Result<String> {
    anyhow::bail!("no GUI askpass on this platform")
}
