//! Behavior tests for `nx.picker` — the fuzzy finder over the unified float-list
//! widget (`docs/specs/2026-06-14-nx-ui-float-widget.md`, Phase 2: the prompt
//! input-grab, the Rust matcher, dynamic-source forwarding, generation tokens).
//!
//! Black-box like the rest: a real server sources an `init.lua`, the picker is
//! driven over the same msgpack-RPC a UI uses, and the assertions are on the
//! projected `menu` redraw surface (rows, selected, query, match spans) and on the
//! `confirm` side effects read back through `nvim_exec_lua`.
//!
//! The sources here are custom **in-memory** drivers (no process spawn), so the
//! suite is hermetic — it never depends on `rg` being installed. The shipped
//! `files`/`live_grep` sources (which stream `rg`) are exercised by the example
//! config and manual runs, not here.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, command, cursor, drain_to_latest_redraw, exec_lua, feed, feed_mouse, lines, map_get,
    spawn, temp_dir,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start(dir: &std::path::Path, init_lua: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(dir.join("init.lua"), init_lua).expect("write init.lua");
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Poll for the latest redraw whose `menu` key is a map (the take-latest pattern).
async fn poll_menu(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<(Value, Value)>> {
    for _ in 0..60 {
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| {
            matches!(map_get(m, "menu"), Some(Value::Map(_)))
        }) {
            return Some(map);
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    None
}

fn menu_of(map: &[(Value, Value)]) -> Vec<(Value, Value)> {
    match map_get(map, "menu") {
        Some(Value::Map(m)) => m.clone(),
        other => panic!("expected a menu map, got {other:?}"),
    }
}

/// The menu's visible row labels, in order.
fn menu_items(menu: &[(Value, Value)]) -> Vec<String> {
    match map_get(menu, "items") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|row| match row {
                // A row is `[label, ...]` or a bare label string.
                Value::Array(a) => a.first().and_then(Value::as_str).unwrap_or("").to_string(),
                Value::String(s) => s.as_str().unwrap_or("").to_string(),
                other => panic!("unexpected menu row {other:?}"),
            })
            .collect(),
        other => panic!("expected menu items array, got {other:?}"),
    }
}

/// Register a static source over a fixed list of `{ text=… }` items.
const STATIC_SRC: &str = r#"
nx.picker.source {
  name = "fruits",
  items = function(ctx)
    for _, t in ipairs({ "apple", "apricot", "banana", "cherry" }) do
      ctx.push { text = t, fruit = t }
    end
  end,
  confirm = function(item) _G.picked = item.fruit end,
}
"#;

#[tokio::test]
async fn static_source_streams_then_fuzzy_filters_without_touching_the_buffer() {
    let dir = temp_dir("picker_static_filter");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;

    exec_lua(&rpc, "nx.picker.open('fruits')").await;

    // All four candidates show on open (no query yet).
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    assert_eq!(
        menu_items(&menu),
        vec!["apple", "apricot", "banana", "cherry"]
    );

    // Type "ap": the prompt grabs the keys (the document buffer is untouched) and
    // the Rust matcher narrows to the two "ap…" fruits.
    feed(&rpc, "ap");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("filtered menu"));
    assert_eq!(menu_items(&menu), vec!["apple", "apricot"]);

    // The keystrokes never reached the document.
    assert_eq!(lines(&rpc).await, vec![""]);

    // The prompt surface carries the live query.
    assert_eq!(
        map_get(&menu, "query").and_then(Value::as_str),
        Some("ap"),
        "menu projects the prompt query"
    );
}

#[tokio::test]
async fn confirm_runs_the_source_confirm_with_the_chosen_item() {
    let dir = temp_dir("picker_confirm");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;

    exec_lua(&rpc, "_G.picked = nil; nx.picker.open('fruits')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");

    // Move down one (apple -> apricot) and confirm.
    feed(&rpc, "<C-n>");
    feed(&rpc, "<CR>");

    assert_eq!(
        exec_lua(&rpc, "return _G.picked").await.as_str(),
        Some("apricot"),
        "confirm received the highlighted item with its original fields"
    );
}

#[tokio::test]
async fn escape_closes_the_picker_without_confirming() {
    let dir = temp_dir("picker_escape");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;

    exec_lua(&rpc, "_G.picked = 'unset'; nx.picker.open('fruits')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");

    feed(&rpc, "<Esc>");

    // No confirm fired, and the widget is gone (no menu in the next redraw).
    assert_eq!(
        exec_lua(&rpc, "return _G.picked").await.as_str(),
        Some("unset")
    );
    assert_eq!(
        exec_lua(&rpc, "return nx._picker == nil").await,
        Value::Boolean(true),
        "the active picker is cleared on cancel"
    );
}

#[tokio::test]
async fn dynamic_source_reruns_on_each_query_edit() {
    // A dynamic source bypasses the matcher: it produces rows from the query
    // itself, so re-running it per edit is observable in the rows.
    let dir = temp_dir("picker_dynamic");
    let src = r#"
nx.picker.source {
  name = "echo",
  dynamic = true,
  debounce = 0,                              -- run immediately (test the re-run, not the debounce)
  items = function(ctx)
    if ctx.query ~= "" then
      ctx.push { text = ctx.query .. "-1", q = ctx.query }
      ctx.push { text = ctx.query .. "-2", q = ctx.query }
    end
  end,
  confirm = function(item) _G.picked = item.q end,
}
"#;
    let (rpc, mut incoming) = start(&dir, src).await;

    exec_lua(&rpc, "nx.picker.open('echo')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");

    feed(&rpc, "ab");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu for 'ab'"));
    assert_eq!(
        menu_items(&menu),
        vec!["ab-1", "ab-2"],
        "the dynamic source re-ran for the live query, replacing stale rows"
    );

    // Confirm hands the source the item built for the *current* query.
    feed(&rpc, "<CR>");
    assert_eq!(
        exec_lua(&rpc, "return _G.picked").await.as_str(),
        Some("ab")
    );
}

