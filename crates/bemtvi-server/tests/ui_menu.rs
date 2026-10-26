//! Behavior tests for `btv.ui.menu` — the context menu on the `btv.ui.select` widget
//! (`docs/plans/2026-10-25-modeless-context-menu.md`): rows with a detail column,
//! greyed-out rows and separators that can't be chosen, and the `"mouse"` anchor that
//! opens the list at the pointer, where it acts like a GUI menu.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    attach, exec_lua, feed, feed_mouse, map_get, menu_items, menu_of, poll_menu, poll_true, spawn,
    write_temp,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// An 80x24 session editing a few lines of text, with no line numbers so a screen
/// column is a buffer column.
async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    let text = "hello world\n".repeat(30);
    let path = write_temp("ui_menu", "txt", &text);
    let (rpc, incoming) = spawn(ServerInit {
        file: Some(path),
        ..Default::default()
    });
    attach(&rpc, 80, 24).await;
    exec_lua(&rpc, "btv.cmd('set nonumber norelativenumber')").await;
    (rpc, incoming)
}

/// Open `btv.ui.menu` over `entries` (a Lua list literal) with `opts`, recording the
/// resolved entry's label (or `"nil"`) in `_G.got`.
async fn open(rpc: &Rpc, entries: &str, opts: &str) {
    exec_lua(
        rpc,
        &format!(
            "_G.got = 'unset'
             btv.ui.menu({entries}, {opts}):next(function(e)
               _G.got = e and e.label or 'nil'
             end)"
        ),
    )
    .await;
}

async fn got(rpc: &Rpc) -> String {
    exec_lua(rpc, "return _G.got")
        .await
        .as_str()
        .unwrap_or("")
        .to_string()
}

const MIXED: &str = "{
  { label = 'Cut', detail = 'Ctrl+X' },
  { separator = true },
  { label = 'Paste', detail = 'Ctrl+V', enabled = false },
  { label = 'Select All', detail = 'Ctrl+A', action = 'select_all' },
}";

fn num(menu: &[(Value, Value)], key: &str) -> u64 {
    map_get(menu, key)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("menu.{key} missing in {menu:?}"))
}

/// Put the pointer at `(row, col)` without a menu open: a left click on the text.
fn point_at(rpc: &Rpc, row: usize, col: usize) {
    feed_mouse(rpc, "left", "press", row, col);
    feed_mouse(rpc, "left", "release", row, col);
}

#[tokio::test]
async fn keys_skip_rows_that_cant_be_chosen() {
    let (rpc, _inc) = start().await;
    open(&rpc, MIXED, "{}").await;
    // The first `j` reveals the highlight on Cut; the next one jumps past the
    // separator and the greyed-out Paste.
    feed(&rpc, "jj<CR>");
    assert_eq!(got(&rpc).await, "Select All");
}

#[tokio::test]
async fn the_resolved_entry_keeps_its_own_fields() {
    let (rpc, _inc) = start().await;
    exec_lua(
        &rpc,
        &format!(
            "_G.action = nil
             btv.ui.menu({MIXED}):next(function(e) _G.action = e.action end)"
        ),
    )
    .await;
    feed(&rpc, "G<CR>");
    assert!(poll_true(&rpc, "return _G.action == 'select_all'").await);
}

#[tokio::test]
async fn rows_carry_details_and_greyed_out_highlights() {
    let (rpc, mut inc) = start().await;
    // No colorscheme is loaded here, so give the greyed-out look something to link to.
    exec_lua(&rpc, "btv.hl.define(0, 'Comment', { fg = '#808080' })").await;
    open(&rpc, MIXED, "{}").await;
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("menu"));
    assert_eq!(menu_items(&menu), ["Cut", "", "Paste", "Select All"]);
    let separators = map_get(&menu, "separators").expect("separators are flagged");
    let separators: Vec<bool> = separators
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_bool().unwrap())
        .collect();
    assert_eq!(separators, [false, true, false, false]);
    let kinds = map_get(&menu, "kinds").expect("details ride the kinds column");
    let kinds: Vec<Option<&str>> = kinds
        .as_array()
        .unwrap()
        .iter()
        .map(Value::as_str)
        .collect();
    assert_eq!(
        kinds,
        [Some("Ctrl+X"), None, Some("Ctrl+V"), Some("Ctrl+A")]
    );
    let hls = map_get(&menu, "row_hls").expect("greyed-out rows are painted");
    let painted: Vec<bool> = hls
        .as_array()
        .unwrap()
        .iter()
        .map(|v| !v.is_nil())
        .collect();
    assert_eq!(painted, [false, false, true, false]);
}

