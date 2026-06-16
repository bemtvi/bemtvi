//! Regression: dropping the `Engine` must not segfault when a buffer's parser was
//! left mid-parse with its external-scanner payload still allocated.
//!
//! A reparse cancelled by `PARSE_DEADLINE` (a large file that doesn't finish parsing
//! in one frame) leaves the `tree_sitter::Parser` with a non-null external-scanner
//! payload. `Parser::drop` → `ts_parser_delete` then calls the grammar's
//! `external_scanner.destroy` *through* the `TSLanguage`, which lives in the grammar's
//! dlopen'd `.so`. If the `Engine` drops its grammar libraries before its parsers, that
//! call dereferences unmapped memory and the process dies with SIGSEGV at exit — which
//! is exactly what `:q` on a large Python file used to do. The `Engine` field order
//! (`buffers` before `grammars`) is what keeps the library mapped until the parsers are
//! gone. A completed parse never hit this because tree-sitter resets the scanner (and
//! nulls the payload) at the end of a successful parse, while the library is still live.
//!
//! `#[ignore]`d, not hermetic: it installs a real grammar **with an external scanner**
//! (Python) into a temp data dir, which needs network + a C compiler. This mirrors the
//! PTY e2e convention (real-resource tests are opt-in). Run with:
//!
//! ```sh
//! cargo test -p nxvim-ts --test drop_order -- --ignored --nocapture
//! ```

use std::path::PathBuf;

use nxvim_core::BufferId;
use nxvim_ts::Engine;

/// A unique temp dir under the system temp root (the harness convention — no
/// tempfile dep), removed on drop.
struct TempDataDir(PathBuf);

impl TempDataDir {
    fn new() -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("nxvim_ts_drop_order_{pid}_{nanos}"));
        std::fs::create_dir_all(&dir).expect("create temp data dir");
        TempDataDir(dir)
    }
}

impl Drop for TempDataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Python source large enough that a single full parse blows past `PARSE_DEADLINE`
/// (50ms) and is cancelled — leaving the parser mid-parse with its external-scanner
/// payload allocated. Deliberately oversized so the deadline trips even on a fast
/// machine; Python's grammar carries an external scanner (indentation), which is the
/// shape that triggers the crash.
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
    // ~16 MB: enough that the incremental full parse cannot finish in one 50ms frame.
    block.repeat(120_000)
}

#[test]
#[ignore = "needs network + a C compiler to install a real grammar; opt-in like PTY e2e"]
fn dropping_engine_with_a_cancelled_parse_does_not_segfault() {
    let data = TempDataDir::new();

    // Install the Python grammar (parser .so + queries) into the temp data dir. Fail
    // loud if it can't be fetched/compiled — this opt-in test has nothing to assert
    // without a real external-scanner grammar.
    nxvim_ts::install::install(&data.0, "python")
        .expect("install python grammar (network + C compiler required)");

    let mut engine = Engine::new(data.0.clone());

    // Opening parses from full text; the oversized source forces the reparse to hit
    // PARSE_DEADLINE and be cancelled, leaving the parser's external-scanner payload
    // allocated (the precondition for the destroy-after-unload crash).
    let src = big_python_source();
    engine.open(BufferId(1), "python", &src);

    // The crash was at teardown: dropping the engine drops its grammar libraries and
    // its parsers. With the buggy field order the library unloaded first and this drop
    // segfaulted. Reaching the end of the test (clean drop) is the assertion.
    drop(engine);
}
