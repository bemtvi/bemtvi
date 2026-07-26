//! Behavior tests for `nx.picker` — the fuzzy finder over the unified float-list
//! widget (`docs/specs/2026-06-14-nx-ui-float-widget.md`, Phase 2: the prompt
//! input-grab, the Rust matcher, dynamic-source forwarding, generation tokens).
//!
//! Black-box like the rest: a real server sources an `init.lua`, the picker is
//! driven over the same msgpack-RPC a UI uses, and the assertions are on the
//! projected `menu` redraw surface (rows, selected, query, match spans) and on the
//! `confirm` side effects read back through `nvim_exec_lua`.
//!
//! Most sources here are custom **in-memory** drivers (no process spawn), so the
//! suite is hermetic — it never depends on `rg` being installed. The two exceptions
//! drive the shipped `files`/`live_grep` sources over a temp tree to pin their
//! *unrestricted* search (ignored + hidden files in, `.git` out); they stay hermetic
//! because every leg of those sources' fallback chain enumerates the same set,
//! whichever binary the machine happens to have.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, command, cursor, drain_to_latest_redraw, exec_lua, feed, feed_mouse, feed_mouse_mod,
    lines, map_get, menu_items, menu_of, poll_menu, spawn, temp_dir,
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

/// The `region` string of every window painted in a redraw map (`"main"` /
/// `"dock_bottom"` / …).
fn regions(map: &[(Value, Value)]) -> Vec<String> {
    let Some(Value::Array(wins)) = map_get(map, "windows") else {
        return Vec::new();
    };
    wins.iter()
        .filter_map(|w| match w {
            Value::Map(m) => map_get(m, "region")
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        })
        .collect()
}

/// The menu's highlighted row, as a (windowed) view index.
fn menu_selected(menu: &[(Value, Value)]) -> Option<u64> {
    map_get(menu, "selected").and_then(Value::as_u64)
}

/// The menu's per-row `marked` flags (multi-select), in visible order.
fn menu_marked(menu: &[(Value, Value)]) -> Vec<bool> {
    match map_get(menu, "marked") {
        Some(Value::Array(a)) => a.iter().map(|v| v.as_bool().unwrap_or(false)).collect(),
        _ => Vec::new(),
    }
}

/// Resolve the `menu.styles[region]` palette id against the frame's top-level
/// `styles` palette and return its `attr` (`"fg"` / `"bg"`) color as `0xRRGGBB`,
/// or `None` when the region (or attribute) is absent.
fn menu_style_color(map: &[(Value, Value)], region: &str, attr: &str) -> Option<u32> {
    let menu = match map_get(map, "menu") {
        Some(Value::Map(m)) => m,
        _ => return None,
    };
    let styles = match map_get(menu, "styles") {
        Some(Value::Map(s)) => s,
        _ => return None,
    };
    let id = map_get(styles, region)?.as_u64()? as usize;
    let palette = match map_get(map, "styles") {
        Some(Value::Array(a)) => a,
        _ => return None,
    };
    match palette.get(id)? {
        Value::Map(style) => map_get(style, attr)?.as_u64().map(|n| n as u32),
        _ => None,
    }
}

/// The fuzzy picker resolves its colors from telescope's highlight groups so a
/// colorscheme themes it automatically: `TelescopeSelection` tints the selected
/// row, `TelescopeMatching` the matched characters, `TelescopeBorder` the box. The
/// server resolves them to `menu.styles` palette ids (each with a fallback chain),
/// so no client hardcodes the popup look.
#[tokio::test]
async fn picker_styles_resolve_from_telescope_groups() {
    let dir = temp_dir("picker_telescope_style");
    let init = format!(
        "vim.api.nvim_set_hl(0, 'TelescopeSelection', {{ bg = '#313244' }})\n\
         vim.api.nvim_set_hl(0, 'TelescopeMatching',  {{ fg = '#f9e2af', bold = true }})\n\
         vim.api.nvim_set_hl(0, 'TelescopeBorder',    {{ fg = '#585b70' }})\n{STATIC_SRC}"
    );
    let (rpc, mut incoming) = start(&dir, &init).await;

    exec_lua(&rpc, "nx.picker.open('fruits')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "ap");
    let map = poll_menu(&rpc, &mut incoming).await.expect("filtered menu");

    assert_eq!(
        menu_style_color(&map, "sel", "bg"),
        Some(0x0031_3244),
        "the selected row uses TelescopeSelection's bg"
    );
    assert_eq!(
        menu_style_color(&map, "match", "fg"),
        Some(0x00f9_e2af),
        "matched characters use TelescopeMatching's fg"
    );
    assert_eq!(
        menu_style_color(&map, "border", "fg"),
        Some(0x0058_5b70),
        "the box border uses TelescopeBorder's fg"
    );
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
async fn open_with_an_initial_query_opens_already_filtered() {
    // `nx.picker.open(name, { query = "ap" })` pre-fills the prompt and kicks the
    // gen-0 run with that query, so the picker opens already narrowed (the seam the
    // cmdline file picker rides — `:e src/ed<Tab>` carries `src/ed` into the box)
    // with the caret parked at the end of the seed.
    let dir = temp_dir("picker_seeded_query");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;

    exec_lua(&rpc, "nx.picker.open('fruits', { query = 'ap' })").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));

    assert_eq!(
        menu_items(&menu),
        vec!["apple", "apricot"],
        "the seeded query filters the very first frame"
    );
    assert_eq!(
        map_get(&menu, "query").and_then(Value::as_str),
        Some("ap"),
        "the prompt shows the seed"
    );
    assert_eq!(
        map_get(&menu, "query_cursor").and_then(Value::as_u64),
        Some(2),
        "the caret parks at the end of the seed so typing appends"
    );

    // Backspacing the seed widens the list — the seed is ordinary prompt text.
    feed(&rpc, "<BS>");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("widened"));
    assert_eq!(menu_items(&menu), vec!["apple", "apricot", "banana"]);
}

#[tokio::test]
async fn open_with_a_title_projects_it_on_the_box() {
    // `nx.picker.open(name, { title = … })` puts a title on the picker box's top
    // border; it crosses the wire on the `menu` map for the clients to render.
    let dir = temp_dir("picker_title");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;

    exec_lua(&rpc, "nx.picker.open('fruits', { title = 'Pick a fruit' })").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    assert_eq!(
        map_get(&menu, "title").and_then(Value::as_str),
        Some("Pick a fruit"),
        "the picker projects its box title"
    );

    // A title-less open carries no title key (so the clients draw a plain border).
    exec_lua(&rpc, "nx.picker.open('fruits')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu reopens"));
    assert_eq!(map_get(&menu, "title"), None, "no title key when unset");
}

#[tokio::test]
async fn built_in_sources_carry_titles() {
    // The shipped sources name themselves on the box (telescope-style).
    let dir = temp_dir("picker_builtin_titles");
    let (rpc, _incoming) = start(&dir, "-- defaults only").await;
    let titles = exec_lua(
        &rpc,
        "local s = nx.picker._sources\n\
         return (s.files.title or '?') .. '|' .. (s.live_grep.title or '?')\n\
           .. '|' .. (s.buffers.title or '?')",
    )
    .await;
    assert_eq!(
        titles.as_str(),
        Some("Find Files|Live Grep|Buffers"),
        "the built-in pickers are titled"
    );
}

#[tokio::test]
async fn multiselect_false_disables_tab_marking() {
    // `nx.picker.open(name, { multiselect = false })` makes `<Tab>` a no-op — a
    // single-choice picker (the cmdline file completer rides this).
    let dir = temp_dir("picker_multiselect_off");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;

    exec_lua(&rpc, "nx.picker.open('fruits', { multiselect = false })").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "<Tab>");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("after <Tab>"));
    assert!(
        menu_marked(&menu).iter().all(|&m| !m),
        "no row is marked when multiselect is off: {:?}",
        menu_marked(&menu)
    );

    // The default (multiselect on) still marks — proving the flag is what changed it.
    exec_lua(&rpc, "nx.picker.open('fruits')").await;
    poll_menu(&rpc, &mut incoming).await.expect("reopens");
    feed(&rpc, "<Tab>");
    let menu = poll_menu(&rpc, &mut incoming).await.expect("after <Tab>");
    let menu = menu_of(&menu);
    assert!(
        menu_marked(&menu).iter().any(|&m| m),
        "the default picker marks on <Tab>: {:?}",
        menu_marked(&menu)
    );
}

