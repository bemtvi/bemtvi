//! The parsed form of a `btv.session.reconnect(spec)` request (§B of the remote-connectors
//! plan). The Lua seam (`btv.session.reconnect`) validates + normalizes the spec and the
//! server pushes it to the client verbatim as the single param of a `btv_session_reconnect`
//! notification; both native front ends (TUI + GUI) parse it here into a [`ReconnectSpec`]
//! and perform the client-persistent session swap. Kept in the server crate (alongside
//! [`ConfigSource`](crate::ConfigSource) and the reconnecting-connect API) so the two front
//! ends share one parser rather than each re-deriving the wire shape.

use anyhow::{anyhow, bail, Result};
use rmpv::Value;
use tokio::process::Command;

use crate::ConfigSource;

/// How a `spawn` transport's daemon child is launched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpawnCommand {
    /// A structured argv, run **without a shell** — `Command::new(argv[0]).args(argv[1..])`.
    /// The preferred form: wire input can't smuggle shell metacharacters. Guaranteed
    /// non-empty by the parser.
    Argv(Vec<String>),
    /// A shell command line, run via `sh -c` — mirrors `BEMTVI_DAEMON_CMD`, so a one-line
    /// `"ssh host bemtvi --daemon"` / `"docker exec … bemtvi --daemon"` works verbatim. Only
    /// as safe as its origin (local Lua, which already has arbitrary execution).
    Shell(String),
}

impl SpawnCommand {
    /// The base [`Command`] (program + args) — the caller wires stdio / `kill_on_drop`.
    /// `Argv` runs the program directly (no shell); `Shell` runs the line through `sh -c`.
    pub fn to_command(&self) -> Command {
        match self {
            SpawnCommand::Argv(argv) => {
                let mut c = Command::new(&argv[0]);
                c.args(&argv[1..]);
                c
            }
            SpawnCommand::Shell(line) => {
                let mut c = Command::new("sh");
                c.arg("-c").arg(line);
                c
            }
        }
    }
}

/// Where a swapped session's backend comes from — the two transport shapes that already
/// exist (`connect_daemon_reconnecting` over a child's stdio, and the QUIC dialer).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconnectTransport {
    /// Spawn a daemon child (see [`SpawnCommand`]) and run the daemon over its stdin/stdout.
    /// The client feeds this into the reconnecting dialer so a dropped link re-runs it.
    Spawn { command: SpawnCommand },
    /// Dial a `--daemon --listen` QUIC endpoint at `addr`
    /// (`bemtvi://host:port/token?cert=hash`).
    Quic { addr: String },
}

/// A client-directed session swap, parsed from the `btv_session_reconnect` notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconnectSpec {
    /// The backend transport to bring up.
    pub transport: ReconnectTransport,
    /// Whether the swapped session runs the daemon's config or this machine's (§D).
    pub config_source: ConfigSource,
    /// Whether to carry the current session's buffers across the swap. Reserved: the
    /// current swap always brings up a fresh session (buffers come from the new backend);
    /// a client that cannot honor `true` says so loudly rather than dropping buffers
    /// silently.
    pub keep_buffers: bool,
}

impl ReconnectSpec {
    /// Fail loud on `keep_buffers = true`: the swap always brings up a fresh session
    /// (buffers come from the new backend), and carrying local buffers across is not
    /// implemented — say so rather than dropping them silently. Every client's swap
    /// builder calls this first.
    pub fn reject_keep_buffers(&self) -> Result<()> {
        if self.keep_buffers {
            bail!("btv.session.reconnect: keep_buffers = true is not supported yet");
        }
        Ok(())
    }

