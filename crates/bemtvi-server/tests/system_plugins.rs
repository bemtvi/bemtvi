//! System-plugin tier (§A of the remote-connectors plan): local plugins the *client*
//! seeds into every session via `ServerInit::system_plugins`. A system plugin must
//!
//!   * load BEFORE `init.lua` (so a connector is present before any config line runs),
//!   * be ABSENT when the tier is empty (headless/`--lua` hermeticity), and
//!   * never shadow a user config module of the same name (config wins on `package.path`),
//!
//! and — because its dir is both spliced onto the runtimepath AND sourced in the
//! dedicated pre-init phase — must be sourced EXACTLY once (the later `source_plugins`
//! pass skips it).

use std::path::Path;

use bemtvi_server::{ServerInit, SystemPluginSpec};
use bemtvi_test_harness::{exec_lua, start_attached, temp_dir};

/// A minimal on-disk system plugin under `base/<name>`: its `plugin/init.lua` runs a
/// registration script (appends to `_G.LOAD_ORDER`, bumps `_G.SYS_COUNT`, sets a flag),
/// and its `lua/<name>.lua` is a require-able module. Returns the plugin dir.
fn write_system_plugin(base: &Path, name: &str) -> std::path::PathBuf {
    let dir = base.join(name);
    std::fs::create_dir_all(dir.join("plugin")).unwrap();
    std::fs::create_dir_all(dir.join("lua")).unwrap();
    std::fs::write(
        dir.join("plugin").join("init.lua"),
        "_G.SYS_PLUGIN_LOADED = true\n\
         _G.SYS_COUNT = (_G.SYS_COUNT or 0) + 1\n\
         _G.LOAD_ORDER = (_G.LOAD_ORDER or \"\") .. \"sys;\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("lua").join(format!("{name}.lua")),
        "return { from = \"system\" }\n",
    )
    .unwrap();
    dir
}

#[tokio::test]
async fn a_system_plugin_loads_before_init_lua_and_is_require_able() {
    let base = temp_dir("system_plugin_loads");
    let cfg = base.join("config");
    std::fs::create_dir_all(cfg.join("lua")).unwrap();

    let plugdir = write_system_plugin(&base, "myconnector");

    // init.lua observes the tier: the system plugin's plugin/ script has already run
    // (LOAD_ORDER starts with "sys;"), its lua/ module resolves, and the tier registry
    // lists it. We record init's own position in LOAD_ORDER to prove ordering.
    std::fs::write(
        cfg.join("init.lua"),
        "_G.LOAD_ORDER = (_G.LOAD_ORDER or \"\") .. \"init;\"\n\
         _G.SEEN_AT_INIT = _G.SYS_PLUGIN_LOADED\n\
         _G.MOD_AT_INIT = require(\"myconnector\").from\n",
    )
    .unwrap();

    let si = ServerInit {
        config_dir: Some(cfg.clone()),
        runtimepath: vec![cfg.clone()],
        system_plugins: vec![SystemPluginSpec {
            name: "myconnector".into(),
            dir: plugdir.clone(),
        }],
        ..Default::default()
    };
    let (rpc, _incoming) = start_attached(si, 80, 24).await;

    // The system plugin's plugin/ script ran…
    assert_eq!(
        exec_lua(&rpc, "return _G.SYS_PLUGIN_LOADED == true")
            .await
            .as_bool(),
        Some(true),
        "the system plugin's plugin/ script must run",
    );
    // …BEFORE init.lua (system sourced first, then init sourced).
    assert_eq!(
        exec_lua(&rpc, "return _G.LOAD_ORDER").await.as_str(),
        Some("sys;init;"),
        "the system plugin must load before init.lua",
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.SEEN_AT_INIT == true")
            .await
            .as_bool(),
        Some(true),
        "init.lua must see the system plugin already loaded",
    );
    // Its lua/ module is require-able from init.lua.
    assert_eq!(
        exec_lua(&rpc, "return _G.MOD_AT_INIT").await.as_str(),
        Some("system"),
        "the system plugin's lua/ module must be require-able",
    );
    // It is recorded in the tier registry (not the managed spec set).
    assert_eq!(
        exec_lua(&rpc, "return btv.plugins._system['myconnector'] ~= nil")
            .await
            .as_bool(),
        Some(true),
        "the system plugin must be registered in the tier",
    );
    assert_eq!(
        exec_lua(&rpc, "return btv.plugins._specs['myconnector'] == nil")
            .await
            .as_bool(),
        Some(true),
        "a system plugin must NOT leak into the managed spec set (sync/clean must ignore it)",
    );
    // Sourced EXACTLY once — the later runtimepath source_plugins pass skipped it.
    assert_eq!(
        exec_lua(&rpc, "return _G.SYS_COUNT").await.as_u64(),
        Some(1),
        "a system plugin must be sourced exactly once (no double-source)",
    );
}

