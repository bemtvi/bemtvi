//! Black-box hardening tests for malformed data from an (untrusted) language
//! server. A server is a semi-trusted external process; a malformed reply must
//! never crash the editor thread — the server keeps running and the buffer ends
//! in a defined state (the no-silent-stubs / loud-recovery contract).
//!
//! The reachable seam without spinning up a real stdio server is
//! `btv._lsp_apply_workspace_edit` (the Lua entry behind
//! `vim.lsp.util.apply_workspace_edit`): it hands an LSP-shape `WorkspaceEdit`
//! straight into the same `lsp_range_to_bytes_in` → `Editor::apply_edits_to` path
//! a native rename / code-action reply uses, so a reversed-range edit exercises
//! the exact boundary a hostile/buggy server hits.

use std::path::Path;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{attach, command, exec_lua, lines, spawn, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

// The `incoming` receiver must be kept alive for the test's whole body: the
// server emits a `redraw` notification after each command, and dropping the
// receiver tears the connection down — so it is returned, not discarded.
async fn start(dir: &Path) -> (Rpc, UnboundedReceiver<Incoming>) {
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// A `WorkspaceEdit` whose `range.start` sits **after** its `range.end` (a
/// reversed/malformed range) used to produce an inverted byte range
/// (`start > end`); the cursor-shift planner in `Editor::apply_edits_to` then
/// computed the unsigned delta `e_row - s_row`, which underflowed and panicked the
/// server thread — a one-request DoS from a malformed reply. The conversion
/// boundary now clamps the range to be non-inverted, so the edit degrades to an
/// insert-only edit and the server stays alive.
#[tokio::test]
async fn reversed_workspace_edit_range_does_not_crash_the_server() {
    let dir = std::fs::canonicalize(temp_dir("lsp_malformed_reversed")).unwrap();
    let file = dir.join("target.txt");
    std::fs::write(&file, "one\ntwo\nthree\nfour\n").unwrap();
    let abs = file.to_string_lossy().into_owned();

    let (rpc, _incoming) = start(&dir).await;
    command(&rpc, &format!("edit {abs}")).await;
    assert_eq!(lines(&rpc).await, vec!["one", "two", "three", "four"]);

    // A reversed range: start at line 2 col 0, end at line 0 col 0. Through the
    // independently-clamped endpoint conversion this is byte 8 .. byte 0 — an
    // inverted byte range that used to underflow the cursor-shift math and crash
    // the server. `pcall` is reported back so a Lua-side error is visible too, but
    // the real assertion is that the server is still answering afterward.
    let pcall = exec_lua(
        &rpc,
        &format!(
            "local ok, err = pcall(function() \
               btv._lsp_apply_workspace_edit({{ changes = {{ ['file://{abs}'] = {{ \
                 {{ range = {{ start = {{ line = 2, character = 0 }}, \
                               ['end'] = {{ line = 0, character = 0 }} }}, \
                    newText = 'X' }} }} }} }}) \
             end); return tostring(ok)"
        ),
    )
    .await;
    assert_eq!(
        pcall.as_str(),
        Some("true"),
        "applying the malformed edit raised no Lua error"
    );

    // The server is still alive and answering (a panic would have dropped the
    // pipe, failing this RPC), and the reversed range degraded to an insert-only
    // edit at the clamped position rather than crashing or deleting a reversed
    // span.
    assert_eq!(
        lines(&rpc).await,
        vec!["one", "two", "Xthree", "four"],
        "reversed range clamped to an empty (insert-only) edit; server survived"
    );
    assert_eq!(
        exec_lua(&rpc, "return 1 + 1").await.as_u64(),
        Some(2),
        "the server still services RPC after the malformed edit"
    );
}
