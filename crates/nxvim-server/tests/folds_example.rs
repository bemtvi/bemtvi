//! The shipped `examples/folds/` config must load and fold the sample buffer
//! end-to-end — the project's "verified end-to-end" example convention. Loaded
//! exactly as a user runs it (`NXVIM_CONFIG=examples/folds cargo run -p nxvim --
//! .../sample.lua`): the config dir's `init.lua` sources at startup, then the
//! sample opens and its `FileType`/`BufReadPost` autocmd sets `foldmethod=indent`,
//! so the nested blocks collapse on screen.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, barrier, drain_to_latest_redraw, feed, spawn, temp_dir, window0_field,
};
use rmpv::Value;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

/// Window 0's visible buffer-line numbers (filler rows dropped) — what the screen
/// shows once folds collapse hidden lines.
fn visible_numbers(map: &[(Value, Value)]) -> Vec<u64> {
    window0_field(map, "numbers")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

/// Pump redraws (each `barrier` forces one) until window 0's visible line numbers
/// satisfy `pred` — the indent fold is computed a tick after the buffer loads.
async fn poll_visible_numbers(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    pred: impl Fn(&[u64]) -> bool,
) -> Option<Vec<u64>> {
    for _ in 0..100 {
        barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |_| true) {
            let nums = visible_numbers(&map);
            if pred(&nums) {
                return Some(nums);
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

#[tokio::test]
async fn shipped_example_folds_the_sample() {
    let example_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/folds");

    // Copy the sample into a temp `.lua` file so the test never edits the checked-in
    // one (and so the `FileType=lua` autocmd fires on a real path).
    let sample = include_str!("../../../examples/folds/sample.lua");
    let dir = temp_dir("folds_example");
    let path = dir.join("sample.lua");
    std::fs::write(&path, sample).expect("write sample");
    let total = sample.lines().count() as u64;

    let init = ServerInit {
        config_dir: Some(example_dir),
        file: Some(path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, mut incoming) = spawn(init);
    attach(&rpc, 80, 40).await;
    // A no-op motion runs an input tick, which recomputes the indent folds from the
    // `foldmethod` the config's autocmd set (the fold engine recomputes on the input
    // loop, not on a bare redraw barrier).
    feed(&rpc, "gg");

    // The config sets `foldmethod=indent`, so the nested blocks fold closed at the
    // default `foldlevel=0`: fewer lines show than the file has, and an indented
    // interior line (`    border = "rounded",`, line 9, inside a closed table) is
    // hidden behind its fold's placeholder.
    let nums = poll_visible_numbers(&rpc, &mut incoming, |n| {
        (n.len() as u64) < total && !n.contains(&9)
    })
    .await;

    assert!(
        nums.is_some(),
        "the example should fold the sample's nested blocks (line 9 hidden, \
         fewer than {total} lines visible); got {:?}",
        nums
    );
}
