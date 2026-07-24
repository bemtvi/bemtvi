//! Black-box tests for the built-in diagnostic-navigation defaults that neovim
//! ships in core: `]d`/`[d` (next/previous diagnostic), `]e`/`[e` (next/previous
//! *error*), and `<C-w>d` / `<C-w><C-d>` (show the cursor's diagnostics in a
//! float). The bracket keys are prelude default keymaps over `nx.diagnostic.goto_*`;
//! `<C-w>d` rides the native `<C-w>` window grammar in core, draining to the
//! diagnostic float in `run_pending`.
//!
//! Diagnostics are seeded with `nx.diagnostic.set` (the client-set surface), which
//! the merged store feeds to the same cursor-anchored paths the LSP set uses — so
//! these exercise the navigation without standing up a language server.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, cursor, exec_lua, feed, lines, panel_is_open, spawn, write_n_lines,
};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (rpc, incoming) = spawn(ServerInit::default());
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Open a 7-line temp file and seed client diagnostics: errors on 0-based lines 1
/// and 5, a warning on line 3. (Cursor lines below are 1-based, so these land the
/// cursor on 1-based lines 2, 4, 6.)
async fn open_with_diagnostics(rpc: &Rpc) {
    let path = write_n_lines("diagnostic_nav", 7);
    feed(rpc, &format!(":e {path}<CR>"));
    exec_lua(
        rpc,
        r#"
        local E = nx.diagnostic.severity.ERROR
        local W = nx.diagnostic.severity.WARN
        nx.diagnostic.set(nx.ns.create("test"), 0, {
          { lnum = 1, col = 0, message = "err one",  severity = E },
          { lnum = 3, col = 0, message = "warn one", severity = W },
          { lnum = 5, col = 0, message = "err two",  severity = E },
        })
        return true
        "#,
    )
    .await;
}

/// `]d` / `[d` walk every diagnostic in position order and wrap at the ends.
#[tokio::test]
async fn bracket_d_jumps_to_next_and_previous_diagnostic() {
    let (rpc, _incoming) = start().await;
    open_with_diagnostics(&rpc).await;

    // Cursor starts on line 1; `]d` walks forward through lines 2, 4, 6 (the
    // diagnostics' 0-based lines 1, 3, 5), then wraps back to the first.
    feed(&rpc, "]d");
    assert_eq!(cursor(&rpc).await.0, 2, "first `]d` -> first diagnostic");
    feed(&rpc, "]d");
    assert_eq!(cursor(&rpc).await.0, 4, "second `]d` -> warning");
    feed(&rpc, "]d");
    assert_eq!(cursor(&rpc).await.0, 6, "third `]d` -> last diagnostic");
    feed(&rpc, "]d");
    assert_eq!(cursor(&rpc).await.0, 2, "`]d` wraps to the first");

    // `[d` walks backward and wraps the other way.
    feed(&rpc, "[d");
    assert_eq!(cursor(&rpc).await.0, 6, "`[d` wraps to the last");
    feed(&rpc, "[d");
    assert_eq!(cursor(&rpc).await.0, 4, "`[d` -> warning");
}

/// `]e` / `[e` stop only on errors, skipping the warning between them.
#[tokio::test]
async fn bracket_e_jumps_only_between_errors() {
    let (rpc, _incoming) = start().await;
    open_with_diagnostics(&rpc).await;

    // Errors live on lines 2 and 6; the warning on line 4 is skipped.
    feed(&rpc, "]e");
    assert_eq!(cursor(&rpc).await.0, 2, "first `]e` -> first error");
    feed(&rpc, "]e");
    assert_eq!(
        cursor(&rpc).await.0,
        6,
        "`]e` skips the warning -> next error"
    );
    feed(&rpc, "]e");
    assert_eq!(cursor(&rpc).await.0, 2, "`]e` wraps to the first error");

    feed(&rpc, "[e");
    assert_eq!(cursor(&rpc).await.0, 6, "`[e` wraps to the last error");
}

/// `<C-w>d` opens a float listing the cursor line's diagnostics; `<C-w><C-d>` is
/// the same. A line with no diagnostics is a loud no-op (no panel).
#[tokio::test]
async fn ctrl_w_d_opens_the_diagnostic_float() {
    let (rpc, _incoming) = start().await;
    open_with_diagnostics(&rpc).await;

    // On a clean line (line 1, no diagnostic) the chord opens nothing.
    feed(&rpc, "<C-w>d");
    assert!(
        !panel_is_open(&rpc).await,
        "`<C-w>d` on a clean line opens no float"
    );

    // Jump onto the first error, then show it: a panel opens carrying the message.
    feed(&rpc, "]d");
    feed(&rpc, "<C-w>d");
    assert!(
        panel_is_open(&rpc).await,
        "`<C-w>d` opens the diagnostic float"
    );
    assert!(
        lines(&rpc).await.iter().any(|l| l.contains("err one")),
        "the float lists the cursor line's diagnostic"
    );

    // Close it, and confirm the control twin `<C-w><C-d>` does the same.
    feed(&rpc, "q");
    feed(&rpc, "<C-w><C-d>");
    assert!(
        panel_is_open(&rpc).await,
        "`<C-w><C-d>` opens the diagnostic float too"
    );
    assert!(
        lines(&rpc).await.iter().any(|l| l.contains("err one")),
        "the float lists the diagnostic via the control twin"
    );
}

/// The defaults register with `default = true`, so a user map on `]d` wins.
#[tokio::test]
async fn user_map_overrides_the_default() {
    let (rpc, _incoming) = start().await;
    open_with_diagnostics(&rpc).await;

    exec_lua(
        &rpc,
        r#"nx.keymap.set("n", "]d", function() nx._test_hit = true end)
           return true"#,
    )
    .await;

    feed(&rpc, "]d");
    // The user RHS ran (flag set) and the built-in goto did NOT move the cursor.
    assert_eq!(
        exec_lua(&rpc, "return nx._test_hit").await.as_bool(),
        Some(true),
        "the user `]d` map fired"
    );
    assert_eq!(
        cursor(&rpc).await.0,
        1,
        "the overridden default did not also jump"
    );
}
