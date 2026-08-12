//! `:connect` targets for the GUI — the *edit-host split* over SSH or WebTransport.
//!
//! `:connect [user@]host[:port][/file]` and `:connect bemtvi://…` switch a running
//! window to an **edit-host (daemon) session**: the editor (buffers, Lua, LSP) stays
//! local for a zero-round-trip keystroke path, while its fs/process/watch/LSP host
//! seams are pointed at a remote `bemtvi --daemon` (see
//! [`crate::session::spawn_session`]). This module parses the target into a
//! [`ConnectTarget`] and, for the SSH form, builds the `ssh … bemtvi --daemon` command
//! (routing interactive prompts to a native dialog via `SSH_ASKPASS`, so auth works
//! from a desktop launch with no terminal — see [`run_askpass_if_invoked`]).
//!
//! The daemon wire (msgpack-RPC) rides the ssh child's stdout/stdin; the editor RPC the
//! window drives is a separate in-process duplex. So unlike the classic "whole editor
//! runs remote" topology (removed), `:connect` keeps the keystroke path local.

use std::process::Stdio;

// The `Context` trait's `.context()` is called only in the macOS (`osascript`) and
// Windows (`powershell`) dialog paths; other `.context()` uses are the inherent method on
// `anyhow::Error`, which needs no import. Gating to those two targets keeps the import from
// reading as unused on Linux (where neither dialog path compiles).
#[cfg(any(target_os = "macos", windows))]
use anyhow::Context as _;
use anyhow::Result;
use tokio::process::Command;

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
        // the leading `/` from the split — and `ssh_daemon_command` shell-quotes it.)
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

    /// The target string ssh wants: `user@host`, or just `host` when no user.
    fn ssh_target(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }
}

/// Where a `:connect` switches the window to: an SSH-spawned `bemtvi --daemon` over
/// stdio, or a WebTransport/QUIC `--daemon --listen` listener. Either way the editor
/// stays local; only the fs/process/watch/LSP host seams cross the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectTarget {
    /// `[user@]host[:port][/file]` — `ssh … bemtvi --daemon` over stdio, with the GUI's
    /// `SSH_ASKPASS` pointed at this binary so prompts pop a native dialog.
    Ssh(RemoteSpec),
    /// `bemtvi://HOST:PORT/TOKEN?cert=HASH` — dial a `--daemon --listen` QUIC listener
    /// ([`bemtvi_server::connect_quic`]).
    Quic(String),
}

impl ConnectTarget {
    /// The file to open in the new session, if the target embeds one (`/file` after an
    /// SSH host). A QUIC URI carries no file, so its session opens `[No Name]`.
    pub fn embedded_file(&self) -> Option<String> {
        match self {
            ConnectTarget::Ssh(spec) => spec.file.clone(),
            ConnectTarget::Quic(_) => None,
        }
    }
}

/// Parse a `:connect [user@]host[:port][/file]` / `:connect bemtvi://…` command line —
/// the text *after* the `:` (so the leading word is `connect`) — into its
/// [`ConnectTarget`], or `None` for any other command line. The client intercepts this
/// on `<CR>` (the server knows nothing of `:connect`).
pub fn connect_command(cmdline: &str) -> Option<ConnectTarget> {
    let mut parts = cmdline.split_whitespace();
    match parts.next() {
        Some("connect") | Some("Connect") => {}
        _ => return None,
    }
    connect_target(parts.next()?)
}

/// Parse a single `:connect` argument — an `bemtvi://…` QUIC URI or a bare
/// `[user@]host[:port][/file]` ssh target — into a [`ConnectTarget`], or `None` if it is
/// neither. This is the arg half of [`connect_command`], reused for the fallback dial: when
/// `:connect <url>` finds no connect-provider, the server pushes the raw URL back as a
/// `btv_connect_fallback` notification and the GUI dials it through here (keeping the
/// `SSH_ASKPASS` path for ssh targets). A `scheme://` other than `bemtvi://` is rejected
/// rather than mis-read as a nonsense ssh host.
pub fn connect_target(arg: &str) -> Option<ConnectTarget> {
    if is_connect_uri(arg) {
        return Some(ConnectTarget::Quic(arg.to_string()));
    }
    if arg.contains("://") {
        return None;
    }
    RemoteSpec::parse_target(arg).map(ConnectTarget::Ssh)
}