    /// Parse the normalized wire spec (the `btv.session.reconnect` Lua surface guarantees the
    /// shape, but a bad payload still fails loud here rather than swapping onto nothing).
    pub fn from_value(value: &Value) -> Result<Self> {
        let transport = get(value, "transport")
            .ok_or_else(|| anyhow!("btv_session_reconnect: missing `transport`"))?;
        let kind = get_str(transport, "kind")
            .ok_or_else(|| anyhow!("btv_session_reconnect: missing `transport.kind`"))?;
        let transport = match kind {
            "spawn" => {
                // Prefer the structured argv (no shell); fall back to the shell command line.
                let command = if let Some(argv) = get(transport, "argv") {
                    let argv = str_list(argv).ok_or_else(|| {
                        anyhow!("btv_session_reconnect: spawn `argv` must be a list of strings")
                    })?;
                    if argv.is_empty() {
                        bail!("btv_session_reconnect: spawn `argv` must be non-empty");
                    }
                    SpawnCommand::Argv(argv)
                } else if let Some(cmd) = get_str(transport, "cmd").filter(|s| !s.is_empty()) {
                    SpawnCommand::Shell(cmd.to_string())
                } else {
                    bail!("btv_session_reconnect: spawn transport needs `argv` or `cmd`");
                };
                ReconnectTransport::Spawn { command }
            }
            "quic" => {
                let addr = get_str(transport, "addr")
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("btv_session_reconnect: quic transport needs `addr`"))?;
                ReconnectTransport::Quic {
                    addr: addr.to_string(),
                }
            }
            other => bail!("btv_session_reconnect: unknown transport.kind {other:?}"),
        };
        let config_source = match get_str(value, "config_source") {
            Some("remote") | None => ConfigSource::Remote,
            Some("local") => ConfigSource::Local,
            // Reserved (§D) but not built yet — the Lua seam already rejects it; guard here
            // too for a spec that reaches the wire another way, with the same clear message.
            Some("merged") => bail!(
                "btv_session_reconnect: config_source \"merged\" is not implemented yet — use \"remote\" or \"local\""
            ),
            Some(other) => bail!("btv_session_reconnect: unknown config_source {other:?}"),
        };
        let keep_buffers = matches!(get(value, "keep_buffers"), Some(Value::Boolean(true)));
        Ok(ReconnectSpec {
            transport,
            config_source,
            keep_buffers,
        })
    }

    /// The **fallback** swap for a `:connect <url>` that no connect-provider owns (§C): the
    /// client's built-in direct dial, expressed as a spec so it rides the same swap path.
    /// `bemtvi://…` → a QUIC dial; a bare `[user@]host[:port]` → `ssh <target> bemtvi --daemon`
    /// over stdio (structured argv, no shell). A trailing `/file` and any other `scheme://`
    /// are rejected LOUD (the TUI fallback opens no remote file, and an unknown scheme with
    /// no provider is a mistype, not a silent no-op). The daemon's config is used
    /// (`config_source = remote`), matching a plain `--connect-daemon`. Used by the native
    /// TUI; the GUI runs its own fallback (an `SSH_ASKPASS`-wired ssh child) instead.
    pub fn fallback_from_url(url: &str) -> Result<Self> {
        let transport = if url.starts_with("bemtvi://") {
            ReconnectTransport::Quic {
                addr: url.to_string(),
            }
        } else if url.contains("://") {
            // A `scheme://` we don't dial directly and no provider claimed — say so loudly.
            bail!(
                "connect: no provider for {url:?} (only bemtvi:// and [user@]host are dialed directly)"
            );
        } else {
            ReconnectTransport::Spawn {
                command: SpawnCommand::Argv(ssh_daemon_argv(url)?),
            }
        };
        Ok(ReconnectSpec {
            transport,
            config_source: ConfigSource::Remote,
            keep_buffers: false,
        })
    }
}

/// Build the argv for `ssh [-p PORT] -- <target> bemtvi --daemon` from a bare
/// `[user@]host[:port]` connect target (the TUI's fallback dial). Rejects a `/file` suffix
/// (opening a remote file over this fallback isn't wired) and a `host`/`user` beginning with
/// `-` (it would be smuggled to ssh as an option — an RCE vector), matching the GUI's
/// `RemoteSpec::parse_target` hardening. `--` stops ssh reading the target as an option.
fn ssh_daemon_argv(target: &str) -> Result<Vec<String>> {
    if let Some((_, _)) = target.split_once('/') {
        bail!("connect: a remote file path ({target:?}) is not supported by the ssh fallback — connect to the host, then open the file");
    }
    let (user, hostport) = match target.split_once('@') {
        Some((u, r)) if !u.is_empty() && !r.is_empty() => (Some(u), r),
        Some(_) => bail!("connect: malformed ssh target {target:?} (empty user or host)"),
        None => (None, target),
    };
    // A trailing `:<digits>` is the ssh port; a colon followed by non-digits stays part of
    // the host (we don't parse bracketed IPv6 literals here).
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
            (h, Some(p))
        }
        _ => (hostport, None),
    };
    if host.is_empty() {
        bail!("connect: ssh target {target:?} has no host");
    }
    if host.starts_with('-') || user.is_some_and(|u| u.starts_with('-')) {
        bail!("connect: ssh target {target:?} must not begin with '-' (it would be read as an ssh option)");
    }
    let dest = match user {
        Some(u) => format!("{u}@{host}"),
        None => host.to_string(),
    };
    let mut argv = vec!["ssh".to_string()];
    if let Some(port) = port {
        argv.push("-p".to_string());
        argv.push(port.to_string());
    }
    argv.push("--".to_string());
    argv.push(dest);
    argv.push("bemtvi".to_string());
    argv.push("--daemon".to_string());
    Ok(argv)
}

/// Look up `key` in an rmpv map value.
fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, val)| val),
        _ => None,
    }
}

/// Look up `key` in an rmpv map and read it as a string.
fn get_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    get(v, key).and_then(Value::as_str)
}

/// Read an rmpv array of strings into a `Vec<String>` (`None` if not an array of strings).
fn str_list(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::Array(items) => items
            .iter()
            .map(|i| i.as_str().map(str::to_string))
            .collect(),
        _ => None,
    }
}
