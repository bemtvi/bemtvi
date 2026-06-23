//! Materialize a fetched [`RemoteConfigBundle`] onto a local cache so a remote session
//! runs the **daemon's** config + plugins *locally* — Phase 2 of
//! `docs/plans/2026-06-23-remote-config-and-plugins.md`.
//!
//! Lua's `require` / runtimepath / runtime-file lookups are synchronous and cannot
//! await the daemon, so the edit-host can't load config straight off the wire. Instead
//! it mirrors the bundle's files into a local cache dir — rebasing each daemon-absolute
//! path under the cache root — and points `config_dir` / `runtimepath` at that copy.
//! Every existing synchronous Lua path (init.lua sourcing, plugin discovery, `require`,
//! `nvim_get_runtime_file`) then resolves against fetched files with no awaiting.
//!
//! The cache is **per process** and **rewritten fresh on every connect** (the chosen
//! freshness policy): a stale or removed remote file never lingers, and two concurrent
//! remote sessions don't clobber each other.

use std::path::{Component, Path, PathBuf};

use crate::RemoteConfigBundle;

/// Mirror `bundle`'s files into this process's remote-config cache and return the
/// rebased local `(config_dir, runtimepath)` to feed into [`ServerInit`](crate::ServerInit).
/// The cache root is `$XDG_CACHE_HOME/nxvim/remote/<pid>` (else `$HOME/.cache/…`); a
/// failure to resolve it is loud (a remote session with no place to stage its config is
/// not silently downgraded to "no config").
pub fn materialize_remote_config(
    bundle: RemoteConfigBundle,
) -> std::io::Result<(Option<PathBuf>, Vec<PathBuf>)> {
    let cache_root = remote_cache_root()?;
    materialize_remote_config_into(&cache_root, bundle)
}

/// [`materialize_remote_config`] against an explicit `cache_root` (the seam the tests
/// drive over a temp dir). Clears any prior contents first — fresh every connect — then
/// writes each fetched file at its rebased path and returns the rebased roots.
pub fn materialize_remote_config_into(
    cache_root: &Path,
    bundle: RemoteConfigBundle,
) -> std::io::Result<(Option<PathBuf>, Vec<PathBuf>)> {
    // Fresh every connect: a removed remote file must not survive as a stale local copy.
    if cache_root.exists() {
        std::fs::remove_dir_all(cache_root)?;
    }
    std::fs::create_dir_all(cache_root)?;

    for (remote_path, bytes) in &bundle.files {
        let local = rebase(cache_root, remote_path);
        if let Some(parent) = local.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&local, bytes)?;
    }

    let config_dir = bundle.config_dir.as_deref().map(|p| rebase(cache_root, p));
    let runtimepath = bundle
        .runtimepath
        .iter()
        .map(|p| rebase(cache_root, p))
        .collect();
    Ok((config_dir, runtimepath))
}

/// Mirror a daemon-absolute `remote` path under `cache_root`: keep only the path's
/// `Normal` components (dropping the root, any drive prefix, and — defensively — any
/// `.`/`..`), so the result always stays *inside* the cache root even if the daemon
/// ever sent a malformed path. `/home/u/.config/nxvim/init.lua` →
/// `<cache_root>/home/u/.config/nxvim/init.lua`.
fn rebase(cache_root: &Path, remote: &str) -> PathBuf {
    let mut local = cache_root.to_path_buf();
    for comp in Path::new(remote).components() {
        if let Component::Normal(part) = comp {
            local.push(part);
        }
    }
    local
}

/// The per-process remote-config cache dir: `$XDG_CACHE_HOME/nxvim/remote/<pid>`, else
/// `$HOME/.cache/nxvim/remote/<pid>`. Errors loudly if neither resolves.
fn remote_cache_root() -> std::io::Result<PathBuf> {
    let base = if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(xdg)
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".cache")
    } else {
        return Err(std::io::Error::other(
            "no XDG_CACHE_HOME or HOME to stage the remote config cache",
        ));
    };
    Ok(base
        .join("nxvim")
        .join("remote")
        .join(std::process::id().to_string()))
}
