//! The daemon wire protocol for the edit-host split (process + filesystem + blocking system).
//!
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md` → Phase 3 moves the
//! network boundary *below* the editor: the edit-host (core + Lua + treesitter)
//! runs **local** for a zero-round-trip keystroke path, and only fs + process +
//! watch — the lag-tolerant work — run on a remote **daemon**. This module holds
//! both legs of that wire — the daemon-side servers ([`serve_daemon`],
//! [`serve_fs_daemon`]) and the edit-host-side clients ([`RemoteHostProc`],
//! [`RemoteHostFs`]) — over any [`AsyncRead`]/[`AsyncWrite`] transport: an
//! in-process `tokio::io::duplex` (how the tests drive it), or — in the real split —
//! ssh stdio to `bemtvi --daemon`.
//!
//! ## The process leg (notifications)
//!
//! [`HostProc`] is already async + event-routed (pid then exit come back as separate
//! events, not a return value), so it maps onto a wire with no impedance mismatch.
//! Four notifications correlated by a per-spawn `id` the edit-host mints and the
//! daemon echoes back — notifications (not request/response) because a child's life
//! is two events at different times, which a single reply can't model:
//!
//! | direction | method | params |
//! | --- | --- | --- |
//! | edit-host → daemon | `proc_spawn` | `[id, argv, cwd?, env, stdin]` |
//! | edit-host → daemon | `proc_kill`  | `[id]` |
//! | daemon → edit-host | `proc_spawned` | `[id, pid?]` |
//! | daemon → edit-host | `proc_exited`  | `[id, code, stdout, stderr]` |
//!
//! The daemon runs each child through the *same* [`StdHostProc`] the local server
//! uses today — it relays that machinery's [`LoopEvent`]s straight onto the wire —
//! so a process behaves identically whether it ran here or across the network.
//!
//! ## The filesystem leg (request/response)
//!
//! Core's [`HostFs`](bemtvi_core::HostFs) is *synchronous* — a daemon-backed read
//! can't block the single editor thread on the network (the latency thesis) — so the
//! remote fs is **not** that sync trait. It is a small *async* seam, [`HostFsAsync`],
//! the server consumes **off the editor tick**: it fetches a buffer's bytes over the
//! wire, then hands core a populated replica via `Editor::load_str` (the in-memory
//! open the web build already uses). Unlike the process leg, a file read is naturally
//! request/response, so this needs no `id`/demux — `bemtvi_rpc`'s `request` routes the
//! reply directly:
//!
//! | direction | method | reply |
//! | --- | --- | --- |
//! | edit-host → daemon | `fs_read [path]`         | `["file", bytes]` / `["new"]` / `["dir", path, entries]`, or an RPC error |
//! | edit-host → daemon | `fs_chdir [path]`        | `["ok", canonical]` (a `:cd` target's resolved dir), or an `E344` RPC error |
//! | edit-host → daemon | `fs_write [path, bytes]` | `["ok", stat?]`, or an RPC error                |
//!
//! `serve_fs_daemon` reads an existing file (`file`), reports a not-yet-existing one as a
//! new-file buffer (`new`), or lists a directory (`dir` — the remote explorer, Phase 3g:
//! the daemon's canonical path plus its raw `[is_dir, name]` entries, which the edit-host
//! sorts and renders); any other read error comes back as a loud RPC error. `fs_write`
//! does the atomic write through the same sync [`HostFs`] and replies with the new
//! [`FileStat`](bemtvi_core::FileStat) (so the edit-host can stamp its `disk` snapshot
//! without a remote stat round-trip), or a loud error.
//!
//! **The save path is off-tick, like the read** (`docs/plans/…` → Phase 3e, *the save
//! slice*): core does *not* write through the sync [`HostFs`](bemtvi_core::HostFs) in a
//! daemon session — it snapshots the buffer at command time and enqueues a
//! [`PendingSave`](bemtvi_core::PendingSave); the server pushes those bytes over
//! `fs_write` off the editor tick and finalizes the buffer's saved-state only on the
//! daemon's ack, so a slow remote write never freezes typing. (`:read` still uses the
//! sync [`HostFs`], on local disk, for now.)
//!
//! ## The watch leg (`HostWatch` — server push)
//!
//! Only the daemon can watch a remote file, so it **owns change detection**: the
//! edit-host arms a watch per open file-backed buffer and the daemon pushes a change.
//! Unlike the read/write requests, a change is a server-initiated *notification* (the
//! one daemon→edit-host push on the fs leg), so it can't be a reply:
//!
//! | direction | method | params |
//! | --- | --- | --- |
//! | edit-host → daemon | `fs_watch [path, known?]` | arm a watch on `path` (`known` = the edit-host's disk baseline) |
//! | edit-host → daemon | `fs_unwatch [path]` | drop the watch |
//! | daemon → edit-host | `fs_changed [path, stat?]` | `path` changed (nil stat = vanished) |
//!
//! `serve_fs_daemon` baselines each watched path's stat at `fs_watch` time and re-stats
//! on a coarse [`WATCH_POLL`] interval (the daemon is the lag-tolerant leg), pushing
//! `fs_changed` whenever one drifts. The optional `known` stat closes the **reconnect**
//! gap: a re-dialed daemon is a fresh process that lost every prior baseline, so a file
//! changed *during the outage* would otherwise be silently re-baselined as the new normal.
//! When the edit-host re-arms a watch it passes its own last-read/written stat as `known`;
//! the daemon compares it to the live stat and, if they differ, pushes `fs_changed`
//! immediately (the edit-host reconciles via the normal `reconcile_remote_change` path —
//! autoread reload or `FileChangedShell`). On the initial arm `known` equals the live stat,
//! so nothing spurious fires. A successful `fs_write` refreshes the baseline so
//! the edit-host's **own** save doesn't echo back as an external change. The edit-host
//! turns each push into a [`WatchEvent`] the server reconciles off the editor tick (the
//! `FileChangedShell` round-trip; a reload re-fetches over `fs_read`) — the remote
//! analogue of the local per-buffer file watch.
//!
//! ## The LSP leg (`lsp_*` — long-lived bidirectional pipes)
//!
//! A language server is neither run-to-completion (the `proc_*` leg) nor
//! request/response (`fs_*`): it is a *long-lived child whose stdio is a raw
//! bidirectional pipe*, JSON-RPC flowing both ways for the server's whole life and
//! stdout consumed incrementally. So this leg streams the pipe itself — raw stdin/stdout/
//! stderr chunks correlated by a per-spawn `id`:
//!
//! | direction | method | params |
//! | --- | --- | --- |
//! | edit-host → daemon | `lsp_spawn` | `[id, program, args, cwd, env]` |
//! | edit-host → daemon | `lsp_stdin` | `[id, bytes]` |
//! | edit-host → daemon | `lsp_kill`  | `[id]` |
//! | daemon → edit-host | `lsp_stdout` | `[id, bytes]` |
//! | daemon → edit-host | `lsp_stderr` | `[id, bytes]` |
//! | daemon → edit-host | `lsp_exited` | `[id, code?, signal?]` |
//!
//! [`RemoteLspTransport`] (the edit-host side, an [`LspTransport`]) hands the
//! [`LspManager`](bemtvi_lsp::LspManager) a [`LspChannel`] whose stdout/stderr are fed by
//! demuxed `lsp_stdout`/`lsp_stderr` chunks and whose stdin is pumped onto the wire as
//! `lsp_stdin` — so the manager drives its `async-lsp` loop unchanged, never knowing the
//! server runs across the network. `serve_lsp_daemon` spawns the actual child (the *same*
//! `tokio::process` machinery the local transport uses) and streams its pipes back; it
//! joins the stdout/stderr pumps before signaling `lsp_exited`, so no trailing output is
//! lost to the exit.

use std::collections::HashMap;
use std::future::Future;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, UNIX_EPOCH};

use rmpv::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::sync::mpsc::{
    channel, unbounded_channel, Receiver, Sender, UnboundedReceiver, UnboundedSender,
};
use tokio::sync::oneshot;
use tokio::sync::watch;

use bemtvi_core::{DirEntry, FileStat, HostFs};
use bemtvi_lsp::{LspChannel, LspProcess, LspTransport, ServerSpawn};
use bemtvi_lua::LuaFs;
use bemtvi_rpc::{connect_bounded, Incoming, Rpc};

use crate::evloop::LoopEvent;
use crate::host::{HostProc, ProcEvents, ProcSpec, StdHostProc};
use crate::remote_config::{decode_config_bundle, RemoteConfigBundle};

const FS_READ: &str = "fs_read";
const FS_WRITE: &str = "fs_write";
// `:cd` in a daemon session: resolve + validate a directory on the daemon and reply with
// its canonical path (request/response, like a read). Pure — it does NOT chdir the daemon
// *process* (one daemon serves many concurrent sessions; a process-global cwd would
// corrupt the others), so the edit-host owns the logical cwd in `DirState` and resolves
// its own relative paths against it. See `docs/plans/2026-06-23-remote-cwd.md`.
const FS_CHDIR: &str = "fs_chdir";
// Whole-directory primitives for the remote-shada mirror (Approach A, per-instance):
// `fs_mkdir` ensures the per-namespace remote shada dir exists before the first upload
// (`fs_write` doesn't create parents), and `fs_remove` deletes an absorbed sibling store
// on the daemon at clean-exit compaction. Both request/response, like a read.
const FS_MKDIR: &str = "fs_mkdir";
const FS_REMOVE: &str = "fs_remove";
// The watch leg (`HostWatch`): the edit-host arms/disarms watches on the daemon, the
// daemon pushes a change. Server-*push* — the only daemon→edit-host *notification* on
// the fs leg (reads/writes are request/response).
const FS_WATCH: &str = "fs_watch";
const FS_UNWATCH: &str = "fs_unwatch";
const FS_CHANGED: &str = "fs_changed";

/// How often the daemon re-stats its watched paths. The daemon is the lag-tolerant
/// leg (the whole reason the watch lives here, not on the editor tick), so a coarse
/// poll is fine — it owns change detection and the edit-host only reacts to a push.
const WATCH_POLL: Duration = Duration::from_millis(200);

// Wire method names. Kept as constants so the two halves can never drift on a typo.
const PROC_SPAWN: &str = "proc_spawn";
const PROC_KILL: &str = "proc_kill";
const PROC_SPAWNED: &str = "proc_spawned";
const PROC_STDOUT: &str = "proc_stdout";
const PROC_EXITED: &str = "proc_exited";

// The terminal leg (`term_*`): a *streaming* PTY per buffer — the web `:terminal`
// (Phase 7). Unlike the run-to-completion process leg above, a terminal stays open
// for its whole life with raw PTY bytes flowing both ways: the edit-host pushes
// keystrokes/resizes in, the daemon streams the child's output back. The daemon runs
// the real PTY via the native [`TerminalManager`](crate::terminal::native::TerminalManager)
// (the same engine a local `:terminal` uses); the browser owns the vt100 emulation.
const TERM_OPEN: &str = "term_open";
const TERM_WRITE: &str = "term_write";
const TERM_RESIZE: &str = "term_resize";
const TERM_KILL: &str = "term_kill";
const TERM_DATA: &str = "term_data";
const TERM_EXIT: &str = "term_exit";

// The LSP leg: a *long-lived bidirectional pipe* per language server. Unlike every
// other leg (run-to-completion `proc_*`, request/response `fs_*`), a
// language server's stdio stays open for its whole life, with JSON-RPC flowing both
// ways and stdout consumed incrementally — so the wire streams raw stdin/stdout/stderr
// chunks correlated by a per-spawn `id`, never a single buffered result.
const LSP_SPAWN: &str = "lsp_spawn"; // edit-host → daemon: [id, program, args, cwd, env]
const LSP_STDIN: &str = "lsp_stdin"; // edit-host → daemon: [id, bytes]
const LSP_KILL: &str = "lsp_kill"; // edit-host → daemon: [id]
const LSP_STDOUT: &str = "lsp_stdout"; // daemon → edit-host: [id, bytes]
const LSP_STDERR: &str = "lsp_stderr"; // daemon → edit-host: [id, bytes]
const LSP_EXITED: &str = "lsp_exited"; // daemon → edit-host: [id, code?, signal?]

// The duplex-process leg (`dproc_*`): a *long-lived bidirectional pipe* per
// `btv.process.open` child — the DAP / framed-protocol transport. Like the LSP leg
// (and unlike the run-to-completion `proc_*`), the child's stdio stays open for its
// whole life with raw bytes flowing both ways. Distinct from `lsp_*` because the
// edit-host routes its output to Lua callbacks, not the LSP client.
const DPROC_OPEN: &str = "dproc_open"; // edit-host → daemon: [id, argv, cwd, env]
const DPROC_WRITE: &str = "dproc_write"; // edit-host → daemon: [id, bytes]
const DPROC_KILL: &str = "dproc_kill"; // edit-host → daemon: [id]
const DPROC_OUT: &str = "dproc_out"; // daemon → edit-host: [id, bytes, is_stderr]
const DPROC_EXIT: &str = "dproc_exit"; // daemon → edit-host: [id, code]

// The socket leg (`sock_*`): a *long-lived bidirectional TCP connection* per
// `btv.socket.connect` — a DAP `type="server"` adapter transport. The daemon dials
// the host:port and streams bytes both ways.
const SOCK_CONNECT: &str = "sock_connect"; // edit-host → daemon: [id, host, port]
const SOCK_WRITE: &str = "sock_write"; // edit-host → daemon: [id, bytes]
const SOCK_CLOSE: &str = "sock_close"; // edit-host → daemon: [id]
const SOCK_CONNECTED: &str = "sock_connected"; // daemon → edit-host: [id]
const SOCK_DATA: &str = "sock_data"; // daemon → edit-host: [id, bytes]
const SOCK_CLOSED: &str = "sock_closed"; // daemon → edit-host: [id, error?]

// The Lua-`btv.fs` off-tick op leg (`luafs_op`): a request/response per **high-level**
// `btv.fs.*` op (`readdir` / `read_text` / `write` / `copy{recursive}` / …) — the ONE fs
// path both the native-daemon edit-host (via [`RemoteFsJobs`]) and the wasm edit-host (over
// WebTransport) use. It carries a whole [`FsJob`](bemtvi_lua::FsJob) and runs it through
// [`run_fs_job`](bemtvi_lua::run_fs_job) on the daemon — so a compound op (a recursive copy /
// remove) decomposes into local syscalls daemon-side rather than a round-trip per step. The
// request is a map (`{ op, path, … }`), the reply the `["ok", <fs-value>] | ["err", code,
// message]` envelope `bemtvi_lua::fswire` encodes. (The retired low-level per-`LuaFs`-op
// `luafs` leg, which backed the removed synchronous `vim.fn` fs builtins, is gone.)
const LUAFS_OP: &str = "luafs_op";

// The Lua-`btv.http.fetch` off-tick leg (`http_op`): a request/response per fetch, carrying
// a whole [`HttpRequest`](bemtvi_lua::HttpRequest) run through
// [`run_http_request`](crate::http::run_http_request) on the daemon (which owns the network
// and dodges the browser's CORS — the same reason `btv.fs` / processes route to the daemon).
// The request is a map (`{ method, url, headers, body, … }`), the reply the `["ok", …] |
// ["err", message]` envelope `bemtvi_lua::httpwire` encodes. Both the native-daemon edit-host
// (via [`RemoteHttp`]) and the wasm edit-host (over WebTransport) use this one leg.
const HTTP_OP: &str = "http_op";

// The Lua-`btv.git` off-tick leg (`git_op`): a request/response per op, carrying a whole
// [`GitJob`](bemtvi_lua::GitJob) run through `bemtvi_git::run_git_job` on the daemon (which
// owns the repo — the same reason `btv.fs` routes there). The request is a map (`{ op, path,
// … }`), the reply the `["ok", <git-value>] | ["err", code, message]` envelope
// `bemtvi_lua::gitwire` encodes. Both the native-daemon edit-host (via [`RemoteGitJobs`]) and
// a web edit-host use this one leg; a serverless session with no daemon rejects loud.
const GIT_OP: &str = "git_op";

// The Lua-`btv.fs.watch` streaming leg (`luafs_watch`) — the route BOTH daemon-backed
// edit-hosts take: the browser's Worker (Phase 3b of the off-tick plan) and, via
// [`RemoteFsWatch`], the native `--connect-daemon` session. DISTINCT from the
// buffer-reconcile `fs_watch` leg (a coarse single-path stat-poll keyed by path): this is a
// recursive, change-classified watch keyed by a stream `id`, reusing the native event-loop
// actor's coalescing watcher
// ([`start_fs_watch_coalesced`](crate::evloop::start_fs_watch_coalesced)). The edit-host arms /
// disarms by notification; the daemon pushes change batches / a terminal error back.
const LUAFS_WATCH: &str = "luafs_watch"; // edit-host → daemon: [id, path, recursive]
const LUAFS_UNWATCH: &str = "luafs_unwatch"; // edit-host → daemon: [id]
const LUAFS_CHANGE: &str = "luafs_change"; // daemon → edit-host: [id, kind, [path, …]]
const LUAFS_WATCH_ERR: &str = "luafs_watch_err"; // daemon → edit-host: [id, message]

// The config leg (`config_*`): a single request/response that ships the daemon's
// whole config surface — its `config_dir`, `runtimepath`, and every source file under
// those roots — so a remote session loads the *daemon's* config + plugins (fetched,
// materialized locally, then run locally), not the client's. One round trip; the
// daemon walks the tree daemon-side and the edit-host mirrors it onto a local cache.
// See `docs/plans/2026-06-23-remote-config-and-plugins.md`.
//
// | direction | method | reply |
// | edit-host → daemon | `config_bundle []` | `[config_dir?, [runtimepath…], [[abspath, bytes], …], [ts_lang…]]`, or a loud error |
//
// `ts_lang…` is the daemon's installed tree-sitter parser languages; the client
// auto-installs the same set locally (parsers are native artifacts, never fetched).
const CONFIG_BUNDLE: &str = "config_bundle";

/// One latency class of daemon traffic — the unit a multi-stream transport
/// (QUIC/WebTransport) gives its **own** bidi stream, so a flood on one class can't
/// head-of-line-block another at the protocol level. The four groups partition every wire
/// method (see [`LegGroup::classify`]); a single-stream transport (ssh/stdio, the
/// in-process test duplex) carries all four over one stream instead, demuxed by method.
///
/// A stream's group is fixed by a one-byte **tag** ([`LegGroup::tag`]) the *client* writes
/// as the stream's first byte; the daemon reads it ([`LegGroup::from_tag`]) and routes the
/// rest of the stream to that group's legs. See
/// `docs/plans/2026-06-26-multi-stream-daemon-transport.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LegGroup {
    /// Latency-critical + low-volume: `fs_*` (save/read/watch), `config_*` (one-shot
    /// bundle), and the `btv.fs` legs (`luafs_op`, `luafs_watch`/`luafs_unwatch`).
    Control,
    /// Run-to-completion process floods (`proc_*` — `rg`, `npm install`).
    Proc,
    /// The long-lived bidirectional LSP pipe (`lsp_*`).
    Lsp,
    /// The streaming PTY (`term_*`) — a browser-only sender today.
    Term,
}

impl LegGroup {
    /// The wire tag byte that names this group on a freshly-opened stream. The client
    /// writes it as the stream's first byte; the daemon reads it ([`from_tag`]) and
    /// dispatches the rest of the stream to this group's legs.
    ///
    /// [`from_tag`]: LegGroup::from_tag
    pub(crate) fn tag(self) -> u8 {
        match self {
            LegGroup::Control => 0,
            LegGroup::Proc => 1,
            LegGroup::Lsp => 2,
            LegGroup::Term => 3,
        }
    }

    /// Resolve a stream's leading tag byte back to its group, or a loud error on an
    /// unrecognised tag (a protocol mismatch — never silently dropped, per
    /// `No silent stubs or skips`).
    pub(crate) fn from_tag(b: u8) -> anyhow::Result<Self> {
        match b {
            0 => Ok(LegGroup::Control),
            1 => Ok(LegGroup::Proc),
            2 => Ok(LegGroup::Lsp),
            3 => Ok(LegGroup::Term),
            other => Err(anyhow::anyhow!("unknown daemon stream group tag {other}")),
        }
    }

    /// The group that owns a wire method, or `None` for an unknown method (the peer is
    /// the same build, so an unrecognised method is dropped). The four arms partition the
    /// method namespace disjointly — `fs_*` / `config_*` / `luafs_*` / `http_*` to [`Control`],
    /// `proc_*` / `dproc_*` / `sock_*` to [`Proc`], `lsp_*` to [`Lsp`], `term_*` to [`Term`].
    /// (`dproc_*` / `sock_*` are the duplex `btv.process` / `btv.socket` DAP transports — they
    /// ride the Proc stream as process/socket siblings, kept off the latency-critical
    /// Control stream.)
    ///
    /// [`Control`]: LegGroup::Control
    /// [`Proc`]: LegGroup::Proc
    /// [`Lsp`]: LegGroup::Lsp
    /// [`Term`]: LegGroup::Term
    pub(crate) fn classify(method: &str) -> Option<Self> {
        if method.starts_with("fs_")
            || method.starts_with("config_")
            || method.starts_with("luafs_")
            || method.starts_with("http_")
            || method.starts_with("git_")
        {
            Some(LegGroup::Control)
        } else if method.starts_with("proc_")
            || method.starts_with("dproc_")
            || method.starts_with("sock_")
        {
            Some(LegGroup::Proc)
        } else if method.starts_with("lsp_") {
            Some(LegGroup::Lsp)
        } else if method.starts_with("term_") {
            Some(LegGroup::Term)
        } else {
            None
        }
    }
}

/// What the daemon reports back about one child, demuxed off the wire and handed to
/// the [`RemoteHostProc::run`] future waiting on that spawn's `id`. Mirrors the two
/// [`ProcEvents`] reports the future then re-emits to the editor.
enum DaemonEvent {
    /// The child is running (or failed to spawn — `None` pid).
    Spawned(Option<u32>),
    /// A streaming child emitted a batch of stdout lines (`btv.run_stream`'s streamed stdout).
    Stdout(Vec<String>),
    /// The child exited (`code = -1` on spawn failure or a kill).
    Exited {
        code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
}

/// The table of spawns awaiting their daemon reports: `id` → the channel into the
/// [`RemoteHostProc::run`] future driving that child. The demux task forwards each
/// `proc_spawned` / `proc_exited` to the matching sender; the future removes its own
/// entry when the child exits.
type Inflight = Arc<Mutex<HashMap<u64, UnboundedSender<DaemonEvent>>>>;

// ----- reconnectable link (Phase 1 of `docs/plans/2026-06-29-daemon-reconnect.md`) ------
//
// The editor runs *local*; the daemon only provides the fs/proc/lsp/term seams. So a
// dropped connection must not tear the session down (that loses the local buffers/undo) —
// it must re-dial *underneath the seam handles the editor already holds*. [`LinkRpc`] is
// the indirection that makes that possible: every seam issues on a `LinkRpc` rather than a
// concrete [`Rpc`], so the link supervisor can swap the current connection's `Rpc` in and
// out without the seams (or the editor) ever being rebuilt. While disconnected the cell is
// empty and every request/notify fails *loud* — never hangs.

/// A swappable handle to the current daemon connection's [`Rpc`]. Cloned into every seam;
/// the link supervisor publishes a live `Rpc` on each (re)dial and clears it on a drop.
#[derive(Clone)]
pub(crate) struct LinkRpc {
    inner: Arc<Mutex<Option<Rpc>>>,
}

impl LinkRpc {
    /// An empty (disconnected) cell — the reconnecting supervisor fills it on each dial.
    fn empty() -> LinkRpc {
        LinkRpc {
            inner: Arc::new(Mutex::new(None)),
        }
    }

