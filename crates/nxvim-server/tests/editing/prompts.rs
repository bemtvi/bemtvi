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

// ----- vim.fn.input / vim.fn.confirm (synchronous prompts) ------------------
//
// Unlike the async `vim.ui.input` (callback) surface above, these *block* the
// calling Lua chunk and return the answer inline: `input` returns the typed
// string (`""` on cancel), `confirm` a 1-based button index (`0` on cancel).
// They are driven through a coroutine the entry point (`:lua`, a keymap, a user
// command) runs the chunk inside, so a `coroutine.yield` parks the chunk on the
// command-line prompt and the prompt result resumes it. Tests open the prompt
// with a `:lua …<CR>` trigger (a notification, so it never deadlocks an RPC
// reply), feed the answer, and observe the inline result via `print`.

#[tokio::test]
async fn vim_fn_input_returns_typed_text() {
    let (rpc, mut incoming) = start(None).await;
    // The chunk blocks on `vim.fn.input`; the prompt opens and the chunk parks.
    let map = latest_after(
        &rpc,
        &mut incoming,
        ":lua print('got:' .. vim.fn.input('Name: '))<CR>",
    )
    .await;
    assert_eq!(
        field(&map, "command_mode").and_then(Value::as_bool),
        Some(true),
        "the prompt opens (command mode) while the chunk is parked"
    );
    assert_eq!(
        field(&map, "cmdline_prompt").and_then(Value::as_str),
        Some("Name: "),
        "the input label is projected into the redraw"
    );
    // Typing the answer and submitting resumes the parked chunk, which returns
    // the line inline and prints it.
    let map = latest_after(&rpc, &mut incoming, "Bob<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("got:Bob"),
        "vim.fn.input returns the typed line inline"
    );
    assert_eq!(
        field(&map, "command_mode").and_then(Value::as_bool),
        Some(false),
        "the prompt closed once the answer was submitted"
    );
}

#[tokio::test]
async fn vim_fn_input_esc_returns_empty_string() {
    let (rpc, mut incoming) = start(None).await;
    // Cancelling `vim.fn.input` returns "" (an empty string), NOT nil — the key
    // contract difference from `vim.ui.input`, which hands its callback nil.
    latest_after(
        &rpc,
        &mut incoming,
        ":lua print('got[' .. vim.fn.input('Name: ') .. ']')<CR>",
    )
    .await;
    let map = latest_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("got[]"),
        "a cancelled vim.fn.input returns an empty string"
    );
}

#[tokio::test]
async fn vim_fn_input_default_prefills_and_is_editable() {
    let (rpc, mut incoming) = start(None).await;
    // The positional `(prompt, default)` form prefills the line; the user edits
    // it before submitting.
    latest_after(
        &rpc,
        &mut incoming,
        ":lua print('got:' .. vim.fn.input('Q: ', 'foo'))<CR>",
    )
    .await;
    let map = latest_after(&rpc, &mut incoming, "bar<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("got:foobar"),
        "the default is prefilled and editable"
    );
}

#[tokio::test]
async fn vim_fn_input_accepts_table_opts() {
    let (rpc, mut incoming) = start(None).await;
    // The neovim `vim.fn.input({ prompt = …, default = … })` table form.
    latest_after(
        &rpc,
        &mut incoming,
        ":lua print('got:' .. vim.fn.input({ prompt = 'P: ', default = 'x' }))<CR>",
    )
    .await;
    let map = latest_after(&rpc, &mut incoming, "<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("got:x"),
        "the table form's prompt/default are honored"
    );
}

#[tokio::test]
async fn vim_fn_input_works_from_a_keymap_callback() {
    let (rpc, mut incoming) = start(None).await;
    // A keymap RHS is also a pumped entry: a mapping that calls vim.fn.input can
    // block and use the answer.
    rpc.request(
        "nvim_exec_lua",
        vec![
            Value::from(
                "vim.keymap.set('n', '<Space>n', function() \
                   print('hi ' .. vim.fn.input('who? ')) end)",
            ),
            Value::Array(vec![]),
        ],
    )
    .await
    .expect("set keymap");
    let map = latest_after(&rpc, &mut incoming, " n").await;
    assert_eq!(
        field(&map, "cmdline_prompt").and_then(Value::as_str),
        Some("who? "),
        "the keymap's vim.fn.input opens its prompt"
    );
    let map = latest_after(&rpc, &mut incoming, "Sam<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("hi Sam"),
        "the keymap callback gets the inline answer"
    );
}

#[tokio::test]
async fn editor_is_responsive_after_an_input_prompt() {
    let (rpc, mut incoming) = start(None).await;
    // Resolving a prompt must leave the editor cleanly back in normal mode — no
    // residual command-line state.
    latest_after(
        &rpc,
        &mut incoming,
        ":lua print('got:' .. vim.fn.input('X: '))<CR>",
    )
    .await;
    feed(&rpc, "answer<CR>");
    // Normal editing works immediately afterward.
    feed(&rpc, "ihello world<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello world"]);
}

