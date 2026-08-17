//! `btv.picker.scorer` — a sandbox re-ranker over a picker's **surviving** rows.
//!
//! The engine still does the matching; the scorer only reorders what matched, and
//! only the top slice of it. Black-box: source an `init.lua`, drive the picker
//! over RPC, assert on the projected `menu` rows.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    attach, exec_lua, feed, menu_items, menu_of, message, poll_menu, redraw_after, spawn, temp_dir,
};
use tokio::sync::mpsc::UnboundedReceiver;

/// Three rows that all match `aa`, plus one that cannot.
const SRC: &str = r#"
btv.picker.source {
  name = "rows",
  items = function(ctx)
    for _, t in ipairs({ "aa1", "aa2", "aa3", "zzz" }) do
      ctx.push { text = t }
    end
  end,
}
"#;

async fn start(dir: &std::path::Path, init_lua: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(dir.join("init.lua"), init_lua).expect("write init.lua");
    let (rpc, incoming) = spawn(ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    });
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Open the picker, type `aa`, and read the resulting row order.
async fn rows_for(dir: &std::path::Path, scorer: &str) -> Vec<String> {
    let init = format!("{SRC}\n{scorer}\n");
    let (rpc, mut incoming) = start(dir, &init).await;
    exec_lua(&rpc, "btv.picker.open('rows')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "aa");
    let map = poll_menu(&rpc, &mut incoming).await.expect("filtered menu");
    menu_items(&menu_of(&map))
}

#[tokio::test]
async fn without_a_scorer_the_rows_keep_native_order() {
    let dir = temp_dir("scorer_none");
    assert_eq!(rows_for(&dir, "").await, vec!["aa1", "aa2", "aa3"]);
}

#[tokio::test]
async fn a_scorer_reorders_the_surviving_rows() {
    let dir = temp_dir("scorer_reorder");
    let rows = rows_for(
        &dir,
        r#"btv.picker.scorer([[ label == "aa3" and 100 or 0 ]])"#,
    )
    .await;
    // aa3 promoted; the rest tie and keep native order (the sort is stable).
    assert_eq!(rows, vec!["aa3", "aa1", "aa2"]);
}

#[tokio::test]
async fn returning_the_native_score_reproduces_native_order() {
    let dir = temp_dir("scorer_identity");
    // Proves `score` really is the fuzzy score the row earned, not a placeholder.
    assert_eq!(
        rows_for(&dir, "btv.picker.scorer([[ score ]])").await,
        vec!["aa1", "aa2", "aa3"]
    );
}

#[tokio::test]
async fn the_scorer_never_sees_a_row_that_did_not_match() {
    let dir = temp_dir("scorer_survivors");
    // `zzz` cannot match `aa`. Even a scorer that would rank it top must not be
    // able to pull it into the view — the scorer re-ranks survivors, it does not
    // re-open matching.
    let rows = rows_for(
        &dir,
        r#"btv.picker.scorer([[ label == "zzz" and 9999 or 0 ]])"#,
    )
    .await;
    assert!(!rows.contains(&"zzz".to_string()), "got {rows:?}");
    assert_eq!(rows.len(), 3);
}

#[tokio::test]
async fn clearing_the_scorer_restores_native_order() {
    let dir = temp_dir("scorer_clear");
    let init = format!(
        "{SRC}\nbtv.picker.scorer([[ label == \"aa3\" and 100 or 0 ]])\nbtv.picker.scorer(nil)\n"
    );
    let (rpc, mut incoming) = start(&dir, &init).await;
    exec_lua(&rpc, "btv.picker.open('rows')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "aa");
    let map = poll_menu(&rpc, &mut incoming).await.expect("filtered menu");
    assert_eq!(menu_items(&menu_of(&map)), vec!["aa1", "aa2", "aa3"]);
}

#[tokio::test]
async fn a_compile_error_is_reported_where_it_is_configured() {
    let dir = temp_dir("scorer_badsyntax");
    let init = format!("{SRC}\nbtv.picker.scorer([[ label == ]])\n");
    let (rpc, mut incoming) = start(&dir, &init).await;
    // The failure is reported at configure time — no picker needs to be opened.
    // A no-op input redraw surfaces whatever `init.lua` left on the message line.
    let msg = message(&redraw_after(&rpc, &mut incoming, "").await);
    assert!(
        msg.contains("btv.picker.scorer") && msg.contains("invalid expression"),
        "expected a configure-time complaint, got {msg:?}"
    );
}

#[tokio::test]
async fn a_failing_scorer_reports_and_leaves_the_picker_usable() {
    let dir = temp_dir("scorer_raise");
    let init = format!("{SRC}\nbtv.picker.scorer([[ error(\"boom\") ]])\n");
    let (rpc, mut incoming) = start(&dir, &init).await;
    exec_lua(&rpc, "btv.picker.open('rows')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "aa");
    let map = poll_menu(&rpc, &mut incoming).await.expect("filtered menu");
    let msg = message(&map);
    assert!(
        msg.contains("btv.picker.scorer"),
        "expected a report, got {msg:?}"
    );
    // Degraded loudly to native order rather than emptying or wedging the picker.
    assert_eq!(menu_items(&menu_of(&map)), vec!["aa1", "aa2", "aa3"]);
}

#[tokio::test]
async fn a_non_number_sort_key_is_refused() {
    let dir = temp_dir("scorer_badret");
    let init = format!("{SRC}\nbtv.picker.scorer([[ \"not a number\" ]])\n");
    let (rpc, mut incoming) = start(&dir, &init).await;
    exec_lua(&rpc, "btv.picker.open('rows')").await;
    poll_menu(&rpc, &mut incoming).await.expect("menu opens");
    feed(&rpc, "aa");
    let map = poll_menu(&rpc, &mut incoming).await.expect("filtered menu");
    let msg = message(&map);
    assert!(
        msg.contains("expected a string or number") || msg.contains("btv.picker.scorer"),
        "a string sort key must be refused, got {msg:?}"
    );
    assert_eq!(menu_items(&menu_of(&map)), vec!["aa1", "aa2", "aa3"]);
}

#[tokio::test]
async fn the_scorer_is_rejected_at_the_lua_boundary_if_it_is_not_source() {
    let dir = temp_dir("scorer_badarg");
    let (rpc, _incoming) = start(&dir, SRC).await;
    let err = exec_lua(
        &rpc,
        "local ok, e = pcall(btv.picker.scorer, function() end) return tostring(e)",
    )
    .await;
    let s = err.as_str().unwrap_or_default();
    assert!(
        s.contains("expected a string of Lua source"),
        "passing a closure must fail loud, got {s:?}"
    );
}