    /// A fixed, never-swapped handle wrapping one connection's `Rpc` — the one-shot paths
    /// (the per-leg `connect` constructors) that never reconnect.
    fn fixed(rpc: Rpc) -> LinkRpc {
        LinkRpc {
            inner: Arc::new(Mutex::new(Some(rpc))),
        }
    }

    /// Publish the current connection's `Rpc` (on dial), or clear it (on drop).
    fn set(&self, rpc: Option<Rpc>) {
        *self.inner.lock().unwrap() = rpc;
    }

    /// The current `Rpc`, cloned (cheap — it is `Arc`-backed), or `None` while disconnected.
    fn current(&self) -> Option<Rpc> {
        self.inner.lock().unwrap().clone()
    }

    /// Issue a request on the current connection; a loud error while disconnected (never a
    /// hang), so a seam op during an outage fails fast instead of parking forever.
    async fn request(&self, method: &str, params: Vec<Value>) -> anyhow::Result<Value> {
        match self.current() {
            Some(rpc) => rpc.request(method, params).await,
            None => Err(anyhow::anyhow!("daemon disconnected")),
        }
    }

    /// Fire a notification on the current connection; dropped while disconnected.
    fn notify(&self, method: &str, params: Vec<Value>) {
        if let Some(rpc) = self.current() {
            rpc.notify(method, params);
        }
    }
}

/// One dialed daemon connection's four leg-group links (Control/Proc/Lsp/Term). The
/// single-stream dialer ([`dial_single_stream`]) splits one stream into the four; the QUIC
/// dialer ([`crate::quic`]) opens one real stream per group and builds this directly via
/// [`DialedConnection::from_groups`].
pub(crate) struct DialedConnection {
    control: GroupLink,
    proc: GroupLink,
    lsp: GroupLink,
    term: GroupLink,
}

impl DialedConnection {
    /// Assemble a connection from its four already-built per-group links — the QUIC dialer's
    /// entry point (one `GroupLink` per real bidi stream). The single-stream path builds the
    /// same shape by splitting one stream ([`split_single_stream`]).
    pub(crate) fn from_groups(
        control: GroupLink,
        proc: GroupLink,
        lsp: GroupLink,
        term: GroupLink,
    ) -> DialedConnection {
        DialedConnection {
            control,
            proc,
            lsp,
            term,
        }
    }
}

/// The stable, connection-independent state a link keeps across re-dials: the swappable
/// per-group [`LinkRpc`] cells the seams issue on, plus the shared inbound state the
/// per-connection demuxes feed. All of this outlives any one connection, so seam handles
/// and push channels survive a reconnect.
struct LinkState {
    control_rpc: LinkRpc,
    proc_rpc: LinkRpc,
    lsp_rpc: LinkRpc,
    term_rpc: LinkRpc,
    proc_inflight: Inflight,
    lsp_inflight: LspInflightMap,
    watch_tx: UnboundedSender<WatchEvent>,
    /// The stable channel the Control demux decodes `luafs_change`/`luafs_watch_err`
    /// pushes onto (the streaming `btv.fs.watch` leg). Stable across re-dials, like
    /// [`watch_tx`](Self::watch_tx) — a watch outlives the connection that armed it.
    fs_watch_tx: UnboundedSender<LoopEvent>,
    /// The armed-watch registry + notifier, held here so a re-dial can re-arm every live
    /// watch on the fresh daemon ([`publish_cells`]).
    fs_watch: RemoteFsWatch,
    term_event_tx: Sender<crate::terminal::native::TermEvent>,
    /// Taken once to drive the `luafs_op` job server (which survives reconnects via the
    /// swappable Control cell).
    fs_jobs_rx: Option<UnboundedReceiver<FsJobReq>>,
    /// Taken once to drive the `http_op` job server (the HTTP twin of `fs_jobs_rx`).
    http_jobs_rx: Option<UnboundedReceiver<HttpJobReq>>,
    /// Taken once to drive the `git_op` job server (the git twin of `fs_jobs_rx`).
    git_jobs_rx: Option<UnboundedReceiver<GitJobReq>>,
}

impl LinkState {
    fn take_fs_jobs_rx(&mut self) -> UnboundedReceiver<FsJobReq> {
        self.fs_jobs_rx
            .take()
            .expect("LinkState::take_fs_jobs_rx called once")
    }

    fn take_http_jobs_rx(&mut self) -> UnboundedReceiver<HttpJobReq> {
        self.http_jobs_rx
            .take()
            .expect("LinkState::take_http_jobs_rx called once")
    }

    fn take_git_jobs_rx(&mut self) -> UnboundedReceiver<GitJobReq> {
        self.git_jobs_rx
            .take()
            .expect("LinkState::take_git_jobs_rx called once")
    }
}

/// Build the six edit-host seams over fresh, empty [`LinkRpc`] cells + the stable push
/// channels, returning the [`LinkState`] the supervisor drives and the [`DaemonClient`] the
/// editor holds. The seams reference the cells, so a later [`LinkState`] re-dial rebinds
/// them in place. Shared by the one-shot path ([`serve_daemon_link_inner`]) and the
/// reconnecting path ([`connect_daemon_reconnecting_on`]).
fn build_link() -> (LinkState, DaemonClient) {
    let control_rpc = LinkRpc::empty();
    let proc_rpc = LinkRpc::empty();
    let lsp_rpc = LinkRpc::empty();
    let term_rpc = LinkRpc::empty();
    let proc_inflight: Inflight = Arc::new(Mutex::new(HashMap::new()));
    let lsp_inflight: LspInflightMap = Arc::new(Mutex::new(HashMap::new()));
    let (watch_tx, watch_rx) = unbounded_channel::<WatchEvent>();
    let (fs_watch_tx, fs_watch_rx) = unbounded_channel::<LoopEvent>();
    let fs_watch = RemoteFsWatch {
        rpc: control_rpc.clone(),
        armed: Arc::new(Mutex::new(HashMap::new())),
        events_rx: Arc::new(Mutex::new(Some(fs_watch_rx))),
    };
    let (term_event_tx, term_event_rx) =
        channel::<crate::terminal::native::TermEvent>(REMOTE_TERM_EVENT_CAP);
    let (fs_jobs_tx, fs_jobs_rx) = unbounded_channel::<FsJobReq>();
    let (http_jobs_tx, http_jobs_rx) = unbounded_channel::<HttpJobReq>();
    let (git_jobs_tx, git_jobs_rx) = unbounded_channel::<GitJobReq>();

    let client = DaemonClient {
        host_fs: RemoteHostFs {
            rpc: control_rpc.clone(),
            watch_rx: Mutex::new(Some(watch_rx)),
        },
        host_proc: RemoteHostProc {
            rpc: proc_rpc.clone(),
            inflight: proc_inflight.clone(),
            next_id: AtomicU64::new(1),
        },
        lsp_transport: RemoteLspTransport {
            rpc: lsp_rpc.clone(),
            inflight: lsp_inflight.clone(),
            next_id: AtomicU64::new(1),
        },
        host_term: RemoteHostTerm::from_parts(term_rpc.clone(), term_event_rx),
        fs_jobs: RemoteFsJobs { req_tx: fs_jobs_tx },
        fs_watch: fs_watch.clone(),
        git_jobs: RemoteGitJobs {
            req_tx: git_jobs_tx,
        },
        http: RemoteHttp {
            req_tx: http_jobs_tx,
        },
        config: RemoteConfig {
            rpc: control_rpc.clone(),
        },
    };
    let state = LinkState {
        control_rpc,
        proc_rpc,
        lsp_rpc,
        term_rpc,
        proc_inflight,
        lsp_inflight,
        watch_tx,
        fs_watch_tx,
        fs_watch,
        term_event_tx,
        fs_jobs_rx: Some(fs_jobs_rx),
        http_jobs_rx: Some(http_jobs_rx),
        git_jobs_rx: Some(git_jobs_rx),
    };
    (state, client)
}

/// Serve one connection: publish its per-group `Rpc`s into the swappable cells, then run the
/// four per-group demuxes (which feed the *stable* push channels). Returns when the
/// connection drops (all four group streams EOF). The caller [`clear_cells`] afterwards (so
/// the clear also covers the case where serving is *cancelled* by a `:disconnect`).
async fn run_connection(state: &LinkState, conn: DialedConnection) {
    publish_cells(state, &conn);

    // The `luafs_op` job server already runs against the Control cell for the link's
    // lifetime (spawned by the caller), so it is not re-run here. The four demuxes are the
    // connection's; they end when its streams EOF.
    tokio::join!(
        run_control_demux(
            conn.control.incoming,
            state.watch_tx.clone(),
            state.fs_watch_tx.clone()
        ),
        run_demux(conn.proc.incoming, state.proc_inflight.clone()),
        run_lsp_demux(conn.lsp.incoming, state.lsp_inflight.clone()),
        run_term_demux(conn.term.incoming, state.term_event_tx.clone()),
    );
}

/// Publish `conn`'s four per-group `Rpc`s into the swappable cells, so the seams the editor
/// holds route onto this connection. Split out from [`run_connection`] so it can also fill the
/// cells *before* the client is handed back — **both** dialers publish here first, so the
/// caller's first seam op (the config-resolve round trip it issues the instant the dial
/// returns) lands on a live cell instead of racing the code that would otherwise fill it: the
/// not-yet-polled supervisor task on the reconnecting path, the link thread's own next step on
/// the one-shot [`serve_daemon_link_inner`] path. And a re-dial publishes before
/// announcing [`DaemonStatus::Connected`] (the editor's reconnect resync fires on that
/// transition and must re-arm watches / re-open LSP onto live cells, never the just-cleared
/// ones). Idempotent — `run_connection` re-publishes the same `Rpc`s.
fn publish_cells(state: &LinkState, conn: &DialedConnection) {
    state.control_rpc.set(Some(conn.control.rpc.clone()));
    state.proc_rpc.set(Some(conn.proc.rpc.clone()));
    state.lsp_rpc.set(Some(conn.lsp.rpc.clone()));
    state.term_rpc.set(Some(conn.term.rpc.clone()));
    // Streaming `btv.fs.watch` subscriptions are re-armed on the fresh daemon, which knows
    // about none of them: a live watch iterator (a file tree, the LSP file-watch client)
    // otherwise survives the outage as an object that simply never yields again — deaf,
    // with nothing to say so. Runs on the initial connect too, where nothing is armed and
    // it is a no-op. The buffer-reconcile `fs_watch` leg is re-armed separately, by the
    // editor's own `resync_after_reconnect` (it re-sends each path's disk baseline).
    state.fs_watch.rearm_all();
}

/// Empty the swappable cells so every subsequent seam op fails loud until the next dial (the
/// demuxes already cleared the proc/lsp inflight maps, failing their pending spawns with a
/// synthesized exit rather than a hang). Called after a connection drops *or* is cancelled.
fn clear_cells(state: &LinkState) {
    state.control_rpc.set(None);
    state.proc_rpc.set(None);
    state.lsp_rpc.set(None);
    state.term_rpc.set(None);
    // Also drop every pending proc/LSP waiter. The demuxes clear these maps when a
    // connection ends *naturally* (their `incoming` streams EOF), but a **cancelled**
    // serve — the `:disconnect` path drops the `run_connection` future mid-poll —
    // never reaches those clears, and a spawn awaiting its daemon report would park
    // forever (its sender stays alive inside the shared map, which outlives the
    // connection). Dropping the senders here is exactly the synthesized-exit path
    // the natural end relies on; doing it twice is an idempotent no-op.
    state.proc_inflight.lock().unwrap().clear();
    state.lsp_inflight.lock().unwrap().clear();
}

/// Split one freshly-connected **single-stream** transport (a duplex / ssh stdio) into the
/// four leg-group [`GroupLink`]s by method — the reconnecting twin of [`serve_daemon_link`],
/// and the body a single-stream dialer wraps. Synchronous (it only builds the `Rpc` + spawns
/// the demux split), so it must be called within a tokio runtime. The QUIC dialer skips this
/// entirely — it opens a real stream per group and builds the [`DialedConnection`] directly.
fn split_single_stream<R, W>(reader: R, writer: W) -> DialedConnection
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (rpc, incoming) = connect_bounded(reader, writer);
    split_groups(rpc, incoming)
}

/// Fan one already-connected single-stream link into the four leg-group
/// [`GroupLink`]s (spawning the [`split_incoming`] demux) — the body shared by
/// [`split_single_stream`] and the one-shot [`serve_daemon_link`]. The per-group
/// channels are bounded ([`bemtvi_rpc::IN_CAP`]) so a peer flooding one group's
/// methods backpressures the split instead of growing that group's queue.
fn split_groups(rpc: Rpc, incoming: Receiver<Incoming>) -> DialedConnection {
    let (ctrl_tx, ctrl_rx) = channel::<Incoming>(bemtvi_rpc::IN_CAP);
    let (proc_tx, proc_rx) = channel::<Incoming>(bemtvi_rpc::IN_CAP);
    let (lsp_tx, lsp_rx) = channel::<Incoming>(bemtvi_rpc::IN_CAP);
    let (term_tx, term_rx) = channel::<Incoming>(bemtvi_rpc::IN_CAP);
    tokio::spawn(split_incoming(incoming, ctrl_tx, proc_tx, lsp_tx, term_tx));
    DialedConnection::from_groups(
        GroupLink {
            rpc: rpc.clone(),
            incoming: ctrl_rx,
        },
        GroupLink {
            rpc: rpc.clone(),
            incoming: proc_rx,
        },
        GroupLink {
            rpc: rpc.clone(),
            incoming: lsp_rx,
        },
        GroupLink {
            rpc,
            incoming: term_rx,
        },
    )
}

/// The connection state of a reconnecting daemon link, surfaced to the editor (and through
/// it to `btv.daemon.status()` / a statusline component, later phases). Carried on a
/// [`watch`] channel so a consumer reads the latest value and awaits changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonStatus {
    /// A live connection — every seam works.
    Connected,
    /// The connection dropped and the supervisor is auto-retrying (`attempt` of `max`).
    Reconnecting { attempt: u32, max: u32 },
    /// No connection: either the auto-retry budget is exhausted (the editor should tell the
    /// user to run `:reconnect`) or `:disconnect` was issued. The supervisor stays parked
    /// until [`ReconnectHandle::reconnect`].
    Disconnected,
}

/// How aggressively a dropped link is auto-retried before giving up and waiting for a manual
/// `:reconnect`. Backoff for attempt *n* is `min(base * 2^(n-1), cap)`.
#[derive(Clone, Copy, Debug)]
pub struct ReconnectPolicy {
    /// How many auto-retries before giving up (then a manual `:reconnect` resets the budget).
    pub max_attempts: u32,
    /// The first attempt's backoff; each subsequent attempt doubles it up to `cap`.
    pub base: Duration,
    /// The longest a single backoff may grow to.
    pub cap: Duration,
}

impl Default for ReconnectPolicy {
    /// "A few times, then tell the user to `:reconnect`": 5 attempts over 0.5 → 8 s.
    fn default() -> Self {
        ReconnectPolicy {
            max_attempts: 5,
            base: Duration::from_millis(500),
            cap: Duration::from_secs(8),
        }
    }
}

impl ReconnectPolicy {
    /// The backoff before attempt `n` (1-based), capped.
    fn backoff(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(31);
        let scaled = self.base.saturating_mul(1u32 << shift);
        scaled.min(self.cap)
    }
}

/// A control message from the editor to the link supervisor.
#[derive(Clone, Copy)]
enum LinkCommand {
    /// `:reconnect` — re-dial now and reset the retry budget.
    Reconnect,
    /// `:disconnect` — drop the live connection (if any) and stay down until a `:reconnect`.
    Disconnect,
}

/// A handle the editor holds to drive a reconnecting daemon link: trigger a `:reconnect` /
/// `:disconnect`, read the current [`DaemonStatus`], or subscribe to status changes. The
/// seams rebind in place on a re-dial — the editor keeps its local buffers/undo.
pub struct ReconnectHandle {
    cmd_tx: UnboundedSender<LinkCommand>,
    status: watch::Receiver<DaemonStatus>,
}

impl ReconnectHandle {
    /// Re-dial the daemon now, resetting the auto-retry budget. Use after the supervisor has
    /// given up (status [`DaemonStatus::Disconnected`]), or to retry sooner than the backoff.
    pub fn reconnect(&self) {
        let _ = self.cmd_tx.send(LinkCommand::Reconnect);
    }

    /// Drop the live connection (if any) and stay disconnected until a [`reconnect`](Self::reconnect).
    pub fn disconnect(&self) {
        let _ = self.cmd_tx.send(LinkCommand::Disconnect);
    }

    /// The current link status.
    pub fn status(&self) -> DaemonStatus {
        *self.status.borrow()
    }

    /// A receiver the editor's run loop selects on to learn of status changes off the tick.
    pub fn subscribe(&self) -> watch::Receiver<DaemonStatus> {
        self.status.clone()
    }
}

/// How serving a single connection ended.
enum Served {
    /// The connection dropped (its streams EOF'd) — the supervisor should auto-retry.
    Dropped,
    /// `:disconnect` cancelled it — stay down until a manual `:reconnect`.
    Disconnect,
    /// The editor dropped its [`ReconnectHandle`] (session ending) — stop maintaining.
    Closed,
}

/// Serve `conn` until it drops, or until a `:disconnect`/handle-drop arrives on `cmd_rx`.
/// A `:reconnect` while already connected is ignored (we keep serving). The caller
/// [`clear_cells`] after this returns (covering the cancelled case too).
async fn serve_connection(
    state: &LinkState,
    conn: DialedConnection,
    cmd_rx: &mut UnboundedReceiver<LinkCommand>,
) -> Served {
    let serving = run_connection(state, conn);
    tokio::pin!(serving);
    loop {
        tokio::select! {
            _ = &mut serving => return Served::Dropped,
            cmd = cmd_rx.recv() => match cmd {
                Some(LinkCommand::Reconnect) => continue, // already connected: ignore
                Some(LinkCommand::Disconnect) => return Served::Disconnect,
                None => return Served::Closed,
            },
        }
    }
}

/// Sleep for `dur`, unless a command arrives first.
enum Waited {
    Elapsed,
    Cmd(LinkCommand),
    Closed,
}

async fn wait_or_cmd(dur: Duration, cmd_rx: &mut UnboundedReceiver<LinkCommand>) -> Waited {
    tokio::select! {
        _ = tokio::time::sleep(dur) => Waited::Elapsed,
        cmd = cmd_rx.recv() => match cmd {
            Some(c) => Waited::Cmd(c),
            None => Waited::Closed,
        },
    }
}

/// Re-dial a dropped link with bounded backoff. On a successful dial, serve that connection
/// and return how *it* ended. If `immediate`, the first attempt skips its backoff (a manual
/// `:reconnect`). If every attempt fails, returns [`Served::Disconnect`] so the caller parks
/// as "given up — run `:reconnect`".
async fn reconnect_cycle<D, DFut>(
    state: &LinkState,
    make: &mut D,
    cmd_rx: &mut UnboundedReceiver<LinkCommand>,
    status: &watch::Sender<DaemonStatus>,
    policy: &ReconnectPolicy,
    immediate: bool,
) -> Served
where
    D: FnMut() -> DFut,
    DFut: Future<Output = anyhow::Result<DialedConnection>>,
{
    for attempt in 1..=policy.max_attempts {
        status.send_replace(DaemonStatus::Reconnecting {
            attempt,
            max: policy.max_attempts,
        });
        // Back off before each attempt (a manual reconnect skips the first wait), but let a
        // command interrupt the wait — a `:reconnect` to dial now, a `:disconnect` to stop.
        if !(immediate && attempt == 1) {
            match wait_or_cmd(policy.backoff(attempt), cmd_rx).await {
                Waited::Elapsed | Waited::Cmd(LinkCommand::Reconnect) => {}
                Waited::Cmd(LinkCommand::Disconnect) => return Served::Disconnect,
                Waited::Closed => return Served::Closed,
            }
        }
        match make().await {
            Ok(conn) => {
                // Fill the cells *before* announcing Connected: the editor's reconnect resync
                // (off the Connected transition) re-arms watches + re-opens LSP, and those ops
                // must land on this live connection, not the cleared cells. `run_connection`
                // re-publishes the same `Rpc`s (idempotent).
                publish_cells(state, &conn);
                status.send_replace(DaemonStatus::Connected);
                return serve_connection(state, conn, cmd_rx).await;
            }
            // A failed attempt is loud (the daemon log) and falls through to the next.
            Err(e) => eprintln!("bemtvi daemon link: re-dial attempt {attempt} failed: {e:#}"),
        }
    }
    // Budget exhausted: give up and wait for a manual `:reconnect`.
    Served::Disconnect
}

/// The reconnect supervisor: serve the first connection, then on each drop auto-retry with
/// bounded backoff ([`reconnect_cycle`]); on `:disconnect` or budget exhaustion, park as
/// [`DaemonStatus::Disconnected`] until a manual `:reconnect`. The same [`LinkState`] (and so
/// the same seam handles) is reused throughout, so the editor never rebuilds.
async fn maintain_link<D, DFut>(
    state: LinkState,
    mut make: D,
    first: DialedConnection,
    mut cmd_rx: UnboundedReceiver<LinkCommand>,
    status: watch::Sender<DaemonStatus>,
    policy: ReconnectPolicy,
) where
    D: FnMut() -> DFut,
    DFut: Future<Output = anyhow::Result<DialedConnection>>,
{
    let mut served = serve_connection(&state, first, &mut cmd_rx).await;
    loop {
        clear_cells(&state);
        match served {
            Served::Closed => return,
            // Auto-retry a lost connection.
            Served::Dropped => {
                served =
                    reconnect_cycle(&state, &mut make, &mut cmd_rx, &status, &policy, false).await;
            }
            // Given up / explicitly disconnected: park until a manual `:reconnect`.
            Served::Disconnect => {
                status.send_replace(DaemonStatus::Disconnected);
                served = loop {
                    match cmd_rx.recv().await {
                        None => return,
                        Some(LinkCommand::Disconnect) => continue, // already down
                        Some(LinkCommand::Reconnect) => {
                            break reconnect_cycle(
                                &state,
                                &mut make,
                                &mut cmd_rx,
                                &status,
                                &policy,
                                true,
                            )
                            .await
                        }
                    }
                };
            }
        }
    }
}

/// The URI scheme a QUIC daemon prints for clients to dial:
/// `bemtvi://HOST:PORT?cert=HASH` (the bearer token travels separately — see
/// [`DAEMON_TOKEN_ENV`]; the legacy `bemtvi://HOST:PORT/TOKEN?cert=HASH` path
/// form still dials, for the browser which has no shell env).
pub const CONNECT_URI_SCHEME: &str = "bemtvi://";

/// The env var carrying the daemon's bearer token to a QUIC-dialing client
/// (`BEMTVI_DAEMON_CMD`'s sibling). The daemon prints its connect command with
/// the token here rather than in the URI: the URI is copy-paste-able text that
/// lands in shell history, logs, docs, and reconnect configs, and it is the
/// daemon's sole auth credential (a leaked token is RCE on the daemon's host),
/// so it must not ride in the string itself. The browser leg is the one
/// exception — a webpage has no shell env, so its paste string keeps the legacy
/// `/TOKEN` path form.
pub const DAEMON_TOKEN_ENV: &str = "BEMTVI_DAEMON_TOKEN";

