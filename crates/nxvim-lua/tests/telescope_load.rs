//! Load coverage: the real telescope.nvim + plenary.nvim must `require` cleanly
//! against nxvim's `vim.*` surface, set up, and open every picker shape (builtin
//! find_files / live_grep + a custom table picker) without erroring — the
//! synchronous half of "telescope works" (the async filtering is proven
//! end-to-end in nxvim-server's `telescope_e2e`). Also guards the shipped
//! `examples/telescope/init.lua` against bitrot. Clones telescope/plenary (pinned)
//! into a shared cache so it runs against a known-good revision rather than the
//! developer's local install — hermetic. SKIPS only when the clone can't happen
//! (no `git` / no network), like the `lspconfig_configs` submodule test.
//!
//! (This crate can't dev-depend on `nxvim-test-harness` — that would form a Lua-
//! backend-feature cycle through `nxvim-server` — so the small clone helper is
//! inlined here rather than shared from the harness.)

use nxvim_lua::LuaRuntime;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// `(name, git URL, pinned commit)` for the plugins this test needs — the same
/// known-good revisions the harness pins.
const PINS: &[(&str, &str, &str)] = &[
    (
        "telescope.nvim",
        "https://github.com/nvim-telescope/telescope.nvim.git",
        "a0bbec21143c7bc5f8bb02e0005fa0b982edc026",
    ),
    (
        "plenary.nvim",
        "https://github.com/nvim-lua/plenary.nvim.git",
        "74b06c6c75e4eeb3108ec01852001636d85a932b",
    ),
];

/// Clone telescope + plenary (pinned) into the shared cache; `None` if either can't
/// be fetched (no git / no network) so the caller skips.
fn telescope_plenary() -> Option<(PathBuf, PathBuf)> {
    Some((
        clone_plugin("telescope.nvim")?,
        clone_plugin("plenary.nvim")?,
    ))
}

/// Clone the pinned `name` into the shared `nxvim-test-plugins` cache (reused
/// across runs, keyed by commit; published atomically), returning its path or
/// `None` on failure. A compact mirror of the harness' `clone_plugin`.
fn clone_plugin(name: &str) -> Option<PathBuf> {
    let &(_, url, rev) = PINS.iter().find(|(n, _, _)| *n == name)?;
    let cache = std::env::temp_dir().join("nxvim-test-plugins");
    let target = cache.join(name);
    if at_rev(&target, rev) {
        return Some(target);
    }
    std::fs::create_dir_all(&cache).ok()?;
    if target.exists() {
        let _ = std::fs::remove_dir_all(&target);
    }
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = cache.join(format!(
        ".tmp-{name}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let t = tmp.to_string_lossy().into_owned();
    let ok = git(&["init", "--quiet", &t])
        && git(&["-C", &t, "remote", "add", "origin", url])
        && git(&["-C", &t, "fetch", "--quiet", "--depth", "1", "origin", rev])
        && git(&["-C", &t, "checkout", "--quiet", "FETCH_HEAD"]);
    if !ok {
        let _ = std::fs::remove_dir_all(&tmp);
        return at_rev(&target, rev).then_some(target);
    }
    match std::fs::rename(&tmp, &target) {
        Ok(()) => Some(target),
        Err(_) => {
            let _ = std::fs::remove_dir_all(&tmp);
            at_rev(&target, rev).then_some(target)
        }
    }
}

/// True when `dir` is a git checkout sitting exactly at `rev`.
fn at_rev(dir: &std::path::Path, rev: &str) -> bool {
    if !dir.join(".git").exists() {
        return false;
    }
    std::process::Command::new("git")
        .args(["-C", &dir.to_string_lossy(), "rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| String::from_utf8_lossy(&o.stdout).trim() == rev)
}

/// Run `git` with `args`, suppressing output; return whether it exited 0.
fn git(args: &[&str]) -> bool {
    std::process::Command::new("git")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn telescope_requires_and_sets_up() {
    let Some((telescope, plenary)) = telescope_plenary() else {
        eprintln!("skip: could not clone telescope/plenary (no git / no network)");
        return;
    };

    let rt = LuaRuntime::new(vec![telescope, plenary]).expect("runtime");
    rt.set_buf_snapshot(1, "/tmp/scratch.txt", "text").unwrap();

    let harness = r#"
local out = {}
local function step(label, fn)
  local ok, err = pcall(fn)
  out[#out+1] = (ok and 'OK  ' or 'ERR ') .. label .. (ok and '' or '  -->  ' .. tostring(err))
  return ok
end

step("require('telescope').setup", function() require('telescope').setup{} end)

-- Drive a real picker end-to-end (the synchronous part: float + scratch buffers +
-- keymaps + autocmds + the finder/sorter wiring). The async job results need the
-- server loop to drain, which this bare runtime has no; the picker OPENING is the
-- API-surface exercise we want.
step("builtin.find_files{}", function()
  require('telescope.builtin').find_files({ cwd = '/tmp' })
end)

step("builtin.live_grep{}", function()
  require('telescope.builtin').live_grep({ cwd = '/tmp' })
end)

step("custom pickers.new + find", function()
  local pickers = require('telescope.pickers')
  local finders = require('telescope.finders')
  local conf = require('telescope.config').values
  pickers.new({}, {
    prompt_title = 'scratch',
    finder = finders.new_table({ results = { 'alpha', 'beta', 'gamma' } }),
    sorter = conf.generic_sorter({}),
  }):find()
end)

return table.concat(out, '\n')
"#;

    let report = rt
        .eval_to_value(harness)
        .expect("harness ran")
        .as_str()
        .unwrap_or("<non-string>")
        .to_string();
    eprintln!("\n===== telescope load report =====\n{report}\n=================================");
    assert!(
        !report.contains("ERR "),
        "a telescope load/open step failed:\n{report}"
    );
}

/// The shipped `examples/telescope/init.lua` must load cleanly against the same
/// surface (it's the user-facing entry point), registering its `<leader>f*` maps.
#[test]
fn example_init_loads() {
    let Some((telescope, plenary)) = telescope_plenary() else {
        eprintln!("skip: could not clone telescope/plenary (no git / no network)");
        return;
    };
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let init = std::fs::read_to_string(repo.join("examples/telescope/init.lua"))
        .expect("read example init.lua");

    let rt = LuaRuntime::new(vec![telescope, plenary]).expect("runtime");
    rt.set_buf_snapshot(1, "/tmp/scratch.txt", "text").unwrap();
    rt.exec(&init)
        .expect("example init.lua loads without error");

    // The three pickers' leader maps should be registered.
    let maps = rt
        .eval_to_value("return #(vim._keymaps or {})")
        .expect("count maps");
    assert!(
        maps.as_i64().unwrap_or(0) >= 3,
        "example should register its <leader>f* maps, found {maps:?}"
    );
}
