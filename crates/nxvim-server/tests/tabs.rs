//! Behavior tests for tab pages, driven the way a real client drives the editor
//! (black-box RPC, exactly like `windows.rs` / `editing.rs`).
//!
//! Phase 1 is the model plus the **read-only** `nvim_tabpage_*` surface: the
//! editor always holds at least one tab, and the introspection RPCs resolve
//! against it. There is no creation/switch surface yet (`:tabnew` and friends
//! land in Phase 2), so these tests pin the single seeded tab and its
//! relationship to the window surface.

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
        let init = ServerInit::default();
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

/// Issue a request and return the raw reply.
async fn req(rpc: &Rpc, method: &str, args: Vec<Value>) -> Value {
    rpc.request(method, args).await.expect(method)
}

/// Decode a reply that is a single u64 handle.
fn handle(v: &Value) -> u64 {
    v.as_u64().expect("u64 handle")
}

/// Decode a reply that is an array of u64 handles.
fn handles(v: &Value) -> Vec<u64> {
    match v {
        Value::Array(a) => a.iter().map(|x| x.as_u64().expect("u64")).collect(),
        _ => panic!("expected an array, got {v:?}"),
    }
}

/// Type a string of vim key-notation.
fn feed(rpc: &Rpc, keys: &str) {
    rpc.notify("nvim_input", vec![Value::from(keys)]);
}

#[tokio::test]
async fn one_tab_exists_at_startup_and_is_current() {
    let (rpc, _incoming) = start().await;

    let tabs = handles(&req(&rpc, "nvim_list_tabpages", vec![]).await);
    assert_eq!(tabs, vec![1], "exactly one tab, handle 1, at startup");

    let current = handle(&req(&rpc, "nvim_get_current_tabpage", vec![]).await);
    assert_eq!(current, 1, "the seeded tab is current");
}

#[tokio::test]
async fn tabpage_validity_and_number() {
    let (rpc, _incoming) = start().await;

    // The seeded tab is valid and is tab number 1.
    let valid = req(&rpc, "nvim_tabpage_is_valid", vec![Value::from(1u64)]).await;
    assert_eq!(valid, Value::from(true), "tab 1 is valid");
    let number = handle(&req(&rpc, "nvim_tabpage_get_number", vec![Value::from(1u64)]).await);
    assert_eq!(number, 1, "tab 1 is the first (and only) tab");

    // Handle 0 resolves to the current tab.
    let cur_number = handle(&req(&rpc, "nvim_tabpage_get_number", vec![Value::from(0u64)]).await);
    assert_eq!(cur_number, 1, "handle 0 == current tab, number 1");

    // An unknown handle is invalid and has no number.
    let unknown = req(&rpc, "nvim_tabpage_is_valid", vec![Value::from(99u64)]).await;
    assert_eq!(unknown, Value::from(false), "tab 99 does not exist");
    let unknown_num = handle(&req(&rpc, "nvim_tabpage_get_number", vec![Value::from(99u64)]).await);
    assert_eq!(unknown_num, 0, "an unknown tab reports number 0");
}

#[tokio::test]
async fn current_tab_lists_the_window_set() {
    let (rpc, _incoming) = start().await;

    // With one window, the tab lists exactly that window, and it is the tab's
    // focused window — matching the global window surface.
    let wins = handles(&req(&rpc, "nvim_tabpage_list_wins", vec![Value::from(0u64)]).await);
    let global = handles(&req(&rpc, "nvim_list_wins", vec![]).await);
    assert_eq!(wins, global, "the current tab owns every open window");

    let tab_cur = handle(&req(&rpc, "nvim_tabpage_get_win", vec![Value::from(0u64)]).await);
    let global_cur = handle(&req(&rpc, "nvim_get_current_win", vec![]).await);
    assert_eq!(
        tab_cur, global_cur,
        "the tab's focused window is the current window"
    );
}

#[tokio::test]
async fn splits_grow_the_current_tabs_window_set() {
    let (rpc, _incoming) = start().await;

    // Splitting adds windows to the (only) tab — there is still one tab, now
    // with two windows under it.
    feed(&rpc, "<Esc><C-w>s");
    let wins = handles(&req(&rpc, "nvim_tabpage_list_wins", vec![Value::from(0u64)]).await);
    assert_eq!(wins.len(), 2, "the split's two windows both live in tab 1");

    let tabs = handles(&req(&rpc, "nvim_list_tabpages", vec![]).await);
    assert_eq!(tabs, vec![1], "splitting does not create a tab");

    // The tab's focused window follows the live focus.
    let tab_cur = handle(&req(&rpc, "nvim_tabpage_get_win", vec![Value::from(0u64)]).await);
    let global_cur = handle(&req(&rpc, "nvim_get_current_win", vec![]).await);
    assert_eq!(
        tab_cur, global_cur,
        "tab focus tracks the focused window after a split"
    );
}
