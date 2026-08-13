//! Host primitives the Lua bridge stands on: runtime-file globbing across the
//! runtimepath and the standard-path math. Pure Rust (no `mlua` types), called from
//! [`crate::install`] and [`crate::runtime`] (to seed `package.path`).

use std::path::{Path, PathBuf};

/// Full paths of the files matching `name` across `runtimepath`, the engine of
/// `nvim_get_runtime_file`. `name` is a runtimepath-relative path whose final
/// component may be a glob in the full [`bemtvi_core::glob`] dialect (`*`, `?`,
/// `[abc]`, `{a,b}`, several wildcards in one component); earlier components are
/// matched literally. Stops at the first hit when `!all`.
///
/// Within one runtimepath entry the directory listing is sorted, so `all = false`
/// picks a *deterministic* first match rather than whatever order the filesystem
/// happened to yield.
pub(crate) fn get_runtime_file(runtimepath: &[PathBuf], name: &str, all: bool) -> Vec<String> {
    let (dir_part, file_part) = name.rsplit_once('/').unwrap_or(("", name));
    // Compile once, outside the loop: the same glob is tested against every
    // runtimepath entry's listing, and the engine caches the compiled regex anyway.
    //
    // A pattern that carries a metacharacter but does not COMPILE (a reversed range,
    // say) falls through to the literal branch. That is not a swallowed error: a real
    // filename may contain `[`, `?` or `{`, so "not a valid glob" genuinely means
    // "this is a literal name" here.
    let glob = if bemtvi_core::glob::is_glob(file_part) {
        bemtvi_core::glob::compile(file_part, &bemtvi_core::glob::GlobOpts::default()).ok()
    } else {
        None
    };
    let mut out = Vec::new();
    for rt in runtimepath {
        let base = if dir_part.is_empty() {
            rt.clone()
        } else {
            rt.join(dir_part)
        };
        match &glob {
            Some(glob) => {
                let Ok(entries) = std::fs::read_dir(&base) else {
                    continue;
                };
                let mut hits: Vec<PathBuf> = entries
                    .flatten()
                    .filter(|e| glob.is_match(entry_name_bytes(&e.file_name())))
                    .map(|e| e.path())
                    .collect();
                hits.sort();
                for hit in hits {
                    out.push(hit.to_string_lossy().into_owned());
                    if !all {
                        return out;
                    }
                }
            }
            None => {
                let full = base.join(file_part);
                if full.exists() {
                    out.push(full.to_string_lossy().into_owned());
                    if !all {
                        return out;
                    }
                }
            }
        }
    }
    out
}

/// A directory entry's name as the bytes the filesystem actually holds, which is what
/// the glob engine matches. On unix a filename is arbitrary bytes, so matching its
/// lossy UTF-8 rendering would let a `?` or `*` mismatch a perfectly real file (every
/// invalid byte having become a 3-byte U+FFFD). Where `OsStr` offers no byte view,
/// lossy is all there is — but those platforms have no byte filenames either.
fn entry_name_bytes(name: &std::ffi::OsStr) -> std::borrow::Cow<'_, [u8]> {
    #[cfg(unix)]
    {
        std::borrow::Cow::Borrowed(std::os::unix::ffi::OsStrExt::as_bytes(name))
    }
    #[cfg(not(unix))]
    {
        match name.to_string_lossy() {
            std::borrow::Cow::Borrowed(s) => std::borrow::Cow::Borrowed(s.as_bytes()),
            std::borrow::Cow::Owned(s) => std::borrow::Cow::Owned(s.into_bytes()),
        }
    }
}

/// Registry key holding the pristine (stock) `package.path` — everything Lua shipped
/// with, captured once before any runtimepath seeding. Every rebuild keeps it as the
/// suffix so the stock system paths always sort LAST, behind config + plugins.
const STOCK_PATH_KEY: &str = "bemtvi.stock_package_path";

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
        // A non-string `package.path` here means a plugin already corrupted it before
        // the first seed — failing loud beats silently storing "" as the "stock" path,
        // which would permanently lose the system tail from every later rebuild.
        let stock: String = package.get("path")?;
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

/// Resolve a `vim.fn.stdpath(what)` directory under an `bemtvi` subdir, the way
/// neovim derives its standard paths from XDG (with `$HOME` fallbacks). `config`
/// additionally honors `$BEMTVI_CONFIG`. Unknown `what` falls back to the cache
/// dir rather than erroring.
pub fn stdpath(what: &str) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = |var: &str, fallback: &str| -> PathBuf {
        if let Some(dir) = std::env::var_os(var) {
            PathBuf::from(dir).join("bemtvi")
        } else if let Some(home) = &home {
            home.join(fallback).join("bemtvi")
        } else {
            PathBuf::from("bemtvi")
        }
    };
    let path = match what {
        "config" => std::env::var_os("BEMTVI_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config")),
        "data" => xdg("XDG_DATA_HOME", ".local/share"),
        "state" => xdg("XDG_STATE_HOME", ".local/state"),
        _ => xdg("XDG_CACHE_HOME", ".cache"),
    };
    path.to_string_lossy().into_owned()
}