/// Parse a `:workspace [dir]` command line — the text *after* the `:` (so the leading
/// word is `workspace`) — into the directory to open as a workspace, or `None` for any
/// other command line. A bare `:workspace` (no argument) targets the current directory
/// (`.`), matching the TUI's `bemtvi --workspace`. The whole remainder is the path —
/// trimmed, but otherwise taken verbatim so a directory name with spaces works. The
/// client intercepts this on `<CR>` and swaps the window onto a fresh **local** workspace
/// session (the server knows nothing of `:workspace`, exactly like `:connect`).
pub fn workspace_command(cmdline: &str) -> Option<String> {
    let trimmed = cmdline.trim_start();
    let rest = trimmed
        .strip_prefix("workspace")
        .or_else(|| trimmed.strip_prefix("Workspace"))?;
    // The command word must end here — `workspacefoo` is some other command, not
    // `:workspace` with an argument. An empty remainder (bare `:workspace`) is allowed.
    match rest.chars().next() {
        None => Some(".".to_string()),
        Some(c) if c.is_whitespace() => {
            let dir = rest.trim();
            Some(if dir.is_empty() { "." } else { dir }.to_string())
        }
        Some(_) => None,
    }
}

/// Whether `arg` is a WebTransport daemon URI (`bemtvi://…`, the scheme the daemon
/// prints) rather than an SSH `[user@]host` target.
pub fn is_connect_uri(arg: &str) -> bool {
    arg.starts_with("bemtvi://")
}

/// The remote command run over ssh — `$BEMTVI_REMOTE_CMD`, else `bemtvi --daemon` (the
/// single binary's *edit-host daemon* role; assumed on the remote `PATH`). Split on
/// whitespace into argv pieces, each shell-quoted by [`ssh_daemon_command`].
fn remote_cmd() -> String {
    std::env::var("BEMTVI_REMOTE_CMD").unwrap_or_else(|_| "bemtvi --daemon".to_string())
}

/// The local ssh program — `$BEMTVI_SSH`, else `ssh`.
fn ssh_program() -> String {
    std::env::var("BEMTVI_SSH").unwrap_or_else(|_| "ssh".to_string())
}

/// POSIX single-quote a string so the *remote* shell (ssh joins the command args and
/// runs them through it) treats it as one literal word — so a file path with spaces or
/// shell metacharacters (`;`, `|`, `$(…)`, …) can't inject a command.
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

/// Build `ssh [-p PORT] -- TARGET <remote command>` for an SSH [`RemoteSpec`], ready to
/// `.spawn()` (stdin/stdout piped, stderr inherited, `kill_on_drop`). The remote command
/// is `bemtvi --daemon [file]` ([`remote_cmd`]), each piece shell-quoted so the remote
/// shell can't be tricked by a crafted file path; `--` stops ssh parsing the destination
/// as an option (belt-and-suspenders with the `-`-leading rejection in
/// [`RemoteSpec::parse_target`]). The child's stdout/stdin *is* the daemon wire —
/// [`bemtvi_server::connect_daemon`] drives it for the five host seams.
///
/// Interactive prompts (host-key acceptance, password / key passphrase) are routed to a
/// GUI dialog via `SSH_ASKPASS` (see [`run_askpass_if_invoked`]), so auth works from a
/// desktop launch with no terminal. stderr is **inherited**, so ssh's diagnostics (a
/// "Permission denied", a "bemtvi: command not found" from the remote shell, the remote
/// daemon's own stderr) reach the user's terminal if there is one.
pub fn ssh_daemon_command(spec: &RemoteSpec) -> Command {
    let ssh = ssh_program();
    let mut cmd = Command::new(&ssh);
    if let Some(port) = spec.port {
        cmd.arg("-p").arg(port.to_string());
    }
    // Build the remote command as one shell-safe string: each `remote_cmd` token plus
    // the optional file, individually quoted, joined by spaces. ssh would otherwise
    // concatenate separate args with no quoting, breaking a path with spaces and
    // (worse) letting metacharacters reach the remote shell.
    let mut remote: Vec<String> = remote_cmd().split_whitespace().map(shell_quote).collect();
    if let Some(file) = &spec.file {
        remote.push(shell_quote(file));
    }
    cmd.arg("--").arg(spec.ssh_target()).arg(remote.join(" "));
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);

    // Route ssh's interactive prompts — host-key acceptance, password / key passphrase
    // — to a GUI dialog by re-invoking *this* binary as ssh's askpass helper (see
    // [`run_askpass_if_invoked`]). Without this they go to the controlling terminal, so
    // a desktop launch (no tty) couldn't authenticate or accept a new host key at all.
    // `SSH_ASKPASS_REQUIRE=force` (OpenSSH 8.4+) uses the helper even when a tty exists,
    // so the dialog is consistent either way. (Key-agent auth with a known host needs
    // no prompt, so nothing pops then.)
    if let Ok(exe) = std::env::current_exe() {
        cmd.env("SSH_ASKPASS", &exe)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env(ASKPASS_ENV, "1");
    }
    cmd
}

