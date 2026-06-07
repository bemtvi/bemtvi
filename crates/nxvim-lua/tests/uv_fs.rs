//! The `vim.uv` / `vim.loop` libuv **filesystem** surface that real plugins bind
//! directly (rather than going through `vim.system`). `plenary.path` is the
//! canonical consumer: it opens/reads/writes through the `uv.fs_open`,
//! `uv.fs_read`, `uv.fs_write`, and `uv.fs_close` family, stats with
//! `uv.fs_stat`, and decides `is_dir()`/`is_file()` from the unix `st_mode` bits
//! in `fs_stat(...).mode`.
//!
//! These tests exercise that surface end-to-end against a real temp directory,
//! including the exact `S_IF`-bitmask logic `plenary.path` uses, so a regression
//! in the `mode` field (which `plenary` depends on but `vim.fs`/lspconfig never
//! touched) is caught here rather than as a mysterious "every path is a file".

use nxvim_lua::LuaRuntime;
use std::path::PathBuf;

/// A per-test scratch dir under the OS temp dir, unique by test name, recreated
/// fresh each run. Returned as a forward-slashed string Lua can paste into paths.
fn scratch(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("nxvim-uvfs-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir.to_string_lossy().replace('\\', "/")
}

#[test]
fn fs_open_write_read_close_round_trips_a_file() {
    let dir = scratch("rw");
    let rt = LuaRuntime::new(vec![]).expect("runtime");
    let report = rt
        .eval_to_value(&format!(
            r#"
local uv = vim.uv
local file = "{dir}/hello.txt"

-- write: open 'w' (create+truncate, mode 0644), write at -1, close.
local fd = assert(uv.fs_open(file, "w", tonumber("644", 8)))
assert(uv.fs_write(fd, "hello uv\n", -1))
assert(uv.fs_close(fd))

-- the file now exists and stats as a regular file of the right size.
local st = assert(uv.fs_stat(file))
assert(st.type == "file", "type should be file, got " .. tostring(st.type))
assert(st.size == 9, "size should be 9, got " .. tostring(st.size))

-- read it back: open 'r', fstat for the size, read that many bytes at 0.
local rfd = assert(uv.fs_open(file, "r", tonumber("644", 8)))
local fst = assert(uv.fs_fstat(rfd))
local data = assert(uv.fs_read(rfd, fst.size, 0))
assert(uv.fs_close(rfd))
assert(data == "hello uv\n", "round-tripped data mismatch: " .. vim.inspect(data))

return "ok"
"#
        ))
        .expect("harness ran")
        .as_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(report, "ok");
}

#[test]
fn fs_stat_mode_drives_plenary_is_dir_and_is_file() {
    let dir = scratch("mode");
    std::fs::write(
        format!("{dir}/f.txt").replace('/', std::path::MAIN_SEPARATOR_STR),
        "x",
    )
    .ok();
    let rt = LuaRuntime::new(vec![]).expect("runtime");
    let report = rt
        .eval_to_value(&format!(
            r#"
local uv = vim.uv
-- plenary.path's exact discriminator: S_IF.DIR / S_IF.REG bitmask over st_mode.
local S_IF = {{ DIR = 0x4000, REG = 0x8000 }}
local band = function(reg, value) return bit.band(reg, value) == reg end

local fst = assert(uv.fs_open("{dir}/f.txt", "w", tonumber("644", 8)))
uv.fs_close(fst)

local file_mode = (uv.fs_stat("{dir}/f.txt") or {{}}).mode or 0
local dir_mode  = (uv.fs_stat("{dir}") or {{}}).mode or 0

-- A regular file: REG bit set, DIR bit clear.
assert(band(S_IF.REG, file_mode), "file should match S_IF.REG (mode=" .. file_mode .. ")")
assert(not band(S_IF.DIR, file_mode), "file should NOT match S_IF.DIR")
-- A directory: DIR bit set, REG bit clear.
assert(band(S_IF.DIR, dir_mode), "dir should match S_IF.DIR (mode=" .. dir_mode .. ")")
assert(not band(S_IF.REG, dir_mode), "dir should NOT match S_IF.REG")

return "ok"
"#
        ))
        .expect("harness ran")
        .as_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(report, "ok");
}

#[test]
fn fs_mkdir_rename_unlink_rmdir_manage_the_tree() {
    let dir = scratch("tree");
    let rt = LuaRuntime::new(vec![]).expect("runtime");
    let report = rt
        .eval_to_value(&format!(
            r#"
local uv = vim.uv

-- mkdir a single level; a second mkdir of the same path fails (EEXIST -> nil),
-- which is exactly the signal plenary.path:mkdir() branches on.
assert(uv.fs_mkdir("{dir}/sub", tonumber("755", 8)))
assert(not uv.fs_mkdir("{dir}/sub", tonumber("755", 8)), "re-mkdir should fail")
assert((uv.fs_stat("{dir}/sub") or {{}}).type == "directory")

-- create a file, rename it, confirm old gone / new present, then unlink it.
local fd = assert(uv.fs_open("{dir}/sub/a.txt", "w", tonumber("644", 8)))
uv.fs_close(fd)
assert(uv.fs_rename("{dir}/sub/a.txt", "{dir}/sub/b.txt"))
assert(uv.fs_stat("{dir}/sub/a.txt") == nil, "old name should be gone")
assert(uv.fs_stat("{dir}/sub/b.txt") ~= nil, "new name should exist")
assert(uv.fs_unlink("{dir}/sub/b.txt"))
assert(uv.fs_stat("{dir}/sub/b.txt") == nil, "unlinked file should be gone")

-- rmdir the now-empty dir.
assert(uv.fs_rmdir("{dir}/sub"))
assert(uv.fs_stat("{dir}/sub") == nil, "rmdir'd dir should be gone")

return "ok"
"#
        ))
        .expect("harness ran")
        .as_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(report, "ok");
}

#[test]
fn fs_scandir_iterates_entries_and_fs_access_gates_them() {
    let dir = scratch("scandir");
    std::fs::write(format!("{dir}/a.txt"), "a").unwrap();
    std::fs::write(format!("{dir}/b.txt"), "b").unwrap();
    std::fs::create_dir(format!("{dir}/sub")).unwrap();
    let rt = LuaRuntime::new(vec![]).expect("runtime");
    let report = rt
        .eval_to_value(&format!(
            r#"
local uv = vim.uv

-- fs_access: a real, traversable directory is accessible for "X"; a path that
-- does not exist is not. (plenary.scandir gates each base path on this.)
assert(uv.fs_access("{dir}", "X") == true, "scratch dir should be X-accessible")
assert(uv.fs_access("{dir}/nope", "X") == false, "missing path should not be accessible")

-- fs_scandir + fs_scandir_next: enumerate the directory, collecting name->type.
-- The handle iterates until fs_scandir_next returns nil.
local fd = assert(uv.fs_scandir("{dir}"))
local seen = {{}}
while true do
  local name, typ = uv.fs_scandir_next(fd)
  if name == nil then break end
  seen[name] = typ
end
assert(seen["a.txt"] == "file", "a.txt should be a file, got " .. tostring(seen["a.txt"]))
assert(seen["b.txt"] == "file", "b.txt should be a file, got " .. tostring(seen["b.txt"]))
assert(seen["sub"] == "directory", "sub should be a directory, got " .. tostring(seen["sub"]))

-- scandir of a non-directory fails (nil + err), not a crash.
assert(uv.fs_scandir("{dir}/a.txt") == nil, "scandir of a file should fail")

return "ok"
"#
        ))
        .expect("harness ran")
        .as_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(report, "ok");
}

/// The lazy.nvim default install location of plenary, or `None` when it isn't
/// present — the real-plenary tests skip in that case, the same way
/// `lspconfig_configs.rs` skips on a missing submodule, so CI without plenary
/// stays green while a developer who has it gets the real proof.
fn plenary_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".local/share/nvim/lazy/plenary.nvim");
    if dir.join("lua/plenary/path.lua").is_file() {
        Some(dir)
    } else {
        eprintln!("skipping: plenary.nvim not installed at {}", dir.display());
        None
    }
}

