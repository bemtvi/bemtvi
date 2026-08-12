//! `foldmethod=expr` with the native tree-sitter foldexpr (Phase 4a), end-to-end
//! over a *real* parse: a grammar with a `folds.scm` is installed, a file is
//! opened, the tree-sitter foldexpr is enabled, and the redraw must collapse the
//! foldable block — hiding interior lines while keeping the construct's first line
//! visible.
//!
//! `#[ignore]`d, not hermetic: it installs a real grammar into a temp data dir,
//! which needs network + a C compiler — the same opt-in posture as the other
//! treesitter / PTY e2e tests. Run with:
//!
//! ```sh
//! cargo test -p bemtvi-server --test treesitter_folds -- --ignored --nocapture
//! ```

use bemtvi_server::ServerInit;
use bemtvi_test_harness::*;
use rmpv::Value;

/// The visible buffer-line numbers of the first window (the redraw `numbers`
/// array, dropping `~` fillers past end-of-buffer).
fn visible_numbers(map: &[(Value, Value)]) -> Vec<u64> {
    window0_field(map, "numbers")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

#[tokio::test]
#[ignore = "needs network + a C compiler to install a real grammar; opt-in like the other ts e2e tests"]
async fn treesitter_foldexpr_collapses_a_block_body() {
    // The server resolves grammars from `BEMTVI_DATA_DIR` (process-global); serialize
    // against other tests that touch process-wide state.
    let _guard = serial_lock().lock().await;

    let data = temp_dir("ts_folds_data");
    bemtvi_ts::install::install(&data, "lua")
        .expect("install lua grammar (network + C compiler required)");
    std::env::set_var("BEMTVI_DATA_DIR", &data);

    // A five-line lua function; its body is foldable via lua's `folds.scm`.
    let src = "local function f()\n  local x = 1\n  local y = 2\n  return x + y\nend\n";
    let file = write_temp("ts_folds", "lua", src);
    let (rpc, mut incoming) = start_attached(
        ServerInit {
            file: Some(file),
            ..Default::default()
        },
        80,
        24,
    )
    .await;

    // Enable the native tree-sitter foldexpr.
    feed(&rpc, ":set foldexpr=v:lua.vim.treesitter.foldexpr()<CR>");
    feed(&rpc, ":set foldmethod=expr<CR>");

    // The function body folds: at least one interior line is hidden (fewer than the
    // five buffer lines remain visible), and the function's first line is shown.
    let map = wait_redraw(&mut incoming, |m| visible_numbers(m).len() < 5).await;
    let visible = visible_numbers(&map);
    assert!(
        visible.contains(&1),
        "the function's first line stays visible, got {visible:?}"
    );
    assert!(
        visible.len() < 5,
        "the fold hides interior lines, got {visible:?}"
    );

    std::env::remove_var("BEMTVI_DATA_DIR");
}
