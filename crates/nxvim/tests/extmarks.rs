//! The extmark / decoration layer, end to end through the real stack: a Lua
//! chunk (`nvim_create_namespace` + `nvim_buf_set_extmark`) sets buffer-anchored
//! highlight marks, and we assert they land in the redraw `highlights` payload,
//! track edits, clear by namespace, and round-trip through `nvim_buf_get_extmarks`.
//!
//! Unlike the treesitter highlight tests these need no grammar fixture: extmarks
//! highlight plain text. They still drain to the latest redraw with a bounded
//! poll (the client reader-task race documented in CLAUDE.md).
//!
//! See docs/specs/2026-06-07-extmark-decoration-layer-design.md.

use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{drain_latest_redraw, exec_lua, feed, start_attached, window0};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const COLS: u16 = 80;
const ROWS: u16 = 24;

// ----- harness --------------------------------------------------------------

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(
        ServerInit {
            file: None,
            ..Default::default()
        },
        COLS,
        ROWS - 2,
    )
    .await
}

async fn barrier(rpc: &Rpc) {
    rpc.request(
        "nvim_buf_get_lines",
        vec![
            Value::from(0u64),
            Value::from(0i64),
            Value::from(-1i64),
            Value::Boolean(false),
        ],
    )
    .await
    .expect("barrier");
}

/// The per-row highlight spans `[(start_col, end_col, group)]` from a redraw.
fn highlights_of(params: &[Value]) -> Vec<Vec<(u64, u64, String)>> {
    let Some(rows) = window0(params)
        .and_then(|win| win.iter().find(|(k, _)| k.as_str() == Some("highlights")))
        .and_then(|(_, v)| v.as_array())
    else {
        return Vec::new();
    };
    rows.iter()
        .map(|row| {
            row.as_array()
                .map(|spans| {
                    spans
                        .iter()
                        .filter_map(|s| {
                            let a = s.as_array()?;
                            Some((a[0].as_u64()?, a[1].as_u64()?, a[2].as_str()?.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect()
}

/// Poll (bounded) for a redraw whose row-0 highlights satisfy `done`, returning
/// the parsed highlight rows. Each poll sends a barrier (whose own redraw is
/// state-identical for the persistent `highlights`, so taking the latest is safe).
async fn wait_for_highlights(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    done: impl Fn(&[Vec<(u64, u64, String)>]) -> bool,
) -> Vec<Vec<(u64, u64, String)>> {
    for _ in 0..100 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            let hl = highlights_of(&params);
            if done(&hl) {
                return hl;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("highlights never satisfied the condition within timeout");
}

/// Find row 0's span whose group equals `group`, returning `(start_col, end_col)`.
fn span_with_group(hl: &[Vec<(u64, u64, String)>], group: &str) -> Option<(u64, u64)> {
    hl.first()?
        .iter()
        .find(|(_, _, g)| g == group)
        .map(|(s, e, _)| (*s, *e))
}

// ----- tests ----------------------------------------------------------------

/// The headline: an extmark with `hl_group` over a byte range surfaces as a
/// highlight span in the redraw, in screen columns, carrying its group.
#[tokio::test]
async fn an_extmark_paints_a_highlight_span() {
    let (rpc, mut incoming) = start().await;
    feed(&rpc, "ihello world<Esc>");
    exec_lua(
        &rpc,
        r#"
        local ns = vim.api.nvim_create_namespace('paint')
        vim.api.nvim_buf_set_extmark(0, ns, 0, 0, { end_row = 0, end_col = 5, hl_group = 'Comment' })
        "#,
    )
    .await;

    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        span_with_group(hl, "Comment").is_some()
    })
    .await;
    assert_eq!(
        span_with_group(&hl, "Comment"),
        Some((0, 5)),
        "the extmark highlights `hello` (cols 0..5)"
    );
}

/// A point/range extmark's anchors shift with edits: inserting text *before* the
/// mark slides both ends right by the inserted width (right-gravity start,
/// left-gravity end), exercised through a real normal-mode edit.
#[tokio::test]
async fn an_extmark_shifts_with_edits() {
    let (rpc, mut incoming) = start().await;
    feed(&rpc, "ihello world<Esc>");
    exec_lua(
        &rpc,
        r#"
        local ns = vim.api.nvim_create_namespace('shift')
        vim.api.nvim_buf_set_extmark(0, ns, 0, 0, { end_row = 0, end_col = 5, hl_group = 'Comment' })
        "#,
    )
    .await;
    wait_for_highlights(&rpc, &mut incoming, |hl| {
        span_with_group(hl, "Comment") == Some((0, 5))
    })
    .await;

    // Prepend "AB" at column 0: the span must slide to cols 2..7.
    feed(&rpc, "gg0iAB<Esc>");
    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        span_with_group(hl, "Comment") == Some((2, 7))
    })
    .await;
    assert_eq!(
        span_with_group(&hl, "Comment"),
        Some((2, 7)),
        "inserting 2 chars before the mark slides it right by 2"
    );
}

/// Extmarks survive undo/redo (neovim preserves them — only a destructive
/// reload drops marks). An edit then undo must leave the mark in place, restored
/// to its history-point position; a redo brings the edit (and the shifted mark)
/// back. Regression guard: undo replaces the whole rope via `mark_resync`, which
/// must not be allowed to wipe the marks.
#[tokio::test]
async fn extmarks_survive_undo_and_redo() {
    let (rpc, mut incoming) = start().await;
    feed(&rpc, "ihello world<Esc>");
    exec_lua(
        &rpc,
        r#"
        local ns = vim.api.nvim_create_namespace('undo')
        vim.api.nvim_buf_set_extmark(0, ns, 0, 0, { end_row = 0, end_col = 5, hl_group = 'Comment' })
        "#,
    )
    .await;
    wait_for_highlights(&rpc, &mut incoming, |hl| {
        span_with_group(hl, "Comment") == Some((0, 5))
    })
    .await;

    // Prepend "AB" — the mark slides to 2..7 — then undo: it must return to 0..5,
    // not vanish.
    feed(&rpc, "gg0iAB<Esc>");
    wait_for_highlights(&rpc, &mut incoming, |hl| {
        span_with_group(hl, "Comment") == Some((2, 7))
    })
    .await;
    feed(&rpc, "u");
    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        span_with_group(hl, "Comment") == Some((0, 5))
    })
    .await;
    assert_eq!(
        span_with_group(&hl, "Comment"),
        Some((0, 5)),
        "undo restores the mark to its pre-edit position, not clears it"
    );

    // Redo brings the edit back, and the mark shifts with it again.
    feed(&rpc, "<C-r>");
    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        span_with_group(hl, "Comment") == Some((2, 7))
    })
    .await;
    assert_eq!(
        span_with_group(&hl, "Comment"),
        Some((2, 7)),
        "redo restores the post-edit mark position"
    );
}

/// `nvim_buf_clear_namespace` removes a namespace's marks, so the highlight
/// disappears from subsequent redraws.
#[tokio::test]
async fn clearing_a_namespace_removes_the_highlight() {
    let (rpc, mut incoming) = start().await;
    feed(&rpc, "ihello world<Esc>");
    exec_lua(
        &rpc,
        r#"
        ExtNs = vim.api.nvim_create_namespace('clearme')
        vim.api.nvim_buf_set_extmark(0, ExtNs, 0, 0, { end_row = 0, end_col = 5, hl_group = 'Comment' })
        "#,
    )
    .await;
    wait_for_highlights(&rpc, &mut incoming, |hl| {
        span_with_group(hl, "Comment").is_some()
    })
    .await;

    exec_lua(&rpc, "vim.api.nvim_buf_clear_namespace(0, ExtNs, 0, -1)").await;
    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        span_with_group(hl, "Comment").is_none()
    })
    .await;
    assert!(
        span_with_group(&hl, "Comment").is_none(),
        "after clear_namespace the highlight is gone"
    );
}

