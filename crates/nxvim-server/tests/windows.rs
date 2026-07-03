//! Behavior tests for window equalization — `<C-w>=` (`'equalize'`) and the
//! `'equalalways'` auto-balance on split / close — driven black-box over RPC,
//! exactly like `tabs.rs` / `dock.rs`.
//!
//! The interesting case is a *nested, lopsided* layout: a window split, then one
//! of its halves split again, so the tree is `A | (C | B)`. With even per-split
//! weights the outer split stays 50/50 and `A` ends as wide as the whole `C | B`
//! column — the latent `<C-w>=` bug. Equalizing by leaf-count weight (and doing
//! it automatically under `'equalalways'`) gives three equal columns instead.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, feed, start_attached};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Start an 80×24 server on its own thread and return a connected client.
async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

async fn req(rpc: &Rpc, method: &str, args: Vec<Value>) -> Value {
    rpc.request(method, args).await.expect(method)
}

/// The open window handles (`nvim_list_wins`, layout order).
async fn win_handles(rpc: &Rpc) -> Vec<u64> {
    match req(rpc, "nvim_list_wins", vec![]).await {
        Value::Array(a) => a.iter().map(|x| x.as_u64().expect("u64 handle")).collect(),
        v => panic!("expected an array of window handles, got {v:?}"),
    }
}

/// The cell width of each open window, in `nvim_list_wins` order.
async fn win_widths(rpc: &Rpc) -> Vec<u64> {
    let mut out = Vec::new();
    for w in win_handles(rpc).await {
        let v = req(rpc, "nvim_win_get_width", vec![Value::from(w)]).await;
        out.push(v.as_u64().expect("u64 width"));
    }
    out
}

/// The cell height (text rows) of each open window, in `nvim_list_wins` order.
async fn win_heights(rpc: &Rpc) -> Vec<u64> {
    let mut out = Vec::new();
    for w in win_handles(rpc).await {
        let v = req(rpc, "nvim_win_get_height", vec![Value::from(w)]).await;
        out.push(v.as_u64().expect("u64 height"));
    }
    out
}

/// Spread (max − min) of a size set — small once windows are equalized (a couple
/// cells of rounding/border slop is unavoidable in a nested tree, as in vim),
/// large while lopsided.
fn spread(sizes: &[u64]) -> u64 {
    let max = sizes.iter().copied().max().unwrap_or(0);
    let min = sizes.iter().copied().min().unwrap_or(0);
    max - min
}

/// Build the lopsided `A | (C | B)` three-column layout: vsplit, move into the
/// right half, vsplit again. The caller controls `'equalalways'` beforehand.
async fn build_nested_vsplits(rpc: &Rpc) {
    feed(rpc, "<C-w>v"); // A | B, focus the new left window A
    feed(rpc, "<C-w>l"); // move focus into the right window B
    feed(rpc, "<C-w>v"); // B becomes C | B → tree is A | (C | B)
                         // Barrier so every queued split has converged before we measure.
    let _ = req(rpc, "nvim_get_mode", vec![]).await;
}

#[tokio::test]
async fn set_current_focuses_the_requested_window() {
    let (rpc, _incoming) = start().await;
    feed(&rpc, "<C-w>v"); // two windows
    let wins = win_handles(&rpc).await;
    assert_eq!(wins.len(), 2, "a split yields two windows");
    let cur = req(&rpc, "nvim_get_current_win", vec![])
        .await
        .as_u64()
        .expect("current win id");
    let other = *wins
        .iter()
        .find(|&&w| w != cur)
        .expect("an unfocused window");

    // The new public focus API moves the current window to `other`.
    exec_lua(&rpc, &format!("nx.win.set_current({other})")).await;
    let now = req(&rpc, "nvim_get_current_win", vec![])
        .await
        .as_u64()
        .expect("current win id");
    assert_eq!(
        now, other,
        "nx.win.set_current focuses the requested window"
    );
}

#[tokio::test]
async fn equalize_balances_a_nested_lopsided_layout() {
    let (rpc, _incoming) = start().await;

    // Build the layout WITHOUT auto-equalize so the lopsidedness is observable.
    feed(&rpc, ":set noequalalways<CR>");
    build_nested_vsplits(&rpc).await;

    // Before `<C-w>=`: A keeps ~half the screen, C and B share the other half —
    // a clearly uneven three-column layout.
    let before = win_widths(&rpc).await;
    assert_eq!(before.len(), 3, "three tiled windows");
    assert!(
        spread(&before) >= 10,
        "the nested split is lopsided before equalizing: {before:?}"
    );

    // `<C-w>=` weights each split by its leaf count, so all three columns come
    // out within a cell of each other.
    feed(&rpc, "<C-w>=");
    let after = win_widths(&rpc).await;
    assert!(
        spread(&after) <= 2,
        "`<C-w>=` equalizes the nested layout to even columns: {after:?}"
    );
}

