//! Black-box tests for per-mark extmark **gravity** (`nx.buf.set_extmark`'s
//! `right_gravity` / `end_right_gravity`, alias `nvim_buf_set_extmark`).
//!
//! Gravity decides which way each edge of a ranged mark is dragged when text is
//! inserted *at* it. The default (start right-gravity, end left-gravity) is a
//! highlight span that does NOT grow when you type at its edges; the opposite
//! (`right_gravity = false`, `end_right_gravity = true`) makes an even-empty range
//! GROW to swallow text typed at either edge — the anchor shape a live snippet
//! tabstop needs, and the case the previously-fixed gravity could not express.
//!
//! The tests place both flavors as zero-width marks at the same point, insert text
//! there through the normal edit path (feeding keys), then read the marks' ranges
//! back off the refreshed mirror. Flipping the flags flips the outcome — a genuine
//! mutation test, not a tautology.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, feed, start_attached, write_temp};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn open(content: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let path = write_temp("extmark_gravity", "txt", content);
    let init = ServerInit {
        file: Some(path),
        ..Default::default()
    };
    start_attached(init, 80, 24).await
}

/// `"row,col,end_row,end_col"` of mark `id` in the test namespace `_G.__ns`, read off
/// the (server-refreshed) extmark mirror. `"nil"` if the mark is gone.
async fn span(rpc: &Rpc, id: u64) -> String {
    let lua = format!(
        r#"local ms = nx.buf.extmarks(0, _G.__ns, 0, -1, {{ details = true }})
           for _, m in ipairs(ms) do
             if m[1] == {id} then
               return string.format("%d,%d,%d,%d", m[2], m[3], m[4].end_row, m[4].end_col)
             end
           end
           return "nil""#
    );
    match exec_lua(rpc, &lua).await {
        Value::String(s) => s.into_str().unwrap_or_default(),
        other => panic!("expected a string, got {other:?}"),
    }
}

/// Place a growing mark (id 1) and a default mark (id 2) as empty ranges at (0,1).
async fn place_marks(rpc: &Rpc) {
    exec_lua(
        rpc,
        r#"_G.__ns = nx.ns.create("gravtest")
           -- id 1: GROWS — start left-gravity, end right-gravity.
           nx.buf.set_extmark(0, _G.__ns, 0, 1, {
             id = 1, end_row = 0, end_col = 1, hl_group = "Search",
             right_gravity = false, end_right_gravity = true,
           })
           -- id 2: default (highlight span) gravity — does not grow.
           nx.buf.set_extmark(0, _G.__ns, 0, 1, {
             id = 2, end_row = 0, end_col = 1, hl_group = "Search",
           })"#,
    )
    .await;
}

#[tokio::test]
async fn a_growing_range_swallows_text_typed_at_its_edge() {
    let (rpc, _inc) = open("ab\n").await;
    place_marks(&rpc).await;
    // Move onto 'b' (col 1) and insert "XY" before it → "aXYb"; the insert is exactly
    // at byte 1, the marks' shared anchor point.
    feed(&rpc, "liXY");
    feed(&rpc, "<Esc>");
    // The growing mark expanded to cover the inserted "XY": [1, 3).
    assert_eq!(span(&rpc, 1).await, "0,1,0,3");
}

#[tokio::test]
async fn a_default_range_does_not_grow() {
    let (rpc, _inc) = open("ab\n").await;
    place_marks(&rpc).await;
    feed(&rpc, "liXY");
    feed(&rpc, "<Esc>");
    // The default mark stayed empty and was pushed to the right of the insert: [3, 3).
    assert_eq!(span(&rpc, 2).await, "0,3,0,3");
}

#[tokio::test]
async fn gravity_flags_survive_a_details_read_after_the_tick() {
    let (rpc, _inc) = open("ab\n").await;
    place_marks(&rpc).await;
    // Force a server mirror refresh (any round-trip) before reading details, so this
    // reads the round-tripped flags, not the same-chunk write-through.
    feed(&rpc, "<Esc>");
    let flags = exec_lua(
        &rpc,
        r#"local ms = nx.buf.extmarks(0, _G.__ns, 0, -1, { details = true })
           for _, m in ipairs(ms) do
             if m[1] == 1 then
               return tostring(m[4].right_gravity) .. "," .. tostring(m[4].end_right_gravity)
             end
           end
           return "nil""#,
    )
    .await;
    assert_eq!(
        flags,
        Value::String("false,true".into()),
        "the growing mark's non-default gravity round-trips through the mirror"
    );
}
