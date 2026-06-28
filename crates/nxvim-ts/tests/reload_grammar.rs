//! Regression: `reload_grammar` (`:TSInstall`/reinstall) must not unload a loaded
//! grammar's library out from under buffers still parsed against it.
//!
//! Sibling of `drop_order.rs`. A reparse cancelled by `PARSE_DEADLINE` leaves the
//! `tree_sitter::Parser` holding an external-scanner payload that `ts_parser_delete`
//! later frees *through* the `TSLanguage` living in the grammar's dlopen'd `.so`. The
//! `Engine` field order keeps that library mapped until the parsers are gone — but
//! `reload_grammar` evicts the grammar's cache slot mid-session. If that eviction
//! *dropped* a loaded grammar, the library would unmap while the open buffer's parser
//! (mid-parse, payload allocated) still pointed into it, so re-opening the buffer (or
//! dropping the engine) would call `external_scanner.destroy` on unmapped memory and
//! SIGSEGV. `reload_grammar` therefore *retires* a loaded grammar instead of dropping
//! it. Reaching the end of this test (clean re-open + drop) is the assertion.
//!
//! `#[ignore]`d, not hermetic: installs a real external-scanner grammar (Python) into
//! a temp data dir, which needs network + a C compiler — same opt-in convention as
//! `drop_order.rs`. Run with:
//!
//! ```sh
//! cargo test -p nxvim-ts --test reload_grammar -- --ignored --nocapture
//! ```

use std::path::PathBuf;

use nxvim_core::{BufferId, SyntaxEngine};
use nxvim_ts::Engine;

/// A unique temp dir under the system temp root (harness convention — no tempfile
/// dep), removed on drop.
struct TempDataDir(PathBuf);

impl TempDataDir {
    fn new() -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("nxvim_ts_reload_grammar_{pid}_{nanos}"));
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
/// and is cancelled — leaving the parser mid-parse with its external-scanner payload
/// allocated, the precondition for the destroy-after-unload crash.
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
    block.repeat(120_000)
}

#[test]
#[ignore = "needs network + a C compiler to install a real grammar; opt-in like PTY e2e"]
fn reload_grammar_with_a_cancelled_parse_does_not_segfault() {
    let data = TempDataDir::new();

    nxvim_ts::install::install(&data.0, "python")
        .expect("install python grammar (network + C compiler required)");

    let mut engine = Engine::new(data.0.clone());

    // Open the oversized source: the reparse hits PARSE_DEADLINE and is cancelled,
    // leaving the parser's external-scanner payload allocated.
    let src = big_python_source();
    engine.open(BufferId(1), "python", &src);

    // Reinstall/update the grammar mid-session (what `:TSInstall python` triggers when
    // python is already loaded and a python buffer is open). With the buggy version
    // this dropped the grammar's `.so` while the buffer's mid-parse parser still
    // pointed into it.
    engine.reload_grammar("python");

    // Re-open the buffer against the freshly-resolved grammar. This drops the OLD
    // BufferState (its parser → `ts_parser_delete` → `external_scanner.destroy` through
    // the old `TSLanguage`). That call must hit *mapped* code — i.e. the old library
    // must still be alive (retired, not dropped) — or it SIGSEGVs here.
    engine.open(BufferId(1), "python", &src);

    // Final teardown drops the engine: buffers, then grammars + retired grammars.
    drop(engine);
}
