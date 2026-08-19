//! The `btv.test` framework + `btv._ui` mirror must be GATED behind plugin-test mode
//! (the `btv_enable_test_mode` RPC the `--test-plugin` runner sends): absent in a
//! normal editor session, present only once enabled.

use bemtvi_server::ServerInit;
use bemtvi_test_harness::{exec_lua, feed, lines, start_attached};
use rmpv::Value;

#[tokio::test]
async fn test_api_is_absent_until_enabled() {
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;

    // A normal session: no test API, no UI mirror.
    assert_eq!(
        exec_lua(&rpc, "return btv.test == nil").await,
        Value::Boolean(true),
        "btv.test must be nil in a normal session"
    );
    assert_eq!(
        exec_lua(&rpc, "return btv._ui == nil").await,
        Value::Boolean(true),
        "btv._ui must be nil before test mode populates it"
    );

    // Enable test mode (what the runner does after attach).
    rpc.request("btv_enable_test_mode", vec![])
        .await
        .expect("enable test mode");

    assert_eq!(
        exec_lua(&rpc, "return type(btv.test)").await,
        Value::from("table"),
        "btv.test must be installed after enabling test mode"
    );
    assert_eq!(
        exec_lua(&rpc, "return type(btv.test.describe)").await,
        Value::from("function"),
    );

    // The UI mirror populates on the next redraw once test mode is on.
    feed(&rpc, "ihi");
    let mirrored = exec_lua(&rpc, "return btv._ui ~= nil").await;
    assert_eq!(
        mirrored,
        Value::Boolean(true),
        "btv._ui must be populated once test mode is on"
    );
}

#[tokio::test]
async fn ui_statusline_mirror_carries_the_per_window_bar_too() {
    // The mirror used to read ONLY the global bar (`laststatus=3`), so at every
    // other `'laststatus'` — including the default — `t:statusline()` reported an
    // empty string even though a status line was plainly being painted. That is a
    // stub that quietly succeeds: a spec asserting on the bar passes vacuously.
    // With no global bar, mirror the FOCUSED window's own status row instead.
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    rpc.request("btv_enable_test_mode", vec![])
        .await
        .expect("enable test mode");

    // laststatus=2: every window paints its own bar, and there is no global one.
    feed(&rpc, ":set laststatus=2<CR>");
    feed(&rpc, ":set statusline=per-window-status<CR>");
    feed(&rpc, "<Esc>");

    let sl = exec_lua(&rpc, "return btv._ui.statusline").await;
    assert_eq!(
        sl.as_str().map(|s| s.trim().to_string()),
        Some("per-window-status".to_string()),
        "the mirror must carry the focused window's bar when there is no global one"
    );

    // The global bar still wins when there is one.
    feed(&rpc, ":set laststatus=3<CR>");
    feed(&rpc, ":set statusline=global-status<CR>");
    feed(&rpc, "<Esc>");
    let sl = exec_lua(&rpc, "return btv._ui.statusline").await;
    assert_eq!(
        sl.as_str().map(|s| s.trim().to_string()),
        Some("global-status".to_string()),
        "the global bar is still what the mirror reports at laststatus=3"
    );
}

#[tokio::test]
async fn ui_statusline_mirror_carries_the_rendered_segment_text() {
    // `t:statusline()` (the `btv._ui.statusline` mirror) must reflect the actual
    // rendered global status line. It mirrors `global_status` — an array of
    // `{ text, style }` segment maps — so the text extractor has to read each map's
    // `text` key. (Regression: it used to only handle chunk-pair arrays and dropped
    // the map text entirely, leaving the mirror empty for every statusline.)
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    rpc.request("btv_enable_test_mode", vec![])
        .await
        .expect("enable test mode");

    // A single global bar (laststatus=3) with a literal `%`-format, so the rendered
    // text is deterministic regardless of file/cursor.
    feed(&rpc, ":set laststatus=3<CR>");
    feed(&rpc, ":set statusline=hello-status<CR>");
    // A keystroke to force a redraw that refreshes the mirror.
    feed(&rpc, "<Esc>");

    let sl = exec_lua(&rpc, "return btv._ui.statusline").await;
    assert_eq!(
        sl,
        Value::from("hello-status"),
        "the statusline mirror must carry the rendered global-bar text"
    );
}

