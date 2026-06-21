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

/// Read a `window0` field by key.
fn win_field<'a>(params: &'a [Value], key: &str) -> Option<&'a Value> {
    window0(params)
        .and_then(|win| win.iter().find(|(k, _)| k.as_str() == Some(key)))
        .map(|(_, v)| v)
}

/// The focused window's gutter sign on each row as `Some(glyph)` / `None`, from the
/// `diagnostics_signs` redraw array (`[glyph, code, style_id]` per row).
fn signs_of(params: &[Value]) -> Vec<Option<String>> {
    win_field(params, "diagnostics_signs")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|r| {
                    r.as_array()
                        .and_then(|c| c.first())
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The focused window's reserved sign-column width in cells.
fn sign_width_of(params: &[Value]) -> u64 {
    win_field(params, "sign_width")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Poll (bounded) for a redraw whose focused-window signs satisfy `done`.
async fn wait_for_signs(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    done: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    for _ in 0..100 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            if done(&params) {
                return params;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("signs never satisfied the condition within timeout");
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

    // Read it back in a *separate* chunk: the server rebuilt nx._extmarks from
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

/// A `virt_text` extmark is accepted, stored, and round-trips: the mark is
/// created and its payload comes back from a details read. This guards the
/// store + `details` mirror in isolation; the redraw-projection / painting of
/// each `virt_text_pos` (eol, inline, overlay, right_align, win_col) is covered
/// by the dedicated tests below.
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

/// Row 0's `virt_text` placements from a redraw, each as `(pos, col, [chunk text,
/// …])`. The wire shape is `[pos, col, hl_mode, [[text, style_id], …]]` per
/// placement; `pos` is `0`=eol / `1`=inline, `col` the screen column (eol → `0`).
fn virt_text_row0(params: &[Value]) -> Vec<(u64, u64, Vec<String>)> {
    let Some(rows) = window0(params)
        .and_then(|win| win.iter().find(|(k, _)| k.as_str() == Some("virt_text")))
        .and_then(|(_, v)| v.as_array())
    else {
        return Vec::new();
    };
    let Some(row0) = rows.first().and_then(Value::as_array) else {
        return Vec::new();
    };
    row0.iter()
        .filter_map(|p| {
            let a = p.as_array()?;
            let pos = a[0].as_u64()?;
            let col = a[1].as_u64()?;
            let chunks = a[3]
                .as_array()?
                .iter()
                .filter_map(|c| c.as_array()?[0].as_str().map(str::to_string))
                .collect();
            Some((pos, col, chunks))
        })
        .collect()
}

/// Poll (bounded) for a redraw whose row-0 `virt_text` satisfies `done`.
async fn wait_for_virt_text(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    done: impl Fn(&[(u64, u64, Vec<String>)]) -> bool,
) -> Vec<(u64, u64, Vec<String>)> {
    for _ in 0..100 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            let vt = virt_text_row0(&params);
            if done(&vt) {
                return vt;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("virt_text never satisfied the condition within timeout");
}

/// The headline for Phase 2: an extmark with `virt_text` at the default `eol`
/// position surfaces in the redraw `virt_text` payload as an end-of-line placement
/// (`pos == 0`) carrying its chunk text — so the client paints it after the line.
#[tokio::test]
async fn eol_virt_text_paints_after_the_line() {
    let (rpc, mut incoming) = start().await;
    feed(&rpc, "ihello<Esc>");
    exec_lua(
        &rpc,
        r#"
        local ns = vim.api.nvim_create_namespace('eolvt')
        vim.api.nvim_buf_set_extmark(0, ns, 0, 0, {
          virt_text = {{' <-- mark', 'Comment'}}, virt_text_pos = 'eol',
        })
        "#,
    )
    .await;

    let vt = wait_for_virt_text(&rpc, &mut incoming, |vt| {
        vt.iter()
            .any(|(pos, _, chunks)| *pos == 0 && chunks.iter().any(|c| c.contains("<-- mark")))
    })
    .await;
    let eol = vt
        .iter()
        .find(|(pos, _, _)| *pos == 0)
        .expect("an eol placement");
    assert_eq!(
        eol.2,
        vec![" <-- mark".to_string()],
        "the eol virt_text chunk text reaches the client verbatim"
    );
}

/// Phase 3: an extmark with `virt_text_pos = 'inline'` surfaces as an inline
/// placement (`pos == 1`) whose `col` is the screen column of its byte anchor —
/// the client splices the chunk into the row there. The anchor sits after a tab,
/// so the column is the *display* column (not the byte offset), exercising the
/// `virtcol` mapping the server shares with inlay hints / highlights.
#[tokio::test]
async fn inline_virt_text_anchors_at_its_screen_column() {
    let (rpc, mut incoming) = start().await;
    // `\tword` — one leading tab (the default tabstop, 4 display cells) then
    // "word"; the mark at byte col 1 (just after the tab) is at screen column 4,
    // so the screen column is the *display* column, not the byte offset.
    feed(&rpc, "i<Tab>word<Esc>");
    exec_lua(
        &rpc,
        r#"
        local ns = vim.api.nvim_create_namespace('inlinevt')
        vim.api.nvim_buf_set_extmark(0, ns, 0, 1, {
          virt_text = {{'HINT', 'Comment'}}, virt_text_pos = 'inline',
        })
        "#,
    )
    .await;

    let vt = wait_for_virt_text(&rpc, &mut incoming, |vt| {
        vt.iter()
            .any(|(pos, _, chunks)| *pos == 1 && chunks.iter().any(|c| c == "HINT"))
    })
    .await;
    let inline = vt
        .iter()
        .find(|(pos, _, _)| *pos == 1)
        .expect("an inline placement");
    assert_eq!(
        (inline.1, &inline.2),
        (4, &vec!["HINT".to_string()]),
        "inline virt_text anchors at screen column 4 (past the leading tab)"
    );
}

/// Phase 4: the `overlay`, `right_align`, and `win_col` positions each project with
/// their distinct `pos` tag and column — overlay at its anchor's screen column,
/// right_align with `col == 0` (the client flushes it to the right edge), and
/// win_col at its fixed window column independent of the mark anchor.
#[tokio::test]
async fn overlay_rightalign_wincol_positions_project() {
    let (rpc, mut incoming) = start().await;
    feed(&rpc, "ihello world<Esc>");
    exec_lua(
        &rpc,
        r#"
        local ns = vim.api.nvim_create_namespace('pos')
        vim.api.nvim_buf_set_extmark(0, ns, 0, 2, {
          virt_text = {{'OV', 'Comment'}}, virt_text_pos = 'overlay',
        })
        vim.api.nvim_buf_set_extmark(0, ns, 0, 0, {
          virt_text = {{'RA', 'Comment'}}, virt_text_pos = 'right_align',
        })
        vim.api.nvim_buf_set_extmark(0, ns, 0, 0, {
          virt_text = {{'WC', 'Comment'}}, virt_text_win_col = 40,
        })
        "#,
    )
    .await;

    let vt = wait_for_virt_text(&rpc, &mut incoming, |vt| {
        [2, 3, 4]
            .iter()
            .all(|want| vt.iter().any(|(pos, _, _)| pos == want))
    })
    .await;
    let find = |pos: u64| {
        vt.iter()
            .find(|(p, _, _)| *p == pos)
            .map(|(_, c, t)| (*c, t.clone()))
    };
    assert_eq!(
        find(2),
        Some((2, vec!["OV".to_string()])),
        "overlay projects at its anchor's screen column (2)"
    );
    assert_eq!(
        find(3),
        Some((0, vec!["RA".to_string()])),
        "right_align projects with col 0 (the client positions it at the right edge)"
    );
    assert_eq!(
        find(4),
        Some((40, vec!["WC".to_string()])),
        "win_col projects at its fixed window column (40)"
    );
}

// ----- Phase 5: virt_lines (whole virtual rows) -----------------------------

/// The focused window's interleaved row layout from a redraw: per visible screen
/// row, `(number, line_text, virt_lines)`. `number` is the 1-based buffer line
/// (`None` for a `~` filler *or* a virtual row), `line_text` the rendered text, and
/// `virt_lines` the chunk texts when the row is a virtual line (`None` otherwise) —
/// the field that tells a `None`-number row apart from a `~` filler.
#[allow(clippy::type_complexity)]
fn row_layout(params: &[Value]) -> Vec<(Option<u64>, String, Option<Vec<String>>)> {
    let Some(win) = window0(params) else {
        return Vec::new();
    };
    let get = |key: &str| {
        win.iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v)
    };
    let lines: Vec<String> = get("lines")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default();
    let numbers: Vec<Option<u64>> = get("numbers")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(Value::as_u64).collect())
        .unwrap_or_default();
    let virt: Vec<Option<Vec<String>>> = get("virt_lines")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|row| {
                    row.as_array().map(|chunks| {
                        chunks
                            .iter()
                            .filter_map(|c| c.as_array()?[0].as_str().map(str::to_string))
                            .collect()
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    (0..lines.len())
        .map(|i| {
            (
                numbers.get(i).copied().flatten(),
                lines.get(i).cloned().unwrap_or_default(),
                virt.get(i).cloned().flatten(),
            )
        })
        .collect()
}

/// Poll (bounded) for a redraw whose row layout satisfies `done`.
#[allow(clippy::type_complexity)]
async fn wait_for_layout(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    done: impl Fn(&[(Option<u64>, String, Option<Vec<String>>)]) -> bool,
) -> Vec<(Option<u64>, String, Option<Vec<String>>)> {
    for _ in 0..100 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            let layout = row_layout(&params);
            if done(&layout) {
                return layout;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("row layout never satisfied the condition within timeout");
}

/// Phase 5: `virt_lines` expand a buffer line into extra screen rows — one drawn
/// *above* the line, one *below* it — interleaved into the window's rows. The
/// virtual rows carry no buffer line number (like a `~` filler) but do carry their
/// chunk text in the `virt_lines` payload, and the real lines keep their order and
/// numbers around the inserted rows.
#[tokio::test]
async fn virt_lines_interleave_above_and_below_their_line() {
    let (rpc, mut incoming) = start().await;
    // Build the buffer through keystrokes — the `nvim_buf_set_*` mutation API is
    // intentionally absent in nxvim (extmark reads/creates exist, mutation doesn't).
    feed(&rpc, "iAAA<CR>BBB<CR>CCC<Esc>");
    exec_lua(
        &rpc,
        r#"
        local ns = vim.api.nvim_create_namespace('vlines')
        -- A virtual line drawn ABOVE the second buffer line ('BBB', 0-based row 1).
        vim.api.nvim_buf_set_extmark(0, ns, 1, 0, {
          virt_lines = {{{'== above BBB ==', 'Comment'}}}, virt_lines_above = true,
        })
        -- A virtual line drawn BELOW the second buffer line.
        vim.api.nvim_buf_set_extmark(0, ns, 1, 0, {
          virt_lines = {{{'== below BBB =='}}},
        })
        "#,
    )
    .await;

    let layout = wait_for_layout(&rpc, &mut incoming, |rows| {
        let has = |needle: &str| {
            rows.iter().any(|(_, _, v)| {
                v.as_ref()
                    .is_some_and(|c| c.iter().any(|t| t.contains(needle)))
            })
        };
        has("above BBB") && has("below BBB")
    })
    .await;

    let bbb = layout
        .iter()
        .position(|(n, l, _)| *n == Some(2) && l == "BBB")
        .expect("the BBB text row (buffer line 2)");
    // Directly above BBB: the 'above' virtual row (no number, its chunk text).
    assert_eq!(
        (layout[bbb - 1].0, &layout[bbb - 1].2),
        (None, &Some(vec!["== above BBB ==".to_string()])),
        "the above virtual line sits directly above its buffer line, with no number"
    );
    // Directly below BBB: the 'below' virtual row.
    assert_eq!(
        (layout[bbb + 1].0, &layout[bbb + 1].2),
        (None, &Some(vec!["== below BBB ==".to_string()])),
        "the below virtual line sits directly below its buffer line"
    );
    // The real lines keep their order and numbers around the inserted virtual rows.
    assert_eq!(
        layout[bbb - 2].0,
        Some(1),
        "AAA stays line 1, above the virtual row"
    );
    assert_eq!(
        layout[bbb + 2].0,
        Some(3),
        "CCC stays line 3, below the virtual row"
    );
}

/// Phase 5a: the scroll math counts a line's `virt_lines` as extra screen rows, so
/// the cursor stays visible past them. With the buffer filled to exactly the text
/// height (so it would all fit at the top with no scroll) and 3 virtual lines
/// attached below the first line, jumping to the last line must scroll the viewport
/// — pushing line 1 (and its virtual rows) off the top. Without plines-aware
/// scrolling the editor would think the `h` lines fit and leave line 1 visible.
#[tokio::test]
async fn scroll_accounts_for_virt_lines_to_keep_the_cursor_visible() {
    let (rpc, mut incoming) = start().await;
    // The window's text-body height (one row per buffer line, no virtual lines yet).
    let h = wait_for_layout(&rpc, &mut incoming, |rows| !rows.is_empty())
        .await
        .len();
    assert!(h >= 4, "need a few rows of text body to exercise scrolling");

    // Fill the buffer to exactly `h` lines through keystrokes (no `nvim_buf_set_lines`
    // in nxvim) — so with no virtual lines the whole buffer fits at top=0.
    let body = (1..=h)
        .map(|i| format!("L{i:02}"))
        .collect::<Vec<_>>()
        .join("<CR>");
    feed(&rpc, &format!("i{body}<Esc>"));
    exec_lua(
        &rpc,
        r#"
        local ns = vim.api.nvim_create_namespace('vlscroll')
        -- Three virtual lines below the FIRST buffer line: the buffer + its virtual
        -- rows are now `h + 3` screen rows, taller than the window.
        vim.api.nvim_buf_set_extmark(0, ns, 0, 0, {
          virt_lines = {{{'~~ v1 ~~'}}, {{'~~ v2 ~~'}}, {{'~~ v3 ~~'}}},
        })
        "#,
    )
    .await;
    // Top of the buffer, then jump to the last line: revealing it must scroll past
    // line 1 and its virtual rows.
    feed(&rpc, "ggG");

    let layout = wait_for_layout(&rpc, &mut incoming, |rows| {
        let visible = |n: u64| rows.iter().any(|(num, _, _)| *num == Some(n));
        // The cursor's last line stays visible AND line 1 scrolled off the top.
        visible(h as u64) && !visible(1)
    })
    .await;
    assert!(
        layout.iter().any(|(n, _, _)| *n == Some(h as u64)),
        "the cursor's last line stays visible after jumping to it"
    );
    assert!(
        !layout.iter().any(|(n, _, _)| *n == Some(1)),
        "line 1 scrolled off the top — the scroll accounted for the virtual rows below it"
    );
}

// ----- Phase 6: priority ordering + virt_text_hide --------------------------

/// Every chunk text across row 0's `virt_text` placements, in wire (draw) order.
fn vt_texts(vt: &[(u64, u64, Vec<String>)]) -> Vec<String> {
    vt.iter().flat_map(|(_, _, c)| c.clone()).collect()
}

/// Phase 6: two `virt_text` marks anchored at the same column emit in **priority**
/// order — the `(start, priority, id)` sort makes priority the tie-break at a shared
/// anchor, so a higher-priority mark stacks after a lower one regardless of creation
/// order / id. The high-priority mark is created first (smaller id), so only priority
/// (not id) can yield the asserted order.
#[tokio::test]
async fn virt_text_marks_emit_in_priority_order() {
    let (rpc, mut incoming) = start().await;
    feed(&rpc, "ihello<Esc>");
    exec_lua(
        &rpc,
        r#"
        local ns = vim.api.nvim_create_namespace('prio')
        vim.api.nvim_buf_set_extmark(0, ns, 0, 0, {
          virt_text = {{'HIGH', 'Comment'}}, virt_text_pos = 'eol', priority = 200,
        })
        vim.api.nvim_buf_set_extmark(0, ns, 0, 0, {
          virt_text = {{'LOW', 'Comment'}}, virt_text_pos = 'eol', priority = 10,
        })
        "#,
    )
    .await;

    let vt = wait_for_virt_text(&rpc, &mut incoming, |vt| {
        vt.iter().filter(|(pos, _, _)| *pos == 0).count() == 2
    })
    .await;
    assert_eq!(
        vt_texts(&vt),
        vec!["LOW".to_string(), "HIGH".to_string()],
        "the lower-priority mark is emitted first; priority — not id / creation order — drives it"
    );
}

/// Phase 6: `virt_text_hide` hides a mark's virtual text while the line's background
/// text is covered by the visual selection, and shows it again once the selection is
/// gone; a sibling mark without the flag stays visible throughout.
#[tokio::test]
async fn virt_text_hide_drops_under_a_visual_selection() {
    let (rpc, mut incoming) = start().await;
    feed(&rpc, "ihello<Esc>");
    exec_lua(
        &rpc,
        r#"
        local ns = vim.api.nvim_create_namespace('hide')
        vim.api.nvim_buf_set_extmark(0, ns, 0, 0, {
          virt_text = {{'HIDES', 'Comment'}}, virt_text_pos = 'eol', virt_text_hide = true,
        })
        vim.api.nvim_buf_set_extmark(0, ns, 0, 0, {
          virt_text = {{'STAYS', 'Comment'}}, virt_text_pos = 'eol',
        })
        "#,
    )
    .await;

    // Normal mode (line not selected): both placements show.
    let vt = wait_for_virt_text(&rpc, &mut incoming, |vt| {
        let t = vt_texts(vt);
        t.contains(&"HIDES".to_string()) && t.contains(&"STAYS".to_string())
    })
    .await;
    assert!(
        vt_texts(&vt).contains(&"HIDES".to_string()),
        "the hide mark shows in normal mode"
    );

    // Select the line (visual-line mode): the hide mark drops, the plain one stays.
    feed(&rpc, "V");
    let vt = wait_for_virt_text(&rpc, &mut incoming, |vt| {
        let t = vt_texts(vt);
        !t.contains(&"HIDES".to_string()) && t.contains(&"STAYS".to_string())
    })
    .await;
    assert!(
        !vt_texts(&vt).contains(&"HIDES".to_string()),
        "virt_text_hide drops the text while the line is selected"
    );
    assert!(
        vt_texts(&vt).contains(&"STAYS".to_string()),
        "a mark without virt_text_hide stays visible under the selection"
    );

    // Leave visual mode: the hidden text comes back.
    feed(&rpc, "<Esc>");
    wait_for_virt_text(&rpc, &mut incoming, |vt| {
        vt_texts(vt).contains(&"HIDES".to_string())
    })
    .await;
}

/// A `sign_text` extmark paints a gutter sign: the glyph shows on the mark's line
/// in the `diagnostics_signs` column and the window reserves a 2-cell sign column.
/// (`signcolumn` defaults to `auto`, so the column appears only because a sign does.)
#[tokio::test]
async fn a_sign_text_extmark_paints_a_gutter_sign() {
    let (rpc, mut incoming) = start().await;
    feed(&rpc, "iline one<CR>line two<CR>line three<Esc>");
    exec_lua(
        &rpc,
        r#"
        local ns = vim.api.nvim_create_namespace('signs')
        -- A gutter sign on line 2 (0-based row 1), styled with a group.
        vim.api.nvim_buf_set_extmark(0, ns, 1, 0, { sign_text = '>>', sign_hl_group = 'WarningMsg' })
        "#,
    )
    .await;

    let params = wait_for_signs(&rpc, &mut incoming, |p| {
        signs_of(p).get(1) == Some(&Some(">>".to_string()))
    })
    .await;

    let signs = signs_of(&params);
    assert_eq!(
        signs.get(1),
        Some(&Some(">>".to_string())),
        "the sign glyph shows on the mark's line (row 1)"
    );
    assert_eq!(signs.first(), Some(&None), "no sign on row 0");
    assert_eq!(
        sign_width_of(&params),
        2,
        "an extmark sign reserves a 2-cell sign column under signcolumn=auto"
    );
}

/// A higher-priority `sign_text` mark wins the single sign cell on a shared line.
#[tokio::test]
async fn the_highest_priority_sign_wins_the_cell() {
    let (rpc, mut incoming) = start().await;
    feed(&rpc, "ihello<Esc>");
    exec_lua(
        &rpc,
        r#"
        local ns = vim.api.nvim_create_namespace('prio')
        vim.api.nvim_buf_set_extmark(0, ns, 0, 0, { sign_text = 'LO', priority = 5 })
        vim.api.nvim_buf_set_extmark(0, ns, 0, 0, { sign_text = 'HI', priority = 50 })
        "#,
    )
    .await;
    let params = wait_for_signs(&rpc, &mut incoming, |p| {
        signs_of(p).first() == Some(&Some("HI".to_string()))
    })
    .await;
    assert_eq!(
        signs_of(&params).first(),
        Some(&Some("HI".to_string())),
        "the priority-50 sign wins over the priority-5 sign"
    );
}

/// A `sign_text` mark round-trips through `get_extmarks(details=true)` AFTER the
/// tick (the server refreshes the Lua mirror), so a plugin can read its sign back.
#[tokio::test]
async fn sign_text_round_trips_across_chunks() {
    let (rpc, _rx) = start().await;
    feed(&rpc, "ihello<Esc>");
    exec_lua(
        &rpc,
        r#"
        SignNs = vim.api.nvim_create_namespace('signrt')
        vim.api.nvim_buf_set_extmark(0, SignNs, 0, 0, { sign_text = '!!', sign_hl_group = 'ErrorMsg' })
        "#,
    )
    .await;
    // Separate chunk: the mirror was rebuilt from core, so the sign survives.
    let summary = exec_lua(
        &rpc,
        r#"
        local marks = vim.api.nvim_buf_get_extmarks(0, SignNs, 0, -1, { details = true })
        if #marks ~= 1 then return 'count=' .. #marks end
        local d = marks[1][4]
        return tostring(d.sign_text) .. ',' .. tostring(d.sign_hl_group)
        "#,
    )
    .await;
    assert_eq!(
        summary.as_str(),
        Some("!!,ErrorMsg"),
        "get_extmarks details returns the sign_text/sign_hl_group after the tick"
    );
}

/// An nx-native `line_fill` mark fills the anchored row with a repeated glyph,
/// surfacing as a full-width `Overlay` (`pos=2`) virt_text placement at column 0 —
/// e.g. a `-` rule on a blank alignment / filler row.
#[tokio::test]
async fn a_line_fill_mark_fills_the_row_with_a_glyph() {
    let (rpc, mut incoming) = start().await;
    // The empty buffer's row 0 is a blank line — fill it.
    exec_lua(
        &rpc,
        r#"
        local ns = vim.api.nvim_create_namespace('fill')
        vim.api.nvim_buf_set_extmark(0, ns, 0, 0, { line_fill = { text = '-', hl_group = 'NonText' } })
        "#,
    )
    .await;
    let vt = wait_for_virt_text(&rpc, &mut incoming, |vt| {
        vt.iter().any(|(pos, col, chunks)| {
            *pos == 2 && *col == 0 && chunks.iter().any(|c| c.starts_with("---"))
        })
    })
    .await;
    let fill = vt
        .iter()
        .find(|(pos, _, _)| *pos == 2)
        .expect("a line_fill Overlay placement");
    assert_eq!(fill.1, 0, "the fill starts at column 0");
    let text = fill.2.first().expect("the fill chunk text");
    assert!(
        text.len() >= 40 && text.chars().all(|c| c == '-'),
        "the row is filled across its width with the glyph, got {text:?}"
    );
}

/// A `line_fill` mark round-trips through `get_extmarks(details=true)` after the
/// tick, so a plugin can read back the fill it placed (symmetry with `sign_text`).
#[tokio::test]
async fn line_fill_round_trips_across_chunks() {
    let (rpc, _rx) = start().await;
    exec_lua(
        &rpc,
        r#"
        FillNs = vim.api.nvim_create_namespace('fillrt')
        vim.api.nvim_buf_set_extmark(0, FillNs, 0, 0, { line_fill = { text = '.', hl_group = 'NonText' } })
        "#,
    )
    .await;
    let summary = exec_lua(
        &rpc,
        r#"
        local marks = vim.api.nvim_buf_get_extmarks(0, FillNs, 0, -1, { details = true })
        if #marks ~= 1 then return 'count=' .. #marks end
        local lf = marks[1][4].line_fill
        if not lf then return 'no line_fill' end
        return tostring(lf.text) .. ',' .. tostring(lf.hl_group)
        "#,
    )
    .await;
    assert_eq!(
        summary.as_str(),
        Some(".,NonText"),
        "get_extmarks details returns the line_fill text/hl_group after the tick"
    );
}
