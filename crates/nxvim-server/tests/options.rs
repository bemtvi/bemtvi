//! Global option plumbing: an option set through `vim.o` reaches the core, reads
//! back consistently, and — for UI-relevant ones like `guifont` — is relayed to
//! the client in the `redraw` (where a GUI parses it for the font).

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    command, drain_to_latest_redraw, exec_lua, field_str, message, start_attached,
};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// `:set <name…>` then read the echoed message off the redraw — the seam a loud
/// `:set` error surfaces through.
async fn set_message(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>, args: &str) -> String {
    command(rpc, &format!("set {args}")).await;
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(incoming, |_| true).expect("a redraw arrived");
    message(&frame)
}

/// Every option `:set` recognizes (the `canonical` registry) must be wired all the
/// way to its storage — `:set <name>?` returns a real readout, never an empty
/// (silent) message and never E518 (the wiring-gap error). This guards the exact
/// `imagepreview` bug: a name added to the registry but missing from `apply_set_*`'s
/// slot match used to silently no-op; now it's loud, and this catches it. Add a name
/// here when you add one to `canonical`.
#[tokio::test]
async fn every_known_option_is_wired_not_silent() {
    let (rpc, mut incoming) = start().await;
    let names = [
        // bool
        "number",
        "relativenumber",
        "cursorline",
        "ignorecase",
        "smartcase",
        "wrapscan",
        "hlsearch",
        "incsearch",
        "autoread",
        "imagepreview",
        "expandtab",
        "bomb",
        "scrollanim",
        "ts_highlight",
        // number
        "tabstop",
        "shiftwidth",
        "softtabstop",
        "sidescroll",
        "sidescrolloff",
        "showtabline",
        "laststatus",
        "mousetime",
        "scrollanimduration",
        "scrollback",
        // string
        "statusline",
        "tabline",
        "guifont",
        "mouse",
        "mousemodel",
        "mousescroll",
        "regexsyntax",
        "fileencoding",
        "fileencodings",
        "errorformat",
        "switchbuf",
        "makeprg",
        "grepprg",
        "grepformat",
        "filetype",
        "fillchars",
    ];
    for name in names {
        let msg = set_message(&rpc, &mut incoming, &format!("{name}?")).await;
        assert!(
            msg.contains(name) && !msg.contains("E518"),
            "`:set {name}?` must give a real readout (option wired), got {msg:?}"
        );
    }
}

#[tokio::test]
async fn set_unknown_option_errors_loudly() {
    let (rpc, mut incoming) = start().await;
    // A genuinely non-existent option name is a loud E518, naming the option — never
    // a silent no-op (CLAUDE.md's no-silent-stub rule). Covers a typo and a bogus name.
    let msg = set_message(&rpc, &mut incoming, "nonexistentoption").await;
    assert!(
        msg.contains("E518") && msg.contains("nonexistentoption"),
        "unknown :set option must fail loud naming it, got {msg:?}"
    );

    // A `no`-prefixed bogus boolean, an `=`-assignment to a bogus name, and a `?`
    // query of a bogus name are all equally loud (not silently swallowed by the prefix
    // parsing).
    let msg = set_message(&rpc, &mut incoming, "nobogus").await;
    assert!(
        msg.contains("E518"),
        "`:set nobogus` must be loud, got {msg:?}"
    );
    let msg = set_message(&rpc, &mut incoming, "bogus=3").await;
    assert!(
        msg.contains("E518"),
        "`:set bogus=3` must be loud, got {msg:?}"
    );
    let msg = set_message(&rpc, &mut incoming, "bogus?").await;
    assert!(
        msg.contains("E518"),
        "`:set bogus?` must be loud, got {msg:?}"
    );
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
async fn fillchars_round_trips_through_set_and_vim_wo() {
    let (rpc, mut incoming) = start().await;

    // Defaults empty (vim's default look, `eob:~`), both via `:set …?` and `vim.wo`.
    let msg = set_message(&rpc, &mut incoming, "fillchars?").await;
    assert_eq!(msg, "fillchars=");
    assert_eq!(
        exec_lua(&rpc, "return vim.wo.fillchars").await.as_str(),
        Some("")
    );

    // `vim.wo.fillchars = 'eob: '` (the window-local Lua form) reaches the core and
    // reads back through both the `:set …?` echo and `vim.wo`.
    exec_lua(&rpc, "vim.wo.fillchars = 'eob: '").await;
    let msg = set_message(&rpc, &mut incoming, "fillchars?").await;
    assert_eq!(msg, "fillchars=eob: ");
    assert_eq!(
        exec_lua(&rpc, "return vim.wo.fillchars").await.as_str(),
        Some("eob: ")
    );
}

#[tokio::test]
async fn unknown_vim_o_option_warns_but_stores() {
    let (rpc, mut incoming) = start().await;

    // First, a KNOWN option through `vim.o` must NOT warn (clean session, empty
    // message before any unknown write).
    exec_lua(&rpc, "vim.o.hlsearch = false").await;
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw arrived");
    assert!(
        !message(&frame).to_lowercase().contains("unknown option"),
        "a known vim.o option must not warn, got {:?}",
        message(&frame)
    );

    // An option nxvim doesn't model (a typo, or an unmodeled real neovim option) is
    // KEPT — compat: a config setting it still loads — but surfaces a warning naming
    // it, so it isn't silently swallowed (unlike `:set`, which rejects it outright).
    exec_lua(&rpc, "vim.o.nonexistentopt = true").await;
    let frame = drain_to_latest_redraw(&mut incoming, |m| message(m).contains("nonexistentopt"))
        .expect("the unknown-option warning surfaced");
    let msg = message(&frame);
    assert!(
        msg.contains("nonexistentopt") && msg.to_lowercase().contains("unknown option"),
        "an unmodeled vim.o option must warn naming it, got {msg:?}"
    );

    // …and it is still stored (reads back), so the compat catch-all is preserved.
    assert_eq!(
        exec_lua(&rpc, "return vim.o.nonexistentopt")
            .await
            .as_bool(),
        Some(true),
        "the unknown option is kept (compat), not rejected"
    );
}

#[tokio::test]
async fn seeded_read_mostly_options_write_without_warning() {
    // `background` / `termguicolors` are deliberately modeled as read-mostly
    // store-backed options (seeded with neovim defaults so colorschemes can read
    // them). Writing them is expected — every colorscheme does — so it must NOT
    // surface the "unknown option" warning reserved for genuine typos. (Regression:
    // catppuccin's `vim.o.termguicolors`/`vim.o.background` warned on load.)
    let (rpc, mut incoming) = start().await;

    exec_lua(&rpc, "vim.o.termguicolors = true").await;
    exec_lua(&rpc, "vim.o.background = 'dark'").await;
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw arrived");
    assert!(
        !message(&frame).to_lowercase().contains("unknown option"),
        "writing a seeded read-mostly option must not warn, got {:?}",
        message(&frame)
    );

    // …and they still round-trip (the store keeps the written value).
    assert_eq!(
        exec_lua(&rpc, "return vim.o.background").await.as_str(),
        Some("dark")
    );
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
