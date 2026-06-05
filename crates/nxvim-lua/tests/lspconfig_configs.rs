//! End-to-end coverage that the real nvim-lspconfig server configs load and
//! resolve against nxvim's `vim.*` surface — the guard for "all of
//! nvim-lspconfig works out of the box". For every `lsp/<name>.lua` in the
//! vendored (submodule) checkout it loads the chunk, then resolves `root_dir`
//! and the `cmd` builder exactly as `vim.lsp.enable` does, and asserts none
//! error — except a documented allowlist.
//!
//! `vendor/nvim-lspconfig` is a git submodule; when it isn't initialized this
//! test skips (it is reference-only, like `vendor/neovim`). To run it:
//!   git submodule update --init vendor/nvim-lspconfig

use nxvim_lua::LuaRuntime;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn every_lspconfig_server_config_loads_and_resolves() {
    let repo = repo_root();
    let vendor = repo.join("vendor/nvim-lspconfig");
    if !vendor.join("lsp").is_dir() {
        eprintln!("skipping: vendor/nvim-lspconfig submodule not initialized");
        return;
    }

    let rt = LuaRuntime::new(vec![vendor]).expect("runtime");
    // A real source file inside a Cargo project, so the `vim.fs.root`/`cargo
    // metadata`-style root resolution the configs do has something to find.
    let rs = repo.join("crates/nxvim-core/src/editor.rs");
    rt.set_buf_snapshot(1, rs.to_str().unwrap(), "rust")
        .unwrap();

    // Loads each config chunk RAW (bypassing the lsp_base_config pcall that would
    // otherwise swallow a top-level error into an empty config), then resolves
    // root_dir + cmd the way the start path does. Returns "TOTAL=N ERRORS=M\n<one
    // line per failing config>".
    let harness = r#"
local files = vim.api.nvim_get_runtime_file('lsp/*.lua', true)
local out, nerr = {}, 0
for _, file in ipairs(files) do
  local name = file:match('([^/]+)%.lua$')
  local chunk, perr = loadstring(vim._read_file(file), '@' .. file)
  local ok, cfg = false, nil
  if chunk then ok, cfg = pcall(chunk) else cfg = 'parse: ' .. tostring(perr) end
  if not ok then
    nerr = nerr + 1; out[#out+1] = name .. ' | LOAD: ' .. tostring(cfg)
  elseif type(cfg) ~= 'table' then
    nerr = nerr + 1; out[#out+1] = name .. ' | NOTABLE'
  else
    local root, ok_root, rerr = nil, true, nil
    local rd = cfg.root_dir
    if type(rd) == 'function' then
      ok_root, rerr = pcall(rd, 1, function(r) root = r end)
    elseif type(rd) == 'string' then
      root = rd
    elseif cfg.root_markers then
      ok_root, rerr = pcall(function() root = vim.fs.root(1, cfg.root_markers) end)
    end
    if not ok_root then
      nerr = nerr + 1; out[#out+1] = name .. ' | ROOT_DIR: ' .. tostring(rerr)
    elseif type(cfg.cmd) == 'function' then
      local config = {}
      for k, v in pairs(cfg) do config[k] = v end
      config.root_dir = root
      local ok_cmd, res = pcall(cfg.cmd, {}, config)
      if not ok_cmd then
        nerr = nerr + 1; out[#out+1] = name .. ' | CMD: ' .. tostring(res)
      end
    end
  end
end
return 'TOTAL=' .. #files .. ' ERRORS=' .. nerr .. '\n' .. table.concat(out, '\n')
"#;

    let report = rt
        .eval_to_value(harness)
        .expect("harness ran")
        .as_str()
        .unwrap_or("")
        .to_string();
    let lines: Vec<&str> = report.lines().collect();
    let summary = lines.first().copied().unwrap_or("");
    eprintln!("{report}");

    // Configs allowed to fail to RESOLVE (they need user-supplied config that has
    // no sensible default): powershell_es needs `bundle_path` to the
    // PowerShellEditorServices bundle (neovim errors here too). At the real
    // `vim.lsp.enable` path these are pcall-skipped, never crashing enable.
    const ALLOWED: &[&str] = &["powershell_es"];

    let failures: Vec<&str> = lines
        .iter()
        .skip(1)
        .filter(|l| !l.is_empty())
        .filter(|l| {
            let name = l.split_once(" | ").map(|(n, _)| n).unwrap_or(l);
            !ALLOWED.contains(&name)
        })
        .copied()
        .collect();

    assert!(
        failures.is_empty(),
        "{} nvim-lspconfig config(s) failed to load/resolve ({summary}):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