/// The real `plenary.path` module loaded onto the runtimepath and driven through
/// a write → read → exists → is_dir round trip.
#[test]
fn real_plenary_path_loads_and_round_trips() {
    let Some(plenary) = plenary_dir() else {
        return;
    };
    let dir = scratch("plenary");
    let rt = LuaRuntime::new(vec![plenary]).expect("runtime");
    let report = rt
        .eval_to_value(&format!(
            r##"
local Path = require("plenary.path")

local p = Path:new("{dir}/note.md")
assert(not p:exists(), "fresh path should not exist yet")
p:write("# hi\nfrom plenary\n", "w")
assert(p:exists(), "after :write the path should exist")
assert(p:is_file(), ":is_file should be true for a written file")
assert(not p:is_dir(), ":is_dir should be false for a file")
local back = p:read()
assert(back == "# hi\nfrom plenary\n", "Path:read round-trip mismatch: " .. vim.inspect(back))

local d = Path:new("{dir}")
assert(d:is_dir(), "the scratch dir should report is_dir")

return "ok"
"##
        ))
        .expect("plenary harness ran")
        .as_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(report, "ok");
}

/// The real `plenary.scandir` module, driven against a small on-disk tree. This
/// exercises the directory-iteration trio (`fs_access` + `fs_scandir` +
/// `fs_scandir_next`) through the actual plugin's recursive `scan_dir`.
#[test]
fn real_plenary_scandir_walks_a_tree() {
    let Some(plenary) = plenary_dir() else {
        return;
    };
    let dir = scratch("scandir-plenary");
    // a small recursive tree: top-level a.txt + b.txt, and sub/c.txt.
    std::fs::write(format!("{dir}/a.txt"), "a").unwrap();
    std::fs::write(format!("{dir}/b.txt"), "b").unwrap();
    std::fs::create_dir(format!("{dir}/sub")).unwrap();
    std::fs::write(format!("{dir}/sub/c.txt"), "c").unwrap();

    let rt = LuaRuntime::new(vec![plenary]).expect("runtime");
    let report = rt
        .eval_to_value(&format!(
            r#"
local scan = require("plenary.scandir")

-- Recursive scan (the default): every file under the tree, absolute paths.
local files = scan.scan_dir("{dir}", {{ hidden = true }})
local found = {{}}
for _, f in ipairs(files) do
  found[f] = true
end

assert(found["{dir}/a.txt"], "a.txt should be found")
assert(found["{dir}/b.txt"], "b.txt should be found")
assert(found["{dir}/sub/c.txt"], "sub/c.txt should be found (recursive)")
assert(#files == 3, "expected exactly 3 files, got " .. #files .. ": " .. vim.inspect(files))

return "ok"
"#
        ))
        .expect("plenary scandir harness ran")
        .as_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(report, "ok");
}
