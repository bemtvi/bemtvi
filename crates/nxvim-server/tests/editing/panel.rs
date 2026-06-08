use crate::support::*;

// ----- bottom panel (`:messages`, `:ls`) ---------------------------------

#[tokio::test]
async fn messages_command_shows_history_in_a_panel() {
    let (rpc, mut incoming) = start(None).await;

    // Two printed lines build up the message history.
    feed(&rpc, ":lua print('alpha')<CR>");
    feed(&rpc, ":lua print('beta')<CR>");
    let map = latest_after(&rpc, &mut incoming, ":messages<CR>").await;

    // The panel opens with title "Messages" and the history (newest last).
    assert_eq!(panel_title(&map), "Messages");
    let lines = panel_lines(&map);
    assert!(
        lines.contains(&"alpha".to_string()) && lines.contains(&"beta".to_string()),
        "history was: {lines:?}"
    );
}

#[tokio::test]
async fn panel_navigates_and_closes_with_q() {
    let (rpc, mut incoming) = start(None).await;
    for i in 0..15 {
        feed(&rpc, &format!(":lua print('line{i}')<CR>"));
    }
    let map = latest_after(&rpc, &mut incoming, ":messages<CR>").await;
    // `:messages` opens scrolled to the end with the newest line selected, so the
    // cursor sits on the last visible row.
    let height = panel_u64(&map, "height");
    assert_eq!(
        panel_u64(&map, "cursor_row"),
        height - 1,
        "opens at the bottom"
    );

    // `gg` returns to the top; `j` moves the panel cursor down.
    let map = latest_after(&rpc, &mut incoming, "gg").await;
    assert_eq!(panel_u64(&map, "cursor_row"), 0);
    let map = latest_after(&rpc, &mut incoming, "j").await;
    assert_eq!(panel_u64(&map, "cursor_row"), 1);

    // `G` jumps back to the last line; scrolled to the bottom again.
    let map = latest_after(&rpc, &mut incoming, "G").await;
    assert_eq!(panel_u64(&map, "cursor_row"), height - 1);

    // `q` closes the panel — the redraw no longer carries one.
    let map = latest_after(&rpc, &mut incoming, "q").await;
    assert!(panel(&map).is_none(), "q should close the panel");
}

#[tokio::test]
async fn panelopen_reopens_the_last_panel() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, ":lua print('alpha')<CR>");
    feed(&rpc, ":lua print('beta')<CR>");

    // Open the messages panel, then close it.
    let map = latest_after(&rpc, &mut incoming, ":messages<CR>").await;
    let opened = panel_lines(&map);
    assert!(opened.contains(&"alpha".to_string()));
    let map = latest_after(&rpc, &mut incoming, "q").await;
    assert!(panel(&map).is_none(), "q closed the panel");

    // `:panelopen` brings the same panel back with identical title and content.
    let map = latest_after(&rpc, &mut incoming, ":panelopen<CR>").await;
    assert_eq!(panel_title(&map), "Messages", "the last panel reopens");
    assert_eq!(
        panel_lines(&map),
        opened,
        "reopened with the same content it had"
    );
}

#[tokio::test]
async fn panelopen_with_no_prior_panel_reports_nothing() {
    let (rpc, mut incoming) = start(None).await;
    // Nothing has ever been shown in a panel.
    let map = latest_after(&rpc, &mut incoming, ":panelopen<CR>").await;
    assert!(panel(&map).is_none(), "no panel to reopen, so none opens");
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("No panel to reopen"),
    );
}

#[tokio::test]
async fn panel_grabs_focus_so_the_buffer_is_not_edited() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>"); // buffer: "hello"
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    feed(&rpc, ":messages<CR>"); // open the panel (grabs focus)
                                 // While the panel is focused these keys drive the panel, not the buffer:
                                 // `i` and the letters are ignored, and the trailing <Esc> closes the panel.
    feed(&rpc, "iworld<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello"], "buffer must be untouched");
}