#[tokio::test]
async fn vim_fn_input_outside_a_pumped_context_fails_loud() {
    let (rpc, mut incoming) = start(None).await;
    // A scheduled callback runs outside the coroutine-pumped entry path, so a
    // blocking prompt there cannot suspend. It must fail loud (E5108), never
    // hang the editor or fabricate a value.
    let map = latest_after(
        &rpc,
        &mut incoming,
        ":lua vim.schedule(function() vim.fn.input('X: ') end)<CR>",
    )
    .await;
    let msg = field(&map, "message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        msg.contains("E5108") && msg.contains("input"),
        "a blocking prompt outside a pumped context fails loud: {msg:?}"
    );
}

#[tokio::test]
async fn vim_fn_confirm_accelerator_key_picks_the_button() {
    let (rpc, mut incoming) = start(None).await;
    // `confirm` lists the buttons and resolves on a single accelerator keypress
    // (the char after `&`), returning that button's 1-based index.
    let map = latest_after(
        &rpc,
        &mut incoming,
        ":lua print('c=' .. vim.fn.confirm('Save?', '&Yes\\n&No'))<CR>",
    )
    .await;
    assert_eq!(
        field(&map, "command_mode").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        field(&map, "cmdline_prompt").and_then(Value::as_str),
        Some("Save? [Y]es, [N]o: "),
        "the confirm message and buttons are projected"
    );
    let map = latest_after(&rpc, &mut incoming, "n").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("c=2"),
        "pressing the 'N' accelerator returns button 2"
    );
}

#[tokio::test]
async fn vim_fn_confirm_enter_picks_the_default() {
    let (rpc, mut incoming) = start(None).await;
    // `<CR>` resolves to the default button (the 3rd arg, 1-based).
    latest_after(
        &rpc,
        &mut incoming,
        ":lua print('c=' .. vim.fn.confirm('Q', '&Yes\\n&No', 1))<CR>",
    )
    .await;
    let map = latest_after(&rpc, &mut incoming, "<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("c=1"),
        "Enter selects the default button"
    );
}

#[tokio::test]
async fn vim_fn_confirm_esc_returns_zero() {
    let (rpc, mut incoming) = start(None).await;
    // Cancelling (`<Esc>`) returns 0.
    latest_after(
        &rpc,
        &mut incoming,
        ":lua print('c=' .. vim.fn.confirm('Q', '&Yes\\n&No', 1))<CR>",
    )
    .await;
    let map = latest_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("c=0"),
        "a cancelled confirm returns 0"
    );
}

#[tokio::test]
async fn sync_prompts_example_config_drives_input_and_confirm() {
    // The shipped `examples/sync-prompts` config sources cleanly and its keymaps
    // actually drive vim.fn.input / vim.fn.confirm end-to-end (not just "loads").
    let dir = temp_dir("sync-prompts-ex");
    let init = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/sync-prompts/init.lua"
    ))
    .expect("read example init.lua");
    let (rpc, mut incoming) = start_with_config(&dir, &init).await;
    let msg = startup_message(&rpc, &mut incoming).await;
    assert!(
        !msg.contains("Error"),
        "example config left an error: {msg:?}"
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
        "the example's vim.fn.input keymap returns and uses the typed line"
    );

    // Put a line in the buffer, then `<Space>d` → the confirm dialog → 'y' deletes
    // it (proving confirm's single-key accept and the inline return both work).
    feed(&rpc, "ineedle<Esc>");
    assert_eq!(lines(&rpc).await, vec!["needle"]);
    let map = latest_after(&rpc, &mut incoming, " d").await;
    assert_eq!(
        field(&map, "cmdline_prompt").and_then(Value::as_str),
        Some("Delete the line? [Y]es, [N]o, [C]ancel: "),
        "the example's vim.fn.confirm dialog renders its buttons"
    );
    let map = latest_after(&rpc, &mut incoming, "y").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("deleted")
    );
    assert_eq!(lines(&rpc).await, vec![""], "Yes deleted the line");

    // `<Space>r` chains input() THEN confirm() in one body — two yields on the
    // same coroutine. Type the new text, submit, then confirm: proves a nested
    // prompt re-parks and resumes the same blocked call cleanly.
    feed(&rpc, "iold<Esc>");
    let map = latest_after(&rpc, &mut incoming, " r").await;
    assert_eq!(
        field(&map, "cmdline_prompt").and_then(Value::as_str),
        Some("New text: ")
    );
    // Submitting the input opens the SECOND (confirm) prompt from the same body.
    let map = latest_after(&rpc, &mut incoming, "new<CR>").await;
    assert_eq!(
        field(&map, "cmdline_prompt").and_then(Value::as_str),
        Some("Replace this line? [Y]es, [N]o: "),
        "input() resolved and confirm() opened in the same keymap body"
    );
    let map = latest_after(&rpc, &mut incoming, "y").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("replaced")
    );
    assert_eq!(lines(&rpc).await, vec!["new"], "the chained rename applied");
}
