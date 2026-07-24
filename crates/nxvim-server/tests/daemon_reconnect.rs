//! The **reconnectable daemon link** (`docs/plans/2026-06-29-daemon-reconnect.md`).
//!
//! The editor runs *local*; the daemon only provides the fs/proc/lsp/term seams. So a
//! dropped connection must NOT tear the session down — the link re-dials underneath the seam
//! handles the editor already holds, and the local buffers/undo survive.
//!
//! These drive the real thing: an editor whose async fs is the reconnecting
//! [`RemoteHostFs`](nxvim_server::RemoteHostFs) built by
//! [`connect_daemon_reconnecting_on`](nxvim_server::connect_daemon_reconnecting_on), talking
//! to an `fs` daemon over an in-process duplex the test can **sever** (abort the daemon
//! task) and **fail** (refuse a re-dial). The contract proven across the phases:
//!
//! - Phase 1 — a dropped link re-dials underneath the seams; a save during the outage fails
//!   loud (never reaches the remote) while the local edit survives, and the *same* seam
//!   resumes after the re-dial.
//! - Phase 2 — a drop is **auto-retried** with bounded backoff (no manual action); when the
//!   retry budget is exhausted the link parks [`Disconnected`](nxvim_server::DaemonStatus)
//!   until a manual [`reconnect`](nxvim_server::ReconnectHandle::reconnect); and
//!   [`disconnect`](nxvim_server::ReconnectHandle::disconnect) drops a live link on demand.
//! - Phase 6 — **state re-sync**: a file changed on the remote *while the link was down* is
//!   caught when it comes back (the resync re-arms each watch carrying the editor's disk
//!   baseline, so the fresh daemon detects + pushes the drift and the unmodified buffer
//!   autoreloads), rather than being silently adopted as the new normal.
//!
//! Hermetic: a real server over the in-process RPC pipe, an in-memory remote fs, no disk.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nxvim_server::{
    DaemonClient, DaemonStatus, FsRead, HostFsAsync, ReconnectHandle, ReconnectPolicy, ServerInit,
};
use nxvim_test_harness::{attach, await_lines, buf_lines, exec_lua, feed, spawn, DaemonFs};
use tokio::io::{DuplexStream, ReadHalf, WriteHalf};
use tokio::task::JoinHandle;

/// The future one re-dial produces: the edit-host end of a fresh duplex (or a refusal).
type DialFut =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<DialEnds>> + Send>>;
/// The edit-host reader/writer halves a successful dial hands back.
type DialEnds = (ReadHalf<DuplexStream>, WriteHalf<DuplexStream>);

/// Test control over the re-dial factory: the shared remote fs, the spawned daemon tasks
/// (newest last — the test severs by aborting the most recent), and a `fail` switch that
/// makes the *next* dial(s) refuse (to exercise the retry budget and the give-up park).
#[derive(Clone)]
struct Dialer {
    fake: DaemonFs,
    daemons: Arc<Mutex<Vec<JoinHandle<()>>>>,
    fail: Arc<AtomicBool>,
}

impl Dialer {
    fn new(fake: DaemonFs) -> Self {
        Dialer {
            fake,
            daemons: Arc::new(Mutex::new(Vec::new())),
            fail: Arc::new(AtomicBool::new(false)),
        }
    }

    fn set_fail(&self, fail: bool) {
        self.fail.store(fail, Ordering::SeqCst);
    }

    /// Abort the live daemon — the edit-host sees EOF and the supervisor reacts as if the
    /// network dropped.
    fn sever(&self) {
        if let Some(h) = self.daemons.lock().unwrap().last() {
            h.abort();
        }
    }

