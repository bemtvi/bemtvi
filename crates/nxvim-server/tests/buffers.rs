//! Behavior tests for multiple open buffers, driven the way a real client
//! drives the editor (black-box RPC, exactly like `editing.rs`).
//!
//! Phase 2 covers the switch *mechanism*: `:e` opening/reusing buffers, the
//! alternate buffer (`<C-^>`), and each buffer keeping its own content, cursor
//! position, and undo history across switches. Phase 3 adds the list surface
//! (`:ls`, `:b`, `:bnext`/`:bprev`, `:bd`, `:wall`) and the buffer RPC API.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server on its own thread and return a connected client.
async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (server_end, client_end) = tokio::io::duplex(1 << 16);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("server runtime");
        let _ = runtime.block_on(run_server(server_end, ServerInit::default()));
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

/// Run an ex-command (`nvim_command`), awaiting the response as a barrier.
async fn command(rpc: &Rpc, cmd: &str) {
    rpc.request("nvim_command", vec![Value::from(cmd)])
        .await
        .expect("command");
}

/// Fetch all current-buffer lines (also a barrier: awaiting it guarantees the
/// server has processed every message sent before it).
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

/// Cursor position as `(1-based line, 0-based column)`.
async fn cursor(rpc: &Rpc) -> (usize, usize) {
    let result = rpc
        .request("nvim_win_get_cursor", vec![Value::from(0u64)])
        .await
        .expect("get_cursor");
    match result {
        Value::Array(a) => (
            a.first().and_then(Value::as_u64).unwrap_or(0) as usize,
            a.get(1).and_then(Value::as_u64).unwrap_or(0) as usize,
        ),
        _ => (0, 0),
    }
}

/// The current status `message`, read off the latest `redraw`. Sends a barrier
/// request so the redraw for the preceding action is already queued, then drains
/// to the most recent one.
async fn message(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> String {
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let mut msg = String::new();
    while let Ok(inc) = incoming.try_recv() {
        if let Incoming::Notification { method, params } = inc {
            if method == "redraw" {
                if let Some(Value::Map(map)) = params.into_iter().next() {
                    msg = map
                        .iter()
                        .find(|(k, _)| k.as_str() == Some("message"))
                        .and_then(|(_, v)| v.as_str())
                        .unwrap_or("")
                        .to_string();
                }
            }
        }
    }
    msg
}

/// The bottom panel's `(title, content lines, selected row)`, read off the
/// latest `redraw`. The selected row is `cursor_row`, relative to the visible
/// slice. `None` when no panel is open. Drains to the most recent redraw like
/// [`message`] does.
async fn panel(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<(String, Vec<String>, usize)> {
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let mut result = None;
    while let Ok(inc) = incoming.try_recv() {
        if let Incoming::Notification { method, params } = inc {
            if method == "redraw" {
                if let Some(Value::Map(map)) = params.into_iter().next() {
                    let p = map
                        .iter()
                        .find(|(k, _)| k.as_str() == Some("panel"))
                        .map(|(_, v)| v);
                    result = match p {
                        Some(Value::Map(panel)) => {
                            let get = |key| panel.iter().find(|(k, _)| k.as_str() == Some(key));
                            let title = get("title")
                                .and_then(|(_, v)| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let lines = get("lines")
                                .and_then(|(_, v)| v.as_array())
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|v| v.as_str().map(str::to_string))
                                        .collect()
                                })
                                .unwrap_or_default();
                            let cursor_row = get("cursor_row")
                                .and_then(|(_, v)| v.as_u64())
                                .unwrap_or(0) as usize;
                            Some((title, lines, cursor_row))
                        }
                        _ => None,
                    };
                }
            }
        }
    }
    result
}

/// A uniquely-named temp file path with the given contents. `.txt` so no
/// treesitter grammar is involved (keeps these tests free of the syntax worker).
fn temp_file(tag: &str, contents: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("nxvim_buf_{tag}_{}_{n}.txt", std::process::id()));
    std::fs::write(&path, contents).unwrap();
    path
}

