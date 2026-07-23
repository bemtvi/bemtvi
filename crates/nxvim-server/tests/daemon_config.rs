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

use nxvim_server::{
    connect_daemon, materialize_remote_config_into, ConfigSource, DaemonClient, RemoteConfig,
    ServerInit,
};
use nxvim_test_harness::{
    attach, exec_lua, feed, lines, serial_lock, spawn, start_attached, temp_dir,
};

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

/// Connect a [`RemoteConfig`] to a `serve_config_daemon` over an in-process duplex —
/// the wire the real session makes at startup. The spawned daemon serves until the
/// returned [`RemoteConfig`]'s end drops, so the caller can `fetch`/`resolve` on it.
fn connect_over_wire() -> RemoteConfig {
    let (edit_host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (daemon_reader, daemon_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = nxvim_server::serve_config_daemon(daemon_reader, daemon_writer).await;
    });
    let (host_reader, host_writer) = tokio::io::split(edit_host_end);
    RemoteConfig::connect(host_reader, host_writer)
}

/// Fetch the bundle over a fresh wire. `include_files` is the full vs lite fetch — a
/// remote-config session asks for the files (`true`), a local-config session for only
/// the metadata (`false`).
async fn fetch_over_wire(include_files: bool) -> nxvim_server::RemoteConfigBundle {
    connect_over_wire()
        .fetch(include_files)
        .await
        .expect("config_bundle fetch")
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

    let bundle = fetch_over_wire(true).await;

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

    // The bundle also carries the daemon's home, the base a leading `~` in a file
    // argument expands against (`:e ~/x` reads on the daemon). Served in-process, so the
    // daemon's `$HOME` is this test process's `$HOME`.
    assert_eq!(
        bundle.home.as_deref(),
        std::env::var("HOME").ok().as_deref(),
        "the bundle reports the daemon's home (the `~`-expansion base)"
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

    let bundle = fetch_over_wire(true).await;

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

    let bundle = fetch_over_wire(true).await;

    let mut langs = bundle.ts_languages.clone();
    langs.sort();
    assert_eq!(
        langs,
        vec!["lua".to_string(), "rust".to_string()],
        "the bundle lists the daemon's installed treesitter parsers"
    );
}

/// The **lite** fetch (`include_files = false`, a `ConfigSource::Local` session) skips
/// the file walk: the daemon still reports its config dir / cwd / parser set (the
/// metadata a local-config session needs to seed `DirState` + parsers), but ships **no**
/// file bytes — a local session runs the client's own config, so transferring the
/// daemon's tree would be wasted.
#[tokio::test]
async fn config_bundle_lite_fetch_skips_the_files() {
    let _g = serial_lock().lock().await;
    let cfg = temp_dir("daemon_config_lite");
    populate_config(&cfg);
    let _c = EnvGuard::set("NXVIM_CONFIG", Some(&cfg));
    let _r = EnvGuard::set("NXVIM_RUNTIMEPATH", None);
    let _d = EnvGuard::set("NXVIM_DATA_DIR", Some(&temp_dir("daemon_config_lite_data")));

    let bundle = fetch_over_wire(false).await;

    // The cheap metadata still crosses (the local session seeds cwd from it).
    assert_eq!(
        bundle.config_dir.as_deref(),
        Some(cfg.to_string_lossy().as_ref()),
        "the lite fetch still reports the daemon's config dir"
    );
    assert!(
        bundle.cwd.is_some(),
        "the lite fetch still reports the daemon's cwd (the remote-cwd seed)"
    );
    // But the file bytes — the expensive part — are omitted entirely. The full fetch
    // (asserted above) carries init.lua / lua/ / plugin/; the lite one carries none.
    assert!(
        bundle.files.is_empty(),
        "the lite fetch ships no file bytes (got {:?})",
        bundle.files.iter().map(|(p, _)| p).collect::<Vec<_>>()
    );
}

/// `RemoteConfig::resolve(ConfigSource::Local)` runs **this machine's** config
/// ([`default_runtime`]) rather than materializing the daemon's, while still seeding the
/// daemon's cwd so relative paths resolve on the remote disk (the buffers / fs stay on
/// the daemon in every mode). The config dir is the local one verbatim — never a
/// remote-config cache path.
#[tokio::test]
async fn resolve_local_runs_local_config_and_seeds_remote_cwd() {
    let _g = serial_lock().lock().await;
    let cfg = temp_dir("daemon_resolve_local");
    populate_config(&cfg);
    let _c = EnvGuard::set("NXVIM_CONFIG", Some(&cfg));
    let _r = EnvGuard::set("NXVIM_RUNTIMEPATH", None);
    let _d = EnvGuard::set(
        "NXVIM_DATA_DIR",
        Some(&temp_dir("daemon_resolve_local_data")),
    );

    let resolved = connect_over_wire()
        .resolve(ConfigSource::Local)
        .await
        .expect("resolve local");

    // Local mode sources `init.lua` from the local config dir, verbatim — not a
    // materialized cache copy (which would live under a `…/remote/<pid>/…` path).
    assert_eq!(
        resolved.config_dir.as_deref(),
        Some(cfg.as_path()),
        "local mode runs the local config dir (no materialize)"
    );
    // The daemon's cwd is still seeded (buffers/fs are remote even with local config).
    assert_eq!(
        resolved.remote_cwd,
        std::env::current_dir().ok(),
        "local mode still seeds the daemon's cwd for relative-path resolution"
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
    let bundle = client
        .config
        .fetch(true)
        .await
        .expect("config_bundle fetch");
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

/// Stand up a real daemon (`run_daemon_io`) over an in-process duplex and connect the
/// full edit-host client — every host seam over one link, as the binary does over stdio.
/// The daemon serves against the real disk (scoped to the test's temp env), so a
/// `Remote`-config session's shada lands under the daemon's `shada_dir()`.
fn spawn_daemon_client() -> DaemonClient {
    let (host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (d_reader, d_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = nxvim_server::run_daemon_io(d_reader, d_writer).await;
    });
    let (h_reader, h_writer) = tokio::io::split(host_end);
    connect_daemon(h_reader, h_writer)
}

/// Drain the client's incoming channel until it closes — which happens only after the
/// server thread fully returns from `run_server`, i.e. *after* the clean-exit flush AND
/// the awaited remote shada upload. So awaiting this is a reliable "the daemon has the
/// uploaded shada" barrier, with no reliance on wall-clock timing.
async fn drain_until_exit(mut incoming: tokio::sync::mpsc::UnboundedReceiver<nxvim_rpc::Incoming>) {
    while incoming.recv().await.is_some() {}
}

/// Build the [`ServerInit`] for a `Remote`-config daemon session against `client`: resolve
/// the remote config + the on-daemon shada (Approach A), exactly as the binary's
/// `run_edit_host_session` does. Returns the init, ready for `start_attached`.
async fn remote_session_init(
    client: DaemonClient,
    file: Option<String>,
    namespace: Option<&str>,
) -> ServerInit {
    let resolved = client
        .config
        .resolve(ConfigSource::Remote)
        .await
        .expect("resolve remote config");
    let (store, remote_shada) = nxvim_server::resolve_session_shada(
        &client.host_fs,
        ConfigSource::Remote,
        resolved.state_dir.as_deref(),
        namespace,
        nxvim_server::default_shada(),
    )
    .await;
    assert!(
        remote_shada.is_some(),
        "a Remote-config session must get an on-daemon shada target"
    );
    ServerInit {
        file,
        config_dir: resolved.config_dir,
        runtimepath: resolved.runtimepath,
        shada: Some(store),
        remote_shada,
        host_proc: Some(Box::new(client.host_proc)),
        host_fs_async: Some(Box::new(client.host_fs)),
        lsp_transport: Some(Box::new(client.lsp_transport)),
        fs_jobs: Some(client.fs_jobs),
        ..Default::default()
    }
}

/// End to end: a `Remote`-config session keeps its shada **on the daemon** (Approach A).
/// Session 1 yanks a word into register `a` and quits; the editor uploads its staged store
/// to the daemon over the fs seam. The file appears under the *daemon's* `shada_dir()` (the
/// temp state dir), and a second Remote-config session restores register `a` from it —
/// proving the bytes round-tripped through the daemon, not a local store.
#[tokio::test]
async fn remote_config_session_keeps_shada_on_the_daemon() {
    let _g = serial_lock().lock().await;
    // Scope every state/config/cache dir to temp trees: the daemon's shada (incl. our
    // remote-session file) lands under XDG_STATE_HOME; the client's materialize cache +
    // shada staging land under XDG_CACHE_HOME. Hermetic — the real dirs are untouched.
    let state = temp_dir("remote_shada_state");
    let _xs = EnvGuard::set("XDG_STATE_HOME", Some(&state));
    let _xc = EnvGuard::set("XDG_CACHE_HOME", Some(&temp_dir("remote_shada_cache")));
    let _c = EnvGuard::set("NXVIM_CONFIG", Some(&temp_dir("remote_shada_cfg")));
    let _r = EnvGuard::set("NXVIM_RUNTIMEPATH", None);
    let _d = EnvGuard::set("NXVIM_DATA_DIR", Some(&temp_dir("remote_shada_data")));

    // Session 1: type a line, yank "hello" into register `a`, quit. No file — buffers are
    // beside the point here; we exercise the shada sync, not the off-tick open.
    {
        let init = remote_session_init(spawn_daemon_client(), None, None).await;
        let (rpc, incoming) = start_attached(init, 80, 25).await;
        feed(&rpc, "ihello world<Esc>");
        feed(&rpc, "0\"ayiw");
        assert_eq!(lines(&rpc).await, vec!["hello world"]);
        feed(&rpc, ":qa!<CR>");
        // Barrier: the server has flushed AND uploaded the shada to the daemon.
        drain_until_exit(incoming).await;
    }

    // The shada landed on the *daemon*, in its remote shada dir — a per-instance `.redb`
    // store under `<shada_dir>/remote/`, not a local store and not in the daemon's own
    // native shada dir.
    let remote_dir = state.join("nxvim/shada/remote");
    let remote_stores = store_files_in(&remote_dir);
    assert_eq!(
        remote_stores.len(),
        1,
        "the Remote-config session synced exactly one store to the daemon, got {remote_stores:?}"
    );

    // Session 2: a fresh Remote-config session downloads the daemon's store and restores
    // register `a`, so `"ap` pastes "hello" — the state came back over the wire.
    {
        let init = remote_session_init(spawn_daemon_client(), None, None).await;
        let (rpc, _incoming) = start_attached(init, 80, 25).await;
        feed(&rpc, "\"ap");
        assert_eq!(
            lines(&rpc).await,
            vec!["hello"],
            "register `a` was restored from the daemon's shada"
        );
    }
}

/// The companion to the round trip above: a **Local**-config daemon session keeps its
/// shada local (a `.redb` store under the state dir) and writes **no** remote-session
/// file to the daemon — `local config → local shada`, the native default.
#[tokio::test]
async fn local_config_session_keeps_shada_local() {
    let _g = serial_lock().lock().await;
    let state = temp_dir("local_shada_state");
    let _xs = EnvGuard::set("XDG_STATE_HOME", Some(&state));
    let _xc = EnvGuard::set("XDG_CACHE_HOME", Some(&temp_dir("local_shada_cache")));
    let _c = EnvGuard::set("NXVIM_CONFIG", Some(&temp_dir("local_shada_cfg")));
    let _r = EnvGuard::set("NXVIM_RUNTIMEPATH", None);
    let _d = EnvGuard::set("NXVIM_DATA_DIR", Some(&temp_dir("local_shada_data")));

    let client = spawn_daemon_client();
    let resolved = client
        .config
        .resolve(ConfigSource::Local)
        .await
        .expect("resolve local config");
    let (store, remote_shada) = nxvim_server::resolve_session_shada(
        &client.host_fs,
        ConfigSource::Local,
        resolved.state_dir.as_deref(),
        None,
        nxvim_server::default_shada(),
    )
    .await;
    assert!(
        remote_shada.is_none(),
        "a Local-config session must NOT get an on-daemon shada target"
    );

    let init = ServerInit {
        config_dir: resolved.config_dir,
        runtimepath: resolved.runtimepath,
        shada: Some(store),
        remote_shada,
        host_proc: Some(Box::new(client.host_proc)),
        host_fs_async: Some(Box::new(client.host_fs)),
        lsp_transport: Some(Box::new(client.lsp_transport)),
        fs_jobs: Some(client.fs_jobs),
        ..Default::default()
    };
    let (rpc, incoming) = start_attached(init, 80, 25).await;
    feed(&rpc, "ihello<Esc>\"ayiw");
    feed(&rpc, ":qa!<CR>");
    drain_until_exit(incoming).await;

    let shada = state.join("nxvim/shada");
    assert!(
        !shada.join("remote").exists(),
        "a Local-config session must not create a remote shada dir on the daemon"
    );
    assert!(
        !store_files_in(&shada).is_empty(),
        "a Local-config session persists to a local .redb store under {shada:?}"
    );
}

/// The `.redb` store files directly under `dir` (non-recursive), for asserting on the
/// remote shada mirror's contents.
fn store_files_in(dir: &std::path::Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| nxvim_server::is_store_file(p))
        .collect()
}

/// Phase 3 (per-instance mirror): the remote shada dir **compacts + carries forward** like
/// the local store. Session 1 stores register `a` on the daemon; session 2 stores `b`, and
/// because it absorbs + removes session 1's sibling, the remote dir still holds exactly one
/// store — bounded by live sessions, not launches. Session 3 sees *both* registers, proving
/// the carry-forward merge round-tripped through the daemon.
#[tokio::test]
async fn remote_shada_compacts_and_merges_across_sessions() {
    let _g = serial_lock().lock().await;
    let state = temp_dir("remote_shada_compact_state");
    let _xs = EnvGuard::set("XDG_STATE_HOME", Some(&state));
    let _xc = EnvGuard::set(
        "XDG_CACHE_HOME",
        Some(&temp_dir("remote_shada_compact_cache")),
    );
    let _c = EnvGuard::set("NXVIM_CONFIG", Some(&temp_dir("remote_shada_compact_cfg")));
    let _r = EnvGuard::set("NXVIM_RUNTIMEPATH", None);
    let _d = EnvGuard::set(
        "NXVIM_DATA_DIR",
        Some(&temp_dir("remote_shada_compact_data")),
    );
    let remote_dir = state.join("nxvim/shada/remote");

    // Session 1: yank a line into register `a`, quit.
    {
        let init = remote_session_init(spawn_daemon_client(), None, None).await;
        let (rpc, incoming) = start_attached(init, 80, 25).await;
        feed(&rpc, "ihello world<Esc>\"ayy");
        assert_eq!(lines(&rpc).await, vec!["hello world"]);
        feed(&rpc, ":qa!<CR>");
        drain_until_exit(incoming).await;
    }
    assert_eq!(
        store_files_in(&remote_dir).len(),
        1,
        "one remote store after session 1"
    );

    // Session 2: yank a line into register `b`, quit. It absorbs session 1's store and
    // removes it on the daemon, so the count stays at one.
    {
        let init = remote_session_init(spawn_daemon_client(), None, None).await;
        let (rpc, incoming) = start_attached(init, 80, 25).await;
        feed(&rpc, "ifoo bar<Esc>\"byy");
        assert_eq!(lines(&rpc).await, vec!["foo bar"]);
        feed(&rpc, ":qa!<CR>");
        drain_until_exit(incoming).await;
    }
    assert_eq!(
        store_files_in(&remote_dir).len(),
        1,
        "still one remote store after session 2 — session 1's was compacted away"
    );

    // Session 3: both registers survived the carry-forward (session 1's `a` merged into
    // session 2's store, which also holds `b`).
    {
        let init = remote_session_init(spawn_daemon_client(), None, None).await;
        let (rpc, _incoming) = start_attached(init, 80, 25).await;
        feed(&rpc, "\"ap\"bp");
        assert_eq!(lines(&rpc).await, vec!["", "hello world", "foo bar"]);
    }
}

/// Phase 3: a namespace isolates a project's shada on the daemon under the daemon's *native*
/// `ns/<NS>/` dir — the SAME store a local editor on the daemon machine uses for that
/// namespace, so the two share it (a remote daemon workspace == an on-host session). A
/// register set in namespace `proj-a` is invisible in `proj-b`, and reconnecting `proj-a`
/// restores it — two projects on the same daemon never share marks/registers.
#[tokio::test]
async fn remote_shada_namespace_isolates_projects() {
    let _g = serial_lock().lock().await;
    let state = temp_dir("remote_shada_ns_state");
    let _xs = EnvGuard::set("XDG_STATE_HOME", Some(&state));
    let _xc = EnvGuard::set("XDG_CACHE_HOME", Some(&temp_dir("remote_shada_ns_cache")));
    let _c = EnvGuard::set("NXVIM_CONFIG", Some(&temp_dir("remote_shada_ns_cfg")));
    let _r = EnvGuard::set("NXVIM_RUNTIMEPATH", None);
    let _d = EnvGuard::set("NXVIM_DATA_DIR", Some(&temp_dir("remote_shada_ns_data")));
    // A namespaced remote session lands in the daemon's native `shada/ns/<NS>`, NOT under a
    // `remote/` sibling — that is what lets a local editor on the host share it.
    let ns_base = state.join("nxvim/shada/ns");

    // proj-a: store "alpha" in register `a`.
    {
        let init = remote_session_init(spawn_daemon_client(), None, Some("proj-a")).await;
        let (rpc, incoming) = start_attached(init, 80, 25).await;
        feed(&rpc, "ialpha<Esc>\"ayy");
        feed(&rpc, ":qa!<CR>");
        drain_until_exit(incoming).await;
    }

    // proj-b: register `a` is empty here (isolated) — pasting it changes nothing. Then
    // store "beta" in register `b`.
    {
        let init = remote_session_init(spawn_daemon_client(), None, Some("proj-b")).await;
        let (rpc, incoming) = start_attached(init, 80, 25).await;
        feed(&rpc, "\"ap");
        assert_eq!(
            lines(&rpc).await,
            vec![""],
            "register `a` from proj-a must NOT leak into proj-b"
        );
        feed(&rpc, "ibeta<Esc>\"byy");
        feed(&rpc, ":qa!<CR>");
        drain_until_exit(incoming).await;
    }

    // Each namespace has its own store dir on the daemon's native shada (shared with a
    // local on-host editor using the same namespace), not under a `remote/` sibling.
    assert_eq!(store_files_in(&ns_base.join("proj-a")).len(), 1);
    assert_eq!(store_files_in(&ns_base.join("proj-b")).len(), 1);
    assert!(
        !state.join("nxvim/shada/remote").exists(),
        "a namespaced remote session must not use the isolated remote/ subdir"
    );

    // Reconnect proj-a: register `a` ("alpha") is back, and `b` (proj-b's) is absent.
    {
        let init = remote_session_init(spawn_daemon_client(), None, Some("proj-a")).await;
        let (rpc, _incoming) = start_attached(init, 80, 25).await;
        feed(&rpc, "\"ap");
        assert_eq!(
            lines(&rpc).await,
            vec!["", "alpha"],
            "proj-a's `a` restored"
        );
        feed(&rpc, "\"bp");
        assert_eq!(
            lines(&rpc).await,
            vec!["", "alpha"],
            "proj-b's `b` must NOT appear in proj-a"
        );
    }
}