#[tokio::test]
async fn clicking_a_panel_row_selects_the_wrapped_entry() {
    // The mouse path: the client maps a click to a content display row and sends
    // `nxvim_panel_click(row)`. The panel word-wraps (width 80), so a display row
    // must map back to its logical entry — the second half of a wrapped entry
    // selects that whole entry, not the next one.
    let (rpc, mut incoming) = start(None).await;
    let long = "x".repeat(100); // wraps to two display rows at width 80
    let content = Value::Array(vec![
        Value::from("aaa"),
        Value::from(long.as_str()),
        Value::from("ccc"),
    ]);
    rpc.notify(
        "nxvim_panel_open",
        vec![
            Value::from("Picks"),
            content,
            Value::from(false),
            Value::from(0u64),
        ],
    );
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(panel_u64(&map, "cursor_row"), 0, "opens on the first entry");

    // Display rows: 0="aaa", 1..2=wrapped long entry, 3="ccc". Clicking row 2 (the
    // second half of the wrapped entry) selects that entry: its first row is 1 and
    // it spans 2 rows.
    rpc.notify("nxvim_panel_click", vec![Value::from(2u64)]);
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(
        panel_u64(&map, "cursor_row"),
        1,
        "the wrapped entry is selected"
    );
    assert_eq!(
        panel_u64(&map, "cursor_span"),
        2,
        "its whole span is focused"
    );

    // Clicking row 3 lands on the entry past the wrap (a single-row entry).
    rpc.notify("nxvim_panel_click", vec![Value::from(3u64)]);
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(panel_u64(&map, "cursor_row"), 3);
    assert_eq!(panel_u64(&map, "cursor_span"), 1);

    // A row past the content clamps to the last entry, never wrapping around.
    rpc.notify("nxvim_panel_click", vec![Value::from(99u64)]);
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(panel_u64(&map, "cursor_row"), 3, "clamps to the last entry");
}

#[tokio::test]
async fn clicking_the_selected_panel_row_activates_it() {
    // Select-then-confirm: the first click selects a row (`nxvim_panel_click`),
    // and a click on the already-selected row activates it — which the client
    // sends as `<CR>`. On a select-enabled panel that emits `nxvim_panel_select`.
    let (rpc, mut incoming) = start(None).await;
    let content = Value::Array(vec![
        Value::from("one"),
        Value::from("two"),
        Value::from("three"),
    ]);
    rpc.notify(
        "nxvim_panel_open",
        vec![
            Value::from("Picks"),
            content,
            Value::from(true), // wants_select
            Value::from(0u64),
        ],
    );
    drain_latest(&rpc, &mut incoming).await;

    // Click row 2 to select "three".
    rpc.notify("nxvim_panel_click", vec![Value::from(2u64)]);
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(panel_u64(&map, "cursor_row"), 2);

    // The client sends <CR> for a click on the already-selected row; the server
    // emits a select event for that entry (1-based index, line text).
    rpc.notify("nvim_input", vec![Value::from("<CR>")]);
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    tokio::task::yield_now().await;
    let mut selected = None;
    while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
        if method == "nxvim_panel_select" {
            selected = params.into_iter().next();
        }
    }
    let Some(Value::Map(sel)) = selected else {
        panic!("no nxvim_panel_select notification arrived");
    };
    assert_eq!(field(&sel, "index").and_then(Value::as_u64), Some(3));
    assert_eq!(field(&sel, "line").and_then(Value::as_str), Some("three"));
}

#[tokio::test]
async fn panel_shrinks_the_text_window() {
    let (rpc, mut incoming) = start(None).await;
    // No panel: the text window fills the attached height.
    let map = latest_after(&rpc, &mut incoming, "<Esc>").await;
    let full = lines_len(&map);

    let map = latest_after(&rpc, &mut incoming, ":messages<CR>").await;
    let with_panel = lines_len(&map);
    let panel_rows = panel_u64(&map, "height") + 1; // content + title bar
    assert_eq!(
        with_panel,
        full - panel_rows as usize,
        "the panel claims rows off the text window"
    );
}

// ----- scriptable panel API (`vim.panel.*`, `nxvim_panel_*`) -------------