#[tokio::test]
async fn ui_mirror_carries_the_resolved_colorcolumn_rulers() {
    // `'colorcolumn'` is painted by the CLIENT — the server sends the resolved
    // column list, not highlight spans — so a spec could see it in none of the
    // existing views: not `t:lines()` (it is not text), not `t:screen()` (it is a
    // background), not `t:highlights()` (no span is emitted). Mirror the resolved
    // list, which is also the only place the "+N is skipped" rule is observable:
    // the option string still reads "+1" either way.
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    rpc.request("btv_enable_test_mode", vec![])
        .await
        .expect("enable test mode");

    feed(&rpc, ":set colorcolumn=10,40<CR>");
    feed(&rpc, "<Esc>");
    let cols = exec_lua(&rpc, "return table.concat(btv._ui.colorcolumn, ',')").await;
    assert_eq!(
        cols.as_str(),
        Some("10,40"),
        "the resolved 1-based ruler columns must reach the mirror"
    );

    // A 'textwidth'-relative entry is accepted but resolves to nothing (bemtvi
    // models no 'textwidth' to anchor it), which only the resolved list shows.
    feed(&rpc, ":set colorcolumn=+1<CR>");
    feed(&rpc, "<Esc>");
    let cols = exec_lua(&rpc, "return #btv._ui.colorcolumn").await;
    assert_eq!(
        cols.as_i64(),
        Some(0),
        "a textwidth-relative entry resolves to no ruler"
    );

    feed(&rpc, ":set colorcolumn=<CR>");
    feed(&rpc, "<Esc>");
    let cols = exec_lua(&rpc, "return #btv._ui.colorcolumn").await;
    assert_eq!(cols.as_i64(), Some(0), "cleared means no rulers");
}

#[tokio::test]
async fn ui_mirror_carries_the_open_menu() {
    // The completion popup, the wildmenu, `btv.ui.select` and the picker are all
    // the same float-list widget — and a spec could see none of them. They are not
    // buffer text, not painted rows of the focused window, and not the content
    // float `t:float()` reads, so a suite for any of those features could only
    // assert on what happened AFTER an accept. Mirror the projected menu.
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    rpc.request("btv_enable_test_mode", vec![])
        .await
        .expect("enable test mode");

    // Nothing open: the mirror says so rather than reporting an empty menu.
    feed(&rpc, "ihello<Esc>");
    assert_eq!(
        exec_lua(&rpc, "return btv._ui.menu == nil").await,
        Value::Boolean(true),
        "no menu is open, so the mirror carries none"
    );

    // `btv.ui.select` puts one up.
    exec_lua(
        &rpc,
        r#"btv.ui.select({ "alpha", "beta", "gamma" }, {}):next(function() end)"#,
    )
    .await;
    // The mirror refreshes on the next redraw, so settle one first.
    let _ = lines(&rpc).await;
    let items = exec_lua(&rpc, "return table.concat(btv._ui.menu.items or {}, ',')").await;
    assert_eq!(
        items.as_str(),
        Some("alpha,beta,gamma"),
        "the menu's rows must reach the mirror"
    );
    // It opens `noselect`: a row index is carried, but nothing is highlighted yet.
    assert_eq!(
        exec_lua(&rpc, "return btv._ui.menu.selected_active").await,
        Value::Boolean(false),
        "the menu opens with nothing highlighted"
    );

    // Moving the highlight moves it in the mirror too.
    feed(&rpc, "j");
    feed(&rpc, "<C-n>");
    let _ = lines(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return btv._ui.menu.selected")
            .await
            .as_i64(),
        Some(1),
        "the highlight advanced to the second row (0-based on the wire)"
    );
    assert_eq!(
        exec_lua(&rpc, "return btv._ui.menu.selected_active").await,
        Value::Boolean(true),
    );

    // …and dismissing it clears the mirror.
    feed(&rpc, "<Esc>");
    assert_eq!(
        exec_lua(&rpc, "return btv._ui.menu == nil").await,
        Value::Boolean(true),
        "a dismissed menu leaves nothing behind"
    );
}

#[tokio::test]
async fn ui_mirror_carries_the_line_background_layer() {
    // `line_hl_group` — the full-width row tint — is the fourth decoration payload
    // and the only one a spec could not see: it is not buffer text, it is not a
    // glyph, and it rides its own `line_bg` wire layer rather than the highlight
    // spans. Mirror which rows carry one. (Which GROUP is deliberately absent, as
    // for the other decoration layers: the wire carries a per-frame palette id.)
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    rpc.request("btv_enable_test_mode", vec![])
        .await
        .expect("enable test mode");

    feed(&rpc, "ione<CR>two<CR>three<Esc>");
    exec_lua(
        &rpc,
        r##"btv.hl.define(0, "SpecRowTint", { bg = "#332211" })
           local ns = btv.ns.create("spec-line-bg")
           btv.buf.set_extmark(0, ns, 1, 0, { line_hl_group = "SpecRowTint" })"##,
    )
    .await;
    let _ = lines(&rpc).await;
    let tinted = exec_lua(
        &rpc,
        r#"local out = {}
           for _, place in ipairs(btv._ui.line_bg or {}) do out[#out + 1] = place[1] + 1 end
           table.sort(out)
           return table.concat(out, ",")"#,
    )
    .await;
    assert_eq!(
        tinted.as_str(),
        Some("2"),
        "only the row carrying the line_hl_group is tinted (1-based screen rows)"
    );
}