#[tokio::test]
async fn the_mouse_anchor_opens_at_the_pointer() {
    let (rpc, mut inc) = start().await;
    point_at(&rpc, 5, 10);
    open(&rpc, MIXED, "{ anchor = 'mouse' }").await;
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("menu"));
    assert_eq!(
        map_get(&menu, "editor_relative"),
        Some(&Value::Boolean(true))
    );
    assert_eq!((num(&menu, "row"), num(&menu, "col")), (5, 10));
    assert_eq!(num(&menu, "height"), 4);
    assert_eq!(num(&menu, "kind_col"), ("Select All".len() + 2) as u64);
}

#[tokio::test]
async fn near_the_bottom_right_corner_it_opens_up_and_left() {
    let (rpc, mut inc) = start().await;
    point_at(&rpc, 20, 78);
    open(&rpc, MIXED, "{ anchor = 'mouse' }").await;
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("menu"));
    let (row, col) = (num(&menu, "row"), num(&menu, "col"));
    let (w, h) = (num(&menu, "width"), num(&menu, "height"));
    assert_eq!(col + w + 2, 79, "the box's right edge is on the pointer");
    assert_eq!(row + h + 2, 21, "the box's bottom edge is on the pointer");
}

#[tokio::test]
async fn without_a_mouse_event_the_mouse_anchor_falls_back_to_the_cursor() {
    let (rpc, mut inc) = start().await;
    open(&rpc, MIXED, "{ anchor = 'mouse' }").await;
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("menu"));
    assert_eq!(map_get(&menu, "editor_relative"), None);
}

#[tokio::test]
async fn one_click_on_a_row_at_the_pointer_chooses_it() {
    let (rpc, mut inc) = start().await;
    point_at(&rpc, 5, 10);
    open(&rpc, MIXED, "{ anchor = 'mouse' }").await;
    poll_menu(&rpc, &mut inc).await.expect("menu");
    // The box's border is at (5, 10); Select All is its fourth row.
    feed_mouse(&rpc, "left", "press", 9, 13);
    feed_mouse(&rpc, "left", "release", 9, 13);
    assert!(poll_true(&rpc, "return _G.got == 'Select All'").await);
}

#[tokio::test]
async fn clicking_a_separator_or_greyed_out_row_does_nothing() {
    let (rpc, _inc) = start().await;
    point_at(&rpc, 5, 10);
    open(&rpc, MIXED, "{ anchor = 'mouse' }").await;
    for row in [7, 8] {
        feed_mouse(&rpc, "left", "press", row, 13);
        feed_mouse(&rpc, "left", "release", row, 13);
    }
    assert_eq!(got(&rpc).await, "unset", "still open");
    feed(&rpc, "<Esc>");
    assert_eq!(got(&rpc).await, "nil");
}

#[tokio::test]
async fn hovering_a_row_highlights_it() {
    let (rpc, mut inc) = start().await;
    point_at(&rpc, 5, 10);
    open(&rpc, MIXED, "{ anchor = 'mouse' }").await;
    feed_mouse(&rpc, "move", "move", 9, 13);
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("menu"));
    assert_eq!(
        map_get(&menu, "selected_active"),
        Some(&Value::Boolean(true))
    );
    assert_eq!(num(&menu, "selected"), 3);
    // Hovering a greyed-out row or a separator lights nothing, the way a GUI menu
    // does, so `<CR>` there chooses nothing.
    for at in [8, 7] {
        feed_mouse(&rpc, "move", "move", at, 13);
        let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("menu"));
        assert_eq!(
            map_get(&menu, "selected_active"),
            Some(&Value::Boolean(false)),
            "row at screen row {at}"
        );
    }
    feed(&rpc, "<CR>");
    assert_eq!(got(&rpc).await, "unset");
    // Back on a row that can be chosen, it lights again.
    feed_mouse(&rpc, "move", "move", 9, 13);
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("menu"));
    assert_eq!(
        map_get(&menu, "selected_active"),
        Some(&Value::Boolean(true))
    );
    assert_eq!(num(&menu, "selected"), 3);
    feed(&rpc, "<CR>");
    assert_eq!(got(&rpc).await, "Select All");
}

