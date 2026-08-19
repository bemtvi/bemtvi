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

#[tokio::test]
async fn ui_mirror_carries_the_diagnostic_inline_messages() {
    // The end-of-line diagnostic message rides its OWN wire layer, not the extmark
    // `virt_text` one — so `t:decor().virt_text` could not see it even though the
    // signs beside it were already mirrored. A suite for diagnostic rendering could
    // check the gutter letter and nothing else.
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    rpc.request("btv_enable_test_mode", vec![])
        .await
        .expect("enable test mode");

    feed(&rpc, "ione<CR>two<CR>three<Esc>");
    exec_lua(
        &rpc,
        r#"btv.diagnostic.config({ signs = true, virtual_text = true })
           local ns = btv.ns.create("spec-diags")
           btv.diagnostic.set(ns, 0, {
             { lnum = 1, col = 0, message = "a warning here", severity = btv.diagnostic.severity.WARN },
           })"#,
    )
    .await;
    let _ = lines(&rpc).await;

    let text = exec_lua(
        &rpc,
        "return tostring((btv._ui.diagnostics_virt or {})[2] and btv._ui.diagnostics_virt[2][1])",
    )
    .await;
    assert!(
        text.as_str().unwrap_or_default().contains("a warning here"),
        "the inline diagnostic message must reach the mirror, got {text:?}"
    );
    // …and a clean row carries nothing.
    let clean = exec_lua(&rpc, "return type((btv._ui.diagnostics_virt or {})[1])").await;
    assert_eq!(
        clean.as_str(),
        Some("nil"),
        "a row with no diagnostic has no inline message"
    );
}

#[tokio::test]
async fn ui_mirror_carries_the_gutter_widths() {
    // `'numberwidth'` and `'signcolumn'` decide how wide the left gutter is, and
    // the CLIENT draws it from the reserved widths the server sends — so it is not
    // in `t:screen()` (those rows are the text area alone) nor in any other view. A
    // spec could read the option strings back and nothing else, which says what was
    // asked for rather than what was reserved.
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    rpc.request("btv_enable_test_mode", vec![])
        .await
        .expect("enable test mode");

    feed(&rpc, "ihello<Esc>");
    feed(&rpc, ":setlocal number numberwidth=8 signcolumn=yes:2<CR>");
    feed(&rpc, "<Esc>");
    let _ = lines(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return btv._ui.number_width").await.as_i64(),
        Some(8),
        "the reserved number column"
    );
    assert_eq!(
        exec_lua(&rpc, "return btv._ui.sign_width").await.as_i64(),
        Some(4),
        "two sign columns of two cells each"
    );

    // …and they follow the options.
    feed(&rpc, ":setlocal numberwidth=4<CR>");
    feed(&rpc, ":setlocal signcolumn=no<CR>");
    feed(&rpc, "<Esc>");
    let _ = lines(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return btv._ui.number_width").await.as_i64(),
        Some(4),
    );
    assert_eq!(
        exec_lua(&rpc, "return btv._ui.sign_width").await.as_i64(),
        Some(0),
    );

    // With 'nonumber' the column is not drawn at all. `number_width` still reports
    // the width it WOULD take, so the mirror also carries whether it is drawn —
    // which is the half `t:gutter()` folds in.
    // Both flags draw the column, so both have to go.
    feed(&rpc, ":setlocal nonumber norelativenumber<CR>");
    feed(&rpc, "j");
    feed(&rpc, "k");
    let _ = lines(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return btv._ui.number_shown").await,
        Value::Boolean(false),
    );
    // …and `'relativenumber'` draws it just as `'number'` does.
    feed(&rpc, ":setlocal relativenumber<CR>");
    feed(&rpc, "j");
    feed(&rpc, "k");
    let _ = lines(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return btv._ui.number_shown").await,
        Value::Boolean(true),
    );
}

