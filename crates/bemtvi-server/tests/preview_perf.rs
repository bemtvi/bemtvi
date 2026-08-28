//! Perf guard for the **picker preview pane's** tree-sitter highlighting.
//!
//! Moving the selection re-targets the preview at another file. Highlighting that
//! file is a whole-file parse plus a query, and it used to run *inline on the frame
//! the selection moved* — over the **entire file**, however tall the pane. Holding
//! `<C-n>` through a result list therefore stalled the editor for hundreds of
//! milliseconds per row on any sizeable source file: the preview's cost scaled with
//! the file rather than with the pane, and it landed on the keystroke path.
//!
//! Two properties are pinned here, and they only mean something together:
//!
//! 1. **Fast navigation is free.** Moving row to row without pausing costs no more
//!    than moving through files with no grammar at all — the highlight is debounced,
//!    so a selection that is already gone by the next frame is never highlighted.
//! 2. **A settled selection still colours.** Pause on a row and its spans arrive.
//!    Without this, "fast" would be trivially satisfiable by not highlighting.
//!
//! `#[ignore]`d, not hermetic: it needs a real grammar (the same opt-in posture as
//! the other tree-sitter e2e tests), which it takes from the data dir rather than
//! installing — skipping cleanly when there is none. Run with:
//!
//! ```sh
//! cargo test --workspace --test preview_perf -- --ignored --nocapture
//! ```

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    attach, exec_lua, feed, map_get, menu_of, poll_menu, serial_lock, spawn, temp_dir,
};
use rmpv::Value;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;

/// How many files the picker lists, and how tall each one is. Tall enough that a
/// whole-file parse + query is unmistakably slower than a pane-sized one.
const FILES: usize = 8;
const LINES: usize = 4_000;

/// Lua source with enough structure (functions, tables, strings, comments) to give
/// the highlighter real work on every line.
fn lua_source(nlines: usize) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while out.len() < nlines {
        out.extend([
            format!("-- widget {i}"),
            format!("local function process_{i}(items, opts)"),
            format!("  local total = {i}"),
            "  for _, x in ipairs(items) do".to_string(),
            format!("    if x > {i} and x < {} then", i * 2),
            "      total = total + x * 2".to_string(),
            "    else".to_string(),
            format!("      total = total - #tostring('w{i}')"),
            "    end".to_string(),
            "  end".to_string(),
            "  return { total = total, name = opts.name }".to_string(),
            "end".to_string(),
            String::new(),
        ]);
        i += 1;
    }
    out.truncate(nlines);
    out.join("\n") + "\n"
}

async fn start(dir: &std::path::Path, init_lua: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(dir.join("init.lua"), init_lua).expect("write init.lua");
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 120, 40).await;
    (rpc, incoming)
}

/// The preview pane's total tree-sitter span count in a menu redraw.
fn preview_spans(menu: &[(Value, Value)]) -> usize {
    let Some(Value::Map(preview)) = map_get(menu, "preview") else {
        return 0;
    };
    let Some(Value::Array(rows)) = map_get(&preview[..], "highlights") else {
        return 0;
    };
    rows.iter().filter_map(Value::as_array).map(Vec::len).sum()
}

/// Write `FILES` copies of `body` under `dir` with extension `ext`, and register a
/// file picker over them. Returns the source's `init.lua`.
fn picker_over(dir: &std::path::Path, ext: &str, body: &str) -> String {
    let mut pushes = String::new();
    for k in 0..FILES {
        let path = dir.join(format!("sample{k}.{ext}"));
        std::fs::write(&path, body).expect("write sample");
        pushes.push_str(&format!(
            "  ctx.push {{ text = 'sample{k}', path = [[{}]] }}\n",
            path.display()
        ));
    }
    format!(
        "btv.picker.source {{\n  name = 'perf_files',\n  preview = 'file',\n\
         \x20 items = function(ctx)\n{pushes}  end,\n  confirm = function() end,\n}}\n"
    )
}

/// The worst single-move latency over `FILES - 1` `<C-n>` presses, each fed as soon
/// as the previous frame landed — the "holding the key down" case.
async fn worst_move(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> Duration {
    let mut worst = Duration::ZERO;
    for _ in 0..FILES - 1 {
        let t0 = Instant::now();
        feed(rpc, "<C-n>");
        poll_menu(rpc, incoming).await.expect("the menu repaints");
        worst = worst.max(t0.elapsed());
    }
    worst
}

#[tokio::test]
#[ignore = "needs an installed grammar; opt-in like the other treesitter e2e tests"]
async fn navigating_the_picker_never_pays_the_preview_parse() {
    // The server resolves grammars from `BEMTVI_DATA_DIR` (process-global), so
    // serialize against other tests that touch process-wide state.
    let _guard = serial_lock().lock().await;

    if !bemtvi_ts::installed_parsers()
        .iter()
        .any(|p| p.lang == "lua")
    {
        eprintln!(
            "skip: lua grammar not installed under {} (set BEMTVI_DATA_DIR or :TSInstall lua)",
            bemtvi_ts::data_dir().display()
        );
        return;
    }

    let body = lua_source(LINES);

    // Baseline: the same content with an extension no grammar claims, so the frame
    // does everything *except* highlight — the read, the window, the projection.
    let plain_dir = temp_dir("preview_perf_txt");
    let src = picker_over(&plain_dir, "txt", &body);
    let (rpc, mut incoming) = start(&plain_dir, &src).await;
    exec_lua(&rpc, "btv.picker.open('perf_files')").await;
    poll_menu(&rpc, &mut incoming)
        .await
        .expect("the menu opens");
    let plain = worst_move(&rpc, &mut incoming).await;

    // The same navigation over highlightable files.
    let lua_dir = temp_dir("preview_perf_lua");
    let src = picker_over(&lua_dir, "lua", &body);
    let (rpc, mut incoming) = start(&lua_dir, &src).await;
    exec_lua(&rpc, "btv.picker.open('perf_files')").await;

    // Settle on the first row until its highlights arrive: the grammar loads off the
    // thread, so timing before it lands would time the un-highlighted path and prove
    // nothing. This is also property 2 — a *settled* selection colours in.
    let mut spans = 0;
    for _ in 0..200 {
        if let Some(map) = poll_menu(&rpc, &mut incoming).await {
            spans = preview_spans(&menu_of(&map));
            if spans > 0 {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        spans > 0,
        "a settled preview must highlight (grammar loaded, spans painted)"
    );

    let highlighted = worst_move(&rpc, &mut incoming).await;

    // Machine-independent: the highlightable run must not cost meaningfully more per
    // keystroke than the identical un-highlightable one. Before the fix this was
    // ~130x (≈330ms vs ≈2.5ms in a debug build) — the whole file was parsed and
    // queried inline on the frame that moved the selection.
    let ratio = highlighted.as_secs_f64() / plain.as_secs_f64().max(0.001);
    eprintln!("worst move: plain {plain:.2?}, highlighted {highlighted:.2?} ({ratio:.1}x)");
    assert!(
        ratio < 4.0,
        "moving the picker selection over highlightable files must not pay the \
         preview's parse on the keystroke: {highlighted:.2?} vs {plain:.2?} ({ratio:.1}x)"
    );
}