/// `nvim_buf_set_extmark` returns a stable id, and `nvim_buf_get_extmarks` reads
/// the mark back across chunks (proving the Rust→Lua mirror, refreshed from the
/// authoritative core store, is wired) — with `details` carrying the attrs.
#[tokio::test]
async fn get_extmarks_round_trips_across_chunks() {
    let (rpc, _rx) = start().await;
    feed(&rpc, "ihello world<Esc>");

    // Set in one chunk; the returned id is stable (1-based, allocated Lua-side).
    let id = exec_lua(
        &rpc,
        r#"
        GetNs = vim.api.nvim_create_namespace('getme')
        return vim.api.nvim_buf_set_extmark(0, GetNs, 0, 6, { end_row = 0, end_col = 11, hl_group = 'Keyword' })
        "#,
    )
    .await;
    assert_eq!(id.as_u64(), Some(1), "first mark id is 1");

    // Read it back in a *separate* chunk: the server rebuilt vim._extmarks from
    // core before this eval, so position + details are present.
    let summary = exec_lua(
        &rpc,
        r#"
        local marks = vim.api.nvim_buf_get_extmarks(0, GetNs, 0, -1, { details = true })
        if #marks ~= 1 then return 'count=' .. #marks end
        local m = marks[1]
        local d = m[4]
        return table.concat({ m[1], m[2], m[3], d.end_col, d.hl_group }, ',')
        "#,
    )
    .await;
    assert_eq!(
        summary.as_str(),
        Some("1,0,6,11,Keyword"),
        "get_extmarks returns the mark's id, row, col, end_col, and hl_group"
    );
}

