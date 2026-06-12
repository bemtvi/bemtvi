use crate::support::*;

// ----- vim.ui.select / vim.ui.input (Phase 8) -------------------------------

#[tokio::test]
async fn vim_ui_select_routes_the_pick_to_on_choice() {
    let (rpc, mut incoming) = start(None).await;
    // `vim.ui.select` lists the choices in the panel; a `<CR>` on the focused row
    // hands the item + 1-based index to `on_choice`, which echoes them so we can
    // observe the pick.
    rpc.request(
        "nvim_command",
        vec![Value::from(
            "lua vim.ui.select({'alpha', 'beta'}, { prompt = 'Pick:' }, \
             function(item, idx) print('chose:' .. item .. ':' .. idx) end)",
        )],
    )
    .await
    .expect("select");
    drain_latest(&rpc, &mut incoming).await;

    // Move to the second row (the panel has focus) and pick it.
    let map = latest_after(&rpc, &mut incoming, "j<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("chose:beta:2"),
        "on_choice(item, index) fires for the focused row"
    );
}

#[tokio::test]
async fn vim_ui_select_format_item_renders_the_rows() {
    let (rpc, mut incoming) = start(None).await;
    // `opts.format_item` controls the displayed text while `on_choice` still
    // receives the original item — here items are tables rendered by `.label`.
    rpc.request(
        "nvim_command",
        vec![Value::from(
            "lua vim.ui.select({ { label = 'One', id = 11 }, { label = 'Two', id = 22 } }, \
             { format_item = function(it) return it.label end }, \
             function(item) print('id:' .. item.id) end)",
        )],
    )
    .await
    .expect("select");
    let map = drain_latest(&rpc, &mut incoming).await;
    // The panel shows the formatted labels, not the raw tables.
    assert_eq!(
        panel_lines(&map),
        vec!["One".to_string(), "Two".to_string()]
    );

    // Picking the first row hands the original table to on_choice.
    let map = latest_after(&rpc, &mut incoming, "<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("id:11"),
        "on_choice receives the original item, not the formatted string"
    );
}

#[tokio::test]
async fn vim_ui_input_hands_the_typed_line_to_on_confirm() {
    let (rpc, mut incoming) = start(None).await;
    // `vim.ui.input` opens a command-line prompt; the typed text reaches
    // `on_confirm` on `<CR>`.
    let map = latest_after(
        &rpc,
        &mut incoming,
        ":lua vim.ui.input({ prompt = 'Name: ' }, function(t) print('got:' .. tostring(t)) end)<CR>",
    )
    .await;
    // The prompt is open: command mode, with the label projected for the client.
    assert_eq!(
        field(&map, "command_mode").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        field(&map, "cmdline_prompt").and_then(Value::as_str),
        Some("Name: "),
        "the input label is projected into the redraw"
    );

    // Type a line and submit: the callback fires with the text.
    let map = latest_after(&rpc, &mut incoming, "Bob<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("got:Bob")
    );
    // The prompt closed — back to normal mode.
    assert_eq!(
        field(&map, "command_mode").and_then(Value::as_bool),
        Some(false)
    );
}

#[tokio::test]
async fn vim_ui_input_default_prefills_and_is_editable() {
    let (rpc, mut incoming) = start(None).await;
    // `opts.default` prefills the line; the user edits it before submitting.
    latest_after(
        &rpc,
        &mut incoming,
        ":lua vim.ui.input({ prompt = 'Q: ', default = 'foo' }, \
         function(t) print('got:' .. tostring(t)) end)<CR>",
    )
    .await;
    // Append "bar" to the prefilled "foo" and submit.
    let map = latest_after(&rpc, &mut incoming, "bar<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("got:foobar"),
        "the default is prefilled and editable"
    );
}

#[tokio::test]
async fn vim_ui_input_cancel_hands_nil() {
    let (rpc, mut incoming) = start(None).await;
    // Cancelling the prompt (`<Esc>`) delivers `nil`, matching neovim's
    // `on_confirm(nil)`.
    latest_after(
        &rpc,
        &mut incoming,
        ":lua vim.ui.input({ prompt = 'Name: ' }, function(t) print('got:' .. tostring(t)) end)<CR>",
    )
    .await;
    let map = latest_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("got:nil"),
        "a cancelled input hands the callback nil"
    );
}

#[tokio::test]
async fn phase8_example_config_drives_select_and_input() {
    // The shipped `examples/phase8-ui` config sources cleanly and its keymaps
    // actually drive the vim.ui surfaces end-to-end (not just "loads").
    let dir = temp_dir("phase8-ex");
    let init = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/phase8-ui/init.lua"
    ))
    .expect("read example init.lua");
    let (rpc, mut incoming) = start_with_config(&dir, &init).await;
    // Startup is clean (no E5108 load error left on the message line).
    let msg = startup_message(&rpc, &mut incoming).await;
    assert!(
        !msg.contains("Error"),
        "example config left an error: {msg:?}"
    );

    // `<Space>s` opens the fruit picker; pick the second row.
    drain_latest(&rpc, &mut incoming).await;
    feed(&rpc, " s");
    drain_latest(&rpc, &mut incoming).await;
    let map = latest_after(&rpc, &mut incoming, "j<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("you picked: banana (row 2)"),
        "the example's vim.ui.select keymap works"
    );

    // `<Space>i` opens the name prompt (prefilled "anon"); append and submit.
    let map = latest_after(&rpc, &mut incoming, " i").await;
    assert_eq!(
        field(&map, "cmdline_prompt").and_then(Value::as_str),
        Some("Your name: ")
    );
    let map = latest_after(&rpc, &mut incoming, "X<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("hello, anonX!"),
        "the example's vim.ui.input keymap works"
    );
}
