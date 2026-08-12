//! Regression: a grabbing float opened at startup must NOT leave the buffer behind
//! it un-highlighted.
//!
//! The reported bug (`examples/btvchecklist`): an `init.lua` that `btv.schedule`s a
//! grabbing `btv.view` float at startup spawns the dialog immediately, but the file
//! in the background window stays dark — its syntax highlighting only appears once
//! the float is dismissed. The cause was that `refresh_highlights` only ever
//! refreshed the *current* (focused) buffer's spans; with the float grabbing focus
//! on the very first convergence, the background buffer was never the current buffer
//! and so was never highlighted, until closing the float refocused it.
//!
//! This asserts the whole server path: a config whose `init.lua` schedules a
//! grabbing float over a freshly-opened (highlightable) file, then — with the float
//! still up — a redraw frame in which the background window paints highlight spans.
//! With the old behaviour no such frame ever arrives (`wait_redraw` times out and
//! panics); with the fix the background colours in while the float is open.
//!
//! `#[ignore]`d, not hermetic: it installs a real grammar into a temp data dir,
//! which needs network + a C compiler — the same opt-in posture as the other
//! treesitter e2e tests. Run with:
//!
//! ```sh
//! cargo test -p bemtvi-server --test highlight_under_float -- --ignored --nocapture
//! ```

use bemtvi_server::ServerInit;
use bemtvi_test_harness::*;
use rmpv::Value;

/// Number of windows in a redraw frame.
fn window_count(map: &[(Value, Value)]) -> usize {
    map_get(map, "windows")
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

/// True if **any** window in the frame paints at least one highlight span. The
/// float's own buffer has no filetype/grammar, so the only highlights can come from
/// the background file window.
fn any_window_highlighted(map: &[(Value, Value)]) -> bool {
    let Some(Value::Array(wins)) = map_get(map, "windows") else {
        return false;
    };
    wins.iter().any(|w| {
        let Value::Map(win) = w else { return false };
        map_get(win, "highlights")
            .and_then(Value::as_array)
            .is_some_and(|rows| {
                rows.iter()
                    .any(|r| r.as_array().is_some_and(|c| !c.is_empty()))
            })
    })
}

#[tokio::test]
#[ignore = "needs network + a C compiler to install a real grammar; opt-in like the other ts e2e tests"]
async fn background_buffer_highlights_while_a_grabbing_float_is_open() {
    // The server resolves grammars from `BEMTVI_DATA_DIR` (process-global), so serialize
    // against other tests that touch process-wide state while it is set.
    let _guard = serial_lock().lock().await;

    let data = temp_dir("hl_under_float_data");
    bemtvi_ts::install::install(&data, "rust")
        .expect("install rust grammar (network + C compiler required)");
    std::env::set_var("BEMTVI_DATA_DIR", &data);

    // A config that — exactly like examples/btvchecklist — schedules a grabbing float
    // at startup, so it grabs focus on the first convergence before the background
    // file has ever been highlighted.
    let config = temp_dir("hl_under_float_cfg");
    std::fs::write(
        config.join("init.lua"),
        r#"btv.schedule(function()
             local vw = btv.view.create{}
             vw:set_lines{ "dialog" }
             vw:mount{ float = { width = 20, height = 4, grab = true } }
           end)"#,
    )
    .expect("write init.lua");

    let file = write_temp(
        "hl_under_float",
        "rs",
        "fn main() {\n    let x = 42;\n    println!(\"{}\", x);\n}\n",
    );

    let (rpc, mut incoming) = start_attached(
        ServerInit {
            file: Some(file),
            config_dir: Some(config),
            ..Default::default()
        },
        80,
        40,
    )
    .await;

    // Pump the loop so the scheduled float actually opens (each barrier round-trip
    // lets the server drain the deferred callback and repaint).
    for _ in 0..20 {
        rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    }

    // With the float still grabbing focus, a frame must show the background window's
    // highlights. Pre-fix this never arrives → `wait_redraw` times out and panics.
    let map = wait_redraw(&mut incoming, |m| {
        window_count(m) >= 2 && any_window_highlighted(m)
    })
    .await;

    assert!(
        window_count(&map) >= 2,
        "the grabbing float should still be open ({} windows)",
        window_count(&map)
    );
    assert!(
        any_window_highlighted(&map),
        "the background buffer should be highlighted while the float is open"
    );

    std::env::remove_var("BEMTVI_DATA_DIR");
}
