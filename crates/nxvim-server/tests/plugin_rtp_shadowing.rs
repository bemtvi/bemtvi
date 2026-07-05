//! Regression: a loaded plugin must NOT shadow the user's own `require`-able config
//! modules on `package.path`.
//!
//! Repro of the reported "nx.plugins race — plugins get deleted" bug: init.lua splits
//! its plugin declarations across two `nx.plugins{}` calls — some inline, the rest in a
//! `lua/plugins.lua` pulled in with `require("plugins")` (the standard neovim idiom).
//! The first call eagerly loads its plugins, and each load runs `nx._add_rtp`, which
//! used to PREPEND the plugin's `lua/` dir onto `package.path` — ahead of the user's
//! own config dir. So if any eagerly-loaded plugin shipped a `lua/plugins.lua` (a
//! common module name), the later `require("plugins")` resolved to the PLUGIN's module
//! instead of the user's — and every plugin declared in the user's `plugins.lua`
//! silently never registered. To the user, that whole batch of plugins "disappeared"
//! (and, still on disk but no longer declared, a later `:PluginClean` would delete
//! them). The fix rebuilds `package.path` in runtimepath order (config dir first), so a
//! plugin can never outrank a user config module.

use std::path::Path;

use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, start_attached, temp_dir};

fn q(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

#[tokio::test]
async fn a_loaded_plugin_does_not_shadow_the_users_require_of_a_config_module() {
    let base = temp_dir("plug_rtp_shadow");
    let cfg = base.join("config");
    let root = base.join("data").join("plugins");
    std::fs::create_dir_all(cfg.join("lua")).unwrap();
    std::fs::create_dir_all(&root).unwrap();

    // A dev (`dir`) plugin that loads eagerly and happens to ship its OWN
    // `lua/plugins.lua` module — the collision that used to hijack the user's require.
    let plugdir = base.join("shadowplug");
    std::fs::create_dir_all(plugdir.join("lua")).unwrap();
    std::fs::write(
        plugdir.join("lua").join("plugins.lua"),
        "_G.PLUGIN_MODULE_WON = true\nreturn {}\n",
    )
    .unwrap();

    // The user's config module: this is the one `require("plugins")` must resolve to.
    // It declares a second plugin so we can also assert the declaration registered.
    std::fs::write(
        cfg.join("lua").join("plugins.lua"),
        "_G.USER_MODULE_WON = true\n\
         nx.plugins({ { dir = \"/no/such/dir\", name = \"declared-in-user-plugins\", enabled = false } })\n",
    )
    .unwrap();

    // init.lua: eagerly load the dir plugin (which prepends its lua/ historically),
    // THEN require the user's config module — the ordering that exposed the bug.
    std::fs::write(
        cfg.join("init.lua"),
        format!(
            "nx.plugins.setup_manager({{ root = \"{root}\" }})\n\
             nx.plugins({{ {{ dir = \"{plug}\", name = \"shadowplug\" }} }})\n\
             _G.REQ_OK, _G.REQ_ERR = pcall(require, \"plugins\")\n",
            root = q(&root),
            plug = q(&plugdir),
        ),
    )
    .unwrap();

    let si = ServerInit {
        config_dir: Some(cfg.clone()),
        // The real binary folds config_dir into the runtimepath (default_runtime);
        // that is what seeds package.path so require of a config module resolves.
        runtimepath: vec![cfg.clone()],
        ..Default::default()
    };
    let (rpc, _incoming) = start_attached(si, 80, 24).await;

    // require("plugins") resolved to the USER's config/lua/plugins.lua …
    assert_eq!(
        exec_lua(&rpc, "return _G.USER_MODULE_WON == true")
            .await
            .as_bool(),
        Some(true),
        "require(\"plugins\") must resolve to the user's config module; \
         req_ok={:?} req_err={:?}",
        exec_lua(&rpc, "return _G.REQ_OK").await,
        exec_lua(&rpc, "return tostring(_G.REQ_ERR)").await,
    );
    // … NOT the loaded plugin's lua/plugins.lua.
    assert_ne!(
        exec_lua(&rpc, "return _G.PLUGIN_MODULE_WON == true")
            .await
            .as_bool(),
        Some(true),
        "a loaded plugin's lua/plugins.lua must not shadow the user's require(\"plugins\")",
    );
    // The plugin declared inside the user's plugins.lua registered — it did not
    // silently vanish.
    assert_eq!(
        exec_lua(
            &rpc,
            "return nx.plugins._specs['declared-in-user-plugins'] ~= nil"
        )
        .await
        .as_bool(),
        Some(true),
        "plugins declared in the user's required config module must register",
    );
    // The dir plugin's own lua/ IS still on the path (it just sorts after the config
    // dir now), so the plugin itself remains require-able.
    assert_eq!(
        exec_lua(&rpc, "return nx.plugins._loaded['shadowplug'] == true")
            .await
            .as_bool(),
        Some(true),
        "the eagerly-loaded dir plugin should still load",
    );
}
