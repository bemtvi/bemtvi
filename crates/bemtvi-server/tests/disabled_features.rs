//! Features an embedding app turns off: the plugin manager
//! (`ServerInit::disable_plugin_manager`, `E5805`), LSP (`disable_lsp`, `E5804`) and
//! pickers by source name (`disabled_pickers`, `E5803`). Each refuses loudly, takes its
//! commands with it where it has them, and is on by default.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{attach, exec_lua, message_after, poll_true, spawn, temp_dir};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start(init: ServerInit) -> (Rpc, UnboundedReceiver<Incoming>) {
    let dir = temp_dir("disabled_features");
    std::fs::write(
        dir.join("init.lua"),
        "btv.on('PluginsLoaded', function() btv.g.loaded = true end)\n",
    )
    .unwrap();
    let init = ServerInit {
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir],
        ..init
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 120, 30).await;
    (rpc, incoming)
}

/// `pcall(code)` in the editor: `(ok, error text)`.
async fn attempt(rpc: &Rpc, call: &str) -> (bool, String) {
    let got = exec_lua(
        rpc,
        &format!("local ok, err = pcall(function() {call} end) return {{ ok, tostring(err) }}"),
    )
    .await;
    let got = got.as_array().unwrap();
    (
        got[0].as_bool().unwrap(),
        got[1].as_str().unwrap_or_default().to_owned(),
    )
}

#[tokio::test]
async fn the_plugin_manager_can_be_turned_off() {
    let (rpc, mut incoming) = start(ServerInit {
        disable_plugin_manager: true,
        ..Default::default()
    })
    .await;

    assert_eq!(
        exec_lua(&rpc, "return btv.plugins.disabled()").await,
        Value::from(true)
    );
    for call in [
        "btv.plugins({ 'owner/repo' })",
        "btv.plugins.load('repo')",
        "btv.plugins.sync()",
    ] {
        let (ok, err) = attempt(&rpc, call).await;
        assert!(!ok, "{call}");
        assert!(
            err.contains("E5805") && err.contains("plugin manager is disabled"),
            "{call}: {err}"
        );
    }
    let said = message_after(&rpc, &mut incoming, ":PluginSync<CR>").await;
    assert!(said.contains("E5800: :PluginSync is disabled"), "{said:?}");
    // The editor's own startup still completes (`PluginsLoaded` comes from the manager).
    assert!(poll_true(&rpc, "return btv.g.loaded == true").await);
}

#[tokio::test]
async fn lsp_can_be_turned_off() {
    let (rpc, mut incoming) = start(ServerInit {
        disable_lsp: true,
        ..Default::default()
    })
    .await;

    assert_eq!(
        exec_lua(&rpc, "return btv.lsp.disabled()").await,
        Value::from(true)
    );
    for call in [
        "btv.lsp.enable('lua_ls')",
        "btv.lsp.start({ name = 'x', cmd = { 'true' } })",
        "btv.lsp.restart('lua_ls')",
    ] {
        let (ok, err) = attempt(&rpc, call).await;
        assert!(!ok, "{call}");
        assert!(
            err.contains("E5804") && err.contains("LSP is disabled"),
            "{call}: {err}"
        );
    }
    let said = message_after(&rpc, &mut incoming, ":LspInfo<CR>").await;
    assert!(said.contains("E5800: :LspInfo is disabled"), "{said:?}");
}

#[tokio::test]
async fn pickers_can_be_taken_away() {
    let (rpc, _incoming) = start(ServerInit {
        disabled_pickers: vec!["files".into(), "live_grep".into()],
        ..Default::default()
    })
    .await;

    let (ok, err) = attempt(&rpc, "btv.picker.open('files')").await;
    assert!(!ok);
    assert!(err.contains("E5803: the files picker is disabled"), "{err}");
    let (ok, _) = attempt(&rpc, "btv.picker.open('live_grep')").await;
    assert!(!ok);
    // Another picker still opens.
    let (ok, err) = attempt(&rpc, "btv.picker.open('buffers')").await;
    assert!(ok, "{err}");
    assert_eq!(
        exec_lua(
            &rpc,
            "return { btv.picker.disabled('files'), btv.picker.disabled('buffers') }"
        )
        .await,
        Value::Array(vec![Value::from(true), Value::from(false)])
    );
    // The picker of pickers leaves them out.
    let listed = exec_lua(
        &rpc,
        "local out = {} \
         btv.picker._sources.pickers.items({ push = function(it) out[#out + 1] = it.text end }) \
         return out",
    )
    .await;
    let listed: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        !listed.contains(&"files") && !listed.contains(&"live_grep"),
        "{listed:?}"
    );
    assert!(listed.contains(&"buffers"), "{listed:?}");
}

#[tokio::test]
async fn an_unsafe_picker_name_fails_the_start() {
    let (rpc, _incoming) = spawn(ServerInit {
        disabled_pickers: vec!["x'); os.exit(1) --".into()],
        ..Default::default()
    });
    assert!(rpc.request("nvim_get_mode", vec![]).await.is_err());
}

#[tokio::test]
async fn all_on_by_default() {
    let (rpc, _incoming) = start(ServerInit::default()).await;

    assert_eq!(
        exec_lua(
            &rpc,
            "return { btv.plugins.disabled(), btv.lsp.disabled(), btv.picker.disabled('files') }"
        )
        .await,
        Value::Array(vec![Value::from(false); 3])
    );
    let (ok, err) = attempt(&rpc, "btv.picker.open('files')").await;
    assert!(ok, "{err}");
}
