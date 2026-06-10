//! Global option plumbing: an option set through `vim.o` reaches the core, reads
//! back consistently, and — for UI-relevant ones like `guifont` — is relayed to
//! the client in the `redraw` (where a GUI parses it for the font).

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{drain_to_latest_redraw, exec_lua, field_str, start_attached};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

#[tokio::test]
async fn guifont_round_trips_and_reaches_the_redraw() {
    let (rpc, mut incoming) = start().await;

    // `vim.o.guifont = …` (the init.lua form) reaches the core and reads back.
    exec_lua(&rpc, "vim.o.guifont = 'Fira Code:h14'").await;
    let read = exec_lua(&rpc, "return vim.o.guifont").await;
    assert_eq!(
        read.as_str(),
        Some("Fira Code:h14"),
        "vim.o.guifont reads back"
    );

    // And it is relayed to the UI in the redraw, so a GUI can apply the font.
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw arrived");
    assert_eq!(field_str(&frame, "guifont"), "Fira Code:h14");
}

#[tokio::test]
async fn guifont_defaults_empty() {
    // Unset, both the read-back and the redraw field are empty — the GUI then uses
    // its own default font.
    let (rpc, mut incoming) = start().await;
    assert_eq!(
        exec_lua(&rpc, "return vim.o.guifont").await.as_str(),
        Some("")
    );
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw arrived");
    assert_eq!(field_str(&frame, "guifont"), "");
}

#[tokio::test]
async fn autoread_defaults_on_and_round_trips_through_vim_o() {
    let (rpc, _incoming) = start().await;

    // neovim's default is on, so the mirror reflects the core default before any set.
    assert_eq!(
        exec_lua(&rpc, "return vim.o.autoread").await.as_bool(),
        Some(true),
        "vim.o.autoread defaults on (neovim)"
    );

    // A write through `vim.o` reaches the core and reads back (the `:checktime`
    // reload-vs-warn decision reads this exact flag).
    exec_lua(&rpc, "vim.o.autoread = false").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.o.autoread").await.as_bool(),
        Some(false),
        "vim.o.autoread = false round-trips"
    );
}