/// Parse a `bemtvi://HOST:PORT?cert=HASH` connect URI into the pieces a QUIC
/// dial needs: the `https://HOST:PORT` dial URL (WebTransport requires the
/// `https` scheme), the bearer token, and the TOFU cert `HASH` (the `cert`
/// query). The token comes from the legacy `/TOKEN` path when present, else from
/// [`DAEMON_TOKEN_ENV`] — a URI with neither fails loud rather than dialing
/// unauthenticated. Fails loud on any malformed URI rather than dialing a
/// half-specified target. Shared by every dialing client (the TUI binary and
/// the GUI).
pub fn parse_connect_uri(uri: &str) -> anyhow::Result<(String, String, String)> {
    use anyhow::anyhow;
    let rest = uri.strip_prefix(CONNECT_URI_SCHEME).ok_or_else(|| {
        anyhow!("daemon connect URI must start with {CONNECT_URI_SCHEME}: {uri:?}")
    })?;
    // `HOST:PORT[/TOKEN][?cert=HASH]` — the `/TOKEN` path is the legacy form
    // (the browser needs it: a webpage has no env to read); a tokenless URI
    // resolves its token from `DAEMON_TOKEN_ENV` below.
    let (authority_and_path, query) = rest
        .split_once('?')
        .ok_or_else(|| anyhow!("daemon connect URI is missing the ?cert=HASH query: {uri:?}"))?;
    let (authority, path_token) = authority_and_path
        .split_once('/')
        .map_or((authority_and_path, None), |(a, t)| (a, Some(t)));
    if authority.is_empty() {
        return Err(anyhow!("daemon connect URI is missing HOST:PORT: {uri:?}"));
    }
    let cert_hash = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("cert="))
        .filter(|h| !h.is_empty())
        .ok_or_else(|| anyhow!("daemon connect URI is missing cert=HASH: {uri:?}"))?;
    // An explicit path token wins over the ambient env var.
    let token = match path_token.filter(|t| !t.is_empty()) {
        Some(t) => t.to_owned(),
        None => std::env::var(DAEMON_TOKEN_ENV).map_err(|_| {
            anyhow!(
                "daemon connect URI has no /TOKEN and {DAEMON_TOKEN_ENV} is unset: {uri:?}"
            )
        })?,
    };
    Ok((
        format!("https://{authority}"),
        cert_hash.to_owned(),
        token,
    ))
}

/// Connect to a daemon over a **reconnectable** single-stream transport, driven on the
/// *current* tokio runtime (no dedicated link thread). `make` produces a fresh
/// reader/writer on each (re)dial — a duplex+daemon factory (tests), or a re-spawned ssh
/// child (Phase 4). On a drop the supervisor auto-retries per `policy`, then parks as
/// [`DaemonStatus::Disconnected`] until [`ReconnectHandle::reconnect`]. Returns the
/// [`DaemonClient`] the editor holds and a [`ReconnectHandle`] (status + `:reconnect` /
/// `:disconnect`). The initial dial is awaited, so a connect failure is a loud `Err` here,
/// before the editor is built.
///
/// The GUI/TUI wrap this on a dedicated link thread (later phase); a test drives it directly
/// on its own runtime.
pub async fn connect_daemon_reconnecting_on<F, Fut, R, W>(
    mut make: F,
    policy: ReconnectPolicy,
) -> anyhow::Result<(DaemonClient, ReconnectHandle)>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<(R, W)>> + Send + 'static,
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // Adapt the single-stream reader/writer factory into a `DialedConnection` dialer (split
    // the one stream into the four leg groups), then run the transport-agnostic supervisor.
    let dialer = move || {
        let fut = make();
        async move {
            let (reader, writer) = fut.await?;
            Ok(split_single_stream(reader, writer))
        }
    };
    connect_reconnecting_on(dialer, policy).await
}

/// The transport-agnostic core of the **current-runtime** reconnecting connect: dial the
/// first connection from `make` (a [`DialedConnection`] dialer — single-stream-split or QUIC),
/// build the stable seams, and spawn the supervisor. Shared by the single-stream
/// ([`connect_daemon_reconnecting_on`]) and QUIC ([`crate::quic`]) entry points; a test drives
/// it directly on its own runtime.
pub(crate) async fn connect_reconnecting_on<D, DFut>(
    mut make: D,
    policy: ReconnectPolicy,
) -> anyhow::Result<(DaemonClient, ReconnectHandle)>
where
    D: FnMut() -> DFut + Send + 'static,
    DFut: Future<Output = anyhow::Result<DialedConnection>> + Send + 'static,
{
    let first = make().await?;
    let (mut state, client) = build_link();
    // The `luafs_op` / `http_op` job servers ride the swappable Control cell, so they survive
    // reconnects.
    tokio::spawn(run_fs_jobs(
        state.control_rpc.clone(),
        state.take_fs_jobs_rx(),
    ));
    tokio::spawn(run_http_jobs(
        state.control_rpc.clone(),
        state.take_http_jobs_rx(),
    ));
    tokio::spawn(run_git_jobs(
        state.control_rpc.clone(),
        state.take_git_jobs_rx(),
    ));
    let (cmd_tx, cmd_rx) = unbounded_channel::<LinkCommand>();
    let (status_tx, status_rx) = watch::channel(DaemonStatus::Connected);
    // Publish the first connection's `Rpc`s into the cells *before* returning the client:
    // the caller issues the config-resolve round trip the instant this returns, and the
    // supervisor (`maintain_link`, spawned below) has not run its own `publish_cells` yet, so
    // the first seam op would otherwise race it and fail loud with "daemon disconnected".
    // Idempotent with `run_connection`'s re-publish.
    publish_cells(&state, &first);
    tokio::spawn(maintain_link(state, make, first, cmd_rx, status_tx, policy));
    Ok((
        client,
        ReconnectHandle {
            cmd_tx,
            status: status_rx,
        },
    ))
}

/// Connect to a daemon over a **reconnectable** single-stream transport on a **dedicated
/// link thread** (its own current-thread runtime) — the production twin of
/// [`connect_daemon`], with auto-reconnect. The wire runs off the server thread for the
/// same reason `connect_daemon`/`connect_quic` do (Open Decision #5: a synchronous seam
/// bridge parks the server thread on a `std` reply channel, so the wire must be driven
/// elsewhere or the park starves the reader carrying its own reply). `make` re-spawns the
/// transport on each (re)dial — for the GUI/TUI that re-runs `ssh … bemtvi --daemon` and
/// keeps the child alive itself.
///
/// **Blocking**: waits for the initial dial, so a bad host / refused connect is a loud
/// `Err` here (before the editor is built), exactly like `connect_daemon`. The returned
/// [`ReconnectHandle`] drops into [`ServerInit::daemon_link`](crate::ServerInit) so the run
/// loop reflects status + `:reconnect`/`:disconnect` drive the link; on a drop the
/// supervisor auto-retries per `policy`, then parks until a manual reconnect.
pub fn connect_daemon_reconnecting<F, Fut, R, W>(
    mut make: F,
    policy: ReconnectPolicy,
) -> anyhow::Result<(DaemonClient, ReconnectHandle)>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<(R, W)>> + Send + 'static,
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let dialer = move || {
        let fut = make();
        async move {
            let (reader, writer) = fut.await?;
            Ok(split_single_stream(reader, writer))
        }
    };
    connect_reconnecting_thread(dialer, policy)
}

/// The transport-agnostic core of the **dedicated-link-thread** reconnecting connect (the
/// production twin of [`connect_daemon`] / [`connect_quic`]): on its own current-thread
/// runtime, dial `make`'s first connection, hand the seams + handle back to the blocked
/// caller, then drive the supervisor until the editor drops its handle. The wire runs off the
/// server thread for the same reason `connect_daemon` does (Open Decision #5: a synchronous
/// seam bridge parks the server thread, so the wire must be driven elsewhere). `make` is the
/// [`DialedConnection`] dialer — the QUIC path builds its endpoint + opens four streams here,
/// the ssh path re-spawns its child and splits one stream. Blocking: the initial dial is
/// awaited, so a bad host / refused connect is a loud `Err` before the editor is built.
pub(crate) fn connect_reconnecting_thread<D, DFut>(
    make: D,
    policy: ReconnectPolicy,
) -> anyhow::Result<(DaemonClient, ReconnectHandle)>
where
    D: FnMut() -> DFut + Send + 'static,
    DFut: Future<Output = anyhow::Result<DialedConnection>> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<(DaemonClient, ReconnectHandle)>>();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send(Err(
                    anyhow::anyhow!(e).context("building the daemon link runtime")
                ));
                return;
            }
        };
        rt.block_on(async move {
            let mut make = make;
            // The initial dial (blocking the caller below); a failure surfaces as the
            // caller's `Err` rather than a half-built session.
            let first = match make().await {
                Ok(conn) => conn,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            };
            let (mut state, client) = build_link();
            tokio::spawn(run_fs_jobs(
                state.control_rpc.clone(),
                state.take_fs_jobs_rx(),
            ));
            tokio::spawn(run_http_jobs(
                state.control_rpc.clone(),
                state.take_http_jobs_rx(),
            ));
            tokio::spawn(run_git_jobs(
                state.control_rpc.clone(),
                state.take_git_jobs_rx(),
            ));
            let (cmd_tx, cmd_rx) = unbounded_channel::<LinkCommand>();
            let (status_tx, status_rx) = watch::channel(DaemonStatus::Connected);
            let handle = ReconnectHandle {
                cmd_tx,
                status: status_rx,
            };
            // Publish the first connection's `Rpc`s into the swappable cells *before* handing
            // the client back: the caller (a different thread) unblocks on `tx.send` and
            // immediately issues the config-resolve round trip, which must land on a live cell.
            // The cells are otherwise only filled later, inside the supervisor's `run_connection`
            // — spawned as `maintain_link` below and not yet polled at this point — so without
            // this the very first seam op races the supervisor and fails loud with "daemon
            // disconnected" (the intermittent `could not resolve the session config from the
            // daemon`). Idempotent: `run_connection` re-publishes the same `Rpc`s.
            publish_cells(&state, &first);
            // Hand the seams + handle back to the (blocked) caller; if it already gave up,
            // there's nothing to drive.
            if tx.send(Ok((client, handle))).is_err() {
                return;
            }
            // Drive the supervisor on this thread until the editor drops its handle (session
            // swap / quit closes the command channel) — then return so the runtime drops,
            // aborting the per-connection wire tasks and reaping the transport (the ssh child
            // / QUIC connection the dialer holds). This is why the wrapper awaits rather than
            // spawning.
            maintain_link(state, make, first, cmd_rx, status_tx, policy).await;
        });
    });
    rx.recv().map_err(|_| {
        anyhow::anyhow!("connect_daemon_reconnecting: the link thread died before dialing")
    })?
}

/// A [`HostProc`] that runs children on a remote daemon instead of locally: each
/// [`run`](HostProc::run) forwards the spawn over the wire and relays the daemon's
/// pid/exit back to the editor's [`ProcEvents`], so the event-loop actor that drives
/// it never knows the process ran across a network. The drop-in for
/// [`StdHostProc`](crate::host::StdHostProc) on the edit-host side of the split.
///
/// `Send + Sync` (it holds only the cloneable [`Rpc`] handle, a shared map, and an
/// id counter) so it rides [`ServerInit`](crate::ServerInit) onto the server thread
/// and is shared across spawns by the actor, exactly as the local host is.
pub struct RemoteHostProc {
    rpc: LinkRpc,
    inflight: Inflight,
    /// Per-spawn correlation id minted here (not the editor's callback id, which
    /// never needs to cross the wire — the demux routes purely by this).
    next_id: AtomicU64,
}

impl RemoteHostProc {
    /// Connect to a daemon over `reader`/`writer` (a duplex, or ssh stdio). Spawns
    /// the demux task that fans the daemon's replies out to in-flight spawns; its
    /// RPC reader/writer tasks live on the runtime this is called from (the same
    /// arrangement [`bemtvi_rpc::connect`] makes for any client), so call it from
    /// within a tokio runtime.
    pub fn connect<R, W>(reader: R, writer: W) -> RemoteHostProc
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (rpc, incoming) = connect_bounded(reader, writer);
        let inflight: Inflight = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(run_demux(incoming, inflight.clone()));
        RemoteHostProc {
            rpc: LinkRpc::fixed(rpc),
            inflight,
            next_id: AtomicU64::new(1),
        }
    }
}

impl HostProc for RemoteHostProc {
    fn run(
        &self,
        spec: ProcSpec,
        mut kill: oneshot::Receiver<()>,
        events: ProcEvents,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let rpc = self.rpc.clone();
        let inflight = self.inflight.clone();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            // While disconnected, fail LOUD and fast: `notify` below would be
            // dropped on the floor, and nothing would ever resolve this spawn (the
            // inflight entry isn't tied to any connection, so no teardown clears
            // it) — the job's `on_exit` would hang forever. This is the seam
            // contract ("while disconnected … fails loud, never hangs") applied to
            // the one notify-based seam that otherwise couldn't honor it. A drop
            // racing in right after this check is the cancelled-serve case, which
            // `clear_cells` fails over by dropping the pending senders.
            if rpc.current().is_none() {
                events.spawned(None);
                events.exited(-1, Vec::new(), b"vim.system: daemon disconnected".to_vec());
                return;
            }
            // Register *before* the spawn request so the daemon's reply can never
            // race ahead of a receiver to land in.
            let (tx, mut rx) = unbounded_channel::<DaemonEvent>();
            inflight.lock().unwrap().insert(id, tx);
            rpc.notify(PROC_SPAWN, encode_spawn(id, spec));

            // Hold `events` in an Option so the `&self` `spawned` calls and the
            // self-consuming `exited` call coexist (and `exited` fires exactly once).
            let mut events = Some(events);
            let mut killed = false;
            loop {
                tokio::select! {
                    // Once kill has fired, disable this arm: re-polling a consumed
                    // oneshot returns instantly and would busy-loop. The child still
                    // exits via the daemon's `proc_exited` (code -1), keeping the
                    // exactly-one-exit contract.
                    _ = &mut kill, if !killed => {
                        killed = true;
                        rpc.notify(PROC_KILL, vec![Value::from(id)]);
                    }
                    ev = rx.recv() => match ev {
                        Some(DaemonEvent::Spawned(pid)) => {
                            if let Some(e) = &events {
                                e.spawned(pid);
                            }
                        }
                        Some(DaemonEvent::Stdout(lines)) => {
                            if let Some(e) = &events {
                                e.stdout(lines);
                            }
                        }
                        Some(DaemonEvent::Exited { code, stdout, stderr }) => {
                            if let Some(e) = events.take() {
                                e.exited(code, stdout, stderr);
                            }
                            break;
                        }
                        // The demux dropped our sender: the daemon connection died.
                        // Synthesize an exit so the editor's one-shot `on_exit`
                        // always fires and is never leaked.
                        None => {
                            if let Some(e) = events.take() {
                                e.exited(-1, Vec::new(), b"daemon connection closed".to_vec());
                            }
                            break;
                        }
                    }
                }
            }
            inflight.lock().unwrap().remove(&id);
        })
    }
}

/// Pump the daemon's replies off the wire and forward each to the spawn it belongs
/// to. On connection teardown (`incoming` ends) it clears [`Inflight`], dropping
/// every pending sender so each waiting [`RemoteHostProc::run`] future observes the
/// EOF and reports a `-1` exit rather than hanging on a child that will never report.
async fn run_demux(mut incoming: Receiver<Incoming>, inflight: Inflight) {
    while let Some(msg) = incoming.recv().await {
        let Incoming::Notification { method, params } = msg else {
            continue; // the daemon speaks only notifications; ignore stray requests
        };
        match method.as_str() {
            PROC_SPAWNED => {
                if let Some((id, ev)) = decode_spawned(&params) {
                    forward(&inflight, id, ev);
                }
            }
            PROC_STDOUT => {
                if let Some((id, ev)) = decode_stdout(params) {
                    forward(&inflight, id, ev);
                }
            }
            PROC_EXITED => {
                if let Some((id, ev)) = decode_exited(params) {
                    forward(&inflight, id, ev);
                }
            }
            _ => {}
        }
    }
    inflight.lock().unwrap().clear();
}

/// Deliver `ev` to the spawn registered under `id` (a no-op if it already exited and
/// removed itself — a late or duplicate report is harmless).
fn forward(inflight: &Inflight, id: u64, ev: DaemonEvent) {
    if let Some(tx) = inflight.lock().unwrap().get(&id) {
        let _ = tx.send(ev);
    }
}

/// Backpressure window for the remote terminal's inbound `term_data`/`term_exit` →
/// [`TermEvent`] channel — the edit-host twin of the daemon-side bound (and of the
/// local [`TerminalManager`](crate::terminal::native::TerminalManager)'s `TERM_EVENT_CAP`).
/// When the run loop's `on_term_events` arm falls behind, the demux's `send().await`
/// blocks, the demux stops draining the wire, and the daemon's backpressured `term_data`
/// stream throttles the child — so a flood never queues without bound.
const REMOTE_TERM_EVENT_CAP: usize = 4;

/// The edit-host-side **terminal** seam: forwards `:terminal` ops to the daemon's PTY host
/// over the Term leg and surfaces the child's output/exit back as [`TermEvent`]s — the
/// native twin of the wasm `HostEffects::term_*` path. The daemon runs the real PTY
/// ([`serve_term_daemon_on`]); this seam only ships `term_open`/`term_write`/`term_resize`/
/// `term_kill` notifications and decodes the `term_data`/`term_exit` pushes into the *same*
/// `TermEvent` channel the local [`TerminalManager`](crate::terminal::native::TerminalManager)
/// feeds, so the run loop's [`on_term_events`](crate::EditHost::on_term_events) arm consumes a
/// remote terminal identically to a local one. Without this, a daemon session would open a PTY
/// on the *local* machine (the bug `docs/plans/2026-06-28-native-remote-terminal.md` fixes).
pub struct RemoteHostTerm {
    /// The Term-leg `Rpc` outbound ops are sent on (its own QUIC stream, or the shared
    /// single-stream `Rpc` on ssh/stdio).
    rpc: LinkRpc,
    /// The decoded inbound events, handed to the run loop once via [`take_events`]. `None`
    /// thereafter (a second take would strand the producer).
    ///
    /// [`take_events`]: RemoteHostTerm::take_events
    events: Option<Receiver<crate::terminal::native::TermEvent>>,
}

impl RemoteHostTerm {
    /// Connect to a daemon over `reader`/`writer` (a standalone duplex / ssh stdio) and
    /// spawn the demux that decodes its `term_data`/`term_exit` pushes. Mirrors
    /// [`RemoteHostProc::connect`]; the multiplexer path builds the seam over a shared
    /// group link via [`with_link`](Self::with_link) instead.
    pub fn connect<R, W>(reader: R, writer: W) -> RemoteHostTerm
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (rpc, incoming) = connect_bounded(reader, writer);
        Self::with_link(rpc, incoming)
    }

    /// Build the seam over an already-connected Term group link (the
    /// [`serve_daemon_link_inner`] / QUIC multiplexer path): `rpc` sends ops, `incoming`
    /// is the Term group's demuxed inbound stream the term demux drains.
    pub(crate) fn with_link(rpc: Rpc, incoming: Receiver<Incoming>) -> RemoteHostTerm {
        let (event_tx, event_rx) =
            channel::<crate::terminal::native::TermEvent>(REMOTE_TERM_EVENT_CAP);
        tokio::spawn(run_term_demux(incoming, event_tx));
        RemoteHostTerm::from_parts(LinkRpc::fixed(rpc), event_rx)
    }

    /// Build the seam from an already-swappable `rpc` cell and the (stable) inbound
    /// `TermEvent` receiver. The reconnecting path ([`build_link`]) creates the channel and
    /// runs the term demux itself (per connection), so this only stores the fields.
    fn from_parts(
        rpc: LinkRpc,
        events: Receiver<crate::terminal::native::TermEvent>,
    ) -> RemoteHostTerm {
        RemoteHostTerm {
            rpc,
            events: Some(events),
        }
    }

    /// Take the inbound `TermEvent` receiver (once) — the run loop selects on it in place
    /// of the local terminal actor's. Returns `None` if already taken.
    pub(crate) fn take_events(&mut self) -> Option<Receiver<crate::terminal::native::TermEvent>> {
        self.events.take()
    }

    /// Ship a `term_open` for `buf`: run `argv` (empty ⇒ the daemon's default shell) in
    /// `cwd` (the daemon-side absolute dir), sized `rows`×`cols`.
    pub(crate) fn open(
        &self,
        buf: u64,
        argv: Vec<String>,
        cwd: Option<String>,
        rows: u16,
        cols: u16,
    ) {
        self.rpc.notify(
            TERM_OPEN,
            vec![
                Value::from(buf),
                Value::Array(argv.into_iter().map(Value::from).collect()),
                cwd.map(Value::from).unwrap_or(Value::Nil),
                Value::from(rows),
                Value::from(cols),
            ],
        );
    }

    /// Ship input bytes to `buf`'s daemon PTY (a forwarded keystroke / paste).
    pub(crate) fn write(&self, buf: u64, bytes: Vec<u8>) {
        self.rpc
            .notify(TERM_WRITE, vec![Value::from(buf), Value::Binary(bytes)]);
    }

    /// Ship a resize so the daemon child re-lays-out.
    pub(crate) fn resize(&self, buf: u64, rows: u16, cols: u16) {
        self.rpc.notify(
            TERM_RESIZE,
            vec![Value::from(buf), Value::from(rows), Value::from(cols)],
        );
    }

    /// Ship a kill: terminate `buf`'s daemon child and forget the session.
    pub(crate) fn kill(&self, buf: u64) {
        self.rpc.notify(TERM_KILL, vec![Value::from(buf)]);
    }
}

/// Pump the daemon's `term_data`/`term_exit` pushes off the Term wire and decode each into a
/// [`TermEvent`](crate::terminal::native::TermEvent) on the bounded channel the run loop drains
/// — the symmetric twin of the daemon-side forwarder in [`serve_term_daemon_on`]. Ends when the
/// wire EOFs (the channel sender drops, so the run loop's terminal arm sees no more events) or
/// the run loop drops its receiver (no consumer left).
async fn run_term_demux(
    mut incoming: Receiver<Incoming>,
    event_tx: Sender<crate::terminal::native::TermEvent>,
) {
    use crate::terminal::native::TermEvent;
    use bemtvi_core::BufferId;

    while let Some(msg) = incoming.recv().await {
        let Incoming::Notification { method, params } = msg else {
            continue; // the daemon speaks only notifications; ignore stray requests
        };
        let ev = match method.as_str() {
            TERM_DATA => {
                let buf = params.first().and_then(Value::as_u64);
                let bytes = params.get(1).and_then(|v| match v {
                    Value::Binary(b) => Some(b.clone()),
                    Value::String(s) => Some(s.as_bytes().to_vec()),
                    _ => None,
                });
                match (buf, bytes) {
                    (Some(buf), Some(bytes)) => TermEvent::Data {
                        buf: BufferId(buf),
                        bytes,
                    },
                    _ => continue,
                }
            }
            TERM_EXIT => match params.first().and_then(Value::as_u64) {
                Some(buf) => TermEvent::Exit {
                    buf: BufferId(buf),
                    code: params
                        .get(1)
                        .and_then(Value::as_i64)
                        .map(|c| c as i32)
                        .unwrap_or(-1),
                },
                None => continue,
            },
            _ => continue,
        };
        // The run loop dropped its receiver (shutdown) — nothing left to feed.
        if event_tx.send(ev).await.is_err() {
            break;
        }
    }
}

/// Run the daemon end of the wire over `reader`/`writer`: spawn the children a far
/// edit-host asks for and stream their pid/exit back. Returns when the connection
/// closes (the edit-host hung up). Each child runs through [`StdHostProc`] — the
/// exact local-spawn machinery — and its [`LoopEvent`]s are relayed onto the wire, so
/// `vim.system` / `jobstart` / `:!` behave identically remote and local.
pub async fn serve_daemon<R, W>(reader: R, writer: W) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (rpc, incoming) = connect_bounded(reader, writer);
    serve_proc_daemon_on(rpc, incoming).await
}

/// The process leg's connection-agnostic core: drives the `proc_*` wire over a
/// pre-built shared [`Rpc`] + its own demuxed inbound stream. The single-stdio
/// Any inbound `Incoming` stream a daemon leg core can drain: the bounded
/// `mpsc::Receiver` the daemon's per-leg queues feed, or the unbounded receiver
/// a direct caller (the per-leg tests, a standalone leg over its own duplex)
/// hands over. Generic so one core serves both — the daemon keeps its queues
/// bounded while the tests keep passing their unbounded receivers.
pub trait IncomingStream {
    fn recv(&mut self) -> impl std::future::Future<Output = Option<Incoming>> + Send;
}