/// A picker is an EDITOR-LEVEL overlay: its box is sized and centered over the
/// WHOLE editor, not the focused window. With a vertical split the focused pane is
/// only ~half the width, yet the picker must keep its full-editor width and be
/// flagged `editor_relative` so the client floats it over the entire editor (not
/// squeezed into the active pane). Before the fix the geometry was computed against
/// the focused window's text area, so the box shrank to the split pane and anchored
/// inside it.
#[tokio::test]
async fn picker_overlays_the_whole_editor_not_the_focused_window() {
    let dir = temp_dir("picker_overlay_editor");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;

    // Split vertically so the focused window is ~40 cols wide (half of the 80-col
    // editor). A window-confined picker would size against this narrow pane.
    command(&rpc, "vsplit").await;

    exec_lua(&rpc, "nx.picker.open('fruits')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));

    // Default width is ~80% of the 80-col EDITOR (round(80 * 0.8) = 64), far wider
    // than the ~40-col focused window could hold (~32). This is the headline symptom.
    assert_eq!(
        map_get(&menu, "width").and_then(Value::as_u64),
        Some(64),
        "picker spans ~80% of the whole editor, not the ~40-col split pane",
    );
    // The outer box (width + 2 border = 66) is centered over the whole 80-col
    // editor: col = (80 - 66) / 2 = 7. A window-anchored box would center within the
    // ~40-col pane (and the client would offset it by the pane origin).
    assert_eq!(map_get(&menu, "col").and_then(Value::as_u64), Some(7));
    // Flagged editor-relative so every client anchors it to the windows area (the
    // whole editor), not the focused window's text inner.
    assert_eq!(
        map_get(&menu, "editor_relative").and_then(Value::as_bool),
        Some(true),
        "picker is flagged editor-relative so it floats over the whole editor",
    );
}

/// The editor-relative picker's **mouse hit-test** is editor-absolute too: with the
/// focused window offset into the editor (the right pane of a vsplit, origin ~col 40),
/// a click on a list row — addressed in global cells, where the box is centered over
/// the WHOLE editor — must still land on that row. Before the fix the hit-test rebased
/// the box onto the focused pane, so a click on the (editor-centered) box missed.
#[tokio::test]
async fn clicking_a_picker_row_in_a_split_hits_via_editor_absolute_geometry() {
    let dir = temp_dir("picker_mouse_split");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;
    command(&rpc, "set nonumber norelativenumber").await;
    // Split vertically and focus the RIGHT pane, so the focused window's origin is
    // well into the editor (~col 40) — not the (0,0) a single window would have.
    command(&rpc, "vsplit").await;
    command(&rpc, "wincmd l").await;

    exec_lua(&rpc, "_G.picked = nil; nx.picker.open('fruits')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    // The box is editor-absolute and centered over the whole editor, not the pane.
    assert_eq!(
        map_get(&menu, "editor_relative").and_then(Value::as_bool),
        Some(true),
    );

    // Click the third row ("banana") at its GLOBAL cell: the hit-test inverts the
    // same editor-absolute geometry, so the row highlights.
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
        "the clicked row highlights — the hit-test is editor-absolute",
    );
    // A second click on the highlighted row confirms it.
    feed_mouse(&rpc, "left", "press", r, c);
    assert_eq!(
        exec_lua(&rpc, "return _G.picked").await.as_str(),
        Some("banana"),
        "clicking the highlighted row confirms it",
    );
}

/// Poll the focused window's non-empty rendered rows until there are `want` of them
/// — a picker `<C-q>` send schedules its named-list dock tab open a couple ticks
/// later (and focuses it), so its rendered rows appear there.
async fn poll_sent_rows(rpc: &Rpc, want: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for _ in 0..200 {
        rows = lines(rpc)
            .await
            .into_iter()
            .filter(|l| !l.is_empty())
            .collect();
        if rows.len() == want {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    rows
}

/// Phase 4: `<C-q>` sends the picker's **current (filtered)** results to a
/// window-independent **named list** keyed `<picker>:<query>` (so each distinct
/// search is its own persistent dock tab). With `'qfdock'` on (the default), it opens
/// as a bottom-dock tab and `<CR>` on an entry jumps into the main layer. Only the
/// rows matching the live query are sent, not every candidate.
#[tokio::test]
async fn ctrl_q_sends_filtered_results_to_a_named_dock_list() {
    let dir = temp_dir("picker_send_loclist");
    let a = dir.join("a.txt");
    let b = dir.join("b.txt");
    std::fs::write(&a, "one\ntwo\nthree\n").expect("write a");
    std::fs::write(&b, "x\ny\nz\n").expect("write b");
    let src = format!(
        r#"
nx.picker.source {{
  name = "locs",
  items = function(ctx)
    ctx.push {{ text = "foo one", path = "{a}", row = 1, col = 1 }}
    ctx.push {{ text = "foo two", path = "{b}", row = 2, col = 1 }}
    ctx.push {{ text = "bar three", path = "{a}", row = 3, col = 1 }}
  end,
}}
"#,
        a = a.display(),
        b = b.display(),
    );
    let (rpc, mut incoming) = start(&dir, &src).await;

    exec_lua(&rpc, "nx.picker.open('locs')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");

    // Filter to the two "foo" rows (the matcher drops "bar three").
    feed(&rpc, "foo");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("filtered menu"));
    assert_eq!(menu_items(&menu), vec!["foo one", "foo two"]);

    // <C-q>: send only those filtered rows to the named list `locs:foo`, which opens
    // as its own dock tab (focused). Its rendered rows are the two filtered entries
    // (not "bar three"); each renders as `file|row col col| text`.
    feed(&rpc, "<C-q>");
    let rows = poll_sent_rows(&rpc, 2).await;
    assert!(
        rows.len() == 2 && rows[0].ends_with("| foo one") && rows[1].ends_with("| foo two"),
        "only the filtered results were sent, got {rows:?}"
    );

    // It opened as a bottom-dock tab (the default nxvim way).
    let rd = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw");
    assert!(
        regions(&rd).iter().any(|r| r == "dock_bottom"),
        "results opened in the bottom dock, got {:?}",
        regions(&rd)
    );

    // <CR> on the first entry jumps into its file, in the main layer.
    feed(&rpc, "<CR>");
    nxvim_test_harness::barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["one", "two", "three"],
        "jumped into the first entry's file (main layer)"
    );
    assert_eq!(cursor(&rpc).await.0, 1, "landed on the entry's line");
}

/// Phase 5: `<Tab>` multi-selects picker rows (marking them and advancing), the
/// marks project into the `menu` redraw, and `<C-q>` then sends **only the marked**
/// rows to the named list (telescope's send-selected). The marks survive even if
/// the cursor is elsewhere.
#[tokio::test]
async fn tab_marks_rows_and_ctrl_q_sends_only_the_marked() {
    let dir = temp_dir("picker_multiselect");
    let a = dir.join("a.txt");
    let b = dir.join("b.txt");
    std::fs::write(&a, "one\ntwo\nthree\n").expect("write a");
    std::fs::write(&b, "x\ny\nz\n").expect("write b");
    let src = format!(
        r#"
nx.picker.source {{
  name = "locs",
  items = function(ctx)
    ctx.push {{ text = "alpha", path = "{a}", row = 1, col = 1 }}
    ctx.push {{ text = "beta", path = "{b}", row = 2, col = 1 }}
    ctx.push {{ text = "gamma", path = "{a}", row = 3, col = 1 }}
  end,
}}
"#,
        a = a.display(),
        b = b.display(),
    );
    let (rpc, mut incoming) = start(&dir, &src).await;

    exec_lua(&rpc, "nx.picker.open('locs')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");

    // <Tab> marks the current row and advances: mark "alpha", then "beta".
    feed(&rpc, "<Tab>");
    feed(&rpc, "<Tab>");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("marked menu"));
    assert_eq!(
        menu_marked(&menu),
        vec![true, true, false],
        "alpha and beta are marked, gamma is not"
    );

    // <C-q> sends only the marked rows (not gamma) to the named list, in mark order.
    feed(&rpc, "<C-q>");
    let rows = poll_sent_rows(&rpc, 2).await;
    assert!(
        rows.len() == 2 && rows[0].ends_with("| alpha") && rows[1].ends_with("| beta"),
        "only the marked rows were sent, in mark order, got {rows:?}"
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

#[tokio::test]
async fn picker_file_open_records_a_jump_so_ctrl_o_returns() {
    // The finder's location-less file confirm (`files` source → `nx.picker.edit`
    // with no `row`) opens through `nx._open_switchbuf`, which — like `:edit` — is
    // a jump: `<C-o>` must return to where we were, not one entry further back.
    let dir = temp_dir("picker_jump");
    let file = dir.join("other.txt");
    std::fs::write(&file, "x\ny\nz\n").unwrap();
    let (rpc, _incoming) = start(&dir, "").await;

    // Buffer 1: six lines of "a"; search then `n` `n` lands on line 4.
    feed(&rpc, "ia<CR>a<CR>a<CR>a<CR>a<CR>a<Esc>");
    feed(&rpc, "gg");
    feed(&rpc, "/a<CR>");
    feed(&rpc, "nn");
    assert_eq!(cursor(&rpc).await.0, 4);

    // Pick a file in the finder (no location) — a plain open, still a jump.
    exec_lua(
        &rpc,
        &format!("nx.picker.edit({{ path = '{}' }})", file.display()),
    )
    .await;
    assert_eq!(
        lines(&rpc).await,
        vec!["x", "y", "z"],
        "opened the picked file"
    );

    // `<C-o>` returns to line 4 of the original buffer.
    feed(&rpc, "<C-o>");
    assert_eq!(
        cursor(&rpc).await.0,
        4,
        "returns to the last match, not the previous one"
    );
    assert_eq!(lines(&rpc).await.len(), 6, "back in the six-line buffer");
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

/// The buffers picker previews an open buffer from its IN-MEMORY lines, not by
/// re-reading the file — so the preview reflects live unsaved edits, and (the
/// reported bug) it works on a daemon / the web client where the host FS is off-tick
/// and a filesystem read can only return a "loading…" placeholder. Proven here by an
/// unsaved edit: the on-disk file says one thing, the buffer another, and the preview
/// must show the buffer's content.
#[tokio::test]
async fn buffers_preview_reads_the_in_memory_buffer_not_the_file() {
    let dir = temp_dir("picker_preview_buf");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "on disk line\nsecond\n").unwrap();
    let (rpc, mut incoming) = start(&dir, "").await;

    // Open the file, then edit line 1 in memory WITHOUT saving (a synchronous normal-
    // mode change) — the buffer now diverges from disk.
    exec_lua(&rpc, &format!("vim.cmd('edit {}')", file.display())).await;
    feed(&rpc, "ccLIVE UNSAVED EDIT<Esc>");

    exec_lua(&rpc, "nx.picker.open('buffers')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    let preview = preview_of(&menu).expect("buffers picker carries a preview pane");

    // The preview shows the buffer's in-memory first line, not the on-disk one.
    assert_eq!(preview_lines(&preview)[0], "LIVE UNSAVED EDIT");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "on disk line\nsecond\n"
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

// ===== the confirm target layer (`layer = "main" | "active"`) ==============

/// A source declaring `layer = "main"` opens its confirmed file in the **main**
/// editor layer, even when the picker was launched from a dock. Regression: the
/// files/live_grep pickers opened into whatever layer had focus, so picking a file
/// while focused in a sidebar dock yanked the document into the sidebar.
#[tokio::test]
async fn a_main_layer_source_opens_the_file_in_main_not_the_dock() {
    let dir = temp_dir("picker_layer_main");
    let target = dir.join("target.txt");
    std::fs::write(&target, "alpha\nbeta\n").unwrap();
    let init = format!(
        r#"
nx.picker.source({{
  name = "myfiles",
  layer = "main",
  items = function(ctx) ctx.push({{ text = "target", path = "{}" }}) end,
  confirm = function(item, mode, layer) nx.picker.edit(item, mode, layer) end,
}})
"#,
        target.display()
    );
    let (rpc, mut incoming) = start(&dir, &init).await;

    // A left dock with a scratch buffer; focus it and mark it.
    exec_lua(&rpc, "nx.dock.open({ side = 'left', size = 20 })").await;
    feed(&rpc, "idock<Esc>");
    assert_eq!(lines(&rpc).await, vec!["dock"], "the dock buffer is marked");

    // Open the picker from the dock and confirm the only file.
    exec_lua(&rpc, "nx.picker.open('myfiles')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "<CR>");
    nxvim_test_harness::barrier(&rpc).await;

    // The file opened in the focused window — which is now the MAIN layer.
    assert_eq!(
        lines(&rpc).await,
        vec!["alpha", "beta"],
        "the file opened in the main layer"
    );

    // The dock is untouched: it still shows its scratch buffer. Under the bug the
    // dock window would have been replaced by the file.
    exec_lua(&rpc, "nx.dock.focus('left')").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["dock"],
        "the dock buffer is unchanged — the file did not land in the sidebar"
    );
}

/// The default (no `layer`, i.e. `"active"`): a source confirmed while focused in a
/// dock opens its file IN that dock — the picker does not force every open to main,
/// so a dock-local picker (like the layer-scoped `buffers` source) stays local.
#[tokio::test]
async fn an_active_layer_source_opens_the_file_in_the_focused_dock() {
    let dir = temp_dir("picker_layer_active");
    let target = dir.join("target.txt");
    std::fs::write(&target, "alpha\nbeta\n").unwrap();
    let init = format!(
        r#"
nx.picker.source({{
  name = "myfiles",
  items = function(ctx) ctx.push({{ text = "target", path = "{}" }}) end,
  confirm = function(item, mode, layer) nx.picker.edit(item, mode, layer) end,
}})
"#,
        target.display()
    );
    let (rpc, mut incoming) = start(&dir, &init).await;

    exec_lua(&rpc, "nx.dock.open({ side = 'left', size = 20 })").await;
    feed(&rpc, "idock<Esc>");

    exec_lua(&rpc, "nx.picker.open('myfiles')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "<CR>");
    nxvim_test_harness::barrier(&rpc).await;

    // The file opened in the focused dock window.
    assert_eq!(
        lines(&rpc).await,
        vec!["alpha", "beta"],
        "the file opened in the focused dock"
    );

    // The main layer is untouched (still its empty [No Name] buffer).
    exec_lua(&rpc, "nx.layer.main()").await;
    assert_eq!(
        lines(&rpc).await,
        vec![""],
        "main is untouched — the active-layer open stayed in the dock"
    );
}

/// The shipped sources carry the right confirm target: `files` / `live_grep` open
/// in `main` (they find documents to edit), `buffers` in `active` (it is scoped to
/// the focused layer, so it must stay there — `:ls` semantics).
#[tokio::test]
async fn shipped_sources_declare_their_confirm_layer() {
    let dir = temp_dir("picker_shipped_layers");
    let (rpc, _incoming) = start(&dir, "").await;
    assert_eq!(
        exec_lua(&rpc, "return nx.picker._sources.files.layer").await,
        Value::from("main"),
    );
    assert_eq!(
        exec_lua(&rpc, "return nx.picker._sources.live_grep.layer").await,
        Value::from("main"),
    );
    assert_eq!(
        exec_lua(&rpc, "return nx.picker._sources.buffers.layer").await,
        Value::from("active"),
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

/// A tab-indented preview line is expanded to spaces (at the default tabstop of 8)
/// before it reaches the client. The preview paints char-by-char with each cell one
/// column, so a raw `\t` would render as a zero-width control and collapse the
/// indentation of tab-indented content (help code blocks, aligned source) leftward.
#[tokio::test]
async fn preview_expands_tabs_to_spaces() {
    let dir = temp_dir("picker_preview_tabs");
    // A leading tab (→ column 8) and a mid-line tab from column 2 (→ column 8: 6 spaces).
    std::fs::write(dir.join("t.txt"), "\tindented\nab\tcd\n").unwrap();
    let t = dir.join("t.txt");
    let src = format!(
        r#"
nx.picker.source {{
  name = "tabs_test",
  preview = "file",
  items = function(ctx) ctx.push {{ text = "t.txt", path = "{t}" }} end,
  confirm = function(item) _G.picked = item.path end,
}}
"#,
        t = t.display(),
    );
    let (rpc, mut incoming) = start(&dir, &src).await;

    exec_lua(&rpc, "nx.picker.open('tabs_test')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    let preview = preview_of(&menu).expect("file picker carries a preview pane");
    let lines = preview_lines(&preview);

    // The leading tab becomes 8 spaces; the mid-line tab pads column 2 out to 8 (6
    // spaces). No `\t` survives into the painted lines.
    assert_eq!(lines[0], "        indented");
    assert_eq!(lines[1], "ab      cd");
    assert!(lines.iter().all(|l| !l.contains('\t')));
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

/// A vim help file (`doc/*.txt` with a `ft=help` modeline) resolves its preview
/// highlight language to `vimdoc`, even though the `.txt` extension alone maps to no
/// grammar — while an ordinary `.txt` (no modeline / not under `doc/`) stays plain and
/// a `.rs` still resolves to `rust`. This is grammar-independent: it asserts the
/// preview pane's reported `filetype`, which drives (and, when the `vimdoc` parser is
/// installed, produces) the actual highlighting.
#[tokio::test]
async fn help_doc_preview_resolves_to_vimdoc() {
    let dir = temp_dir("picker_preview_help");
    let doc = dir.join("doc");
    std::fs::create_dir_all(&doc).unwrap();
    // A real help file: under doc/, .txt, with a trailing vim ft=help modeline.
    std::fs::write(
        doc.join("thing.txt"),
        "*thing*\tThe thing.\n\nBody text.\n\nvim:tw=78:ft=help:\n",
    )
    .unwrap();
    // A plain .txt under doc/ but WITHOUT the modeline stays unhighlighted.
    std::fs::write(doc.join("plain.txt"), "just notes\nno modeline\n").unwrap();
    // A .rs still resolves by extension.
    std::fs::write(dir.join("code.rs"), "fn main() {}\n").unwrap();
    let help = doc.join("thing.txt");
    let plain = doc.join("plain.txt");
    let code = dir.join("code.rs");
    let src = format!(
        r#"
nx.picker.source {{
  name = "ft_test",
  preview = "location",
  items = function(ctx)
    ctx.push {{ text = "help", path = "{help}", row = 1, col = 0 }}
    ctx.push {{ text = "plain", path = "{plain}", row = 1, col = 0 }}
    ctx.push {{ text = "code", path = "{code}", row = 1, col = 0 }}
  end,
  confirm = function(item) _G.picked = item.path end,
}}
"#,
        help = help.display(),
        plain = plain.display(),
        code = code.display(),
    );
    let (rpc, mut incoming) = start(&dir, &src).await;

    let filetype_of = |menu: &[(Value, Value)]| -> String {
        let preview = preview_of(menu).expect("picker carries a preview pane");
        map_get(&preview, "filetype")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };

    exec_lua(&rpc, "nx.picker.open('ft_test')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    assert_eq!(filetype_of(&menu), "vimdoc", "help doc -> vimdoc");

    feed(&rpc, "<C-n>");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu updates"));
    assert_eq!(filetype_of(&menu), "", "plain .txt stays unhighlighted");

    feed(&rpc, "<C-n>");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu updates"));
    assert_eq!(
        filetype_of(&menu),
        "rust",
        ".rs still resolves by extension"
    );
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

#[tokio::test]
async fn shift_or_horizontal_wheel_scrolls_the_preview_sideways() {
    let dir = temp_dir("picker_preview_hwheel");
    // Lines far wider than the preview pane (80 cols of a repeating digit run) so the
    // horizontal window never bottoms out, and a known column maps to a known char.
    let wide = "0123456789".repeat(8);
    let body: String = (0..20).map(|_| format!("{wide}\n")).collect();
    std::fs::write(dir.join("wide.txt"), &body).unwrap();
    let wide_path = dir.join("wide.txt");
    let src = format!(
        r#"
nx.picker.source {{
  name = "pvh",
  preview = "file",
  items = function(ctx) ctx.push {{ text = "wide.txt", path = "{p}" }} end,
}}
"#,
        p = wide_path.display(),
    );
    let (rpc, mut incoming) = start(&dir, &src).await;
    command(&rpc, "set nonumber norelativenumber").await;

    exec_lua(&rpc, "nx.picker.open('pvh')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("menu opens"));
    let preview = preview_of(&menu).expect("preview pane");
    let before: String = preview_lines(&preview)[0].clone();
    assert!(before.starts_with("0123456789"), "unscrolled: {before:?}");

    // A horizontal wheel notch over the preview pane (right ~60% of the box) scrolls it
    // 6 columns right — the visible line drops its first 6 chars, no vertical move.
    let row = menu_u64(&menu, "row");
    let col = menu_u64(&menu, "col");
    let width = menu_u64(&menu, "width");
    let (pr, pc) = (row + 3, col + width - 1);
    feed_mouse(&rpc, "wheel", "right", pr, pc);
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("after h-wheel"));
    let preview = preview_of(&menu).expect("preview present");
    assert_eq!(
        preview_first_line(&preview),
        1,
        "horizontal scroll leaves the vertical window alone"
    );
    let after: String = preview_lines(&preview)[0].clone();
    let expected: String = before.chars().skip(6).collect();
    assert_eq!(after, expected, "the line scrolled 6 columns right");

    // Shift + a vertical wheel-down notch is the same gesture (vim's <S-ScrollWheel>):
    // it advances another 6 columns right, not down.
    feed_mouse_mod(&rpc, "wheel", "down", "S", pr, pc);
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("after S-wheel"));
    let preview = preview_of(&menu).expect("preview present");
    let after2: String = preview_lines(&preview)[0].clone();
    assert_eq!(after2, before.chars().skip(12).collect::<String>());

    // A horizontal wheel-left notch scrolls back; another is clamped at column 0.
    feed_mouse(&rpc, "wheel", "left", pr, pc);
    feed_mouse(&rpc, "wheel", "left", pr, pc);
    feed_mouse(&rpc, "wheel", "left", pr, pc);
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("after lefts"));
    let preview = preview_of(&menu).expect("preview present");
    assert_eq!(
        preview_lines(&preview)[0],
        before,
        "scrolled all the way back to column 0 (clamped, never negative)"
    );
}

// ----- Phase 2: picker opens honor 'switchbuf' ------------------------------

/// The current tab id (`nvim_get_current_tabpage`).
async fn cur_tab(rpc: &Rpc) -> u64 {
    exec_lua(rpc, "return vim.api.nvim_get_current_tabpage()")
        .await
        .as_u64()
        .expect("tab id")
}

/// The current buffer id (`nvim_get_current_buf`).
async fn cur_buf(rpc: &Rpc) -> u64 {
    exec_lua(rpc, "return vim.api.nvim_get_current_buf()")
        .await
        .as_u64()
        .expect("buf id")
}

/// The number of open tab pages.
async fn tab_count(rpc: &Rpc) -> u64 {
    exec_lua(rpc, "return #vim.api.nvim_list_tabpages()")
        .await
        .as_u64()
        .expect("tab count")
}

/// Two named buffers, one per tab (zebra in tab 1, mango in the current tab 2),
/// returning `(dir, zebra path, mango path, zebra bufnr)`.
async fn two_tabs_two_buffers(rpc: &Rpc, tag: &str) -> (std::path::PathBuf, String, String, u64) {
    let dir = temp_dir(tag);
    let zebra = dir.join("zebrafile.txt");
    let mango = dir.join("mangofile.txt");
    std::fs::write(&zebra, "alpha\nbeta\n").expect("write zebra");
    std::fs::write(&mango, "one\ntwo\n").expect("write mango");
    command(rpc, &format!("edit {}", zebra.display())).await; // tab 1 shows zebra
    let zebra_buf = cur_buf(rpc).await;
    command(rpc, &format!("tabedit {}", mango.display())).await; // tab 2 shows mango (current)
    assert_eq!(cur_tab(rpc).await, 2, "set-up leaves us on tab 2");
    (
        dir,
        zebra.display().to_string(),
        mango.display().to_string(),
        zebra_buf,
    )
}

/// The built-in `buffers` picker honors the default `'switchbuf'=usetab`: picking a
/// buffer already shown in another tab switches to that tab instead of swapping it
/// into the current window.
#[tokio::test]
async fn buffers_picker_usetab_switches_to_the_tab_showing_the_buffer() {
    let dir = temp_dir("picker_buf_usetab");
    let (rpc, mut incoming) = start(&dir, "").await;
    let (_d, _zebra, _mango, zebra_buf) =
        two_tabs_two_buffers(&rpc, "picker_buf_usetab_files").await;

    exec_lua(&rpc, "nx.picker.open('buffers')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "zebra"); // narrow to the zebra buffer
    poll_menu(&rpc, &mut incoming).await;
    feed(&rpc, "<CR>");
    nxvim_test_harness::barrier(&rpc).await;

    assert_eq!(tab_count(&rpc).await, 2, "no new tab opened");
    assert_eq!(
        cur_tab(&rpc).await,
        1,
        "picking the buffer switched to the tab already showing it"
    );
    assert_eq!(cur_buf(&rpc).await, zebra_buf);
}

/// With `'switchbuf'` empty, the `buffers` picker opens the chosen buffer in the
/// current window — no tab hop (the gating guard).
#[tokio::test]
async fn buffers_picker_empty_switchbuf_stays_in_current_tab() {
    let dir = temp_dir("picker_buf_empty");
    let (rpc, mut incoming) = start(&dir, "").await;
    let (_d, _zebra, _mango, zebra_buf) =
        two_tabs_two_buffers(&rpc, "picker_buf_empty_files").await;
    exec_lua(&rpc, "nx.o.switchbuf = ''").await;

    exec_lua(&rpc, "nx.picker.open('buffers')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "zebra");
    poll_menu(&rpc, &mut incoming).await;
    feed(&rpc, "<CR>");
    nxvim_test_harness::barrier(&rpc).await;

    assert_eq!(cur_tab(&rpc).await, 2, "empty 'switchbuf' makes no tab hop");
    assert_eq!(
        cur_buf(&rpc).await,
        zebra_buf,
        "the buffer opened in the current tab's window"
    );
}

/// A location-less confirm (the `files` shape: `nx.picker.edit` on a `path`-only
/// item) routes through the `'switchbuf'`-aware open, so picking a file already
/// shown in another tab switches to that tab.
#[tokio::test]
async fn files_style_open_usetab_switches_to_the_tab_showing_the_file() {
    let dir = temp_dir("picker_file_usetab");
    let zebra = dir.join("zebrafile.txt");
    let mango = dir.join("mangofile.txt");
    std::fs::write(&zebra, "alpha\nbeta\n").expect("write zebra");
    std::fs::write(&mango, "one\ntwo\n").expect("write mango");
    let src = format!(
        r#"
nx.picker.source {{
  name = "openfiles",
  items = function(ctx)
    ctx.push {{ text = "zebra", path = "{z}" }}
  end,
  confirm = function(item) nx.picker.edit(item) end,
}}
"#,
        z = zebra.display(),
    );
    let (rpc, mut incoming) = start(&dir, &src).await;

    command(&rpc, &format!("edit {}", zebra.display())).await; // tab 1 shows zebra
    let zebra_buf = cur_buf(&rpc).await;
    command(&rpc, &format!("tabedit {}", mango.display())).await; // tab 2 (current)

    exec_lua(&rpc, "nx.picker.open('openfiles')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "<CR>");
    nxvim_test_harness::barrier(&rpc).await;

    assert_eq!(tab_count(&rpc).await, 2, "no new tab opened");
    assert_eq!(
        cur_tab(&rpc).await,
        1,
        "a location-less file open follows 'switchbuf'=usetab"
    );
    assert_eq!(cur_buf(&rpc).await, zebra_buf);
}

// ----- Phase 3: <C-t> opens the selection in a new tab ----------------------

/// `<C-t>` on a `buffers` entry opens that buffer in a **new** tab — even when the
/// buffer is already shown elsewhere (the explicit tab gesture bypasses
/// `'switchbuf'`).
#[tokio::test]
async fn ctrl_t_opens_a_buffer_in_a_new_tab() {
    let dir = temp_dir("picker_ctrl_t_buf");
    let (rpc, mut incoming) = start(&dir, "").await;
    let (_d, _zebra, _mango, zebra_buf) =
        two_tabs_two_buffers(&rpc, "picker_ctrl_t_buf_files").await;

    exec_lua(&rpc, "nx.picker.open('buffers')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "zebra"); // zebra is already shown in tab 1
    poll_menu(&rpc, &mut incoming).await;
    feed(&rpc, "<C-t>");
    nxvim_test_harness::barrier(&rpc).await;

    assert_eq!(tab_count(&rpc).await, 3, "<C-t> opened a third tab");
    assert_eq!(cur_tab(&rpc).await, 3, "the new tab is focused");
    assert_eq!(
        cur_buf(&rpc).await,
        zebra_buf,
        "the new tab shows the picked buffer"
    );
}

/// `<C-t>` on a located item opens its file in a new tab with the cursor on the
/// item's row/col.
#[tokio::test]
async fn ctrl_t_opens_a_located_item_in_a_new_tab() {
    let dir = temp_dir("picker_ctrl_t_loc");
    let file = dir.join("locfile.txt");
    std::fs::write(&file, "one\ntwo\nthree\nfour\n").expect("write file");
    let src = format!(
        r#"
nx.picker.source {{
  name = "locs",
  items = function(ctx)
    ctx.push {{ text = "to three", path = "{f}", row = 3, col = 2 }}
  end,
  confirm = function(item, mode) nx.picker.edit(item, mode) end,
}}
"#,
        f = file.display(),
    );
    let (rpc, mut incoming) = start(&dir, &src).await;

    exec_lua(&rpc, "nx.picker.open('locs')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "<C-t>");
    nxvim_test_harness::barrier(&rpc).await;

    assert_eq!(tab_count(&rpc).await, 2, "<C-t> opened a second tab");
    assert_eq!(cur_tab(&rpc).await, 2, "the new tab is focused");
    assert_eq!(
        lines(&rpc).await,
        vec!["one", "two", "three", "four"],
        "the new tab shows the file"
    );
    // cursor() reports (1-based row, 0-based col); 1-based col=2 lands at 1.
    assert_eq!(cursor(&rpc).await, (3, 1), "the cursor landed on the item");
}

/// `<CR>` is unaffected by the new `<C-t>` mode — it still opens in the current
/// window (a guard that the per-confirm mode resets, not leaks).
#[tokio::test]
async fn cr_still_opens_in_the_current_window_after_adding_ctrl_t() {
    let dir = temp_dir("picker_cr_guard");
    let file = dir.join("crfile.txt");
    std::fs::write(&file, "x\ny\n").expect("write file");
    let src = format!(
        r#"
nx.picker.source {{
  name = "openone",
  items = function(ctx) ctx.push {{ text = "x", path = "{f}" }} end,
  confirm = function(item) nx.picker.edit(item) end,
}}
"#,
        f = file.display(),
    );
    let (rpc, mut incoming) = start(&dir, &src).await;

    exec_lua(&rpc, "nx.picker.open('openone')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "<CR>");
    nxvim_test_harness::barrier(&rpc).await;

    assert_eq!(tab_count(&rpc).await, 1, "<CR> opens no new tab");
    assert_eq!(
        lines(&rpc).await,
        vec!["x", "y"],
        "the file opened in place"
    );
}

// ----- Phase 5: <C-x> / <C-v> open the selection in a split -----------------

/// Number of windows in the current tab page.
async fn win_count(rpc: &Rpc) -> u64 {
    exec_lua(rpc, "return #vim.api.nvim_tabpage_list_wins(0)")
        .await
        .as_u64()
        .expect("win count")
}

/// The current window's width / height (`nvim_win_get_width/height`).
async fn win_width(rpc: &Rpc) -> u64 {
    exec_lua(rpc, "return vim.api.nvim_win_get_width(0)")
        .await
        .as_u64()
        .expect("win width")
}
async fn win_height(rpc: &Rpc) -> u64 {
    exec_lua(rpc, "return vim.api.nvim_win_get_height(0)")
        .await
        .as_u64()
        .expect("win height")
}

/// `<C-x>` on a `buffers` entry opens it in a horizontal split of the current tab
/// (a new window, full width, reduced height) — no new tab, bypassing 'switchbuf'.
#[tokio::test]
async fn ctrl_x_opens_a_buffer_in_a_horizontal_split() {
    let dir = temp_dir("picker_ctrl_x_buf");
    let (rpc, mut incoming) = start(&dir, "").await;
    let (_d, _zebra, _mango, zebra_buf) =
        two_tabs_two_buffers(&rpc, "picker_ctrl_x_buf_files").await;

    exec_lua(&rpc, "nx.picker.open('buffers')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "zebra");
    poll_menu(&rpc, &mut incoming).await;
    feed(&rpc, "<C-x>");
    nxvim_test_harness::barrier(&rpc).await;

    assert_eq!(tab_count(&rpc).await, 2, "<C-x> opens no new tab");
    assert_eq!(cur_tab(&rpc).await, 2, "the split is in the current tab");
    assert_eq!(win_count(&rpc).await, 2, "<C-x> split the window");
    assert_eq!(cur_buf(&rpc).await, zebra_buf, "the split shows the buffer");
    assert!(
        win_width(&rpc).await >= 78,
        "a horizontal split keeps full width, got {}",
        win_width(&rpc).await
    );
}

/// `<C-v>` on a `buffers` entry opens it in a vertical split (a new window, reduced
/// width, full height).
#[tokio::test]
async fn ctrl_v_opens_a_buffer_in_a_vertical_split() {
    let dir = temp_dir("picker_ctrl_v_buf");
    let (rpc, mut incoming) = start(&dir, "").await;
    let (_d, _zebra, _mango, zebra_buf) =
        two_tabs_two_buffers(&rpc, "picker_ctrl_v_buf_files").await;

    exec_lua(&rpc, "nx.picker.open('buffers')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "zebra");
    poll_menu(&rpc, &mut incoming).await;
    feed(&rpc, "<C-v>");
    nxvim_test_harness::barrier(&rpc).await;

    assert_eq!(cur_tab(&rpc).await, 2, "the split is in the current tab");
    assert_eq!(win_count(&rpc).await, 2, "<C-v> split the window");
    assert_eq!(
        cur_buf(&rpc).await,
        zebra_buf,
        "the vsplit shows the buffer"
    );
    assert!(
        win_width(&rpc).await < 60,
        "a vertical split narrows the window, got {}",
        win_width(&rpc).await
    );
}

/// `<C-x>` on a located item opens its file in a split with the cursor on the
/// item's row/col.
#[tokio::test]
async fn ctrl_x_opens_a_located_item_in_a_split() {
    let dir = temp_dir("picker_ctrl_x_loc");
    let file = dir.join("locfile.txt");
    std::fs::write(&file, "one\ntwo\nthree\nfour\n").expect("write file");
    let src = format!(
        r#"
nx.picker.source {{
  name = "locs",
  items = function(ctx)
    ctx.push {{ text = "to three", path = "{f}", row = 3, col = 2 }}
  end,
  confirm = function(item, mode) nx.picker.edit(item, mode) end,
}}
"#,
        f = file.display(),
    );
    let (rpc, mut incoming) = start(&dir, &src).await;

    exec_lua(&rpc, "nx.picker.open('locs')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "<C-x>");
    nxvim_test_harness::barrier(&rpc).await;

    assert_eq!(tab_count(&rpc).await, 1, "<C-x> opens no new tab");
    assert_eq!(win_count(&rpc).await, 2, "<C-x> split the window");
    assert_eq!(
        lines(&rpc).await,
        vec!["one", "two", "three", "four"],
        "the split shows the file"
    );
    assert_eq!(cursor(&rpc).await, (3, 1), "the cursor landed on the item");
    let _ = win_height(&rpc).await; // height is reduced; exact value depends on layout
}

// ----- resume (telescope's `resume` / `<leader>fr`) --------------------------

#[tokio::test]
async fn resume_reopens_the_last_picker_with_its_query() {
    // `nx.picker.resume()` replays the closed picker's frozen content: the displayed
    // rows and the prompt text come back exactly as they were.
    let dir = temp_dir("picker_resume_query");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;

    // Open, narrow to "ap", then close without confirming.
    exec_lua(&rpc, "nx.picker.open('fruits')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "ap");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("filtered"));
    assert_eq!(menu_items(&menu), vec!["apple", "apricot"]);
    feed(&rpc, "<Esc>");

    assert_eq!(
        exec_lua(&rpc, "return nx.picker._last.name").await.as_str(),
        Some("fruits"),
        "the resume slot remembers the source",
    );

    // Resume: the same picker reopens with the same rows and prompt.
    exec_lua(&rpc, "nx.picker.resume()").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("resumed menu"));
    assert_eq!(
        menu_items(&menu),
        vec!["apple", "apricot"],
        "resume replays the exact rows the picker showed at close",
    );
    assert_eq!(
        map_get(&menu, "query").and_then(Value::as_str),
        Some("ap"),
        "the prompt shows the resumed query",
    );
}

#[tokio::test]
async fn resume_restores_the_highlighted_row_and_marks() {
    // Resume restores the *selection*, not just the query: the highlighted row lands
    // back where it was, and multi-select marks reappear on the same items.
    let dir = temp_dir("picker_resume_selection");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;

    exec_lua(&rpc, "nx.picker.open('fruits')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");

    // Mark apple + apricot (each <Tab> marks the row and advances), landing the
    // highlight on banana (row 2).
    feed(&rpc, "<Tab>"); // mark apple -> cursor on apricot
    feed(&rpc, "<Tab>"); // mark apricot -> cursor on banana
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("marked"));
    assert_eq!(menu_selected(&menu), Some(2), "highlight on banana");
    assert_eq!(
        menu_marked(&menu),
        vec![true, true, false, false],
        "apple + apricot marked",
    );
    feed(&rpc, "<Esc>");

    // Resume: same highlight, same marks.
    exec_lua(&rpc, "nx.picker.resume()").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("resumed"));
    assert_eq!(
        menu_items(&menu),
        vec!["apple", "apricot", "banana", "cherry"]
    );
    assert_eq!(
        menu_selected(&menu),
        Some(2),
        "resume restores the highlighted row",
    );
    assert_eq!(
        menu_marked(&menu),
        vec![true, true, false, false],
        "resume restores the multi-select marks",
    );
}

#[tokio::test]
async fn resume_with_no_prior_picker_is_a_graceful_noop() {
    // Before any picker has opened, `resume()` notifies and opens nothing — it must
    // never error (a `<leader>fr` on a fresh session is harmless).
    let dir = temp_dir("picker_resume_empty");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;

    let ok = exec_lua(&rpc, "return pcall(nx.picker.resume)").await;
    assert_eq!(ok, Value::Boolean(true), "resume never errors");
    assert!(
        poll_menu(&rpc, &mut incoming).await.is_none(),
        "no picker opens when there is nothing to resume",
    );
}

#[tokio::test]
async fn resume_after_confirm_replays_the_list() {
    // Closing via confirm (not just cancel) also snapshots, so resuming after picking
    // an item reopens the same frozen list with the highlight on the confirmed row.
    let dir = temp_dir("picker_resume_confirm");
    let (rpc, mut incoming) = start(&dir, STATIC_SRC).await;

    exec_lua(&rpc, "_G.picked = nil; nx.picker.open('fruits')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "ap");
    poll_menu(&rpc, &mut incoming).await.expect("filtered");
    feed(&rpc, "<CR>");
    assert_eq!(
        exec_lua(&rpc, "return _G.picked").await.as_str(),
        Some("apple"),
        "confirm fired",
    );

    // Resume reopens the picker as it was at confirm: same rows, same prompt.
    exec_lua(&rpc, "nx.picker.resume()").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("resumed"));
    assert_eq!(menu_items(&menu), vec!["apple", "apricot"]);
    assert_eq!(map_get(&menu, "query").and_then(Value::as_str), Some("ap"));

    // Confirming again works — the item tables survived the snapshot/resume.
    exec_lua(&rpc, "_G.picked = nil").await;
    feed(&rpc, "<CR>");
    assert_eq!(
        exec_lua(&rpc, "return _G.picked").await.as_str(),
        Some("apple"),
        "confirm still resolves the item after a resume",
    );
}

#[tokio::test]
async fn a_non_resumable_source_does_not_clobber_the_resume_slot() {
    // `resumable = false` (the cmdline file completer) opens without touching the
    // resume slot, so a transient internal picker can't shadow the last real one —
    // `<leader>fr` still reopens the user-facing picker.
    let dir = temp_dir("picker_resume_optout");
    let src = format!(
        "{STATIC_SRC}\n\
         nx.picker.source {{ name = 'scratch', resumable = false,\n\
           items = function(ctx) ctx.push {{ text = 'x' }} end }}"
    );
    let (rpc, mut incoming) = start(&dir, &src).await;

    // A real picker, narrowed and closed — it owns the resume slot.
    exec_lua(&rpc, "nx.picker.open('fruits')").await;
    poll_menu(&rpc, &mut incoming).await.expect("fruits opens");
    feed(&rpc, "ap");
    poll_menu(&rpc, &mut incoming).await.expect("filtered");
    feed(&rpc, "<Esc>");

    // The transient picker opens and closes without taking the slot.
    exec_lua(&rpc, "nx.picker.open('scratch')").await;
    poll_menu(&rpc, &mut incoming).await.expect("scratch opens");
    feed(&rpc, "<Esc>");

    assert_eq!(
        exec_lua(&rpc, "return nx.picker._last.name").await.as_str(),
        Some("fruits"),
        "the non-resumable picker left the slot on the last real one",
    );

    // Resume reopens fruits at "ap", not the scratch picker.
    exec_lua(&rpc, "nx.picker.resume()").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("resumed"));
    assert_eq!(menu_items(&menu), vec!["apple", "apricot"]);
}

#[tokio::test]
async fn resume_snapshot_is_bounded_to_a_window_around_the_cursor() {
    // A huge result set isn't kept whole — the snapshot (and the retained item
    // tables) are bounded to a window around the cursor, so a closed 100k-row picker
    // doesn't pin its entire list in memory.
    let dir = temp_dir("picker_resume_window");
    let src = r#"
nx.picker.source {
  name = "many",
  items = function(ctx)
    for i = 0, 2999 do ctx.push { text = string.format("item%04d", i) } end
  end,
}
"#;
    let (rpc, mut incoming) = start(&dir, src).await;

    exec_lua(&rpc, "nx.picker.open('many')").await;
    poll_menu(&rpc, &mut incoming).await.expect("opens");
    feed(&rpc, "<Esc>");

    // Only the window's item tables are retained for resume (≤ RESUME_WINDOW = 1000),
    // not all 3000.
    let kept = exec_lua(
        &rpc,
        "local n = 0; for _ in pairs(nx.picker._last.picker.items) do n = n + 1 end; return n",
    )
    .await;
    assert_eq!(
        kept,
        Value::from(1000),
        "retained items are bounded to the window",
    );

    // Resume still works — the windowed rows replay, starting at the cursor (row 0).
    exec_lua(&rpc, "nx.picker.resume()").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("resumed"));
    assert_eq!(
        menu_items(&menu).first().map(String::as_str),
        Some("item0000"),
        "the window starts at the cursor (row 0)",
    );
}

/// A dynamic source whose result *order* changes every run (live-grep's reality).
/// Each call flips the order, so re-running on resume would show a DIFFERENT list —
/// resume must replay the captured snapshot instead, without invoking the source.
const SHUFFLING_SRC: &str = r#"
_G.run = 0
nx.picker.source {
  name = "shuf",
  dynamic = true,
  debounce = 0,
  items = function(ctx)
    _G.run = _G.run + 1
    if _G.run % 2 == 1 then
      ctx.push { text = "one" }
      ctx.push { text = "two" }
    else
      ctx.push { text = "two" }
      ctx.push { text = "one" }
    end
  end,
  confirm = function(item) _G.picked = item.text end,
}
"#;

#[tokio::test]
async fn resume_replays_the_snapshot_without_rerunning_a_dynamic_source() {
    // The crux: a live-grep-style source has no stable order, so resume can't re-run
    // it — it replays the exact rows captured at close.
    let dir = temp_dir("picker_resume_dynamic");
    let (rpc, mut incoming) = start(&dir, SHUFFLING_SRC).await;

    exec_lua(&rpc, "nx.picker.open('shuf')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("opens"));
    assert_eq!(menu_items(&menu), vec!["one", "two"], "first run order");
    assert_eq!(exec_lua(&rpc, "return _G.run").await, Value::from(1));
    feed(&rpc, "<Esc>");

    // Resume shows the SAME rows — and the source was not invoked again (run still 1).
    // A re-run would flip to ["two", "one"].
    exec_lua(&rpc, "nx.picker.resume()").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("resumed"));
    assert_eq!(
        menu_items(&menu),
        vec!["one", "two"],
        "resume replays the frozen snapshot, not a fresh (reordered) search",
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.run").await,
        Value::from(1),
        "the source was NOT re-run on resume",
    );

    // Confirm resolves the snapshot's item; editing the query DOES re-run the source.
    feed(&rpc, "x");
    poll_menu(&rpc, &mut incoming)
        .await
        .expect("re-ran on edit");
    assert_eq!(
        exec_lua(&rpc, "return _G.run").await,
        Value::from(2),
        "a query edit after resume re-runs the live source",
    );
}

/// The `curbuf` / `diagnostics` / `keymaps` / `pickers` sources are shipped
/// built-in (no config needed) and drive for real on a bare server: `curbuf`
/// navigates the cursor, and `diagnostics` lists a set diagnostic.
#[tokio::test]
async fn builtin_native_sources_drive_without_config() {
    let dir = temp_dir("picker_builtin_native");
    let (rpc, mut incoming) = start(&dir, "").await;

    // All four register out of the box.
    let ok = exec_lua(
        &rpc,
        r#"local s = nx.picker._sources
           for _, n in ipairs({ "curbuf","diagnostics","keymaps","pickers" }) do
             if s[n] == nil then return "missing:" .. n end
           end
           return "ok""#,
    )
    .await;
    assert_eq!(ok.as_str(), Some("ok"), "native sources ship built-in");

    // curbuf: give the buffer lines, pick the second row, cursor jumps to line 2.
    feed(&rpc, "ione<CR>two<CR>three<Esc>gg");
    exec_lua(&rpc, "nx.picker.open('curbuf')").await;
    poll_menu(&rpc, &mut incoming).await.expect("curbuf opens");
    feed(&rpc, "<C-n>");
    feed(&rpc, "<CR>");
    let (line, _) = cursor(&rpc).await;
    assert_eq!(line, 2, "confirming the second curbuf row jumps to line 2");

    // diagnostics: set one, then it shows up in the diagnostics picker.
    exec_lua(
        &rpc,
        "nx.diagnostic.set(1, 0, { { lnum = 1, col = 0, message = 'boom', \
         severity = nx.diagnostic.severity.ERROR } })",
    )
    .await;
    exec_lua(&rpc, "nx.picker.open('diagnostics')").await;
    let m = poll_menu(&rpc, &mut incoming)
        .await
        .expect("diagnostics opens");
    let items = menu_items(&menu_of(&m));
    assert!(
        items
            .iter()
            .any(|r| r.contains("boom") && r.contains("ERROR")),
        "the set diagnostic appears in the picker, got {items:?}"
    );
    feed(&rpc, "<Esc>");

    // keymaps: a space-leader map's lhs renders as `<Space>` notation, not a
    // hard-to-see literal blank (nx.keytrans), so the row is legible.
    exec_lua(
        &rpc,
        "vim.g.mapleader = ' '; nx.keymap.set('n', '<leader>xy', function() end, \
         { desc = 'space leader probe' })",
    )
    .await;
    exec_lua(&rpc, "nx.picker.open('keymaps')").await;
    poll_menu(&rpc, &mut incoming).await.expect("keymaps opens");
    // Filter to the probe map (the list windows its projection, so type to surface it).
    feed(&rpc, "xy");
    let mut rows = Vec::new();
    for _ in 0..40 {
        if let Some(m) = poll_menu(&rpc, &mut incoming).await {
            rows = menu_items(&menu_of(&m));
            if rows.iter().any(|r| r.contains("xy")) {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        rows.iter().any(|r| r.contains("<Space>xy")),
        "the space-leader map shows as <Space>xy, got {rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r.contains("  xy ")),
        "the lhs is not rendered with a literal leading space, got {rows:?}"
    );
}

/// The `keymaps` picker also lists the CURRENT buffer's buffer-local maps (not just
/// the globals), so a plugin's on-buffer bindings are discoverable while its buffer
/// is focused — the "what does `a` do in the tree?" case. A buffer-local map shadows
/// a global one at the same lhs, and buffer-local rows carry a `@` marker.
#[tokio::test]
async fn keymaps_picker_lists_buffer_local_and_shadows_globals() {
    let dir = temp_dir("picker_keymaps_buflocal");
    let (rpc, mut incoming) = start(&dir, "").await;

    // A plugin-style buffer-local map on the current buffer (the `a` → create case),
    // a GLOBAL map at `gx`, and a BUFFER-LOCAL map at the same `gx` that must shadow
    // the global (only the binding that actually fires should appear).
    exec_lua(
        &rpc,
        "nx.keymap.set('n', 'aa', function() end, \
           { buffer = 0, desc = 'nxvim-tree: Create a file' })
         nx.keymap.set('n', 'gx', function() end, { desc = 'global probe' })
         nx.keymap.set('n', 'gx', function() end, \
           { buffer = 0, desc = 'buflocal shadow probe' })",
    )
    .await;

    // The buffer-local plugin map is discoverable: filter to it and it shows with the
    // `@` buffer-local marker and its friendly description.
    exec_lua(&rpc, "nx.picker.open('keymaps')").await;
    poll_menu(&rpc, &mut incoming).await.expect("keymaps opens");
    feed(&rpc, "Create");
    let mut rows = Vec::new();
    for _ in 0..40 {
        if let Some(m) = poll_menu(&rpc, &mut incoming).await {
            rows = menu_items(&menu_of(&m));
            if rows.iter().any(|r| r.contains("Create a file")) {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        rows.iter().any(|r| r.contains("@")
            && r.contains("aa")
            && r.contains("nxvim-tree: Create a file")),
        "the buffer-local plugin map shows with a @ marker and friendly desc, got {rows:?}"
    );
    feed(&rpc, "<Esc>");

    // Shadowing: `gx` has both a global and a buffer-local map. Only the buffer-local
    // one (which is what fires) appears — the global `gx` is dropped.
    exec_lua(&rpc, "nx.picker.open('keymaps')").await;
    poll_menu(&rpc, &mut incoming)
        .await
        .expect("keymaps reopens");
    feed(&rpc, "gx");
    let mut rows = Vec::new();
    for _ in 0..40 {
        if let Some(m) = poll_menu(&rpc, &mut incoming).await {
            rows = menu_items(&menu_of(&m));
            if rows.iter().any(|r| r.contains("gx")) {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let gx: Vec<&String> = rows.iter().filter(|r| r.contains("gx")).collect();
    assert!(
        gx.iter().any(|r| r.contains("buflocal shadow probe")),
        "the buffer-local gx map is listed, got {rows:?}"
    );
    assert!(
        !gx.iter().any(|r| r.contains("global probe")),
        "the global gx map is shadowed by the buffer-local one and not listed, got {rows:?}"
    );
    feed(&rpc, "<Esc>");
}

/// `nx.mark.list` reads the server-pushed `nx._marks` mirror, and the built-in
/// `marks` picker lists + jumps to a set mark. Bare server, driven for real:
/// set mark `a` on a distinct line, confirm it via the picker, cursor lands there.
#[tokio::test]
async fn builtin_marks_list_and_picker_jump() {
    let dir = temp_dir("picker_marks");
    let (rpc, mut incoming) = start(&dir, "").await;

    // Three lines; put mark `a` on the middle (uniquely-worded) one, cursor back up.
    feed(&rpc, "iaaa first<CR>zzz middle<CR>ccc last<Esc>2Gma gg");

    // nx.mark.list surfaces mark `a` at the 0-based middle line (1) with its text.
    let info = exec_lua(
        &rpc,
        r#"for _, m in ipairs(nx.mark.list()) do
             if m.name == "a" then return m.line .. "|" .. m.text end
           end
           return "no-a""#,
    )
    .await;
    assert_eq!(
        info.as_str(),
        Some("1|zzz middle"),
        "nx.mark.list reports mark a at line 1 (0-based) with the line's text"
    );

    // The picker lists it; filter to the unique word and confirm the jump.
    exec_lua(&rpc, "nx.picker.open('marks')").await;
    poll_menu(&rpc, &mut incoming)
        .await
        .expect("marks picker opens");
    feed(&rpc, "zzz");
    let mut rows = Vec::new();
    for _ in 0..40 {
        if let Some(m) = poll_menu(&rpc, &mut incoming).await {
            rows = menu_items(&menu_of(&m));
            if rows.iter().any(|r| r.contains("zzz middle")) {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        rows.iter()
            .any(|r| r.starts_with("a  ") && r.contains("zzz middle")),
        "the marks picker lists mark a, got {rows:?}"
    );
    feed(&rpc, "<CR>");
    let (line, _) = cursor(&rpc).await;
    assert_eq!(
        line, 2,
        "confirming mark a jumps the cursor to line 2 (1-based)"
    );
}

/// The built-in `jumplist` picker reads the window jumplist mirror
/// (`nx.jumplist.get`), lists the focused window's jump history newest-first with
/// each entry's line text, and confirming a row jumps the cursor there. Bare
/// server: build a jump from line 2, then jump away — the recorded departure shows
/// up and picking it returns the cursor to it.
#[tokio::test]
async fn builtin_jumplist_picker_lists_and_jumps() {
    let dir = temp_dir("picker_jumplist");
    let (rpc, mut incoming) = start(&dir, "").await;

    // Ships built-in with no config.
    let ok = exec_lua(&rpc, "return tostring(nx.picker._sources.jumplist ~= nil)").await;
    assert_eq!(ok.as_str(), Some("true"), "jumplist source ships built-in");

    // Three lines; sit on the uniquely-worded middle line, then `G` (a jump) records
    // it as a departure the jumplist remembers.
    feed(&rpc, "iaaa first<CR>zzz middle<CR>ccc last<Esc>2GG");

    // The picker lists the recorded departure (line 2, its text); filter, confirm.
    exec_lua(&rpc, "nx.picker.open('jumplist')").await;
    poll_menu(&rpc, &mut incoming)
        .await
        .expect("jumplist picker opens");
    feed(&rpc, "zzz");
    let mut rows = Vec::new();
    for _ in 0..40 {
        if let Some(m) = poll_menu(&rpc, &mut incoming).await {
            rows = menu_items(&menu_of(&m));
            if rows.iter().any(|r| r.contains("zzz middle")) {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        rows.iter()
            .any(|r| r.contains("2:") && r.contains("zzz middle")),
        "the jumplist picker lists the line-2 departure, got {rows:?}"
    );
    feed(&rpc, "<CR>");
    let (line, _) = cursor(&rpc).await;
    assert_eq!(
        line, 2,
        "confirming the jumplist row jumps the cursor back to line 2 (1-based)"
    );
}

/// The shipped `files` source searches **unrestricted**: a `.gitignore`d file and a
/// dotfile both show up (`rg -uu` = `--no-ignore --hidden`), while `.git`'s own
/// contents stay out. Hermetic without pinning a binary — every leg of the fallback
/// chain (`rg -uu -g'!.git'` → `find … -not -path '*/.git/*'` → `nx.fs.walk{hidden}`,
/// whose default `skip` prunes `.git`) is specified to produce exactly this set, so
/// the assertion holds whichever one the machine lands on.
#[tokio::test]
async fn builtin_files_picker_lists_ignored_and_hidden_files() {
    let dir = temp_dir("picker_files_unrestricted_cfg");
    let proj = temp_dir("picker_files_unrestricted");
    // A git repo (rg honors .gitignore only inside one) with one ignored file, one
    // dotfile, one plain file, and a `.git` entry that must never be listed.
    std::fs::create_dir(proj.join(".git")).expect("mkdir .git");
    std::fs::write(proj.join(".git").join("config"), "[core]\n").expect("write .git/config");
    std::fs::write(proj.join(".gitignore"), "ignored.txt\n").expect("write .gitignore");
    std::fs::write(proj.join("ignored.txt"), "x\n").expect("write ignored.txt");
    std::fs::write(proj.join(".hidden.txt"), "x\n").expect("write .hidden.txt");
    std::fs::write(proj.join("visible.txt"), "x\n").expect("write visible.txt");

    let (rpc, mut incoming) = start(&dir, "").await;
    command(&rpc, &format!("cd {}", proj.display())).await;

    exec_lua(&rpc, "nx.picker.open('files')").await;
    let mut rows = Vec::new();
    for _ in 0..60 {
        if let Some(m) = poll_menu(&rpc, &mut incoming).await {
            rows = menu_items(&menu_of(&m));
            if rows.len() >= 4 {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    for want in ["visible.txt", "ignored.txt", ".hidden.txt", ".gitignore"] {
        assert!(
            rows.iter().any(|r| r == want),
            "the files picker is unrestricted, so it lists {want}; got {rows:?}"
        );
    }
    assert!(
        !rows.iter().any(|r| r.starts_with(".git/")),
        ".git's contents stay out of the listing; got {rows:?}"
    );
}

/// Same for `live_grep`: the match inside a `.gitignore`d file and inside a dotfile
/// both surface. Binary-agnostic for the same reason as the `files` test — `rg -uu`,
/// `grep -rnI --exclude-dir=.git` and the `nx.fs` walk all search this set.
#[tokio::test]
async fn builtin_live_grep_searches_ignored_and_hidden_files() {
    let dir = temp_dir("picker_grep_unrestricted_cfg");
    let proj = temp_dir("picker_grep_unrestricted");
    std::fs::create_dir(proj.join(".git")).expect("mkdir .git");
    std::fs::write(proj.join(".gitignore"), "ignored.txt\n").expect("write .gitignore");
    for name in ["ignored.txt", ".hidden.txt", "visible.txt"] {
        std::fs::write(proj.join(name), "zqxneedle here\n").expect("write file");
    }

    let (rpc, mut incoming) = start(&dir, "").await;
    command(&rpc, &format!("cd {}", proj.display())).await;

    exec_lua(&rpc, "nx.picker.open('live_grep')").await;
    poll_menu(&rpc, &mut incoming)
        .await
        .expect("live_grep picker opens");
    feed(&rpc, "zqxneedle");

    let mut rows = Vec::new();
    for _ in 0..200 {
        if let Some(m) = poll_menu(&rpc, &mut incoming).await {
            let now = menu_items(&menu_of(&m));
            if now.len() >= rows.len() {
                rows = now;
            }
            if rows.len() >= 3 {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    for want in ["ignored.txt", ".hidden.txt", "visible.txt"] {
        assert!(
            rows.iter().any(|r| r.starts_with(&format!("{want}:"))),
            "live_grep is unrestricted, so it matches inside {want}; got {rows:?}"
        );
    }
}