#[tokio::test]
async fn lua_vim_panel_opens_sets_and_closes() {
    let (rpc, mut incoming) = start(None).await;
    // Drive via `nvim_command` (not focused keystrokes): once the panel is open
    // it grabs input focus, so a typed `:lua` would go to the panel — but a
    // scripted command still reaches the editor.
    let lua = |src: &str| rpc.request("nvim_command", vec![Value::from(format!("lua {src}"))]);

    lua("vim.panel.open('Custom', {'one', 'two'})")
        .await
        .expect("open");
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(panel_title(&map), "Custom");
    assert_eq!(panel_lines(&map), vec!["one", "two"]);

    // set_lines(lines) replaces the content, keeping the title.
    lua("vim.panel.set_lines({'alpha', 'beta', 'gamma'})")
        .await
        .expect("set_lines");
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(panel_title(&map), "Custom");
    assert_eq!(panel_lines(&map), vec!["alpha", "beta", "gamma"]);

    // close() dismisses it.
    lua("vim.panel.close()").await.expect("close");
    let map = drain_latest(&rpc, &mut incoming).await;
    assert!(
        panel(&map).is_none(),
        "vim.panel.close() should close the panel"
    );
}

#[tokio::test]
async fn rpc_nxvim_panel_open_set_close_and_query() {
    let (rpc, mut incoming) = start(None).await;

    assert_eq!(
        rpc.request("nxvim_panel_is_open", vec![]).await.unwrap(),
        Value::from(false),
        "no panel open initially"
    );

    rpc.request(
        "nxvim_panel_open",
        vec![
            Value::from("RPC"),
            Value::Array(vec![Value::from("a"), Value::from("b")]),
        ],
    )
    .await
    .expect("panel_open");
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(panel_title(&map), "RPC");
    assert_eq!(panel_lines(&map), vec!["a", "b"]);
    assert_eq!(
        rpc.request("nxvim_panel_is_open", vec![]).await.unwrap(),
        Value::from(true)
    );

    rpc.request(
        "nxvim_panel_set_lines",
        vec![Value::Array(vec![Value::from("only")])],
    )
    .await
    .expect("panel_set_lines");
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(panel_lines(&map), vec!["only"]);

    rpc.request("nxvim_panel_close", vec![])
        .await
        .expect("panel_close");
    let map = drain_latest(&rpc, &mut incoming).await;
    assert!(panel(&map).is_none());
    assert_eq!(
        rpc.request("nxvim_panel_is_open", vec![]).await.unwrap(),
        Value::from(false)
    );
}

#[tokio::test]
async fn scripted_panel_is_navigable_like_the_builtin_one() {
    let (rpc, mut incoming) = start(None).await;
    let many: Vec<String> = (0..20).map(|i| format!("row{i}")).collect();
    let lines = Value::Array(many.into_iter().map(Value::from).collect());
    rpc.request("nxvim_panel_open", vec![Value::from("Big"), lines])
        .await
        .expect("panel_open");
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(panel_u64(&map, "cursor_row"), 0);

    // The panel grabs focus, so j/G navigate it (not the buffer).
    let map = latest_after(&rpc, &mut incoming, "G").await;
    let height = panel_u64(&map, "height");
    assert_eq!(panel_u64(&map, "cursor_row"), height - 1);
}

#[tokio::test]
async fn lua_vim_panel_opens_at_a_cursor_and_set_cursor_moves_it() {
    let (rpc, mut incoming) = start(None).await;
    let lua = |src: &str| rpc.request("nvim_command", vec![Value::from(format!("lua {src}"))]);

    // open(title, lines, on_select, cursor): the 1-based cursor selects (and
    // scrolls to) that line. 20 rows > the panel height, so line 20 scrolls to
    // the bottom and the cursor sits on the last visible row.
    lua("local t = {} for i = 1, 20 do t[i] = 'row' .. i end \
         vim.panel.open('Jump', t, nil, 20)")
    .await
    .expect("open");
    let map = drain_latest(&rpc, &mut incoming).await;
    let height = panel_u64(&map, "height");
    assert_eq!(
        panel_u64(&map, "cursor_row"),
        height - 1,
        "opens scrolled to the requested line"
    );

    // set_cursor(line) moves the selection back to the top (1-based line 1).
    lua("vim.panel.set_cursor(1)").await.expect("set_cursor");
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(
        panel_u64(&map, "cursor_row"),
        0,
        "set_cursor moves to the top"
    );
}

