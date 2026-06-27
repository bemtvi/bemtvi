//! Per-workspace **option overrides** (`nx.wso`): a sparse overlay above the process-
//! global options that takes precedence over the global value while a workspace is open,
//! and persists in the workspace shada. Black-box, driven through the running server: set
//! an override from Lua, then read the effective value back through `nx.o` (the GoMirror,
//! which carries the *effective* options) and the override through `nx.wso`.

use std::path::Path;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::{RedbFileStore, ServerInit};
use nxvim_test_harness::{
    command, drain_to_latest_redraw, exec_lua, feed, message, start_attached, temp_dir,
};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// `nx.o.<name>` — the EFFECTIVE value (global base with the workspace overlay applied),
/// read in its own chunk so the server has drained the previous chunk's ops and re-pushed
/// the mirror.
async fn effective_bool(rpc: &Rpc, name: &str) -> Option<bool> {
    exec_lua(rpc, &format!("return nx.o.{name}"))
        .await
        .as_bool()
}

#[tokio::test]
async fn workspace_override_wins_over_the_global_value() {
    let (rpc, _incoming) = start().await;

    // Global base off; the workspace overlay forces it on. `nx.o` (effective) reads the
    // override, and `nx.wso` reads the override itself.
    exec_lua(&rpc, "nx.o.ignorecase = false").await;
    exec_lua(&rpc, "nx.wso.ignorecase = true").await;
    assert_eq!(effective_bool(&rpc, "ignorecase").await, Some(true));
    assert_eq!(
        exec_lua(&rpc, "return nx.wso.ignorecase").await.as_bool(),
        Some(true),
        "nx.wso reads the override value"
    );

    // The override keeps winning even when the GLOBAL value is set again afterward — it is
    // a live layer, not a one-shot apply (a recompute re-applies the overlay on top).
    exec_lua(&rpc, "nx.o.ignorecase = false").await;
    assert_eq!(
        effective_bool(&rpc, "ignorecase").await,
        Some(true),
        "a later global set does not clobber the workspace override"
    );

    // Clearing the override (`nx.wso.x = nil`) reverts to the global base.
    exec_lua(&rpc, "nx.wso.ignorecase = nil").await;
    assert_eq!(
        effective_bool(&rpc, "ignorecase").await,
        Some(false),
        "clearing the override falls back to the global value"
    );
    assert!(
        exec_lua(&rpc, "return nx.wso.ignorecase").await.is_nil(),
        "a cleared override reads back nil"
    );
}

#[tokio::test]
async fn override_covers_numeric_and_string_globals() {
    let (rpc, _incoming) = start().await;

    // A numeric global (laststatus) and a string global (switchbuf) both take an override.
    exec_lua(&rpc, "nx.o.laststatus = 2").await;
    exec_lua(&rpc, "nx.wso.laststatus = 3").await;
    exec_lua(&rpc, "nx.o.switchbuf = 'usetab'").await;
    exec_lua(&rpc, "nx.wso.switchbuf = 'useopen'").await;
    assert_eq!(
        exec_lua(&rpc, "return nx.o.laststatus").await.as_i64(),
        Some(3)
    );
    assert_eq!(
        exec_lua(&rpc, "return nx.o.switchbuf").await.as_str(),
        Some("useopen")
    );
    assert_eq!(
        exec_lua(&rpc, "return nx.wso.switchbuf").await.as_str(),
        Some("useopen")
    );
}

#[tokio::test]
async fn set_query_echoes_the_effective_overridden_value() {
    // The `:set ic?` ex-query path reads the EFFECTIVE value, so it reflects the workspace
    // override (the override is not just a Lua-surface illusion).
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "nx.o.ignorecase = false").await;
    exec_lua(&rpc, "nx.wso.ignorecase = true").await;
    command(&rpc, "set ignorecase?").await;
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw arrived");
    assert_eq!(
        message(&frame),
        "ignorecase",
        "the ex `?` query shows the overridden (on) value, not `noignorecase`"
    );
}

#[tokio::test]
async fn a_non_global_option_is_rejected() {
    // Only global options take a workspace override; `number` is window-local, so the
    // `nx.wso` write raises (the prelude catches it before it reaches the core).
    let (rpc, _incoming) = start().await;
    let err = exec_lua(
        &rpc,
        "local ok, e = pcall(function() nx.wso.number = true end); return e",
    )
    .await;
    assert!(
        err.as_str()
            .unwrap_or_default()
            .contains("not a global option"),
        "expected a 'not a global option' error, got {err:?}"
    );
}

/// A workspace-scoped server persisting into `dir` (the `--workspace` combination: a
/// namespaced shada with session capture on).
fn workspace_init(dir: &Path) -> ServerInit {
    ServerInit {
        shada: Some(Box::new(RedbFileStore::new(dir.to_path_buf()))),
        workspace_session: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn overrides_persist_across_a_workspace_session() {
    let dir = temp_dir("wso_store");

    // Session 1: override two globals, then quit so the exit flush captures the overlay.
    {
        let (rpc, mut incoming) = start_attached(workspace_init(&dir), 80, 25).await;
        exec_lua(&rpc, "nx.wso.ignorecase = true").await;
        exec_lua(&rpc, "nx.wso.laststatus = 3").await;
        feed(&rpc, ":qa<CR>"); // exit flush captures the overlay into the workspace store
                               // Drain until the server winds down (the exit flush wrote the store).
        while incoming.recv().await.is_some() {}
    }

    // Session 2: a fresh server over the same store restores the overlay and applies it, so
    // the effective options reflect the overrides (ignorecase defaults OFF, laststatus 2).
    {
        let (rpc, _incoming) = start_attached(workspace_init(&dir), 80, 25).await;
        assert_eq!(
            effective_bool(&rpc, "ignorecase").await,
            Some(true),
            "the ignorecase override was restored and applied"
        );
        assert_eq!(
            exec_lua(&rpc, "return nx.o.laststatus").await.as_i64(),
            Some(3),
            "the laststatus override was restored"
        );
        // And `nx.wso` reports the restored overrides (from the shada, not set this session).
        assert_eq!(
            exec_lua(&rpc, "return nx.wso.ignorecase").await.as_bool(),
            Some(true)
        );
    }
}