impl IncomingStream for Receiver<Incoming> {
    fn recv(&mut self) -> impl std::future::Future<Output = Option<Incoming>> + Send {
        Receiver::recv(self)
    }
}

impl IncomingStream for UnboundedReceiver<Incoming> {
    fn recv(&mut self) -> impl std::future::Future<Output = Option<Incoming>> + Send {
        UnboundedReceiver::recv(self)
    }
}

/// daemon multiplexer ([`run_daemon_io`]) fans one connection across every leg's
/// `*_on`; [`serve_daemon`] is the standalone wrapper (its own connection) the
/// per-leg tests drive.
pub async fn serve_proc_daemon_on(
    rpc: Rpc,
    mut incoming: Receiver<Incoming>,
) -> anyhow::Result<()> {
    // One forwarder turns the children's `LoopEvent`s — the same events the local
    // event-loop actor consumes — into wire notifications back to the edit-host.
    let (ev_tx, mut ev_rx) = unbounded_channel::<LoopEvent>();
    let reply = rpc.clone();
    tokio::spawn(async move {
        while let Some(ev) = ev_rx.recv().await {
            match ev {
                LoopEvent::ProcessSpawned { id, pid } => reply.notify(
                    PROC_SPAWNED,
                    vec![
                        Value::from(id),
                        pid.map_or(Value::Nil, |p| Value::from(p as u64)),
                    ],
                ),
                LoopEvent::ProcessStdout { id, lines } => reply.notify(
                    PROC_STDOUT,
                    vec![
                        Value::from(id),
                        Value::Array(lines.into_iter().map(Value::from).collect()),
                    ],
                ),
                LoopEvent::ProcessExit {
                    id,
                    code,
                    stdout,
                    stderr,
                } => reply.notify(
                    PROC_EXITED,
                    vec![
                        Value::from(id),
                        Value::from(code as i64),
                        Value::Binary(stdout),
                        Value::Binary(stderr),
                    ],
                ),
                // The daemon only spawns processes — it arms no timers, no filesystem
                // watches, no `btv.fs` ops (the luafs leg has its own handler), and no
                // `btv.http.mount` listener (mounts are always local: the Lua VM that answers
                // them lives in the edit-host) — so no other variant can reach here.
                LoopEvent::Timer { .. }
                | LoopEvent::FsEvent { .. }
                | LoopEvent::FsResult { .. }
                | LoopEvent::GitResult { .. }
                | LoopEvent::HttpResult { .. }
                | LoopEvent::HttpMountResult { .. }
                | LoopEvent::HttpServerRequest { .. }
                | LoopEvent::HttpRebound { .. }
                | LoopEvent::HttpRebindErr { .. }
                | LoopEvent::ProcOut { .. }
                | LoopEvent::ProcExit { .. }
                | LoopEvent::SockConnected { .. }
                | LoopEvent::SockData { .. }
                | LoopEvent::SockClosed { .. } => {}
            }
        }
    });

    let host = StdHostProc;
    // Per-child kill channels, keyed by the edit-host's spawn id, so a `proc_kill`
    // can reach the running child (mirrors the event-loop actor's `procs` map).
    let mut kills: HashMap<u64, oneshot::Sender<()>> = HashMap::new();
    while let Some(msg) = incoming.recv().await {
        let Incoming::Notification { method, params } = msg else {
            continue; // the edit-host drives the daemon with notifications only
        };
        match method.as_str() {
            PROC_SPAWN => {
                if let Some((id, spec)) = decode_spawn(params) {
                    let (kill_tx, kill_rx) = oneshot::channel();
                    kills.insert(id, kill_tx);
                    let events = ProcEvents::new(id, ev_tx.clone());
                    tokio::spawn(host.run(spec, kill_rx, events));
                }
            }
            PROC_KILL => {
                if let Some(id) = params.first().and_then(Value::as_u64) {
                    if let Some(kill_tx) = kills.remove(&id) {
                        let _ = kill_tx.send(());
                    }
                }
            }
            _ => {}
        }
        // Forget kill channels whose child tasks have closed them (the child exited),
        // the same leak guard the event-loop actor applies to its `procs` map.
        kills.retain(|_, tx| !tx.is_closed());
    }
    Ok(())
}

/// The terminal leg's connection-agnostic core: drives the `term_*` wire over a
/// pre-built shared [`Rpc`] + its own demuxed inbound stream — the streaming sibling of
/// [`serve_proc_daemon_on`]. The single-stdio daemon multiplexer ([`run_daemon_io`]) fans
/// one connection across every leg's `*_on`; this leg owns a native
/// [`TerminalManager`](crate::terminal::native::TerminalManager) (the same PTY engine a
/// local `:terminal` uses) and bridges it to the wire: incoming `term_open`/`term_write`/
/// `term_resize`/`term_kill` notifications drive the manager, and the children's
/// [`TermEvent`](crate::terminal::native::TermEvent) output/exit stream back as
/// `term_data`/`term_exit` pushes the browser feeds to its own vt100 emulator. The buffer
/// id (`BufferId(u64)`) is the per-terminal key, carried verbatim on the wire.
pub async fn serve_term_daemon_on<R: IncomingStream>(
    rpc: Rpc,
    mut incoming: R,
) -> anyhow::Result<()> {
    use crate::terminal::native::{TermCommand, TermEvent, TerminalManager};
    use bemtvi_core::BufferId;

    let (mut terminals, mut term_events) = TerminalManager::new();

    // One forwarder turns the children's `TermEvent`s — the same events the local run
    // loop's `on_term_events` arm consumes — into wire notifications back to the browser.
    let reply = rpc.clone();
    tokio::spawn(async move {
        while let Some(ev) = term_events.recv().await {
            match ev {
                // Data goes over the *backpressured* stream channel: when the wire is
                // behind (browser slow / QUIC congested), this `await` blocks, so we
                // stop draining `term_events`, the bounded event channel fills, the PTY
                // reader blocks, and the child is throttled — no unbounded backlog, so a
                // `^C` actually stops the output. Exit stays on the control channel so it
                // is delivered promptly even behind a backed-up data stream.
                TermEvent::Data { buf, bytes } => {
                    reply
                        .notify_stream(TERM_DATA, vec![Value::from(buf.0), Value::Binary(bytes)])
                        .await
                }
                TermEvent::Exit { buf, code } => reply.notify(
                    TERM_EXIT,
                    vec![Value::from(buf.0), Value::from(code as i64)],
                ),
            }
        }
    });

    while let Some(msg) = incoming.recv().await {
        let Incoming::Notification { method, params } = msg else {
            continue; // the edit-host drives the daemon with notifications only
        };
        match method.as_str() {
            TERM_OPEN => {
                if let Some(cmd) = decode_term_open(params) {
                    terminals.send(cmd);
                }
            }
            TERM_WRITE => {
                let buf = params.first().and_then(Value::as_u64);
                let bytes = params.get(1).and_then(|v| match v {
                    Value::Binary(b) => Some(b.clone()),
                    Value::String(s) => Some(s.as_bytes().to_vec()),
                    _ => None,
                });
                if let (Some(buf), Some(bytes)) = (buf, bytes) {
                    terminals.send(TermCommand::Write {
                        buf: BufferId(buf),
                        bytes,
                    });
                }
            }
            TERM_RESIZE => {
                let buf = params.first().and_then(Value::as_u64);
                let rows = params.get(1).and_then(Value::as_u64);
                let cols = params.get(2).and_then(Value::as_u64);
                if let (Some(buf), Some(rows), Some(cols)) = (buf, rows, cols) {
                    terminals.send(TermCommand::Resize {
                        buf: BufferId(buf),
                        rows: rows as u16,
                        cols: cols as u16,
                    });
                }
            }
            TERM_KILL => {
                if let Some(buf) = params.first().and_then(Value::as_u64) {
                    terminals.send(TermCommand::Kill { buf: BufferId(buf) });
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// `term_open` params → a [`TermCommand::Open`]: `[buf(u64), argv([str]), cwd(str|nil),
/// rows(u64), cols(u64)]`. Returns `None` (the open is dropped) on a malformed frame —
/// the peer is the same build, so this only guards against a truncated message.
fn decode_term_open(params: Vec<Value>) -> Option<crate::terminal::native::TermCommand> {
    use crate::terminal::native::TermCommand;
    use bemtvi_core::BufferId;

    let buf = params.first().and_then(Value::as_u64)?;
    let argv = match params.get(1) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    let cwd = params.get(2).and_then(Value::as_str).map(str::to_string);
    let rows = params.get(3).and_then(Value::as_u64)? as u16;
    let cols = params.get(4).and_then(Value::as_u64)? as u16;
    Some(TermCommand::Open {
        buf: BufferId(buf),
        argv,
        cwd,
        rows,
        cols,
    })
}

/// `ProcSpec` → `proc_spawn` params. Consumes the spec so `stdin` (potentially
/// large) moves onto the wire rather than copying.
fn encode_spawn(id: u64, spec: ProcSpec) -> Vec<Value> {
    let ProcSpec {
        argv,
        cwd,
        env,
        stdin,
        stream,
    } = spec;
    vec![
        Value::from(id),
        Value::Array(argv.into_iter().map(Value::from).collect()),
        cwd.map_or(Value::Nil, Value::from),
        Value::Array(
            env.into_iter()
                .map(|(k, v)| Value::Array(vec![Value::from(k), Value::from(v)]))
                .collect(),
        ),
        Value::Binary(stdin),
        Value::from(stream),
    ]
}

/// `proc_spawn` params → `(id, ProcSpec)`, or `None` on a malformed frame (which the
/// daemon simply drops — a peer is the same build, so this only guards against
/// corruption). Moves `stdin` / `argv` out rather than cloning.
fn decode_spawn(mut params: Vec<Value>) -> Option<(u64, ProcSpec)> {
    if params.len() < 5 {
        return None;
    }
    let (id, argv, cwd, env) = decode_proc_head(&mut params)?;
    let stdin = match std::mem::replace(&mut params[4], Value::Nil) {
        Value::Binary(b) => b,
        _ => Vec::new(),
    };
    // `stream` (6th param) may be absent from an older peer's frame — default
    // false (the one-shot `vim.system` shape).
    let stream = params.get(5).and_then(Value::as_bool).unwrap_or(false);
    Some((
        id,
        ProcSpec {
            argv,
            cwd,
            env,
            stdin,
            stream,
        },
    ))
}

/// The `(id, argv, cwd?, env)` head the two process-open wires share
/// (`proc_spawn` params 0–3, the whole of `dproc_open`).
type ProcHead = (u64, Vec<String>, Option<String>, Vec<(String, String)>);

/// Decode the shared [`ProcHead`]. The caller has already checked its own
/// minimum length; `None` on a malformed id/argv.
fn decode_proc_head(params: &mut [Value]) -> Option<ProcHead> {
    let id = params[0].as_u64()?;
    let argv = match std::mem::replace(&mut params[1], Value::Nil) {
        Value::Array(a) => a
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => return None,
    };
    let cwd = params[2].as_str().map(str::to_string);
    let env = match std::mem::replace(&mut params[3], Value::Nil) {
        Value::Array(pairs) => pairs
            .into_iter()
            .filter_map(|pair| match pair {
                Value::Array(kv) => {
                    let k = kv.first()?.as_str()?.to_string();
                    let v = kv.get(1)?.as_str()?.to_string();
                    Some((k, v))
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    Some((id, argv, cwd, env))
}

/// `proc_spawned` params → `(id, Spawned)`. A nil/absent pid means the spawn failed.
fn decode_spawned(params: &[Value]) -> Option<(u64, DaemonEvent)> {
    let id = params.first()?.as_u64()?;
    let pid = params.get(1).and_then(Value::as_u64).map(|p| p as u32);
    Some((id, DaemonEvent::Spawned(pid)))
}

/// `proc_stdout` params → `(id, Stdout)` — a streaming child's batch of stdout lines.
fn decode_stdout(mut params: Vec<Value>) -> Option<(u64, DaemonEvent)> {
    let id = params.first()?.as_u64()?;
    let lines = match params.get_mut(1).map(|v| std::mem::replace(v, Value::Nil)) {
        Some(Value::Array(a)) => a
            .into_iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect(),
        _ => Vec::new(),
    };
    Some((id, DaemonEvent::Stdout(lines)))
}

/// `proc_exited` params → `(id, Exited)`. Moves the captured output out of `params`.
fn decode_exited(mut params: Vec<Value>) -> Option<(u64, DaemonEvent)> {
    if params.len() < 4 {
        return None;
    }
    let id = params[0].as_u64()?;
    let code = params[1].as_i64().unwrap_or(-1) as i32;
    let stdout = match std::mem::replace(&mut params[2], Value::Nil) {
        Value::Binary(b) => b,
        _ => Vec::new(),
    };
    let stderr = match std::mem::replace(&mut params[3], Value::Nil) {
        Value::Binary(b) => b,
        _ => Vec::new(),
    };
    Some((
        id,
        DaemonEvent::Exited {
            code,
            stdout,
            stderr,
        },
    ))
}

// ===== the filesystem leg =====================================================

/// What a daemon `fs_read` resolves a path to — a file's bytes, a new-file marker, or a
/// directory listing. A genuine read error (a permission failure, a dead connection) is
/// *not* one of these — it surfaces as an `Err` the server echoes loudly, never a silent
/// empty buffer.
pub enum FsRead {
    /// An existing file's bytes plus its stat at read time — load the bytes into the buffer
    /// (a replica of the remote) and stamp the [`FileStat`] as the `disk` baseline, so the
    /// buffer counts as read-from-disk (fires `BufReadPost`, not `BufNewFile`) and the watch
    /// leg's later `fs_changed` pushes compare against an accurate snapshot. `None` if the
    /// daemon couldn't stat the (still readable) file — a rare degrade to a size-only baseline.
    File(Vec<u8>, Option<FileStat>),
    /// The path doesn't exist yet — open an empty new-file buffer named for it (the
    /// `:e newfile` case), so a first `:w` would create it.
    New,
    /// The path is a **directory** — open it as the in-window file explorer. `path` is
    /// the daemon's *canonical* directory path (so `../`/descend navigation is unambiguous
    /// on the edit-host side); `entries` are its immediate, unsorted entries (the edit-host
    /// renders the listing via [`bemtvi_core::dir_listing`] for the explorer plugin).
    Dir {
        path: String,
        entries: Vec<DirEntry>,
    },
}

/// The **async** filesystem seam the server fetches buffer contents through, off the
/// editor tick — the daemon/remote analog of core's *synchronous*
/// [`HostFs`](bemtvi_core::HostFs). Where the sync trait reads local disk at the open
/// call (and must, since it runs on the single editor thread), this returns a future
/// the server awaits *off-tick* and then hands core populated bytes, so a slow remote
/// read never freezes typing. [`RemoteHostFs`] is the over-the-wire implementation;
/// a test can supply a fake.
///
/// Object-safe (returns a boxed `Send` future, no `async fn`) to match the
/// `Box<dyn …>` DI style the rest of the server uses without an `async-trait`
/// dependency. `read` resolves the path to a file, a new-file marker, or a directory
/// listing (the [`FsRead`] variants — so it covers buffer opens *and* the remote
/// explorer); `write` is the save path.
pub trait HostFsAsync: Send + Sync {
    /// Fetch `path` for a buffer open: its bytes (a file), a new-file marker (absent), or
    /// the directory listing (the remote explorer) — whichever the path resolves to.
    fn read(&self, path: String) -> Pin<Box<dyn Future<Output = io::Result<FsRead>> + Send>>;

    /// Resolve + validate a `:cd` target on the daemon, resolving to its canonical absolute
    /// path (or a loud `E344` error if it isn't a directory) — the off-tick half of
    /// remote `:cd` (`docs/plans/2026-06-23-remote-cwd.md`). Pure: the daemon does not chdir
    /// its process (it serves many sessions), so the edit-host installs the returned path
    /// into its own [`DirState`](crate). The default fails loud — a backend with no remote
    /// `:cd` support must say so at runtime, not silently succeed (the no-silent-stub rule).
    fn chdir(&self, _path: String) -> Pin<Box<dyn Future<Output = io::Result<String>> + Send>> {
        Box::pin(async {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "remote :cd is not supported by this filesystem backend",
            ))
        })
    }

    /// Atomically write `bytes` to `path` (the off-tick `:w`). Resolves to the file's
    /// new [`FileStat`] on success — which the editor stamps as its `disk` baseline so
    /// a later change check doesn't false-positive on our own write — or a loud error
    /// (a failed write is never silently dropped; the contract is that the editor's
    /// saved-state clears *only* on this ack). `None` stat means the write succeeded
    /// but the daemon could not stat the result.
    fn write(
        &self,
        path: String,
        bytes: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = io::Result<Option<FileStat>>> + Send>>;

    /// Recursively create a directory on the remote (the remote-shada mirror ensures its
    /// per-namespace dir before writing). The default fails loud — a backend with no
    /// remote mkdir must say so at runtime, not silently succeed (the no-silent-stub rule).
    fn mkdir(&self, _path: String) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send>> {
        Box::pin(async {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "remote mkdir is not supported by this filesystem backend",
            ))
        })
    }

    /// Remove a file on the remote (the remote-shada mirror's clean-exit compaction
    /// deletes absorbed sibling stores). The default fails loud (see [`Self::mkdir`]).
    fn remove(&self, _path: String) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send>> {
        Box::pin(async {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "remote remove is not supported by this filesystem backend",
            ))
        })
    }

    /// Arm a remote watch on `path` (the `HostWatch` leg): the daemon stats it now as
    /// the baseline and pushes a [`WatchEvent`] each time it changes thereafter.
    /// Fire-and-forget — the change comes back asynchronously via [`Self::take_watch_events`].
    /// `known` is the edit-host's disk baseline for the path: the daemon compares it to the
    /// live stat at arm time and pushes a change immediately if they differ, so a file that
    /// changed while the link was down (a re-dialed daemon lost its old baselines) is caught
    /// on re-arm rather than silently re-baselined. `None` skips that one-shot compare.
    /// The default is a no-op (an impl with no remote, e.g. a local fake that never pushes).
    fn watch(&self, _path: String, _known: Option<FileStat>) {}

    /// Disarm the remote watch on `path` (the buffer closed / lost its file). The
    /// default is a no-op, matching [`Self::watch`].
    fn unwatch(&self, _path: String) {}

    /// Take the receiver of server-pushed [`WatchEvent`]s — the edit-host side of the
    /// `HostWatch` leg. Returns `Some` exactly once (the first call) for an impl that
    /// pushes ([`RemoteHostFs`]); `None` for one that never watches. The server wires
    /// the receiver as a `select!` arm and reconciles each push off the editor tick.
    fn take_watch_events(&self) -> Option<UnboundedReceiver<WatchEvent>> {
        None
    }
}

/// A server-pushed file change from the daemon's watch leg (the `fs_changed`
/// notification): the watched `path` and its new [`FileStat`] (`None` = the file
/// vanished on the daemon). The edit-host turns it into a `FileChangedShell` reconcile
/// off the editor tick — the remote analogue of the local per-buffer file watch.
pub struct WatchEvent {
    /// The watched path that changed (as the edit-host armed it — the buffer's name).
    pub path: String,
    /// The file's new stat, or `None` if it vanished (drives the `"deleted"` reason).
    pub stat: Option<FileStat>,
}

/// A [`HostFsAsync`] that reads files from a remote daemon over the wire. `read`
/// issues an `fs_read` request and awaits the reply — a file read is naturally
/// request/response, so (unlike [`RemoteHostProc`]) there is no per-call demux:
/// [`bemtvi_rpc`] routes each response to its awaiting `request` by msgid.
pub struct RemoteHostFs {
    rpc: LinkRpc,
    /// The receiver of `fs_changed` pushes, handed to the server once via
    /// [`HostFsAsync::take_watch_events`]. Behind a `Mutex<Option<…>>` because the
    /// trait method is `&self` and the receiver can only be taken out once.
    watch_rx: Mutex<Option<UnboundedReceiver<WatchEvent>>>,
}

impl RemoteHostFs {
    /// Connect to a daemon over `reader`/`writer`. The daemon sends `fs_read` /
    /// `fs_write` *responses* (which `bemtvi_rpc` routes internally) and `fs_changed`
    /// *notifications* (the watch leg); a drain task consumes the `Incoming` stream —
    /// dropping the receiver would tear the connection down — and forwards each
    /// `fs_changed` to the watch channel the server drains. RPC tasks live on the
    /// runtime this is called from, as for any [`bemtvi_rpc::connect`].
    pub fn connect<R, W>(reader: R, writer: W) -> RemoteHostFs
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (rpc, mut incoming) = connect_bounded(reader, writer);
        let (watch_tx, watch_rx) = unbounded_channel::<WatchEvent>();
        tokio::spawn(async move {
            while let Some(msg) = incoming.recv().await {
                if let Incoming::Notification { method, params } = msg {
                    if method == FS_CHANGED {
                        if let Some(ev) = decode_fs_changed(params) {
                            // The server may not have taken the receiver yet at startup;
                            // a send that finds no receiver is harmlessly dropped.
                            let _ = watch_tx.send(ev);
                        }
                    }
                }
            }
        });
        RemoteHostFs {
            rpc: LinkRpc::fixed(rpc),
            watch_rx: Mutex::new(Some(watch_rx)),
        }
    }
}

impl HostFsAsync for RemoteHostFs {
    fn read(&self, path: String) -> Pin<Box<dyn Future<Output = io::Result<FsRead>> + Send>> {
        let rpc = self.rpc.clone();
        Box::pin(async move {
            match rpc.request(FS_READ, vec![Value::from(path)]).await {
                Ok(Value::Array(mut a)) => decode_fs_read(&mut a),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fs_read: malformed reply",
                )),
                // A transport failure (daemon gone) is a loud read error, not a
                // silent empty buffer.
                Err(e) => Err(io::Error::other(e.to_string())),
            }
        })
    }

    fn write(
        &self,
        path: String,
        bytes: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = io::Result<Option<FileStat>>> + Send>> {
        let rpc = self.rpc.clone();
        Box::pin(async move {
            match rpc
                .request(FS_WRITE, vec![Value::from(path), Value::Binary(bytes)])
                .await
            {
                Ok(Value::Array(a)) => decode_fs_write(&a),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fs_write: malformed reply",
                )),
                // A daemon error (permission, transport gone) is a loud write
                // failure the editor surfaces — the saved-state never clears on it.
                Err(e) => Err(io::Error::other(e.to_string())),
            }
        })
    }

    fn chdir(&self, path: String) -> Pin<Box<dyn Future<Output = io::Result<String>> + Send>> {
        let rpc = self.rpc.clone();
        Box::pin(async move {
            match rpc.request(FS_CHDIR, vec![Value::from(path)]).await {
                Ok(Value::Array(a)) => decode_fs_chdir(&a),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fs_chdir: malformed reply",
                )),
                // A daemon error reply carries the `E344` text (a missing/!dir target) or
                // a transport failure; either is surfaced loud, never a silent no-move.
                Err(e) => Err(io::Error::other(e.to_string())),
            }
        })
    }

    fn mkdir(&self, path: String) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send>> {
        let rpc = self.rpc.clone();
        Box::pin(async move {
            // `["ok"]` on success; any error reply (or transport failure) is loud.
            match rpc.request(FS_MKDIR, vec![Value::from(path)]).await {
                Ok(_) => Ok(()),
                Err(e) => Err(io::Error::other(e.to_string())),
            }
        })
    }

    fn remove(&self, path: String) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send>> {
        let rpc = self.rpc.clone();
        Box::pin(async move {
            match rpc.request(FS_REMOVE, vec![Value::from(path)]).await {
                Ok(_) => Ok(()),
                Err(e) => Err(io::Error::other(e.to_string())),
            }
        })
    }

    fn watch(&self, path: String, known: Option<FileStat>) {
        self.rpc.notify(
            FS_WATCH,
            vec![
                Value::from(path),
                known.map_or(Value::Nil, |s| encode_stat(&s)),
            ],
        );
    }

    fn unwatch(&self, path: String) {
        self.rpc.notify(FS_UNWATCH, vec![Value::from(path)]);
    }

    fn take_watch_events(&self) -> Option<UnboundedReceiver<WatchEvent>> {
        self.watch_rx.lock().unwrap().take()
    }
}

