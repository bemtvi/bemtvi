//! Black-box test of `nxvim --daemon` — the binary's **daemon** role: the
//! edit-host split's remote half, multiplexing every leg of the daemon wire
//! (fs / process / watch / `sys_run` / LSP / `luafs`) over one stdin/stdout stream.
//! This is the transport `ssh host nxvim --daemon` execs; the local edit-host drives
//! it through the ssh pipe (see `docs/plans/2026-06-09-edit-host-and-browser-lua.md`
//! → Phase 3q).
//!
//! It spawns the *real* compiled binary (`CARGO_BIN_EXE_nxvim`) with `--daemon` and
//! piped stdio, connects one [`nxvim_rpc`] client to it, and drives **three distinct
//! wire namespaces over that single connection** — `fs_*` (request/response), `proc_*`
//! (notifications), and `luafs` (request/response). The point the per-leg duplex
//! suites can't make: all the classes coexist on one ordered stdio stream, demuxed by
//! method, without cross-talk. The whole daemon mechanism minus the network hop.

use std::process::Stdio;

use nxvim_rpc::{connect, Incoming};
use nxvim_test_harness::temp_dir;
use rmpv::Value;
use tokio::process::{Child, Command};
use tokio::sync::mpsc::UnboundedReceiver;

/// Spawn `nxvim --daemon` with piped stdio and connect to it. Returns the live
/// child *and* the `incoming` receiver — the caller keeps both alive: dropping the
/// child closes the pipe (`kill_on_drop` reaps it), and dropping `incoming` would tear
/// the connection down on the daemon's first notification.
fn spawn_daemon() -> (nxvim_rpc::Rpc, UnboundedReceiver<Incoming>, Child) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nxvim"))
        .arg("--daemon")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn nxvim --daemon");
    let stdout = child.stdout.take().expect("piped stdout");
    let stdin = child.stdin.take().expect("piped stdin");
    let (rpc, incoming) = connect(stdout, stdin);
    (rpc, incoming, child)
}

/// Drain `incoming` until the daemon reports the given spawn `id` exited; returns
/// `(code, stdout_bytes)`. The `proc_*` leg reports a child's life as two
/// notifications (`proc_spawned` then `proc_exited`) — we skip the spawn and read the
/// exit.
async fn await_proc_exit(incoming: &mut UnboundedReceiver<Incoming>, id: u64) -> (i64, Vec<u8>) {
    while let Some(msg) = incoming.recv().await {
        if let Incoming::Notification { method, params } = msg {
            if method == "proc_exited" && params.first().and_then(Value::as_u64) == Some(id) {
                let code = params.get(1).and_then(Value::as_i64).expect("exit code");
                let stdout = match params.get(2) {
                    Some(Value::Binary(b)) => b.clone(),
                    _ => Vec::new(),
                };
                return (code, stdout);
            }
            // `proc_spawned` (and any other notification for another id) — keep reading.
        }
    }
    panic!("daemon closed before proc {id} exited");
}

/// Three wire namespaces — `fs_*`, `proc_*`, `luafs` — driven over **one** connection
/// to one `nxvim --daemon` child, with an fs request issued *while a process spawn is
/// in flight* so the multiplexer is proven to keep them apart on the shared stream.
#[tokio::test]
async fn daemon_multiplexes_fs_proc_and_luafs_over_one_stream() {
    let (rpc, mut incoming, _child) = spawn_daemon();

    let dir = temp_dir("daemon-stdio");
    let path = dir.join("note.txt");
    let path_str = path.to_str().unwrap().to_string();
    let body = b"written / over / the / daemon / wire".to_vec();

    // --- proc leg (notifications): start a real child on the daemon. Issued *first*,
    // then we interleave an fs round-trip below before collecting its exit, so the
    // two namespaces are genuinely concurrent on the one stream.
    let proc_id = 1u64;
    rpc.notify(
        "proc_spawn",
        vec![
            Value::from(proc_id),
            Value::Array(vec![
                Value::from("sh"),
                Value::from("-c"),
                Value::from("printf hello-from-daemon"),
            ]),
            Value::Nil,           // cwd
            Value::Array(vec![]), // env
            Value::Binary(vec![]),
        ],
    );

    // --- fs leg (request/response): write bytes the daemon's real disk holds, then
    // read them back. Replies are msgid-routed inside `Rpc`, so they never collide
    // with the proc notifications queued on `incoming`.
    let write_reply = rpc
        .request(
            "fs_write",
            vec![Value::from(path_str.as_str()), Value::Binary(body.clone())],
        )
        .await
        .expect("fs_write");
    // `["ok", stat?]`.
    assert_eq!(
        write_reply
            .as_array()
            .and_then(|a| a.first())
            .and_then(Value::as_str),
        Some("ok"),
        "fs_write should ack: {write_reply:?}"
    );

    let read_reply = rpc
        .request("fs_read", vec![Value::from(path_str.as_str())])
        .await
        .expect("fs_read");
    // `["file", bytes]` — the bytes can only have crossed the wire (we wrote them via
    // the daemon's own leg, and read them back through a different one).
    let arr = read_reply.as_array().expect("fs_read array");
    assert_eq!(arr[0].as_str(), Some("file"));
    assert!(
        matches!(&arr[1], Value::Binary(b) if *b == body),
        "fs_read should return the written bytes: {read_reply:?}"
    );

    // --- luafs leg (request/response, a third namespace): the project-facing fs sees
    // the *same* real file. `read_file` → `["ok", bytes]`.
    let luafs_reply = rpc
        .request(
            "luafs",
            vec![Value::from("read_file"), Value::from(path_str.as_str())],
        )
        .await
        .expect("luafs read_file");
    let la = luafs_reply.as_array().expect("luafs array");
    assert_eq!(
        la[0].as_str(),
        Some("ok"),
        "luafs should succeed: {luafs_reply:?}"
    );
    assert!(
        matches!(&la[1], Value::Binary(b) if *b == body),
        "luafs read_file should return the same bytes: {luafs_reply:?}"
    );

    // --- collect the proc leg's result: the child ran on the daemon and its *actual*
    // stdout came back — output a stub can't invent — while the fs/luafs traffic
    // flowed on the same stream.
    let (code, stdout) = await_proc_exit(&mut incoming, proc_id).await;
    assert_eq!(code, 0, "daemon child should exit 0");
    assert_eq!(stdout, b"hello-from-daemon");
}
