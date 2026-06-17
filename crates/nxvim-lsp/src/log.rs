//! A small append-only LSP log, the analogue of neovim's `~/.local/state/nvim/lsp.log`.
//!
//! Lines are `[LEVEL][YYYY-MM-DD HH:MM:SS] server\tmessage` (timestamps in UTC),
//! with a `[START]` line written when a session first opens the file. The level
//! threshold is read once from `$NXVIM_LSP_LOG_LEVEL` (`off`/`error`/`warn`/
//! `info`/`debug`/`trace`, default `warn`); a message is written when its
//! severity is at or above the threshold. `$NXVIM_LSP_LOG_FILE` overrides the
//! path (used by tests so they never touch the real state dir).
//!
//! The log is created lazily — only when the manager's supervisor first starts
//! (i.e. a configured filetype is actually opened) — so a session that never
//! touches LSP writes nothing.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Severity ordering: a message at `level` is logged when `level >= threshold`.
/// `Off` is the maximum, so a threshold of `Off` silences everything.
///
/// The wasm sync client only ever logs at `Warn` (and constructs `Off` via
/// [`LspLog::disabled`]); the finer levels are produced by the native manager's
/// `window/*Message` routing, so they read as dead there.
#[cfg_attr(not(feature = "native"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Off,
}

impl LogLevel {
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    fn parse(s: &str) -> Option<LogLevel> {
        match s.trim().to_ascii_lowercase().as_str() {
            "trace" => Some(LogLevel::Trace),
            "debug" => Some(LogLevel::Debug),
            "info" => Some(LogLevel::Info),
            "warn" | "warning" => Some(LogLevel::Warn),
            "error" => Some(LogLevel::Error),
            "off" | "none" => Some(LogLevel::Off),
            _ => None,
        }
    }

    fn tag(self) -> &'static str {
        match self {
            LogLevel::Trace => "TRACE",
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
            LogLevel::Off => "OFF",
        }
    }
}

/// The shared log sink. Cheap to clone the `Arc` the manager threads into each
/// per-server task; writes serialize on the inner mutex.
pub(crate) struct LspLog {
    threshold: LogLevel,
    sink: Option<Mutex<std::fs::File>>,
}

impl LspLog {
    /// A silent log — no sink, threshold `Off`, so every [`log`](Self::log) is a
    /// no-op and [`enabled`](Self::enabled) is always `false`. The browser sync
    /// client (Phase 6e) uses this: the wasm build has no real filesystem for a log
    /// file, and a server's stderr is surfaced to the editor's messages, not here.
    #[cfg_attr(feature = "native", allow(dead_code))]
    pub(crate) fn disabled() -> LspLog {
        LspLog {
            threshold: LogLevel::Off,
            sink: None,
        }
    }

    /// Open the log per the environment and write the `[START]` banner. Returns a
    /// silent log (no file) when the level is `off` or the file can't be opened.
    /// Native only — the wasm build has no filesystem and uses [`Self::disabled`].
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    pub(crate) fn from_env() -> LspLog {
        let threshold = std::env::var("NXVIM_LSP_LOG_LEVEL")
            .ok()
            .and_then(|s| LogLevel::parse(&s))
            .unwrap_or(LogLevel::Warn);
        if threshold == LogLevel::Off {
            return LspLog {
                threshold,
                sink: None,
            };
        }
        let path = log_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let sink = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(Mutex::new);
        let log = LspLog { threshold, sink };
        log.emit("START", None, "LSP logging initiated");
        log
    }

    /// Write `message` (attributed to `server`) if `level` clears the threshold.
    pub(crate) fn log(&self, level: LogLevel, server: &str, message: &str) {
        if level < self.threshold {
            return;
        }
        self.emit(level.tag(), Some(server), message);
    }

    /// Whether a message at `level` would be written — lets a caller skip building
    /// an expensive message string when it would be dropped. Only the native
    /// manager guards log calls this way; the sync client always logs unconditionally.
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    pub(crate) fn enabled(&self, level: LogLevel) -> bool {
        self.sink.is_some() && level >= self.threshold
    }

    fn emit(&self, tag: &str, server: Option<&str>, message: &str) {
        let Some(sink) = &self.sink else {
            return;
        };
        let ts = format_timestamp(SystemTime::now());
        let line = match server {
            Some(server) => format!("[{tag}][{ts}] {server}\t{message}"),
            None => format!("[{tag}][{ts}] {message}"),
        };
        if let Ok(mut file) = sink.lock() {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    }
}

/// The log file path: `$NXVIM_LSP_LOG_FILE`, else `<state-dir>/lsp.log`.
#[cfg_attr(not(feature = "native"), allow(dead_code))]
fn log_path() -> PathBuf {
    if let Some(path) = std::env::var_os("NXVIM_LSP_LOG_FILE") {
        return PathBuf::from(path);
    }
    state_dir().join("lsp.log")
}

/// nxvim's per-user state directory: `$XDG_STATE_HOME/nxvim`, else
/// `$HOME/.local/state/nxvim` (and `%LOCALAPPDATA%\nxvim` on Windows). Mirrors
/// `nxvim_ts::data_dir`, but for *state* (logs) rather than data (grammars).
#[cfg_attr(not(feature = "native"), allow(dead_code))]
fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(dir).join("nxvim");
    }
    #[cfg(windows)]
    if let Some(dir) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(dir).join("nxvim");
    }
    #[cfg(not(windows))]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/nxvim");
    }
    PathBuf::from(".nxvim")
}

/// Format a `SystemTime` as `YYYY-MM-DD HH:MM:SS` in **UTC**, dependency-free
/// (Howard Hinnant's civil-from-days algorithm). UTC keeps it timezone-database
/// free; a debug log doesn't need local time.
fn format_timestamp(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (hour, min, sec) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02}")
}

/// Convert a count of days since the Unix epoch to a UTC `(year, month, day)`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (year + if month <= 2 { 1 } else { 0 }, month, day)
}