#[tokio::test]
async fn dynamic_source_is_debounced_rapid_typing_runs_once() {
    // A dynamic source must NOT spawn per keystroke. Typing several chars in one
    // input batch produces several query edits, but the debounce coalesces them
    // into a single source run for the final query — the fix for "live grep runs a
    // process on every key". `items` counts its invocations in `_G.runs`.
    let dir = temp_dir("picker_debounce");
    let src = r#"
nx.picker.source {
  name = "counted",
  dynamic = true,
  debounce = 60,
  items = function(ctx)
    if ctx.query ~= "" then
      _G.runs = (_G.runs or 0) + 1
      ctx.push { text = ctx.query }
    end
  end,
  confirm = function(item) end,
}
"#;
    let (rpc, mut incoming) = start(&dir, src).await;
    exec_lua(&rpc, "_G.runs = 0; nx.picker.open('counted')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");

    // Feed three chars as one batch → three query edits (gen 1/2/3) in one tick.
    // Only the last survives the debounce, so the source runs exactly once.
    feed(&rpc, "abc");
    // Before the debounce elapses, nothing has run yet.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.runs").await.as_u64(),
        Some(0),
        "the source has not run before the debounce elapses"
    );

    // After the debounce fires, the menu fills — poll until the rows arrive (the
    // open-but-empty menu shows before the debounced run completes).
    let mut items = Vec::new();
    for _ in 0..60 {
        if let Some(map) = poll_menu(&rpc, &mut incoming).await {
            items = menu_items(&menu_of(&map));
            if !items.is_empty() {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(items, vec!["abc"], "one run, for the final query");
    assert_eq!(
        exec_lua(&rpc, "return _G.runs").await.as_u64(),
        Some(1),
        "rapid typing across three keys ran the source exactly once, not three times"
    );
}

#[tokio::test]
async fn debounce_default_is_250_and_overridable_per_open() {
    let dir = temp_dir("picker_debounce_cfg");
    // The source asks for a very long debounce; the per-open override must win.
    let src = r#"
nx.picker.source {
  name = "echo",
  dynamic = true,
  debounce = 100000,                         -- would never fire within the test
  items = function(ctx)
    if ctx.query ~= "" then ctx.push { text = ctx.query } end
  end,
  confirm = function(item) end,
}
"#;
    let (rpc, mut incoming) = start(&dir, src).await;

    // The shipped global default is 250ms.
    assert_eq!(
        exec_lua(&rpc, "return nx.picker.debounce").await.as_u64(),
        Some(250),
        "the global default debounce is 250ms"
    );

    // Per-open `debounce = 0` overrides the source's 100000ms → the run is immediate.
    exec_lua(&rpc, "nx.picker.open('echo', { debounce = 0 })").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "hi");
    let mut items = Vec::new();
    for _ in 0..40 {
        if let Some(map) = poll_menu(&rpc, &mut incoming).await {
            items = menu_items(&menu_of(&map));
            if !items.is_empty() {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        items,
        vec!["hi"],
        "per-open debounce=0 overrode the source's huge debounce and ran immediately"
    );
}

#[tokio::test]
async fn scrolling_past_the_window_tracks_the_selection() {
    // A list taller than the box scrolls: moving the selection down past the visible
    // window keeps it on screen (wire `selected` stays within the window), and
    // confirm resolves the globally-selected item — not the windowed index.
    let dir = temp_dir("picker_scroll");
    let src = r#"
nx.picker.source {
  name = "many",
  items = function(ctx)
    for i = 1, 50 do ctx.push { text = string.format("row-%02d", i), n = i } end
  end,
  confirm = function(item) _G.picked = item.n end,
}
"#;
    let (rpc, mut incoming) = start(&dir, src).await;
    exec_lua(&rpc, "nx.picker.open('many')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("opens"));
    let list_rows = menu_items(&menu).len();
    assert!(
        list_rows < 50,
        "the box shows only a window of the 50 rows ({list_rows})"
    );

    // Move the selection down 30 times — well past the first window.
    for _ in 0..30 {
        feed(&rpc, "<C-n>");
    }
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("scrolled"));
    let sel = map_get(&menu, "selected").and_then(Value::as_u64).unwrap() as usize;
    let items = menu_items(&menu);
    assert!(sel < items.len(), "selected stays within the sent window");
    assert_eq!(
        items[sel], "row-31",
        "the 31st row (0-based 30) is the highlighted, scrolled-into-view row"
    );

    feed(&rpc, "<CR>");
    assert_eq!(
        exec_lua(&rpc, "return _G.picked").await.as_u64(),
        Some(31),
        "confirm resolved the globally-selected item, not the window index"
    );
}

#[tokio::test]
async fn scales_to_100k_candidates_and_windows_the_projection() {
    // A 100k-element static source must open, fuzzy-filter, and confirm fast — and
    // the redraw must carry only the visible WINDOW of rows, never the whole list.
    let dir = temp_dir("picker_100k");
    let src = r#"
nx.picker.source {
  name = "huge",
  items = function(ctx)
    for i = 1, 100000 do ctx.push { text = "item-" .. i, n = i } end
  end,
  confirm = function(item) _G.picked = item.n end,
}
"#;
    let (rpc, mut incoming) = start(&dir, src).await;

    let opened = std::time::Instant::now();
    exec_lua(&rpc, "nx.picker.open('huge')").await;
    // Poll until all 100k have streamed in (total reflects the full set).
    let mut total = 0u64;
    for _ in 0..100 {
        total = exec_lua(&rpc, "return nx._picker and nx._picker.nitems or 0")
            .await
            .as_u64()
            .unwrap_or(0);
        if total >= 100000 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(total, 100000, "all 100k candidates streamed in");
    assert!(
        opened.elapsed() < std::time::Duration::from_secs(10),
        "100k stream+render stayed responsive ({:?})",
        opened.elapsed()
    );

    // The redraw carries only the visible window, not 100k rows.
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    let shown = match map_get(&menu, "items") {
        Some(Value::Array(a)) => a.len(),
        _ => panic!("no items"),
    };
    assert!(
        shown < 100,
        "only the visible window is sent ({shown} rows), not all 100k"
    );

    // Fuzzy-filter to a unique item and confirm it — matcher considers all 100k.
    feed(&rpc, "item-99999");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("filtered"));
    assert_eq!(
        menu_items(&menu)[0],
        "item-99999",
        "the 100k matcher found it"
    );
    feed(&rpc, "<CR>");
    assert_eq!(
        exec_lua(&rpc, "return _G.picked").await.as_u64(),
        Some(99999),
        "confirm resolved the chosen item across the 100k set"
    );
}

#[tokio::test]
async fn old_results_stay_visible_until_the_new_search_returns() {
    // While a debounced search is in flight the previous results MUST remain on
    // screen — the list never flashes empty on a keystroke. A no-match query clears
    // only once its search completes (`done`).
    let dir = temp_dir("picker_persist");
    let src = r#"
nx.picker.source {
  name = "deferred",
  dynamic = true,
  debounce = 0,
  items = function(ctx)
    -- Only "hit" yields a result; defer both the ctx.push and completion so the
    -- in-flight window is observable — the source returns a promise that resolves
    -- (= done) when the timer fires.
    return nx.promise.new(function(resolve)
      nx.timer(function()
        if ctx.query == "hit" then ctx.push { text = "FOUND:" .. ctx.query } end
        resolve()
      end, 60)
    end)
  end,
  confirm = function(item) end,
}
"#;
    let (rpc, mut incoming) = start(&dir, src).await;
    exec_lua(&rpc, "nx.picker.open('deferred')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");

    // First query "hit" → after its 60ms run, the row appears.
    feed(&rpc, "hit");
    let mut items = Vec::new();
    for _ in 0..60 {
        if let Some(map) = poll_menu(&rpc, &mut incoming).await {
            items = menu_items(&menu_of(&map));
            if !items.is_empty() {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(items, vec!["FOUND:hit"], "first query's result is shown");

    // Type more (query becomes "hitx", which has no match). DURING the new search's
    // in-flight window the old "FOUND:hit" row must still be visible (not flashed
    // empty by the keystroke).
    feed(&rpc, "x");
    tokio::time::sleep(std::time::Duration::from_millis(20)).await; // < the 60ms run
    let menu = menu_of(
        &poll_menu(&rpc, &mut incoming)
            .await
            .expect("in-flight menu"),
    );
    assert_eq!(
        menu_items(&menu),
        vec!["FOUND:hit"],
        "the previous result stays while the new search runs"
    );
    assert_eq!(
        map_get(&menu, "query").and_then(Value::as_str),
        Some("hitx"),
        "the prompt already shows the new query"
    );

    // Once the no-match search completes, the stale row clears.
    let mut cleared = false;
    for _ in 0..60 {
        if let Some(map) = poll_menu(&rpc, &mut incoming).await {
            if menu_items(&menu_of(&map)).is_empty() {
                cleared = true;
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        cleared,
        "a no-match query clears the stale results once its search finishes"
    );
}

#[tokio::test]
async fn streaming_source_appears_as_rows_arrive() {
    // A source that streams via `nx.run_stream` (the async-iterator path):
    // `printf` (coreutils) emits three lines, which arrive as picker rows. Proves
    // the streaming primitive end-to-end, not just the synchronous ctx.push path.
    let dir = temp_dir("picker_stream");
    let src = r#"
nx.picker.source {
  name = "stream",
  items = nx.async(function(ctx)
    for batch in nx.await_each(nx.run_stream { cmd = "printf", args = { "one\ntwo\nthree\n" } }) do
      for _, l in ipairs(batch) do
        if l ~= "" then ctx.push { text = l } end
      end
    end
  end),
  confirm = function(item) _G.picked = item.text end,
}
"#;
    let (rpc, mut incoming) = start(&dir, src).await;
    exec_lua(&rpc, "nx.picker.open('stream')").await;

    // Poll until all three streamed rows have landed.
    let mut items = Vec::new();
    for _ in 0..60 {
        if let Some(map) = poll_menu(&rpc, &mut incoming).await {
            items = menu_items(&menu_of(&map));
            if items.len() == 3 {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        items,
        vec!["one", "two", "three"],
        "the three streamed lines arrived as rows"
    );

    // Fuzzy-filter the streamed list and confirm.
    feed(&rpc, "thr");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("filtered"));
    assert_eq!(menu_items(&menu), vec!["three"]);
    feed(&rpc, "<CR>");
    assert_eq!(
        exec_lua(&rpc, "return _G.picked").await.as_str(),
        Some("three")
    );
}

#[tokio::test]
async fn a_stale_generation_push_is_dropped() {
    // A dynamic source whose ctx.push is *deferred* (a timer) carries the generation
    // of the query that scheduled it. Typing past that query (bumping the
    // generation) must drop the late ctx.push so a superseded query's rows never show.
    let dir = temp_dir("picker_stale");
    let src = r#"
nx.picker.source {
  name = "deferred",
  dynamic = true,
  debounce = 0,                              -- isolate the generation gate from the debounce
  items = function(ctx)
    if ctx.query == "" then return end
    -- Push one row for this query, but only after a delay — by which time a
    -- faster typist may have moved the generation on.
    return nx.promise.new(function(resolve)
      nx.timer(function()
        ctx.push { text = "row:" .. ctx.query }
        resolve()
      end, 40)
    end)
  end,
  confirm = function(item) _G.picked = item.text end,
}
"#;
    let (rpc, mut incoming) = start(&dir, src).await;
    exec_lua(&rpc, "nx.picker.open('deferred')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");

    // Type "a" then immediately "b": the gen-1 ("a") deferred ctx.push must be dropped;
    // only the gen-2 ("ab") row may appear.
    feed(&rpc, "a");
    feed(&rpc, "b");

    // Wait for the deferred pushes to fire.
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("settled menu"));
    let items = menu_items(&menu);
    assert!(
        !items.iter().any(|i| i == "row:a"),
        "the stale gen-1 row must never appear, got {items:?}"
    );
    assert_eq!(
        map_get(&menu, "query").and_then(Value::as_str),
        Some("ab"),
        "the live query is 'ab'"
    );
}

#[tokio::test]
async fn a_closed_pickers_late_push_never_leaks_into_the_next() {
    // A source whose ctx.push is deferred (a timer) and which forgets to register
    // `ctx.on_cancel` keeps "streaming" after its picker closes. Because every
    // picker opens at generation 0, that orphaned ctx.push (gen 0) must NOT land in
    // the next picker (also gen 0) — the identity guard drops it.
    let dir = temp_dir("picker_leak");
    let src = r#"
_G.runs = 0
nx.picker.source {
  name = "leak",
  items = function(ctx)
    _G.runs = _G.runs + 1
    local tag = "run" .. _G.runs            -- distinguishes each picker's run
    return nx.promise.new(function(resolve)
      nx.timer(function() ctx.push { text = tag }; resolve() end, 40)
    end)
  end,
  confirm = function(item) end,
}
"#;
    let (rpc, mut incoming) = start(&dir, src).await;

    // Picker A: open, then immediately cancel — its deferred "run1" ctx.push is still
    // pending and its job was never reaped (no on_cancel).
    exec_lua(&rpc, "nx.picker.open('leak')").await;
    poll_menu(&rpc, &mut incoming)
        .await
        .expect("picker A opens");
    feed(&rpc, "<Esc>");

    // Picker B: open a fresh picker before A's timer fires.
    exec_lua(&rpc, "nx.picker.open('leak')").await;
    poll_menu(&rpc, &mut incoming)
        .await
        .expect("picker B opens");

    // Let both deferred pushes fire (A's at ~40ms from its open, B's at ~40ms).
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let menu = menu_of(
        &poll_menu(&rpc, &mut incoming)
            .await
            .expect("picker B settled"),
    );
    let items = menu_items(&menu);
    assert!(
        !items.iter().any(|i| i == "run1"),
        "the closed picker A's late ctx.push must not leak into picker B, got {items:?}"
    );
    assert_eq!(items, vec!["run2"], "picker B shows only its own run's row");
}

#[tokio::test]
async fn prompt_carries_the_caret_column_and_it_tracks_edits() {
    // The picker projects the prompt's text-cursor as `query_cursor` (a char count)
    // so the client can draw a caret in the input box. It advances as you type and
    // moves with <Left>/<Right>.
    let dir = temp_dir("picker_caret");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;
    exec_lua(&rpc, "nx.picker.open('fruits')").await;

    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("opens"));
    // Empty query → caret at column 0.
    assert_eq!(
        map_get(&menu, "query_cursor").and_then(Value::as_u64),
        Some(0)
    );

    feed(&rpc, "ap");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("typed"));
    assert_eq!(
        map_get(&menu, "query_cursor").and_then(Value::as_u64),
        Some(2),
        "caret sits past the two typed chars"
    );

    // <Left> moves the caret without changing the query.
    feed(&rpc, "<Left>");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("moved left"));
    assert_eq!(
        map_get(&menu, "query_cursor").and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(map_get(&menu, "query").and_then(Value::as_str), Some("ap"));
}

#[tokio::test]
async fn prompt_position_defaults_to_top_and_is_configurable_to_bottom() {
    let dir = temp_dir("picker_prompt_pos");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;

    // Default: prompt above the list.
    exec_lua(&rpc, "nx.picker.open('fruits')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("default"));
    assert_eq!(
        map_get(&menu, "prompt_pos").and_then(Value::as_str),
        Some("top"),
        "the prompt sits above the list by default"
    );
    feed(&rpc, "<Esc>");
    poll_menu(&rpc, &mut incoming).await;

    // Per-open override: input below the results.
    exec_lua(&rpc, "nx.picker.open('fruits', { prompt_pos = 'bottom' })").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("bottom"));
    assert_eq!(
        map_get(&menu, "prompt_pos").and_then(Value::as_str),
        Some("bottom"),
        "the open option places the input below the results"
    );
}

#[tokio::test]
async fn the_separator_row_is_reserved_between_prompt_and_list() {
    // A picker box of height H shows H-2 list rows: one row for the prompt and one
    // for the separator between it and the results. With 50 items and height 12 the
    // window is exactly 10 rows (not 11).
    let dir = temp_dir("picker_sep");
    let src = r#"
nx.picker.source {
  name = "many",
  items = function(ctx)
    for i = 1, 50 do ctx.push { text = string.format("row-%02d", i) } end
  end,
  confirm = function(item) end,
}
"#;
    let (rpc, mut incoming) = start(&dir, src).await;
    exec_lua(&rpc, "nx.picker.open('many', { width = 30, height = 12 })").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("opens"));
    assert_eq!(menu_size(&menu), (30, 12), "the box is the requested size");
    assert_eq!(
        menu_items(&menu).len(),
        10,
        "height 12 = prompt + separator + 10 list rows"
    );
}

#[tokio::test]
async fn edit_helper_opens_a_file_and_jumps_to_the_location() {
    // The shipped confirm action `nx.picker.edit` (used by `files` / `live_grep`)
    // must open the file and jump to a 1-based row/col WITHOUT the mutating
    // `vim.api.nvim_win_set_cursor`, which is intentionally nil in Lua (ADR 0002) —
    // the bug behind "attempt to call field 'nvim_win_set_cursor' (a nil value)".
    let dir = temp_dir("picker_edit_goto");
    let file = dir.join("target.txt");
    std::fs::write(&file, "one\ntwo\nthree\nfour\n").unwrap();
    let (rpc, _incoming) = start(&dir, "").await;

    // Drive the exact shipped helper with a live_grep-shaped item (path + row/col).
    exec_lua(
        &rpc,
        &format!(
            "nx.picker.edit({{ path = '{}', row = 3, col = 2 }})",
            file.display()
        ),
    )
    .await;

    assert_eq!(
        lines(&rpc).await,
        vec!["one", "two", "three", "four"],
        "the file opened"
    );
    assert_eq!(
        cursor(&rpc).await,
        (3, 1),
        "cursor jumped to the 1-based row / 1-based col location"
    );
}

/// The menu's projected box size `(width, height)` in content cells.
fn menu_size(menu: &[(Value, Value)]) -> (u64, u64) {
    (
        map_get(menu, "width").and_then(Value::as_u64).unwrap(),
        map_get(menu, "height").and_then(Value::as_u64).unwrap(),
    )
}

#[tokio::test]
async fn picker_is_a_fixed_centered_box_not_content_sized() {
    // The picker floats centered over the editor with a FIXED size (default ~80% x
    // 60% of the viewport) — never hugging its (here, four) short rows. On an 80x24
    // screen that is a wide, tall box pushed well clear of the top-left origin.
    let dir = temp_dir("picker_center");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;
    exec_lua(&rpc, "nx.picker.open('fruits')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    let row = map_get(&menu, "row").and_then(Value::as_u64).unwrap();
    let col = map_get(&menu, "col").and_then(Value::as_u64).unwrap();
    let (w, h) = menu_size(&menu);
    // Fixed default, not content-sized: a 4-item picker would be ~5 rows / ~7 cols
    // if content-hugging; the fixed box is far larger (most of the 80x24 screen).
    assert!(
        w > 40,
        "fixed-width box (~80vw), not content-sized: width={w}"
    );
    assert!(
        h > 8,
        "fixed-height box (~60vh), not content-sized: height={h}"
    );
    assert!(row > 1 && col > 2, "box is centered, row={row} col={col}");
}

#[tokio::test]
async fn picker_size_is_configurable_in_cells_and_viewport_fractions() {
    let dir = temp_dir("picker_size");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;

    // Absolute cells.
    exec_lua(
        &rpc,
        "nx.picker.open('fruits', { width = 30, height = 12 })",
    )
    .await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("sized menu"));
    assert_eq!(menu_size(&menu), (30, 12), "absolute cell size is honored");
    feed(&rpc, "<Esc>");
    poll_menu(&rpc, &mut incoming).await;

    // CSS-style viewport fractions: 50vw of an 80-col screen ≈ 40 (text area is a
    // few cells narrower than the screen); 25vh of 24 rows ≈ 5-6. Assert ranges.
    exec_lua(
        &rpc,
        "nx.picker.open('fruits', { width = '50vw', height = '25vh' })",
    )
    .await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("frac menu"));
    let (w, h) = menu_size(&menu);
    assert!(
        (34..=40).contains(&w),
        "50vw resolves to ~half the width, got {w}"
    );
    assert!(
        (4..=7).contains(&h),
        "25vh resolves to ~quarter the height, got {h}"
    );
}

#[tokio::test]
async fn picker_align_and_margin_place_the_box_in_a_corner_with_a_gap() {
    // The unified geometry: a picker is no longer centered-only — `align` (a 9-grid
    // word) places the box and `margin` insets it from the edges, so it can sit in a
    // corner without kissing the border. `row`/`col` in the menu map are the OUTER
    // box top-left (border included); the projection aligns within the (0,0)-origin
    // text area.
    let dir = temp_dir("picker_align");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;

    let pos = |menu: &[(Value, Value)]| {
        (
            map_get(menu, "row").and_then(Value::as_u64).unwrap(),
            map_get(menu, "col").and_then(Value::as_u64).unwrap(),
        )
    };

    // top-left, no margin: the box hugs the origin.
    exec_lua(
        &rpc,
        "nx.picker.open('fruits', { width = 20, height = 6, align = 'top-left' })",
    )
    .await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("top-left"));
    assert_eq!(
        pos(&menu),
        (0, 0),
        "top-left with no margin hugs the origin"
    );
    feed(&rpc, "<Esc>");
    poll_menu(&rpc, &mut incoming).await;

    // top-left, margin 4: a single value is the vertical gap; the horizontal gap is
    // 2x (cells are ~2x taller than wide), so the box sits at row 4, col 8.
    exec_lua(
        &rpc,
        "nx.picker.open('fruits', { width = 20, height = 6, align = 'top-left', margin = 4 })",
    )
    .await;
    let menu = menu_of(
        &poll_menu(&rpc, &mut incoming)
            .await
            .expect("top-left margin"),
    );
    assert_eq!(
        pos(&menu),
        (4, 8),
        "a scalar margin insets vertically by N and horizontally by 2N"
    );
    feed(&rpc, "<Esc>");
    poll_menu(&rpc, &mut incoming).await;

    // bottom-right, margin {vertical=2, horizontal=5}: pinned to the bottom-right.
    // The box's right edge sits 5 cells from the screen's right; its bottom 2 from
    // the bottom — so both row and col are large (the box left/top is far from 0).
    exec_lua(
        &rpc,
        "nx.picker.open('fruits', { width = 20, height = 6, align = 'bottom-right', margin = { 2, 5 } })",
    )
    .await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("bottom-right"));
    let (r, c) = pos(&menu);
    let (w, _h) = menu_size(&menu);
    // col + outer-width + right-margin == text width. With width 20 (+2 border) and a
    // 5-col right gap on an ~80-col screen, the box's left edge is well past center.
    assert!(
        c >= 45,
        "bottom-right pins the box to the right (col={c}, w={w})"
    );
    assert!(r >= 8, "bottom-right pins the box to the bottom (row={r})");
}

#[tokio::test]
async fn buffers_source_lists_open_buffers() {
    let dir = temp_dir("picker_buffers");
    let file = dir.join("hello.txt");
    std::fs::write(&file, "hi\n").unwrap();
    let (rpc, mut incoming) = start(&dir, "").await;

    // Open a named buffer so the in-memory source has something to list.
    exec_lua(&rpc, &format!("vim.cmd('edit {}')", file.display())).await;
    exec_lua(&rpc, "nx.picker.open('buffers')").await;

    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    let items = menu_items(&menu);
    assert!(
        items.iter().any(|i| i.contains("hello.txt")),
        "buffers picker lists the open buffer, got {items:?}"
    );
}

#[tokio::test]
async fn buffers_picker_is_scoped_to_the_focused_layer() {
    // Like `:ls`, the buffers picker lists only the focused layer's buffers — the
    // main area and each dock keep disjoint lists. Opened in a dock, it shows the
    // dock's buffer and not the main area's.
    let dir = temp_dir("picker_buffers_layer");
    let main_file = dir.join("main.txt");
    let dock_file = dir.join("dock.txt");
    std::fs::write(&main_file, "m\n").unwrap();
    std::fs::write(&dock_file, "d\n").unwrap();
    let (rpc, mut incoming) = start(&dir, "").await;

    // A named buffer in the MAIN layer.
    exec_lua(&rpc, &format!("vim.cmd('edit {}')", main_file.display())).await;

    // Open a bottom dock, focus it, and open a named buffer THERE — so the dock
    // layer owns it (a buffer's home is the layer it was last shown in).
    exec_lua(&rpc, "nx.dock.open({ side = 'bottom', size = 12 })").await;
    exec_lua(&rpc, "nx.dock.focus('bottom')").await;
    exec_lua(&rpc, &format!("vim.cmd('edit {}')", dock_file.display())).await;

    // The buffers picker, focused in the dock, lists the dock's buffer — not main's.
    exec_lua(&rpc, "nx.picker.open('buffers')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    let items = menu_items(&menu);
    assert!(
        items.iter().any(|i| i.contains("dock.txt")),
        "the focused dock layer's buffer is listed, got {items:?}"
    );
    assert!(
        !items.iter().any(|i| i.contains("main.txt")),
        "the main layer's buffer is NOT listed while focused in the dock, got {items:?}"
    );
}

// ===== default leader maps =================================================

#[tokio::test]
async fn default_picker_maps_open_with_the_configured_leader() {
    // The three shipped sources are bound to `<leader>f{f,g,b}` on VimEnter — after
    // init.lua runs, so `<leader>` expands with the config's mapleader, not the
    // default `\`. Use the in-memory `buffers` source (no `rg`) to stay hermetic.
    let dir = temp_dir("picker_default_maps");
    let file = dir.join("hello.txt");
    std::fs::write(&file, "hi\n").unwrap();
    let (rpc, mut incoming) = start(&dir, "vim.g.mapleader = ','").await;

    exec_lua(&rpc, &format!("vim.cmd('edit {}')", file.display())).await;
    // Drive the default map by keystroke at the configured leader (`,fb`).
    feed(&rpc, ",fb");

    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    let items = menu_items(&menu);
    assert!(
        items.iter().any(|i| i.contains("hello.txt")),
        "<leader>fb opened the buffers picker, got {items:?}"
    );
}

#[tokio::test]
async fn a_user_map_overrides_the_default_picker_map() {
    // The defaults are `default = true`, so a user's own `<leader>ff` wins — and the
    // shipped `files` default (which would spawn `rg`) never fires. No mapleader set,
    // so the leader is the default `\`.
    let dir = temp_dir("picker_default_override");
    let (rpc, _incoming) = start(
        &dir,
        "vim.keymap.set('n', '<leader>ff', function() _G.hit = 'user' end)",
    )
    .await;

    feed(&rpc, r"\ff");
    nxvim_test_harness::barrier(&rpc).await;

    let hit = exec_lua(&rpc, "return _G.hit").await;
    assert_eq!(hit, Value::from("user"), "the user map for <leader>ff wins");
}

// ===== Phase 3: the preview pane ============================================

/// The menu's `preview` sub-map, or `None` for a preview-less picker / select.
fn preview_of(menu: &[(Value, Value)]) -> Option<Vec<(Value, Value)>> {
    match map_get(menu, "preview") {
        Some(Value::Map(m)) => Some(m.clone()),
        _ => None,
    }
}

/// The preview pane's windowed lines, in order.
fn preview_lines(preview: &[(Value, Value)]) -> Vec<String> {
    match map_get(preview, "lines") {
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect(),
        other => panic!("expected preview lines array, got {other:?}"),
    }
}

/// The 1-based file line shown at the top of the preview window.
fn preview_first_line(preview: &[(Value, Value)]) -> u64 {
    match map_get(preview, "first_line") {
        Some(v) => v.as_u64().expect("first_line is a number"),
        other => panic!("expected preview first_line, got {other:?}"),
    }
}

/// The preview's `loc` (`[row, col]`) rebased into the window, or `None`.
fn preview_loc(preview: &[(Value, Value)]) -> Option<(u64, u64)> {
    match map_get(preview, "loc") {
        Some(Value::Array(a)) if a.len() == 2 => {
            Some((a[0].as_u64().unwrap(), a[1].as_u64().unwrap()))
        }
        _ => None,
    }
}

#[tokio::test]
async fn file_preview_shows_selected_file_and_swaps_on_move() {
    let dir = temp_dir("picker_preview_file");
    std::fs::write(dir.join("a.txt"), "alpha one\nalpha two\n").unwrap();
    std::fs::write(dir.join("b.txt"), "bravo only\n").unwrap();
    let a = dir.join("a.txt");
    let b = dir.join("b.txt");
    let src = format!(
        r#"
nx.picker.source {{
  name = "files_test",
  preview = "file",
  items = function(ctx)
    ctx.push {{ text = "a.txt", path = "{a}" }}
    ctx.push {{ text = "b.txt", path = "{b}" }}
  end,
  confirm = function(item) _G.picked = item.path end,
}}
"#,
        a = a.display(),
        b = b.display(),
    );
    let (rpc, mut incoming) = start(&dir, &src).await;

    exec_lua(&rpc, "nx.picker.open('files_test')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));

    // The first row (a.txt) previews with its file's head, titled by its path.
    let preview = preview_of(&menu).expect("file picker carries a preview pane");
    assert_eq!(preview_lines(&preview)[0], "alpha one");
    assert_eq!(
        map_get(&preview, "title").and_then(Value::as_str),
        Some(a.display().to_string().as_str())
    );

    // Moving the selection swaps the preview to the newly-selected file.
    feed(&rpc, "<C-n>");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu updates"));
    let preview = preview_of(&menu).expect("preview still present");
    assert_eq!(preview_lines(&preview)[0], "bravo only");

    // The document buffer was never touched by any of this.
    assert_eq!(lines(&rpc).await, vec![""]);
}

#[tokio::test]
async fn location_preview_windows_to_the_match_and_marks_it() {
    let dir = temp_dir("picker_preview_loc");
    // Ten lines "L0".."L9"; the source points at 1-based line 5 (= "L4").
    let body: String = (0..10).map(|i| format!("L{i}\n")).collect();
    std::fs::write(dir.join("big.txt"), body).unwrap();
    let big = dir.join("big.txt");
    let src = format!(
        r#"
nx.picker.source {{
  name = "loc_test",
  preview = "location",
  items = function(ctx)
    ctx.push {{ text = "big:5", path = "{big}", row = 5, col = 2 }}
  end,
  confirm = function(item) _G.picked = item.path end,
}}
"#,
        big = big.display(),
    );
    let (rpc, mut incoming) = start(&dir, &src).await;

    exec_lua(&rpc, "nx.picker.open('loc_test')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    let preview = preview_of(&menu).expect("location picker carries a preview pane");

    // The window holds the file and the match line; the 1-based row 5 / col 2
    // rebases to the 0-based loc (4, 1) within the window (which starts at the top
    // for this short file).
    let lines = preview_lines(&preview);
    assert_eq!(lines[4], "L4");
    assert_eq!(preview_loc(&preview), Some((4, 1)));
}

#[tokio::test]
async fn preview_scrolls_with_ctrl_dufb() {
    let dir = temp_dir("picker_preview_scroll");
    // A file far taller than the preview pane so the window never bottoms out.
    let body: String = (0..200).map(|i| format!("L{i}\n")).collect();
    std::fs::write(dir.join("tall.txt"), body).unwrap();
    let tall = dir.join("tall.txt");
    let src = format!(
        r#"
nx.picker.source {{
  name = "scroll_test",
  preview = "file",
  items = function(ctx)
    ctx.push {{ text = "tall.txt", path = "{tall}" }}
  end,
}}
"#,
        tall = tall.display(),
    );
    let (rpc, mut incoming) = start(&dir, &src).await;

    exec_lua(&rpc, "nx.picker.open('scroll_test')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    let preview = preview_of(&menu).expect("file picker carries a preview pane");
    // A "file" target starts at the top, and the window is full (the file dwarfs it).
    let pane_h = preview_lines(&preview).len() as u64;
    let half = (pane_h / 2).max(1);
    let page = pane_h.saturating_sub(2).max(1);
    assert_eq!(preview_first_line(&preview), 1, "starts at the file head");

    // `<C-u>` at the top is clamped — it can't scroll above line 1.
    feed(&rpc, "<C-u>");
    let preview = preview_of(&menu_of(
        &poll_menu(&rpc, &mut incoming).await.expect("after C-u"),
    ))
    .expect("preview present");
    assert_eq!(preview_first_line(&preview), 1, "clamped at the top");

    // `<C-d>` scrolls down half a pane; a second `<C-d>` advances by another half.
    feed(&rpc, "<C-d>");
    let preview = preview_of(&menu_of(
        &poll_menu(&rpc, &mut incoming).await.expect("after C-d"),
    ))
    .expect("preview present");
    assert_eq!(preview_first_line(&preview), 1 + half);

    feed(&rpc, "<C-d>");
    let preview = preview_of(&menu_of(
        &poll_menu(&rpc, &mut incoming).await.expect("after C-d C-d"),
    ))
    .expect("preview present");
    assert_eq!(preview_first_line(&preview), 1 + 2 * half);

    // `<C-u>` walks it back a half page.
    feed(&rpc, "<C-u>");
    let preview = preview_of(&menu_of(
        &poll_menu(&rpc, &mut incoming).await.expect("after C-u"),
    ))
    .expect("preview present");
    assert_eq!(preview_first_line(&preview), 1 + half);

    // `<C-f>` jumps a full page (a two-line overlap, like the editor's own `<C-f>`).
    feed(&rpc, "<C-f>");
    let preview = preview_of(&menu_of(
        &poll_menu(&rpc, &mut incoming).await.expect("after C-f"),
    ))
    .expect("preview present");
    assert_eq!(preview_first_line(&preview), 1 + half + page);

    // `<C-b>` jumps a full page back.
    feed(&rpc, "<C-b>");
    let preview = preview_of(&menu_of(
        &poll_menu(&rpc, &mut incoming).await.expect("after C-b"),
    ))
    .expect("preview present");
    assert_eq!(preview_first_line(&preview), 1 + half);

    // The document buffer was never touched by any of this scrolling.
    assert_eq!(lines(&rpc).await, vec![""]);
}

#[tokio::test]
async fn preview_scroll_resets_when_the_selection_moves() {
    let dir = temp_dir("picker_preview_scroll_reset");
    let body: String = (0..200).map(|i| format!("L{i}\n")).collect();
    std::fs::write(dir.join("one.txt"), &body).unwrap();
    std::fs::write(dir.join("two.txt"), &body).unwrap();
    let one = dir.join("one.txt");
    let two = dir.join("two.txt");
    let src = format!(
        r#"
nx.picker.source {{
  name = "reset_test",
  preview = "file",
  items = function(ctx)
    ctx.push {{ text = "one.txt", path = "{one}" }}
    ctx.push {{ text = "two.txt", path = "{two}" }}
  end,
}}
"#,
        one = one.display(),
        two = two.display(),
    );
    let (rpc, mut incoming) = start(&dir, &src).await;

    exec_lua(&rpc, "nx.picker.open('reset_test')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    let preview = preview_of(&menu).expect("preview pane");
    let half = (preview_lines(&preview).len() as u64 / 2).max(1);

    // Scroll the first file's preview down.
    feed(&rpc, "<C-d>");
    let preview = preview_of(&menu_of(
        &poll_menu(&rpc, &mut incoming).await.expect("after C-d"),
    ))
    .expect("preview present");
    assert_eq!(preview_first_line(&preview), 1 + half);

    // Moving to the next row re-centers on its target — the scroll offset is per
    // selection, so the new file previews from its own head, not the carried offset.
    feed(&rpc, "<C-n>");
    let preview = preview_of(&menu_of(
        &poll_menu(&rpc, &mut incoming).await.expect("after C-n"),
    ))
    .expect("preview present");
    assert_eq!(
        preview_first_line(&preview),
        1,
        "the new selection starts at top"
    );
}

#[tokio::test]
async fn preview_reserves_a_column_so_the_list_keeps_the_rest() {
    let dir = temp_dir("picker_preview_geom");
    std::fs::write(dir.join("a.txt"), "x\n").unwrap();
    let a = dir.join("a.txt");
    let src = format!(
        r#"
nx.picker.source {{
  name = "geom",
  preview = "file",
  items = function(ctx) ctx.push {{ text = "a", path = "{a}" }} end,
}}
"#,
        a = a.display(),
    );
    let (rpc, mut incoming) = start(&dir, &src).await;
    exec_lua(&rpc, "nx.picker.open('geom')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));

    let box_w = map_get(&menu, "width").and_then(Value::as_u64).unwrap();
    let preview = preview_of(&menu).expect("preview present");
    let pw = map_get(&preview, "width").and_then(Value::as_u64).unwrap();
    assert!(
        pw >= 1 && pw < box_w,
        "preview {pw} fits inside box {box_w}"
    );
    // The list keeps `box - preview - 1` (the separator) and stays non-empty.
    assert!(box_w - pw >= 2, "list column keeps at least one cell");
}

#[tokio::test]
async fn preview_less_picker_emits_no_preview_key() {
    let dir = temp_dir("picker_no_preview");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;
    exec_lua(&rpc, "nx.picker.open('fruits')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    assert!(
        preview_of(&menu).is_none(),
        "a source with no preview kind emits no preview pane"
    );
}

#[tokio::test]
async fn unreadable_preview_path_shows_a_visible_placeholder() {
    let dir = temp_dir("picker_preview_missing");
    let missing = dir.join("does-not-exist.txt");
    let src = format!(
        r#"
nx.picker.source {{
  name = "missing",
  preview = "file",
  items = function(ctx) ctx.push {{ text = "gone", path = "{p}" }} end,
}}
"#,
        p = missing.display(),
    );
    let (rpc, mut incoming) = start(&dir, &src).await;
    exec_lua(&rpc, "nx.picker.open('missing')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));

    // The unreadable file yields a single visible placeholder line naming the path —
    // never a silent blank — and no location highlight.
    let preview = preview_of(&menu).expect("preview pane still reserved");
    let lines = preview_lines(&preview);
    assert!(
        lines.iter().any(|l| l.contains("does-not-exist.txt")),
        "placeholder names the path, got {lines:?}"
    );
    assert_eq!(preview_loc(&preview), None);
}

#[tokio::test]
async fn confirm_on_a_file_preview_picker_still_fires() {
    let dir = temp_dir("picker_preview_confirm");
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    let a = dir.join("a.txt");
    let src = format!(
        r#"
nx.picker.source {{
  name = "fc",
  preview = "file",
  items = function(ctx) ctx.push {{ text = "a", path = "{a}" }} end,
  confirm = function(item) _G.picked = item.path end,
}}
"#,
        a = a.display(),
    );
    let (rpc, mut incoming) = start(&dir, &src).await;
    exec_lua(&rpc, "_G.picked = nil; nx.picker.open('fc')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");

    feed(&rpc, "<CR>");
    // Confirm fired with the chosen item and the menu closed (preview untouched it).
    let picked = exec_lua(&rpc, "return _G.picked").await;
    assert_eq!(picked.as_str(), Some(a.display().to_string().as_str()));
}

// ── Mouse: a picker / select grabs the mouse modally while open (Phase 2). The
//    client forwards raw cells; the core hit-tests the click/wheel back to a row. ──

fn menu_u64(menu: &[(Value, Value)], key: &str) -> usize {
    map_get(menu, key)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("menu has a {key}")) as usize
}

/// The global screen cell of list row `r` of an open, **centered** picker, with the
/// number gutter disabled (so the box's text-area cells are global cells). The box's
/// outer top-left is `(row, col)`; the list sits past the top border (+1) and — when
/// a top prompt is shown — its prompt + separator rows (+2); content is one cell past
/// the left border.
fn list_cell(menu: &[(Value, Value)], r: usize) -> (usize, usize) {
    let row = menu_u64(menu, "row");
    let col = menu_u64(menu, "col");
    let has_prompt = map_get(menu, "query").is_some();
    let prompt_top = map_get(menu, "prompt_pos").and_then(Value::as_str) != Some("bottom");
    let chrome = 1 + if has_prompt && prompt_top { 2 } else { 0 };
    (row + chrome + r, col + 1)
}

#[tokio::test]
async fn clicking_a_picker_row_highlights_it_then_confirms_on_a_second_click() {
    let dir = temp_dir("picker_mouse_click");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;
    command(&rpc, "set nonumber norelativenumber").await;

    exec_lua(&rpc, "_G.picked = nil; nx.picker.open('fruits')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    assert_eq!(
        menu_items(&menu),
        vec!["apple", "apricot", "banana", "cherry"]
    );

    // Click the third row ("banana"): it highlights, the document is untouched, and
    // nothing is confirmed yet.
    let (r, c) = list_cell(&menu, 2);
    feed_mouse(&rpc, "left", "press", r, c);
    let menu = menu_of(
        &poll_menu(&rpc, &mut incoming)
            .await
            .expect("redraw after click"),
    );
    assert_eq!(
        menu_u64(&menu, "selected"),
        2,
        "the clicked row is highlighted"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.picked").await,
        Value::Nil,
        "highlighting does not confirm"
    );

    // Click the highlighted row again: it confirms, running the source's confirm with
    // that item (and closing the picker).
    feed_mouse(&rpc, "left", "press", r, c);
    assert_eq!(
        exec_lua(&rpc, "return _G.picked").await.as_str(),
        Some("banana"),
        "clicking the highlighted row confirms it"
    );
    assert_eq!(
        exec_lua(&rpc, "return nx._picker == nil").await,
        Value::Boolean(true),
        "the picker closes on confirm"
    );
}

#[tokio::test]
async fn clicking_off_a_picker_box_cancels_it() {
    let dir = temp_dir("picker_mouse_cancel");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;
    command(&rpc, "set nonumber norelativenumber").await;

    exec_lua(&rpc, "_G.picked = 'unset'; nx.picker.open('fruits')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");

    // The centered box never reaches the top-left corner — a press there lands off it.
    feed_mouse(&rpc, "left", "press", 0, 0);
    assert_eq!(
        exec_lua(&rpc, "return _G.picked").await.as_str(),
        Some("unset"),
        "a click off the box cancels without confirming"
    );
    assert_eq!(
        exec_lua(&rpc, "return nx._picker == nil").await,
        Value::Boolean(true),
        "the picker is cleared on an off-box click"
    );
}

#[tokio::test]
async fn wheeling_over_a_picker_list_moves_the_highlight() {
    let dir = temp_dir("picker_mouse_wheel");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;
    command(&rpc, "set nonumber norelativenumber").await;

    exec_lua(&rpc, "nx.picker.open('fruits')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    assert_eq!(menu_u64(&menu, "selected"), 0, "opens on the first row");
    let (r, c) = list_cell(&menu, 0);

    // A wheel-down notch over the list moves the highlight down one row.
    feed_mouse(&rpc, "wheel", "down", r, c);
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("redraw"));
    assert_eq!(menu_u64(&menu, "selected"), 1);

    // A wheel-up notch moves it back.
    feed_mouse(&rpc, "wheel", "up", r, c);
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("redraw"));
    assert_eq!(menu_u64(&menu, "selected"), 0);
}

#[tokio::test]
async fn wheeling_over_the_preview_pane_scrolls_the_preview() {
    let dir = temp_dir("picker_preview_wheel");
    let body: String = (0..200).map(|i| format!("L{i}\n")).collect();
    std::fs::write(dir.join("one.txt"), &body).unwrap();
    let one = dir.join("one.txt");
    let src = format!(
        r#"
nx.picker.source {{
  name = "pv",
  preview = "file",
  items = function(ctx) ctx.push {{ text = "one.txt", path = "{one}" }} end,
}}
"#,
        one = one.display(),
    );
    let (rpc, mut incoming) = start(&dir, &src).await;
    command(&rpc, "set nonumber norelativenumber").await;

    exec_lua(&rpc, "nx.picker.open('pv')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    let preview = preview_of(&menu).expect("preview pane");
    let half = (preview_lines(&preview).len() as u64 / 2).max(1);
    assert_eq!(
        preview_first_line(&preview),
        1,
        "the preview starts at the top"
    );

    // A wheel notch over the right ~60% of the box (the preview pane) scrolls the
    // preview a half page — the same gesture as <C-d>, not a list move.
    let row = menu_u64(&menu, "row");
    let col = menu_u64(&menu, "col");
    let width = menu_u64(&menu, "width");
    feed_mouse(&rpc, "wheel", "down", row + 3, col + width - 1);
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("after wheel"));
    let preview = preview_of(&menu).expect("preview present");
    assert_eq!(
        preview_first_line(&preview),
        1 + half,
        "the wheel over the preview scrolled it, not the list"
    );
}
