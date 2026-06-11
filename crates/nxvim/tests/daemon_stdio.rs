//! Black-box test of `nxvim --daemon` — the binary's **daemon** role: the
//! edit-host split's remote half, multiplexing every leg of the daemon wire
//! (fs / process / watch / `sys_run` / LSP / `luafs`) over one stdin/stdout stream
//! (see `docs/plans/2026-06-09-edit-host-and-browser-lua.md` → Phase 3q).
//!
//! Both tests spawn the *real* compiled binary (`CARGO_BIN_EXE_nxvim`) with `--daemon`
//! and piped stdio, and drive multiple wire namespaces over the *one* connection — the
//! point the per-leg duplex suites can't make: all the classes coexist on one ordered
//! stdio stream, demuxed by method, without cross-talk or head-of-line deadlock. They
//! cover the two halves of the multiplexer:
//!
//! - [`daemon_multiplexes_fs_proc_and_luafs_over_one_stream`] drives the **daemon side**
//!   directly with a raw [`nxvim_rpc`] client — `fs_*` (request/response), `proc_*`
//!   (notifications), and `luafs` (request/response) interleaved on one stream.
//! - [`edit_host_drives_a_real_daemon_over_one_stream`] drives the **edit-host side**:
//!   it wraps the child in [`connect_daemon`](nxvim_server::connect_daemon) — the
//!   edit-host multiplexer — and hands the five resulting seams to a real in-process
//!   [`Server`](nxvim_server) (`spawn`/`attach` from the harness), then exercises four
//!   classes through the running editor: the off-tick **fs read** (startup open) and
//!   **write** (`:w`), the blocking **`sys_run`** bridge (`vim.system():wait()`), the
//!   **watch** push (external-change autoreload), and **`luafs`** (`vim.uv.fs_stat`).
//!   All five seams share the single stdio stream via one `connect_daemon` link.

use std::process::Stdio;
use std::time::Duration;

use nxvim_rpc::{connect, Incoming};
use nxvim_server::{connect_daemon, ServerInit};
use nxvim_test_harness::{attach, buf_lines, exec_lua, feed, spawn, temp_dir};
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

// ============================================================================
// The edit-host side: connect_daemon driving a real Server over one stdio stream
// ============================================================================

