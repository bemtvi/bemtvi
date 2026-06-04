//! Behavior tests for the key-mapping engine (`vim.keymap.set`), driven
//! black-box over RPC exactly like `editing.rs` / `autocmds.rs`. Phase 1 proves
//! the server-side withhold/replay matcher and the headline normal-mode surface:
//! a function or string RHS fires on its LHS, the matched keys don't *also* reach
//! the editor, a multi-key map's prefix is replayed intact when the sequence
//! turns out not to match, and a re-`set` of the same LHS wins (last-set-wins).
//!
//! Observability follows the autocmd tests: a function RHS that `print`s a marker
//! lands it on the message line; a string RHS / unmapped key is observed through
//! buffer contents and the cursor. Integration-test files don't share a module,
//! so the `start*/feed/...` helpers are copied from the established pattern.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server on its own thread, sourcing `init_lua` from a throwaway config
/// dir (also the runtimepath), and return a connected client.
async fn start_with_config(
    dir: &std::path::Path,
    init_lua: &str,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(dir.join("init.lua"), init_lua).expect("write init.lua");
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    };
    let (server_end, client_end) = tokio::io::duplex(1 << 16);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("server runtime");
        let _ = runtime.block_on(run_server(server_end, init));
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

/// Type a string of vim key-notation (fire-and-forget notification).
fn feed(rpc: &Rpc, keys: &str) {
    rpc.notify("nvim_input", vec![Value::from(keys)]);
}

/// Feed `keys`, then deterministically return the `redraw` map the server emitted
/// for that input (the serial-ordering trick from `autocmds.rs`).
async fn redraw_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> Vec<(Value, Value)> {
    while incoming.try_recv().is_ok() {} // drop notifications buffered earlier
    rpc.request("nvim_input", vec![Value::from(keys)])
        .await
        .expect("input");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    loop {
        match incoming.try_recv() {
            Ok(Incoming::Notification { method, params }) if method == "redraw" => {
                match params.into_iter().next() {
                    Some(Value::Map(map)) => return map,
                    _ => panic!("redraw without a map"),
                }
            }
            Ok(_) => continue,
            Err(_) => panic!("no redraw arrived for {keys:?}"),
        }
    }
}

fn field<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// The message line from a redraw map.
fn message(map: &[(Value, Value)]) -> String {
    field(map, "message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Fetch all buffer lines. Awaiting it also barriers: every message sent before
/// it has been processed by the server.
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

/// The (1-based line, 0-based col) cursor position.
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

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("nxvim_test_{tag}_{}_{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A function-RHS map fires on its sequence, and the keys it consumed do **not**
/// also reach the editor (the `<Space>` and `x` would otherwise move/delete).
#[tokio::test]
async fn function_map_fires_and_withholds_its_keys() {
    let dir = temp_dir("keymap_fn");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', '<Space>x', function() print('MAPPED') end)\n",
    )
    .await;

    // Put known text in the buffer; `x` on it would delete a char if it leaked.
    feed(&rpc, "ihello<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    let redraw = redraw_after(&rpc, &mut incoming, "<Space>x").await;
    assert_eq!(message(&redraw), "MAPPED", "the mapping's function ran");
    assert_eq!(
        lines(&rpc).await,
        vec!["hello"],
        "neither <Space> nor x reached the editor"
    );
}

/// A `noremap` string RHS is fed straight to the editor: `Y` → `y$` yanks to
/// end-of-line, observable by pasting it back.
#[tokio::test]
async fn string_map_is_fed_to_the_editor() {
    let dir = temp_dir("keymap_str");
    let (rpc, _incoming) = start_with_config(&dir, "vim.keymap.set('n', 'Y', 'y$')\n").await;

    feed(&rpc, "ihello<Esc>0");
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    // `Y` fires `y$` (yank "hello"); `P` pastes it before the cursor (col 0).
    feed(&rpc, "YP");
    assert_eq!(
        lines(&rpc).await,
        vec!["hellohello"],
        "Y mapped to y$ yanked the line, then P pasted it"
    );
}

/// A multi-key map fires only on the full sequence.
#[tokio::test]
async fn multikey_map_fires_on_full_sequence() {
    let dir = temp_dir("keymap_multi");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'gh', function() print('GH') end)\n",
    )
    .await;

    feed(&rpc, "ihello<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    let redraw = redraw_after(&rpc, &mut incoming, "gh").await;
    assert_eq!(message(&redraw), "GH", "gh fired its mapping");
    assert_eq!(lines(&rpc).await, vec!["hello"], "g/h did not reach editor");
}

/// The withhold/replay engine: with `gh` mapped, an unmapped `g`-sequence still
/// reaches the editor intact — the withheld `g` is replayed, so core's `gg`
/// (go-to-top) still happens. (This is the exact behavior the LSP backport reuses
/// for `gd` vs `gg`.) With no input timer a trailing live-prefix stays buffered
/// until the next key flushes it, so a final motion (`0`) is sent to flush it.
#[tokio::test]
async fn unmapped_prefix_sequence_reaches_the_editor() {
    let dir = temp_dir("keymap_replay");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'gh', function() print('GH') end)\n",
    )
    .await;

    // Three lines; cursor ends on the last after the insert.
    feed(&rpc, "iline1<CR>line2<CR>line3<Esc>");
    assert_eq!(lines(&rpc).await, vec!["line1", "line2", "line3"]);
    assert_eq!(cursor(&rpc).await.0, 3, "cursor starts on the last line");

    // `gg` → go to the first line. The trailing `g` is a live prefix of `gh`;
    // the `0` both proves the replay and flushes that buffered `g`.
    feed(&rpc, "gg0");
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "the replayed g's executed gg (go-to-top), then 0 to column 0"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["line1", "line2", "line3"],
        "the g-sequence was motion only; the buffer is untouched"
    );
}

/// Re-`set`ting the same `(mode, lhs)` replaces the prior mapping (last-set-wins).
/// (The *user > default* rung is exercised on the LSP backport, where defaults
/// first exist.)
#[tokio::test]
async fn last_set_mapping_wins() {
    let dir = temp_dir("keymap_last");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', '<Space>p', function() print('FIRST') end)\n\
         vim.keymap.set('n', '<Space>p', function() print('SECOND') end)\n",
    )
    .await;

    let redraw = redraw_after(&rpc, &mut incoming, "<Space>p").await;
    assert_eq!(
        message(&redraw),
        "SECOND",
        "the later mapping shadows the earlier"
    );
}
