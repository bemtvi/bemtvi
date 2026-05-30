//! Behavior tests for nxvim, driven the way a real client drives it.
//!
//! These are deliberately *black box*: every test starts a real server on its
//! own thread, connects over the same msgpack-RPC a UI uses, sends vim
//! key-notation via `nvim_input`, and asserts on observable results — buffer
//! contents (`nvim_buf_get_lines`), the bytes written to disk, or the rendered
//! screen. Nothing reaches into the editor's internals. We verify *what the
//! editor does*, not how it's built.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server on its own thread and return a connected client.
async fn start(file: Option<String>) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (server_end, client_end) = tokio::io::duplex(1 << 16);

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("server runtime");
        let _ = runtime.block_on(run_server(server_end, ServerInit { file }));
    });

    let (reader, writer) = tokio::io::split(client_end);
    let (rpc, incoming) = connect(reader, writer);

    rpc.request(
        "nvim_ui_attach",
        vec![Value::from(80u64), Value::from(24u64), Value::Map(vec![])],
    )
    .await
    .expect("ui attach");

    (rpc, incoming)
}

/// Type a string of vim key-notation.
fn feed(rpc: &Rpc, keys: &str) {
    rpc.notify("nvim_input", vec![Value::from(keys)]);
}

/// Fetch all buffer lines. Also serves as a barrier: awaiting the response
/// guarantees the server has processed every message sent before it.
async fn lines(rpc: &Rpc) -> Vec<String> {
    let result = rpc
        .request(
            "nvim_buf_get_lines",
            vec![
                Value::from(0u64),
                Value::from(0i64),
                Value::from(-1i64),
                Value::Boolean(false),
            ],
        )
        .await
        .expect("get_lines");
    match result {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

fn temp_path(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("nxvim_test_{tag}_{}_{n}.txt", std::process::id()))
}

#[tokio::test]
async fn inserting_text_appears_in_the_buffer() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello world"]);
}

#[tokio::test]
async fn opening_lines_and_navigating() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifirst<Esc>osecond<Esc>othird<Esc>");
    assert_eq!(lines(&rpc).await, vec!["first", "second", "third"]);
}

#[tokio::test]
async fn dd_deletes_the_current_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>othree<Esc>");
    // Back to the middle line and delete it.
    feed(&rpc, "kdd");
    assert_eq!(lines(&rpc).await, vec!["one", "three"]);
}

#[tokio::test]
async fn cw_changes_a_word() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar baz<Esc>");
    // Start of line, change first word.
    feed(&rpc, "0cwqux<Esc>");
    assert_eq!(lines(&rpc).await, vec!["qux bar baz"]);
}

#[tokio::test]
async fn undo_reverts_the_last_change() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>");
    feed(&rpc, "ddu");
    assert_eq!(lines(&rpc).await, vec!["hello"]);
}

#[tokio::test]
async fn yank_and_paste_duplicates_a_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    feed(&rpc, "yyp");
    assert_eq!(lines(&rpc).await, vec!["alpha", "alpha"]);
}

#[tokio::test]
async fn ex_write_persists_changes_to_disk() {
    let path = temp_path("write");
    std::fs::write(&path, "one\ntwo\n").unwrap();

    let (rpc, _incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    // Jump to the last line, open a new one, type, leave insert, then save.
    feed(&rpc, "Gothree<Esc>");
    rpc.request("nvim_command", vec![Value::from("w")])
        .await
        .expect("write");

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, "one\ntwo\nthree\n");
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn lua_vim_cmd_drives_the_editor() {
    // A Lua chunk that opens a file should change what the buffer shows.
    let path = temp_path("lua");
    std::fs::write(&path, "alpha\nbeta\n").unwrap();

    let (rpc, _incoming) = start(None).await;
    let chunk = format!("lua vim.cmd(\"edit {}\")", path.to_string_lossy());
    rpc.request("nvim_command", vec![Value::from(chunk.as_str())])
        .await
        .expect("lua command");

    assert_eq!(lines(&rpc).await, vec!["alpha", "beta"]);
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn screen_reflects_typed_text_and_mode() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello");
    // Barrier: ensure the input (and its redraw) have been processed.
    let _ = lines(&rpc).await;

    let grid = drain_grid(&mut incoming, 24);
    assert!(grid[0].starts_with("hello"), "first row was {:?}", grid[0]);

    let status = &grid[grid.len() - 2];
    assert!(status.contains("INSERT"), "status row was {status:?}");
}

/// Apply all currently-buffered `redraw` notifications onto a fresh grid.
fn drain_grid(incoming: &mut UnboundedReceiver<Incoming>, height: usize) -> Vec<String> {
    let mut rows = vec![String::new(); height];
    while let Ok(message) = incoming.try_recv() {
        let Incoming::Notification { method, params } = message else {
            continue;
        };
        if method != "redraw" {
            continue;
        }
        for event in &params {
            let Value::Array(parts) = event else { continue };
            match parts.first().and_then(Value::as_str) {
                Some("resize") => {
                    let h = parts.get(2).and_then(Value::as_u64).unwrap_or(height as u64) as usize;
                    rows.resize(h, String::new());
                }
                Some("line") => {
                    let row = parts.get(1).and_then(Value::as_u64).unwrap_or(0) as usize;
                    let text = parts.get(2).and_then(Value::as_str).unwrap_or("");
                    if row < rows.len() {
                        rows[row] = text.to_string();
                    }
                }
                _ => {}
            }
        }
    }
    rows
}