#[tokio::test]
async fn rpc_nxvim_panel_open_cursor_and_set_cursor() {
    let (rpc, mut incoming) = start(None).await;
    let many: Vec<String> = (0..20).map(|i| format!("row{i}")).collect();
    let lines = Value::Array(many.into_iter().map(Value::from).collect());

    // open(title, lines, want_select, cursor): the 0-based cursor (19, the last
    // line) opens scrolled to the bottom.
    rpc.request(
        "nxvim_panel_open",
        vec![
            Value::from("Big"),
            lines,
            Value::from(false),
            Value::from(19u64),
        ],
    )
    .await
    .expect("panel_open");
    let map = drain_latest(&rpc, &mut incoming).await;
    let height = panel_u64(&map, "height");
    assert_eq!(
        panel_u64(&map, "cursor_row"),
        height - 1,
        "opens at the cursor"
    );

    // set_cursor(line) moves the 0-based selection back to the top.
    rpc.request("nxvim_panel_set_cursor", vec![Value::from(0u64)])
        .await
        .expect("panel_set_cursor");
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(
        panel_u64(&map, "cursor_row"),
        0,
        "set_cursor moves to the top"
    );
}

// ----- panel <CR> select handler (scriptable) ----------------------------

#[tokio::test]
async fn lua_panel_on_select_fires_on_enter() {
    let (rpc, mut incoming) = start(None).await;
    // Open with an on_select callback that echoes the selected line + 1-based
    // index, so we can observe it firing on the message line.
    rpc.request(
        "nvim_command",
        vec![Value::from(
            "lua vim.panel.open('P', {'aaa', 'bbb'}, \
             function(line, idx) print('sel:' .. line .. ':' .. idx) end)",
        )],
    )
    .await
    .expect("open");
    drain_latest(&rpc, &mut incoming).await;

    // Move to the second line (the panel has focus) and press <CR>.
    let map = latest_after(&rpc, &mut incoming, "j<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("sel:bbb:2"),
        "on_select(line, index) should fire for the focused line"
    );
}

#[tokio::test]
async fn lua_panel_on_select_setter_enables_enter() {
    let (rpc, mut incoming) = start(None).await;
    // Open without a handler, then attach one with the standalone setter.
    rpc.request(
        "nvim_command",
        vec![Value::from("lua vim.panel.open('P', {'only'})")],
    )
    .await
    .expect("open");
    rpc.request(
        "nvim_command",
        vec![Value::from(
            "lua vim.panel.on_select(function(line) print('got:' .. line) end)",
        )],
    )
    .await
    .expect("on_select");
    drain_latest(&rpc, &mut incoming).await;

    let map = latest_after(&rpc, &mut incoming, "<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("got:only")
    );
}

#[tokio::test]
async fn rpc_panel_select_notifies_when_select_enabled() {
    let (rpc, mut incoming) = start(None).await;
    rpc.request(
        "nxvim_panel_open",
        vec![
            Value::from("P"),
            Value::Array(vec![Value::from("x"), Value::from("y")]),
            Value::from(true), // want_select
        ],
    )
    .await
    .expect("open");
    drain_latest(&rpc, &mut incoming).await;

    rpc.notify("nvim_input", vec![Value::from("j<CR>")]);
    let params = drain_notify(&rpc, &mut incoming, "nxvim_panel_select")
        .await
        .expect("a panel_select notification");
    let map = match params.into_iter().next() {
        Some(Value::Map(m)) => m,
        _ => panic!("notification without a map"),
    };
    assert_eq!(field(&map, "index").and_then(Value::as_u64), Some(2)); // 1-based
    assert_eq!(field(&map, "line").and_then(Value::as_str), Some("y"));
}

#[tokio::test]
async fn enter_does_nothing_without_a_select_handler() {
    let (rpc, mut incoming) = start(None).await;
    // A built-in viewer (`:messages`) opts out of select events.
    rpc.request("nvim_command", vec![Value::from("messages")])
        .await
        .expect("messages");
    drain_latest(&rpc, &mut incoming).await;

    rpc.notify("nvim_input", vec![Value::from("<CR>")]);
    assert!(
        drain_notify(&rpc, &mut incoming, "nxvim_panel_select")
            .await
            .is_none(),
        "a panel with no select handler must not emit select events"
    );
}
