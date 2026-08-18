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

/// `t:screen()` is the painted-rows accessor — the sibling of `t:lines()`, and the
/// only way a spec can see what the editor draws *instead of* buffer text.
#[test]
fn screen_exposes_painted_rows_not_buffer_text() {
    let dir = fixture_dir("screen");
    std::fs::write(
        dir.join("test/screen_spec.lua"),
        r#"
        btv.test.describe("t:screen", function()
          btv.test.it("shows the rows the client would paint", function(t)
            t:feed("ione<CR>two<Esc>gg")
            -- Buffer text and painted rows agree on the lines that exist…
            btv.test.expect(t:lines()).to_equal({ "one", "two" })
            btv.test.expect(t:screen()[1]).to_be("one")
            btv.test.expect(t:screen()[2]).to_be("two")
            -- …and only the screen carries the `~` fillers past the end, which are
            -- painted but are not buffer lines at all.
            btv.test.expect(t:screen()[3]).to_be("~")
            btv.test.expect(#t:screen() > #t:lines()).to_be(true)
          end)

          btv.test.it("shows a closed fold's placeholder in place of its lines", function(t)
            t:feed("ia<CR>b<CR>c<CR>d<Esc>gg")
            t:feed("zfj") -- fold lines 1-2, created closed
            -- The buffer still has every line; the screen collapses two into one.
            btv.test.expect(#t:lines()).to_be(4)
            btv.test.expect(t:screen()[1]).to_contain("2 lines: a")
            btv.test.expect(t:screen()[2]).to_be("c")
          end)
        end)
        "#,
    )
    .unwrap();

    let (ok, stdout) = run(&dir);
    assert!(ok, "expected exit 0; stdout:\n{stdout}");
    assert!(stdout.contains("2 passed, 0 failed"), "stdout:\n{stdout}");
}

/// Cases must be independent: what one test changes above the buffer — options,
/// globals, registers, keymaps, commands, the `btv.*` expression surfaces — must
/// not reach the next one, or a suite's result depends on the order it ran in.
#[test]
fn a_test_cannot_leak_state_into_the_next() {
    let dir = fixture_dir("isolation");
    std::fs::write(
        dir.join("test/isolation_spec.lua"),
        r#"
        local base = {}
        btv.test.describe("isolation", function()
          btv.test.it("mutates everything it can reach", function(t)
            base.number = btv.o.number
            base.sw = btv.o.shiftwidth
            base.wrap = btv.wo.wrap
            btv.o.number = not base.number
            btv.o.shiftwidth = (base.sw or 8) + 7
            btv.wo.wrap = not base.wrap
            btv.keymap.set("n", "<leader>zzq", "ihi<Esc>")
            btv.command("LeakCmd", function() end, {})
            btv.fold.text([[ "LEAKED" ]])
            btv.indent.expr([[ 4 ]])
            btv.filetype.detect([[ "leaked" ]])
            btv.picker.scorer([[ score + 1 ]])
            btv.complete.scorer([[ score + 1 ]])
            btv.decor.expr([[ return { { 1, 1, "Todo" } } ]])
            btv.qf.text([[ "LEAKED " .. item.text ]])
            btv.qf.parse([[ return { text = "LEAKED" } ]])
            btv.decor.provider({ name = "leaky", on_range = function() end })
            btv.qf.setqflist({ { filename = "leak.c", lnum = 1, text = "leaked" } }, " ")
            btv.reg.set("z", "leaked")
            btv.g.leak_global = "yes"
          end)

          btv.test.it("sees none of it", function(t)
            local left = {}
            local function note(n, cond) if cond then left[#left + 1] = n end end
            note("o.number", btv.o.number ~= base.number)
            note("o.shiftwidth", btv.o.shiftwidth ~= base.sw)
            note("wo.wrap", btv.wo.wrap ~= base.wrap)
            note("keymap", (function()
              for _, m in ipairs(btv.keymap.get("n") or {}) do
                if (m.lhs or ""):find("zzq") then return true end
              end
              return false
            end)())
            note("command", (btv._user_commands or {}).LeakCmd ~= nil)
            -- Every sandbox-expression surface, not a hand-picked four: the
            -- restore list stopped growing with them once already.
            -- Every sandbox surface, from the registry rather than a list written
            -- here: a surface added later is covered without touching this test,
            -- which is exactly how the restore list fell behind in the first place.
            for name, src in pairs(btv._sandbox_srcs or {}) do
              note("expr:" .. name, src ~= nil)
            end
            note("decor.provider", (function()
              for _, p in ipairs((btv._decor or {}).providers or {}) do
                if p.name == "leaky" then return true end
              end
              return false
            end)())
            note("qflist", #(btv.qf.getqflist() or {}) > 0)
            note("register", (btv.reg.get("z") or ""):find("leaked") ~= nil)
            note("g", btv.g.leak_global == "yes")
            btv.test.expect(table.concat(left, ",")).to_be("")
          end)
        end)
        "#,
    )
    .unwrap();

    let (ok, stdout) = run(&dir);
    assert!(ok, "state leaked between tests; stdout:\n{stdout}");
    assert!(stdout.contains("2 passed, 0 failed"), "stdout:\n{stdout}");
}

/// Isolation is a *baseline*, not a wipe: whatever a spec file installs at load
/// time is the state every test starts from. Otherwise the install-once model
/// specs are written against (`require("plugin").setup{}` at the top of a file)
/// would silently stop working.
#[test]
fn file_level_setup_survives_into_every_test() {
    let dir = fixture_dir("baseline");
    std::fs::write(
        dir.join("test/baseline_spec.lua"),
        r#"
        -- File-level setup: part of the baseline, so it must persist.
        btv.g.from_file = "kept"
        btv.keymap.set("n", "<leader>qq", "ihi<Esc>")

        btv.test.describe("baseline", function()
          btv.test.it("sees the file's setup", function(t)
            btv.test.expect(btv.g.from_file).to_be("kept")
            btv.g.from_test = "temporary"
          end)

          btv.test.it("still sees it, and not the previous test's change", function(t)
            btv.test.expect(btv.g.from_file).to_be("kept")
            btv.test.expect(btv.g.from_test).to_be_nil()
            local found = false
            for _, m in ipairs(btv.keymap.get("n") or {}) do
              if (m.lhs or ""):find("qq") then found = true end
            end
            btv.test.expect(found).to_be(true)
          end)
        end)
        "#,
    )
    .unwrap();

    let (ok, stdout) = run(&dir);
    assert!(ok, "the baseline was not preserved; stdout:\n{stdout}");
    assert!(stdout.contains("2 passed, 0 failed"), "stdout:\n{stdout}");
}
