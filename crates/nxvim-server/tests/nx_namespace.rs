//! The `nx.*` namespace foundation: `nx` is the canonical config surface, and a
//! write through it drives the *same* machinery as the `vim.*` alias. Each test
//! exercises a real effect (a value reaching the core, an autocmd firing, a user
//! command running, a mapping triggering) rather than asserting pointer equality,
//! so it proves `nx.*` is wired, not just aliased to a table.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{drain_to_latest_redraw, exec_lua, feed, field_str, start_attached};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

#[tokio::test]
async fn nx_o_reaches_the_core_and_vim_o_reads_it_back() {
    let (rpc, mut incoming) = start().await;

    // Set through the canonical `nx.o`; the value must reach the core (the same
    // path `vim.o` drives), read back through the `vim.o` alias, and reach the UI.
    exec_lua(&rpc, "nx.o.guifont = 'Fira Code:h14'").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.o.guifont").await.as_str(),
        Some("Fira Code:h14"),
        "a value set through nx.o reads back through the vim.o alias"
    );
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw arrived");
    assert_eq!(
        field_str(&frame, "guifont"),
        "Fira Code:h14",
        "nx.o reaches the core and the redraw, like vim.o"
    );
}

#[tokio::test]
async fn nx_g_and_vim_g_share_one_store() {
    let (rpc, _incoming) = start().await;
    // Written through one name, visible through the other — the same variable store.
    exec_lua(&rpc, "nx.g.nx_marker = 42").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.g.nx_marker").await.as_i64(),
        Some(42)
    );
    exec_lua(&rpc, "vim.g.vim_marker = 7").await;
    assert_eq!(
        exec_lua(&rpc, "return nx.g.vim_marker").await.as_i64(),
        Some(7)
    );
}

#[tokio::test]
async fn nx_on_registers_an_autocmd_that_fires() {
    let (rpc, _incoming) = start().await;

    // Register via the canonical `nx.on(event, opts, fn)`; firing the event (the
    // manual `nvim_exec_autocmds` path) must run the handler.
    exec_lua(
        &rpc,
        "nx.on('User', { pattern = 'NxTest' }, function() vim.g.nx_fired = true end)",
    )
    .await;
    exec_lua(
        &rpc,
        "vim.api.nvim_exec_autocmds('User', { pattern = 'NxTest' })",
    )
    .await;
    assert_eq!(
        exec_lua(&rpc, "return vim.g.nx_fired").await.as_bool(),
        Some(true),
        "an autocmd registered through nx.on fires"
    );
}

#[tokio::test]
async fn nx_command_defines_a_runnable_user_command() {
    let (rpc, _incoming) = start().await;

    exec_lua(
        &rpc,
        "nx.command('NxPing', function() vim.g.nx_pinged = true end, {})",
    )
    .await;
    exec_lua(&rpc, "vim.cmd('NxPing')").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.g.nx_pinged").await.as_bool(),
        Some(true),
        "a :command defined through nx.command runs"
    );
}

#[tokio::test]
async fn nx_keymap_set_drives_a_normal_mode_mapping() {
    let (rpc, _incoming) = start().await;

    // A normal-mode mapping registered through nx.keymap must trigger on input.
    exec_lua(
        &rpc,
        "nx.keymap.set('n', 'Q', function() vim.g.nx_mapped = true end)",
    )
    .await;
    feed(&rpc, "Q");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    assert_eq!(
        exec_lua(&rpc, "return vim.g.nx_mapped").await.as_bool(),
        Some(true),
        "a mapping set through nx.keymap.set fires on the mapped key"
    );
}
