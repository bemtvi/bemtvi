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

/// Registry key holding the pristine (stock) `package.path` — everything Lua shipped
/// with, captured once before any runtimepath seeding. Every rebuild keeps it as the
/// suffix so the stock system paths always sort LAST, behind config + plugins.
const STOCK_PATH_KEY: &str = "nxvim.stock_package_path";

/// Point `package.path` at the runtimepath: each entry's `lua/` patterns in
/// runtimepath order (so the config dir, which is first, wins), followed by the stock
/// system paths. Rebuilt from scratch every call (from the full `runtimepath`), so a
/// plugin added mid-session lands in runtimepath order — AFTER the config dir — rather
/// than shadowing it. This mirrors neovim, where `require("foo")` finds the user's
/// `<config>/lua/foo.lua` before any plugin's `lua/foo.lua`. Idempotent (a rebuild
/// never duplicates entries). No-op on an empty runtimepath the first time (but the
/// stock is still captured, so a later add rebuilds correctly).
pub(crate) fn seed_package_path(lua: &mlua::Lua, runtimepath: &[PathBuf]) -> mlua::Result<()> {
    let package: mlua::Table = lua.globals().get("package")?;
    // Capture the stock path ONCE, before the first seed overwrites it, so every
    // rebuild appends the same untouched system tail.
    if lua
        .named_registry_value::<mlua::Value>(STOCK_PATH_KEY)?
        .is_nil()
    {
        let stock: String = package.get("path").unwrap_or_default();
        lua.set_named_registry_value(STOCK_PATH_KEY, stock)?;
    }
    let stock: String = lua.named_registry_value(STOCK_PATH_KEY).unwrap_or_default();

    let mut patterns: Vec<String> = Vec::with_capacity(runtimepath.len() * 2);
    for rt in runtimepath {
        package_patterns_for(rt, &mut patterns);
    }
    let combined = match (patterns.is_empty(), stock.is_empty()) {
        (true, _) => stock,
        (false, true) => patterns.join(";"),
        (false, false) => format!("{};{stock}", patterns.join(";")),
    };
    package.set("path", combined)?;
    Ok(())
}

/// The two `require` patterns a single runtimepath entry contributes —
/// `<dir>/lua/?.lua` and `<dir>/lua/?/init.lua` — pushed onto `out`. Shared by every
/// [`seed_package_path`] rebuild so they spell the layout identically.
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

/// Resolve a `vim.fn.stdpath(what)` directory under an `nxvim` subdir, the way
/// neovim derives its standard paths from XDG (with `$HOME` fallbacks). `config`
/// additionally honors `$NXVIM_CONFIG`. Unknown `what` falls back to the cache
/// dir rather than erroring.
pub fn stdpath(what: &str) -> String {
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
