//! Black-box coverage for decoration providers (`nvim_set_decoration_provider`)
//! and the ephemeral extmarks they place during redraw — the subsystem nvim-cmp's
//! completion menu uses to highlight matched characters.
//!
//! These prove the provider actually *fires* each frame (not just registers): a
//! provider's `on_win` places an ephemeral highlight, and it shows up in the
//! window's projected `highlights` — then, the frame its guard turns it off, it is
//! gone, proving the mark lived for exactly one frame and never persisted.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, drain_to_latest_redraw, exec_lua, spawn, temp_dir, window0_field,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server with `init_lua` sourced and `file` open in the initial buffer.
async fn start(
    dir: &std::path::Path,
    file: &str,
    init_lua: &str,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(dir.join("init.lua"), init_lua).expect("write init.lua");
    let init = ServerInit {
        file: Some(file.to_string()),
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Feed `keys` and return the most recent `redraw` map (the take-latest pattern:
/// a stale frame can sit ahead of this input's under load).
async fn redraw_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> Vec<(Value, Value)> {
    while incoming.try_recv().is_ok() {}
    rpc.request("nvim_input", vec![Value::from(keys)])
        .await
        .expect("input");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    if let Some(map) = drain_to_latest_redraw(incoming, |_| true) {
        return map;
    }
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |_| true) {
            return map;
        }
    }
    panic!("no redraw arrived for {keys:?}");
}

/// The first window's row-0 highlight spans, each `(start_col, end_col, group)`.
fn row0_spans(map: &[(Value, Value)]) -> Vec<(u64, u64, String)> {
    let Some(Value::Array(rows)) = window0_field(map, "highlights") else {
        return Vec::new();
    };
    let Some(Value::Array(row0)) = rows.first() else {
        return Vec::new();
    };
    row0.iter()
        .filter_map(|span| {
            let Value::Array(a) = span else { return None };
            Some((
                a.first()?.as_u64()?,
                a.get(1)?.as_u64()?,
                a.get(2)?.as_str()?.to_string(),
            ))
        })
        .collect()
}

/// A registered provider's `on_win` places an ephemeral highlight on row 0; it
/// appears in that window's projection — and vanishes the frame its guard turns
/// off, proving the decoration fired and is single-frame, not persistent.
#[tokio::test]
async fn decoration_provider_ephemeral_highlight_appears_then_clears() {
    let dir = temp_dir("decoration");
    std::fs::write(dir.join("hello.txt"), "hello world\n").expect("write file");
    let file = dir.join("hello.txt");
    let (rpc, mut incoming) = start(
        file.parent().unwrap(),
        file.to_str().unwrap(),
        r#"
        vim.g.eph_on = true
        local ns = vim.api.nvim_create_namespace('eph_test')
        vim.api.nvim_set_decoration_provider(ns, {
          on_win = function(_, win, buf, top, bot)
            if not vim.g.eph_on then return end
            -- single-line ephemeral highlight over "hello" (cols 0..5) of row 0.
            vim.api.nvim_buf_set_extmark(buf, ns, 0, 0, {
              end_row = 0, end_col = 5, hl_group = 'EphMark', ephemeral = true,
            })
          end,
        })
        "#,
    )
    .await;

    // Frame 1: provider on → the ephemeral span is in the projection.
    let on = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    let spans = row0_spans(&on);
    assert!(
        spans.contains(&(0, 5, "EphMark".to_string())),
        "ephemeral provider highlight missing from row 0: {spans:?}"
    );

    // It is genuinely ephemeral: never stored in the persistent extmark set.
    let stored = exec_lua(
        &rpc,
        "return #vim.api.nvim_buf_get_extmarks(0, vim.api.nvim_create_namespace('eph_test'), 0, -1, {})",
    )
    .await;
    assert_eq!(
        stored.as_u64(),
        Some(0),
        "ephemeral mark must not persist in the store"
    );

    // Frame 2: provider guard off → the span is gone (cleared, not left behind).
    exec_lua(&rpc, "vim.g.eph_on = false").await;
    let off = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    let spans = row0_spans(&off);
    assert!(
        !spans.iter().any(|(_, _, g)| g == "EphMark"),
        "ephemeral highlight should be cleared once the provider stops placing it: {spans:?}"
    );
}

/// An ephemeral extmark used *outside* a decoration callback is rejected loud —
/// it is only meaningful while the server is driving a provider.
#[tokio::test]
async fn ephemeral_extmark_outside_a_provider_fails_loud() {
    let dir = temp_dir("decoration_guard");
    std::fs::write(dir.join("f.txt"), "x\n").expect("write file");
    let (rpc, _incoming) = start(&dir, dir.join("f.txt").to_str().unwrap(), "").await;
    let report = exec_lua(
        &rpc,
        r#"
        local ns = vim.api.nvim_create_namespace('x')
        local ok, err = pcall(vim.api.nvim_buf_set_extmark, 0, ns, 0, 0, {
          end_row = 0, end_col = 1, hl_group = 'X', ephemeral = true,
        })
        if ok then return "no error" else return tostring(err) end
        "#,
    )
    .await;
    let report = report.as_str().unwrap_or("");
    assert!(
        report.contains("ephemeral marks are only valid inside a decoration provider"),
        "expected a loud rejection, got: {report}"
    );
}
