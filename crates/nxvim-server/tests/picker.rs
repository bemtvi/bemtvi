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
    attach, cursor, drain_to_latest_redraw, exec_lua, feed, lines, map_get, spawn, temp_dir,
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
  items = function(ctx, push, done)
    for _, t in ipairs({ "apple", "apricot", "banana", "cherry" }) do
      push { text = t, fruit = t }
    end
    done()
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
  items = function(ctx, push, done)
    if ctx.query ~= "" then
      push { text = ctx.query .. "-1", q = ctx.query }
      push { text = ctx.query .. "-2", q = ctx.query }
    end
    done()
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
  items = function(ctx, push, done)
    if ctx.query ~= "" then
      _G.runs = (_G.runs or 0) + 1
      push { text = ctx.query }
    end
    done()
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
  items = function(ctx, push, done)
    if ctx.query ~= "" then push { text = ctx.query } end
    done()
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
  items = function(ctx, push, done)
    for i = 1, 50 do push { text = string.format("row-%02d", i), n = i } end
    done()
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
  items = function(ctx, push, done)
    for i = 1, 100000 do push { text = "item-" .. i, n = i } end
    done()
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
  items = function(ctx, push, done)
    -- Only "hit" yields a result; defer both the push and done() so the in-flight
    -- window is observable.
    nx.timer(function()
      if ctx.query == "hit" then push { text = "FOUND:" .. ctx.query } end
      done()
    end, 60)
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
    // A source that streams via `nx.spawn` (the incremental on_stdout path):
    // `printf` (coreutils) emits three lines, which arrive as picker rows. Proves
    // the streaming primitive end-to-end, not just the synchronous push path.
    let dir = temp_dir("picker_stream");
    let src = r#"
nx.picker.source {
  name = "stream",
  items = function(ctx, push, done)
    nx.spawn {
      cmd = "printf",
      args = { "one\ntwo\nthree\n" },
      on_stdout = function(lines)
        for _, l in ipairs(lines) do
          if l ~= "" then push { text = l } end
        end
      end,
      on_exit = function() done() end,
    }
  end,
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
    // A dynamic source whose push is *deferred* (a timer) carries the generation
    // of the query that scheduled it. Typing past that query (bumping the
    // generation) must drop the late push so a superseded query's rows never show.
    let dir = temp_dir("picker_stale");
    let src = r#"
nx.picker.source {
  name = "deferred",
  dynamic = true,
  debounce = 0,                              -- isolate the generation gate from the debounce
  items = function(ctx, push, done)
    if ctx.query == "" then return done() end
    -- Push one row for this query, but only after a delay — by which time a
    -- faster typist may have moved the generation on.
    nx.timer(function()
      push { text = "row:" .. ctx.query }
      done()
    end, 40)
  end,
  confirm = function(item) _G.picked = item.text end,
}
"#;
    let (rpc, mut incoming) = start(&dir, src).await;
    exec_lua(&rpc, "nx.picker.open('deferred')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");

    // Type "a" then immediately "b": the gen-1 ("a") deferred push must be dropped;
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
    // A source whose push is deferred (a timer) and which forgets to register
    // `ctx.on_cancel` keeps "streaming" after its picker closes. Because every
    // picker opens at generation 0, that orphaned push (gen 0) must NOT land in
    // the next picker (also gen 0) — the identity guard drops it.
    let dir = temp_dir("picker_leak");
    let src = r#"
_G.runs = 0
nx.picker.source {
  name = "leak",
  items = function(ctx, push, done)
    _G.runs = _G.runs + 1
    local tag = "run" .. _G.runs            -- distinguishes each picker's run
    nx.timer(function() push { text = tag }; done() end, 40)
  end,
  confirm = function(item) end,
}
"#;
    let (rpc, mut incoming) = start(&dir, src).await;

    // Picker A: open, then immediately cancel — its deferred "run1" push is still
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
        "the closed picker A's late push must not leak into picker B, got {items:?}"
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
  items = function(ctx, push, done)
    for i = 1, 50 do push { text = string.format("row-%02d", i) } end
    done()
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
async fn example_config_loads_and_opens_a_picker() {
    // The shipped `examples/ui-picker` config must load and wire its leader maps.
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/ui-picker")
        .canonicalize()
        .expect("example dir");
    let init = ServerInit {
        config_dir: Some(example.clone()),
        runtimepath: vec![example],
        ..Default::default()
    };
    let (rpc, mut incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    // Open the custom static "colours" source and confirm it filters.
    exec_lua(&rpc, "nx.picker.open('colours')").await;
    feed(&rpc, "ceru"); // a subsequence unique to "cerulean"
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    assert_eq!(menu_items(&menu), vec!["cerulean"]);
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
