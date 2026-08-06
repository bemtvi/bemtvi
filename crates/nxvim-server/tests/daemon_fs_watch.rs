//! The daemon wire protocol, `nx.fs.watch` half — the **streaming watch leg**
//! (`luafs_watch` / `luafs_change` / `luafs_watch_err`).
//!
//! Distinct from `daemon_watch.rs`, which covers the per-buffer `fs_watch` stat-poll the
//! editor reconciles (`:checktime`). This leg is the *Lua* surface: a recursive,
//! change-classified watch keyed by stream id, armed on the daemon because that is where a
//! remote session's files are. Everything built on `nx.fs.watch` rides it — the LSP
//! `workspace/didChangeWatchedFiles` client, file trees, config reloaders — and before it
//! existed a native daemon session armed those watches on its own local disk, so a remote
//! change was never seen (and a remote-only path failed to arm outright).
//!
//! Faithful, not a no-op: the editor is given a
//! [`RemoteFsWatch`](nxvim_server::RemoteFsWatch) as its watch seam, so the event-loop
//! actor has **no** local watcher for `nx.fs.watch` and can only send `luafs_watch` over
//! the wire. Every change that reaches Lua therefore crossed the leg: was armed daemon-side,
//! coalesced daemon-side, pushed back as `luafs_change`, and decoded into the same
//! `LoopEvent::FsEvent` the local `notify` backend produces.