/// `fs_changed [path, stat?]` → [`WatchEvent`]; `None` on a malformed frame (dropped —
/// a peer is the same build). A nil/absent stat means the file vanished.
fn decode_fs_changed(params: Vec<Value>) -> Option<WatchEvent> {
    let path = params.first()?.as_str()?.to_string();
    let stat = params.get(1).and_then(decode_stat);
    Some(WatchEvent { path, stat })
}

/// `["file", bytes]` / `["new"]` → [`FsRead`]; anything else is a malformed reply.
fn decode_fs_read(a: &mut [Value]) -> io::Result<FsRead> {
    match a.first().and_then(Value::as_str) {
        Some("file") => {
            let stat = a.get(2).and_then(decode_stat);
            match a.get_mut(1).map(|v| std::mem::replace(v, Value::Nil)) {
                Some(Value::Binary(bytes)) => Ok(FsRead::File(bytes, stat)),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fs_read: file reply missing bytes",
                )),
            }
        }
        Some("new") => Ok(FsRead::New),
        Some("dir") => {
            let path = a
                .get(1)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let entries = match a.get_mut(2).map(|v| std::mem::replace(v, Value::Nil)) {
                Some(Value::Array(items)) => {
                    items.into_iter().filter_map(decode_dir_entry).collect()
                }
                _ => Vec::new(),
            };
            Ok(FsRead::Dir { path, entries })
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fs_read: unknown reply tag",
        )),
    }
}

/// One `[is_dir, name]` wire pair → a [`DirEntry`]; `None` on a malformed pair (dropped
/// — a peer is the same build, so this only guards corruption).
fn decode_dir_entry(v: Value) -> Option<DirEntry> {
    let a = v.as_array()?;
    let is_dir = a.first()?.as_bool()?;
    let name = a.get(1)?.as_str()?.to_string();
    Some(DirEntry { is_dir, name })
}

/// `["ok", stat?]` → the post-write [`FileStat`] (or `None`); any other tag is a
/// malformed reply. A daemon *error* never reaches here — it comes back as the RPC
/// `Err` arm in [`RemoteHostFs::write`], a loud failure, not an `["ok", …]`.
fn decode_fs_write(a: &[Value]) -> io::Result<Option<FileStat>> {
    match a.first().and_then(Value::as_str) {
        Some("ok") => Ok(a.get(1).and_then(decode_stat)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fs_write: unknown reply tag",
        )),
    }
}

/// Decode an `fs_chdir` ok reply (`["ok", canonical]`) to the canonical directory path.
/// A daemon *error* reply (the `E344` text) never reaches here — `bemtvi_rpc` surfaces it
/// as the `request` future's `Err`, which [`RemoteHostFs::chdir`] maps straight through.
fn decode_fs_chdir(a: &[Value]) -> io::Result<String> {
    match (
        a.first().and_then(Value::as_str),
        a.get(1).and_then(Value::as_str),
    ) {
        (Some("ok"), Some(path)) => Ok(path.to_owned()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fs_chdir: malformed ok reply",
        )),
    }
}

/// A [`FileStat`] on the wire: `[secs, nanos, size]`, where `secs`/`nanos` are the
/// mtime as a duration past the Unix epoch (a `nil` mtime — platform reports none —
/// becomes a nil `secs`). Kept self-contained so both legs agree on the shape.
fn encode_stat(stat: &FileStat) -> Value {
    let (secs, nanos) = match stat.mtime.and_then(|t| t.duration_since(UNIX_EPOCH).ok()) {
        Some(d) => (Value::from(d.as_secs()), Value::from(d.subsec_nanos())),
        None => (Value::Nil, Value::from(0u32)),
    };
    Value::Array(vec![secs, nanos, Value::from(stat.size)])
}

/// Inverse of [`encode_stat`]: `[secs, nanos, size]` → [`FileStat`], or `None` if the
/// value isn't a well-formed stat triple (so a missing/garbled stat degrades to "no
/// baseline" rather than erroring the whole write).
fn decode_stat(v: &Value) -> Option<FileStat> {
    let a = v.as_array()?;
    let size = a.get(2)?.as_u64()?;
    let mtime = match a.first() {
        Some(secs) if !secs.is_nil() => {
            let secs = secs.as_u64()?;
            let nanos = a.get(1).and_then(Value::as_u64).unwrap_or(0) as u32;
            Some(UNIX_EPOCH + Duration::new(secs, nanos))
        }
        _ => None,
    };
    Some(FileStat { mtime, size })
}

/// Run the daemon end of the *filesystem* wire over `reader`/`writer`, serving
/// `fs_read` requests from `fs` (the daemon's real backend — [`StdHostFs`] in the
/// binary, a fake in tests). Returns when the connection closes. Reads run inline
/// (the daemon serves one request at a time); an initial open is a single fetch, so
/// no concurrency is needed yet.
///
/// [`StdHostFs`]: bemtvi_core::StdHostFs
pub async fn serve_fs_daemon<R, W>(
    reader: R,
    writer: W,
    fs: Box<dyn HostFs + Send>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (rpc, incoming) = connect_bounded(reader, writer);
    serve_fs_daemon_on(rpc, incoming, fs).await
}

/// The filesystem + watch leg's connection-agnostic core (see [`serve_proc_daemon_on`]
/// for why the `*_on` split exists). Drives `fs_read`/`fs_write`/`fs_watch` over a
/// shared [`Rpc`] + its demuxed inbound stream.
pub async fn serve_fs_daemon_on(
    rpc: Rpc,
    mut incoming: Receiver<Incoming>,
    fs: Box<dyn HostFs + Send>,
) -> anyhow::Result<()> {
    // The watch leg (`HostWatch`): watched path → last-seen stat. The daemon *owns*
    // change detection — the edit-host arms a watch (`fs_watch`) and only reacts to a
    // push, so it never stats the remote disk itself. A coarse poll (the daemon is the
    // lag-tolerant leg) re-stats each watched path and pushes `fs_changed` on a diff.
    let mut watches: HashMap<PathBuf, Option<FileStat>> = HashMap::new();
    let mut poll = tokio::time::interval(WATCH_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            msg = incoming.recv() => {
                let Some(msg) = msg else { break }; // the edit-host hung up
                match msg {
                    Incoming::Request { id, method, mut params } => {
                        let reply = match method.as_str() {
                            FS_READ => serve_read(&*fs, &params),
                            FS_CHDIR => serve_chdir(&*fs, &params),
                            FS_MKDIR => serve_mkdir(&*fs, &params),
                            FS_REMOVE => serve_remove(&*fs, &params),
                            FS_WRITE => {
                                let reply = serve_write(&*fs, &mut params);
                                // Self-suppress: a successful write changed the file, but
                                // it was the edit-host's *own* `:w` — refresh the watch
                                // baseline so the poll doesn't push it back as an external
                                // change. Same task as the poll, so no race (it can't tick
                                // mid-write). `serve_write` only takes the *bytes* out of
                                // `params`, so the path is still readable here.
                                if reply.is_ok() {
                                    if let Some(path) =
                                        params.first().and_then(Value::as_str).map(PathBuf::from)
                                    {
                                        if let Some(slot) = watches.get_mut(&path) {
                                            *slot = fs.stat(&path);
                                        }
                                    }
                                }
                                reply
                            }
                            other => Err(Value::from(format!("unknown method: {other}"))),
                        };
                        rpc.respond(id, reply);
                    }
                    // The watch leg's arm/disarm — notifications, not requests (there is
                    // no reply; the change comes back later as `fs_changed`).
                    Incoming::Notification { method, params } => match method.as_str() {
                        FS_WATCH => {
                            if let Some(path) =
                                params.first().and_then(Value::as_str).map(PathBuf::from)
                            {
                                // Baseline the current stat so the very next poll doesn't
                                // misfire on a file that hasn't changed since the open.
                                let now = fs.stat(&path);
                                // The reconnect re-stat: the edit-host passes its own disk
                                // baseline as `known`. If the live file already differs (it
                                // changed while the link was down — this fresh daemon never
                                // saw it), push `fs_changed` now so the edit-host reconciles
                                // it, rather than adopting the changed file as the new
                                // silent baseline. An absent/equal `known` pushes nothing.
                                let known = params.get(1).and_then(decode_stat);
                                if params.get(1).is_some_and(|v| !v.is_nil()) && known != now {
                                    rpc.notify(
                                        FS_CHANGED,
                                        vec![
                                            Value::from(path.to_string_lossy().into_owned()),
                                            now.map_or(Value::Nil, |s| encode_stat(&s)),
                                        ],
                                    );
                                }
                                watches.insert(path, now);
                            }
                        }
                        FS_UNWATCH => {
                            if let Some(path) =
                                params.first().and_then(Value::as_str).map(PathBuf::from)
                            {
                                watches.remove(&path);
                            }
                        }
                        _ => {}
                    },
                }
            }
            // Re-stat the watched paths and push any that drifted from their baseline.
            // Disabled while nothing is watched, so an idle fs daemon does no work.
            _ = poll.tick(), if !watches.is_empty() => {
                for (path, last) in watches.iter_mut() {
                    let now = fs.stat(path);
                    if now != *last {
                        *last = now;
                        rpc.notify(
                            FS_CHANGED,
                            vec![
                                Value::from(path.to_string_lossy().into_owned()),
                                now.map_or(Value::Nil, |s| encode_stat(&s)),
                            ],
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

/// Serve one `fs_read [path]` against `fs`, projecting [`classify`]'s result onto the
/// `["file", bytes]` / `["new"]` / `["dir", path, entries]` wire shape (or a loud error
/// reply).
fn serve_read(fs: &dyn HostFs, params: &[Value]) -> Result<Value, Value> {
    let Some(path) = params.first().and_then(Value::as_str).map(PathBuf::from) else {
        return Err(Value::from("fs_read: missing path"));
    };
    match classify(fs, &path) {
        Ok(FsRead::File(bytes, stat)) => Ok(Value::Array(vec![
            Value::from("file"),
            Value::Binary(bytes),
            stat.as_ref().map_or(Value::Nil, encode_stat),
        ])),
        Ok(FsRead::New) => Ok(Value::Array(vec![Value::from("new")])),
        Ok(FsRead::Dir { path, entries }) => Ok(Value::Array(vec![
            Value::from("dir"),
            Value::from(path),
            encode_dir_entries(entries),
        ])),
        Err(e) => Err(Value::from(e.to_string())),
    }
}

/// Serve one `fs_chdir [path]` against `fs`: resolve a `:cd` target on the daemon and
/// reply `["ok", canonical]` with its canonical absolute path, or a loud `E344` error if
/// it isn't a readable directory. Pure — no process `chdir` (the daemon serves many
/// sessions in one process), so this only *resolves and validates*; the edit-host owns
/// the logical cwd. An empty path means `:cd` with no argument → the daemon's `$HOME`; a
/// leading `~` expands against the daemon's home (Unix `:cd` semantics, resolved on the
/// remote where it belongs). Directory-ness is checked through the [`HostFs`] seam
/// (`read_dir` succeeds only for a directory), so a fake test backend behaves identically.
fn serve_chdir(fs: &dyn HostFs, params: &[Value]) -> Result<Value, Value> {
    let Some(arg) = params.first().and_then(Value::as_str) else {
        return Err(Value::from("fs_chdir: missing path"));
    };
    let target = expand_remote_cd_arg(arg);
    // `read_dir` is the directory check: it succeeds only for a directory and fails
    // (NotFound / NotADirectory) for anything else — exactly vim's `E344` condition.
    match fs.read_dir(&target) {
        Ok(_) => {
            // The canonical absolute path (symlinks resolved on the daemon) is what the
            // edit-host stores + reports, so `:pwd` shows the real remote directory.
            let canon = fs.canonicalize(&target).unwrap_or(target);
            Ok(Value::Array(vec![
                Value::from("ok"),
                Value::from(canon.to_string_lossy().into_owned()),
            ]))
        }
        Err(e) => Err(Value::from(format!(
            "E344: Can't change directory to \"{}\": {e}",
            target.display()
        ))),
    }
}

/// Recursively create a directory on the daemon (`fs_mkdir [path]` → `["ok"]`), for the
/// remote-shada mirror to ensure its per-namespace dir before the first upload. A failure
/// is a loud error reply (the session surfaces it, never a silent no-op).
fn serve_mkdir(fs: &dyn HostFs, params: &[Value]) -> Result<Value, Value> {
    let Some(path) = params.first().and_then(Value::as_str) else {
        return Err(Value::from("fs_mkdir: missing path"));
    };
    match fs.create_dir_all(Path::new(path)) {
        Ok(()) => Ok(Value::Array(vec![Value::from("ok")])),
        Err(e) => Err(Value::from(format!("fs_mkdir: {e}"))),
    }
}

/// Remove a file on the daemon (`fs_remove [path]` → `["ok"]`), for the remote-shada
/// mirror's clean-exit compaction (deleting an absorbed sibling store). A failure is a
/// loud error reply.
fn serve_remove(fs: &dyn HostFs, params: &[Value]) -> Result<Value, Value> {
    let Some(path) = params.first().and_then(Value::as_str) else {
        return Err(Value::from("fs_remove: missing path"));
    };
    match fs.remove_file(Path::new(path)) {
        Ok(()) => Ok(Value::Array(vec![Value::from("ok")])),
        Err(e) => Err(Value::from(format!("fs_remove: {e}"))),
    }
}

/// Expand a `:cd` argument on the **daemon** side: an empty arg → `$HOME` (Unix `:cd` with
/// no directory), a leading `~` / `~/…` → the daemon's home dir, anything else verbatim
/// (the edit-host already absolutized relative paths against its `DirState`, so what
/// arrives is absolute or `~`-prefixed). Mirrors the edit-host's local `expand_cd_arg`,
/// but rooted at the *daemon's* `$HOME` — the home `~` must mean on the remote.
fn expand_remote_cd_arg(arg: &str) -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    if arg.is_empty() {
        return home().unwrap_or_else(|| PathBuf::from("/"));
    }
    if let Some(rest) = arg.strip_prefix('~') {
        if rest.is_empty() {
            return home().unwrap_or_else(|| PathBuf::from(arg));
        }
        if let Some(rest) = rest.strip_prefix('/') {
            if let Some(h) = home() {
                return h.join(rest);
            }
        }
    }
    PathBuf::from(arg)
}

/// `[[is_dir, name], …]` — a directory's entries on the wire. The edit-host sorts and
/// renders them; the daemon only reports the raw `(is_dir, name)` pairs.
fn encode_dir_entries(entries: Vec<DirEntry>) -> Value {
    Value::Array(
        entries
            .into_iter()
            .map(|e| Value::Array(vec![Value::from(e.is_dir), Value::from(e.name)]))
            .collect(),
    )
}

/// Serve one `fs_write [path, bytes]` against `fs`: do the atomic write through the
/// same sync [`HostFs`] the local server uses, then re-stat so the reply carries the
/// new [`FileStat`] the edit-host stamps as its `disk` baseline. A write failure is a
/// loud error reply — the edit-host's saved-state clears *only* on the `["ok", …]`.
fn serve_write(fs: &dyn HostFs, params: &mut [Value]) -> Result<Value, Value> {
    let Some(path) = params.first().and_then(Value::as_str).map(PathBuf::from) else {
        return Err(Value::from("fs_write: missing path"));
    };
    let bytes = match params.get_mut(1).map(|v| std::mem::replace(v, Value::Nil)) {
        Some(Value::Binary(b)) => b,
        _ => return Err(Value::from("fs_write: missing bytes")),
    };
    match fs.write_atomic(&path, &bytes) {
        Ok(()) => {
            let stat = fs
                .stat(&path)
                .map(|s| encode_stat(&s))
                .unwrap_or(Value::Nil);
            Ok(Value::Array(vec![Value::from("ok"), stat]))
        }
        Err(e) => Err(Value::from(e.to_string())),
    }
}

/// Resolve `path` against `fs` to a [`FsRead`], using only the sync [`HostFs`]
/// surface (so a fake test backend and the real disk behave identically). A readable
/// directory becomes a `Dir` listing (the remote explorer); a `NotFound` is the
/// legitimate new-file case; any other read error propagates loudly.
fn classify(fs: &dyn HostFs, path: &Path) -> io::Result<FsRead> {
    if let Ok(entries) = fs.read_dir(path) {
        // A directory: list it for the remote explorer (Phase 3g). Canonicalize so the
        // edit-host's `../`/descend navigation is unambiguous; fall back to the given
        // path if it can't be resolved.
        let dir = fs.canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        return Ok(FsRead::Dir {
            path: dir.to_string_lossy().into_owned(),
            entries,
        });
    }
    match fs.open_read(path) {
        Ok(mut reader) => {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;
            // Stat the file we just read so the edit-host stamps an accurate `disk`
            // baseline (and so an existing file fires `BufReadPost`, not `BufNewFile`).
            Ok(FsRead::File(bytes, fs.stat(path)))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(FsRead::New),
        Err(e) => Err(e),
    }
}

// ===== the LSP leg (long-lived bidirectional pipes) ===========================

/// Per-server routing on the edit-host side, keyed by the per-spawn `id`: where the
/// demux delivers a server's stdout/stderr chunks and its eventual exit. The
/// `stdout_tx`/`stderr_tx` feed the [`ChannelReader`]s the manager reads; dropping
/// them (on exit, or a dead link) is what EOFs those readers.
struct LspInflight {
    stdout_tx: UnboundedSender<Vec<u8>>,
    stderr_tx: UnboundedSender<Vec<u8>>,
    exit_tx: oneshot::Sender<(Option<i32>, Option<i32>)>,
}

/// The table of live servers awaiting their daemon reports: `id` → its routing. The
/// demux forwards each `lsp_stdout`/`lsp_stderr` to the matching sinks and fires
/// `exit_tx` (removing the entry) on `lsp_exited`.
type LspInflightMap = Arc<Mutex<HashMap<u64, LspInflight>>>;

/// An [`LspTransport`] that runs language servers on a remote daemon instead of
/// locally: each [`spawn`](LspTransport::spawn) tunnels the server's stdio over the
/// wire to a [`serve_lsp_daemon`] holding the actual child, so the
/// [`LspManager`](bemtvi_lsp::LspManager) drives its `async-lsp` loop unchanged. The
/// drop-in for [`LocalLspTransport`](bemtvi_lsp::LocalLspTransport) on the edit-host
/// side of the split — the long-lived bidirectional-pipe analogue of
/// [`RemoteHostProc`]'s run-to-completion path.
pub struct RemoteLspTransport {
    rpc: LinkRpc,
    inflight: LspInflightMap,
    /// Per-spawn correlation id minted here; the demux routes purely by it.
    next_id: AtomicU64,
}

impl RemoteLspTransport {
    /// Connect to a daemon over `reader`/`writer` (a duplex, or ssh stdio). Spawns
    /// the demux task that fans the daemon's stdout/stderr/exit out to per-server
    /// sinks; call it from within a tokio runtime (its RPC tasks live there).
    pub fn connect<R, W>(reader: R, writer: W) -> RemoteLspTransport
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (rpc, incoming) = connect_bounded(reader, writer);
        let inflight: LspInflightMap = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(run_lsp_demux(incoming, inflight.clone()));
        RemoteLspTransport {
            rpc: LinkRpc::fixed(rpc),
            inflight,
            next_id: AtomicU64::new(1),
        }
    }
}

impl LspTransport for RemoteLspTransport {
    fn spawn(
        &self,
        spec: &ServerSpawn,
    ) -> Pin<Box<dyn Future<Output = io::Result<LspChannel>> + Send>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let rpc = self.rpc.clone();
        let inflight = self.inflight.clone();
        let program = spec.program.clone();
        let args = spec.args.clone();
        // The config's `cmd_env` rides to the daemon, where the process actually
        // runs — a remote session must configure a server exactly as a local one does.
        let env = spec.env.clone();
        // …and so does the working directory. It is a path on the DAEMON's machine
        // (a remote session's cwd and buffer paths already are), resolved editor-side
        // so `cmd_cwd` / `:cd` land identically here and locally rather than the
        // daemon's own launch dir standing in. Empty = inherit the daemon's.
        let cwd = spec
            .cwd
            .as_ref()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default();
        Box::pin(async move {
            let (stdout_tx, stdout_rx) = unbounded_channel::<Vec<u8>>();
            let (stderr_tx, stderr_rx) = unbounded_channel::<Vec<u8>>();
            let (exit_tx, exit_rx) = oneshot::channel();
            // Register *before* the spawn request so a fast reply can't race ahead of
            // its sinks (mirrors [`RemoteHostProc::run`]).
            inflight.lock().unwrap().insert(
                id,
                LspInflight {
                    stdout_tx,
                    stderr_tx,
                    exit_tx,
                },
            );
            // client → server: the manager writes JSON-RPC into `stdin_writer`; a pump
            // reads the other end of the duplex and forwards each chunk as `lsp_stdin`.
            let (stdin_writer, stdin_reader) = tokio::io::duplex(1 << 16);
            tokio::spawn(pump_lsp_stdin(id, stdin_reader, rpc.clone()));
            rpc.notify(
                LSP_SPAWN,
                vec![
                    Value::from(id),
                    Value::from(program),
                    Value::Array(args.into_iter().map(Value::from).collect()),
                    Value::from(cwd),
                    Value::Array(
                        env.into_iter()
                            .map(|(k, v)| Value::Array(vec![Value::from(k), Value::from(v)]))
                            .collect(),
                    ),
                ],
            );
            Ok(LspChannel {
                stdout: Box::pin(ChannelReader::new(stdout_rx)),
                stdin: Box::pin(stdin_writer),
                stderr: Some(Box::pin(ChannelReader::new(stderr_rx))),
                process: Box::new(RemoteLspProcess { id, rpc, exit_rx }),
            })
        })
    }
}

/// The edit-host-side [`LspProcess`]: terminate the remote server (`lsp_kill`) and
/// await its exit, which the demux fires off `lsp_exited`. A dropped daemon link
/// drops the `exit_tx`, so `wait` resolves to `(None, None)` rather than hanging.
struct RemoteLspProcess {
    id: u64,
    rpc: LinkRpc,
    exit_rx: oneshot::Receiver<(Option<i32>, Option<i32>)>,
}

impl LspProcess for RemoteLspProcess {
    fn start_kill(&mut self) {
        self.rpc.notify(LSP_KILL, vec![Value::from(self.id)]);
    }

    fn wait(self: Box<Self>) -> bemtvi_lsp::ExitFuture {
        Box::pin(async move { self.exit_rx.await.unwrap_or((None, None)) })
    }
}

/// Pump the daemon's per-server stdout/stderr/exit off the wire and route each to the
/// server it belongs to. On teardown (`incoming` ends) it clears [`LspInflightMap`],
/// dropping every sink (EOF the readers) and every `exit_tx` (so each waiting server
/// reports `(None, None)` rather than hanging).
async fn run_lsp_demux(mut incoming: Receiver<Incoming>, inflight: LspInflightMap) {
    while let Some(msg) = incoming.recv().await {
        let Incoming::Notification { method, params } = msg else {
            continue; // the daemon speaks only notifications; ignore stray requests
        };
        route_lsp_notification(&inflight, &method, params);
    }
    inflight.lock().unwrap().clear();
}

/// Route one daemon→edit-host LSP notification (`lsp_stdout` / `lsp_stderr` /
/// `lsp_exited`) to the server it belongs to by `id`. Factored out of [`run_lsp_demux`]
/// so the multiplexed [`connect_daemon`] demux — which fans *all* legs off one shared
/// `incoming` — reuses the exact same routing. A method that isn't an LSP push is a
/// no-op. (`stdout`/`stderr` chunks queue onto the unbounded sinks in wire order, so the
/// `lsp_exited` remove-and-drop can't strand trailing output: the reader drains the
/// queued chunks before observing the sink's EOF.)
fn route_lsp_notification(inflight: &LspInflightMap, method: &str, params: Vec<Value>) {
    match method {
        LSP_STDOUT => {
            if let Some((id, bytes)) = decode_id_bytes(params) {
                if let Some(inf) = inflight.lock().unwrap().get(&id) {
                    let _ = inf.stdout_tx.send(bytes);
                }
            }
        }
        LSP_STDERR => {
            if let Some((id, bytes)) = decode_id_bytes(params) {
                if let Some(inf) = inflight.lock().unwrap().get(&id) {
                    let _ = inf.stderr_tx.send(bytes);
                }
            }
        }
        LSP_EXITED => {
            if let Some((id, code, signal)) = decode_lsp_exited(&params) {
                if let Some(inf) = inflight.lock().unwrap().remove(&id) {
                    let _ = inf.exit_tx.send((code, signal));
                }
            }
        }
        _ => {}
    }
}

/// Forward everything the manager writes to a server's stdin onto the wire as
/// `lsp_stdin` chunks, until the duplex closes (the manager's loop ended).
async fn pump_lsp_stdin(id: u64, mut reader: DuplexStream, rpc: LinkRpc) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => rpc.notify(
                LSP_STDIN,
                vec![Value::from(id), Value::Binary(buf[..n].to_vec())],
            ),
        }
    }
}

/// A [`tokio::io::AsyncRead`] fed by an unbounded channel of byte chunks — the bridge
/// from the demux (which receives discrete `lsp_stdout`/`lsp_stderr` *messages*) to the
/// streaming reader the manager's `async-lsp` loop expects. Buffers one chunk across
/// reads; a closed channel reads as EOF.
struct ChannelReader {
    rx: UnboundedReceiver<Vec<u8>>,
    chunk: Vec<u8>,
    pos: usize,
}

impl ChannelReader {
    fn new(rx: UnboundedReceiver<Vec<u8>>) -> ChannelReader {
        ChannelReader {
            rx,
            chunk: Vec::new(),
            pos: 0,
        }
    }
}

impl AsyncRead for ChannelReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.pos < this.chunk.len() {
                let n = std::cmp::min(buf.remaining(), this.chunk.len() - this.pos);
                buf.put_slice(&this.chunk[this.pos..this.pos + n]);
                this.pos += n;
                return Poll::Ready(Ok(()));
            }
            match this.rx.poll_recv(cx) {
                Poll::Ready(Some(chunk)) => {
                    if chunk.is_empty() {
                        continue; // a stray empty chunk would falsely read as EOF
                    }
                    this.chunk = chunk;
                    this.pos = 0;
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())), // sinks dropped → EOF
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Run the daemon end of the LSP wire over `reader`/`writer`: spawn the language
/// servers a far edit-host asks for and stream their stdio back. Returns when the
/// connection closes. Each child runs through the *same* `tokio::process` machinery
/// the local transport uses, so a server behaves identically remote and local.
pub async fn serve_lsp_daemon<R, W>(reader: R, writer: W) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (rpc, incoming) = connect_bounded(reader, writer);
    serve_lsp_daemon_on(rpc, incoming).await
}

