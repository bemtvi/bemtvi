//! Host primitives the Lua bridge stands on: filesystem / process / `$PATH` /
//! glob helpers and the standard-path math. Pure Rust (no `mlua` types), called
//! from [`crate::install`] to back `vim.fn.*` / `vim.uv.*` and from
//! [`crate::runtime`] to seed `package.path`.

use std::path::PathBuf;

use crate::luafs::LuaFs;

/// Expand a shell-style glob (only `*` and `?`, matched per path component) into
/// the existing paths it matches. Enough for the `lib/python*/site-packages`-
/// style patterns the config files build; a relative pattern resolves against the
/// cwd. Backs `vim.fn.glob`. Directory listings + existence go through `fs` (the
/// project-facing seam), so a daemon session globs the *remote* tree; the relative
/// base is still the local cwd (the path-space split is a separate concern).
pub(crate) fn glob_paths(pattern: &str, fs: &dyn LuaFs) -> Vec<String> {
    let absolute = pattern.starts_with('/');
    let mut frontier = vec![if absolute {
        String::from("/")
    } else {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".into())
    }];
    for seg in pattern.split('/').filter(|s| !s.is_empty()) {
        let mut next = Vec::new();
        if seg.contains('*') || seg.contains('?') {
            for base in &frontier {
                if let Ok(entries) = fs.scandir(base) {
                    for entry in entries {
                        if wildcard_match(seg, &entry.name) {
                            next.push(join_path(base, &entry.name));
                        }
                    }
                }
            }
        } else {
            for base in &frontier {
                let cand = join_path(base, seg);
                // Existence test (empty access modes = F_OK).
                if fs.access(&cand, "") {
                    next.push(cand);
                }
            }
        }
        frontier = next;
    }
    frontier.sort();
    frontier
}

fn join_path(base: &str, name: &str) -> String {
    if base == "/" {
        format!("/{name}")
    } else if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// Glob match for one path component: `*` matches any run of non-`/` chars, `?`
/// any single char. A small backtracking matcher over bytes.
fn wildcard_match(pat: &str, s: &str) -> bool {
    let (pat, s) = (pat.as_bytes(), s.as_bytes());
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while si < s.len() {
        if pi < pat.len() && (pat[pi] == b'?' || pat[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < pat.len() && pat[pi] == b'*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

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
        let lua_dir = rt.join("lua");
        patterns.push(lua_dir.join("?.lua").to_string_lossy().into_owned());
        patterns.push(
            lua_dir
                .join("?")
                .join("init.lua")
                .to_string_lossy()
                .into_owned(),
        );
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

/// Resolve `mkdir`'s `prot` argument to a permission mode. Accepts an octal
/// string (`"0700"`, `"700"`) or a numeric mode; defaults to `0o755` (neovim's
/// default) when absent or unparseable.
pub(crate) fn parse_mode(prot: Option<mlua::Value>) -> u32 {
    const DEFAULT: u32 = 0o755;
    match prot {
        Some(mlua::Value::Integer(n)) => n as u32,
        Some(mlua::Value::Number(n)) => n as u32,
        Some(mlua::Value::String(s)) => s
            .to_str()
            .ok()
            .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0o"), 8).ok())
            .unwrap_or(DEFAULT),
        _ => DEFAULT,
    }
}

/// Create `path` (and parents) with permission `mode`. On Unix the mode is
/// applied to every directory created; elsewhere `mode` is ignored. Returns
/// whether the directory now exists.
pub(crate) fn create_dir_all_mode(path: &str, mode: u32) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(mode)
            .create(path)
            .is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        std::fs::create_dir_all(path).is_ok()
    }
}
