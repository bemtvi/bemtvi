//! Behavior tests for ex-command surface the core defers to the server resolver
//! (`excmd.rs`): the `:silent[!]` command modifier and `:source`. Driven black-box
//! over RPC like the other suites; the message line (read off the latest `redraw`)
//! is the observable for suppression / errors, and a `vim.g` marker proves a Lua
//! script actually ran. The local `redraw_after` mirrors the take-latest pattern
//! the harness documents (integration-test files don't share a module).

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{attach, drain_to_latest_redraw, exec_lua, message, spawn, temp_dir};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start(dir: &std::path::Path) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(dir.join("init.lua"), "").expect("write init.lua");
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Feed `keys`, then return the message line off the most-recent queued `redraw`
/// (take-latest, per the harness convention — a stale frame can sit ahead of this
/// input's under load).
async fn message_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> String {
    while incoming.try_recv().is_ok() {}
    rpc.request("nvim_input", vec![Value::from(keys)])
        .await
        .expect("input");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    if let Some(map) = drain_to_latest_redraw(incoming, |_| true) {
        return message(&map);
    }
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |_| true) {
            return message(&map);
        }
    }
    panic!("no redraw arrived for {keys:?}");
}

/// `:silent! {cmd}` swallows the error a bare `{cmd}` would echo — a plugin manager
/// relies on this for `silent! runtime <optional file>`. The unmodified command still
/// reports loudly (the modifier is the only thing suppressing it).
#[tokio::test]
async fn silent_bang_suppresses_command_error() {
    let dir = temp_dir("excmd_silent");
    let (rpc, mut incoming) = start(&dir).await;

    // The bare unknown command reports E492 on the message line.
    let loud = message_after(&rpc, &mut incoming, ":NotARealCommand<CR>").await;
    assert!(
        loud.contains("E492"),
        "an unknown command should report E492, got {loud:?}"
    );

    // `silent!` swallows it: the message line is left clean.
    let quiet = message_after(&rpc, &mut incoming, ":silent! NotARealCommand<CR>").await;
    assert!(
        !quiet.contains("E492") && quiet.trim().is_empty(),
        "silent! should suppress the error, got {quiet:?}"
    );
}

/// `:silent! runtime <missing>` — a plugin manager's typical bootstrap call (`silent!
/// runtime plugin/rplugin.vim`) — must not surface an error for the unimplemented
/// `:runtime`; the modifier swallows it.
#[tokio::test]
async fn silent_bang_swallows_runtime() {
    let dir = temp_dir("excmd_silent_runtime");
    let (rpc, mut incoming) = start(&dir).await;
    let msg = message_after(
        &rpc,
        &mut incoming,
        ":silent! runtime plugin/rplugin.vim<CR>",
    )
    .await;
    assert!(
        msg.trim().is_empty(),
        "silent! runtime should be quiet, got {msg:?}"
    );
}

/// `:source {file.lua}` runs the Lua file: a `vim.g` marker it sets is observable
/// afterward. (a plugin manager's loader sources plugin/ftdetect Lua files this way.)
#[tokio::test]
async fn source_runs_a_lua_file() {
    let dir = temp_dir("excmd_source");
    let script = dir.join("marker.lua");
    std::fs::write(&script, "vim.g.sourced_marker = 42\n").unwrap();

    let (rpc, mut incoming) = start(&dir).await;
    let _ = message_after(
        &rpc,
        &mut incoming,
        &format!(":source {}<CR>", script.to_string_lossy()),
    )
    .await;

    let got = exec_lua(&rpc, "return vim.g.sourced_marker").await;
    assert_eq!(
        got.as_u64(),
        Some(42),
        "sourcing a .lua file should run it: {got:?}"
    );
}

/// `:source` of a file that doesn't exist is the standard E484 (a fail-loud, not a
/// silent skip that would make a never-applied script look loaded).
#[tokio::test]
async fn source_missing_file_errors() {
    let dir = temp_dir("excmd_source_missing");
    let (rpc, mut incoming) = start(&dir).await;
    let msg = message_after(&rpc, &mut incoming, ":source /no/such/nxvim/file.lua<CR>").await;
    assert!(
        msg.contains("E484"),
        "sourcing a missing file should report E484, got {msg:?}"
    );
}