fn name(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ----- buffer RPC API helpers -------------------------------------------------

async fn list_bufs(rpc: &Rpc) -> Vec<u64> {
    match rpc
        .request("nvim_list_bufs", vec![])
        .await
        .expect("list_bufs")
    {
        Value::Array(a) => a.iter().filter_map(Value::as_u64).collect(),
        _ => Vec::new(),
    }
}

async fn current_buf(rpc: &Rpc) -> u64 {
    rpc.request("nvim_get_current_buf", vec![])
        .await
        .expect("get_current_buf")
        .as_u64()
        .expect("u64")
}

async fn set_current_buf(rpc: &Rpc, id: u64) {
    rpc.request("nvim_set_current_buf", vec![Value::from(id)])
        .await
        .expect("set_current_buf");
}

async fn create_buf(rpc: &Rpc) -> u64 {
    rpc.request("nvim_create_buf", vec![])
        .await
        .expect("create_buf")
        .as_u64()
        .expect("u64")
}

async fn buf_name(rpc: &Rpc, handle: u64) -> String {
    rpc.request("nvim_buf_get_name", vec![Value::from(handle)])
        .await
        .expect("buf_get_name")
        .as_str()
        .unwrap_or("")
        .to_string()
}

/// Lines of a specific buffer by handle (0 = current).
async fn buf_lines(rpc: &Rpc, handle: u64) -> Vec<String> {
    let result = rpc
        .request(
            "nvim_buf_get_lines",
            vec![
                Value::from(handle),
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

#[tokio::test]
async fn edit_reuses_the_throwaway_buffer_then_opens_new_ones() {
    let a = temp_file("a", "a1\na2\na3\n");
    let b = temp_file("b", "b1\nb2\nb3\n");
    let (rpc, _incoming) = start().await;

    // First `:e` reuses the initial empty [No Name] buffer in place.
    command(&rpc, &format!("e {}", name(&a))).await;
    assert_eq!(lines(&rpc).await, vec!["a1", "a2", "a3"]);

    // A second file opens a new buffer and switches to it.
    command(&rpc, &format!("e {}", name(&b))).await;
    assert_eq!(lines(&rpc).await, vec!["b1", "b2", "b3"]);

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn reediting_an_open_file_switches_back_and_restores_the_cursor() {
    let a = temp_file("a", "a1\na2\na3\n");
    let b = temp_file("b", "b1\nb2\nb3\n");
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await;
    feed(&rpc, "jl"); // cursor to line 2, col 1
    assert_eq!(cursor(&rpc).await, (2, 1));

    command(&rpc, &format!("e {}", name(&b))).await;
    assert_eq!(lines(&rpc).await, vec!["b1", "b2", "b3"]);
    assert_eq!(cursor(&rpc).await, (1, 0));

    // Re-editing `a` finds the existing buffer and switches back — no duplicate,
    // and the cursor is where we left it.
    command(&rpc, &format!("e {}", name(&a))).await;
    assert_eq!(lines(&rpc).await, vec!["a1", "a2", "a3"]);
    assert_eq!(cursor(&rpc).await, (2, 1));

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn ctrl_caret_toggles_the_alternate_buffer() {
    let a = temp_file("a", "a1\na2\n");
    let b = temp_file("b", "b1\nb2\n");
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await;
    command(&rpc, &format!("e {}", name(&b))).await;
    assert_eq!(lines(&rpc).await, vec!["b1", "b2"]);

    feed(&rpc, "<C-^>"); // -> alternate (a)
    assert_eq!(lines(&rpc).await, vec!["a1", "a2"]);

    feed(&rpc, "<C-^>"); // -> back to b
    assert_eq!(lines(&rpc).await, vec!["b1", "b2"]);

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn ctrl_caret_without_an_alternate_reports_e23() {
    let (rpc, mut incoming) = start().await;
    feed(&rpc, "<C-^>");
    assert_eq!(message(&rpc, &mut incoming).await, "E23: No alternate file");
}

#[tokio::test]
async fn undo_history_is_independent_per_buffer() {
    let a = temp_file("a", "a1\na2\n");
    let b = temp_file("b", "b1\nb2\n");
    let (rpc, mut incoming) = start().await;

    // Edit buffer a (open a new line), leaving it modified.
    command(&rpc, &format!("e {}", name(&a))).await;
    feed(&rpc, "oINSERTED<Esc>");
    assert_eq!(lines(&rpc).await, vec!["a1", "INSERTED", "a2"]);

    // Switch to b (a stays open, modified). Undo in b touches nothing in b and
    // reports the empty-history message — proving b has its own stack.
    command(&rpc, &format!("e {}", name(&b))).await;
    feed(&rpc, "u");
    assert_eq!(
        message(&rpc, &mut incoming).await,
        "Already at oldest change"
    );
    assert_eq!(lines(&rpc).await, vec!["b1", "b2"]);

    // Back to a: its undo stack is intact, so `u` removes the inserted line.
    command(&rpc, &format!("e {}", name(&a))).await;
    feed(&rpc, "u");
    assert_eq!(lines(&rpc).await, vec!["a1", "a2"]);

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn reediting_the_same_file_honors_the_modified_guard() {
    let a = temp_file("a", "a1\na2\n");
    let (rpc, mut incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await;
    feed(&rpc, "oDIRTY<Esc>");
    assert_eq!(lines(&rpc).await, vec!["a1", "DIRTY", "a2"]);

    // `:e a` on the same, modified file refuses without `!`.
    command(&rpc, &format!("e {}", name(&a))).await;
    assert_eq!(
        message(&rpc, &mut incoming).await,
        "E37: No write since last change (add ! to override)"
    );
    assert_eq!(lines(&rpc).await, vec!["a1", "DIRTY", "a2"]);

    // `:e!` reloads from disk, discarding the change.
    command(&rpc, &format!("e! {}", name(&a))).await;
    assert_eq!(lines(&rpc).await, vec!["a1", "a2"]);

    std::fs::remove_file(&a).ok();
}

#[tokio::test]
async fn enew_opens_an_empty_buffer_and_keeps_the_old_one() {
    let a = temp_file("a", "a1\na2\n");
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await;
    command(&rpc, "enew").await;
    assert_eq!(lines(&rpc).await, vec![""]);

    // The previous file is the alternate, reachable with <C-^>.
    feed(&rpc, "<C-^>");
    assert_eq!(lines(&rpc).await, vec!["a1", "a2"]);

    std::fs::remove_file(&a).ok();
}

#[tokio::test]
async fn ls_lists_open_buffers_with_flags() {
    let a = temp_file("a", "a1\n");
    let b = temp_file("b", "b1\n");
    let (rpc, mut incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await; // buffer 1, becomes alternate
    command(&rpc, &format!("e {}", name(&b))).await; // buffer 2, current

    command(&rpc, "ls").await;
    let (title, rows, selected) = panel(&rpc, &mut incoming)
        .await
        .expect(":ls opens the panel");

    // `:ls` opens the "Buffers" panel: one row per buffer; current is `%a`, the
    // alternate is `#h`.
    assert_eq!(title, "Buffers");
    assert_eq!(rows.len(), 2, "listing was: {rows:?}");
    assert!(
        rows[0].contains("#h") && rows[0].contains(&name(&a)),
        "{rows:?}"
    );
    assert!(
        rows[1].contains("%a") && rows[1].contains(&name(&b)),
        "{rows:?}"
    );
    // The panel opens with the current buffer (row 1, `%a`) selected.
    assert_eq!(selected, 1, "current buffer starts selected");

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn buffer_command_switches_by_number_and_name() {
    let a = temp_file("a", "a1\na2\n");
    let b = temp_file("b", "b1\nb2\n");
    let c = temp_file("c", "c1\nc2\n");
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await; // 1
    command(&rpc, &format!("e {}", name(&b))).await; // 2
    command(&rpc, &format!("e {}", name(&c))).await; // 3
    assert_eq!(list_bufs(&rpc).await, vec![1, 2, 3]);

    command(&rpc, "b 1").await;
    assert_eq!(lines(&rpc).await, vec!["a1", "a2"]);

    // Switch by file-name substring (the full path is unique).
    command(&rpc, &format!("b {}", name(&b))).await;
    assert_eq!(lines(&rpc).await, vec!["b1", "b2"]);

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
    std::fs::remove_file(&c).ok();
}

#[tokio::test]
async fn bnext_and_bprev_wrap_around() {
    let a = temp_file("a", "a\n");
    let b = temp_file("b", "b\n");
    let c = temp_file("c", "c\n");
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await; // 1
    command(&rpc, &format!("e {}", name(&b))).await; // 2
    command(&rpc, &format!("e {}", name(&c))).await; // 3 (current)

    command(&rpc, "bnext").await; // 3 -> wraps to 1
    assert_eq!(lines(&rpc).await, vec!["a"]);
    command(&rpc, "bnext").await; // -> 2
    assert_eq!(lines(&rpc).await, vec!["b"]);
    command(&rpc, "bprev").await; // -> 1
    assert_eq!(lines(&rpc).await, vec!["a"]);
    command(&rpc, "bprev").await; // 1 -> wraps to 3
    assert_eq!(lines(&rpc).await, vec!["c"]);

    command(&rpc, "bfirst").await;
    assert_eq!(lines(&rpc).await, vec!["a"]);
    command(&rpc, "blast").await;
    assert_eq!(lines(&rpc).await, vec!["c"]);

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
    std::fs::remove_file(&c).ok();
}

#[tokio::test]
async fn bdelete_blocks_modified_then_falls_back_to_alternate() {
    let a = temp_file("a", "a1\na2\n");
    let b = temp_file("b", "b1\nb2\n");
    let (rpc, mut incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await; // 1 (alternate)
    command(&rpc, &format!("e {}", name(&b))).await; // 2 (current)
    feed(&rpc, "oDIRTY<Esc>"); // modify b

    // `:bd` refuses the modified current buffer without `!`.
    command(&rpc, "bd").await;
    assert!(
        message(&rpc, &mut incoming).await.starts_with("E89"),
        "expected E89"
    );
    assert_eq!(list_bufs(&rpc).await, vec![1, 2]);

    // `:bd!` deletes it and falls back to the alternate (a).
    command(&rpc, "bd!").await;
    assert_eq!(list_bufs(&rpc).await, vec![1]);
    assert_eq!(lines(&rpc).await, vec!["a1", "a2"]);

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn bdelete_last_buffer_leaves_a_fresh_no_name() {
    let (rpc, _incoming) = start().await; // single empty [No Name] buffer (1)
    command(&rpc, "bd").await;

    // A new empty buffer takes its place — never zero buffers.
    let bufs = list_bufs(&rpc).await;
    assert_eq!(bufs.len(), 1);
    assert_ne!(bufs[0], 1, "the deleted id is not reused");
    assert_eq!(lines(&rpc).await, vec![""]);
    assert_eq!(buf_name(&rpc, 0).await, "");
}

#[tokio::test]
async fn buffer_rpc_api_lists_reads_switches_and_creates() {
    let a = temp_file("a", "a1\na2\n");
    let b = temp_file("b", "b1\nb2\n");
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await; // 1
    command(&rpc, &format!("e {}", name(&b))).await; // 2 (current)

    assert_eq!(list_bufs(&rpc).await, vec![1, 2]);
    assert_eq!(current_buf(&rpc).await, 2);
    assert_eq!(buf_name(&rpc, 1).await, name(&a));
    // Read a non-current buffer by handle.
    assert_eq!(buf_lines(&rpc, 1).await, vec!["a1", "a2"]);

    set_current_buf(&rpc, 1).await;
    assert_eq!(current_buf(&rpc).await, 1);
    assert_eq!(lines(&rpc).await, vec!["a1", "a2"]);

    // create_buf adds a buffer without switching to it.
    let new = create_buf(&rpc).await;
    assert_eq!(new, 3);
    assert_eq!(current_buf(&rpc).await, 1);
    assert_eq!(list_bufs(&rpc).await, vec![1, 2, 3]);

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn wall_writes_every_modified_buffer() {
    let a = temp_file("a", "a1\n");
    let b = temp_file("b", "b1\n");
    let (rpc, _incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await;
    feed(&rpc, "oAAA<Esc>");
    command(&rpc, &format!("e {}", name(&b))).await;
    feed(&rpc, "oBBB<Esc>");

    command(&rpc, "wall").await;

    // Both files are persisted to disk with their edits.
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "a1\nAAA\n");
    assert_eq!(std::fs::read_to_string(&b).unwrap(), "b1\nBBB\n");

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn quit_warns_and_shows_a_modified_buffer_instead_of_losing_it() {
    // `:q` quits the editor only when nothing is unsaved; if a buffer is
    // modified it switches the window to that buffer and warns (E37), rather
    // than quitting and dropping the change.
    let a = temp_file("a", "a1\na2\n");
    let b = temp_file("b", "b1\nb2\n");
    let (rpc, mut incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await; // buffer 1
    feed(&rpc, "oAAA<Esc>"); // a modified
    command(&rpc, &format!("e {}", name(&b))).await; // buffer 2 (current, clean)

    // `:q` from the clean buffer b: a is still modified, so it must not quit.
    // It surfaces a (switching to it) and warns.
    command(&rpc, "q").await;
    let msg = message(&rpc, &mut incoming).await;
    assert!(msg.starts_with("E37"), "expected E37, got {msg:?}");
    assert_eq!(
        lines(&rpc).await,
        vec!["a1", "AAA", "a2"],
        "`:q` should switch to and show the modified buffer"
    );
    assert_eq!(
        list_bufs(&rpc).await,
        vec![1, 2],
        "nothing should be closed"
    );

    // Now showing the modified buffer a; `:q` again still warns (a is current).
    command(&rpc, "q").await;
    assert!(message(&rpc, &mut incoming).await.starts_with("E37"));

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn ls_enter_jumps_to_the_selected_buffer() {
    let a = temp_file("a", "a1\n");
    let b = temp_file("b", "b1\n");
    let (rpc, mut incoming) = start().await;

    command(&rpc, &format!("e {}", name(&a))).await; // buffer 1
    command(&rpc, &format!("e {}", name(&b))).await; // buffer 2 (current)

    // `:ls` opens the focused buffer panel with the current buffer (buffer 2,
    // row 1) selected; its rows are id-sorted, so `k` moves up to buffer 1 (a).
    // `<CR>` then jumps to that buffer and dismisses the list.
    command(&rpc, "ls").await;
    feed(&rpc, "k<CR>");

    assert_eq!(
        current_buf(&rpc).await,
        1,
        "selected buffer becomes current"
    );
    assert_eq!(lines(&rpc).await, vec!["a1"]);
    assert!(
        panel(&rpc, &mut incoming).await.is_none(),
        "selecting a buffer closes the list"
    );

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}