#[tokio::test]
async fn a_system_plugin_is_absent_with_the_default_empty_tier() {
    let base = temp_dir("system_plugin_absent");
    let cfg = base.join("config");
    std::fs::create_dir_all(cfg.join("lua")).unwrap();
    // The plugin exists on disk but is NOT threaded into the tier.
    let _plugdir = write_system_plugin(&base, "myconnector");
    std::fs::write(cfg.join("init.lua"), "_G.OK = true\n").unwrap();

    let si = ServerInit {
        config_dir: Some(cfg.clone()),
        runtimepath: vec![cfg.clone()],
        // system_plugins left empty (the default) — the hermetic path.
        ..Default::default()
    };
    let (rpc, _incoming) = start_attached(si, 80, 24).await;

    assert!(
        exec_lua(&rpc, "return _G.SYS_PLUGIN_LOADED").await.is_nil(),
        "with an empty tier no system plugin loads",
    );
    assert_eq!(
        exec_lua(&rpc, "return #btv.plugins.list_system()")
            .await
            .as_u64(),
        Some(0),
        "the tier is empty",
    );
}

#[tokio::test]
async fn a_config_module_shadows_a_same_named_system_module() {
    let base = temp_dir("system_plugin_shadow");
    let cfg = base.join("config");
    std::fs::create_dir_all(cfg.join("lua")).unwrap();

    // A system plugin shipping `lua/shared.lua` — the collision that must NOT hijack the
    // user's own `require("shared")`.
    let plugdir = base.join("sysplug");
    std::fs::create_dir_all(plugdir.join("plugin")).unwrap();
    std::fs::create_dir_all(plugdir.join("lua")).unwrap();
    std::fs::write(
        plugdir.join("plugin").join("init.lua"),
        "_G.SYS_RAN = true\n",
    )
    .unwrap();
    std::fs::write(
        plugdir.join("lua").join("shared.lua"),
        "return \"system\"\n",
    )
    .unwrap();

    // The user's own config module of the same name — this is what require must resolve to.
    std::fs::write(cfg.join("lua").join("shared.lua"), "return \"config\"\n").unwrap();
    std::fs::write(cfg.join("init.lua"), "_G.RESOLVED = require(\"shared\")\n").unwrap();

    let si = ServerInit {
        config_dir: Some(cfg.clone()),
        runtimepath: vec![cfg.clone()],
        system_plugins: vec![SystemPluginSpec {
            name: "sysplug".into(),
            dir: plugdir.clone(),
        }],
        ..Default::default()
    };
    let (rpc, _incoming) = start_attached(si, 80, 24).await;

    // The system plugin DID load (its plugin/ ran)…
    assert_eq!(
        exec_lua(&rpc, "return _G.SYS_RAN == true").await.as_bool(),
        Some(true),
        "the system plugin should still load",
    );
    // …but the user's config/lua/shared.lua wins the require, not the system module.
    assert_eq!(
        exec_lua(&rpc, "return _G.RESOLVED").await.as_str(),
        Some("config"),
        "a config module must shadow a same-named system-plugin module",
    );
}