#[tokio::test]
async fn a_click_off_the_menu_dismisses_it() {
    let (rpc, _inc) = start().await;
    point_at(&rpc, 5, 10);
    open(&rpc, MIXED, "{ anchor = 'mouse' }").await;
    feed_mouse(&rpc, "left", "press", 15, 40);
    assert!(poll_true(&rpc, "return _G.got == 'nil'").await);
}

/// A right-click off an open context menu dismisses it and is then an ordinary
/// right-click, so a `MenuPopup` handler opens a fresh menu where it landed.
#[tokio::test]
async fn a_right_click_off_the_menu_reopens_it_where_it_landed() {
    let (rpc, mut inc) = start().await;
    exec_lua(
        &rpc,
        "_G.results = {}
         btv.contextmenu.set(0, {}) -- the default context menu would answer too
         btv.on('MenuPopup', {}, function()
           btv.ui.menu({ { label = 'One' }, { label = 'Two' } }, { anchor = 'mouse' })
             :next(function(e) table.insert(_G.results, e and e.label or 'nil') end)
         end)",
    )
    .await;
    feed_mouse(&rpc, "right", "press", 5, 10);
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("first menu"));
    assert_eq!((num(&menu, "row"), num(&menu, "col")), (5, 10));
    feed_mouse(&rpc, "right", "press", 12, 30);
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("second menu"));
    assert_eq!((num(&menu, "row"), num(&menu, "col")), (12, 30));
    assert!(poll_true(&rpc, "return #_G.results == 1 and _G.results[1] == 'nil'").await);
    // The fresh menu is live: its first row is a click away.
    feed_mouse(&rpc, "left", "press", 13, 32);
    assert!(poll_true(&rpc, "return _G.results[2] == 'One'").await);
}

#[tokio::test]
async fn a_bad_anchor_fails_loud() {
    let (rpc, _inc) = start().await;
    let err = exec_lua(
        &rpc,
        "local ok, e = pcall(btv.ui.menu, { { label = 'x' } }, { anchor = 'nowhere' })
         return tostring(e)",
    )
    .await;
    assert!(err.as_str().unwrap_or("").contains("anchor"), "{err:?}");
}

#[tokio::test]
async fn a_scrolled_menu_keeps_each_detail_and_separator_on_its_own_row() {
    let (rpc, mut inc) = start().await;
    // Fourteen rows under the cursor: more than the ten the box shows, so selecting
    // the last one scrolls the list. Every row's detail and separator flag must stay
    // with its own label.
    let mut entries = String::from("{");
    for i in 1..=14 {
        if i == 7 {
            entries.push_str("{ separator = true },");
        } else {
            entries.push_str(&format!("{{ label = 'row{i}', detail = 'd{i}' }},"));
        }
    }
    entries.push('}');
    open(&rpc, &entries, "{}").await;
    poll_menu(&rpc, &mut inc).await.expect("the menu opens");
    feed(&rpc, "G");
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("still open"));
    let labels = menu_items(&menu);
    let kinds = map_get(&menu, "kinds").expect("details ride the kinds column");
    let kinds: Vec<Option<&str>> = kinds
        .as_array()
        .unwrap()
        .iter()
        .map(Value::as_str)
        .collect();
    let seps: Vec<bool> = map_get(&menu, "separators")
        .expect("a separator row")
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b.as_bool().unwrap())
        .collect();
    assert_eq!(labels.len(), kinds.len());
    for (i, label) in labels.iter().enumerate() {
        if seps[i] {
            assert_eq!((label.as_str(), kinds[i]), ("", None), "row {i}");
        } else {
            let want = label.replace("row", "d");
            assert_eq!(kinds[i], Some(want.as_str()), "row {i} ({label})");
        }
    }
    assert_eq!(seps.iter().filter(|&&s| s).count(), 1);
    assert!(seps[6], "the separator is the seventh row: {seps:?}");
}