    /// The `make` factory passed to `connect_daemon_reconnecting_on`: each (re)dial stands up
    /// a fresh `fs` daemon over a new duplex against the *same* remote fs (unless `fail` is
    /// set, in which case it refuses), and registers the daemon task so the test can sever it.
    fn make(&self) -> impl FnMut() -> DialFut + Send + 'static {
        let this = self.clone();
        move || {
            let this = this.clone();
            Box::pin(async move {
                if this.fail.load(Ordering::SeqCst) {
                    return Err(io::Error::other("dial refused (test)").into());
                }
                let (eh_end, daemon_end) = tokio::io::duplex(1 << 16);
                let (dr, dw) = tokio::io::split(daemon_end);
                let fake = this.fake.clone();
                let h = tokio::spawn(async move {
                    let _ = nxvim_server::serve_fs_daemon(dr, dw, Box::new(fake)).await;
                });
                this.daemons.lock().unwrap().push(h);
                let (er, ew) = tokio::io::split(eh_end);
                Ok((er, ew))
            })
        }
    }
}

/// A snappy retry policy so the budget exhausts in a fraction of a second under test
/// (production uses [`ReconnectPolicy::default`]: 5 attempts over 0.5 → 8 s).
fn fast_policy() -> ReconnectPolicy {
    ReconnectPolicy {
        max_attempts: 5,
        base: Duration::from_millis(20),
        cap: Duration::from_millis(60),
    }
}

/// Whether the current buffer reports `modified`.
async fn modified(rpc: &nxvim_rpc::Rpc) -> bool {
    exec_lua(rpc, "return vim.bo.modified").await.as_bool() == Some(true)
}

/// Replace the whole (single-line) buffer with `text` via real keystrokes, leaving it
/// modified.
fn set_line(rpc: &nxvim_rpc::Rpc, text: &str) {
    feed(rpc, &format!("ggcc{text}<Esc>"));
}