#[tokio::test]
async fn ui_mirror_carries_the_window_scroll_position() {
    // Where the window is SCROLLED to is in none of the views: `t:screen()` carries
    // each painted row's full text, and the client is what clips it to the window
    // and offsets it by `leftcol`. So a spec for `nowrap` horizontal scrolling could
    // see nothing at all, and one for vertical scrolling had to infer the top line
    // from the text. Mirror the window's `leftcol` and its per-row buffer line
    // numbers — the latter being both the top line and which lines are visible at
    // all (a closed fold takes its rows out of the list).
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    rpc.request("btv_enable_test_mode", vec![])
        .await
        .expect("enable test mode");

    feed(&rpc, ":set nowrap<CR>");
    feed(&rpc, "i");
    feed(&rpc, &format!("{}<Esc>", "x".repeat(300)));
    feed(&rpc, "0");
    let _ = lines(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return btv._ui.leftcol").await.as_i64(),
        Some(0),
        "at column 0 the window is scrolled home"
    );
    feed(&rpc, "$");
    let _ = lines(&rpc).await;
    let leftcol = exec_lua(&rpc, "return btv._ui.leftcol").await.as_i64();
    assert!(
        leftcol.unwrap_or(0) > 0,
        "jumping to the end of a long line scrolls sideways, got {leftcol:?}"
    );

    // The vertical half: the first painted row's buffer line number is the top line.
    feed(&rpc, ":set wrap<CR>");
    feed(&rpc, "ggO<Esc>");
    for _ in 0..60 {
        feed(&rpc, "o<Esc>");
    }
    feed(&rpc, "gg");
    let _ = lines(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return btv._ui.numbers[1]").await.as_i64(),
        Some(1),
        "at the top of the file the first painted row is line 1"
    );
    feed(&rpc, "G");
    let _ = lines(&rpc).await;
    let top = exec_lua(&rpc, "return btv._ui.numbers[1]").await.as_i64();
    assert!(
        top.unwrap_or(0) > 1,
        "at the end of a long file the window has scrolled down, got {top:?}"
    );
}

