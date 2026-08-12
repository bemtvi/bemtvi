//! Black-box coverage for the `bemtvi --test-plugin` runner: build a throwaway plugin
//! repo with a Lua `btv.test` suite, run the real binary against it, and assert on the
//! exit code + report. The runner is Rust, so this is its end-to-end test (the Lua
//! framework it drives is exercised transitively).

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// A unique temp dir for one fixture (pid + counter, no wall-clock — hermetic).
fn fixture_dir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = bemtvi_test_harness::temp_root().join(format!(
        "bemtvi-testrunner-{}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed),
        tag
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("test")).expect("create fixture test dir");
    dir
}

/// Run the built `bemtvi` binary with `--test-plugin <dir>`; return (success, stdout).
fn run(dir: &PathBuf) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bemtvi"))
        .arg("--test-plugin")
        .arg(dir)
        .output()
        .expect("spawn bemtvi --test-plugin");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn passing_suite_exits_zero() {
    let dir = fixture_dir("pass");
    std::fs::write(
        dir.join("test/basic_spec.lua"),
        r#"
        btv.test.describe("fixture", function()
          btv.test.it("edits a buffer", function(t)
            t:feed("ihello<Esc>")
            btv.test.expect(t:lines()).to_equal({ "hello" })
            btv.test.expect(t:mode()).to_be("n")
          end)
          btv.test.it("deletes a line", function(t)
            t:feed("iline1<CR>line2<Esc>gg")
            t:feed("dd")
            btv.test.expect(t:lines()).to_equal({ "line2" })
          end)
        end)
        "#,
    )
    .unwrap();

    let (ok, stdout) = run(&dir);
    assert!(ok, "expected exit 0 for a passing suite; stdout:\n{stdout}");
    assert!(stdout.contains("2 passed, 0 failed"), "stdout:\n{stdout}");
}

#[test]
fn failing_assertion_exits_nonzero() {
    let dir = fixture_dir("fail");
    std::fs::write(
        dir.join("test/fail_spec.lua"),
        r#"
        btv.test.describe("fixture", function()
          btv.test.it("has a wrong expectation", function(t)
            t:feed("iabc<Esc>")
            btv.test.expect(t:lines()).to_equal({ "xyz" })
          end)
        end)
        "#,
    )
    .unwrap();

    let (ok, stdout) = run(&dir);
    assert!(
        !ok,
        "expected a non-zero exit for a failing suite; stdout:\n{stdout}"
    );
    assert!(stdout.contains("0 passed, 1 failed"), "stdout:\n{stdout}");
}

#[test]
fn broken_spec_is_reported_and_fails_the_run() {
    // A spec that fails to *load* (syntax error / top-level throw) must fail the run
    // and be named in the report — not be silently skipped while the rest passes
    // (the server's `nvim_exec_lua` swallows Lua errors into an `:echo` + `Nil`
    // reply, so the runner has to detect the load failure itself).
    let dir = fixture_dir("broken");
    std::fs::write(
        dir.join("test/broken_spec.lua"),
        "btv.test.describe('x', function( -- unterminated\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("test/good_spec.lua"),
        r#"
        btv.test.describe("good", function()
          btv.test.it("still runs", function(t)
            btv.test.expect(1).to_be(1)
          end)
        end)
        "#,
    )
    .unwrap();

    let (ok, stdout) = run(&dir);
    assert!(
        !ok,
        "a spec that fails to load must fail the run; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("broken_spec.lua"),
        "the report must name the broken file; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("1 passed"),
        "the healthy spec must still run; stdout:\n{stdout}"
    );
}

#[test]
fn isolation_between_tests() {
    // A buffer edit in one test must not bleed into the next (fresh-slate per test).
    let dir = fixture_dir("iso");
    std::fs::write(
        dir.join("test/iso_spec.lua"),
        r#"
        btv.test.describe("isolation", function()
          btv.test.it("first writes a lot", function(t)
            t:feed("ifirst test content<Esc>")
            btv.test.expect(t:lines()).to_equal({ "first test content" })
          end)
          btv.test.it("second starts empty", function(t)
            btv.test.expect(t:lines()).to_equal({ "" })
          end)
        end)
        "#,
    )
    .unwrap();

    let (ok, stdout) = run(&dir);
    assert!(ok, "expected isolation to hold; stdout:\n{stdout}");
    assert!(stdout.contains("2 passed, 0 failed"), "stdout:\n{stdout}");
}

#[test]
fn clipboard_and_tempdir_seams() {
    // The hermetic seams: peek what a plugin yanks to "+, seed external clipboard
    // text for paste, and write/read a unique temp dir via btv.fs.
    let dir = fixture_dir("seams");
    std::fs::write(
        dir.join("test/seam_spec.lua"),
        r#"
        btv.test.describe("seams", function()
          btv.test.it("peek sees a yank to the + register", function(t)
            t:feed('ihello world<Esc>')
            t:feed('"+yy')
            local text, linewise = btv.test.clipboard.peek()
            btv.test.expect(text).to_contain("hello world")
            btv.test.expect(linewise).to_be_truthy()
          end)
          btv.test.it("seed makes external text pasteable", function(t)
            btv.test.clipboard.seed("from elsewhere", false)
            t:feed('"+p')
            btv.test.expect(t:lines()).to_equal({ "from elsewhere" })
          end)
          btv.test.it("tempdir is writable", function(t)
            local d = btv.test.tempdir()
            local got = t:exec(function()
              btv.await(btv.fs.write(d .. "/x.txt", "hi"))
              return btv.await(btv.fs.read_text(d .. "/x.txt"))
            end)
            btv.test.expect(got).to_be("hi")
          end)
        end)
        "#,
    )
    .unwrap();

    let (ok, stdout) = run(&dir);
    assert!(ok, "expected the seams to work; stdout:\n{stdout}");
    assert!(stdout.contains("3 passed, 0 failed"), "stdout:\n{stdout}");
}