use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::{RemoteFsWatch, ServerInit};
use nxvim_test_harness::{exec_lua, q, start_attached, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

/// Every `luafs_watch` / `luafs_unwatch` the daemon side actually received, in order,
/// rendered as `"<method> <id> <path?>"`. The tests assert on this because the daemon in an
/// in-process test runs on the same real filesystem as the editor: a change reaching Lua
/// would *also* be explained by the actor quietly arming a local watcher, so "the daemon
/// was asked" is the part that pins the routing down.
type ArmLog = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// Start a server whose `nx.fs.watch` seam is a [`RemoteFsWatch`] talking to a
/// `serve_luafs_watch_daemon_on` over an in-process duplex, with every inbound frame
/// recorded on the way in. The actor is remote-only for watches — it never arms a local
/// `notify` watcher — and the [`ArmLog`] is what proves it.
async fn spawn_with_daemon_fs_watch() -> (Rpc, UnboundedReceiver<Incoming>, ArmLog) {
    let (edit_host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (daemon_reader, daemon_writer) = tokio::io::split(daemon_end);
    let log: ArmLog = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = log.clone();
    tokio::spawn(async move {
        let (rpc, mut incoming) = nxvim_rpc::connect(daemon_reader, daemon_writer);
        // Interpose on the daemon's inbound stream: record each arm/disarm, then hand the
        // frame on to the real leg unchanged.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Incoming>();
        tokio::spawn(async move {
            while let Some(msg) = incoming.recv().await {
                if let Incoming::Notification { method, params } = &msg {
                    if method.starts_with("luafs_") {
                        let id = params.first().and_then(|v| v.as_u64()).unwrap_or(0);
                        let path = params.get(1).and_then(|v| v.as_str()).unwrap_or("");
                        recorder
                            .lock()
                            .unwrap()
                            .push(format!("{method} {id} {path}").trim_end().to_string());
                    }
                }
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });
        let _ = nxvim_server::serve_luafs_watch_daemon_on(rpc, rx).await;
    });
    let (host_reader, host_writer) = tokio::io::split(edit_host_end);
    let init = ServerInit {
        fs_watch: Some(RemoteFsWatch::connect(host_reader, host_writer)),
        ..Default::default()
    };
    let (rpc, incoming) = start_attached(init, 80, 24).await;
    (rpc, incoming, log)
}

/// Poll the daemon's [`ArmLog`] until a line matching `pred` appears (~3s).
async fn await_arm(log: &ArmLog, pred: impl Fn(&str) -> bool) -> Option<String> {
    for _ in 0..120 {
        if let Some(line) = log.lock().unwrap().iter().find(|l| pred(l)) {
            return Some(line.clone());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    None
}

/// Arm a watch on `dir` that accumulates every change batch into `_G.evs`, and its
/// terminal error (if the arm fails) into `_G.err`.
async fn arm_watch(rpc: &Rpc, dir: &std::path::Path) {
    exec_lua(
        rpc,
        &format!(
            "_G.evs = {{}}\n\
             _G.err = nil\n\
             _G.W = nil\n\
             nx.async(function()\n\
               local w = nx.fs.watch(\"{d}\", {{ recursive = true }})\n\
               _G.W = w\n\
               for ev in nx.await_each(w) do _G.evs[#_G.evs+1] = ev end\n\
             end)():catch(function(e) _G.err = tostring(e) end)",
            d = q(dir)
        ),
    )
    .await;
}

/// Poll a `return`-style chunk until it yields a non-nil, non-empty string (~5s).
async fn poll_string(rpc: &Rpc, code: &str) -> Option<String> {
    for _ in 0..200 {
        if let Some(s) = exec_lua(rpc, code).await.as_str() {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    None
}

/// Drive a full, **reconnecting** daemon link (every leg group, the real multiplexer) over
/// in-process duplexes the test can sever, returning the editor's RPC, the handle that
/// re-dials, and the list of live daemon tasks (newest last) so a test can abort one.
async fn spawn_with_reconnecting_daemon() -> (
    Rpc,
    UnboundedReceiver<Incoming>,
    nxvim_server::ReconnectHandle,
    std::sync::Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
) {
    let daemons: std::sync::Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let make = {
        let daemons = daemons.clone();
        move || {
            let daemons = daemons.clone();
            Box::pin(async move {
                let (eh_end, daemon_end) = tokio::io::duplex(1 << 16);
                let (dr, dw) = tokio::io::split(daemon_end);
                // The REAL daemon multiplexer: every leg group, including the Control
                // group's `luafs_watch`. A re-dial stands up a fresh one — which, like a
                // real re-dialed daemon, knows about no watch armed before it existed.
                daemons.lock().unwrap().push(tokio::spawn(async move {
                    let _ = nxvim_server::run_daemon_io(dr, dw).await;
                }));
                let (er, ew) = tokio::io::split(eh_end);
                Ok((er, ew))
            })
                as std::pin::Pin<
                    Box<
                        dyn std::future::Future<
                                Output = anyhow::Result<(
                                    tokio::io::ReadHalf<tokio::io::DuplexStream>,
                                    tokio::io::WriteHalf<tokio::io::DuplexStream>,
                                )>,
                            > + Send,
                    >,
                >
        }
    };
    let (client, handle) = nxvim_server::connect_daemon_reconnecting_on(
        make,
        nxvim_server::ReconnectPolicy {
            max_attempts: 5,
            base: Duration::from_millis(20),
            cap: Duration::from_millis(60),
        },
    )
    .await
    .expect("the initial daemon dial succeeds");
    let init = ServerInit {
        fs_watch: Some(client.fs_watch),
        ..Default::default()
    };
    let (rpc, incoming) = start_attached(init, 80, 24).await;
    (rpc, incoming, handle, daemons)
}

/// A watch **survives a reconnect**: the re-dialed daemon is a fresh process that knows
/// about no watch, so the link re-arms every live one. Without that, a `nx.fs.watch`
/// iterator (a file tree, the LSP file-watch client) goes permanently deaf after any blip
/// — still "running", never yielding again, with nothing to say so.
#[tokio::test]
async fn a_watch_is_rearmed_after_a_reconnect() {
    let dir = temp_dir("daemon_fs_watch_rearm");
    let (rpc, _incoming, _handle, daemons) = spawn_with_reconnecting_daemon().await;
    arm_watch(&rpc, &dir).await;

    // Live before the outage — otherwise "it worked after" proves nothing about re-arming.
    let mut armed = false;
    for attempt in 0..40 {
        std::fs::write(dir.join(format!("pre{attempt}.txt")), b"x").unwrap();
        if poll_kinds_containing(&rpc, "pre").await.is_some() {
            armed = true;
            break;
        }
    }
    assert!(armed, "the watch must be live before the link is severed");

    // Sever the link: the supervisor re-dials a FRESH daemon underneath the seams.
    if let Some(h) = daemons.lock().unwrap().last() {
        h.abort();
    }
    // The re-dial + re-arm are async; retry with fresh names until a change lands again.
    for attempt in 0..60 {
        std::fs::write(dir.join(format!("post{attempt}.txt")), b"y").unwrap();
        if poll_kinds_containing(&rpc, "post").await.is_some() {
            exec_lua(&rpc, "if _G.W then _G.W:stop() end").await;
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    }
    let err = exec_lua(&rpc, "return _G.err").await;
    panic!("the watch was not re-armed on the re-dialed daemon (watch error: {err:?})");
}

/// A file created on the **daemon's** disk reaches `nx.fs.watch` over the wire, carrying
/// the path and the `"create"` change class. The actor holds no local watcher, so the batch
/// can only have been armed and coalesced daemon-side and pushed back as `luafs_change`.
#[tokio::test]
async fn a_daemon_side_change_reaches_nx_fs_watch() {
    let dir = temp_dir("daemon_fs_watch");
    let (rpc, _incoming, arms) = spawn_with_daemon_fs_watch().await;
    arm_watch(&rpc, &dir).await;
    // The arm went to the DAEMON: the actor asked it to watch, rather than quietly
    // arming a `notify` watcher on this side (which the shared test filesystem would
    // otherwise make indistinguishable).
    let armed_on_daemon = await_arm(&arms, |l| {
        l.starts_with("luafs_watch ") && l.ends_with(&dir.to_string_lossy().to_string())
    })
    .await;
    assert!(
        armed_on_daemon.is_some(),
        "the watch must be armed on the daemon; the daemon saw: {:?}",
        arms.lock().unwrap()
    );
    // The arm crosses the wire and the daemon's `notify` backend takes a moment to come
    // up, so create the file repeatedly (a fresh name each round) until one is reported —
    // the same race the local watcher has, closed the same way.
    for attempt in 0..40 {
        std::fs::write(dir.join(format!("made{attempt}.txt")), b"hello").unwrap();
        let seen = poll_kinds_containing(&rpc, "made").await;
        if let Some(seen) = seen {
            assert!(
                seen.contains("create"),
                "a created file must be reported with its change class; got: {seen}"
            );
            exec_lua(&rpc, "if _G.W then _G.W:stop() end").await;
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    }
    let err = exec_lua(&rpc, "return _G.err").await;
    panic!("no daemon watch batch reached nx.fs.watch (watch error: {err:?})");
}

/// A short poll for a `kind:path` rendering of everything seen so far, filtered to batches
/// naming `needle`.
async fn poll_kinds_containing(rpc: &Rpc, needle: &str) -> Option<String> {
    let code = format!(
        "local out = {{}}\n\
         for _, ev in ipairs(_G.evs or {{}}) do\n\
           for _, p in ipairs(ev.paths or {{}}) do\n\
             if p:find(\"{needle}\", 1, true) then out[#out+1] = ev.kind .. \":\" .. p end\n\
           end\n\
         end\n\
         return table.concat(out, \" \")"
    );
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        if let Some(s) = exec_lua(rpc, &code).await.as_str() {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// A watch that can't arm **on the daemon** rejects the consumer's pull loud, exactly like
/// a local arm failure — never a watch that looks live and silently never fires. This is
/// the `luafs_watch_err` push crossing back and being decoded into the same terminal error
/// event the local actor emits.
#[tokio::test]
async fn a_daemon_arm_failure_rejects_loud() {
    let (rpc, _incoming, _arms) = spawn_with_daemon_fs_watch().await;
    arm_watch(&rpc, std::path::Path::new("/nxvim/definitely/not/here")).await;

    let err = poll_string(&rpc, "return _G.err")
        .await
        .expect("a failed daemon arm must reject the watch's pull");
    // The daemon's own `notify` wording ("No path was found."), not a synthesized
    // client-side message — which is the point: the failure is reported by the side that
    // actually tried to arm.
    assert!(
        err.to_lowercase().contains("path"),
        "the rejection must carry the daemon's reason; got: {err}"
    );
}

/// `:stop()` unwatches on the daemon: after it, a change in the watched tree produces no
/// further batches. Without the `luafs_unwatch` half, the daemon would keep watching (and
/// keep pushing) for the rest of the session.
#[tokio::test]
async fn stop_unwatches_on_the_daemon() {
    let dir = temp_dir("daemon_fs_watch_stop");
    let (rpc, _incoming, arms) = spawn_with_daemon_fs_watch().await;
    arm_watch(&rpc, &dir).await;

    // Establish the watch is live first — otherwise "nothing arrived after :stop" would
    // also pass for a watch that never armed at all.
    let mut armed = false;
    for attempt in 0..40 {
        std::fs::write(dir.join(format!("before{attempt}.txt")), b"x").unwrap();
        if poll_kinds_containing(&rpc, "before").await.is_some() {
            armed = true;
            break;
        }
    }
    assert!(
        armed,
        "the watch must be live before :stop() means anything"
    );

    exec_lua(&rpc, "_G.W:stop()").await;
    // The disarm really crossed the wire — otherwise the daemon would keep watching (and
    // keep pushing) for the rest of the session, and "nothing arrived" would only mean the
    // Lua stream stopped listening.
    assert!(
        await_arm(&arms, |l| l.starts_with("luafs_unwatch "))
            .await
            .is_some(),
        "`:stop()` must send `luafs_unwatch`; the daemon saw: {:?}",
        arms.lock().unwrap()
    );
    // Give the unwatch time to cross, then change the tree again.
    tokio::time::sleep(Duration::from_millis(150)).await;
    for i in 0..5 {
        std::fs::write(dir.join(format!("after{i}.txt")), b"y").unwrap();
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after = poll_kinds_containing(&rpc, "after").await;
    assert!(
        after.is_none(),
        "a stopped watch must report nothing further; got: {after:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
