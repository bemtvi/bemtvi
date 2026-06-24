//! Materializing a fetched config bundle onto a local cache (Phase 2 of
//! `docs/plans/2026-06-23-remote-config-and-plugins.md`).
//!
//! The wire fetch (Phase 1, `daemon_config.rs`) yields a [`RemoteConfigBundle`] of the
//! daemon's absolute paths + bytes. These tests drive `materialize_remote_config_into`
//! over a temp cache and prove it mirrors every file at a *rebased* local path and
//! returns `config_dir`/`runtimepath` pointing at that copy — the local roots a remote
//! session then feeds into `ServerInit` so Lua's synchronous loaders resolve fetched
//! files. A second test pins the "fresh every connect" policy: a removed remote file
//! does not linger.

use nxvim_server::{materialize_remote_config_into, RemoteConfigBundle};
use nxvim_test_harness::temp_dir;

/// A bundle's files land under the cache at their daemon-absolute path (minus the
/// root), and `config_dir`/`runtimepath` are rebased onto the same copy — so the
/// rebased plugin root resolves a real local file.
#[test]
fn materialize_mirrors_files_and_rebases_the_roots() {
    let cache = temp_dir("materialize_cache");
    let bundle = RemoteConfigBundle {
        config_dir: Some("/remote/home/.config/nxvim".to_string()),
        runtimepath: vec![
            "/remote/home/.config/nxvim".to_string(),
            "/remote/home/.config/nxvim/pack/vendor/start/myplugin".to_string(),
        ],
        files: vec![
            (
                "/remote/home/.config/nxvim/init.lua".to_string(),
                b"nx.o.tabstop = 7\n".to_vec(),
            ),
            (
                "/remote/home/.config/nxvim/lua/mymod.lua".to_string(),
                b"return 42\n".to_vec(),
            ),
            (
                "/remote/home/.config/nxvim/pack/vendor/start/myplugin/plugin/myplugin.lua"
                    .to_string(),
                b"-- the plugin\n".to_vec(),
            ),
        ],
        ts_languages: Vec::new(),
        cwd: None,
    };

    let (config_dir, runtimepath) = materialize_remote_config_into(&cache, bundle).unwrap();

    // config_dir is rebased under the cache root (the daemon's absolute path mirrored).
    let want_cfg = cache.join("remote/home/.config/nxvim");
    assert_eq!(
        config_dir.as_deref(),
        Some(want_cfg.as_path()),
        "config_dir rebases onto the local cache"
    );

    // The fetched files exist locally at their rebased paths, with their bytes — this is
    // what a synchronous `require`/init.lua source reads.
    assert_eq!(
        std::fs::read_to_string(want_cfg.join("init.lua")).unwrap(),
        "nx.o.tabstop = 7\n",
        "init.lua is materialized"
    );
    assert_eq!(
        std::fs::read_to_string(want_cfg.join("lua/mymod.lua")).unwrap(),
        "return 42\n",
        "a require-able lua/ module is materialized"
    );

    // runtimepath is rebased entry-for-entry; the plugin root resolves a real file.
    assert_eq!(
        runtimepath,
        vec![
            want_cfg.clone(),
            cache.join("remote/home/.config/nxvim/pack/vendor/start/myplugin"),
        ],
        "runtimepath rebases onto the cache, order preserved"
    );
    assert!(
        runtimepath[1].join("plugin/myplugin.lua").exists(),
        "the rebased plugin root resolves its materialized plugin/ script"
    );
}

/// Fresh every connect: re-materializing into the same cache drops files the new
/// bundle no longer carries (a removed remote file must not survive locally).
#[test]
fn materialize_is_fresh_each_connect() {
    let cache = temp_dir("materialize_fresh");

    let first = RemoteConfigBundle {
        config_dir: Some("/r/cfg".to_string()),
        runtimepath: vec!["/r/cfg".to_string()],
        files: vec![("/r/cfg/old.lua".to_string(), b"old\n".to_vec())],
        ts_languages: Vec::new(),
        cwd: None,
    };
    materialize_remote_config_into(&cache, first).unwrap();
    assert!(cache.join("r/cfg/old.lua").exists());

    let second = RemoteConfigBundle {
        config_dir: Some("/r/cfg".to_string()),
        runtimepath: vec!["/r/cfg".to_string()],
        files: vec![("/r/cfg/new.lua".to_string(), b"new\n".to_vec())],
        ts_languages: Vec::new(),
        cwd: None,
    };
    materialize_remote_config_into(&cache, second).unwrap();

    assert!(
        cache.join("r/cfg/new.lua").exists(),
        "the new bundle's file is materialized"
    );
    assert!(
        !cache.join("r/cfg/old.lua").exists(),
        "a file dropped from the remote does not linger in the cache"
    );
}