// ---------------------------------------------------------------------------
// SSH askpass helper
// ---------------------------------------------------------------------------
//
// `ssh_daemon_command` points `$SSH_ASKPASS` at this very binary (plus `ASKPASS_ENV`
// as the "you are the helper" marker, inherited by the program ssh execs). ssh runs the
// helper once per prompt with the prompt text as `argv[1]`, reads the answer from its
// stdout, and treats a non-zero exit as cancel. So `main` checks
// `run_askpass_if_invoked` first thing: in helper mode it pops a native dialog and
// exits, never starting the editor. This makes host-key acceptance and
// password/passphrase entry work from a desktop launch with no controlling terminal —
// the dialogs shell out to the platform's prompt tool (no winit), so the helper process
// needs no GPU/window.

/// Env var `ssh_daemon_command` sets on the spawned `ssh`, inherited by the askpass
/// program it execs, marking a re-invoked `bemtvi-gui` as the askpass helper.
const ASKPASS_ENV: &str = "BEMTVI_GUI_ASKPASS";

/// If this process was re-invoked by ssh as its `SSH_ASKPASS` helper, pop a dialog for
/// the prompt in `argv[1]`, print the answer to stdout, and return `Some(result)` so
/// `main` exits without starting the editor. `None` on a normal launch. A
/// cancelled/declined dialog returns `Err`, so `main` exits non-zero and ssh aborts the
/// connection rather than retrying with an empty answer.
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

/// Whether `prompt` is ssh's host-key *confirmation* (a yes/no question) rather than a
/// secret to type — keyed off the stable phrasing OpenSSH uses. Exposed for testing
/// (the dialogs themselves need a display).
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
         with hidden answer with title \"bemtvi - SSH\")\n\
         end run",
        prompt,
    )
}

#[cfg(target_os = "macos")]
fn confirm_dialog(prompt: &str) -> Result<String> {
    // Only "Connect" succeeds; "Cancel" raises osascript's user-cancelled error, which
    // `osascript` maps to an abort (so ssh gets a non-zero exit = decline).
    let button = osascript(
        "on run argv\n\
         return button returned of (display dialog (item 1 of argv) \
         buttons {\"Cancel\", \"Connect\"} default button \"Connect\" \
         with title \"bemtvi - SSH\" with icon caution)\n\
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
    // The prompt is passed as an argv item (not interpolated into the script), so an
    // attacker-controlled prompt can't inject AppleScript.
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
    if let Some(answer) = dialog_stdout("zenity", &["--password", "--title=bemtvi - SSH"])? {
        return Ok(answer);
    }
    if let Some(answer) = dialog_stdout("kdialog", &["--title=bemtvi - SSH", "--password", prompt])?
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
        &["--question", "--title=bemtvi - SSH", text.as_str()],
    )? {
        return Ok(if yes { "yes".into() } else { "no".into() });
    }
    if let Some(yes) = dialog_status("kdialog", &["--title=bemtvi - SSH", "--yesno", prompt])? {
        return Ok(if yes { "yes".into() } else { "no".into() });
    }
    anyhow::bail!("no GUI dialog helper found — install `zenity` or `kdialog`")
}

/// Run a dialog tool, returning its stdout on success, `Ok(None)` if the tool isn't
/// installed (try the next), or `Err` if the user cancelled.
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
         $f.Text='bemtvi - SSH';$f.Width=440;$f.Height=170;$f.TopMost=$true;\
         $l=New-Object System.Windows.Forms.Label;$l.Text=$env:BEMTVI_ASKPASS_PROMPT;\
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
         $r=[System.Windows.Forms.MessageBox]::Show($env:BEMTVI_ASKPASS_PROMPT,'bemtvi - SSH',\
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
        .env("BEMTVI_ASKPASS_PROMPT", prompt)
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
