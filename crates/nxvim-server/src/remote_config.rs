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

use rmpv::Value;

/// The daemon's config surface, decoded off a `config_bundle` reply: its `config_dir`,
/// `runtimepath`, and every source file under those roots as `(abspath, bytes)`. Paths
/// are the **daemon's** absolute paths; the edit-host mirrors the files onto a local
/// cache and rebases the roots onto it ([`materialize_remote_config_into`]) so a remote
/// session runs the daemon's config + plugins locally.
///
/// Lives here (not in the native-gated `daemon` module) so the materialize half — and the
/// wire decoder below — are available to the **wasm** edit-host too: the browser build
/// fetches the bundle over WebTransport in JS, then hands the re-encoded reply to Rust,
/// where [`decode_config_bundle_bytes`] reconstructs this struct and the same materialize
/// runs against emscripten's in-memory FS.
pub struct RemoteConfigBundle {
    /// The daemon's config dir (`None` if it resolved none — no remote config).
    pub config_dir: Option<String>,
    /// The daemon's runtimepath, in load order (config dir + discovered plugins).
    pub runtimepath: Vec<String>,
    /// Every fetched source file: its daemon-absolute path and its bytes.
    pub files: Vec<(String, Vec<u8>)>,
    /// The daemon's installed tree-sitter parser languages — the client auto-installs
    /// the same set locally (parsers are native, never fetched over the wire).
    pub ts_languages: Vec<String>,
    /// The daemon's working directory, to seed the edit-host's `DirState` so a remote
    /// session's `:pwd` / `getcwd` / `:cd` operate on the daemon's cwd. `None` when the
    /// daemon couldn't read it (or an older peer omitted it) — the edit-host keeps its
    /// own local cwd.
    pub cwd: Option<String>,
    /// The daemon's shada base dir, where a `Remote`-config session stages + syncs its
    /// shada over the fs seam. `None` when an older peer omitted it — remote shada is
    /// then unavailable (the session falls back to local shada).
    pub state_dir: Option<String>,
    /// The daemon's home directory, so a leading `~` in a file argument (`:e ~/x`)
    /// expands against the **daemon's** `$HOME` — the read lands on the daemon even
    /// though the core runs on the client. `None` when the daemon couldn't read it (or
    /// an older peer omitted it) — the edit-host then falls back to its own `$HOME`.
    pub home: Option<String>,
}

/// Decode a `config_bundle` reply (the inverse of the daemon's `encode_config_bundle`):
/// the `[config_dir?, [runtimepath…], [[abspath, bytes], …], [ts_lang…], cwd?, state_dir?,
/// home?]` array.
/// Any shape mismatch is a loud error string — never a silently-empty bundle that would
/// look like "the remote has no config". Shared by the native daemon client and the wasm
/// edit-host (via [`decode_config_bundle_bytes`]).
pub fn decode_config_bundle(v: Value) -> Result<RemoteConfigBundle, String> {
    let bad = |what: &str| format!("config_bundle: malformed {what}");
    let Value::Array(a) = v else {
        return Err(bad("reply"));
    };
    let mut it = a.into_iter();
    let config_dir = match it.next() {
        None | Some(Value::Nil) => None,
        Some(v) => Some(v.as_str().ok_or_else(|| bad("config_dir"))?.to_owned()),
    };
    let Some(Value::Array(rtp)) = it.next() else {
        return Err(bad("runtimepath"));
    };
    let runtimepath = rtp
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| bad("runtimepath entry"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let Some(Value::Array(raw_files)) = it.next() else {
        return Err(bad("files"));
    };
    let mut files = Vec::with_capacity(raw_files.len());
    for f in raw_files {
        let Value::Array(pair) = f else {
            return Err(bad("file entry"));
        };
        let mut pit = pair.into_iter();
        let path = pit.next().and_then(|v| v.as_str().map(str::to_owned));
        let bytes = match pit.next() {
            Some(Value::Binary(b)) => Some(b),
            _ => None,
        };
        match (path, bytes) {
            (Some(p), Some(b)) => files.push((p, b)),
            _ => return Err(bad("file entry")),
        }
    }
    // The installed tree-sitter languages; absent (an older peer) decodes as empty.
    let ts_languages = match it.next() {
        None | Some(Value::Nil) => Vec::new(),
        Some(Value::Array(langs)) => langs
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| bad("ts_languages entry"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(bad("ts_languages")),
    };
    // The daemon's cwd; absent (an older peer) decodes as `None` (keep the local cwd).
    let cwd = match it.next() {
        None | Some(Value::Nil) => None,
        Some(v) => Some(v.as_str().ok_or_else(|| bad("cwd"))?.to_owned()),
    };
    // The daemon's shada base dir; absent (an older peer) decodes as `None` (no remote
    // shada — the session falls back to local shada).
    let state_dir = match it.next() {
        None | Some(Value::Nil) => None,
        Some(v) => Some(v.as_str().ok_or_else(|| bad("state_dir"))?.to_owned()),
    };
    // The daemon's home dir; absent (an older peer) decodes as `None` (a leading `~`
    // then expands against the edit-host's own `$HOME`).
    let home = match it.next() {
        None | Some(Value::Nil) => None,
        Some(v) => Some(v.as_str().ok_or_else(|| bad("home"))?.to_owned()),
    };
    Ok(RemoteConfigBundle {
        config_dir,
        runtimepath,
        files,
        ts_languages,
        cwd,
        state_dir,
        home,
    })
}

/// Decode a `config_bundle` reply from its raw msgpack bytes — the wasm edit-host's entry
/// point. The browser's JS RPC client decodes the wire to a JS value, re-encodes it to
/// msgpack, and hands the bytes across the FFI; this reconstructs the [`RemoteConfigBundle`]
/// the native client gets from [`decode_config_bundle`].
pub fn decode_config_bundle_bytes(bytes: &[u8]) -> Result<RemoteConfigBundle, String> {
    let value = rmpv::decode::read_value(&mut &bytes[..])
        .map_err(|e| format!("config_bundle: undecodable msgpack: {e}"))?;
    decode_config_bundle(value)
}

/// Mirror `bundle`'s files into this process's remote-config cache and return the
/// rebased local `(config_dir, runtimepath)` to feed into [`ServerInit`](crate::ServerInit).
/// The cache root is `$XDG_CACHE_HOME/nxvim/remote/<pid>` (else `$HOME/.cache/…`); a
/// failure to resolve it is loud (a remote session with no place to stage its config is
/// not silently downgraded to "no config").
#[cfg(feature = "native")]
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
