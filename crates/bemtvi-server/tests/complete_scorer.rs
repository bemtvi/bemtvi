//! `btv.complete.scorer` — the completion popup's re-ranker, the sibling of
//! `btv.picker.scorer`.
//!
//! Black-box: a real server sources an `init.lua` that registers sources and
//! installs a scorer, completion is driven over the same msgpack-RPC a UI uses,
//! and the assertions are on the projected `menu` rows (the order a user reads)
//! and on what a confirm actually accepts.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    attach, drain_to_latest_redraw, exec_lua, feed, lines, map_get, menu_items, menu_of, message,
    poll_menu, spawn, temp_dir,
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

fn menu_selected(menu: &[(Value, Value)]) -> u64 {
    map_get(menu, "selected")
        .and_then(Value::as_u64)
        .expect("menu has a selected index")
}

/// The latest message any frame carries after `keys` — a scorer failure is echoed
/// on the frame it happened, and a later barrier repaint clears the line.
async fn message_from_any_frame(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> String {
    feed(rpc, keys);
    for _ in 0..50 {
        if let Some(m) = drain_to_latest_redraw(incoming, |m| !message(m).is_empty()) {
            return message(&m);
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    String::new()
}

/// Two candidates from one source, so the only thing that can reorder them is the
/// scorer (or the native fuzzy score). Against the prefix `zo` the shorter `zoom`
/// scores better, so `zombie` is the row a scorer has to *promote*.
const TWO_WORDS: &str = "\
btv.complete.source {\n\
  name = 'words', debounce = 0,\n\
  complete = function(ctx) ctx.push('zoom'); ctx.push('zombie') end,\n\
}\n\
btv.complete.setup { sources = { { 'words' } }, min_chars = 2 }\n";

#[tokio::test]
async fn without_a_scorer_the_native_order_stands() {
    let dir = temp_dir("cscore_native");
    let (rpc, mut incoming) = start(&dir, TWO_WORDS).await;
    feed(&rpc, "izo");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(
        menu_items(&menu),
        vec!["zoom", "zombie"],
        "the shorter, better fuzzy match leads natively"
    );
}

#[tokio::test]
async fn a_scorer_reorders_the_popup() {
    let dir = temp_dir("cscore_reorder");
    let init = format!(
        "{TWO_WORDS}btv.complete.scorer([[ score + (label == \"zombie\" and 1000 or 0) ]])\n"
    );
    let (rpc, mut incoming) = start(&dir, &init).await;
    feed(&rpc, "izo");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(
        menu_items(&menu),
        vec!["zombie", "zoom"],
        "the promoted row leads"
    );
}

#[tokio::test]
async fn the_accepted_row_is_the_one_the_scorer_put_on_top() {
    let dir = temp_dir("cscore_accept");
    let init = format!(
        "{TWO_WORDS}btv.complete.scorer([[ score + (label == \"zombie\" and 1000 or 0) ]])\n"
    );
    let (rpc, mut incoming) = start(&dir, &init).await;
    feed(&rpc, "izo");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    // `<C-n>` takes the first row *as displayed*, so a re-rank that only reordered
    // the paint would accept the wrong word here.
    feed(&rpc, "<C-n><C-y>");
    assert_eq!(lines(&rpc).await, vec!["zombie"]);
}

/// Two sources whose only difference is the bias the merge applies (`hi` 50 over
/// `lo` 1) — so the order they come out in *is* the blended key.
const BIASED_SOURCES: &str = "\
btv.complete.source {\n\
  name = 'hi', debounce = 0, priority = 50,\n\
  complete = function(ctx) ctx.push('zoneinfo') end,\n\
}\n\
btv.complete.source {\n\
  name = 'lo', debounce = 0, priority = 1,\n\
  complete = function(ctx) ctx.push('zonelike') end,\n\
}\n\
btv.complete.setup { sources = { { 'hi' }, { 'lo' } }, min_chars = 2 }\n";

#[tokio::test]
async fn the_blended_native_score_is_in_scope() {
    let dir = temp_dir("cscore_blend_native");
    let (rpc, mut incoming) = start(&dir, BIASED_SOURCES).await;
    feed(&rpc, "izo");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(
        menu_items(&menu),
        vec!["zoneinfo", "zonelike"],
        "the higher-biased source leads natively"
    );

    // Negating the key inverts that order — which is only possible if `score`
    // really carries the blended value (the two labels score the same fuzzily; the
    // source bias is the whole difference).
    let dir = temp_dir("cscore_blend_inverted");
    let init = format!("{BIASED_SOURCES}btv.complete.scorer([[ -score ]])\n");
    let (rpc, mut incoming) = start(&dir, &init).await;
    feed(&rpc, "izo");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(
        menu_items(&menu),
        vec!["zonelike", "zoneinfo"],
        "inverting the blended key sinks the biased row"
    );
}

#[tokio::test]
async fn the_query_is_in_scope() {
    // Natively, `zom` ranks the contiguous `zombie` above the gapped `zoom`…
    let dir = temp_dir("cscore_query_native");
    let (rpc, mut incoming) = start(&dir, TWO_WORDS).await;
    feed(&rpc, "izom");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(menu_items(&menu), vec!["zombie", "zoom"]);

    // …so a scorer that promotes `zoom` *only when the query is `zom`* has to see
    // the query grow: the order is native at `zo` and flipped one keystroke later.
    let dir = temp_dir("cscore_query");
    let init = format!(
        "{TWO_WORDS}btv.complete.scorer([[ score + (query == \"zom\" and label == \"zoom\" and 1000 or 0) ]])\n"
    );
    let (rpc, mut incoming) = start(&dir, &init).await;
    feed(&rpc, "izo");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(
        menu_items(&menu),
        vec!["zoom", "zombie"],
        "at `zo` the scorer adds nothing"
    );
    feed(&rpc, "m");
    let menu = menu_of(
        &poll_menu(&rpc, &mut incoming)
            .await
            .expect("popup refreshes"),
    );
    assert_eq!(
        menu_items(&menu),
        vec!["zoom", "zombie"],
        "at `zom` the promotion beats the native order"
    );
}

#[tokio::test]
async fn the_row_kind_is_in_scope() {
    let dir = temp_dir("cscore_kind");
    // A snippet row and a plain word, with the snippet naturally on top (its source
    // bias is higher). The scorer demotes it *by kind*.
    let init = "\
btv.complete.source {\n\
  name = 'snip', debounce = 0, priority = 50,\n\
  complete = function(ctx) ctx.push { text = 'zoneinfo', kind = 'Snippet' } end,\n\
}\n\
btv.complete.source {\n\
  name = 'words', debounce = 0, priority = 1,\n\
  complete = function(ctx) ctx.push('zoom') end,\n\
}\n\
btv.complete.setup { sources = { { 'snip' }, { 'words' } }, min_chars = 2 }\n";
    let (rpc, mut incoming) = start(&dir, init).await;
    feed(&rpc, "izo");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(
        menu_items(&menu),
        vec!["zoneinfo", "zoom"],
        "the snippet source's bias puts it first natively"
    );

    let dir = temp_dir("cscore_kind_demoted");
    let init =
        format!("{init}btv.complete.scorer([[ score - (kind == \"Snippet\" and 500 or 0) ]])\n");
    let (rpc, mut incoming) = start(&dir, &init).await;
    feed(&rpc, "izo");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(
        menu_items(&menu),
        vec!["zoom", "zoneinfo"],
        "the scorer demoted the row by its kind"
    );
}

#[tokio::test]
async fn the_caret_follows_its_row_across_a_rerank() {
    let dir = temp_dir("cscore_selection");
    // The `slow` source's candidate is parked behind a promise the test releases by
    // hand, so the re-rank happens at an exact point: after the navigation, before
    // the accept. The scorer — not a priority — is what sorts it above the selected
    // row, so this measures the re-ranker's own selection handling.
    let init = "\
btv.complete.source {\n\
  name = 'fast', debounce = 0,\n\
  complete = function(ctx) ctx.push('cand_fast') end,\n\
}\n\
btv.complete.source {\n\
  name = 'slow', debounce = 0,\n\
  complete = function(ctx)\n\
    return btv.promise.new(function(resolve)\n\
      _G.release_slow = function() ctx.push('cand_slow'); resolve() end\n\
    end)\n\
  end,\n\
}\n\
btv.complete.setup { sources = { { 'fast' }, { 'slow' } }, min_chars = 2 }\n\
btv.complete.scorer([[ score + (label == 'cand_slow' and 1000 or 0) ]])\n";
    let (rpc, mut incoming) = start(&dir, init).await;

    feed(&rpc, "icand");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(menu_items(&menu), vec!["cand_fast"]);

    // An *active* selection — the state a confirm key acts on.
    feed(&rpc, "<C-n>");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("row selected"));
    assert_eq!(menu_selected(&menu), 0);

    exec_lua(&rpc, "release_slow()").await;
    let menu = menu_of(
        &poll_menu(&rpc, &mut incoming)
            .await
            .expect("popup re-ranks"),
    );
    assert_eq!(
        menu_items(&menu),
        vec!["cand_slow", "cand_fast"],
        "the scorer promoted the late candidate"
    );
    assert_eq!(
        menu_selected(&menu),
        1,
        "the selection followed its row down rather than staying on index 0"
    );
    feed(&rpc, "<C-y>");
    assert_eq!(
        lines(&rpc).await,
        vec!["cand_fast"],
        "the confirm accepted the row the user chose"
    );
}

// ===== failure, loudly =======================================================

#[tokio::test]
async fn a_compile_error_is_reported_when_the_scorer_is_configured() {
    let dir = temp_dir("cscore_compile");
    let init = format!("{TWO_WORDS}btv.complete.scorer([[ 1 + ]])\n");
    let (rpc, mut incoming) = start(&dir, &init).await;
    // Reported at configure time (startup here), before any popup exists.
    let msg = message_from_any_frame(&rpc, &mut incoming, "izo").await;
    assert!(
        msg.contains("btv.complete.scorer") && msg.contains("invalid expression"),
        "a compile error should name itself, got {msg:?}"
    );
}

#[tokio::test]
async fn a_failing_scorer_reports_and_uninstalls_itself() {
    let dir = temp_dir("cscore_runtime");
    let init = format!("{TWO_WORDS}btv.complete.scorer([[ error(\"boom\") ]])\n");
    let (rpc, mut incoming) = start(&dir, &init).await;
    let msg = message_from_any_frame(&rpc, &mut incoming, "izo").await;
    assert!(
        msg.contains("scorer disabled") && msg.contains("boom"),
        "a failing scorer should report once and say it is off, got {msg:?}"
    );
    // Uninstalled — which is what makes "reported once" true: the popup is in
    // native order and there is no scorer left to fail on the next keystroke.
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(menu_items(&menu), vec!["zoom", "zombie"]);
    assert_eq!(
        exec_lua(&rpc, "return 1").await.as_i64(),
        Some(1),
        "the editor is still serving requests after the failure"
    );
}

#[tokio::test]
async fn a_non_number_sort_key_is_reported() {
    let dir = temp_dir("cscore_badreturn");
    let init = format!("{TWO_WORDS}btv.complete.scorer([[ \"top\" ]])\n");
    let (rpc, mut incoming) = start(&dir, &init).await;
    let msg = message_from_any_frame(&rpc, &mut incoming, "izo").await;
    assert!(
        msg.contains("expected a string or number") || msg.contains("scorer disabled"),
        "a string sort key would order rows lexically; it must be refused: {msg:?}"
    );
}

#[tokio::test]
async fn nil_clears_the_scorer() {
    let dir = temp_dir("cscore_clear");
    let init = format!(
        "{TWO_WORDS}btv.complete.scorer([[ score + (label == \"zombie\" and 1000 or 0) ]])\n"
    );
    let (rpc, mut incoming) = start(&dir, &init).await;
    exec_lua(&rpc, "btv.complete.scorer(nil)").await;
    feed(&rpc, "izo");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(
        menu_items(&menu),
        vec!["zoom", "zombie"],
        "clearing the scorer restores native order"
    );
}

#[tokio::test]
async fn a_function_is_refused_because_a_closure_cannot_cross_vms() {
    let dir = temp_dir("cscore_function");
    let (rpc, _incoming) = start(&dir, TWO_WORDS).await;
    let err = exec_lua(
        &rpc,
        "local ok, err = pcall(btv.complete.scorer, function() end) return tostring(err)",
    )
    .await;
    let err = err.as_str().unwrap_or_default();
    assert!(
        err.contains("expected a string of Lua source"),
        "a function must be refused loudly, got {err:?}"
    );
}

#[tokio::test]
async fn the_scorer_cannot_reach_the_editor() {
    let dir = temp_dir("cscore_pure");
    // No `btv` in the sandbox: indexing it raises, which the failure path reports.
    let init = format!("{TWO_WORDS}btv.complete.scorer([[ btv.o.number and 1 or 0 ]])\n");
    let (rpc, mut incoming) = start(&dir, &init).await;
    let msg = message_from_any_frame(&rpc, &mut incoming, "izo").await;
    assert!(
        msg.contains("scorer disabled"),
        "reaching for btv should fail the call, got {msg:?}"
    );
}