/// The LSP leg's connection-agnostic core (see [`serve_proc_daemon_on`] for the `*_on`
/// split). Streams the `lsp_*` raw bidirectional pipe over a shared [`Rpc`] + its
/// demuxed inbound stream.
pub async fn serve_lsp_daemon_on(
    rpc: Rpc,
    mut incoming: Receiver<Incoming>,
) -> anyhow::Result<()> {
    // Per-child stdin channels and kill signals, keyed by the edit-host's spawn id, so
    // `lsp_stdin`/`lsp_kill` can reach the running child (mirrors the process leg's maps).
    let mut stdins: HashMap<u64, UnboundedSender<Vec<u8>>> = HashMap::new();
    let mut kills: HashMap<u64, oneshot::Sender<()>> = HashMap::new();
    while let Some(msg) = incoming.recv().await {
        let Incoming::Notification { method, params } = msg else {
            continue; // the edit-host drives the daemon with notifications only
        };
        match method.as_str() {
            LSP_SPAWN => {
                if let Some((id, program, args, cwd, env)) = decode_lsp_spawn(params) {
                    let (stdin_tx, stdin_rx) = unbounded_channel::<Vec<u8>>();
                    let (kill_tx, kill_rx) = oneshot::channel();
                    stdins.insert(id, stdin_tx);
                    kills.insert(id, kill_tx);
                    tokio::spawn(serve_one_lsp(
                        id,
                        program,
                        args,
                        cwd,
                        env,
                        stdin_rx,
                        kill_rx,
                        rpc.clone(),
                    ));
                }
            }
            LSP_STDIN => {
                if let Some((id, bytes)) = decode_id_bytes(params) {
                    if let Some(tx) = stdins.get(&id) {
                        let _ = tx.send(bytes);
                    }
                }
            }
            LSP_KILL => {
                if let Some(id) = params.first().and_then(Value::as_u64) {
                    if let Some(kill_tx) = kills.remove(&id) {
                        let _ = kill_tx.send(());
                    }
                }
            }
            _ => {}
        }
        // Forget channels whose child tasks have closed them (the child exited), the
        // same leak guard the process leg applies.
        stdins.retain(|_, tx| !tx.is_closed());
        kills.retain(|_, tx| !tx.is_closed());
    }
    Ok(())
}

/// Run one language server to completion (or until killed) on the daemon, streaming its
/// stdout/stderr onto the wire and feeding its stdin from `stdin_rx`. Joins the
/// stdout/stderr pumps *before* sending `lsp_exited`, so the edit-host (which EOFs its
/// reader on exit) never loses trailing output.
#[allow(clippy::too_many_arguments)]
async fn serve_one_lsp(
    id: u64,
    program: String,
    args: Vec<String>,
    cwd: String,
    env: Vec<(String, String)>,
    mut stdin_rx: UnboundedReceiver<Vec<u8>>,
    mut kill_rx: oneshot::Receiver<()>,
    rpc: Rpc,
) {
    let mut command = tokio::process::Command::new(&program);
    command
        .args(&args)
        // Layered over the daemon's own environment, exactly as the local transport
        // layers it over the editor's — `cmd_env` adds, never replaces.
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if !cwd.is_empty() {
        command.current_dir(&cwd);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_e) => {
            // A spawn failure reports a bare exit (no code) — the edit-host's reader
            // EOFs and the manager reports the failure during initialize, the same way
            // a local spawn error does.
            rpc.notify(LSP_EXITED, vec![Value::from(id), Value::Nil, Value::Nil]);
            return;
        }
    };
    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut out_handle =
        stdout.map(|out| tokio::spawn(pump_child_output(out, id, LSP_STDOUT, rpc.clone())));
    let mut err_handle =
        stderr.map(|err| tokio::spawn(pump_child_output(err, id, LSP_STDERR, rpc.clone())));
    let stdin_task = tokio::spawn(async move {
        if let Some(sink) = stdin.as_mut() {
            while let Some(bytes) = stdin_rx.recv().await {
                if sink.write_all(&bytes).await.is_err() || sink.flush().await.is_err() {
                    break;
                }
            }
            let _ = sink.shutdown().await; // close → the server reads EOF
        }
    });
    let mut killed = false;
    let status = loop {
        tokio::select! {
            status = child.wait() => break status.ok(),
            // Disable the arm once fired (re-polling a consumed oneshot busy-loops);
            // the child still exits via `child.wait()` after the kill takes effect.
            _ = &mut kill_rx, if !killed => {
                killed = true;
                let _ = child.start_kill();
            }
        }
    };
    // Flush all stdout/stderr onto the wire *before* signaling exit.
    if let Some(h) = out_handle.take() {
        let _ = h.await;
    }
    if let Some(h) = err_handle.take() {
        let _ = h.await;
    }
    stdin_task.abort();
    let (code, signal) = lsp_exit_code_signal(status);
    rpc.notify(
        LSP_EXITED,
        vec![
            Value::from(id),
            code.map_or(Value::Nil, Value::from),
            signal.map_or(Value::Nil, Value::from),
        ],
    );
}

/// Stream a child's stdout (or stderr) onto the wire as `method` chunks until it
/// closes (the child exited). Stops on the first read error or EOF.
async fn pump_child_output<R>(mut src: R, id: u64, method: &'static str, rpc: Rpc)
where
    R: AsyncRead + Unpin,
{
    let mut buf = [0u8; 8192];
    loop {
        match src.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => rpc.notify(
                method,
                vec![Value::from(id), Value::Binary(buf[..n].to_vec())],
            ),
        }
    }
}

/// Split a child's [`std::process::ExitStatus`] into `(code, signal)` for the
/// `lsp_exited` wire (the daemon-side analogue of `bemtvi-lsp`'s `exit_code_signal`).
fn lsp_exit_code_signal(status: Option<std::process::ExitStatus>) -> (Option<i32>, Option<i32>) {
    let Some(status) = status else {
        return (None, None);
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        (status.code(), status.signal())
    }
    #[cfg(not(unix))]
    {
        (status.code(), None)
    }
}

/// `lsp_spawn` params → `(id, program, args, cwd, env)`, or `None` on a malformed
/// frame. `env` is the trailing `[[name, value], …]` element carrying the config's
/// `cmd_env`; an older peer that doesn't send it yields an empty environment, which
/// is exactly what a config with no `cmd_env` means.
#[allow(clippy::type_complexity)]
fn decode_lsp_spawn(
    mut params: Vec<Value>,
) -> Option<(u64, String, Vec<String>, String, Vec<(String, String)>)> {
    if params.len() < 4 {
        return None;
    }
    let id = params[0].as_u64()?;
    let program = params[1].as_str()?.to_string();
    let args = match std::mem::replace(&mut params[2], Value::Nil) {
        Value::Array(a) => a
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => return None,
    };
    let cwd = params[3].as_str().unwrap_or("").to_string();
    let env = match params.get_mut(4).map(|v| std::mem::replace(v, Value::Nil)) {
        Some(Value::Array(pairs)) => pairs
            .into_iter()
            .filter_map(|pair| match pair {
                Value::Array(kv) if kv.len() >= 2 => Some((
                    kv[0].as_str()?.to_string(),
                    kv[1].as_str().unwrap_or("").to_string(),
                )),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    Some((id, program, args, cwd, env))
}

/// `[id, bytes]` → `(id, bytes)`, moving the (potentially large) payload out. Used by
/// both `lsp_stdin` (daemon side) and `lsp_stdout`/`lsp_stderr` (edit-host side).
fn decode_id_bytes(mut params: Vec<Value>) -> Option<(u64, Vec<u8>)> {
    if params.len() < 2 {
        return None;
    }
    let id = params[0].as_u64()?;
    let bytes = match std::mem::replace(&mut params[1], Value::Nil) {
        Value::Binary(b) => b,
        Value::String(s) => s.into_bytes(),
        _ => Vec::new(),
    };
    Some((id, bytes))
}

/// `lsp_exited` params → `(id, code?, signal?)`. A nil code/signal stays `None`.
fn decode_lsp_exited(params: &[Value]) -> Option<(u64, Option<i32>, Option<i32>)> {
    let id = params.first()?.as_u64()?;
    let code = params.get(1).and_then(Value::as_i64).map(|c| c as i32);
    let signal = params.get(2).and_then(Value::as_i64).map(|s| s as i32);
    Some((id, code, signal))
}

// ----- the duplex-process leg (`dproc_*`) -----------------------------------------
//
// The remote half of `btv.process.open`: a long-lived duplex child (the DAP / framed-
// protocol transport) run on the daemon, with raw stdout/stderr streamed back and
// stdin fed over the wire. Reuses [`run_duplex_process`](crate::host::run_duplex_process)
// — the same function the native event-loop actor runs — with a tiny forwarder turning
// its [`LoopEvent`]s into wire notifications. The wasm edit-host forwards the identical
// requests over WebTransport; one leg, one shape.

/// `dproc_open` params → `(id, argv, cwd?, env)` — exactly the shared
/// [`ProcHead`] [`decode_proc_head`] decodes (`proc_spawn` carries stdin/stream
/// on top).
fn decode_dproc_open(mut params: Vec<Value>) -> Option<ProcHead> {
    if params.len() < 4 {
        return None;
    }
    decode_proc_head(&mut params)
}

/// The `dproc_*` leg's connection-agnostic core (see [`serve_proc_daemon_on`] for the
/// `*_on` convention). Run on a per-connection demuxed `incoming`.
pub async fn serve_dproc_daemon_on(
    rpc: Rpc,
    mut incoming: UnboundedReceiver<Incoming>,
) -> anyhow::Result<()> {
    use crate::evloop::LoopEvent;
    use tokio::sync::mpsc::unbounded_channel;

    // One forwarder turns the children's `LoopEvent`s into wire notifications.
    let (ev_tx, mut ev_rx) = unbounded_channel::<LoopEvent>();
    let reply = rpc.clone();
    tokio::spawn(async move {
        while let Some(ev) = ev_rx.recv().await {
            match ev {
                LoopEvent::ProcOut { id, data, stderr } => reply.notify(
                    DPROC_OUT,
                    vec![Value::from(id), Value::Binary(data), Value::from(stderr)],
                ),
                LoopEvent::ProcExit { id, code } => {
                    reply.notify(DPROC_EXIT, vec![Value::from(id), Value::from(code as i64)])
                }
                _ => {}
            }
        }
    });

    let mut stdins: HashMap<u64, UnboundedSender<Vec<u8>>> = HashMap::new();
    let mut kills: HashMap<u64, oneshot::Sender<()>> = HashMap::new();
    while let Some(msg) = incoming.recv().await {
        let Incoming::Notification { method, params } = msg else {
            continue;
        };
        match method.as_str() {
            DPROC_OPEN => {
                if let Some((id, argv, cwd, env)) = decode_dproc_open(params) {
                    let (stdin_tx, stdin_rx) = unbounded_channel::<Vec<u8>>();
                    let (kill_tx, kill_rx) = oneshot::channel();
                    stdins.insert(id, stdin_tx);
                    kills.insert(id, kill_tx);
                    tokio::spawn(crate::host::run_duplex_process(
                        id,
                        argv,
                        cwd,
                        env,
                        kill_rx,
                        stdin_rx,
                        ev_tx.clone(),
                    ));
                }
            }
            DPROC_WRITE => {
                if let Some((id, bytes)) = decode_id_bytes(params) {
                    if let Some(tx) = stdins.get(&id) {
                        let _ = tx.send(bytes);
                    }
                }
            }
            DPROC_KILL => {
                if let Some(id) = params.first().and_then(Value::as_u64) {
                    if let Some(kill_tx) = kills.remove(&id) {
                        let _ = kill_tx.send(());
                    }
                    stdins.remove(&id);
                }
            }
            _ => {}
        }
        stdins.retain(|_, tx| !tx.is_closed());
        kills.retain(|_, tx| !tx.is_closed());
    }
    Ok(())
}

// ----- the socket leg (`sock_*`) --------------------------------------------------
//
// The remote half of `btv.socket.connect`: a long-lived TCP connection the daemon
// dials, streaming bytes both ways. Reuses
// [`run_socket_connection`](crate::host::run_socket_connection).

/// The `sock_*` leg's connection-agnostic core.
pub async fn serve_sock_daemon_on(
    rpc: Rpc,
    mut incoming: UnboundedReceiver<Incoming>,
) -> anyhow::Result<()> {
    use crate::evloop::LoopEvent;
    use tokio::sync::mpsc::unbounded_channel;

    let (ev_tx, mut ev_rx) = unbounded_channel::<LoopEvent>();
    let reply = rpc.clone();
    tokio::spawn(async move {
        while let Some(ev) = ev_rx.recv().await {
            match ev {
                LoopEvent::SockConnected { id } => {
                    reply.notify(SOCK_CONNECTED, vec![Value::from(id)])
                }
                LoopEvent::SockData { id, data } => {
                    reply.notify(SOCK_DATA, vec![Value::from(id), Value::Binary(data)])
                }
                LoopEvent::SockClosed { id, error } => reply.notify(
                    SOCK_CLOSED,
                    vec![Value::from(id), error.map_or(Value::Nil, Value::from)],
                ),
                _ => {}
            }
        }
    });

    let mut writes: HashMap<u64, UnboundedSender<Vec<u8>>> = HashMap::new();
    let mut closes: HashMap<u64, oneshot::Sender<()>> = HashMap::new();
    while let Some(msg) = incoming.recv().await {
        let Incoming::Notification { method, params } = msg else {
            continue;
        };
        match method.as_str() {
            SOCK_CONNECT => {
                if let (Some(id), Some(host), Some(port)) = (
                    params.first().and_then(Value::as_u64),
                    params.get(1).and_then(|v| v.as_str().map(str::to_owned)),
                    params.get(2).and_then(Value::as_u64),
                ) {
                    let (write_tx, write_rx) = unbounded_channel::<Vec<u8>>();
                    let (close_tx, close_rx) = oneshot::channel();
                    writes.insert(id, write_tx);
                    closes.insert(id, close_tx);
                    tokio::spawn(crate::host::run_socket_connection(
                        id,
                        host,
                        port as u16,
                        close_rx,
                        write_rx,
                        ev_tx.clone(),
                    ));
                }
            }
            SOCK_WRITE => {
                if let Some((id, bytes)) = decode_id_bytes(params) {
                    if let Some(tx) = writes.get(&id) {
                        let _ = tx.send(bytes);
                    }
                }
            }
            SOCK_CLOSE => {
                if let Some(id) = params.first().and_then(Value::as_u64) {
                    if let Some(close_tx) = closes.remove(&id) {
                        let _ = close_tx.send(());
                    }
                    writes.remove(&id);
                }
            }
            _ => {}
        }
        writes.retain(|_, tx| !tx.is_closed());
        closes.retain(|_, tx| !tx.is_closed());
    }
    Ok(())
}

// ----- the Lua-filesystem leg (`luafs_op`) ----------------------------------------
//
// `RemoteFsJobs` (the edit-host side) is how a **native-daemon** session runs async
// `btv.fs`: the event-loop actor hands a whole [`FsJob`](bemtvi_lua::FsJob) here, it
// crosses in ONE `luafs_op` request, and the daemon runs it through
// [`run_fs_job`](bemtvi_lua::run_fs_job) against its [`StdLuaFs`](bemtvi_lua::StdLuaFs)
// (decomposing any compound op daemon-side, so a recursive copy is one round-trip, not a
// chatter of per-op calls). The wasm edit-host forwards the identical `luafs_op` request
// over WebTransport — one leg, one shape. Unlike the retired per-op `RemoteLuaFs` bridge
// this parks no thread: the actor `await`s the reply on the shared link runtime.

/// One queued fs job on the link thread: the whole [`FsJob`](bemtvi_lua::FsJob) and the
/// tokio oneshot the awaiting actor parks on for the typed result. Async (a tokio channel),
/// because the caller is the event-loop actor's task, not a synchronous editor-thread call.
type FsJobReq = (
    bemtvi_lua::FsJob,
    tokio::sync::oneshot::Sender<Result<bemtvi_lua::FsValue, bemtvi_lua::FsError>>,
);

/// One queued git job on the link thread — the whole [`GitJob`](bemtvi_lua::GitJob) and the
/// oneshot the awaiting actor parks on for the typed result. The git twin of [`FsJobReq`].
type GitJobReq = (
    bemtvi_lua::GitJob,
    tokio::sync::oneshot::Sender<Result<bemtvi_lua::GitValue, bemtvi_lua::GitError>>,
);

/// One queued HTTP job on the link thread — the [`HttpRequest`](bemtvi_lua::HttpRequest) and
/// the oneshot the awaiting actor parks on for the typed result. The HTTP sibling of
/// [`FsJobReq`].
type HttpJobReq = (
    bemtvi_lua::HttpRequest,
    tokio::sync::oneshot::Sender<Result<bemtvi_lua::HttpResponse, bemtvi_lua::HttpError>>,
);

/// The `luafs_op` leg's job server: pull each whole [`FsJob`](bemtvi_lua::FsJob) off
/// `req_rx`, send it as one `luafs_op` request over `rpc`, decode the reply through the
/// shared [`fswire`](bemtvi_lua) codec, and deliver the typed result to the awaiting actor.
async fn run_fs_jobs(rpc: LinkRpc, mut req_rx: UnboundedReceiver<FsJobReq>) {
    while let Some((job, reply_tx)) = req_rx.recv().await {
        let result = match rpc
            .request(LUAFS_OP, vec![bemtvi_lua::fs_job_to_value(&job)])
            .await
        {
            Ok(v) => bemtvi_lua::fs_result_from_value(&v),
            // A transport failure (daemon gone) rejects the promise loud — never a panic.
            Err(e) => Err(bemtvi_lua::FsError {
                code: "EIO".to_string(),
                message: format!("btv.fs: daemon error: {e}"),
            }),
        };
        let _ = reply_tx.send(result);
    }
}

/// The `git_op` leg's job server: pull each whole [`GitJob`](bemtvi_lua::GitJob) off `req_rx`,
/// send it as one `git_op` request over `rpc`, decode the reply through the shared
/// [`gitwire`](bemtvi_lua) codec, and deliver the typed result to the awaiting actor. The git
/// twin of [`run_fs_jobs`].
async fn run_git_jobs(rpc: LinkRpc, mut req_rx: UnboundedReceiver<GitJobReq>) {
    while let Some((job, reply_tx)) = req_rx.recv().await {
        let result = match rpc
            .request(GIT_OP, vec![bemtvi_lua::git_job_to_value(&job)])
            .await
        {
            Ok(v) => bemtvi_lua::git_result_from_value(&v),
            // A transport failure (daemon gone) rejects the promise loud — never a panic.
            Err(e) => Err(bemtvi_lua::GitError {
                code: "EIO".to_string(),
                message: format!("btv.git: daemon error: {e}"),
            }),
        };
        let _ = reply_tx.send(result);
    }
}

/// Spawn a dedicated link thread (its own current-thread runtime + the RPC link) that
/// drives `serve` over the freshly-connected transport — the shared body of the
/// standalone single-leg `connect` forms ([`RemoteFsJobs::connect`] /
/// [`RemoteHttp::connect`]). A runtime that can't build means the link is dead on
/// arrival: the thread returns, `serve`'s captured receiver drops, and every job sees
/// the channel closed and rejects loudly. These legs have no daemon→edit-host pushes,
/// so `incoming` is drained (dropping the receiver would tear the connection down).
fn spawn_leg_thread<R, W, F, Fut>(reader: R, writer: W, serve: F)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    F: FnOnce(LinkRpc) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()>,
{
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        rt.block_on(async move {
            let (rpc, mut incoming) = connect_bounded(reader, writer);
            tokio::spawn(async move { while incoming.recv().await.is_some() {} });
            serve(LinkRpc::fixed(rpc)).await;
        });
    });
}

/// The edit-host side of the `luafs_op` leg for a **native-daemon** session — the actor
/// sends a whole [`FsJob`](bemtvi_lua::FsJob) here and `await`s its typed result. Holds a
/// tokio sender to the shared link runtime's [`run_fs_jobs`]; `Clone` so each `btv.fs` op
/// can be driven concurrently, `Send + Sync` so it rides [`ServerInit`](crate::ServerInit)
/// onto the server thread.
#[derive(Clone)]
pub struct RemoteFsJobs {
    req_tx: UnboundedSender<FsJobReq>,
}

impl RemoteFsJobs {
    /// Connect to a daemon over `reader`/`writer` as a standalone leg, spawning a
    /// dedicated link thread (its own current-thread runtime + the RPC link) that runs
    /// [`run_fs_jobs`]. The multiplexed [`connect_daemon`] builds a `RemoteFsJobs`
    /// directly instead (sharing one link across all legs); this single-leg form is for
    /// driving the `luafs_op` leg in isolation (tests).
    pub fn connect<R, W>(reader: R, writer: W) -> RemoteFsJobs
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (req_tx, req_rx) = unbounded_channel::<FsJobReq>();
        spawn_leg_thread(reader, writer, move |rpc| run_fs_jobs(rpc, req_rx));
        RemoteFsJobs { req_tx }
    }

    /// Send `job` to the daemon over `luafs_op` and `await` the typed result. Off the
    /// editor tick (the caller is the actor's async task), so this is a tokio await, not a
    /// thread park; a dropped link rejects loud.
    pub async fn run(
        &self,
        job: bemtvi_lua::FsJob,
    ) -> Result<bemtvi_lua::FsValue, bemtvi_lua::FsError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if self.req_tx.send((job, reply_tx)).is_err() {
            return Err(bemtvi_lua::FsError {
                code: "ENOTCONN".to_string(),
                message: "btv.fs: daemon link is gone".to_string(),
            });
        }
        reply_rx.await.unwrap_or_else(|_| {
            Err(bemtvi_lua::FsError {
                code: "EIO".to_string(),
                message: "btv.fs: daemon link dropped the request".to_string(),
            })
        })
    }
}

