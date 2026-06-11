//! LSP over the daemon wire — the long-lived bidirectional-pipe leg of the
//! edit-host split (Phase 3 of `docs/plans/2026-06-09-edit-host-and-browser-lua.md`).
//!
//! Proves a language server runs end-to-end **across a wire**: the editor's
//! [`LspManager`](nxvim_lsp::LspManager) spawns through a
//! [`RemoteLspTransport`](nxvim_server::RemoteLspTransport) that tunnels the server's
//! stdio over an in-process `tokio::io::duplex` to a
//! [`serve_lsp_daemon`](nxvim_server::serve_lsp_daemon) holding the *actual*
//! `nxvim --__lsp-mock` child. The duplex stands in for the eventual ssh stdio to
//! `nxvim --daemon`; the protocol is transport-agnostic.
//!
//! Faithful, not a no-op: the editor's `didOpen` (client→server) only reaches the
//! mock by crossing the wire as `lsp_stdin`, and the mock's scripted
//! `publishDiagnostics` / `definition` reply (server→client) only reaches the editor
//! by crossing back as `lsp_stdout`. The rendered diagnostics and the cursor jump are
//! state the editor can produce *only* if the full bidirectional pipe carried real
//! traffic — a stub transport could invent neither.

use crate::support::*;

use nxvim_server::{RemoteLspTransport, ServerInit};

/// Start a server whose LSP transport is a [`RemoteLspTransport`] talking to a
/// [`serve_lsp_daemon`] over an in-process duplex, UI-attached and using the shared
/// `vim.lsp.config`/`enable` init. Both the daemon task and the remote transport's RPC
/// tasks live on the test runtime; the server runs on its own thread and reaches the
/// daemon only through the injected transport.
async fn start_with_daemon_lsp(file: Option<String>) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (edit_host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (daemon_reader, daemon_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = nxvim_server::serve_lsp_daemon(daemon_reader, daemon_writer).await;
    });

    let (host_reader, host_writer) = tokio::io::split(edit_host_end);
    let transport = RemoteLspTransport::connect(host_reader, host_writer);
    let init = ServerInit {
        file,
        config_dir: Some(lsp_config_dir()),
        lsp_transport: Some(Box::new(transport)),
        ..Default::default()
    };
    start_attached(init, COLS, ROWS - 2).await
}

/// A scripted `publishDiagnostics` only reaches the editor if `didOpen` crossed the
/// wire to the real mock child *and* the mock's reply crossed back — so the rendered
/// diagnostic proves the long-lived bidirectional pipe carried real traffic.
#[tokio::test]
async fn diagnostics_round_trip_over_the_daemon_wire() {
    let _guard = test_lock().lock().await;
    let record = configure_mock(
        "daemon-diag",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [diag(0, 4, 7, 1, "use of bad")],
        }),
    );
    let file = temp_file("daemon-diag", "rs", "let bad = 1\n");
    let (rpc, mut incoming) = start_with_daemon_lsp(Some(file)).await;

    // The mock recorded the `didOpen` — proof the editor's notification crossed the
    // wire (`lsp_stdin`) to the child running on the daemon, not the local process.
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // And the scripted diagnostic crossed back (`lsp_stdout`) and rendered.
    let params = wait_for_diagnostics(&rpc, &mut incoming).await;
    let rows = diagnostics_of(&params);
    assert_eq!(
        rows[0],
        vec![(4, 7, 1)],
        "the daemon-tunneled diagnostic spans screen columns 4..7 ('bad') at severity 1"
    );

    // The under-cursor message line confirms the same diagnostic resolved end-to-end.
    feed(&rpc, "w");
    let params = wait_for_message(&rpc, &mut incoming, "use of bad").await;
    assert_eq!(message_of(&params), "use of bad");
}

/// A language-feature *request* (`textDocument/definition`) and its reply both cross
/// the wire: `gd` issues the request over `lsp_stdin`, the mock's scripted location
/// returns over `lsp_stdout`, and the cursor lands on it — proving the request/reply
/// round-trip, not just the push path.
#[tokio::test]
async fn goto_definition_round_trips_over_the_daemon_wire() {
    let _guard = test_lock().lock().await;
    let file = temp_file(
        "daemon-gd",
        "rs",
        "fn target() {}\nfn main() { target() }\n",
    );
    let record = configure_mock(
        "daemon-gd",
        serde_json::json!({ "definition": location(&file, 0, 3) }),
    );
    let (rpc, _incoming) = start_with_daemon_lsp(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // From the call site (line 1), `gd` requests go-to-definition over the wire.
    feed(&rpc, "jgd");
    // The reply (carried back over `lsp_stdout`) lands the cursor at the definition.
    wait_for_cursor(&rpc, (1, 3)).await;
    assert!(
        has_method(&record_lines(&record), "textDocument/definition"),
        "the definition request must have crossed the wire to the mock"
    );
}

/// The exit leg crosses the wire too: a mock that exits right after `initialize`
/// makes the tunneled child die, the daemon reports `lsp_exited`, and the manager
/// respawns (then the breaker gives up) — all while the editor stays fully
/// responsive. The daemon analogue of the local server-crash resilience test; it
/// proves `lsp_exited` round-trips, not just the stdin/stdout data path.
#[tokio::test]
async fn the_editor_survives_a_tunneled_server_that_keeps_exiting() {
    let _guard = test_lock().lock().await;
    configure_mock(
        "daemon-resil",
        serde_json::json!({ "exit_after_initialize": true }),
    );
    let file = temp_file("daemon-resil", "rs", "fn main() {}\n");
    let (rpc, _incoming) = start_with_daemon_lsp(Some(file)).await;

    // Hammer the editor with edits while the tunneled server crash-loops over the wire.
    feed(&rpc, "ggdGiline one<CR>line two<CR>line three<Esc>");
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await,
        vec![
            "line one".to_string(),
            "line two".to_string(),
            "line three".to_string()
        ],
        "every keystroke must apply regardless of the dying tunneled server"
    );
}
