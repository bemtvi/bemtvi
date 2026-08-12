//! Black-box tests for the built-in diagnostic-navigation defaults that neovim
//! ships in core: `]d`/`[d` (next/previous diagnostic), `]e`/`[e` (next/previous
//! *error*), and `<C-w>d` / `<C-w><C-d>` (show the cursor's diagnostics in a
//! float). The bracket keys are prelude default keymaps over `btv.diagnostic.goto_*`;
//! `<C-w>d` rides the native `<C-w>` window grammar in core, draining to the
//! diagnostic float in `run_pending`.
//!
//! Diagnostics are seeded with `btv.diagnostic.set` (the client-set surface), which
//! the merged store feeds to the same cursor-anchored paths the LSP set uses — so
//! these exercise the navigation without standing up a language server.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    attach, cursor, exec_lua, feed, field, lines, panel_is_open, redraw_after_matching, spawn,
    write_n_lines,
};
use rmpv::Value;
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
        local E = btv.diagnostic.severity.ERROR
        local W = btv.diagnostic.severity.WARN
        btv.diagnostic.set(btv.ns.create("test"), 0, {
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
        r#"btv.keymap.set("n", "]d", function() btv._test_hit = true end)
           return true"#,
    )
    .await;

    feed(&rpc, "]d");
    // The user RHS ran (flag set) and the built-in goto did NOT move the cursor.
    assert_eq!(
        exec_lua(&rpc, "return btv._test_hit").await.as_bool(),
        Some(true),
        "the user `]d` map fired"
    );
    assert_eq!(
        cursor(&rpc).await.0,
        1,
        "the overridden default did not also jump"
    );
}

/// The 0-based buffer line a `scroll` band row points at. The band is laid out in
/// **screen rows** (`to_cursor_row` & co. are offsets into it), so a row is mapped
/// back to a buffer line through the band's own 1-based `numbers` array.
fn scroll_band_line(map: &[(Value, Value)], row_key: &str) -> u64 {
    let Some(Value::Map(s)) = field(map, "scroll") else {
        panic!("no scroll gesture on the redraw");
    };
    let get = |k: &str| {
        s.iter()
            .find(|(kk, _)| kk.as_str() == Some(k))
            .map(|(_, v)| v)
    };
    let row = get(row_key)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("scroll.{row_key} missing")) as usize;
    get("numbers")
        .and_then(Value::as_array)
        .and_then(|a| a.get(row))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("scroll band row {row} has no buffer line"))
        - 1
}

/// An off-screen `]d` **animates** the slide, exactly like the native jumps
/// (`G`, `n`, `<C-o>`) do: the input's redraw carries the one-shot `scroll`
/// gesture the client eases the viewport along, instead of the viewport
/// teleporting. The navigation runs from Lua (the default `]d` map ->
/// `btv.diagnostic.goto_next` -> an `LspOp`) and so never reaches `Editor::input`,
/// where a typed jump takes its own viewport snapshot — the gesture has to be
/// built around the Lua-effects convergence too.
#[tokio::test]
async fn bracket_d_animates_an_off_screen_jump() {
    let (rpc, mut incoming) = start().await;
    let path = write_n_lines("diagnostic_nav_scroll", 300);
    feed(&rpc, &format!(":e {path}<CR>"));
    exec_lua(
        &rpc,
        r#"
        btv.diagnostic.set(btv.ns.create("scrolltest"), 0, {
          { lnum = 200, col = 0, message = "far away", severity = btv.diagnostic.severity.ERROR },
        })
        return true
        "#,
    )
    .await;

    // The cursor starts on line 1 and the only diagnostic sits on 0-based line
    // 200 - far past the bottom of the 24-row viewport, so the jump scrolls.
    let has_scroll = |m: &[(Value, Value)]| matches!(field(m, "scroll"), Some(Value::Map(_)));
    let map = redraw_after_matching(&rpc, &mut incoming, "]d", has_scroll).await;
    assert_eq!(
        scroll_band_line(&map, "to_cursor_row"),
        200,
        "the slide ends on the diagnostic's line"
    );
    assert_eq!(cursor(&rpc).await.0, 201, "`]d` landed on the diagnostic");

    // ...and back: `[d` wraps to the same diagnostic from the other side, an
    // upward slide that animates too.
    feed(&rpc, "gg");
    let back = redraw_after_matching(&rpc, &mut incoming, "[d", has_scroll).await;
    assert_eq!(
        scroll_band_line(&back, "to_cursor_row"),
        200,
        "`[d` animates its jump as well"
    );
}
