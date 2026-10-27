//! Startup-only user commands (`ServerInit::startup_user_commands_only`): once startup
//! ends (`PluginsLoaded`), a user command may be defined only by code from the config or
//! a plugin loaded at startup — told by the file the defining code is in, so a startup
//! plugin defining one later (on an event) still may — or by the client's
//! `nvim_exec_lua`. A plugin loaded later, typed `:lua`, a file sourced from elsewhere
//! and `:command` are refused with `E5802`.

use std::path::{Path, PathBuf};

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    attach, exec_lua, feed, lines, message_after, poll_true, spawn, temp_dir,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const INIT: &str = r#"
btv.command("FromInit", function() btv.g.from_init = 1 end)
vim.cmd("command FromExDuringStartup lua btv.g.ex_startup = 1")
btv.on("User", { pattern = "InitLater", callback = function()
  btv.command("InitLater", function() btv.g.init_later = 1 end)
end })
btv.on("PluginsLoaded", function() btv.g.loaded = true end)
"#;

const EARLY_PLUGIN: &str = r#"
btv.command("Early", function() btv.g.early = 1 end)
btv.on("User", { pattern = "EarlyLater", callback = function()
  btv.command("EarlyLater", function() btv.g.early_later = 1 end)
end })
"#;

struct Fixture {
    rpc: Rpc,
    incoming: UnboundedReceiver<Incoming>,
    /// A plugin loaded after startup (not on the runtimepath until a test adds it).
    lazy: PathBuf,
    /// A file outside every runtimepath entry.
    elsewhere: PathBuf,
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

async fn start(startup_only: bool) -> Fixture {
    let config = temp_dir("startup_cmds_config");
    write(&config.join("init.lua"), INIT);
    let early = config.join("pack/p/start/early");
    write(&early.join("plugin/early.lua"), EARLY_PLUGIN);
    let lazy = temp_dir("startup_cmds_lazy");
    write(
        &lazy.join("lua/lazyplug.lua"),
        "btv.command('Lazy', function() end)\nreturn {}\n",
    );
    let elsewhere = temp_dir("startup_cmds_elsewhere").join("defs.lua");
    write(&elsewhere, "btv.command('Sourced', function() end)\n");

    let init = ServerInit {
        config_dir: Some(config.clone()),
        runtimepath: vec![config, early],
        startup_user_commands_only: startup_only,
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 120, 24).await;
    assert!(
        poll_true(&rpc, "return btv.g.loaded == true").await,
        "startup ends"
    );
    Fixture {
        rpc,
        incoming,
        lazy,
        elsewhere,
    }
}

async fn defined(rpc: &Rpc, name: &str) -> bool {
    lines(rpc).await;
    exec_lua(rpc, &format!("return btv.user_command.get().{name} ~= nil")).await
        == Value::from(true)
}

async fn g(rpc: &Rpc, var: &str) -> Value {
    lines(rpc).await;
    exec_lua(rpc, &format!("return btv.g.{var}")).await
}

#[tokio::test]
async fn startup_commands_keep_working() {
    let f = start(true).await;

    feed(&f.rpc, ":FromInit<CR>:Early<CR>:FromExDuringStartup<CR>");
    assert_eq!(g(&f.rpc, "from_init").await, Value::from(1));
    assert_eq!(g(&f.rpc, "early").await, Value::from(1));
    assert_eq!(g(&f.rpc, "ex_startup").await, Value::from(1));
}

#[tokio::test]
async fn startup_code_may_define_a_command_later() {
    let f = start(true).await;

    // A startup plugin's and the config's autocmds, firing after startup.
    feed(
        &f.rpc,
        ":doautocmd User EarlyLater<CR>:doautocmd User InitLater<CR>",
    );
    feed(&f.rpc, ":EarlyLater<CR>:InitLater<CR>");
    assert_eq!(g(&f.rpc, "early_later").await, Value::from(1));
    assert_eq!(g(&f.rpc, "init_later").await, Value::from(1));
}

#[tokio::test]
async fn typed_lua_may_not() {
    let mut f = start(true).await;

    let said = message_after(
        &f.rpc,
        &mut f.incoming,
        ":lua btv.command('Typed', function() end)<CR>",
    )
    .await;
    assert!(said.contains("E5802: can't define :Typed"), "{said:?}");
    assert!(said.contains("comes from :lua"), "{said:?}");
    assert!(!defined(&f.rpc, "Typed").await);
}

#[tokio::test]
async fn a_plugin_loaded_later_may_not() {
    let f = start(true).await;

    let refused = exec_lua(
        &f.rpc,
        &format!(
            "btv._add_rtp([[{}]]) local ok, err = pcall(require, 'lazyplug') return {{ ok, tostring(err) }}",
            f.lazy.display()
        ),
    )
    .await;
    let refused = refused.as_array().unwrap();
    assert_eq!(refused[0], Value::from(false));
    let err = refused[1].as_str().unwrap();
    assert!(err.contains("E5802: can't define :Lazy"), "{err}");
    assert!(err.contains("lazyplug.lua"), "names the file: {err}");
    assert!(!defined(&f.rpc, "Lazy").await);
}

#[tokio::test]
async fn a_file_sourced_from_elsewhere_may_not() {
    let mut f = start(true).await;

    let said = message_after(
        &f.rpc,
        &mut f.incoming,
        &format!(":source {}<CR>", f.elsewhere.display()),
    )
    .await;
    assert!(said.contains("E5802: can't define :Sourced"), "{said:?}");
    assert!(!defined(&f.rpc, "Sourced").await);
}

#[tokio::test]
async fn ex_command_may_not_after_startup() {
    let mut f = start(true).await;

    let said = message_after(&f.rpc, &mut f.incoming, ":command Later echo 1<CR>").await;
    assert!(said.contains("E5802: can't define :Later"), "{said:?}");
    assert!(!defined(&f.rpc, "Later").await);
}

#[tokio::test]
async fn the_client_may() {
    let f = start(true).await;

    exec_lua(&f.rpc, "btv.command('FromHost', function() end)").await;
    assert!(defined(&f.rpc, "FromHost").await);
}

#[tokio::test]
async fn off_by_default_anything_may() {
    let f = start(false).await;

    feed(&f.rpc, ":lua btv.command('Typed', function() end)<CR>");
    feed(&f.rpc, &format!(":source {}<CR>", f.elsewhere.display()));
    feed(&f.rpc, ":command Later echo 1<CR>");
    assert!(defined(&f.rpc, "Typed").await);
    assert!(defined(&f.rpc, "Sourced").await);
    assert!(defined(&f.rpc, "Later").await);
}