/// Spawn `nxvim --daemon`, wrap it in [`connect_daemon`] (the edit-host multiplexer),
/// and start a real in-process edit-host [`Server`](nxvim_server) whose five host seams
/// all ride that one connection. The startup `file` is fetched off-tick over the wire.
/// Returns the editor's RPC handle, its notification stream, and the live daemon child —
/// the caller keeps the child alive (`kill_on_drop` reaps it on drop).
async fn spawn_edit_host(file: &str) -> (nxvim_rpc::Rpc, UnboundedReceiver<Incoming>, Child) {
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

    // One connection → all five seams. This is the edit-host multiplexer under test.
    let client = connect_daemon(stdout, stdin);
    let init = ServerInit {
        file: Some(file.to_string()),
        host_fs_async: Some(Box::new(client.host_fs)),
        host_proc: Some(Box::new(client.host_proc)),
        blocking_system: Some(Box::new(client.blocking_system)),
        lsp_transport: Some(Box::new(client.lsp_transport)),
        lua_fs: Some(Box::new(client.lua_fs)),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    // `attach` returning proves startup did not block on the (deferred, off-tick) fetch.
    attach(&rpc, 80, 24).await;
    (rpc, incoming, child)
}

/// Poll `nvim_buf_get_lines` until it matches `want` or the budget runs out. The initial
/// open and the watch-driven autoreload both land off-tick, a moment after the
/// triggering action, so a bounded retry beats a fixed sleep.
async fn await_lines(rpc: &nxvim_rpc::Rpc, want: &[&str]) -> Vec<String> {
    for _ in 0..150 {
        let lines = buf_lines(rpc, 0).await;
        if lines == want {
            return lines;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    buf_lines(rpc, 0).await
}

/// A real edit-host `Server` drives a real `nxvim --daemon` child through
/// [`connect_daemon`], exercising **four wire classes through the running editor over
/// one stdio stream**: the off-tick fs read (startup open) and write (`:w`), the
/// blocking `sys_run` bridge, the watch push, and `luafs`. Together with the proc leg
/// (covered by the daemon-side test above and wired identically here), this proves the
/// edit-host multiplexer keeps every seam apart on the single shared connection — no
/// cross-talk, and no deadlock from the blocking bridges parking the editor thread while
/// the shared link drives their replies.
#[tokio::test]
async fn edit_host_drives_a_real_daemon_over_one_stream() {
    let dir = temp_dir("daemon-stdio-edithost");
    let path = dir.join("doc.txt");
    let path_str = path.to_str().unwrap().to_string();
    std::fs::write(&path, "line one\nline two\n").expect("seed file");

    let (rpc, _incoming, _child) = spawn_edit_host(&path_str).await;

    // --- fs read leg: the startup buffer was fetched off-tick over the wire. With a
    // daemon fs seam present the editor never reads local disk for the open — the bytes
    // only reach the buffer if the `fs_read` request and reply crossed the one stream.
    let lines = await_lines(&rpc, &["line one", "line two"]).await;
    assert_eq!(
        lines,
        &["line one", "line two"],
        "startup open over the wire"
    );

    // --- luafs leg: `vim.uv.fs_stat` resolves against the daemon's fs. The file is 18
    // bytes ("line one\nline two\n"); `filereadable` is 1.
    let size = exec_lua(
        &rpc,
        &format!(r#"return vim.uv.fs_stat("{path_str}").size"#),
    )
    .await;
    assert_eq!(
        size.as_u64(),
        Some(18),
        "vim.uv.fs_stat over luafs: {size:?}"
    );
    let readable = exec_lua(
        &rpc,
        &format!(r#"return vim.fn.filereadable("{path_str}")"#),
    )
    .await;
    assert_eq!(
        readable.as_u64(),
        Some(1),
        "vim.fn.filereadable over luafs: {readable:?}"
    );

    // --- sys_run leg: the *blocking* `vim.system(...):wait()` parks the editor thread on
    // the reply while the shared link thread drives the wire and the daemon runs the
    // child — the exact bridge that would deadlock if the connection were driven by the
    // (parked) editor thread. The echoed stdout proves the round-trip completed inline.
    let sys = exec_lua(
        &rpc,
        r#"return vim.system({ "printf", "sys-leg-ok" }):wait().stdout"#,
    )
    .await;
    assert_eq!(
        sys.as_str(),
        Some("sys-leg-ok"),
        "vim.system:wait over sys_run: {sys:?}"
    );

    // --- fs write leg: edit the buffer and `:w`. The save is off-tick — `modified`
    // clears only when the daemon acks the `fs_write` — so poll until it clears, then
    // confirm the *edited* bytes actually landed on the daemon's disk.
    feed(&rpc, "ggO"); // open a line above
    feed(&rpc, "line zero");
    feed(&rpc, "<Esc>");
    feed(&rpc, ":w<CR>");
    let mut cleared = false;
    for _ in 0..150 {
        if exec_lua(&rpc, "return vim.bo.modified").await.as_bool() == Some(false) {
            cleared = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        cleared,
        ":w should clear `modified` once the daemon acks the write"
    );
    let on_disk = std::fs::read_to_string(&path).expect("read back the saved file");
    assert_eq!(
        on_disk, "line zero\nline one\nline two\n",
        "the edited bytes crossed the wire and the daemon wrote them"
    );

    // --- watch leg: change the file *externally* (the test process, not the editor).
    // Only the daemon watches the remote file; it must detect the drift and push
    // `fs_changed`, which the edit-host turns into an off-tick autoreload (the buffer is
    // clean after the save, so `'autoread'` reloads it silently).
    std::fs::write(&path, "reloaded\nfrom\nwatch\n").expect("external write");
    let reloaded = await_lines(&rpc, &["reloaded", "from", "watch"]).await;
    assert_eq!(
        reloaded,
        &["reloaded", "from", "watch"],
        "external change autoreloaded via the daemon's watch push over the wire"
    );
}
