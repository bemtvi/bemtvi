//! Load coverage: the real telescope.nvim + plenary.nvim must `require` cleanly
//! against nxvim's `vim.*` surface, set up, and open every picker shape (builtin
//! find_files / live_grep + a custom table picker) without erroring — the
//! synchronous half of "telescope works" (the async filtering is proven
//! end-to-end in nxvim-server's `telescope_e2e`). Also guards the shipped
//! `examples/telescope/init.lua` against bitrot. Points runtimepath at the user's
//! lazy install (telescope/plenary aren't vendored); SKIPS when absent, like the
//! `lspconfig_configs` submodule test.

use nxvim_lua::LuaRuntime;
use std::path::PathBuf;

fn lazy_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let d = PathBuf::from(home).join(".local/share/nvim/lazy");
    d.is_dir().then_some(d)
}

#[test]
fn telescope_requires_and_sets_up() {
    let Some(lazy) = lazy_dir() else {
        eprintln!("skip: no ~/.local/share/nvim/lazy");
        return;
    };
    let telescope = lazy.join("telescope.nvim");
    let plenary = lazy.join("plenary.nvim");
    if !telescope.join("lua").is_dir() || !plenary.join("lua").is_dir() {
        eprintln!("skip: telescope/plenary not installed");
        return;
    }

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
    let Some(lazy) = lazy_dir() else {
        eprintln!("skip: no ~/.local/share/nvim/lazy");
        return;
    };
    let telescope = lazy.join("telescope.nvim");
    let plenary = lazy.join("plenary.nvim");
    if !telescope.join("lua").is_dir() || !plenary.join("lua").is_dir() {
        eprintln!("skip: telescope/plenary not installed");
        return;
    }
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
