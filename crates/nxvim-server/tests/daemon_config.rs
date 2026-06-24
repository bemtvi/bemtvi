//! The daemon wire protocol, *config* leg (remote config + plugins, Phase 1 of
//! `docs/plans/2026-06-23-remote-config-and-plugins.md`).
//!
//! Proves a single `config_bundle` request ships the **daemon's** whole config
//! surface — its `config_dir`, `runtimepath`, and every source file under those roots —
//! across a real wire (an in-process `tokio::io::duplex` standing in for the eventual
//! ssh/QUIC link to `nxvim --daemon`). This is the fetch half of "config and plugins
//! come from the remote, run locally"; Phase 2 materializes the bundle onto a local
//! cache and rebases the roots onto it.
//!
//! Faithful, not a no-op: the config lives in a temp dir the daemon resolves via
//! `NXVIM_CONFIG`, and the bundle's bytes can only have come from walking that tree
//! daemon-side. A native artifact (`parser/foo.so`) seeded into the plugin proves the
//! walk **skips** compiled binaries (tree-sitter is compiled locally on the client).
//!
//! Env-mutating, so the whole body holds the process-wide `serial_lock` and restores
//! the prior `NXVIM_CONFIG` / `NXVIM_RUNTIMEPATH`.

use std::path::PathBuf;

use nxvim_server::{connect_daemon, materialize_remote_config_into, RemoteConfig, ServerInit};
use nxvim_test_harness::{attach, exec_lua, serial_lock, spawn, temp_dir};

