//! Behavior tests for the native snippet engine (`nx.snippet`) — the LSP-syntax
//! parser, the tabstop session (`<Tab>` / `<S-Tab>` navigation), mirrored
//! placeholders, and the `snippets` completion source.
//!
//! Black-box like the rest: a real server sources an `init.lua`, snippets are
//! driven over the same msgpack-RPC a UI uses, and assertions are on the resulting
//! buffer lines and cursor after expansion / navigation / accept.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, cursor, drain_to_latest_redraw, exec_lua, feed, lines, map_get, spawn, temp_dir,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start(dir: &std::path::Path, init_lua: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(dir.join("init.lua"), init_lua).expect("write init.lua");
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Expand a body via `nx.snippet.expand` and let the effect drain run.
async fn expand(rpc: &Rpc, body: &str) {
    // `body` is embedded in a `[[ ]]` long string, so it needs no escaping.
    exec_lua(rpc, &format!("nx.snippet.expand([[{body}]])")).await;
}

#[tokio::test]
async fn expand_places_and_tabs_through_tabstops() {
    let dir = temp_dir("snippet-tabs");
    let (rpc, _incoming) = start(&dir, "nx.snippet.setup{}").await;

    expand(&rpc, "foo($1, $2)$0").await;
    // The literal text lands with placeholders empty; the cursor sits at $1.
    assert_eq!(lines(&rpc).await, vec!["foo(, )".to_string()]);
    assert_eq!(cursor(&rpc).await, (1, 4));

    feed(&rpc, "x");
    assert_eq!(lines(&rpc).await, vec!["foo(x, )".to_string()]);

    // <Tab> jumps to $2 (just before the close paren).
    feed(&rpc, "<Tab>");
    assert_eq!(cursor(&rpc).await, (1, 7));
    feed(&rpc, "y");
    assert_eq!(lines(&rpc).await, vec!["foo(x, y)".to_string()]);

    // <S-Tab> jumps back to $1, after the typed "x".
    feed(&rpc, "<S-Tab>");
    assert_eq!(cursor(&rpc).await, (1, 5));

    // <Tab> to $2, then to the final $0 (after the close paren), ending the session.
    feed(&rpc, "<Tab><Tab>");
    assert_eq!(cursor(&rpc).await, (1, 9));
}

#[tokio::test]
async fn placeholder_default_and_mirror_sync() {
    let dir = temp_dir("snippet-mirror");
    let (rpc, _incoming) = start(&dir, "nx.snippet.setup{}").await;

    // ${1:v} provides a default that a bare $1 mirrors.
    expand(&rpc, "${1:v}=$1").await;
    assert_eq!(lines(&rpc).await, vec!["v=v".to_string()]);
    // Cursor parks at the end of the primary occurrence's default.
    assert_eq!(cursor(&rpc).await, (1, 1));

    // Typing into the active tabstop updates the mirror in lockstep.
    feed(&rpc, "x");
    assert_eq!(lines(&rpc).await, vec!["vx=vx".to_string()]);
}

#[tokio::test]
async fn multiline_body_reindents_continuation_lines() {
    let dir = temp_dir("snippet-indent");
    let (rpc, _incoming) = start(&dir, "nx.snippet.setup{}").await;

    // Open insert at an indented column so continuation lines inherit the indent.
    feed(&rpc, "i\t");
    expand(&rpc, "if $1 then\n\t$0\nend").await;
    assert_eq!(
        lines(&rpc).await,
        vec![
            "\tif  then".to_string(),
            "\t\t".to_string(),
            "\tend".to_string(),
        ]
    );
}

/// Poll for the latest redraw carrying a completion `menu` map.
async fn poll_menu(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> bool {
    for _ in 0..60 {
        nxvim_test_harness::barrier(rpc).await;
        if drain_to_latest_redraw(incoming, |m| {
            matches!(map_get(m, "menu"), Some(Value::Map(_)))
        })
        .is_some()
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    false
}

#[tokio::test]
async fn snippets_completion_source_expands_on_accept() {
    let dir = temp_dir("snippet-source");
    // Register a snippet for the no-name buffer's (empty) filetype and enable the
    // `snippets` completion source.
    let init = "nx.snippet.setup{}\n\
        nx.snippet.add('', { { trigger = 'fn', body = 'function $1()$0 end' } })\n\
        nx.complete.setup { sources = { { 'snippets' } } }";
    let (rpc, mut incoming) = start(&dir, init).await;

    feed(&rpc, "ifn");
    assert!(poll_menu(&rpc, &mut incoming).await, "snippet menu opened");

    // Select the row and accept: the trigger word is replaced by the expanded body.
    feed(&rpc, "<C-n>");
    feed(&rpc, "<C-y>");
    assert_eq!(lines(&rpc).await, vec!["function () end".to_string()]);
}

#[tokio::test]
async fn unsupported_construct_errors_loud() {
    let dir = temp_dir("snippet-unsupported");
    let (rpc, _incoming) = start(&dir, "nx.snippet.setup{}").await;

    // A variable (`$TM_FILENAME`) is unsupported: it must not insert raw text.
    expand(&rpc, "$TM_FILENAME").await;
    assert_eq!(lines(&rpc).await, vec![String::new()]);
}
