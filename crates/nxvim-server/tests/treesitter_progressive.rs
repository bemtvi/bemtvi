//! Regression: a large file's treesitter highlighting must arrive **progressively**,
//! not never.
//!
//! The engine bounds each (re)parse to a per-frame deadline so a keystroke is never
//! blocked on parsing a huge file. A parse that blows that budget is *cancelled* —
//! but tree-sitter retains the outstanding parse, so re-invoking it resumes where it
//! left off. The engine flags the buffer `parse_pending`; the server bypasses its
//! highlight memo while pending and re-arms a short timer after each redraw, so the
//! parse is driven to completion across frames **with no user input** and the file
//! colours in within a few frames. (Before this, a cancelled parse left `tree = None`
//! forever and nothing ever re-drove it: the file stayed permanently un-highlighted.)
//!
//! This asserts the whole server-driven path: open a file too big to parse in one
//! frame, feed *nothing*, and wait for the redraw `highlights` payload to fill in.
//! With the old behaviour `wait_redraw` would time out (panic); with progressive
//! parsing it converges.
//!
//! `#[ignore]`d, not hermetic: it installs a real grammar **with an external
//! scanner** (Python) into a temp data dir, which needs network + a C compiler — the
//! same opt-in posture as the PTY e2e tests. Run with:
//!
//! ```sh
//! cargo test -p nxvim-server --test treesitter_progressive -- --ignored --nocapture
//! ```

use nxvim_server::ServerInit;
use nxvim_test_harness::*;
use rmpv::Value;

/// Python source big enough that a single full parse can't finish inside the
/// engine's per-frame deadline — even in a release build (~1 MB parses in well over
/// the budget) — so the progressive resume path is actually exercised rather than a
/// one-frame complete parse, yet small enough to converge in a fraction of a second.
fn big_python_source() -> String {
    let block = "\
def f(x):
    if x > 0:
        for i in range(x):
            y = [a for a in range(i) if a % 2 == 0]
            z = {'k': (i, y), 'n': i * x - 1}
            print(y, z)
    return x

class C:
    def m(self):
        return [self.m() for _ in range(3)]
";
    block.repeat(4_000)
}

#[tokio::test]
#[ignore = "needs network + a C compiler to install a real grammar; opt-in like PTY e2e"]
async fn large_file_highlights_progressively_without_input() {
    // The server resolves grammars from `NXVIM_DATA_DIR` (process-global), so serialize
    // against other tests that touch process-wide state while it is set.
    let _guard = serial_lock().lock().await;

    let data = temp_dir("ts_progressive_data");
    nxvim_ts::install::install(&data, "python")
        .expect("install python grammar (network + C compiler required)");
    std::env::set_var("NXVIM_DATA_DIR", &data);

    let file = write_temp("ts_progressive", "py", &big_python_source());
    let (_rpc, mut incoming) = start_attached(
        ServerInit {
            file: Some(file),
            ..Default::default()
        },
        80,
        40,
    )
    .await;

    // No input: the server's resume timer alone must drive the parse to completion and
    // fill the highlights payload. `wait_redraw` panics if no matching frame arrives,
    // which is exactly the pre-fix "never highlights" failure.
    let map = wait_redraw(&mut incoming, |m| {
        window0_field(m, "highlights")
            .and_then(Value::as_array)
            .is_some_and(|rows| {
                rows.iter()
                    .any(|r| r.as_array().is_some_and(|cols| !cols.is_empty()))
            })
    })
    .await;

    let spans: usize = window0_field(&map, "highlights")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter_map(Value::as_array)
        .map(Vec::len)
        .sum();
    assert!(spans > 0, "a large file should eventually highlight");

    std::env::remove_var("NXVIM_DATA_DIR");
}