/// `nvim_buf_del_extmark` reports whether the mark existed and removes it.
#[tokio::test]
async fn del_extmark_reports_existence() {
    let (rpc, _rx) = start().await;
    feed(&rpc, "ihello world<Esc>");
    let result = exec_lua(
        &rpc,
        r#"
        local ns = vim.api.nvim_create_namespace('delme')
        local id = vim.api.nvim_buf_set_extmark(0, ns, 0, 0, { end_row = 0, end_col = 5, hl_group = 'Comment' })
        local first = vim.api.nvim_buf_del_extmark(0, ns, id)
        local second = vim.api.nvim_buf_del_extmark(0, ns, id)
        return tostring(first) .. ',' .. tostring(second)
        "#,
    )
    .await;
    assert_eq!(
        result.as_str(),
        Some("true,false"),
        "deleting an existing mark returns true, deleting it again false"
    );
}

/// A genuinely unknown option fails loud rather than silently doing nothing (the
/// no-silent-stubs rule): a key from neither the rendered set nor the
/// accepted-but-unrendered decoration set errors, naming itself.
#[tokio::test]
async fn unknown_extmark_option_errors() {
    let (rpc, _rx) = start().await;
    let result = exec_lua(
        &rpc,
        r#"
        local ns = vim.api.nvim_create_namespace('bogus')
        local ok, err = pcall(vim.api.nvim_buf_set_extmark, 0, ns, 0, 0, { not_a_real_option = 1 })
        return tostring(ok) .. '|' .. tostring(err)
        "#,
    )
    .await;
    let s = result.as_str().unwrap_or("");
    assert!(
        s.starts_with("false|") && s.contains("not_a_real_option"),
        "an unknown option should raise naming the option, got {s:?}"
    );
}

/// Virtual text (and the rest of the decoration family) is ACCEPTED and STORED —
/// a documented approximation: the mark is created (so the plugin's render path
/// doesn't break) and the payload is returned by a details read, but it isn't
/// painted yet. This is what lets telescope's result counter / preview overlays
/// run instead of erroring.
#[tokio::test]
async fn virtual_text_is_accepted_and_stored() {
    let (rpc, _rx) = start().await;
    let result = exec_lua(
        &rpc,
        r#"
        local ns = vim.api.nvim_create_namespace('virt')
        local ok = pcall(vim.api.nvim_buf_set_extmark, 0, ns, 0, 0, {
          virt_text = {{'x', 'Comment'}}, virt_text_pos = 'right_align',
        })
        local marks = vim.api.nvim_buf_get_extmarks(0, ns, 0, -1, { details = true })
        local d = marks[1] and marks[1][4]
        local has_vt = d ~= nil and d.virt_text ~= nil
        return tostring(ok) .. '|' .. tostring(#marks) .. '|' .. tostring(has_vt)
        "#,
    )
    .await;
    let s = result.as_str().unwrap_or("");
    assert_eq!(
        s, "true|1|true",
        "virt_text should create a mark and be retrievable via details, got {s:?}"
    );
}

/// The shipped `examples/extmarks/` config works end to end through the real
/// stack: sourcing its `init.lua` over the sample buffer paints the startup marks
/// (proving the documented API surface stays correct), and its `:ExtClear`
/// command wipes the namespace. Guards the example against bitrot.
#[tokio::test]
async fn the_extmarks_example_config_paints_and_clears() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/extmarks");
    let init = std::fs::read_to_string(format!("{root}/init.lua")).unwrap();
    let sample = std::fs::read_to_string(format!("{root}/sample.txt")).unwrap();
    let (rpc, mut incoming) = start().await;

    // Load the sample text into the buffer (via the Lua API), then source config.
    let set = sample
        .lines()
        .map(|l| format!("{l:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    exec_lua(
        &rpc,
        &format!("vim.api.nvim_buf_set_lines(0, 0, -1, false, {{ {set} }})"),
    )
    .await;
    exec_lua(&rpc, &init).await;

    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.get(1)
            .is_some_and(|r| r.iter().any(|(_, _, g)| g == "ExtNote"))
            && hl
                .get(2)
                .is_some_and(|r| r.iter().any(|(_, _, g)| g == "ExtTodo"))
    })
    .await;
    assert!(
        hl[1].iter().any(|(_, _, g)| g == "ExtNote"),
        "line 1 carries ExtNote marks"
    );
    assert!(
        hl[2].iter().any(|(_, _, g)| g == "ExtTodo"),
        "the TODO: tag is marked"
    );
    assert!(
        hl[3].iter().any(|(_, _, g)| g == "ExtWarn"),
        "the NOTE: tag is marked"
    );

    exec_lua(&rpc, "vim.cmd('ExtClear')").await;
    let cleared = wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.iter()
            .all(|r| r.iter().all(|(_, _, g)| !g.starts_with("Ext")))
    })
    .await;
    assert!(
        cleared
            .iter()
            .all(|r| r.iter().all(|(_, _, g)| !g.starts_with("Ext"))),
        ":ExtClear wipes the namespace"
    );
}
