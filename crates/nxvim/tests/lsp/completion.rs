//! LSP Phase 5: completion (the popup menu) — capability advertisement,
//! resolve, the doc preview, mouse navigation, ranking/filtering, accept, and
//! resilience.

use crate::support::*;
use ratatui::style::Modifier;

// ----- pmenu helpers (completion-only) --------------------------------------

/// The `pmenu` redraw key as `(labels, selected)`, or `None` when no popup is
/// open (the key is `Nil`). `selected` is `-1` until the user navigates.
fn pmenu_of(params: &[Value]) -> Option<(Vec<String>, i64)> {
    let Value::Map(map) = params.first()? else {
        return None;
    };
    let pmenu = map
        .iter()
        .find(|(k, _)| k.as_str() == Some("pmenu"))?
        .1
        .clone();
    let Value::Map(pm) = pmenu else {
        return None; // Nil ⇒ no popup
    };
    let get = |key: &str| {
        pm.iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v)
    };
    let labels = get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|it| it.as_array()?.first()?.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let selected = get("selected").and_then(Value::as_i64).unwrap_or(-1);
    Some((labels, selected))
}

/// The popup items' `detail` column (the 3rd element of each `[label, kind,
/// detail]` item), in visible order. The surface a `completionItem/resolve`-filled
/// `detail` becomes observable on, since `pmenu_value` already projects it.
fn pmenu_details(params: &[Value]) -> Vec<String> {
    let Some(Value::Map(map)) = params.first() else {
        return Vec::new();
    };
    let Some((_, Value::Map(pm))) = map.iter().find(|(k, _)| k.as_str() == Some("pmenu")) else {
        return Vec::new();
    };
    pm.iter()
        .find(|(k, _)| k.as_str() == Some("items"))
        .and_then(|(_, v)| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|it| it.as_array()?.get(2)?.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The popup's `doc` lines — the selected item's documentation projected for the
/// preview box. Empty when nothing is selected or the item has no docs.
fn pmenu_doc(params: &[Value]) -> Vec<String> {
    let Some(Value::Map(map)) = params.first() else {
        return Vec::new();
    };
    let Some((_, Value::Map(pm))) = map.iter().find(|(k, _)| k.as_str() == Some("pmenu")) else {
        return Vec::new();
    };
    pm.iter()
        .find(|(k, _)| k.as_str() == Some("doc"))
        .and_then(|(_, v)| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Poll until a redraw whose `pmenu` satisfies `pred` arrives, returning it.
async fn wait_for_pmenu_where(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    pred: impl Fn(&(Vec<String>, i64)) -> bool,
) -> Vec<Value> {
    for _ in 0..60 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            if pmenu_of(&params).as_ref().is_some_and(&pred) {
                return params;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the completion popup never reached the expected state");
}

/// Poll until the completion popup is open, returning that redraw's params.
async fn wait_for_pmenu(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> Vec<Value> {
    wait_for_pmenu_where(rpc, incoming, |_| true).await
}

/// Poll until the popup's item labels equal `want`, asserting it stays open the
/// whole time (no drained redraw shows `pmenu: Nil`) — i.e. it refreshes in
/// place rather than closing and reopening.
async fn wait_for_pmenu_items(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    want: &[&str],
) {
    for _ in 0..60 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        for params in drain_all_redraws(incoming) {
            let Some((labels, _)) = pmenu_of(&params) else {
                panic!("the completion popup closed during a live refresh");
            };
            if labels.iter().map(String::as_str).eq(want.iter().copied()) {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the popup items never became {want:?}");
}

/// Poll until a redraw shows the popup closed (`pmenu: Nil`).
async fn wait_for_pmenu_closed(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) {
    for _ in 0..40 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            if pmenu_of(&params).is_none() {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the completion popup never closed");
}

/// A bare `CompletionItem` (just a label).
fn citem(label: &str) -> Json {
    serde_json::json!({ "label": label })
}

/// A `CompletionItem[]` response (a `CompletionResponse::Array`, always complete).
fn completion_items(labels: &[&str]) -> Json {
    serde_json::json!(labels.iter().map(|l| citem(l)).collect::<Vec<_>>())
}

/// A `CompletionList` response with the given `isIncomplete` flag.
fn completion_list(incomplete: bool, labels: &[&str]) -> Json {
    serde_json::json!({
        "isIncomplete": incomplete,
        "items": labels.iter().map(|l| citem(l)).collect::<Vec<_>>(),
    })
}

#[tokio::test]
async fn completion_capability_advertises_documentation_and_resolve() {
    let _guard = test_lock().lock().await;
    // Phase 1: nxvim must declare `completion.completionItem` at `initialize` so
    // servers that gate per-item docs on the capability send them, and so the
    // lazy `completionItem/resolve` round-trip (Phase 2) is on the table. The mock
    // records the `initialize` request verbatim, so the advertised capability is
    // observable end to end.
    let record = configure_mock(
        "compl-cap",
        serde_json::json!({ "completion": completion_items(&["alpha"]) }),
    );
    let file = temp_file("compl-cap", "rs", "fn main() {}\n");
    let (rpc, _incoming) = start(Some(file)).await;
    // didOpen is sent only after `initialize`/`initialized`, so waiting on it
    // guarantees the handshake — and the recorded `initialize` — is present.
    let recs = wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    let init = find(&recs, "initialize").expect("the initialize request was recorded");
    let completion_item =
        &init["params"]["capabilities"]["textDocument"]["completion"]["completionItem"];
    assert!(
        !completion_item.is_null(),
        "initialize advertises completion.completionItem; got {init:#}"
    );

    let formats = completion_item["documentationFormat"]
        .as_array()
        .expect("documentationFormat is advertised");
    assert!(
        formats.iter().any(|f| f == "markdown") && formats.iter().any(|f| f == "plaintext"),
        "documentationFormat accepts markdown and plaintext; got {formats:?}"
    );

    let resolvable = completion_item["resolveSupport"]["properties"]
        .as_array()
        .expect("resolveSupport.properties is advertised");
    assert!(
        resolvable.iter().any(|p| p == "documentation") && resolvable.iter().any(|p| p == "detail"),
        "resolveSupport lists documentation and detail; got {resolvable:?}"
    );
}

#[tokio::test]
async fn a_documented_completion_item_opens_the_menu() {
    let _guard = test_lock().lock().await;
    // Phase 1: an item carrying inline `documentation` (a MarkupContent block) and
    // a `data` blob distills cleanly and rides the reply into the menu — the new
    // fields don't perturb the candidate path. The doc *text* gets its own preview
    // surface in Phase 3; here we prove the documented item reaches the menu like
    // any other (it opens, with the item's label).
    let record = configure_mock(
        "compl-doc",
        serde_json::json!({
            "completion": [{
                "label": "connect",
                "detail": "fn() -> Conn",
                "data": { "id": 42 },
                "documentation": {
                    "kind": "markdown",
                    "value": "Opens a connection.\n\n# Errors\nFails when unreachable.\n"
                }
            }]
        }),
    );
    let file = temp_file("compl-doc", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "ocon<C-x><C-o>");
    let params = wait_for_pmenu(&rpc, &mut incoming).await;
    assert_eq!(
        pmenu_of(&params).unwrap().0,
        vec!["connect"],
        "the documented item opens the menu like any other candidate"
    );
}

#[tokio::test]
async fn selecting_a_docless_item_resolves_it_and_merges_the_result() {
    let _guard = test_lock().lock().await;
    // Phase 2: the list item carries no `documentation`/`detail` (only `data`) —
    // the rust_analyzer shape. Selecting it must fire `completionItem/resolve`
    // round-tripping the *original* item (so the server recognizes its `data`),
    // and the resolved `detail` must merge into the open menu off-tick (the
    // resolved `documentation` lands in `raw` too, surfaced by the Phase 3 preview).
    let record = configure_mock(
        "compl-resolve",
        serde_json::json!({
            "completion": [{ "label": "connect", "data": { "id": 7 } }],
            "completion_resolve": {
                "label": "connect",
                "data": { "id": 7 },
                "detail": "fn() -> Conn",
                "documentation": { "kind": "markdown", "value": "Opens a connection." }
            }
        }),
    );
    let file = temp_file("compl-resolve", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Open the menu (empty prefix → the single item) and select it.
    feed(&rpc, "o<C-x><C-o>");
    wait_for_pmenu(&rpc, &mut incoming).await;
    feed(&rpc, "<C-n>");

    // The resolved detail merges into the menu off-tick (a tick after selection).
    let mut merged = false;
    for _ in 0..60 {
        barrier(&rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(&mut incoming) {
            if pmenu_details(&params).iter().any(|d| d == "fn() -> Conn") {
                merged = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        merged,
        "the resolved `detail` merged into the open menu after selection"
    );

    // The resolve round-tripped the original item verbatim — crucially its `data`,
    // which rust_analyzer matches the resolve against.
    let recs = record_lines(&record);
    let resolve = find(&recs, "completionItem/resolve").expect("completionItem/resolve was issued");
    assert_eq!(
        resolve["params"]["label"], "connect",
        "resolve carries the original item"
    );
    assert_eq!(
        resolve["params"]["data"],
        serde_json::json!({ "id": 7 }),
        "resolve round-trips the original item's data blob"
    );
}

#[tokio::test]
async fn a_completion_resolve_failure_leaves_the_item_docless() {
    let _guard = test_lock().lock().await;
    // Phase 2 fail-loud: with no `completion_resolve` scripted the mock replies
    // `null`, which can't deserialize into a `CompletionItem` — a resolve failure.
    // It must be logged (not faked) and leave the item exactly as it was: still in
    // the menu, still with no `detail`/docs. The menu doesn't break.
    let record = configure_mock(
        "compl-resolve-fail",
        serde_json::json!({
            "completion": [{ "label": "connect", "data": { "id": 7 } }],
        }),
    );
    let file = temp_file("compl-resolve-fail", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "o<C-x><C-o>");
    wait_for_pmenu(&rpc, &mut incoming).await;
    feed(&rpc, "<C-n>");

    // Selecting the docless item still issues the resolve.
    wait_for_record(&rpc, &record, |r| has_method(r, "completionItem/resolve")).await;
    // Let the failed reply land and be dropped.
    for _ in 0..5 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    // The menu is still open and the item still carries no detail (no fake doc).
    let params = wait_for_pmenu(&rpc, &mut incoming).await;
    assert_eq!(
        pmenu_of(&params).unwrap().0,
        vec!["connect"],
        "the item stays in the menu after a failed resolve"
    );
    assert!(
        pmenu_details(&params).iter().all(|d| d.is_empty()),
        "a failed resolve leaves the item with no detail (and no faked docs)"
    );
}

#[tokio::test]
async fn selecting_a_documented_item_shows_a_doc_preview() {
    let _guard = test_lock().lock().await;
    // Phase 3: the selected item's documentation is projected as `pmenu.doc`, and
    // the client floats it in a preview box beside the popup. No selection ⇒ no
    // preview; selecting the documented item fills it (and the box paints).
    let record = configure_mock(
        "compl-preview",
        serde_json::json!({
            "completion": [{
                "label": "connect",
                "detail": "fn() -> Conn",
                "documentation": {
                    "kind": "markdown",
                    "value": "Opens a connection.\nReturns a handle."
                }
            }]
        }),
    );
    let file = temp_file("compl-preview", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "o<C-x><C-o>");
    let params = wait_for_pmenu(&rpc, &mut incoming).await;
    assert!(
        pmenu_doc(&params).is_empty(),
        "nothing selected ⇒ no documentation preview"
    );

    feed(&rpc, "<C-n>");
    let params = wait_for_pmenu_where(&rpc, &mut incoming, |(_, sel)| *sel == 0).await;
    assert_eq!(
        pmenu_doc(&params),
        vec!["Opens a connection.", "Returns a handle."],
        "the selected item's documentation rides the pmenu redraw as `doc` lines"
    );

    // The real client paints the docs in a bordered preview box beside the popup.
    let buf = paint(&View::from_redraw(&params), COLS, ROWS);
    let row_text = |y: u16| {
        (0..COLS)
            .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
            .collect::<String>()
    };
    assert!(
        (0..ROWS).any(|y| row_text(y).contains("Opens a connection.")),
        "the preview box paints the documentation text beside the popup"
    );
}

#[tokio::test]
async fn the_doc_preview_scrolls_with_the_mouse_wheel() {
    let _guard = test_lock().lock().await;
    // A documentation block taller than the capped preview box can be scrolled
    // (the client owns the box height, so this is a pure client-side offset). The
    // box shows the top by default and a later slice once scrolled to the bottom.
    let doc_value = (1..=20)
        .map(|i| format!("line {i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let record = configure_mock(
        "compl-scroll",
        serde_json::json!({
            "completion": [{
                "label": "connect",
                "documentation": { "kind": "markdown", "value": doc_value }
            }]
        }),
    );
    let file = temp_file("compl-scroll", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "o<C-x><C-o>");
    wait_for_pmenu(&rpc, &mut incoming).await;
    feed(&rpc, "<C-n>");
    let params = wait_for_pmenu_where(&rpc, &mut incoming, |(_, sel)| *sel == 0).await;
    let view = View::from_redraw(&params);

    // The 20-line doc overflows the capped box, so there's content to scroll to.
    let (bx, by, bw, _bh, max_scroll) =
        pmenu_doc_geometry(COLS, ROWS, &view).expect("the doc preview box has geometry");
    assert!(
        max_scroll > 0,
        "a doc taller than the box can scroll (max_scroll={max_scroll})"
    );

    // The text on the box's first inner row (between its borders).
    let box_top_line = |buf: &ratatui::buffer::Buffer| {
        (bx + 1..bx + bw - 1)
            .map(|x| buf.cell((x, by + 1)).unwrap().symbol().to_string())
            .collect::<String>()
            .trim_end()
            .to_string()
    };

    // Unscrolled, the docs start at line 1…
    let top = paint(&view, COLS, ROWS);
    assert_eq!(
        box_top_line(&top),
        "line 01",
        "unscrolled, the preview starts at the first doc line"
    );

    // …and scrolling to the bottom brings the corresponding later line to the top.
    let bottom = paint_doc_scrolled(&view, COLS, ROWS, max_scroll);
    assert_eq!(
        box_top_line(&bottom),
        format!("line {:02}", 1 + max_scroll),
        "scrolled by max_scroll, the box top shows the matching later doc line"
    );
}

#[tokio::test]
async fn a_click_selects_then_accepts_a_completion_item() {
    let _guard = test_lock().lock().await;
    // The mouse path: the client maps a click row to an item index via
    // `pmenu_geometry`, then drives the server with `nxvim_complete_select`
    // (highlight) and, on a click of the already-selected row,
    // `nxvim_complete_accept` (insert) — the <C-n>/<C-y> equivalents.
    let record = configure_mock(
        "compl-click",
        serde_json::json!({ "completion": completion_items(&["alpha", "beta", "gamma"]) }),
    );
    let file = temp_file("compl-click", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "o<C-x><C-o>");
    let params = wait_for_pmenu(&rpc, &mut incoming).await;
    let (labels, selected) = pmenu_of(&params).unwrap();
    assert_eq!(selected, -1, "nothing is selected until the user acts");

    // The geometry the client hit-tests: pick the box row for visible item 2 and
    // confirm the click math (`start + (row - y)`) round-trips to that index.
    let view = View::from_redraw(&params);
    let (_px, py, _pw, _ph, start) =
        pmenu_geometry(COLS, ROWS, &view).expect("the popup has geometry");
    let target = 2usize;
    let click_row = py + (target - start) as u16;
    assert_eq!(
        start + (click_row - py) as usize,
        target,
        "the click row maps back to the intended item index"
    );

    // First click on that row selects it (highlight only, no insert yet).
    rpc.notify("nxvim_complete_select", vec![Value::from(target as u64)]);
    let params = wait_for_pmenu_where(&rpc, &mut incoming, |(_, sel)| *sel == target as i64).await;
    assert_eq!(pmenu_of(&params).unwrap().1, target as i64);

    // A second click on the already-selected row accepts it.
    rpc.notify("nxvim_complete_accept", vec![]);
    wait_for_pmenu_closed(&rpc, &mut incoming).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {}".to_string(), labels[target].clone()],
        "accepting the clicked item inserts its label on the new line"
    );
}

#[tokio::test]
async fn the_completion_popup_scrolls_with_the_mouse_wheel() {
    let _guard = test_lock().lock().await;
    // A list taller than the popup's capped height scrolls as the wheel moves the
    // selection. Each notch is the client computing the next index from the current
    // selection (non-wrapping) and sending `nxvim_complete_select`; once the
    // selection passes the bottom of the box, the visible window (pmenu_geometry's
    // `start`) advances to keep it on screen.
    let labels: Vec<String> = (0..15).map(|i| format!("item{i:02}")).collect();
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let record = configure_mock(
        "compl-wheel",
        serde_json::json!({ "completion": completion_items(&label_refs) }),
    );
    let file = temp_file("compl-wheel", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "o<C-x><C-o>");
    let mut params = wait_for_pmenu(&rpc, &mut incoming).await;
    let n = pmenu_of(&params).unwrap().0.len();
    assert!(n >= 12, "enough items to overflow the capped box (got {n})");

    // The list starts at the top.
    let (.., start0) =
        pmenu_geometry(COLS, ROWS, &View::from_redraw(&params)).expect("the popup has geometry");
    assert_eq!(start0, 0, "the list starts unscrolled");

    // Wheel down twelve notches: each reads the live selection and steps one item,
    // exactly as the event loop computes `next`.
    for _ in 0..12 {
        let sel = pmenu_of(&params).unwrap().1;
        let next = if sel < 0 {
            0
        } else {
            ((sel + 1) as usize).min(n - 1)
        };
        rpc.notify("nxvim_complete_select", vec![Value::from(next as u64)]);
        params = wait_for_pmenu_where(&rpc, &mut incoming, move |(_, s)| *s == next as i64).await;
    }

    let sel = pmenu_of(&params).unwrap().1 as usize;
    let view = View::from_redraw(&params);
    let (_x, _py, _w, ph, start) =
        pmenu_geometry(COLS, ROWS, &view).expect("the popup has geometry");
    assert!(
        start > 0,
        "the list scrolled to follow the selection (start={start})"
    );
    assert!(
        sel - start < ph as usize,
        "the selected item stays within the visible box rows"
    );

    // Clamp: a wheel/click past the end lands on the last item, never wrapping.
    rpc.notify("nxvim_complete_select", vec![Value::from(999u64)]);
    let params =
        wait_for_pmenu_where(&rpc, &mut incoming, move |(_, s)| *s == (n - 1) as i64).await;
    assert_eq!(
        pmenu_of(&params).unwrap().1,
        (n - 1) as i64,
        "selecting past the end clamps to the last item"
    );
}

#[tokio::test]
async fn completion_orders_by_importance_and_filters_the_prefix() {
    let _guard = test_lock().lock().await;
    // The headline: with `use nv` typed, the menu shows the items matching `nv`
    // (`nva`, `nvb`) — ahead of and to the exclusion of `self`/`pub` — even though
    // the server returned them in a deliberately unhelpful order. A complete list
    // means the narrowing is a client-side refilter: exactly one request is sent.
    let record = configure_mock(
        "compl-order",
        serde_json::json!({ "completion": completion_items(&["pub", "self", "nvb", "nva"]) }),
    );
    let file = temp_file("compl-order", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Open a line, type `use `, trigger: the popup opens with everything (empty
    // prefix), ordered by the server priority (here, the label).
    feed(&rpc, "ouse <C-x><C-o>");
    let params = wait_for_pmenu(&rpc, &mut incoming).await;
    assert_eq!(
        pmenu_of(&params).unwrap().0,
        vec!["nva", "nvb", "pub", "self"],
        "an empty prefix shows every candidate"
    );

    // Type `n` then `v`: the menu stays open and narrows in place to the `nv`
    // matches, in importance order — `self`/`pub` gone.
    feed(&rpc, "n");
    wait_for_pmenu_items(&rpc, &mut incoming, &["nva", "nvb"]).await;
    feed(&rpc, "v");
    wait_for_pmenu_items(&rpc, &mut incoming, &["nva", "nvb"]).await;

    // A complete list ⇒ the narrowing filtered the cache; no extra request fired.
    assert_eq!(
        count_method(&record_lines(&record), "textDocument/completion"),
        1,
        "a complete list is filtered client-side, not re-requested"
    );
}

#[tokio::test]
async fn completion_ranking_honors_sort_text_over_the_label() {
    let _guard = test_lock().lock().await;
    // Two prefix matches whose `sortText` order reverses their alphabetical order
    // (`config` sorts after `connect`), plus a subsequence-only item. Importance
    // wins: `sortText` orders the prefix matches, and the subsequence ranks below.
    let record = configure_mock(
        "compl-sort",
        serde_json::json!({
            "completion": [
                { "label": "config", "sortText": "2" },
                { "label": "connect", "sortText": "1" },
                { "label": "disconnect" },
            ]
        }),
    );
    let file = temp_file("compl-sort", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "ocon<C-x><C-o>");
    wait_for_pmenu(&rpc, &mut incoming).await;
    // `connect` (sortText 1) before `config` (sortText 2); `disconnect` (only a
    // subsequence of `con`) last.
    wait_for_pmenu_items(&rpc, &mut incoming, &["connect", "config", "disconnect"]).await;
}

#[tokio::test]
async fn an_incomplete_list_re_requests_and_the_menu_stays_open() {
    let _guard = test_lock().lock().await;
    // `isIncomplete:true` ⇒ each narrowing keystroke fires a fresh request whose
    // result replaces the list, rather than filtering the cache. The popup stays
    // open across the round-trip (never goes `Nil`).
    let record = configure_mock(
        "compl-live",
        serde_json::json!({
            "completion_sequence": [
                completion_list(true, &["nano", "never", "nvidia", "nvim"]),
                completion_list(true, &["nvidia", "nvim"]),
            ]
        }),
    );
    let file = temp_file("compl-live", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Type `n`, trigger → the broad list (filtered to the `n` matches).
    feed(&rpc, "on<C-x><C-o>");
    wait_for_pmenu(&rpc, &mut incoming).await;
    wait_for_pmenu_items(&rpc, &mut incoming, &["nano", "never", "nvidia", "nvim"]).await;
    assert_eq!(
        count_method(&record_lines(&record), "textDocument/completion"),
        1
    );

    // Type `v`: a *second* request lands the narrowed list, and the menu stayed
    // open throughout (the helper fails if any redraw showed it closed).
    feed(&rpc, "v");
    wait_for_pmenu_items(&rpc, &mut incoming, &["nvidia", "nvim"]).await;
    let recs = wait_for_record(&rpc, &record, |r| {
        count_method(r, "textDocument/completion") >= 2
    })
    .await;
    assert_eq!(
        count_method(&recs, "textDocument/completion"),
        2,
        "an incomplete list re-requests on the narrowing keystroke"
    );
}

#[tokio::test]
async fn accepting_a_completion_inserts_the_item_and_additional_edits() {
    let _guard = test_lock().lock().await;
    // Accept replaces the typed word with the item (not appends) and applies its
    // `additionalTextEdits` (an inserted `use` line) — all one undo step.
    let record = configure_mock(
        "compl-accept",
        serde_json::json!({
            "completion": [{
                "label": "println",
                "insertText": "println",
                "additionalTextEdits": [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 0 },
                    },
                    "newText": "use std::io;\n",
                }],
            }]
        }),
    );
    let file = temp_file("compl-accept", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Open a line, type the prefix `pr`, trigger, select the item, accept.
    feed(&rpc, "opr<C-x><C-o>");
    wait_for_pmenu(&rpc, &mut incoming).await;
    feed(&rpc, "<C-n><CR>");
    barrier(&rpc).await;

    // The word became `println` (replaced, not `prprintln`), and the import line
    // was inserted at the top.
    assert_eq!(
        lines(&rpc).await,
        vec![
            "use std::io;".to_string(),
            "fn main() {}".to_string(),
            "println".to_string(),
        ],
        "accept replaced the prefix and applied the additional edit"
    );
    wait_for_pmenu_closed(&rpc, &mut incoming).await;

    // A single undo restores both the insertion and the import.
    feed(&rpc, "<Esc>u");
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {}".to_string()],
        "one undo reverts the whole accept"
    );
}

#[tokio::test]
async fn navigating_and_dismissing_the_completion_menu() {
    let _guard = test_lock().lock().await;
    let record = configure_mock(
        "compl-nav",
        serde_json::json!({ "completion": completion_items(&["alpha", "beta"]) }),
    );
    let file = temp_file("compl-nav", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Open a line, trigger: both items, nothing selected yet.
    feed(&rpc, "o<C-x><C-o>");
    let params = wait_for_pmenu(&rpc, &mut incoming).await;
    assert_eq!(pmenu_of(&params).unwrap().0, vec!["alpha", "beta"]);
    assert_eq!(pmenu_of(&params).unwrap().1, -1, "nothing selected yet");

    // `<C-n>` highlights the first item.
    feed(&rpc, "<C-n>");
    let params = wait_for_pmenu_where(&rpc, &mut incoming, |(_, sel)| *sel == 0).await;
    assert_eq!(pmenu_of(&params).unwrap().1, 0);

    // `<C-e>` dismisses without inserting; the buffer keeps only the empty line
    // `o` opened (no stray `e` from the control key).
    feed(&rpc, "<C-e>");
    wait_for_pmenu_closed(&rpc, &mut incoming).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {}".to_string(), String::new()],
        "<C-e> inserted nothing"
    );

    // Re-open, then `<Esc>` dismisses the menu AND leaves insert mode, still
    // inserting no literal character.
    feed(&rpc, "<C-x><C-o>");
    wait_for_pmenu(&rpc, &mut incoming).await;
    feed(&rpc, "<Esc>");
    wait_for_pmenu_closed(&rpc, &mut incoming).await;
    let mode = match rpc.request("nvim_get_mode", vec![]).await.unwrap() {
        Value::Map(m) => m
            .iter()
            .find(|(k, _)| k.as_str() == Some("mode"))
            .and_then(|(_, v)| v.as_str().map(str::to_string))
            .unwrap_or_default(),
        _ => String::new(),
    };
    assert_eq!(mode, "n", "<Esc> returned to normal mode");
    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {}".to_string(), String::new()],
        "<Esc> inserted nothing"
    );
}

#[tokio::test]
async fn the_completion_popup_paints_as_a_bordered_overlay() {
    let _guard = test_lock().lock().await;
    // Tier 2: the real client paint. The popup is a bordered box anchored one row
    // below the cursor at the word-start column (past the gutter); the selected
    // row is reverse-highlighted, and its cells belong to the menu, not the text.
    let record = configure_mock(
        "compl-paint",
        serde_json::json!({ "completion": completion_items(&["alpha", "beta"]) }),
    );
    let file = temp_file("compl-paint", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Open an empty line (cursor at column 0), trigger, select the first item.
    feed(&rpc, "o<C-x><C-o>");
    wait_for_pmenu(&rpc, &mut incoming).await;
    feed(&rpc, "<C-n>");
    let params = wait_for_pmenu_where(&rpc, &mut incoming, |(_, sel)| *sel == 0).await;
    let buf = paint(&View::from_redraw(&params), COLS, ROWS);

    // The box's top-left border sits at the word start: gutter (4) + col 0, one
    // row below the cursor (cursor row 1 ⇒ border row 2).
    assert_eq!(
        buf.cell((GUTTER, 2)).unwrap().symbol(),
        "┌",
        "the popup is a bordered box anchored under the word"
    );
    // Inside the border: the selected item `alpha` (reversed), then `beta`.
    let item_col = GUTTER + 1;
    assert_eq!(buf.cell((item_col, 3)).unwrap().symbol(), "a");
    assert!(
        buf.cell((item_col, 3))
            .unwrap()
            .style()
            .add_modifier
            .contains(Modifier::REVERSED),
        "the selected row is reverse-highlighted"
    );
    assert_eq!(buf.cell((item_col, 4)).unwrap().symbol(), "b");
    assert!(
        !buf.cell((item_col, 4))
            .unwrap()
            .style()
            .add_modifier
            .contains(Modifier::REVERSED),
        "an unselected row is not highlighted"
    );
}

#[tokio::test]
async fn accepting_a_utf16_text_edit_lands_at_the_right_byte() {
    let _guard = test_lock().lock().await;
    // The completion analogue of the cross-file `é` test: a line with a leading
    // 2-byte `é` and a utf-16 server. The item's `textEdit` range is in utf-16
    // units (char 1..2 = the `x` after `é`); accepting must convert it to byte
    // 2..3, so `x` → `xyz` lands as `éxyz`, not corrupting the `é`.
    let record = configure_mock(
        "compl-utf16",
        serde_json::json!({
            "position_encoding": "utf-16",
            "completion": [{
                "label": "xyz",
                "textEdit": {
                    "range": {
                        "start": { "line": 0, "character": 1 },
                        "end": { "line": 0, "character": 2 },
                    },
                    "newText": "xyz",
                },
            }],
        }),
    );
    let file = temp_file("compl-utf16", "rs", "éx\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Append (insert at end of line, after `x`), trigger, select, accept.
    feed(&rpc, "A<C-x><C-o>");
    wait_for_pmenu(&rpc, &mut incoming).await;
    feed(&rpc, "<C-n><CR>");
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["éxyz".to_string()],
        "the utf-16 edit range converted to the right byte offset (after é)"
    );
}

#[tokio::test]
async fn completion_never_blocks_the_editor() {
    let _guard = test_lock().lock().await;
    // Resilience: a trigger whose server offers nothing (a null reply) opens no
    // menu and the editor keeps editing — text typed right after the trigger
    // lands normally and completion inserts nothing.
    let record = configure_mock("compl-resil", serde_json::json!({}));
    let file = temp_file("compl-resil", "rs", "fn main() {}\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Open a line, type `foo`, trigger (null reply ⇒ no menu), keep typing `bar`.
    feed(&rpc, "ofoo<C-x><C-o>bar<Esc>");
    // The request was genuinely sent (so the resilience path was exercised), and
    // its null reply opened no menu.
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/completion")).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {}".to_string(), "foobar".to_string()],
        "the editor stays fully editable; completion inserted nothing"
    );
}