/// The edit-host side of the **streaming watch** leg (`luafs_watch`) for a native-daemon
/// session: `btv.fs.watch` arms a recursive, change-classified watch on the *daemon*, where
/// the files are, instead of on the local disk.
///
/// Without it a daemon session watched its own machine — so a watch on a remote workspace
/// armed on a path that doesn't exist locally (a loud arm failure at best, a watch on an
/// unrelated local directory at worst). Everything built on `btv.fs.watch` inherits the fix:
/// the LSP `workspace/didChangeWatchedFiles` client
/// (`btv.lsp._register_capability`), file-tree plugins, config reloaders.
///
/// Shape follows the `fs_changed` watch leg rather than the request/response `luafs_op` one:
/// arming is a notification and changes come back as daemon→edit-host pushes, decoded by
/// [`run_control_demux`] into the very [`LoopEvent::FsEvent`]s the local `notify` watcher
/// produces — so the server's landing site cannot tell the two apart, and the coalescing
/// happens daemon-side in the *same* [`start_fs_watch_coalesced`](crate::evloop) both
/// sessions use.
///
/// **Re-dial**: a re-dialed daemon is a fresh process that lost every watch, so
/// [`publish_cells`] re-arms the whole set from `armed` — a live `btv.fs.watch` iterator
/// survives an outage rather than going quietly deaf (`sync_buffer_watches`'s re-arm, for
/// streams). The wasm/browser leg does not do this yet: its Worker ends each watch stream
/// with a `luafs_watch_err` when the link drops, so a browser session's watches must be
/// re-created by their consumer.
#[derive(Clone)]
pub struct RemoteFsWatch {
    rpc: LinkRpc,
    /// Every watch armed on the daemon, `id -> (path, recursive)`, kept so a re-dial can
    /// re-arm them. Entries live until `btv.fs.watch`'s `:stop()` (`unwatch`) — a dropped
    /// link does NOT clear them, which is what makes the re-arm possible.
    armed: Arc<Mutex<HashMap<u64, (String, bool)>>>,
    /// The receiver of decoded `luafs_change` / `luafs_watch_err` pushes, taken once by the
    /// server loop (the [`RemoteHostFs::take_watch_events`] pattern — a `&self` accessor
    /// that can only hand it out once).
    events_rx: Arc<Mutex<Option<UnboundedReceiver<LoopEvent>>>>,
}

impl RemoteFsWatch {
    /// Connect to a daemon over `reader`/`writer` as a standalone leg (a dedicated link
    /// thread whose demux decodes the change pushes) — the watch twin of
    /// [`RemoteFsJobs::connect`], for driving the `luafs_watch` leg in isolation (tests).
    /// The multiplexed [`connect_daemon`] builds a `RemoteFsWatch` directly instead,
    /// sharing one link across all legs — and only that path re-arms across a re-dial (a
    /// single-leg link never re-dials).
    pub fn connect<R, W>(reader: R, writer: W) -> RemoteFsWatch
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (event_tx, event_rx) = unbounded_channel::<LoopEvent>();
        let (rpc_tx, rpc_rx) = std::sync::mpsc::channel::<LinkRpc>();
        std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                let (rpc, mut incoming) = connect_bounded(reader, writer);
                if rpc_tx.send(LinkRpc::fixed(rpc)).is_err() {
                    return;
                }
                // The whole point of this leg is its pushes, so the drain decodes them
                // rather than discarding the stream like `spawn_leg_thread` does.
                while let Some(msg) = incoming.recv().await {
                    let Incoming::Notification { method, params } = msg else {
                        continue;
                    };
                    let ev = match method.as_str() {
                        LUAFS_CHANGE => decode_luafs_change(params),
                        LUAFS_WATCH_ERR => decode_luafs_watch_err(params),
                        _ => None,
                    };
                    if let Some(ev) = ev {
                        if event_tx.send(ev).is_err() {
                            break;
                        }
                    }
                }
            });
        });
        RemoteFsWatch {
            // A failed handshake leaves an empty cell: every arm is then dropped and the
            // consumer's watch simply never fires — the same as a dropped link.
            rpc: rpc_rx.recv().unwrap_or_else(|_| LinkRpc::empty()),
            armed: Arc::new(Mutex::new(HashMap::new())),
            events_rx: Arc::new(Mutex::new(Some(event_rx))),
        }
    }

    /// Arm (or re-arm) watch `id` on the daemon. Fire-and-forget: changes arrive as
    /// [`LoopEvent::FsEvent`]s on [`take_events`](Self::take_events), and an arm failure
    /// comes back on the same channel as an `error` event — never a silent dead watch.
    pub fn watch(&self, id: u64, path: String, recursive: bool) {
        self.armed
            .lock()
            .unwrap()
            .insert(id, (path.clone(), recursive));
        self.rpc.notify(
            LUAFS_WATCH,
            vec![Value::from(id), Value::from(path), Value::from(recursive)],
        );
    }

    /// Stop watch `id` (`:stop()`), and forget it so a later re-dial doesn't resurrect it.
    pub fn unwatch(&self, id: u64) {
        self.armed.lock().unwrap().remove(&id);
        self.rpc.notify(LUAFS_UNWATCH, vec![Value::from(id)]);
    }

    /// Take the push stream — the server loop's `select!` arm feeds each event straight to
    /// the same handler the local actor's events land in. `None` after the first call.
    pub fn take_events(&self) -> Option<UnboundedReceiver<LoopEvent>> {
        self.events_rx.lock().unwrap().take()
    }

    /// Re-send every armed watch to a freshly-dialed daemon (which knows about none of
    /// them). Called from [`publish_cells`] after the new connection's cells are live; a
    /// no-op on the initial connect, where nothing is armed yet.
    fn rearm_all(&self) {
        let armed: Vec<(u64, (String, bool))> = self
            .armed
            .lock()
            .unwrap()
            .iter()
            .map(|(id, w)| (*id, w.clone()))
            .collect();
        for (id, (path, recursive)) in armed {
            self.rpc.notify(
                LUAFS_WATCH,
                vec![Value::from(id), Value::from(path), Value::from(recursive)],
            );
        }
    }
}

/// `luafs_change [id, kind, [path, …]]` → the [`LoopEvent::FsEvent`] the local watcher
/// would have produced. The wire `kind` is mapped back onto the `&'static str` change
/// classes rather than leaked as a `String`: the four classes are the whole vocabulary
/// (the peer is the same build), and an unrecognised one is surfaced as a loud watch
/// error instead of being silently coerced into `"modify"`, which would report a deletion
/// as an edit.
fn decode_luafs_change(params: Vec<Value>) -> Option<LoopEvent> {
    let id = params.first()?.as_u64()?;
    let kind = params.get(1)?.as_str()?;
    let kind = match kind {
        "create" => "create",
        "modify" => "modify",
        "remove" => "remove",
        "rename" => "rename",
        other => {
            return Some(LoopEvent::FsEvent {
                id,
                error: Some(format!(
                    "btv.fs.watch: daemon sent unknown change kind '{other}'"
                )),
                kind: None,
                paths: Vec::new(),
            })
        }
    };
    let paths = params
        .get(2)?
        .as_array()?
        .iter()
        .filter_map(|p| p.as_str().map(std::path::PathBuf::from))
        .collect();
    Some(LoopEvent::FsEvent {
        id,
        error: None,
        kind: Some(kind),
        paths,
    })
}

/// `luafs_watch_err [id, message]` → the terminal error event, identical to the one the
/// local actor emits when a watch can't arm.
fn decode_luafs_watch_err(params: Vec<Value>) -> Option<LoopEvent> {
    let id = params.first()?.as_u64()?;
    let message = params.get(1)?.as_str()?.to_string();
    Some(LoopEvent::FsEvent {
        id,
        error: Some(message),
        kind: None,
        paths: Vec::new(),
    })
}

/// The edit-host side of the `git_op` leg for a **native-daemon** session — the actor sends
/// a whole [`GitJob`](bemtvi_lua::GitJob) here and `await`s its typed result. The git twin of
/// [`RemoteFsJobs`]; `Clone` so each `btv.git` op can be driven concurrently, `Send + Sync`
/// so it rides [`ServerInit`](crate::ServerInit) onto the server thread.
#[derive(Clone)]
pub struct RemoteGitJobs {
    req_tx: UnboundedSender<GitJobReq>,
}

impl RemoteGitJobs {
    /// Connect to a daemon over `reader`/`writer` as a standalone leg (a dedicated link
    /// thread running [`run_git_jobs`]) — the git twin of [`RemoteFsJobs::connect`], for
    /// driving the `git_op` leg in isolation (tests). The multiplexed [`connect_daemon`]
    /// builds a `RemoteGitJobs` directly instead, sharing one link across all legs.
    pub fn connect<R, W>(reader: R, writer: W) -> RemoteGitJobs
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (req_tx, req_rx) = unbounded_channel::<GitJobReq>();
        spawn_leg_thread(reader, writer, move |rpc| run_git_jobs(rpc, req_rx));
        RemoteGitJobs { req_tx }
    }

    /// Send `job` to the daemon over `git_op` and `await` the typed result. Off the editor
    /// tick (the caller is the actor's async task), so a tokio await, not a thread park; a
    /// dropped link rejects loud.
    pub async fn run(
        &self,
        job: bemtvi_lua::GitJob,
    ) -> Result<bemtvi_lua::GitValue, bemtvi_lua::GitError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if self.req_tx.send((job, reply_tx)).is_err() {
            return Err(bemtvi_lua::GitError {
                code: "ENOTCONN".to_string(),
                message: "btv.git: daemon link is gone".to_string(),
            });
        }
        reply_rx.await.unwrap_or_else(|_| {
            Err(bemtvi_lua::GitError {
                code: "EIO".to_string(),
                message: "btv.git: daemon link dropped the request".to_string(),
            })
        })
    }
}

// ----- the Lua-HTTP leg (`http_op`) -----------------------------------------------
//
// The HTTP sibling of the `luafs_op` leg: how a **native-daemon** session runs `btv.http`.
// The event-loop actor hands a whole [`HttpRequest`](bemtvi_lua::HttpRequest) to
// [`RemoteHttp`], it crosses in ONE `http_op` request, and the daemon runs it through
// [`run_http_request`](crate::http::run_http_request). The wasm edit-host forwards the
// identical `http_op` request over WebTransport — one leg, one shape. Parks no thread: the
// actor `await`s the reply on the shared link runtime.

/// The `http_op` leg's job server: pull each [`HttpRequest`](bemtvi_lua::HttpRequest) off
/// `req_rx`, send it as one `http_op` request over `rpc`, decode the reply through the
/// shared [`httpwire`](bemtvi_lua) codec, and deliver the typed result to the awaiting actor.
async fn run_http_jobs(rpc: LinkRpc, mut req_rx: UnboundedReceiver<HttpJobReq>) {
    while let Some((request, reply_tx)) = req_rx.recv().await {
        let result = match rpc
            .request(HTTP_OP, vec![bemtvi_lua::http_request_to_value(&request)])
            .await
        {
            Ok(v) => bemtvi_lua::http_result_from_value(&v),
            // A transport failure (daemon gone) rejects the promise loud — never a panic.
            Err(e) => Err(bemtvi_lua::HttpError {
                message: format!("btv.http: daemon error: {e}"),
            }),
        };
        let _ = reply_tx.send(result);
    }
}

/// The edit-host side of the `http_op` leg for a **native-daemon** session — the actor sends
/// a whole [`HttpRequest`](bemtvi_lua::HttpRequest) here and `await`s its typed result. Holds a
/// tokio sender to the shared link runtime's [`run_http_jobs`]; `Clone` so each `btv.http`
/// request can be driven concurrently. The HTTP twin of [`RemoteFsJobs`].
#[derive(Clone)]
pub struct RemoteHttp {
    req_tx: UnboundedSender<HttpJobReq>,
}

impl RemoteHttp {
    /// Connect to a daemon over `reader`/`writer` as a standalone leg, spawning a dedicated
    /// link thread (its own current-thread runtime + the RPC link) that runs
    /// [`run_http_jobs`]. The multiplexed [`connect_daemon`] builds a `RemoteHttp` directly
    /// instead (sharing one link across all legs); this single-leg form drives the `http_op`
    /// leg in isolation (tests). Mirrors [`RemoteFsJobs::connect`].
    pub fn connect<R, W>(reader: R, writer: W) -> RemoteHttp
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (req_tx, req_rx) = unbounded_channel::<HttpJobReq>();
        spawn_leg_thread(reader, writer, move |rpc| run_http_jobs(rpc, req_rx));
        RemoteHttp { req_tx }
    }

    /// Send `request` to the daemon over `http_op` and `await` the typed result. Off the
    /// editor tick (the caller is the actor's async task), so this is a tokio await, not a
    /// thread park; a dropped link rejects loud.
    pub async fn run(
        &self,
        request: bemtvi_lua::HttpRequest,
    ) -> Result<bemtvi_lua::HttpResponse, bemtvi_lua::HttpError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if self.req_tx.send((request, reply_tx)).is_err() {
            return Err(bemtvi_lua::HttpError {
                message: "btv.http: daemon link is gone".to_string(),
            });
        }
        reply_rx.await.unwrap_or_else(|_| {
            Err(bemtvi_lua::HttpError {
                message: "btv.http: daemon link dropped the request".to_string(),
            })
        })
    }
}

/// Run one high-level `btv.fs` op (the `luafs_op` leg): decode the request map into an
/// [`FsJob`](bemtvi_lua::FsJob), run it through [`run_fs_job`](bemtvi_lua::run_fs_job) against
/// the daemon's `fs`, and shape the `["ok", <fs-value>] | ["err", code, message]` reply.
/// A request that doesn't decode is an `["err", "EWIRE", …]` reply (fail loud — never a
/// silent empty result). Compound ops (recursive copy/remove) decompose into local syscalls
/// inside `run_fs_job`, so this is one wire round-trip regardless of the op's fan-out.
fn serve_fs_op(fs: &dyn LuaFs, params: &[Value]) -> Value {
    let Some(req) = params.first() else {
        return bemtvi_lua::fs_result_to_value(&Err(bemtvi_lua::FsError {
            code: "EWIRE".to_string(),
            message: "luafs_op: request has no job".to_string(),
        }));
    };
    match bemtvi_lua::fs_job_from_value(req) {
        Ok(job) => bemtvi_lua::fs_result_to_value(&bemtvi_lua::run_fs_job(fs, &job)),
        Err(message) => bemtvi_lua::fs_result_to_value(&Err(bemtvi_lua::FsError {
            code: "EWIRE".to_string(),
            message,
        })),
    }
}

/// Run the daemon end of the *Lua-filesystem* wire over `reader`/`writer`, serving
/// `luafs` requests through `fs` (the daemon's real backend —
/// [`StdLuaFs`](bemtvi_lua::StdLuaFs) in the binary, a virtual fs in tests). Each op is
/// offloaded to a blocking-pool thread so a slow fs call can't stall the reader; the
/// `fs` is shared (it owns the open-fd table the `i64` tokens index, so an `fs_open`
/// here is read back by a later `fs_read`). Returns when the connection closes.
pub async fn serve_luafs_daemon<R, W>(
    reader: R,
    writer: W,
    fs: Box<dyn LuaFs + Send + Sync>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (rpc, incoming) = connect_bounded(reader, writer);
    serve_luafs_daemon_on(rpc, incoming, fs).await
}

/// The Lua-visible-fs leg's connection-agnostic core (see [`serve_proc_daemon_on`] for
/// the `*_on` split). Serves the low-level `luafs` request (one `LuaFs` op) **and** the
/// high-level `luafs_op` request (a whole [`FsJob`](bemtvi_lua::FsJob) run through
/// [`run_fs_job`](bemtvi_lua::run_fs_job) — the wasm `btv.fs` leg, Phase 2) over the shared
/// [`Rpc`] + its demuxed stream. Both share the one `StdLuaFs`, and both offload to the
/// blocking pool so a slow fs call can't stall the reader.
pub async fn serve_luafs_daemon_on(
    rpc: Rpc,
    mut incoming: Receiver<Incoming>,
    fs: Box<dyn LuaFs + Send + Sync>,
) -> anyhow::Result<()> {
    let fs: Arc<dyn LuaFs + Send + Sync> = Arc::from(fs);
    while let Some(msg) = incoming.recv().await {
        if let Incoming::Request { id, method, params } = msg {
            if method == LUAFS_OP {
                let fs = fs.clone();
                let rpc = rpc.clone();
                tokio::spawn(async move {
                    let reply =
                        match tokio::task::spawn_blocking(move || serve_fs_op(&*fs, &params)).await
                        {
                            Ok(v) => Ok(v),
                            Err(e) => Err(Value::from(format!("luafs_op: join error: {e}"))),
                        };
                    rpc.respond(id, reply);
                });
            } else {
                rpc.respond(id, Err(Value::from(format!("unknown method: {method}"))));
            }
        }
    }
    Ok(())
}

/// Run one `btv.git.*` op (the `git_op` leg): decode the request map into a
/// [`GitJob`](bemtvi_lua::GitJob), run it through `bemtvi_git::run_git_job`, and shape the
/// `["ok", <git-value>] | ["err", code, message]` reply. A request that doesn't decode is a
/// loud `["err", …]` reply, never a silent empty result. No `fs`/backend argument — the
/// executor discovers the repo from the job's path against the daemon's real disk.
fn serve_git_op(params: &[Value]) -> Value {
    let Some(req) = params.first() else {
        return bemtvi_lua::git_result_to_value(&Err(bemtvi_lua::GitError {
            code: "EWIRE".to_string(),
            message: "git_op: request has no job".to_string(),
        }));
    };
    match bemtvi_lua::git_job_from_value(req) {
        Ok(job) => bemtvi_lua::git_result_to_value(&bemtvi_git::run_git_job(&job)),
        Err(message) => bemtvi_lua::git_result_to_value(&Err(bemtvi_lua::GitError {
            code: "EWIRE".to_string(),
            message,
        })),
    }
}

/// Run the daemon end of the `btv.git` wire over `reader`/`writer` (the standalone
/// single-leg form, for tests — the multiplexed daemon fans `git_op` into
/// [`serve_git_daemon_on`] instead). The git twin of [`serve_luafs_daemon`]. Returns when
/// the connection closes.
pub async fn serve_git_daemon<R, W>(reader: R, writer: W) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (rpc, incoming) = connect_bounded(reader, writer);
    serve_git_daemon_on(rpc, incoming).await
}

/// Run the daemon end of the `btv.git` wire — serve each `git_op` request (a whole
/// [`GitJob`](bemtvi_lua::GitJob)) over the shared [`Rpc`], offloaded to the blocking pool
/// (a repo walk shouldn't stall the reader). The git twin of [`serve_luafs_daemon_on`];
/// needs no backend arg (gix discovers the repo from the path). Returns when the
/// connection closes.
pub async fn serve_git_daemon_on(
    rpc: Rpc,
    mut incoming: Receiver<Incoming>,
) -> anyhow::Result<()> {
    while let Some(msg) = incoming.recv().await {
        if let Incoming::Request { id, method, params } = msg {
            if method == GIT_OP {
                let rpc = rpc.clone();
                tokio::spawn(async move {
                    let reply =
                        match tokio::task::spawn_blocking(move || serve_git_op(&params)).await {
                            Ok(v) => Ok(v),
                            Err(e) => Err(Value::from(format!("git_op: join error: {e}"))),
                        };
                    rpc.respond(id, reply);
                });
            } else {
                rpc.respond(id, Err(Value::from(format!("unknown method: {method}"))));
            }
        }
    }
    Ok(())
}

/// Run one `btv.http.fetch` request (the `http_op` leg): decode the request map into an
/// [`HttpRequest`](bemtvi_lua::HttpRequest), run it through
/// [`run_http_request`](crate::http::run_http_request), and shape the `["ok", …] | ["err",
/// message]` reply. A request that doesn't decode is a loud `["err", …]` reply, never a
/// silent empty fetch.
fn serve_http_op(params: &[Value]) -> Value {
    let Some(req) = params.first() else {
        return bemtvi_lua::http_result_to_value(&Err(bemtvi_lua::HttpError {
            message: "http_op: request has no request".to_string(),
        }));
    };
    match bemtvi_lua::http_request_from_value(req) {
        Ok(request) => bemtvi_lua::http_result_to_value(&crate::http::run_http_request(&request)),
        Err(message) => bemtvi_lua::http_result_to_value(&Err(bemtvi_lua::HttpError { message })),
    }
}

/// Run the daemon end of the *HTTP* wire over `reader`/`writer` (the standalone
/// single-leg form, for tests — the multiplexed daemon fans `http_op` into
/// [`serve_http_daemon_on`] instead). Returns when the connection closes.
pub async fn serve_http_daemon<R, W>(reader: R, writer: W) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (rpc, incoming) = connect_bounded(reader, writer);
    serve_http_daemon_on(rpc, incoming).await
}

/// The `btv.http.fetch` leg's connection-agnostic core (the daemon side of the `http_op`
/// leg — the HTTP twin of [`serve_luafs_daemon_on`]). Serves each `http_op` request by
/// running its round-trip on a blocking-pool thread (`ureq` is blocking) so a slow request
/// can't stall the reader; the typed reply crosses back on the same `Rpc`.
pub async fn serve_http_daemon_on(
    rpc: Rpc,
    mut incoming: Receiver<Incoming>,
) -> anyhow::Result<()> {
    while let Some(msg) = incoming.recv().await {
        if let Incoming::Request { id, method, params } = msg {
            if method == HTTP_OP {
                let rpc = rpc.clone();
                tokio::spawn(async move {
                    let reply =
                        match tokio::task::spawn_blocking(move || serve_http_op(&params)).await {
                            Ok(v) => Ok(v),
                            Err(e) => Err(Value::from(format!("http_op: join error: {e}"))),
                        };
                    rpc.respond(id, reply);
                });
            } else {
                rpc.respond(id, Err(Value::from(format!("unknown method: {method}"))));
            }
        }
    }
    Ok(())
}

/// Run the daemon end of the streaming `btv.fs.watch` leg over `reader`/`writer` — the
/// single-leg form of [`serve_luafs_watch_daemon_on`], the twin of
/// [`serve_luafs_daemon`], for driving the `luafs_watch` leg in isolation (tests). The
/// real daemon multiplexes it onto the Control group instead. Returns when the connection
/// closes (dropping every watcher it armed).
pub async fn serve_luafs_watch_daemon<R, W>(reader: R, writer: W) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (rpc, incoming) = connect_bounded(reader, writer);
    serve_luafs_watch_daemon_on(rpc, incoming).await
}

