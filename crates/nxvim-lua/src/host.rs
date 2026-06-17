//! Host primitives the Lua bridge stands on: runtime-file globbing across the
//! runtimepath and the standard-path math. Pure Rust (no `mlua` types), called from
//! [`crate::install`] and [`crate::runtime`] (to seed `package.path`).

use std::path::{Path, PathBuf};

/// Full paths of the files matching `name` across `runtimepath`, the engine of
/// `nvim_get_runtime_file`. `name` is a runtimepath-relative path whose final
/// component may contain a single `*` glob; earlier components are matched
/// literally. Stops at the first hit when `!all`.
pub(crate) fn get_runtime_file(runtimepath: &[PathBuf], name: &str, all: bool) -> Vec<String> {
    let (dir_part, file_part) = name.rsplit_once('/').unwrap_or(("", name));
    let mut out = Vec::new();
    for rt in runtimepath {
        let base = if dir_part.is_empty() {
            rt.clone()
        } else {
            rt.join(dir_part)
        };
        if file_part.contains('*') {
            let Ok(entries) = std::fs::read_dir(&base) else {
                continue;
            };
            for entry in entries.flatten() {
                let fname = entry.file_name();
                if glob_match(file_part, &fname.to_string_lossy()) {
                    out.push(entry.path().to_string_lossy().into_owned());
                    if !all {
                        return out;
                    }
                }
            }
        } else {
            let full = base.join(file_part);
            if full.exists() {
                out.push(full.to_string_lossy().into_owned());
                if !all {
                    return out;
                }
            }
        }
    }
    out
}

/// Match a single path component against a glob with at most one `*` (the only
/// form `nvim_get_runtime_file` callers use, e.g. `lsp/*.lua`).
fn glob_match(pat: &str, name: &str) -> bool {
    match pat.split_once('*') {
        Some((pre, suf)) => {
            name.len() >= pre.len() + suf.len() && name.starts_with(pre) && name.ends_with(suf)
        }
        None => pat == name,
    }
}

/// Prepend each runtimepath entry's `lua/` directory to Lua's `package.path`,
/// so `require("foo")` finds `<rt>/lua/foo.lua` or `<rt>/lua/foo/init.lua`. The
/// stock `package.path` is kept as a suffix. No-op when the runtimepath is empty.
pub(crate) fn seed_package_path(lua: &mlua::Lua, runtimepath: &[PathBuf]) -> mlua::Result<()> {
    if runtimepath.is_empty() {
        return Ok(());
    }
    let mut patterns: Vec<String> = Vec::with_capacity(runtimepath.len() * 2);
    for rt in runtimepath {
        package_patterns_for(rt, &mut patterns);
    }
    prepend_package_path(lua, patterns)
}

/// The two `require` patterns a single runtimepath entry contributes —
/// `<dir>/lua/?.lua` and `<dir>/lua/?/init.lua` — pushed onto `out`. Shared by the
/// startup seed and the runtime [`seed_one_package_path`] so both spell the layout
/// identically.
fn package_patterns_for(dir: &Path, out: &mut Vec<String>) {
    let lua_dir = dir.join("lua");
    out.push(lua_dir.join("?.lua").to_string_lossy().into_owned());
    out.push(
        lua_dir
            .join("?")
            .join("init.lua")
            .to_string_lossy()
            .into_owned(),
    );
}

/// Prepend `patterns` to Lua's `package.path`, keeping the existing value as a
/// suffix. Empty `patterns` is a no-op.
fn prepend_package_path(lua: &mlua::Lua, patterns: Vec<String>) -> mlua::Result<()> {
    if patterns.is_empty() {
        return Ok(());
    }
    let package: mlua::Table = lua.globals().get("package")?;
    let existing: String = package.get("path").unwrap_or_default();
    let combined = if existing.is_empty() {
        patterns.join(";")
    } else {
        format!("{};{existing}", patterns.join(";"))
    };
    package.set("path", combined)?;
    Ok(())
}

/// Prepend one directory's `lua/` patterns to `package.path`, so its modules are
/// `require`-able. The runtime sibling of [`seed_package_path`] (which seeds the
/// whole runtimepath at startup): the package manager calls this — via the
/// `nx._add_rtp` bridge — when it installs a plugin mid-session.
pub(crate) fn seed_one_package_path(lua: &mlua::Lua, dir: &Path) -> mlua::Result<()> {
    let mut patterns = Vec::with_capacity(2);
    package_patterns_for(dir, &mut patterns);
    prepend_package_path(lua, patterns)
}

/// Resolve a `vim.fn.stdpath(what)` directory under an `nxvim` subdir, the way
/// neovim derives its standard paths from XDG (with `$HOME` fallbacks). `config`
/// additionally honors `$NXVIM_CONFIG`. Unknown `what` falls back to the cache
/// dir rather than erroring.
pub(crate) fn stdpath(what: &str) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = |var: &str, fallback: &str| -> PathBuf {
        if let Some(dir) = std::env::var_os(var) {
            PathBuf::from(dir).join("nxvim")
        } else if let Some(home) = &home {
            home.join(fallback).join("nxvim")
        } else {
            PathBuf::from("nxvim")
        }
    };
    let path = match what {
        "config" => std::env::var_os("NXVIM_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config")),
        "data" => xdg("XDG_DATA_HOME", ".local/share"),
        "state" => xdg("XDG_STATE_HOME", ".local/state"),
        _ => xdg("XDG_CACHE_HOME", ".cache"),
    };
    path.to_string_lossy().into_owned()
}