/// Save an env var, set it to `val` (or remove it when `None`), and restore the prior
/// value on drop — so a test can scope `NXVIM_CONFIG` to its temp tree without leaking
/// into the next (serialized) test.
struct EnvGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, val: Option<&std::path::Path>) -> EnvGuard {
        let prev = std::env::var_os(key);
        match val {
            Some(p) => std::env::set_var(key, p),
            None => std::env::remove_var(key),
        }
        EnvGuard { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

/// Populate `dir` as a config tree: an `init.lua`, a require-able `lua/mymod.lua`, a
/// packaged plugin under `pack/vendor/start/myplugin/`, and a native artifact the walk
/// must skip. Returns the plugin root (a discovered runtimepath entry).
fn populate_config(dir: &std::path::Path) -> PathBuf {
    std::fs::write(dir.join("init.lua"), "nx.o.tabstop = 7\n").unwrap();
    std::fs::create_dir_all(dir.join("lua")).unwrap();
    std::fs::write(dir.join("lua/mymod.lua"), "return 42\n").unwrap();

    let plugin = dir.join("pack/vendor/start/myplugin");
    std::fs::create_dir_all(plugin.join("plugin")).unwrap();
    std::fs::write(plugin.join("plugin/myplugin.lua"), "-- the plugin\n").unwrap();
    // A locally-compiled artifact that must NOT ride the bundle to a remote-arch client.
    std::fs::create_dir_all(plugin.join("parser")).unwrap();
    std::fs::write(plugin.join("parser/foo.so"), b"\x7fELF-not-portable").unwrap();
    plugin
}

/// Connect a [`RemoteConfig`] to a `serve_config_daemon` over an in-process duplex and
/// fetch the bundle — the wire round trip the real session makes at startup.
async fn fetch_over_wire() -> nxvim_server::RemoteConfigBundle {
    let (edit_host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (daemon_reader, daemon_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = nxvim_server::serve_config_daemon(daemon_reader, daemon_writer).await;
    });
    let (host_reader, host_writer) = tokio::io::split(edit_host_end);
    let remote = RemoteConfig::connect(host_reader, host_writer);
    remote.fetch().await.expect("config_bundle fetch")
}

/// The bundle carries the daemon's config dir, its discovered runtimepath (config dir
/// + the packaged plugin), and the source files under those roots — and **omits** the
/// native artifact.
#[tokio::test]
async fn config_bundle_ships_the_daemons_config_and_plugins() {
    let _g = serial_lock().lock().await;
    let cfg = temp_dir("daemon_config");
    let plugin = populate_config(&cfg);

    // Scope the daemon's config resolution to our temp tree; clear any inherited
    // runtimepath override so the bundle is exactly what this tree contains.
    let _c = EnvGuard::set("NXVIM_CONFIG", Some(&cfg));
    let _r = EnvGuard::set("NXVIM_RUNTIMEPATH", None);

    let bundle = fetch_over_wire().await;

    // The config dir is the daemon's, verbatim.
    assert_eq!(
        bundle.config_dir.as_deref(),
        Some(cfg.to_string_lossy().as_ref()),
        "the bundle reports the daemon's config dir"
    );

    // The runtimepath is the config dir followed by the discovered plugin root.
    assert_eq!(
        bundle.runtimepath,
        vec![
            cfg.to_string_lossy().into_owned(),
            plugin.to_string_lossy().into_owned(),
        ],
        "runtimepath = config dir + the packaged plugin (neovim's pack layout)"
    );

    // The bundle carries the daemon's cwd, which seeds the edit-host's `DirState` so a
    // remote session's `:pwd` / `:cd` / `getcwd` operate on the daemon's directory. The
    // daemon serves in-process here, so its cwd is this test process's cwd.
    assert_eq!(
        bundle.cwd.as_deref(),
        std::env::current_dir()
            .ok()
            .as_deref()
            .map(|p| p.to_string_lossy().into_owned())
            .as_deref(),
        "the bundle reports the daemon's working directory (remote-cwd seed)"
    );

    // Look a fetched file up by the path suffix it must carry, asserting its bytes.
    let find = |suffix: &str| -> Option<Vec<u8>> {
        bundle
            .files
            .iter()
            .find(|(p, _)| p.replace('\\', "/").ends_with(suffix))
            .map(|(_, b)| b.clone())
    };
    assert_eq!(
        find("init.lua").as_deref(),
        Some(b"nx.o.tabstop = 7\n".as_ref()),
        "init.lua is fetched with its bytes"
    );
    assert_eq!(
        find("lua/mymod.lua").as_deref(),
        Some(b"return 42\n".as_ref()),
        "a require-able lua/ module is fetched (so a remote `require` resolves)"
    );
    assert_eq!(
        find("plugin/myplugin.lua").as_deref(),
        Some(b"-- the plugin\n".as_ref()),
        "the packaged plugin's plugin/ script is fetched"
    );

    // The native artifact is skipped — tree-sitter parsers are compiled locally.
    assert!(
        bundle.files.iter().all(|(p, _)| !p.ends_with(".so")),
        "compiled `.so` artifacts must not ride the bundle (got {:?})",
        bundle.files.iter().map(|(p, _)| p).collect::<Vec<_>>()
    );
}

/// With no resolvable config dir, the bundle is empty (a `None` config dir, no files) —
/// not an error. A remote with no config is a normal, quiet outcome.
#[tokio::test]
async fn config_bundle_is_empty_when_the_daemon_has_no_config() {
    let _g = serial_lock().lock().await;
    // Point at a path that does not exist: it resolves as the config dir but the walk
    // finds nothing (a missing root is normal, not a loud error).
    let missing = temp_dir("daemon_config_missing").join("does-not-exist");
    let _c = EnvGuard::set("NXVIM_CONFIG", Some(&missing));
    let _r = EnvGuard::set("NXVIM_RUNTIMEPATH", None);

    let bundle = fetch_over_wire().await;

    assert_eq!(
        bundle.config_dir.as_deref(),
        Some(missing.to_string_lossy().as_ref()),
        "the (non-existent) config dir still resolves"
    );
    assert!(
        bundle.files.is_empty(),
        "a missing config tree yields no files, not an error"
    );
}

/// The bundle lists the daemon's installed tree-sitter parser languages, so the client
/// can compile the same set locally (parsers are native artifacts, never fetched). Seed
/// the daemon's data dir with fake `parser/<lang>.so` files — `installed_parsers` lists
/// by filename, so the languages cross the wire without a real compile.
#[tokio::test]
async fn config_bundle_lists_the_daemons_installed_treesitter_languages() {
    let _g = serial_lock().lock().await;
    let data = temp_dir("daemon_config_ts_data");
    std::fs::create_dir_all(data.join("parser")).unwrap();
    std::fs::write(data.join("parser/rust.so"), b"").unwrap();
    std::fs::write(data.join("parser/lua.so"), b"").unwrap();

    let cfg = temp_dir("daemon_config_ts_cfg"); // an empty config is fine here
    let _c = EnvGuard::set("NXVIM_CONFIG", Some(&cfg));
    let _r = EnvGuard::set("NXVIM_RUNTIMEPATH", None);
    let _d = EnvGuard::set("NXVIM_DATA_DIR", Some(&data));

    let bundle = fetch_over_wire().await;

    let mut langs = bundle.ts_languages.clone();
    langs.sort();
    assert_eq!(
        langs,
        vec!["lua".to_string(), "rust".to_string()],
        "the bundle lists the daemon's installed treesitter parsers"
    );
}

/// The whole chain, end to end: a real editor server started against a real in-process
/// daemon (`run_daemon_io`) loads the **daemon's** config + plugins and runs them
/// locally — the same fetch → materialize → source sequence `run_edit_host_session`
/// performs, minus the TUI.
///
/// Faithful, not a no-op: the daemon's config lives in temp dir A (`NXVIM_CONFIG`), the
/// edit-host materializes it into a *different* temp cache B, and the server's
/// `config_dir`/`runtimepath` point at B. The init.lua option, the `require`d module's
/// value, and the packaged plugin's global can only be present because their source
/// crossed the daemon wire from A into B — there is no local `~/.config/nxvim` in play.
#[tokio::test]
async fn a_daemon_session_loads_the_remotes_config_and_plugins() {
    let _g = serial_lock().lock().await;

    // The daemon's config tree (temp dir A): an option, a require-able module, a plugin.
    let cfg = temp_dir("e2e_remote_cfg");
    std::fs::write(
        cfg.join("init.lua"),
        "nx.o.tabstop = 7\n_G.MYMOD_VALUE = require('mymod').value()\n",
    )
    .unwrap();
    std::fs::create_dir_all(cfg.join("lua")).unwrap();
    std::fs::write(
        cfg.join("lua/mymod.lua"),
        "local M = {}\nfunction M.value() return 42 end\nreturn M\n",
    )
    .unwrap();
    let plugin = cfg.join("pack/vendor/start/myplugin/plugin");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(plugin.join("load.lua"), "_G.PLUGIN_LOADED = true\n").unwrap();

    let _c = EnvGuard::set("NXVIM_CONFIG", Some(&cfg));
    let _r = EnvGuard::set("NXVIM_RUNTIMEPATH", None);
    // Hermetic tree-sitter dir: an empty data dir means the daemon reports no installed
    // parsers, so the session triggers no background compiles (and never touches the dev
    // machine's real parser set).
    let _d = EnvGuard::set("NXVIM_DATA_DIR", Some(&temp_dir("e2e_remote_tsdata")));

    // Stand up a real daemon over an in-process duplex and connect the full edit-host
    // client (every seam over one link), exactly as the binary does over stdio.
    let (host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (d_reader, d_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = nxvim_server::run_daemon_io(d_reader, d_writer).await;
    });
    let (h_reader, h_writer) = tokio::io::split(host_end);
    let client = connect_daemon(h_reader, h_writer);

    // Fetch the daemon's config and materialize it into cache B — the rebased roots.
    let cache = temp_dir("e2e_remote_cache");
    let bundle = client.config.fetch().await.expect("config_bundle fetch");
    let (config_dir, runtimepath) =
        materialize_remote_config_into(&cache, bundle).expect("materialize");
    // The server reads its config from B, never from A.
    assert!(
        config_dir.as_deref().is_some_and(|p| p.starts_with(&cache)),
        "the server's config dir is the local cache, not the daemon's path"
    );

    // Start the editor against the materialized config + the daemon seams — the server
    // half of an edit-host session.
    let init = ServerInit {
        file: None,
        config_dir,
        runtimepath,
        host_fs: None,
        host_proc: Some(Box::new(client.host_proc)),
        host_fs_async: Some(Box::new(client.host_fs)),
        lsp_transport: Some(Box::new(client.lsp_transport)),
        fs_jobs: Some(client.fs_jobs),
        ..Default::default()
    };
    let (rpc, _incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    // init.lua ran: its distinctive option took effect.
    assert_eq!(
        exec_lua(&rpc, "return nx.o.tabstop").await.as_i64(),
        Some(7),
        "the remote init.lua set tabstop"
    );
    // `require` resolved against the materialized `lua/` tree.
    assert_eq!(
        exec_lua(&rpc, "return _G.MYMOD_VALUE").await.as_i64(),
        Some(42),
        "init.lua's require('mymod') resolved from the remote lua/ module"
    );
    // The packaged plugin's `plugin/` script was sourced from the runtimepath.
    assert_eq!(
        exec_lua(&rpc, "return _G.PLUGIN_LOADED").await.as_bool(),
        Some(true),
        "the remote plugin's plugin/ script ran"
    );
}