#[tokio::test]
async fn the_test_frameworks_feed_records_into_a_macro() {
    // `btv.test`'s `t:feed` stands in for the USER typing — that is the whole
    // premise of the framework. It rode the `nvim_feedkeys` typeahead, which
    // deliberately suppresses macro recording ("typeahead is not typed input", so a
    // plugin feeding keys cannot pollute an open recording). Correct for a plugin,
    // wrong for the test framework: it made macros — and anything else that keys
    // off what was TYPED — silently untestable, recording an empty register while
    // every visible effect looked right.
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    rpc.request("btv_enable_test_mode", vec![])
        .await
        .expect("enable test mode");
    feed(&rpc, "ione<CR>two<CR>three<Esc>gg");

    // The test framework's feed: typed.
    exec_lua(&rpc, r#"btv._feedkeys("<F2>a", true, false, true)"#).await;
    exec_lua(&rpc, r#"btv._feedkeys("I- <Esc>j", true, false, true)"#).await;
    exec_lua(&rpc, r#"btv._feedkeys("<F2>", true, false, true)"#).await;
    let _ = lines(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, r#"return vim.fn.getreg("a")"#)
            .await
            .as_str(),
        Some("I-<Space><Esc>j"),
        "a typed feed is captured by an open recording"
    );

    // A PLUGIN feed still is not: that distinction is the point of the flag.
    exec_lua(&rpc, r#"btv._feedkeys("<F2>b", true, false, true)"#).await;
    exec_lua(&rpc, r#"btv._feedkeys("Ix<Esc>", true, false)"#).await;
    exec_lua(&rpc, r#"btv._feedkeys("<F2>", true, false, true)"#).await;
    let _ = lines(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, r#"return vim.fn.getreg("b")"#)
            .await
            .as_str(),
        Some(""),
        "a plugin's typeahead never lands in an open recording"
    );
}

#[tokio::test]
async fn ui_mirror_carries_the_per_region_tablines() {
    // bemtvi's tab pages are PER REGION — the main area and each dock carry their
    // own independent set — but `nvim_list_tabpages` / `nvim_get_current_tabpage`
    // report one global list from every region, so from Lua the three stacks were
    // indistinguishable. A spec for the feature could not tell "a tab was added to
    // the dock" from "a tab was added". Mirror the per-region tablines, which is
    // what the clients already draw each region's strip from.
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    rpc.request("btv_enable_test_mode", vec![])
        .await
        .expect("enable test mode");
    feed(&rpc, "imain<Esc>");

    // A region reports the tabline it DRAWS, so both need one shown.
    feed(&rpc, ":set showtabline=2<CR>");
    exec_lua(
        &rpc,
        r#"btv.dock.open({ side = "left", size = 20, showtabline = 2 })"#,
    )
    .await;
    let _ = lines(&rpc).await;

    // A tab added while the DOCK is focused belongs to the dock alone.
    exec_lua(&rpc, r#"btv.layer.focus("left")"#).await;
    feed(&rpc, ":tabnew<CR>");
    let _ = lines(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return #btv._ui.region_tabs.left.tabs")
            .await
            .as_i64(),
        Some(2),
        "the dock has two tabs"
    );
    assert_eq!(
        exec_lua(&rpc, "return #btv._ui.region_tabs.main.tabs")
            .await
            .as_i64(),
        Some(1),
        "…and the main area still has one"
    );
}

/// The `'showcmd'` corner is mirrored, so a spec can see a key run that has not
/// reached the editor. A mapped prefix is *withheld* by the keymap matcher: no
/// buffer, cursor or mode state moves while it waits, and the corner is the only
/// place it exists at all.
#[tokio::test]
async fn ui_mirror_carries_the_showcmd_corner() {
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    rpc.request("btv_enable_test_mode", vec![])
        .await
        .expect("enable test mode");
    feed(&rpc, "ione<CR>two<CR>three<Esc>");

    // The editor's own pending run: a count and an operator awaiting its motion.
    feed(&rpc, "2d");
    let _ = lines(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return btv._ui.showcmd").await.as_str(),
        Some("2d"),
        "the partly-typed command"
    );
    feed(&rpc, "<Esc>");
    let _ = lines(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return btv._ui.showcmd").await.as_str(),
        Some(""),
        "cleared once nothing is pending"
    );

    // A withheld mapped prefix — the half of the corner only the matcher knows.
    exec_lua(&rpc, r#"btv.keymap.set("n", "<Space>fs", function() end)"#).await;
    feed(&rpc, "<Space>f");
    let _ = lines(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return btv._ui.showcmd").await.as_str(),
        Some("<Space>f"),
        "the mapped prefix the editor never saw"
    );
    feed(&rpc, "<Esc>");

    // …and with the option off the corner is empty whatever is pending.
    feed(&rpc, ":set noshowcmd<CR>");
    feed(&rpc, "2d");
    let _ = lines(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return btv._ui.showcmd").await.as_str(),
        Some(""),
        "'noshowcmd' paints nothing"
    );
}

/// The scroll-animation gesture is mirrored, and it is **sticky** until the next
/// input clears it. It rides exactly one frame — the settle after the input that
/// started it repaints without one — so a spec reading it after its own `t:feed`
/// would otherwise always find nothing.
#[tokio::test]
async fn ui_mirror_carries_the_scroll_gesture() {
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    rpc.request("btv_enable_test_mode", vec![])
        .await
        .expect("enable test mode");
    let body: String = (1..=200).map(|i| format!("line {i}\n")).collect();
    let path = bemtvi_test_harness::write_temp("scroll_mirror", "txt", &body);
    feed(&rpc, &format!(":e {path}<CR>"));
    feed(&rpc, "gg");
    let _ = lines(&rpc).await;

    // A half-page scroll hands the client a slide from where it was to where it is.
    feed(&rpc, "<C-d>");
    let _ = lines(&rpc).await;
    let to = exec_lua(&rpc, "return btv._ui.scroll and btv._ui.scroll.to_row").await;
    assert!(
        to.as_i64().is_some_and(|r| r > 0),
        "the gesture names its destination, got {to:?}"
    );
    let ms = exec_lua(&rpc, "return btv._ui.scroll.duration_ms").await;
    assert!(
        ms.as_i64().is_some_and(|d| d > 0),
        "…and how long the client should take, got {ms:?}"
    );

    // The reset the harness runs before each input is what forgets it.
    exec_lua(&rpc, "btv._test_clear_scroll()").await;
    let _ = lines(&rpc).await;
    assert!(
        exec_lua(&rpc, "return btv._ui.scroll == nil")
            .await
            .as_bool()
            .unwrap_or(false),
        "the reset cleared the sticky gesture"
    );

    // `'noscrollanim'` starts none at all — the viewport still moves.
    feed(&rpc, ":set noscrollanim<CR>");
    feed(&rpc, "<C-d>");
    let _ = lines(&rpc).await;
    assert!(
        exec_lua(&rpc, "return btv._ui.scroll == nil")
            .await
            .as_bool()
            .unwrap_or(false),
        "'noscrollanim' animates nothing"
    );
}
