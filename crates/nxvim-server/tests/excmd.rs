//! Behavior tests for ex-command surface the core defers to the server resolver
//! (`excmd.rs`): the `:silent[!]` command modifier and `:source`. Driven black-box
//! over RPC like the other suites; the message line (read off the latest `redraw`)
//! is the observable for suppression / errors, and a `vim.g` marker proves a Lua
//! script actually ran. The local `redraw_after` mirrors the take-latest pattern
//! the harness documents (integration-test files don't share a module).

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{attach, exec_lua, message_after, spawn, temp_dir};
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

/// The GUI's `report_connect_error` reports a failed `:connect` on the message line
/// via the current session. nxvim is its own editor and implements `:echoerr`, but
/// **not** vimscript's `:echohl` — an `echohl`-bar command (which the GUI used to
/// emit) dies at the first bar with E492, swallowing the real connect error. Guard
/// the working form: `:echoerr '<text>'` surfaces the text and never reports E492.
#[tokio::test]
async fn echoerr_reports_message_not_e492() {
    let dir = temp_dir("excmd_echoerr");
    let (rpc, mut incoming) = start(&dir).await;

    // The form the GUI emits for a connect failure (single quotes Vim-doubled).
    let msg = message_after(
        &rpc,
        &mut incoming,
        ":echoerr ':connect failed: unknown host ''asdfasd'''<CR>",
    )
    .await;
    assert!(
        !msg.contains("E492"),
        ":echoerr must not report E492, got {msg:?}"
    );
    assert!(
        msg.contains("connect failed: unknown host 'asdfasd'"),
        ":echoerr should surface the connect error text, got {msg:?}"
    );

    // The old `echohl`-bar form is exactly what produced the reported bug: `echohl`
    // is not an nxvim ex-command, so the bar command reports E492 (kept as the
    // documented reason the GUI must not use it).
    let broken = message_after(
        &rpc,
        &mut incoming,
        ":echohl ErrorMsg|echom 'x'|echohl NONE<CR>",
    )
    .await;
    assert!(
        broken.contains("E492") && broken.contains("echohl"),
        "the echohl-bar form should fail with E492 on echohl, got {broken:?}"
    );
}