/// Re-issue `:w` until the remote holds `want` (the re-dial is async, so the first save may
/// briefly still find an empty cell), or the budget runs out. Returns whether it landed.
async fn save_until(rpc: &nxvim_rpc::Rpc, fake: &DaemonFs, path: &str, want: &str) -> bool {
    for _ in 0..200 {
        if fake.content(path).as_deref() == Some(want) {
            return true;
        }
        feed(rpc, ":w<CR>");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    fake.content(path).as_deref() == Some(want)
}

/// Drive an editor over a reconnecting fs link against `dialer`, opening `path`. Returns the
/// client RPC + its (kept) notification receiver and the [`ReconnectHandle`].
async fn start(
    dialer: &Dialer,
    path: &str,
) -> (
    nxvim_rpc::Rpc,
    tokio::sync::mpsc::UnboundedReceiver<nxvim_rpc::Incoming>,
    ReconnectHandle,
) {
    let (client, handle) =
        nxvim_server::connect_daemon_reconnecting_on(dialer.make(), fast_policy())
            .await
            .expect("the initial daemon dial succeeds");
    let DaemonClient { host_fs, .. } = client;
    let init = ServerInit {
        file: Some(path.to_string()),
        host_fs_async: Some(Box::new(host_fs)),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming, handle)
}

/// The current `nx.daemon.status()` phase, or `None`.
async fn status(rpc: &nxvim_rpc::Rpc) -> Option<String> {
    exec_lua(rpc, "return nx.daemon.status()")
        .await
        .as_str()
        .map(str::to_string)
}

/// Poll `nx.daemon.status()` until it reads `want`.
async fn await_status(rpc: &nxvim_rpc::Rpc, want: &str) {
    for _ in 0..200 {
        if status(rpc).await.as_deref() == Some(want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        status(rpc).await.as_deref(),
        Some(want),
        "nx.daemon.status() never reached {want:?}"
    );
}

/// Editor integration (Phase 3): the link status is mirrored to `nx.daemon.status()`, a
/// `User DaemonStatusChanged` autocmd fires on each change, and `:reconnect` / `:disconnect`
/// drive the link from the command line.
#[tokio::test]
async fn status_mirror_event_and_commands_drive_the_link() {
    const PATH: &str = "/virtual/note.txt";
    let dialer = Dialer::new(DaemonFs::with(PATH, "one\n"));
    let (client, handle) =
        nxvim_server::connect_daemon_reconnecting_on(dialer.make(), fast_policy())
            .await
            .expect("the initial daemon dial succeeds");
    let DaemonClient { host_fs, .. } = client;
    let init = ServerInit {
        file: Some(PATH.to_string()),
        host_fs_async: Some(Box::new(host_fs)),
        // Phase 3: hand the link to the editor so status flows into Lua and the commands work.
        daemon_link: Some(handle),
        ..Default::default()
    };
    let (rpc, _incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    await_lines(&rpc, &["one"]).await;

    // Count `User DaemonStatusChanged` firings, so we can prove the event reaches plugins.
    exec_lua(
        &rpc,
        "_G.dsc = 0; nx.autocmd.create('User', { pattern = 'DaemonStatusChanged', \
         callback = function() _G.dsc = _G.dsc + 1 end })",
    )
    .await;

    // The initial status is mirrored from the very first frame.
    assert_eq!(
        status(&rpc).await.as_deref(),
        Some("connected"),
        "a fresh daemon session reports connected"
    );

    // Make re-dials fail and sever: the supervisor burns its budget and parks Disconnected,
    // and the editor reflects that phase into Lua.
    dialer.set_fail(true);
    dialer.sever();
    await_status(&rpc, "disconnected").await;
    assert!(
        exec_lua(&rpc, "return _G.dsc").await.as_i64().unwrap_or(0) >= 1,
        "the User DaemonStatusChanged autocmd fired on the status change"
    );

    // `:reconnect` (with the dialer healed) restores the link from the command line.
    dialer.set_fail(false);
    feed(&rpc, ":reconnect<CR>");
    await_status(&rpc, "connected").await;

    // `:disconnect` drops it again on demand.
    feed(&rpc, ":disconnect<CR>");
    await_status(&rpc, "disconnected").await;
}

/// Regression: the seams must be live the *instant* `connect_daemon_reconnecting_on` returns.
///
/// The reconnect supervisor runs as a *spawned* task, and it is what fills the swappable link
/// cells with the live connection's `Rpc`s (inside `run_connection`). If that is the *only*
/// place the cells are published, the caller's first seam op races the supervisor: the client
/// is handed back before the spawned task is ever polled, so the cell is still empty and the op
/// fails loud with "daemon disconnected". In production that first op is the config-resolve
/// round trip a fresh `--daemon --listen` connect issues at startup, surfaced to the user as the
/// intermittent `could not resolve the session config from the daemon: daemon disconnected`. The
/// fix publishes the cells *before* returning the client.
///
/// This reproduces it deterministically: issue an fs op the moment the connect returns, with no
/// intervening `.await` that could yield to the supervisor. The empty-cell path returns `Err`
/// synchronously (it never yields), so pre-fix the op fails without the wire ever being reached —
/// a determinstic failure, not a timing race — while post-fix it reaches the live remote.
#[tokio::test]
async fn seams_are_live_the_instant_connect_returns() {
    const PATH: &str = "/virtual/note.txt";
    let dialer = Dialer::new(DaemonFs::with(PATH, "hello world\n"));
    let (client, _handle) =
        nxvim_server::connect_daemon_reconnecting_on(dialer.make(), fast_policy())
            .await
            .expect("the initial daemon dial succeeds");

    // The first seam op after connect — nothing between the return above and this call yields to
    // the runtime, so the spawned supervisor has had no chance to publish the cells. It must
    // still reach the (live) remote file rather than fail "daemon disconnected".
    match client.host_fs.read(PATH.to_string()).await {
        Ok(FsRead::File(bytes, _)) => assert_eq!(
            String::from_utf8_lossy(&bytes),
            "hello world\n",
            "the first seam op reaches the live remote file"
        ),
        Ok(_) => panic!("expected the remote file bytes from the first seam op"),
        Err(e) => panic!(
            "the first seam op after connect failed instead of reaching the live remote: {e}"
        ),
    }
}

/// A dropped link is **auto-retried** and recovers with no manual action — the headline
/// reliability behavior. The save resumes by itself, and the local edit survives.
#[tokio::test]
async fn auto_retry_recovers_a_dropped_link_without_manual_reconnect() {
    const PATH: &str = "/virtual/note.txt";
    let dialer = Dialer::new(DaemonFs::with(PATH, "one\n"));
    let (rpc, _incoming, _handle) = start(&dialer, PATH).await;

    await_lines(&rpc, &["one"]).await;
    set_line(&rpc, "two");
    feed(&rpc, ":w<CR>");
    assert!(
        save_until(&rpc, &dialer.fake, PATH, "two\n").await,
        "while connected, `:w` writes across the wire"
    );

    // Sever the live link. The factory still succeeds, so the supervisor's auto-retry
    // re-dials a fresh daemon over the same remote fs — without any `reconnect()` call.
    dialer.sever();
    set_line(&rpc, "three");
    assert_eq!(
        buf_lines(&rpc, 0).await,
        vec!["three"],
        "the local edit survives the dropped connection"
    );
    assert!(
        save_until(&rpc, &dialer.fake, PATH, "three\n").await,
        "the link auto-recovers and the save resumes with no manual reconnect"
    );
}

/// When the auto-retry budget is exhausted (every re-dial refused), the link parks
/// `Disconnected` and a save stays loudly failed — until a manual `:reconnect` (with the
/// dialer healthy again) restores it. Proves the bounded-retry + give-up + manual-recovery
/// contract and the status transitions.
#[tokio::test]
async fn exhausting_the_retry_budget_parks_disconnected_until_reconnect() {
    const PATH: &str = "/virtual/note.txt";
    let dialer = Dialer::new(DaemonFs::with(PATH, "one\n"));
    let (rpc, _incoming, handle) = start(&dialer, PATH).await;

    // Record every status the supervisor publishes, so we can assert the transitions.
    let seen = Arc::new(Mutex::new(vec![handle.status()]));
    {
        let seen = seen.clone();
        let mut rx = handle.subscribe();
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                seen.lock().unwrap().push(*rx.borrow());
            }
        });
    }

    await_lines(&rpc, &["one"]).await;
    set_line(&rpc, "two");
    feed(&rpc, ":w<CR>");
    assert!(save_until(&rpc, &dialer.fake, PATH, "two\n").await);

    // Make every re-dial refuse, then sever: the supervisor burns its whole budget and gives
    // up. A save during the outage can never reach the remote (every dial fails), so the
    // remote stays "two\n" — a deterministic assertion, not a timing race.
    dialer.set_fail(true);
    dialer.sever();
    set_line(&rpc, "three");
    feed(&rpc, ":w<CR>");

    // Wait for the supervisor to give up (status → Disconnected).
    let mut parked = false;
    for _ in 0..200 {
        if handle.status() == DaemonStatus::Disconnected {
            parked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        parked,
        "the link parks Disconnected once the retry budget is spent"
    );
    assert_eq!(
        dialer.fake.content(PATH).as_deref(),
        Some("two\n"),
        "no save reached the remote while the link was down"
    );
    assert!(
        modified(&rpc).await,
        "the failed save leaves the buffer modified"
    );

    // The status stream passed through Reconnecting before parking Disconnected.
    {
        let seen = seen.lock().unwrap();
        assert!(
            seen.iter()
                .any(|s| matches!(s, DaemonStatus::Reconnecting { .. })),
            "the supervisor reports Reconnecting while it auto-retries: {seen:?}"
        );
        assert!(
            seen.contains(&DaemonStatus::Disconnected),
            "the supervisor reports Disconnected on giving up: {seen:?}"
        );
    }

    // Heal the dialer and reconnect manually: the same seam resumes and the save lands.
    dialer.set_fail(false);
    handle.reconnect();
    assert!(
        save_until(&rpc, &dialer.fake, PATH, "three\n").await,
        "a manual `:reconnect` after giving up restores the link"
    );
    assert!(
        seen.lock().unwrap().contains(&DaemonStatus::Connected),
        "the supervisor reports Connected again after the manual reconnect"
    );
}

/// `:disconnect` drops a live link on demand (status → Disconnected, saves fail), and a
/// later `:reconnect` brings it back.
#[tokio::test]
async fn disconnect_drops_the_link_and_reconnect_restores_it() {
    const PATH: &str = "/virtual/note.txt";
    let dialer = Dialer::new(DaemonFs::with(PATH, "one\n"));
    let (rpc, _incoming, handle) = start(&dialer, PATH).await;

    await_lines(&rpc, &["one"]).await;
    set_line(&rpc, "two");
    feed(&rpc, ":w<CR>");
    assert!(save_until(&rpc, &dialer.fake, PATH, "two\n").await);

    // Explicit disconnect: the supervisor drops the live connection and parks Disconnected
    // (no auto-retry — it waits for a `:reconnect`).
    handle.disconnect();
    let mut down = false;
    for _ in 0..200 {
        if handle.status() == DaemonStatus::Disconnected {
            down = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(down, "`:disconnect` parks the link Disconnected");

    set_line(&rpc, "three");
    feed(&rpc, ":w<CR>");
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(
        dialer.fake.content(PATH).as_deref(),
        Some("two\n"),
        "a save while disconnected does not reach the remote"
    );

    handle.reconnect();
    assert!(
        save_until(&rpc, &dialer.fake, PATH, "three\n").await,
        "a `:reconnect` after `:disconnect` restores the link"
    );
}

/// Phase 6 — the reconnect **re-stat**. A change made to the remote file while the link is
/// down would be invisible to a naive reconnect: a re-dialed daemon is a fresh process that
/// re-baselines every watched path to its *current* contents, so the change becomes the silent
/// new normal. The resync closes that gap — it re-arms each watch carrying the editor's own
/// disk baseline, the daemon compares it to the live file and pushes the drift, and the
/// unmodified buffer autoreloads. This proves an outage-window edit reaches the editor.
#[tokio::test]
async fn reconnect_restats_and_reloads_an_external_change_made_during_the_outage() {
    const PATH: &str = "/virtual/note.txt";
    let dialer = Dialer::new(DaemonFs::with(PATH, "one\n"));
    let (client, handle) =
        nxvim_server::connect_daemon_reconnecting_on(dialer.make(), fast_policy())
            .await
            .expect("the initial daemon dial succeeds");
    let DaemonClient { host_fs, .. } = client;
    let init = ServerInit {
        file: Some(PATH.to_string()),
        host_fs_async: Some(Box::new(host_fs)),
        // The link must reach the editor so the status transition drives the resync.
        daemon_link: Some(handle),
        ..Default::default()
    };
    let (rpc, _incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    await_lines(&rpc, &["one"]).await;

    // Take the link down deterministically, then have an *external* writer change the remote
    // file while it's down (a different length, so the size-only fake stat actually differs).
    feed(&rpc, ":disconnect<CR>");
    await_status(&rpc, "disconnected").await;
    dialer.fake.set(PATH, "changed during outage\n");

    // Bring it back: the resync re-arms the watch with the pre-outage baseline, the fresh
    // daemon sees the drift, pushes `fs_changed`, and the unmodified buffer autoreloads.
    feed(&rpc, ":reconnect<CR>");
    await_status(&rpc, "connected").await;
    await_lines(&rpc, &["changed during outage"]).await;
    assert!(
        !modified(&rpc).await,
        "the autoreloaded buffer is clean (a silent reload, not a conflict)"
    );
}

/// A buffer the user edited locally during the outage must **not** be clobbered by the
/// reconnect re-stat when the remote also changed: that is a real conflict, so the buffer
/// keeps the local edits (and stays modified) rather than silently autoreloading.
#[tokio::test]
async fn reconnect_restat_does_not_clobber_local_edits_on_conflict() {
    const PATH: &str = "/virtual/note.txt";
    let dialer = Dialer::new(DaemonFs::with(PATH, "one\n"));
    let (client, handle) =
        nxvim_server::connect_daemon_reconnecting_on(dialer.make(), fast_policy())
            .await
            .expect("the initial daemon dial succeeds");
    let DaemonClient { host_fs, .. } = client;
    let init = ServerInit {
        file: Some(PATH.to_string()),
        host_fs_async: Some(Box::new(host_fs)),
        daemon_link: Some(handle),
        ..Default::default()
    };
    let (rpc, _incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    await_lines(&rpc, &["one"]).await;

    feed(&rpc, ":disconnect<CR>");
    await_status(&rpc, "disconnected").await;
    // Local edit (unsaved) AND a concurrent remote change → a conflict on reconnect.
    set_line(&rpc, "local edit");
    dialer.fake.set(PATH, "remote change\n");

    feed(&rpc, ":reconnect<CR>");
    await_status(&rpc, "connected").await;
    // Give the resync's push a beat to land; the modified buffer must NOT be reloaded.
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(
        buf_lines(&rpc, 0).await,
        vec!["local edit"],
        "a modified buffer keeps its local edits across the conflicting reconnect"
    );
    assert!(
        modified(&rpc).await,
        "the conflicted buffer stays modified (its edits are unsaved)"
    );
}

/// The dedicated-thread wrapper (`connect_daemon_reconnecting`, the GUI/TUI production
/// entry) drives the wire off the server thread and still auto-recovers a dropped link —
/// the seams the editor holds reach the link thread's runtime across threads, exactly as a
/// real ssh/stdio session does. Blocking handshake, then a sever auto-recovers with no
/// manual action.
#[tokio::test]
async fn dedicated_thread_wrapper_auto_recovers() {
    const PATH: &str = "/virtual/note.txt";
    let dialer = Dialer::new(DaemonFs::with(PATH, "one\n"));
    // Blocking handshake on a dedicated link thread (its own runtime drives the wire +
    // the daemon tasks the factory spawns there).
    let (client, _handle) = nxvim_server::connect_daemon_reconnecting(dialer.make(), fast_policy())
        .expect("the initial daemon dial succeeds");
    let DaemonClient { host_fs, .. } = client;
    let init = ServerInit {
        file: Some(PATH.to_string()),
        host_fs_async: Some(Box::new(host_fs)),
        ..Default::default()
    };
    let (rpc, _incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    await_lines(&rpc, &["one"]).await;
    set_line(&rpc, "two");
    feed(&rpc, ":w<CR>");
    assert!(
        save_until(&rpc, &dialer.fake, PATH, "two\n").await,
        "the cross-thread seam saves over the wire while connected"
    );

    // Sever the live daemon (running on the link thread's runtime); the supervisor there
    // auto-re-dials a fresh daemon over the same remote fs, with no manual reconnect.
    dialer.sever();
    set_line(&rpc, "three");
    assert_eq!(
        buf_lines(&rpc, 0).await,
        vec!["three"],
        "the local edit survives the dropped connection"
    );
    assert!(
        save_until(&rpc, &dialer.fake, PATH, "three\n").await,
        "the dedicated-thread link auto-recovers and the save resumes"
    );
}

/// Like [`start`], but also wires the daemon-backed **proc** seam (`nx.run` /
/// `vim.system` spawn over the daemon), so the process-path outage behavior is
/// testable. The fake daemon serves only the fs leg — irrelevant here: these tests
/// exercise the *link-down* paths, which must resolve locally without any daemon.
async fn start_with_proc(
    dialer: &Dialer,
    path: &str,
) -> (
    nxvim_rpc::Rpc,
    tokio::sync::mpsc::UnboundedReceiver<nxvim_rpc::Incoming>,
    ReconnectHandle,
) {
    let (client, handle) =
        nxvim_server::connect_daemon_reconnecting_on(dialer.make(), fast_policy())
            .await
            .expect("the initial daemon dial succeeds");
    let DaemonClient {
        host_fs, host_proc, ..
    } = client;
    let init = ServerInit {
        file: Some(path.to_string()),
        host_fs_async: Some(Box::new(host_fs)),
        host_proc: Some(Box::new(host_proc)),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming, handle)
}

/// Kick a daemon-backed `nx.run` and stash its result in `_G.res`.
async fn spawn_probe(rpc: &nxvim_rpc::Rpc) {
    exec_lua(
        rpc,
        "_G.res = nil\n\
         nx.run({ cmd = 'sh', args = { '-c', 'echo hi' } }):next(function(r) _G.res = r end)",
    )
    .await;
}

/// Poll until `_G.res` resolved, returning its `code` — or `None` if it never did
/// (the hang these tests exist to rule out).
async fn probe_code(rpc: &nxvim_rpc::Rpc) -> Option<i64> {
    for _ in 0..100 {
        let code = exec_lua(rpc, "return _G.res and _G.res.code").await;
        if !code.is_nil() {
            return code.as_i64();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

/// A `nx.run` issued while the link is DOWN must fail loud (`code = -1`, the cause
/// on stderr) — never park forever on a spawn notification that was dropped on the
/// floor (the seam contract: "while disconnected … fails loud, never hangs").
#[tokio::test]
async fn a_spawn_while_disconnected_fails_loud_instead_of_hanging() {
    const PATH: &str = "/virtual/spawn.txt";
    let dialer = Dialer::new(DaemonFs::with(PATH, "one\n"));
    let (rpc, _incoming, handle) = start_with_proc(&dialer, PATH).await;
    await_lines(&rpc, &["one"]).await;

    handle.disconnect();
    let mut down = false;
    for _ in 0..200 {
        if handle.status() == DaemonStatus::Disconnected {
            down = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(down, "the disconnect parks the link Disconnected");

    spawn_probe(&rpc).await;
    assert_eq!(
        probe_code(&rpc).await,
        Some(-1),
        "a spawn while disconnected resolves loud with code -1 (it must not hang)"
    );
    let err = exec_lua(&rpc, "return _G.res.stderr").await;
    assert!(
        err.as_str().unwrap_or("").contains("disconnected"),
        "the failure names the cause: {err:?}"
    );
}

/// A spawn already IN FLIGHT when the user `:disconnect`s must have its exit
/// synthesized (`code = -1`) — cancelling the connection's serve must fail the
/// pending waiters exactly like a naturally-dropped connection does, or the job's
/// `on_exit` (and any `:wq` gated on it) hangs forever.
#[tokio::test]
async fn an_inflight_spawn_synthesizes_an_exit_on_disconnect() {
    const PATH: &str = "/virtual/inflight.txt";
    let dialer = Dialer::new(DaemonFs::with(PATH, "one\n"));
    let (rpc, _incoming, handle) = start_with_proc(&dialer, PATH).await;
    await_lines(&rpc, &["one"]).await;

    // The fake daemon serves no proc leg, so this spawn parks awaiting a report
    // that will never come while the link is up — exactly an in-flight child.
    spawn_probe(&rpc).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        exec_lua(&rpc, "return _G.res == nil").await.as_bool() == Some(true),
        "the spawn is genuinely in flight before the disconnect"
    );

    handle.disconnect();
    let mut down = false;
    for _ in 0..200 {
        if handle.status() == DaemonStatus::Disconnected {
            down = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(down, "the disconnect parks the link Disconnected");
    assert_eq!(
        probe_code(&rpc).await,
        Some(-1),
        "an in-flight spawn's exit is synthesized when the link is dropped"
    );
}