/// The `btv.fs.watch` streaming leg's connection-agnostic core. Arms a recursive,
/// change-classified watch per stream `id` (reusing the event-loop actor's coalescing watcher,
/// [`start_fs_watch_coalesced`](crate::evloop::start_fs_watch_coalesced) — the same 10 ms-coalesced
/// `notify` backend the native `btv.fs.watch` rides) and pushes each batch back as `luafs_change
/// [id, kind, paths]` / a terminal `luafs_watch_err [id, message]`. The edit-host arms / disarms
/// by notification (`luafs_watch` / `luafs_unwatch`); there is no reply, so a stray request is
/// answered with an error. Watchers are kept alive in a per-`id` map (dropping one stops its
/// backend thread); the leg ends when the edit-host hangs up.
pub async fn serve_luafs_watch_daemon_on<R: IncomingStream>(
    rpc: Rpc,
    mut incoming: R,
) -> anyhow::Result<()> {
    // The coalescing watcher emits `LoopEvent::FsEvent` (the native actor's shape); we forward
    // each into RPC pushes. One shared channel for all watches — `id` tags every event.
    let (ev_tx, mut ev_rx) = unbounded_channel::<LoopEvent>();
    let mut watchers: HashMap<u64, notify::RecommendedWatcher> = HashMap::new();
    loop {
        tokio::select! {
            msg = incoming.recv() => {
                let Some(msg) = msg else { break }; // the edit-host hung up
                match msg {
                    Incoming::Notification { method, params } => match method.as_str() {
                        LUAFS_WATCH => {
                            let id = params.first().and_then(Value::as_u64).unwrap_or(0);
                            let path = params.get(1).and_then(Value::as_str).unwrap_or("").to_string();
                            let recursive = params.get(2).and_then(Value::as_bool).unwrap_or(false);
                            match crate::evloop::start_fs_watch_coalesced(
                                id, &path, recursive, ev_tx.clone(),
                            ) {
                                Ok(w) => { watchers.insert(id, w); }
                                // Arm failure (bad path / watch limit) is terminal for this
                                // stream — push it loud, exactly as the native arm rejects.
                                Err(e) => rpc.notify(
                                    LUAFS_WATCH_ERR,
                                    vec![Value::from(id), Value::from(e.to_string())],
                                ),
                            }
                        }
                        // Dropping the watcher stops its backend thread (and the coalescing task).
                        LUAFS_UNWATCH => {
                            let id = params.first().and_then(Value::as_u64).unwrap_or(0);
                            watchers.remove(&id);
                        }
                        _ => {}
                    },
                    // The leg speaks only notifications; a request is a protocol error.
                    Incoming::Request { id, .. } => rpc.respond(
                        id,
                        Err(Value::from("luafs_watch leg takes notifications, not requests")),
                    ),
                }
            }
            Some(ev) = ev_rx.recv() => {
                if let LoopEvent::FsEvent { id, error, kind, paths } = ev {
                    match error {
                        Some(msg) => rpc.notify(
                            LUAFS_WATCH_ERR,
                            vec![Value::from(id), Value::from(msg)],
                        ),
                        None => {
                            let plist = paths
                                .into_iter()
                                .map(|p| Value::from(p.to_string_lossy().into_owned()))
                                .collect();
                            rpc.notify(
                                LUAFS_CHANGE,
                                vec![
                                    Value::from(id),
                                    Value::from(kind.unwrap_or("modify")),
                                    Value::Array(plist),
                                ],
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

// ============================================================================
// The config leg (daemon side)
// ============================================================================

/// Run the daemon end of the *config* wire over `reader`/`writer`, answering
/// `config_bundle` requests with this machine's config surface (see [`CONFIG_BUNDLE`]).
/// Returns when the connection closes. The per-leg wrapper the tests drive over a
/// private duplex; the real binary routes the `config_` namespace into
/// [`serve_config_daemon_on`] through the [`run_daemon_io`](crate::run_daemon_io) mux.
pub async fn serve_config_daemon<R, W>(reader: R, writer: W) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (rpc, incoming) = connect_bounded(reader, writer);
    serve_config_daemon_on(rpc, incoming).await
}

/// The config leg's connection-agnostic core: answer each `config_bundle` request by
/// walking the daemon's config tree ([`crate::collect_config_bundle`]) and replying with
/// the encoded bundle, or a loud error. The leg carries no notifications and holds no
/// state, so it is a plain request loop (unlike the stateful `fs_*`/`proc_*` legs).
pub async fn serve_config_daemon_on(
    rpc: Rpc,
    mut incoming: Receiver<Incoming>,
) -> anyhow::Result<()> {
    while let Some(msg) = incoming.recv().await {
        if let Incoming::Request {
            id, method, params, ..
        } = msg
        {
            let reply = match method.as_str() {
                CONFIG_BUNDLE => serve_config_bundle(&params),
                other => Err(Value::from(format!("unknown method: {other}"))),
            };
            rpc.respond(id, reply);
        }
        // The config leg has no notifications; ignore anything else.
    }
    Ok(())
}

/// Walk the daemon's config surface and project it onto the `config_bundle` wire shape,
/// or a loud error reply (a failed walk is never a silently-empty bundle). `params` is
/// `[include_files]`: a **local-config** edit-host passes `false` to skip the file walk
/// (it runs its own config, wanting only the daemon's cwd / parser set); a remote-config
/// session — or an older peer that sends no arg — gets the full tree (`true`).
fn serve_config_bundle(params: &[Value]) -> Result<Value, Value> {
    let include_files = params.first().and_then(Value::as_bool).unwrap_or(true);
    match crate::collect_config_bundle(include_files) {
        Ok((config_dir, runtimepath, files, ts_languages, state_dir)) => Ok(encode_config_bundle(
            config_dir,
            runtimepath,
            files,
            ts_languages,
            // The daemon's process cwd, to seed the edit-host's `DirState` so a remote
            // session's `:pwd` / `getcwd` / `:cd` operate on the daemon's directory
            // (`docs/plans/2026-06-23-remote-cwd.md`). `None` if it can't be read — the
            // edit-host then falls back to its own local cwd.
            std::env::current_dir().ok(),
            // The daemon's shada base dir, where a `Remote`-config session stages + syncs
            // its shada over the fs seam (the daemon itself runs no shada logic).
            state_dir,
            // The daemon's home dir, so a leading `~` in a file argument (`:e ~/x`)
            // expands against the daemon's `$HOME` — where the file read lands.
            std::env::var_os("HOME").map(PathBuf::from),
        )),
        Err(e) => Err(Value::from(format!("config_bundle: {e}"))),
    }
}

/// `[config_dir?, [runtimepath…], [[abspath, bytes], …], [ts_lang…], cwd?, state_dir?,
/// home?]` — the bundle on the wire ([`decode_config_bundle`] is the inverse). Paths are
/// the daemon's absolute paths; the edit-host rebases the config roots onto its local
/// cache. `cwd` is the daemon's working directory, `state_dir` its shada base dir, and
/// `home` its `$HOME` (the base a leading `~` in a file argument expands against) — all
/// trailing fields an older peer omits (→ the edit-host keeps its local cwd / has no
/// remote shada / expands `~` against its own `$HOME`).
fn encode_config_bundle(
    config_dir: Option<PathBuf>,
    runtimepath: Vec<PathBuf>,
    files: Vec<(PathBuf, Vec<u8>)>,
    ts_languages: Vec<String>,
    cwd: Option<PathBuf>,
    state_dir: PathBuf,
    home: Option<PathBuf>,
) -> Value {
    let path_str = |p: PathBuf| Value::from(p.to_string_lossy().into_owned());
    Value::Array(vec![
        config_dir.map_or(Value::Nil, &path_str),
        Value::Array(runtimepath.into_iter().map(&path_str).collect()),
        Value::Array(
            files
                .into_iter()
                .map(|(p, bytes)| Value::Array(vec![path_str(p), Value::Binary(bytes)]))
                .collect(),
        ),
        Value::Array(ts_languages.into_iter().map(Value::from).collect()),
        cwd.map_or(Value::Nil, &path_str),
        path_str(state_dir),
        home.map_or(Value::Nil, &path_str),
    ])
}

// ============================================================================
// The edit-host-side multiplexer
// ============================================================================
//
// Each `Remote*::connect` above opens its own connection — fine for the per-leg tests,
// where each leg gets a private duplex, but the real edit-host talks to *one* daemon
// over *one* transport. `connect_daemon` is the symmetric counterpart of the daemon's
// `run_daemon_io` multiplexer: it `connect`s once and hands back all four seams sharing
// that single link, so one `ServerInit` populates `host_fs_async` / `host_proc` /
// `lsp_transport` / `fs_jobs` from a single `--daemon` child.
//
// Two properties make this a clean router, not a rework (both verified in the code):
// the daemon→edit-host *notifications* split into disjoint method namespaces
// (`proc_spawned`/`proc_exited`, `fs_changed`, `lsp_stdout`/`lsp_stderr`/`lsp_exited`),
// and request *responses* (`fs_read`/`fs_write`/`luafs_op`) are msgid-routed
// *inside* [`Rpc`] and never surface as an [`Incoming`] — so one demux over the shared
// `incoming` covers every leg, and concurrent writes from all legs serialize through
// `Rpc`'s single out-channel.

/// The edit-host side of the config leg: a [`Rpc`] handle that fetches the daemon's
/// config surface with one `config_bundle` request. Shares the single daemon link like
/// the other seams (see [`connect_daemon`]); [`RemoteConfig::connect`] is the per-leg
/// constructor the tests drive over a private duplex.
pub struct RemoteConfig {
    rpc: LinkRpc,
}

impl RemoteConfig {
    /// Connect to a daemon's config leg over `reader`/`writer` (its own link — the
    /// per-leg path the tests use; the real edit-host builds [`RemoteConfig`] inline in
    /// [`serve_daemon_link`] over the shared link). The leg carries no notifications,
    /// but the inbound stream must still be drained or the reader backpressures —
    /// dropping it would tear the connection down — so a task drains it to EOF.
    pub fn connect<R, W>(reader: R, writer: W) -> RemoteConfig
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (rpc, mut incoming) = connect_bounded(reader, writer);
        tokio::spawn(async move { while incoming.recv().await.is_some() {} });
        RemoteConfig {
            rpc: LinkRpc::fixed(rpc),
        }
    }

    /// Fetch the daemon's config surface (one `config_bundle` round trip). `include_files`
    /// asks the daemon to walk + ship its config tree (a remote-config session); `false`
    /// is the lite fetch (a local-config session — just the daemon's cwd / parser set). A
    /// transport failure or a malformed reply is a loud error — never a silently-empty
    /// bundle that would look like "the remote has no config".
    pub async fn fetch(&self, include_files: bool) -> io::Result<RemoteConfigBundle> {
        match self
            .rpc
            .request(CONFIG_BUNDLE, vec![Value::from(include_files)])
            .await
        {
            // The decode (shape validation) lives in `remote_config` so the wasm edit-host
            // shares it; a mismatch is a loud `InvalidData` error here.
            Ok(v) => {
                decode_config_bundle(v).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            }
            Err(e) => Err(io::Error::other(e.to_string())),
        }
    }

    /// Resolve the session's config from a [`ConfigSource`](crate::ConfigSource) — the
    /// shared path both native clients (TUI / GUI) take at connect, so the
    /// fetch-vs-lite + materialize-vs-local decision lives in one place.
    ///
    /// - `Remote`: fetch the full bundle and materialize the daemon's config + plugins
    ///   onto a local cache ([`materialize_remote_config`](crate::materialize_remote_config)).
    /// - `Local`: a lite fetch (cwd / parser set only) and the client's own
    ///   [`default_runtime`](crate::default_runtime) — the buffers / fs still live on the
    ///   daemon, so the cwd is seeded in both modes for relative-path resolution.
    ///
    /// A transport failure or an unstageable cache is loud (a session cannot run a config
    /// it could not resolve), never a silent fall back.
    pub async fn resolve(&self, source: crate::ConfigSource) -> io::Result<crate::ResolvedConfig> {
        let bundle = self
            .fetch(matches!(source, crate::ConfigSource::Remote))
            .await?;
        let remote_cwd = bundle.cwd.clone().map(std::path::PathBuf::from);
        let remote_home = bundle.home.clone().map(std::path::PathBuf::from);
        let ts_autoinstall = bundle.ts_languages.clone();
        let state_dir = bundle.state_dir.clone();
        let (config_dir, runtimepath) = match source {
            crate::ConfigSource::Remote => crate::materialize_remote_config(bundle)
                .map_err(|e| io::Error::other(format!("materialize remote config: {e}")))?,
            crate::ConfigSource::Local => crate::default_runtime(),
        };
        Ok(crate::ResolvedConfig {
            config_dir,
            runtimepath,
            remote_cwd,
            remote_home,
            ts_autoinstall,
            state_dir,
        })
    }
}

/// The five edit-host seams of one daemon connection, all sharing a single link (see
/// [`connect_daemon`]). Each field drops straight into the matching
/// [`ServerInit`](crate::ServerInit) slot — except [`config`](Self::config), which the
/// session fetches *before* building `ServerInit` to derive the local config roots.
pub struct DaemonClient {
    /// The async filesystem seam (`fs_read`/`fs_write`) + the `fs_changed` watch push.
    pub host_fs: RemoteHostFs,
    /// The event-routed process seam (the async `vim.system` / `jobstart` / `:!`).
    pub host_proc: RemoteHostProc,
    /// The streaming-pipe LSP seam (`lsp_*`).
    pub lsp_transport: RemoteLspTransport,
    /// The terminal seam (`term_*`): `:terminal` opens a PTY on the **daemon** and streams
    /// its output back, so a remote session's terminal runs where the files are — not on the
    /// local machine.
    pub host_term: RemoteHostTerm,
    /// The async `btv.fs` seam (`luafs_op`) — whole-job, decomposed daemon-side. The
    /// event-loop actor `await`s it off the editor tick (no thread park).
    pub fs_jobs: RemoteFsJobs,
    /// The streaming `btv.fs.watch` seam (`luafs_watch`) — recursive watches armed on the
    /// daemon, whose coalesced change batches push back as [`LoopEvent::FsEvent`]s. Without
    /// it a daemon session watched the *local* disk, so nothing built on `btv.fs.watch`
    /// (the LSP file-watch client included) saw a remote change.
    pub fs_watch: RemoteFsWatch,
    /// The async `btv.git` seam (`git_op`) — the whole op runs daemon-side (git runs where
    /// the files are). The event-loop actor `await`s it off the editor tick.
    pub git_jobs: RemoteGitJobs,
    /// The async `btv.http` seam (`http_op`) — the whole request runs on the daemon (which
    /// owns the network). The event-loop actor `await`s it off the editor tick.
    pub http: RemoteHttp,
    /// The config seam (`config_bundle`) — fetched once at session start to mirror the
    /// daemon's config + plugins onto a local cache (Phase 2).
    pub config: RemoteConfig,
}

/// Connect to a single daemon over `reader`/`writer` and return all four edit-host
/// seams sharing that one link — the edit-host-side multiplexer (the symmetric twin of
/// the daemon's [`run_daemon_io`](crate::run_daemon_io)). The transport is any
/// [`AsyncRead`]/[`AsyncWrite`] pair: the real `--daemon` binary's stdio (how
/// `daemon_stdio.rs` drives it), an in-process duplex, or the QUIC stream of the future
/// listener.
///
/// **Why a dedicated link thread.** The connection runs on its *own* OS thread + a
/// current-thread runtime — not the server runtime — so the wire I/O is driven off the
/// server's thread. On this one shared thread we run the [`run_fs_jobs`] job server (the
/// `btv.fs` `luafs_op` leg) and the single [`run_client_demux`] that fans every
/// daemon→edit-host notification to the right leg. Every seam
/// (`host_fs`/`host_proc`/`lsp_transport`/`fs_jobs`) holds a clone of the shared [`Rpc`]
/// (or a channel to a job server on this thread) and issues its requests from the server
/// runtime; the actual wire I/O always happens here. No leg parks the editor thread —
/// `btv.fs` is `await`ed off the tick by the event-loop actor, the async legs are
/// fire-and-forget request/response.
pub fn connect_daemon<R, W>(reader: R, writer: W) -> DaemonClient
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // The link thread builds the seams (it owns the `Rpc` the async legs clone) and
    // hands the `DaemonClient` back out; a `std` channel lets a non-async caller block
    // briefly for it. Everything in `DaemonClient` is `Send`.
    let (client_tx, client_rx) = std::sync::mpsc::channel::<DaemonClient>();

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            // A runtime we can't build leaves `client_tx` dropped; the caller's `recv`
            // errors and `connect_daemon` fails loud (a basic OS-capability failure, not
            // a recoverable daemon condition).
            Err(_) => return,
        };
        rt.block_on(async move {
            let (rpc, incoming) = connect_bounded(reader, writer);
            serve_daemon_link(rpc, incoming, client_tx).await;
        });
    });

    client_rx
        .recv()
        .expect("connect_daemon: the link thread could not build a tokio runtime")
}

/// One leg group's link to the daemon: the [`Rpc`] its seams issue requests/notifies on,
/// and the inbound notification stream the daemon pushes back on. A multi-stream transport
/// (QUIC/WebTransport) gives each group its **own** `(Rpc, incoming)` from a dedicated
/// stream; a single-stream transport (ssh/stdio) clones one `Rpc` across the three and
/// splits the one inbound stream into per-group `incoming`s by method
/// ([`serve_daemon_link`]). Either way the seams and the per-group demuxes are identical.
pub(crate) struct GroupLink {
    pub(crate) rpc: Rpc,
    pub(crate) incoming: Receiver<Incoming>,
}

/// Drive a **single-stream** link (ssh/stdio, the in-process test duplex): every leg group
/// shares the one `(rpc, incoming)`, so split the one inbound notification stream into the
/// four logical groups by method and hand them to the same [`serve_daemon_link_inner`] the
/// multi-stream (QUIC) path uses, with the one `Rpc` cloned across the groups. The
/// transport-agnostic seam construction lives in `serve_daemon_link_inner`.
pub(crate) async fn serve_daemon_link(
    rpc: Rpc,
    incoming: Receiver<Incoming>,
    client_tx: std::sync::mpsc::Sender<DaemonClient>,
) {
    serve_daemon_link_inner(split_groups(rpc, incoming), client_tx).await;
}

/// Fan a single-stream link's one inbound stream into the four per-group channels by
/// method (the symmetric twin of the daemon-side `DaemonLegs` demux). An unknown method
/// drops (the peer is the same build). On EOF the four senders drop, closing the per-group
/// demuxes downstream.
async fn split_incoming(
    mut incoming: Receiver<Incoming>,
    ctrl_tx: Sender<Incoming>,
    proc_tx: Sender<Incoming>,
    lsp_tx: Sender<Incoming>,
    term_tx: Sender<Incoming>,
) {
    while let Some(msg) = incoming.recv().await {
        let method = match &msg {
            Incoming::Request { method, .. } | Incoming::Notification { method, .. } => {
                method.as_str()
            }
        };
        let tx = match LegGroup::classify(method) {
            Some(LegGroup::Control) => &ctrl_tx,
            Some(LegGroup::Proc) => &proc_tx,
            Some(LegGroup::Lsp) => &lsp_tx,
            Some(LegGroup::Term) => &term_tx,
            None => continue, // unknown method (same build) — drop
        };
        if tx.send(msg).await.is_err() {
            break; // the group's demux is gone — the link is going away
        }
    }
}

/// Build the six edit-host seams over the four per-group links, hand the
/// [`DaemonClient`] out on `client_tx`, then drive the link — the [`run_fs_jobs`] job
/// server (the `btv.fs` `luafs_op` leg, on the **Control** group) plus one demux per group —
/// until the daemon hangs up. The transport-agnostic heart shared by [`serve_daemon_link`]
/// (single-stream) and the QUIC connector ([`crate::quic::connect_quic`], one real stream
/// per group). Each seam issues on its group's `Rpc`: `host_fs`/`fs_jobs`/`config` on
/// Control, `host_proc` on Proc, `lsp_transport` on Lsp, `host_term` on Term — so a flood on
/// one group's stream can't head-of-line-block another. Runs on the dedicated link thread's
/// runtime. **One-shot**: it serves a single connection and returns when it drops (the link
/// thread winds down) — the reconnecting path ([`connect_daemon_reconnecting_on`]) reuses
/// the same seam construction ([`build_link`]) and per-connection demuxes ([`run_connection`])
/// but loops over re-dials instead.
pub(crate) async fn serve_daemon_link_inner(
    conn: DialedConnection,
    client_tx: std::sync::mpsc::Sender<DaemonClient>,
) {
    let (mut state, client) = build_link();
    // The `btv.fs` (`luafs_op`) / `btv.http` (`http_op`) job servers ride the swappable Control
    // cell (a one-shot link never re-dials, but the wiring is shared with the reconnecting path).
    tokio::spawn(run_fs_jobs(
        state.control_rpc.clone(),
        state.take_fs_jobs_rx(),
    ));
    tokio::spawn(run_http_jobs(
        state.control_rpc.clone(),
        state.take_http_jobs_rx(),
    ));
    tokio::spawn(run_git_jobs(
        state.control_rpc.clone(),
        state.take_git_jobs_rx(),
    ));
    // Publish the connection's `Rpc`s into the cells *before* handing the client out — the
    // same ordering the reconnecting dialer needs, and for the same reason. The caller
    // issues its first seam op (the config-resolve round trip) the instant `connect_daemon`
    // returns, but the publish runs on *this* thread: send the client first and the two
    // race, so a caller that gets going before this thread is next scheduled finds an empty
    // cell and fails loud with "daemon disconnected" — surfacing as an intermittent
    // `could not resolve the session config from the daemon` at session startup. Idempotent
    // with `run_connection`'s re-publish.
    publish_cells(&state, &conn);
    // Hand the seams out before serving; if the caller already dropped, there's nothing
    // to drive.
    if client_tx.send(client).is_err() {
        return;
    }
    // Serve this single connection until every group's stream EOFs, then return.
    run_connection(&state, conn).await;
    clear_cells(&state);
}

/// The **Control** group's inbound demux: `fs_changed` to the watch channel. Request
/// *responses* (`fs_read`/`fs_write`/`luafs_op`/`config_bundle`) never arrive here — [`Rpc`]
/// msgid-routes them internally. `luafs_change`/`luafs_watch_err` have no native consumer
/// and drop. On EOF, dropping `watch_tx` ends the server's watch arm.
async fn run_control_demux(
    mut incoming: Receiver<Incoming>,
    watch_tx: UnboundedSender<WatchEvent>,
    fs_watch_tx: UnboundedSender<LoopEvent>,
) {
    while let Some(msg) = incoming.recv().await {
        let Incoming::Notification { method, params } = msg else {
            continue; // the daemon speaks only notifications; ignore stray requests
        };
        // Two watch legs land here, and they are NOT the same thing: `fs_changed` is the
        // per-buffer stat-poll the editor reconciles (`:checktime`), `luafs_change` is a
        // streaming `btv.fs.watch` batch keyed by stream id.
        match method.as_str() {
            FS_CHANGED => {
                if let Some(ev) = decode_fs_changed(params) {
                    // The server may not have taken the watch receiver yet at startup; a
                    // send that finds no receiver is harmlessly dropped.
                    let _ = watch_tx.send(ev);
                }
            }
            LUAFS_CHANGE => {
                if let Some(ev) = decode_luafs_change(params) {
                    let _ = fs_watch_tx.send(ev);
                }
            }
            LUAFS_WATCH_ERR => {
                if let Some(ev) = decode_luafs_watch_err(params) {
                    let _ = fs_watch_tx.send(ev);
                }
            }
            _ => {}
        }
    }
}