#[tokio::test]
async fn equalalways_on_balances_each_split_automatically() {
    let (rpc, _incoming) = start().await;

    // Default `'equalalways'` is on, so building the same nested layout never
    // goes lopsided — every split re-equalizes the whole tree.
    build_nested_vsplits(&rpc).await;

    let widths = win_widths(&rpc).await;
    assert_eq!(widths.len(), 3, "three tiled windows");
    assert!(
        spread(&widths) <= 2,
        "'equalalways' keeps the nested splits even: {widths:?}"
    );
}

#[tokio::test]
async fn equalalways_off_leaves_each_split_lopsided() {
    let (rpc, _incoming) = start().await;

    // The option actually gates the behavior: with it off the nested layout
    // stays uneven until an explicit `<C-w>=`.
    feed(&rpc, ":set noequalalways<CR>");
    build_nested_vsplits(&rpc).await;

    let widths = win_widths(&rpc).await;
    assert!(
        spread(&widths) >= 10,
        "'noequalalways' keeps the classic carve-from-one-neighbor sizing: {widths:?}"
    );
}

#[tokio::test]
async fn equalalways_rebalances_on_window_close() {
    let (rpc, _incoming) = start().await;

    // Nested `A | (C | B)`, auto-equalized to three even columns; focus lands on
    // the middle window C (the inner split's new left half).
    build_nested_vsplits(&rpc).await;
    assert_eq!(win_widths(&rpc).await.len(), 3);

    // Closing C collapses the inner split, so B would inherit the *whole* right
    // two-thirds (a ~27 | ~52 layout) without rebalancing. 'equalalways' instead
    // re-equalizes, leaving A and B as even halves.
    feed(&rpc, "<C-w>c");
    let after = win_widths(&rpc).await;
    assert_eq!(after.len(), 2, "one window closed");
    assert!(
        spread(&after) <= 1,
        "'equalalways' re-equalizes the survivors after a close: {after:?}"
    );
}

#[tokio::test]
async fn equalize_balances_nested_horizontal_splits() {
    let (rpc, _incoming) = start().await;

    // The same fix in the other axis: A / (C / B) stacked rows. `<C-w>s` is a
    // horizontal split; `<C-w>j` moves into the lower window.
    feed(&rpc, ":set noequalalways<CR>");
    feed(&rpc, "<C-w>s");
    feed(&rpc, "<C-w>j");
    feed(&rpc, "<C-w>s");
    let _ = req(&rpc, "nvim_get_mode", vec![]).await;

    let before = win_heights(&rpc).await;
    assert_eq!(before.len(), 3, "three stacked windows");
    assert!(
        spread(&before) >= 5,
        "the nested horizontal split is lopsided before equalizing: {before:?}"
    );

    feed(&rpc, "<C-w>=");
    let after = win_heights(&rpc).await;
    assert!(
        spread(&after) <= 1,
        "`<C-w>=` equalizes stacked rows by leaf count too: {after:?}"
    );
}

/// Closing the last *tiled* window by id while a **float** holds focus closes
/// the floats and keeps the tiled window (neovim's rule) — and must re-point
/// focus at the survivor. It used to leave `windows.current` dangling on the
/// freed float id, poisoning every later current-window lookup.
#[tokio::test]
async fn closing_last_tiled_window_with_focused_float_refocuses_the_survivor() {
    let (rpc, _incoming) = start().await;
    let tiled = req(&rpc, "nvim_get_current_win", vec![])
        .await
        .as_u64()
        .expect("current win handle");
    // Open a float and give it focus (`enter = true`).
    let config = Value::Map(vec![
        (Value::from("relative"), Value::from("editor")),
        (Value::from("row"), Value::from(2u64)),
        (Value::from("col"), Value::from(3u64)),
        (Value::from("width"), Value::from(20u64)),
        (Value::from("height"), Value::from(5u64)),
    ]);
    let float = req(
        &rpc,
        "nvim_open_win",
        vec![Value::from(0u64), Value::from(true), config],
    )
    .await
    .as_u64()
    .expect("float handle");
    assert_ne!(float, tiled);
    assert_eq!(
        req(&rpc, "nvim_get_current_win", vec![])
            .await
            .as_u64()
            .expect("current"),
        float,
        "the float took focus"
    );

    // Close the *tiled* window while the float is focused: the floats close
    // instead and the tiled window survives.
    req(
        &rpc,
        "nvim_win_close",
        vec![Value::from(tiled), Value::from(false)],
    )
    .await;
    assert_eq!(
        win_handles(&rpc).await,
        vec![tiled],
        "the tiled window survives; the floats are closed"
    );
    assert_eq!(
        req(&rpc, "nvim_get_current_win", vec![])
            .await
            .as_u64()
            .expect("current"),
        tiled,
        "focus fell back to the surviving tiled window, not the freed float id"
    );
    // The editor is still fully operable.
    feed(&rpc, "iok<Esc>");
    let cur = req(&rpc, "nvim_win_get_cursor", vec![Value::from(tiled)]).await;
    assert!(matches!(cur, Value::Array(_)), "server still answers");
}
