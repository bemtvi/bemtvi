//! The modeless editing model (`'modeless'`): a GUI and web session where a
//! modifiable buffer rests in Insert. See `docs/plans/2026-10-22-modeless-editing.md`.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    attach_with_caps, command, cursor, exec_lua, feed, feed_mouse, feed_mouse_mod, feed_sync,
    field, lines, map_get, menu_items, menu_of, message_after, mode, panel_is_open, poll_menu,
    poll_true, redraw_after, redraw_after_matching, spawn, start_clocked_init, wait_redraw,
    write_temp, FakeClipboard,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// A server editing `content`, attached as the given `client` kind.
async fn start_as(client: &str, content: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let path = write_temp("modeless", "txt", content);
    let (rpc, incoming) = spawn(ServerInit {
        file: Some(path),
        ..Default::default()
    });
    attach_with_caps(&rpc, 80, 24, caps(client)).await;
    (rpc, incoming)
}

/// The capabilities a client of kind `client` attaches with: a GUI and the browser
/// always have the keyboard protocol on, as their real clients declare.
fn caps(client: &str) -> Vec<(Value, Value)> {
    let mut caps = vec![(Value::from("client"), Value::from(client))];
    if client == "gui" || client == "web" {
        caps.push((Value::from("keyboard_protocol"), Value::Boolean(true)));
    }
    caps
}

/// A GUI session with `'modeless'` on.
async fn start_modeless(content: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (rpc, incoming) = start_as("gui", content).await;
    command(&rpc, "set modeless").await;
    (rpc, incoming)
}

async fn modeless_on(rpc: &Rpc) -> bool {
    exec_lua(rpc, "return btv.o.modeless").await.as_bool() == Some(true)
}

fn v(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn turning_it_on_rests_in_insert_and_typing_edits() {
    let (rpc, _inc) = start_modeless("hello\n").await;
    assert_eq!(mode(&rpc).await, "i");
    feed(&rpc, "dd");
    assert_eq!(lines(&rpc).await, v(&["ddhello"]));
}

#[tokio::test]
async fn esc_stays_in_insert() {
    let (rpc, _inc) = start_modeless("\n").await;
    feed(&rpc, "foo<Esc>bar");
    assert_eq!(lines(&rpc).await, v(&["foobar"]));
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn esc_in_replace_returns_to_insert() {
    let (rpc, _inc) = start_modeless("abc\n").await;
    exec_lua(&rpc, "btv.cmd('normal! R')").await;
    feed(&rpc, "<Esc>x");
    assert_eq!(mode(&rpc).await, "i");
    assert_eq!(lines(&rpc).await, v(&["xabc"]));
}

#[tokio::test]
async fn turning_it_off_leaves_insert() {
    let (rpc, _inc) = start_modeless("hello\n").await;
    command(&rpc, "set nomodeless").await;
    assert_eq!(mode(&rpc).await, "n");
    feed(&rpc, "dd");
    assert_eq!(lines(&rpc).await, v(&[""]));
}

#[tokio::test]
async fn btv_o_turns_it_on_too() {
    let (rpc, _inc) = start_as("gui", "hello\n").await;
    exec_lua(&rpc, "btv.o.modeless = true").await;
    assert_eq!(mode(&rpc).await, "i");
    assert!(modeless_on(&rpc).await);
}

#[tokio::test]
async fn the_web_and_an_unnamed_client_may_turn_it_on() {
    for client in ["web", ""] {
        let (rpc, _inc) = start_as(client, "hello\n").await;
        command(&rpc, "set modeless").await;
        assert!(modeless_on(&rpc).await, "refused for client {client:?}");
        assert_eq!(mode(&rpc).await, "i");
    }
}

#[tokio::test]
async fn the_terminal_client_refuses_it() {
    let (rpc, mut inc) = start_as("tui", "hello\n").await;
    let msg = message_after(&rpc, &mut inc, ":set modeless<CR>").await;
    assert!(msg.contains("not available in the terminal"), "{msg:?}");
    assert!(!modeless_on(&rpc).await);
    assert_eq!(mode(&rpc).await, "n");
}

#[tokio::test]
async fn a_terminal_attaching_turns_it_off() {
    // The config runs before any client attaches, so it can turn the option on; the
    // terminal arriving afterwards turns it back off and says why.
    let path = write_temp("modeless", "txt", "hello\n");
    let (rpc, mut inc) = spawn(ServerInit {
        file: Some(path),
        ..Default::default()
    });
    exec_lua(&rpc, "btv.o.modeless = true").await;
    assert!(modeless_on(&rpc).await);
    attach_with_caps(
        &rpc,
        80,
        24,
        vec![(Value::from("client"), Value::from("tui"))],
    )
    .await;
    assert!(!modeless_on(&rpc).await);
    assert_eq!(mode(&rpc).await, "n");
    let msg = message_after(&rpc, &mut inc, "").await;
    assert!(msg.contains("terminal client"), "{msg:?}");
}

#[tokio::test]
async fn ui_caps_names_the_client() {
    let (rpc, _inc) = start_as("gui", "\n").await;
    let client = exec_lua(&rpc, "return btv.ui.caps().client").await;
    assert_eq!(client.as_str(), Some("gui"));
}

#[tokio::test]
async fn helix_and_modeless_exclude_each_other() {
    let (rpc, mut inc) = start_as("gui", "hello\n").await;
    command(&rpc, "helix").await;
    let msg = message_after(&rpc, &mut inc, ":set modeless<CR>").await;
    assert!(msg.contains("Helix"), "{msg:?}");
    assert!(!modeless_on(&rpc).await);

    let (rpc, mut inc) = start_modeless("hello\n").await;
    exec_lua(&rpc, "btv.cmd('helix')").await;
    let msg = message_after(&rpc, &mut inc, "").await;
    assert!(msg.contains("modeless"), "{msg:?}");
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn a_workspace_override_is_refused() {
    let (rpc, mut inc) = start_as("gui", "hello\n").await;
    exec_lua(&rpc, "btv.wso.modeless = true").await;
    let msg = message_after(&rpc, &mut inc, "").await;
    assert!(msg.contains("session setting"), "{msg:?}");
    assert!(!modeless_on(&rpc).await);
}

#[tokio::test]
async fn an_ex_command_comes_back_to_insert() {
    let (rpc, _inc) = start_modeless("one\ntwo\n").await;
    command(&rpc, "2").await;
    assert_eq!(mode(&rpc).await, "i");
    command(&rpc, "split").await;
    assert_eq!(mode(&rpc).await, "i");
    focus_other(&rpc).await;
    assert_eq!(mode(&rpc).await, "i");
    command(&rpc, "enew").await;
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn a_read_only_buffer_rests_in_insert_and_refuses_what_would_edit() {
    let (rpc, mut inc) = start_modeless("one\ntwo\n").await;
    command(&rpc, "setlocal nomodifiable").await;
    assert_eq!(mode(&rpc).await, "i", "no Normal mode, even read-only");
    for key in ["j", "<CR>", "<BS>", "<Tab>", "<C-u>"] {
        let msg = message_after(&rpc, &mut inc, key).await;
        assert!(msg.starts_with("E21"), "{key}: {msg:?}");
    }
    assert_eq!(lines(&rpc).await, v(&["one", "two"]));
    assert_eq!(mode(&rpc).await, "i");
    command(&rpc, "setlocal modifiable").await;
    feed(&rpc, "x");
    assert_eq!(lines(&rpc).await, v(&["xone", "two"]));
}

#[tokio::test]
async fn a_read_only_buffer_moves_and_selects_like_an_editable_one() {
    let (rpc, mut inc) = start_modeless("one\ntwo\n").await;
    command(&rpc, "setlocal nomodifiable").await;
    feed(&rpc, "<Down><Right><S-Right><S-Right>");
    assert_eq!(mode(&rpc).await, "s");
    // Type-over is an edit: refused, and the selection stays.
    let msg = message_after(&rpc, &mut inc, "X").await;
    assert!(msg.starts_with("E21"), "{msg:?}");
    assert_eq!(mode(&rpc).await, "s");
    assert_eq!(lines(&rpc).await, v(&["one", "two"]));
    feed(&rpc, "<Esc>");
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn a_buffer_local_modeless_map_drives_a_read_only_buffer() {
    // The contract for a plugin's read-only surface: its keys live in the
    // `modeless` layer, buffer-local, and beat the layout's defaults there.
    let (rpc, _inc) = start_modeless("one\ntwo\n").await;
    command(&rpc, "setlocal nomodifiable").await;
    exec_lua(
        &rpc,
        "_G.hits = {} \
         btv.keymap.set('modeless', '<CR>', function() table.insert(_G.hits, 'cr') end, { buffer = 0 }) \
         btv.keymap.set('modeless', '<C-p>', function() table.insert(_G.hits, 'p') end, { buffer = 0 })",
    )
    .await;
    feed(&rpc, "<CR><C-p>");
    let hits = exec_lua(&rpc, "return table.concat(_G.hits, ',')").await;
    assert_eq!(hits.as_str(), Some("cr,p"));
    assert_eq!(mode(&rpc).await, "i");
}
#[tokio::test]
async fn the_quickfix_window_rests_in_insert_too() {
    let (rpc, _inc) = start_modeless("one\ntwo\n").await;
    exec_lua(
        &rpc,
        "btv.qf.setqflist({ { filename = btv.buf.name(0), lnum = 2, text = 'x' } })",
    )
    .await;
    command(&rpc, "copen").await;
    assert_eq!(mode(&rpc).await, "i");
    command(&rpc, "cclose").await;
    assert_eq!(mode(&rpc).await, "i");
}
#[tokio::test]
async fn keys_a_plugin_feeds_still_mean_normal_mode_after_esc() {
    // A fed `<Esc>` leaves Insert, as plugins expect, and what follows it runs as
    // Normal-mode keys. Once the run ends the session settles back into Insert.
    let (rpc, _inc) = start_modeless("one\ntwo\n").await;
    exec_lua(&rpc, "btv._feedkeys('<Esc>dd', false, false)").await;
    assert_eq!(lines(&rpc).await, v(&["two"]));
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn normal_bang_with_esc_still_runs_normal_keys() {
    let (rpc, _inc) = start_modeless("one\n").await;
    exec_lua(&rpc, "btv.cmd('normal! Afoo\\27x')").await;
    assert_eq!(lines(&rpc).await, v(&["onefo"]));
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn an_insert_mapping_to_esc_settles_back_into_insert() {
    let (rpc, _inc) = start_modeless("\n").await;
    exec_lua(&rpc, "btv.keymap.set('i', 'jk', '<Esc>')").await;
    feed(&rpc, "abjkx");
    assert_eq!(mode(&rpc).await, "i");
    let text = lines(&rpc).await;
    assert!(text[0].contains('x'), "x was typed, not run: {text:?}");
}

#[tokio::test]
async fn insert_enter_fires_once_when_it_turns_on() {
    let (rpc, _inc) = start_as("gui", "hello\n").await;
    exec_lua(
        &rpc,
        "_G.enters = 0 btv.on('InsertEnter', {}, function() _G.enters = _G.enters + 1 end)",
    )
    .await;
    command(&rpc, "set modeless").await;
    feed(&rpc, "abc");
    command(&rpc, "split").await;
    let n = exec_lua(&rpc, "return _G.enters").await;
    assert_eq!(n.as_u64(), Some(1));
}

#[tokio::test]
async fn a_key_after_leaving_a_read_only_window_types_in_the_same_message() {
    // Typed fast, `<C-;>cclose<CR>x` arrives as one message: the cmdline closes the
    // list and lands back in the file in Normal, and the `x` right behind it must
    // type, not delete, so the mode has to settle between the keys of one message.
    let (rpc, _inc) = start_modeless("one\ntwo\n").await;
    exec_lua(
        &rpc,
        "btv.qf.setqflist({ { filename = btv.buf.name(0), lnum = 2, text = 'x' } })",
    )
    .await;
    command(&rpc, "copen").await;
    feed(&rpc, "<C-;>cclose<CR>x");
    assert_eq!(lines(&rpc).await, v(&["xone", "two"]));
    assert_eq!(mode(&rpc).await, "i");
}

// ===== Phase 2: selection =====================================================

#[tokio::test]
async fn shift_right_selects_and_typing_replaces() {
    let (rpc, _inc) = start_modeless("hello\n").await;
    feed(&rpc, "<S-Right><S-Right>");
    assert_eq!(mode(&rpc).await, "s");
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["Xllo"]));
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn shift_left_selects_backwards() {
    let (rpc, _inc) = start_modeless("hello\n").await;
    feed(&rpc, "<End><S-Left><S-Left>X");
    assert_eq!(lines(&rpc).await, v(&["helX"]));
}

#[tokio::test]
async fn shift_end_and_shift_home_select_to_the_line_edges() {
    let (rpc, _inc) = start_modeless("hello world\n").await;
    feed(&rpc, "<C-Right><S-End>!");
    assert_eq!(lines(&rpc).await, v(&["hello!"]));
    feed(&rpc, "<S-Home>X");
    assert_eq!(lines(&rpc).await, v(&["X"]));
}

#[tokio::test]
async fn shift_down_selects_across_lines() {
    let (rpc, _inc) = start_modeless("one\ntwo\n").await;
    feed(&rpc, "<Right><S-Down>X");
    assert_eq!(lines(&rpc).await, v(&["oXwo"]));
}

#[tokio::test]
async fn a_selection_can_end_on_a_line_break() {
    let (rpc, _inc) = start_modeless("a\nb\n").await;
    feed(&rpc, "<End><S-Right>");
    // Drawn with the cursor on the line break it covers, not pulled back onto `a`.
    assert_eq!(cursor(&rpc).await, (1, 1));
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["aXb"]));
}

#[tokio::test]
async fn shift_down_after_shift_right_keeps_the_caret_column() {
    let (rpc, _inc) = start_modeless("abcd\nabcd\n").await;
    feed(&rpc, "<S-Right><S-Right><S-Down>X");
    assert_eq!(lines(&rpc).await, v(&["Xcd"]));
}

#[tokio::test]
async fn shrinking_a_selection_back_to_nothing_returns_to_insert() {
    let (rpc, _inc) = start_modeless("hello\n").await;
    feed(&rpc, "<S-Right><S-Left>");
    assert_eq!(mode(&rpc).await, "i");
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["Xhello"]));
}

#[tokio::test]
async fn a_plain_arrow_collapses_onto_the_side_it_points_at() {
    let (rpc, _inc) = start_modeless("hello\n").await;
    feed(&rpc, "<Right><S-Right><S-Right><Left>X");
    assert_eq!(lines(&rpc).await, v(&["hXello"]));
    let (rpc, _inc) = start_modeless("hello\n").await;
    feed(&rpc, "<Right><S-Right><S-Right><Right>X");
    assert_eq!(lines(&rpc).await, v(&["helXlo"]));
}

#[tokio::test]
async fn esc_drops_the_selection_and_keeps_the_caret_at_its_head() {
    let (rpc, _inc) = start_modeless("hello\n").await;
    feed(&rpc, "<S-Right><S-Right><Esc>");
    assert_eq!(mode(&rpc).await, "i");
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["heXllo"]));
}

#[tokio::test]
async fn backspace_and_delete_remove_the_selection() {
    let (rpc, _inc) = start_modeless("hello\n").await;
    feed(&rpc, "<S-Right><S-Right><BS>");
    assert_eq!(lines(&rpc).await, v(&["llo"]));
    assert_eq!(mode(&rpc).await, "i");
    feed(&rpc, "<S-End><Del>");
    assert_eq!(lines(&rpc).await, v(&[""]));
}

#[tokio::test]
async fn enter_replaces_the_selection_with_a_line_break() {
    let (rpc, _inc) = start_modeless("hello\n").await;
    feed(&rpc, "<Right><S-Right><S-Right><CR>");
    assert_eq!(lines(&rpc).await, v(&["h", "lo"]));
}

#[tokio::test]
async fn typing_over_a_selection_writes_no_register() {
    let (rpc, _inc) = start_modeless("hello\n").await;
    exec_lua(&rpc, "btv.reg.set('\"', 'keep')").await;
    feed(&rpc, "<S-Right><S-Right>X<S-Right><BS>");
    assert_eq!(lines(&rpc).await, v(&["Xlo"]));
    let reg = exec_lua(&rpc, "return btv.reg.get('\"')").await;
    assert_eq!(reg.as_str(), Some("keep"));
}

#[tokio::test]
async fn undo_after_typing_over_restores_the_text() {
    let (rpc, _inc) = start_modeless("hello\n").await;
    feed(&rpc, "<S-Right><S-Right>X");
    assert_eq!(lines(&rpc).await, v(&["Xllo"]));
    command(&rpc, "undo").await;
    assert_eq!(lines(&rpc).await, v(&["hello"]));
}

#[tokio::test]
async fn right_and_left_cross_line_ends() {
    let (rpc, _inc) = start_modeless("ab\ncd\n").await;
    feed(&rpc, "<End><Right>X");
    assert_eq!(lines(&rpc).await, v(&["ab", "Xcd"]));
    feed(&rpc, "<Home><Left><Left>Y");
    assert_eq!(lines(&rpc).await, v(&["aYb", "Xcd"]));
}

#[tokio::test]
async fn ctrl_arrows_move_and_select_by_word() {
    let (rpc, _inc) = start_modeless("foo bar.baz\n").await;
    feed(&rpc, "<C-Right>1");
    assert_eq!(lines(&rpc).await, v(&["foo1 bar.baz"]));
    feed(&rpc, "<C-Right>2");
    assert_eq!(lines(&rpc).await, v(&["foo1 bar2.baz"]));
    feed(&rpc, "<End><C-Left>3");
    assert_eq!(lines(&rpc).await, v(&["foo1 bar2.3baz"]));
    feed(&rpc, "<Home><C-S-Right>X");
    assert_eq!(lines(&rpc).await, v(&["X bar2.3baz"]));
}

#[tokio::test]
async fn home_toggles_between_the_first_non_blank_and_column_zero() {
    let (rpc, _inc) = start_modeless("  foo\n").await;
    feed(&rpc, "<End><Home>1");
    assert_eq!(lines(&rpc).await, v(&["  1foo"]));
    // From after the `1`: the first press lands on the first non-blank, the second
    // on column 0.
    feed(&rpc, "<Home><Home>2");
    assert_eq!(lines(&rpc).await, v(&["2  1foo"]));
}

#[tokio::test]
async fn ctrl_home_and_ctrl_end_select_to_the_buffer_ends() {
    let (rpc, _inc) = start_modeless("one\ntwo\nthree\n").await;
    feed(&rpc, "<Down><C-S-End>X");
    assert_eq!(lines(&rpc).await, v(&["one", "X"]));
    feed(&rpc, "<C-S-Home>Y");
    assert_eq!(lines(&rpc).await, v(&["Y"]));
}

#[tokio::test]
async fn up_and_down_keep_the_column() {
    let (rpc, _inc) = start_modeless("abcd\nx\nabcd\n").await;
    feed(&rpc, "<Right><Right><Right><Down><Down>X");
    assert_eq!(lines(&rpc).await, v(&["abcd", "x", "abcXd"]));
}

#[tokio::test]
async fn shift_arrows_still_only_move_without_modeless() {
    let (rpc, _inc) = start_as("gui", "hello\n").await;
    feed(&rpc, "i<S-Right><S-Right>X");
    assert_eq!(lines(&rpc).await, v(&["heXllo"]));
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn a_mouse_drag_selects_and_typing_replaces_it() {
    let (rpc, _inc) = start_modeless("hello world\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "left", "press", 0, 0);
    feed_mouse(&rpc, "left", "drag", 0, 5);
    feed_mouse(&rpc, "left", "release", 0, 5);
    assert_eq!(mode(&rpc).await, "s");
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["X world"]));
}

#[tokio::test]
async fn a_mouse_drag_past_the_line_end_takes_its_last_character() {
    let (rpc, _inc) = start_modeless("hello world\nnext\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "left", "press", 0, 6);
    feed_mouse(&rpc, "left", "drag", 0, 30);
    feed_mouse(&rpc, "left", "release", 0, 30);
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["hello X", "next"]));
}

#[tokio::test]
async fn a_mouse_drag_back_from_past_the_line_end_takes_its_last_character() {
    let (rpc, _inc) = start_modeless("hello world\nnext\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "left", "press", 0, 30);
    feed_mouse(&rpc, "left", "drag", 0, 6);
    feed_mouse(&rpc, "left", "release", 0, 6);
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["hello X", "next"]));
}

/// Held at the window's right edge, a modeless drag pans a `nowrap` view sideways
/// and the selection grows with it, a column per drag event.
#[tokio::test]
async fn a_mouse_drag_at_the_right_edge_pans_and_extends() {
    let (rpc, _inc) = start_modeless(&format!(
        "{}
",
        "x".repeat(200)
    ))
    .await;
    command(&rpc, "set nonumber norelativenumber nowrap").await;
    feed_mouse(&rpc, "left", "press", 0, 0);
    feed_mouse(&rpc, "left", "drag", 0, 79);
    feed_mouse(&rpc, "left", "drag", 0, 79);
    feed_mouse(&rpc, "left", "release", 0, 79);
    feed(&rpc, "X");
    assert_eq!(
        lines(&rpc).await,
        v(&[&format!("X{}", "x".repeat(119))]),
        "two pans past the 80 visible columns selected 81 characters"
    );
}

#[tokio::test]
async fn a_mouse_double_click_selects_the_word() {
    let path = write_temp("modeless", "txt", "hello world\n");
    let (rpc, clock, _inc) = start_clocked_init(ServerInit {
        file: Some(path),
        ..Default::default()
    })
    .await;
    command(&rpc, "set modeless nonumber norelativenumber").await;
    clock.set_ms(1_000);
    feed_mouse(&rpc, "left", "press", 0, 7);
    feed_mouse(&rpc, "left", "release", 0, 7);
    clock.set_ms(1_100);
    feed_mouse(&rpc, "left", "press", 0, 7);
    feed_mouse(&rpc, "left", "release", 0, 7);
    assert_eq!(mode(&rpc).await, "s");
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["hello X"]));
}

/// A shift-click grows a mouse selection from its anchor to the click, as a GUI
/// editor's does, rather than starting a new one.
#[tokio::test]
async fn a_shift_click_extends_a_mouse_selection() {
    let (rpc, _inc) = start_modeless("hello world foo\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "left", "press", 0, 0);
    feed_mouse(&rpc, "left", "drag", 0, 5);
    feed_mouse(&rpc, "left", "release", 0, 5);
    feed_mouse_mod(&rpc, "left", "press", "S", 0, 11);
    feed_mouse_mod(&rpc, "left", "release", "S", 0, 11);
    assert_eq!(mode(&rpc).await, "s");
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["X foo"]));
}

/// Shift-clicking before a selection's anchor flips it: the anchor stays, so the
/// selection now runs from the click to where the drag began.
#[tokio::test]
async fn a_shift_click_before_the_anchor_selects_back_to_it() {
    let (rpc, _inc) = start_modeless("hello world foo\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "left", "press", 0, 6);
    feed_mouse(&rpc, "left", "drag", 0, 11);
    feed_mouse(&rpc, "left", "release", 0, 11);
    feed_mouse_mod(&rpc, "left", "press", "S", 0, 0);
    feed_mouse_mod(&rpc, "left", "release", "S", 0, 0);
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["Xworld foo"]));
}

/// With no selection, a shift-click selects from the caret to the click.
#[tokio::test]
async fn a_shift_click_selects_from_the_caret() {
    let (rpc, _inc) = start_modeless("hello world foo\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "left", "press", 0, 6);
    feed_mouse(&rpc, "left", "release", 0, 6);
    feed_mouse_mod(&rpc, "left", "press", "S", 0, 11);
    feed_mouse_mod(&rpc, "left", "release", "S", 0, 11);
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["hello X foo"]));
}

/// A shift-click grows a keyboard selection too.
#[tokio::test]
async fn a_shift_click_extends_a_keyboard_selection() {
    let (rpc, _inc) = start_modeless("hello world foo\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed(&rpc, "<S-Right><S-Right>");
    feed_mouse_mod(&rpc, "left", "press", "S", 0, 11);
    feed_mouse_mod(&rpc, "left", "release", "S", 0, 11);
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["X foo"]));
}

#[tokio::test]
async fn a_click_drops_the_selection_and_places_the_caret() {
    let (rpc, _inc) = start_modeless("hello world\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed(&rpc, "<S-Right><S-Right>");
    feed_mouse(&rpc, "left", "press", 0, 11);
    feed_mouse(&rpc, "left", "release", 0, 11);
    assert_eq!(mode(&rpc).await, "i");
    assert_eq!(cursor(&rpc).await, (1, 11));
    feed(&rpc, "!");
    assert_eq!(lines(&rpc).await, v(&["hello world!"]));
}

// ===== Phase 3: the verbs =====================================================

/// A modeless GUI session with an in-memory clipboard.
async fn start_clip(content: &str) -> (Rpc, FakeClipboard, UnboundedReceiver<Incoming>) {
    let path = write_temp("modeless", "txt", content);
    let fake = FakeClipboard::default();
    let (rpc, incoming) = spawn(ServerInit {
        file: Some(path),
        clipboard: bemtvi_server::ClipboardProvider::Custom(Box::new(fake.clone())),
        ..Default::default()
    });
    attach_with_caps(&rpc, 80, 24, caps("gui")).await;
    command(&rpc, "set modeless").await;
    (rpc, fake, incoming)
}

async fn act(rpc: &Rpc, name: &str) {
    exec_lua(rpc, &format!("btv._modeless_action('{name}')")).await;
}

#[tokio::test]
async fn copy_puts_the_selection_on_the_clipboard_and_keeps_it() {
    let (rpc, clip, _inc) = start_clip("hello world\n").await;
    feed(&rpc, "<C-S-Right>");
    act(&rpc, "copy").await;
    assert_eq!(clip.peek(), Some(("hello".to_string(), false)));
    assert_eq!(mode(&rpc).await, "s");
}

#[tokio::test]
async fn copy_with_nothing_selected_takes_the_line() {
    let (rpc, clip, _inc) = start_clip("one\ntwo\n").await;
    feed(&rpc, "<Down><Right>");
    act(&rpc, "copy").await;
    assert_eq!(clip.peek(), Some(("two\n".to_string(), true)));
}

#[tokio::test]
async fn cut_removes_the_selection_or_the_line() {
    let (rpc, clip, _inc) = start_clip("hello world\nnext\n").await;
    feed(&rpc, "<C-S-Right>");
    act(&rpc, "cut").await;
    assert_eq!(lines(&rpc).await, v(&[" world", "next"]));
    assert_eq!(clip.peek(), Some(("hello".to_string(), false)));
    assert_eq!(mode(&rpc).await, "i");
    act(&rpc, "cut").await;
    assert_eq!(lines(&rpc).await, v(&["next"]));
    assert_eq!(clip.peek(), Some((" world\n".to_string(), true)));
}

#[tokio::test]
async fn paste_goes_in_at_the_caret_or_over_the_selection() {
    let (rpc, clip, _inc) = start_clip("hello world\n").await;
    clip.seed("XY", false);
    act(&rpc, "paste").await;
    assert_eq!(lines(&rpc).await, v(&["XYhello world"]));
    feed(&rpc, "!");
    assert_eq!(lines(&rpc).await, v(&["XY!hello world"]));
    feed(&rpc, "<C-S-Right>");
    act(&rpc, "paste").await;
    assert_eq!(lines(&rpc).await, v(&["XY!XY world"]));
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn a_whole_line_copy_pastes_above_the_caret_line() {
    let (rpc, clip, _inc) = start_clip("one\ntwo\n").await;
    clip.seed("new\n", true);
    feed(&rpc, "<Down><Right>");
    act(&rpc, "paste").await;
    assert_eq!(lines(&rpc).await, v(&["one", "new", "two"]));
    feed(&rpc, "!");
    assert_eq!(lines(&rpc).await, v(&["one", "new", "t!wo"]));
}

#[tokio::test]
async fn paste_from_an_empty_clipboard_says_so() {
    let (rpc, _clip, mut inc) = start_clip("hello\n").await;
    exec_lua(&rpc, "btv._modeless_action('paste')").await;
    let msg = message_after(&rpc, &mut inc, "").await;
    assert!(msg.contains("E353"), "{msg:?}");
    assert_eq!(lines(&rpc).await, v(&["hello"]));
}

#[tokio::test]
async fn copy_without_a_clipboard_fails_loud() {
    let (rpc, mut inc) = start_modeless("hello\n").await;
    exec_lua(&rpc, "btv._modeless_action('copy')").await;
    let msg = message_after(&rpc, &mut inc, "").await;
    assert!(msg.contains("clipboard"), "{msg:?}");
}

#[tokio::test]
async fn undo_and_redo_stay_in_insert() {
    let (rpc, _clip, _inc) = start_clip("\n").await;
    feed(&rpc, "foo bar");
    act(&rpc, "undo").await;
    assert_eq!(lines(&rpc).await, v(&["foo"]));
    assert_eq!(mode(&rpc).await, "i");
    act(&rpc, "undo").await;
    assert_eq!(lines(&rpc).await, v(&[""]));
    act(&rpc, "redo").await;
    assert_eq!(lines(&rpc).await, v(&["foo"]));
    act(&rpc, "redo").await;
    assert_eq!(lines(&rpc).await, v(&["foo bar"]));
    assert_eq!(mode(&rpc).await, "i");
    // Typing after an undo starts a new branch and undoes on its own.
    act(&rpc, "undo").await;
    feed(&rpc, "!");
    assert_eq!(lines(&rpc).await, v(&["foo!"]));
    act(&rpc, "undo").await;
    assert_eq!(lines(&rpc).await, v(&["foo"]));
}

#[tokio::test]
async fn a_paste_undoes_as_one_step() {
    let (rpc, clip, _inc) = start_clip("\n").await;
    feed(&rpc, "abc ");
    clip.seed("one two three", false);
    act(&rpc, "paste").await;
    act(&rpc, "undo").await;
    assert_eq!(lines(&rpc).await, v(&["abc "]));
}

#[tokio::test]
async fn select_all_then_typing_replaces_everything() {
    let (rpc, _clip, _inc) = start_clip("one\ntwo\n").await;
    act(&rpc, "select_all").await;
    assert_eq!(mode(&rpc).await, "s");
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["X"]));
}

#[tokio::test]
async fn delete_word_back_and_forward() {
    let (rpc, _clip, _inc) = start_clip("foo bar baz\n").await;
    feed(&rpc, "<End>");
    act(&rpc, "delete_word_back").await;
    assert_eq!(lines(&rpc).await, v(&["foo bar "]));
    feed(&rpc, "<Home>");
    act(&rpc, "delete_word_forward").await;
    assert_eq!(lines(&rpc).await, v(&[" bar "]));
    feed(&rpc, "<S-Right><S-Right>");
    act(&rpc, "delete_word_back").await;
    assert_eq!(lines(&rpc).await, v(&["ar "]));
}

#[tokio::test]
async fn tab_shifts_a_multi_line_selection_and_types_over_a_single_line_one() {
    let (rpc, _inc) = start_modeless("one\ntwo\nthree\n").await;
    command(&rpc, "set shiftwidth=2 expandtab").await;
    feed(&rpc, "<S-Down><S-Right><Tab>");
    assert_eq!(lines(&rpc).await, v(&["  one", "  two", "three"]));
    assert_eq!(mode(&rpc).await, "s", "the lines stay selected");
    feed(&rpc, "<S-Tab>");
    assert_eq!(lines(&rpc).await, v(&["one", "two", "three"]));
    feed(&rpc, "<Esc><C-End><S-Left><Tab>");
    // `expandtab`: the typed-over `<Tab>` is spaces to the next stop.
    assert_eq!(lines(&rpc).await, v(&["one", "two", "thre  "]));
}

#[tokio::test]
async fn shift_tab_outdents_the_caret_line_and_keeps_its_place() {
    let (rpc, _inc) = start_modeless("    foo\n").await;
    command(&rpc, "set shiftwidth=2 expandtab").await;
    feed(&rpc, "<End><S-Tab>!");
    assert_eq!(lines(&rpc).await, v(&["  foo!"]));
}

#[tokio::test]
async fn duplicate_line_copies_below_and_follows_it() {
    let (rpc, _clip, _inc) = start_clip("one\ntwo\n").await;
    feed(&rpc, "<Right>");
    act(&rpc, "duplicate_line").await;
    feed(&rpc, "!");
    assert_eq!(lines(&rpc).await, v(&["one", "o!ne", "two"]));
}

#[tokio::test]
async fn move_line_up_and_down_carry_the_caret_and_the_selection() {
    let (rpc, _clip, _inc) = start_clip("a\nb\nc\n").await;
    act(&rpc, "move_line_down").await;
    assert_eq!(lines(&rpc).await, v(&["b", "a", "c"]));
    feed(&rpc, "!");
    assert_eq!(lines(&rpc).await, v(&["b", "!a", "c"]));
    feed(&rpc, "<C-Home><S-Down><S-Right>");
    act(&rpc, "move_line_down").await;
    assert_eq!(lines(&rpc).await, v(&["c", "b", "!a"]));
    assert_eq!(mode(&rpc).await, "s");
    act(&rpc, "move_line_up").await;
    assert_eq!(lines(&rpc).await, v(&["b", "!a", "c"]));
    act(&rpc, "move_line_up").await;
    assert_eq!(
        lines(&rpc).await,
        v(&["b", "!a", "c"]),
        "already at the top"
    );
}

#[tokio::test]
async fn a_line_verb_undoes_as_one_step() {
    let (rpc, _clip, _inc) = start_clip("a\nb\n").await;
    feed(&rpc, "x");
    act(&rpc, "move_line_down").await;
    act(&rpc, "undo").await;
    assert_eq!(lines(&rpc).await, v(&["xa", "b"]));
}

#[tokio::test]
async fn an_unknown_action_and_one_with_modeless_off_fail_loud() {
    let (rpc, _clip, mut inc) = start_clip("a\n").await;
    exec_lua(&rpc, "btv._modeless_action('fly')").await;
    let msg = message_after(&rpc, &mut inc, "").await;
    assert!(msg.contains("unknown modeless action: fly"), "{msg:?}");
    command(&rpc, "set nomodeless").await;
    exec_lua(&rpc, "btv._modeless_action('copy')").await;
    let msg = message_after(&rpc, &mut inc, "").await;
    assert!(msg.contains("'modeless' is off"), "{msg:?}");
}

#[tokio::test]
async fn typing_after_a_verb_is_a_step_of_its_own() {
    let (rpc, clip, _inc) = start_clip("\n").await;
    feed(&rpc, "abc ");
    clip.seed("one", false);
    act(&rpc, "paste").await;
    feed(&rpc, "x");
    act(&rpc, "undo").await;
    assert_eq!(lines(&rpc).await, v(&["abc one"]));
}

#[tokio::test]
async fn a_selection_ending_at_a_line_start_does_not_take_that_line() {
    // `<S-Down>` from column 0 selects the whole first line, ending at the start of
    // the second: a line verb acts on the first line only.
    let (rpc, _clip, _inc) = start_clip("a\nb\nc\n").await;
    feed(&rpc, "<S-Down>");
    act(&rpc, "move_line_down").await;
    assert_eq!(lines(&rpc).await, v(&["b", "a", "c"]));
}

#[tokio::test]
async fn typing_after_a_cut_that_left_the_caret_in_place_is_its_own_step() {
    // The cut ends with the caret exactly where the typing before it stopped, so
    // the moved-caret rule can't be what separates the next keystroke: the text
    // having changed under it is.
    let (rpc, _clip, _inc) = start_clip("XY\n").await;
    feed(&rpc, "abc<S-Right><S-Right>");
    act(&rpc, "cut").await;
    feed(&rpc, "!");
    assert_eq!(lines(&rpc).await, v(&["abc!"]));
    act(&rpc, "undo").await;
    assert_eq!(lines(&rpc).await, v(&["abc"]));
}

// ===== Phase 4: the key layout =================================================

#[tokio::test]
async fn ctrl_z_and_ctrl_shift_z_undo_and_redo() {
    let (rpc, _inc) = start_modeless("\n").await;
    feed(&rpc, "foo bar<C-z>");
    assert_eq!(lines(&rpc).await, v(&["foo"]));
    feed(&rpc, "<C-S-z>");
    assert_eq!(lines(&rpc).await, v(&["foo bar"]));
    feed(&rpc, "<C-z><C-y>");
    assert_eq!(lines(&rpc).await, v(&["foo bar"]));
}

#[tokio::test]
async fn ctrl_c_ctrl_x_ctrl_v_and_ctrl_a() {
    let (rpc, clip, _inc) = start_clip("one two\n").await;
    feed_sync(&rpc, "<C-S-Right><C-c>").await;
    assert_eq!(clip.peek(), Some(("one".to_string(), false)));
    feed(&rpc, "<C-x>");
    assert_eq!(lines(&rpc).await, v(&[" two"]));
    feed(&rpc, "<End><C-v>");
    assert_eq!(lines(&rpc).await, v(&[" twoone"]));
    feed(&rpc, "<C-a>X");
    assert_eq!(lines(&rpc).await, v(&["X"]));
}

#[tokio::test]
async fn the_command_line_opens_from_insert_and_returns_to_it() {
    let (rpc, _inc) = start_modeless("abc\n").await;
    feed(&rpc, "<End><C-;>set shiftwidth=3<CR>");
    assert_eq!(mode(&rpc).await, "i");
    let sw = exec_lua(&rpc, "return btv.bo.shiftwidth").await;
    assert_eq!(sw.as_u64(), Some(3));
    // The caret came back where it was, at the line's end, not pulled onto `c`.
    feed(&rpc, "!");
    assert_eq!(lines(&rpc).await, v(&["abc!"]));
    feed(&rpc, "<C-;>s/b/X/<CR>");
    assert_eq!(lines(&rpc).await, v(&["aXc!"]));
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn find_selects_the_match_and_find_next_and_previous_move_through_them() {
    let (rpc, _inc) = start_modeless("foo x foo y foo\n").await;
    feed(&rpc, "<C-f>foo<CR>");
    assert_eq!(mode(&rpc).await, "s");
    feed(&rpc, "1");
    assert_eq!(lines(&rpc).await, v(&["foo x 1 y foo"]));
    feed(&rpc, "<C-g>2");
    assert_eq!(lines(&rpc).await, v(&["foo x 1 y 2"]));
    feed(&rpc, "<C-S-g>3");
    assert_eq!(lines(&rpc).await, v(&["3 x 1 y 2"]));
}

#[tokio::test]
async fn ctrl_h_opens_a_substitute() {
    let (rpc, _inc) = start_modeless("a b a\n").await;
    feed(&rpc, "<C-h>a/z/g<CR>");
    assert_eq!(lines(&rpc).await, v(&["z b z"]));
    assert_eq!(mode(&rpc).await, "i");
}

/// Find with text selected searches for that text: the selection seeds the
/// command line, escaped, so `a.b` finds `a.b` and not `axb`.
#[tokio::test]
async fn find_with_a_selection_searches_for_the_selected_text() {
    let (rpc, _inc) = start_modeless("a.b axb a.b\n").await;
    feed(&rpc, "<Home><S-Right><S-Right><S-Right><C-f><CR>");
    assert_eq!(mode(&rpc).await, "s", "the next match comes up selected");
    feed(&rpc, "1");
    assert_eq!(lines(&rpc).await, v(&["a.b axb 1"]));
}

/// Replace with text selected opens the substitute with the selected text as its
/// pattern, escaped, so only the replacement is left to type.
#[tokio::test]
async fn replace_with_a_selection_substitutes_the_selected_text() {
    let (rpc, _inc) = start_modeless("a.b axb a.b\n").await;
    feed(&rpc, "<Home><S-Right><S-Right><S-Right><C-h>z/g<CR>");
    assert_eq!(lines(&rpc).await, v(&["z axb z"]));
    assert_eq!(mode(&rpc).await, "i");
}

/// The selected text's own `/` and regex metacharacters are escaped too, so they
/// neither end the pattern nor change what it matches.
#[tokio::test]
async fn replace_escapes_the_delimiter_and_groups_in_the_selection() {
    let (rpc, _inc) = start_modeless("f(x)/2 fx2 f(x)/2\n").await;
    feed(&rpc, "<Home>");
    for _ in 0..6 {
        feed(&rpc, "<S-Right>");
    }
    feed(&rpc, "<C-h>y/g<CR>");
    assert_eq!(lines(&rpc).await, v(&["y fx2 y"]));
}

/// A selection over multi-byte text seeds whole characters and whole graphemes: an
/// accented letter, a combining sequence (`e` + U+0301) and a ZWJ family emoji are
/// each one Shift-step, and the pattern holds all their bytes, never a cut one.
#[tokio::test]
async fn find_seeds_whole_graphemes_from_a_multi_byte_selection() {
    let text = "x naïve e\u{301}👩\u{200d}👩\u{200d}👧 y naïve e\u{301}👩\u{200d}👩\u{200d}👧\n";
    let (rpc, _inc) = start_modeless(text).await;
    // Past `x `, then select `naïve é👩‍👩‍👧`: 5 letters, a space, 2 graphemes.
    feed(&rpc, "<Home><Right><Right>");
    for _ in 0..8 {
        feed(&rpc, "<S-Right>");
    }
    let sel = exec_lua(
        &rpc,
        "local s = btv.win.selection() \
         return btv.buf.text(0, s[1], s[2], s[3], s[4])[1]",
    )
    .await;
    assert_eq!(
        sel.as_str(),
        Some("naïve e\u{301}👩\u{200d}👩\u{200d}👧"),
        "the range covers whole graphemes"
    );
    feed(&rpc, "<C-f><CR>");
    assert_eq!(mode(&rpc).await, "s", "the next match comes up selected");
    feed(&rpc, "Z");
    assert_eq!(
        lines(&rpc).await,
        v(&["x naïve e\u{301}👩\u{200d}👩\u{200d}👧 y Z"])
    );
}

/// The selection's end can be a combining sequence on its own, and the pattern still
/// takes the whole grapheme: selecting just `é` (`e` + U+0301) finds the next `é`,
/// not a bare `e`.
#[tokio::test]
async fn find_takes_a_combining_sequence_whole() {
    let (rpc, _inc) = start_modeless("e\u{301} e x e\u{301}\n").await;
    feed(&rpc, "<Home><S-Right><C-f><CR>");
    feed(&rpc, "Z");
    assert_eq!(lines(&rpc).await, v(&["e\u{301} e x Z"]));
}

/// A selection over more than one line has no single-line pattern to search for, so
/// find opens empty, as with nothing selected.
#[tokio::test]
async fn find_with_a_multi_line_selection_opens_empty() {
    let (rpc, _inc) = start_modeless("one\ntwo one\n").await;
    feed(&rpc, "<Home><S-Down><C-f>one<CR>");
    assert_eq!(
        cursor(&rpc).await.0,
        2,
        "the typed pattern alone was searched"
    );
}

/// `btv.win.selection()` reads the live selection's extent: 0-based rows, byte
/// columns, end-exclusive, and nil with nothing selected.
#[tokio::test]
async fn btv_win_selection_reads_the_live_selection() {
    let (rpc, _inc) = start_modeless("hello world\nsecond\n").await;
    let none = exec_lua(&rpc, "return btv.win.selection() == nil").await;
    assert_eq!(none.as_bool(), Some(true), "nothing selected reads nil");
    feed(&rpc, "<Home><S-Right><S-Right>");
    let sel = exec_lua(&rpc, "return btv.win.selection()").await;
    let got: Vec<u64> = sel
        .as_array()
        .expect("an extent")
        .iter()
        .map(|n| n.as_u64().unwrap())
        .collect();
    assert_eq!(got, vec![0, 0, 0, 2], "`he` selected");
}

#[tokio::test]
async fn ctrl_h_is_left_to_backspace_without_the_keyboard_protocol() {
    // A client that can't tell `<C-h>` from `<BS>` gets no replace key, so
    // backspace keeps deleting.
    let (rpc, _inc) = start_as("", "ab\n").await;
    command(&rpc, "set modeless").await;
    feed(&rpc, "<End><BS>");
    assert_eq!(lines(&rpc).await, v(&["a"]));
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn the_layout_is_inert_in_a_vim_session() {
    let (rpc, _inc) = start_as("gui", "\n").await;
    feed(&rpc, "ifoo bar<C-z>");
    let text = lines(&rpc).await;
    assert!(
        text[0].starts_with("foo bar"),
        "nothing was undone: {text:?}"
    );
}

#[tokio::test]
async fn a_user_map_in_the_modeless_layer_wins_over_the_default() {
    let (rpc, _inc) = start_modeless("\n").await;
    exec_lua(
        &rpc,
        "btv.keymap.set('modeless', '<C-z>', function() btv.buf.set_lines(0, 0, -1, false, { 'mine' }) end)",
    )
    .await;
    feed(&rpc, "foo<C-z>");
    assert_eq!(lines(&rpc).await, v(&["mine"]));
}

#[tokio::test]
async fn insert_maps_still_fire_and_the_layout_beats_them_on_its_own_keys() {
    let (rpc, _inc) = start_modeless("\n").await;
    exec_lua(
        &rpc,
        "btv.keymap.set('i', '<F5>', function() btv.buf.set_lines(0, 0, -1, false, { 'f5' }) end)
         btv.keymap.set('i', '<C-z>', function() btv.buf.set_lines(0, 0, -1, false, { 'insert map' }) end)",
    )
    .await;
    feed(&rpc, "<F5>");
    assert_eq!(lines(&rpc).await, v(&["f5"]));
    feed(&rpc, "<End> more<C-z>");
    assert_eq!(
        lines(&rpc).await,
        v(&["f5"]),
        "the layout's undo, not the Insert map"
    );
}

#[tokio::test]
async fn the_palette_runs_a_command_and_prefills_one_that_needs_an_argument() {
    let (rpc, mut inc) = start_modeless("one\n").await;
    feed(&rpc, "<C-S-p>");
    poll_menu(&rpc, &mut inc).await.expect("the palette opens");
    feed(&rpc, "split");
    let map = poll_menu(&rpc, &mut inc).await.expect("filtered");
    let items = menu_items(&menu_of(&map));
    assert!(
        items.first().is_some_and(|i| i.starts_with("split")),
        "{items:?}"
    );
    feed(&rpc, "<CR>");
    assert!(
        poll_true(&rpc, "return #btv.win.list() == 2").await,
        "split ran"
    );
    assert_eq!(mode(&rpc).await, "i");

    feed(&rpc, "<C-S-p>");
    poll_menu(&rpc, &mut inc)
        .await
        .expect("the palette opens again");
    feed(&rpc, "echo");
    poll_menu(&rpc, &mut inc).await.expect("filtered");
    feed(&rpc, "<CR>");
    // The confirm opens the line a tick after the picker closes.
    for _ in 0..50 {
        if mode(&rpc).await == "c" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(mode(&rpc).await, "c", "the `:` line opened");
    // Prefilled with `echo `, it needs only the argument.
    let msg = message_after(&rpc, &mut inc, "'hi'<CR>").await;
    assert_eq!(msg, "hi");
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn find_previous_from_a_selected_match_goes_to_the_one_before_it() {
    let (rpc, _inc) = start_modeless("foo x foo y foo\n").await;
    feed(&rpc, "<C-f>foo<CR><C-g><C-S-g>X");
    assert_eq!(lines(&rpc).await, v(&["foo x X y foo"]));
}

#[tokio::test]
async fn select_mode_maps_still_fire_on_a_selection() {
    let (rpc, _inc) = start_modeless("hello\n").await;
    exec_lua(
        &rpc,
        "btv.keymap.set('s', '<F6>', function() btv.buf.set_lines(0, 0, -1, false, { 'select map' }) end)",
    )
    .await;
    feed(&rpc, "<S-Right><F6>");
    assert_eq!(lines(&rpc).await, v(&["select map"]));
}

#[tokio::test]
async fn a_command_line_opened_from_insert_in_a_vim_session_returns_to_insert() {
    let (rpc, _inc) = start_as("gui", "abc\n").await;
    feed(&rpc, "A");
    exec_lua(&rpc, "btv.cmdline.open('set shiftwidth=5')").await;
    feed(&rpc, "<CR>");
    assert_eq!(mode(&rpc).await, "i");
    feed(&rpc, "!");
    assert_eq!(lines(&rpc).await, v(&["abc!"]));
}

// ===== Phase 6: Command on macOS ==============================================

/// A modeless session attached as a GUI on macOS.
async fn start_mac(content: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let path = write_temp("modeless", "txt", content);
    let (rpc, incoming) = spawn(ServerInit {
        file: Some(path),
        ..Default::default()
    });
    let mut caps = caps("gui");
    caps.push((Value::from("platform"), Value::from("macos")));
    attach_with_caps(&rpc, 80, 24, caps).await;
    command(&rpc, "set modeless").await;
    (rpc, incoming)
}

#[tokio::test]
async fn ui_caps_names_the_platform() {
    let (rpc, _inc) = start_mac("\n").await;
    let p = exec_lua(&rpc, "return btv.ui.caps().platform").await;
    assert_eq!(p.as_str(), Some("macos"));
}

#[tokio::test]
async fn command_chords_are_keys_a_map_can_bind() {
    let (rpc, _inc) = start_as("gui", "\n").await;
    exec_lua(
        &rpc,
        "btv.keymap.set('n', '<D-k>', function() btv.buf.set_lines(0, 0, -1, false, { 'cmd k' }) end)
         btv.keymap.set('n', '<D-S-k>', function() btv.buf.set_lines(0, 0, -1, false, { 'cmd shift k' }) end)",
    )
    .await;
    feed(&rpc, "<D-k>");
    assert_eq!(lines(&rpc).await, v(&["cmd k"]));
    feed(&rpc, "<D-S-k>");
    assert_eq!(lines(&rpc).await, v(&["cmd shift k"]));
}

#[tokio::test]
async fn an_unmapped_command_chord_types_nothing() {
    let (rpc, _inc) = start_as("gui", "\n").await;
    feed(&rpc, "i<D-q>x<Esc>");
    assert_eq!(lines(&rpc).await, v(&["x"]));
    let (rpc, _inc) = start_mac("\n").await;
    feed(&rpc, "<D-q>y");
    assert_eq!(lines(&rpc).await, v(&["y"]));
}

#[tokio::test]
async fn on_a_mac_the_layout_is_on_command() {
    let (rpc, _inc) = start_mac("\n").await;
    feed(&rpc, "foo bar<D-z>");
    assert_eq!(lines(&rpc).await, v(&["foo"]));
    feed(&rpc, "<D-S-z>");
    assert_eq!(lines(&rpc).await, v(&["foo bar"]));
    feed(&rpc, "<D-a>X");
    assert_eq!(lines(&rpc).await, v(&["X"]));
    // Control is not the layout's modifier there: `<C-z>` does not undo.
    feed(&rpc, " y<C-z>");
    assert!(
        lines(&rpc).await[0].starts_with("X y"),
        "nothing was undone"
    );
}

#[tokio::test]
async fn on_a_mac_option_command_f_opens_a_substitute() {
    let (rpc, _inc) = start_mac("a b a\n").await;
    feed(&rpc, "<A-D-f>a/z/g<CR>");
    assert_eq!(lines(&rpc).await, v(&["z b z"]));
}

#[tokio::test]
async fn command_and_option_arrows_navigate_the_mac_way() {
    let (rpc, _inc) = start_mac("one two\nthree\n").await;
    feed(&rpc, "<D-Right>!");
    assert_eq!(lines(&rpc).await, v(&["one two!", "three"]));
    feed(&rpc, "<D-Left>[");
    assert_eq!(lines(&rpc).await, v(&["[one two!", "three"]));
    feed(&rpc, "<D-Down>]");
    assert_eq!(lines(&rpc).await, v(&["[one two!", "three]"]));
    feed(&rpc, "<D-Up><A-Right><A-Right>|");
    assert_eq!(lines(&rpc).await, v(&["[one| two!", "three]"]));
    feed(&rpc, "<D-S-Left>X");
    assert_eq!(lines(&rpc).await, v(&["X two!", "three]"]));
    // Option-Backspace deletes a word back: the `!` run first, then `two`.
    feed(&rpc, "<End><A-BS>");
    assert_eq!(lines(&rpc).await, v(&["X two", "three]"]));
    feed(&rpc, "<A-BS>");
    assert_eq!(lines(&rpc).await, v(&["X ", "three]"]));
}

#[tokio::test]
async fn a_second_attach_does_not_register_the_layout_twice() {
    let (rpc, _inc) = start_modeless("\n").await;
    attach_with_caps(&rpc, 80, 24, caps("gui")).await;
    let n = exec_lua(
        &rpc,
        "local n = 0
         for _, m in ipairs(btv.keymap.get('modeless')) do
           if m.lhs == '<C-z>' then n = n + 1 end
         end
         return n",
    )
    .await;
    assert_eq!(n.as_u64(), Some(1));
}

#[tokio::test]
async fn undo_twice_in_one_go_then_redo_brings_back_one_word() {
    let (rpc, _inc) = start_modeless("abc\n").await;
    feed(&rpc, "<End> and more words");
    feed(&rpc, "<C-z>");
    assert_eq!(lines(&rpc).await, v(&["abc and more"]));
    feed(&rpc, "<C-z><C-z>");
    assert_eq!(lines(&rpc).await, v(&["abc"]));
    feed(&rpc, "<C-S-z>");
    assert_eq!(lines(&rpc).await, v(&["abc and"]));
}

#[tokio::test]
async fn typing_after_a_save_stays_undoable() {
    // A save closes the undo group the session holds open. The words typed after it
    // must still be steps of their own, and an undo must never lose them.
    let (rpc, _inc) = start_modeless("\n").await;
    feed(&rpc, "one");
    command(&rpc, "write").await;
    feed(&rpc, " two three");
    feed(&rpc, "<C-z>");
    assert_eq!(lines(&rpc).await, v(&["one two"]));
    feed(&rpc, "<C-z>");
    assert_eq!(lines(&rpc).await, v(&["one"]));
    feed(&rpc, "<C-S-z><C-S-z>");
    assert_eq!(lines(&rpc).await, v(&["one two three"]));
}

#[tokio::test]
async fn typing_after_a_reload_undoes_a_word_at_a_time() {
    let (rpc, _inc) = start_modeless("abc\n").await;
    feed(&rpc, "x");
    command(&rpc, "edit!").await;
    feed(&rpc, "<End> and more");
    feed(&rpc, "<C-z>");
    assert_eq!(lines(&rpc).await, v(&["abc and"]));
}

#[tokio::test]
async fn typing_in_a_buffer_switched_to_undoes_a_word_at_a_time() {
    // The insert session outlives a buffer switch, but the undo group it holds open
    // belonged to the buffer it left: the new buffer needs one of its own.
    let (rpc, _inc) = start_modeless("abc\n").await;
    command(&rpc, "enew").await;
    feed(&rpc, "one two three");
    feed(&rpc, "<C-z>");
    assert_eq!(lines(&rpc).await, v(&["one two"]));
    feed(&rpc, "<C-z>");
    assert_eq!(lines(&rpc).await, v(&["one"]));
    feed(&rpc, "<C-z>");
    assert_eq!(lines(&rpc).await, v(&[""]));
}

#[tokio::test]
async fn ctrl_c_without_a_clipboard_says_so() {
    let (rpc, mut inc) = start_modeless("hello\n").await;
    let msg = message_after(&rpc, &mut inc, "<C-c>").await;
    assert!(msg.contains("clipboard"), "{msg:?}");
}

// ===== Undo puts the selection back ===========================================

#[tokio::test]
async fn undoing_a_type_over_selects_the_text_again() {
    let (rpc, _clip, _inc) = start_clip("hello world\n").await;
    feed(&rpc, "<C-S-Right>X");
    assert_eq!(lines(&rpc).await, v(&["X world"]));
    act(&rpc, "undo").await;
    assert_eq!(lines(&rpc).await, v(&["hello world"]));
    assert_eq!(mode(&rpc).await, "s");
    feed(&rpc, "Z");
    assert_eq!(lines(&rpc).await, v(&["Z world"]));
}

#[tokio::test]
async fn undoing_a_cut_selects_the_text_again() {
    let (rpc, _clip, _inc) = start_clip("hello world\n").await;
    feed(&rpc, "<C-S-Right>");
    act(&rpc, "cut").await;
    assert_eq!(lines(&rpc).await, v(&[" world"]));
    act(&rpc, "undo").await;
    assert_eq!(lines(&rpc).await, v(&["hello world"]));
    assert_eq!(mode(&rpc).await, "s");
    feed(&rpc, "Z");
    assert_eq!(lines(&rpc).await, v(&["Z world"]));
}

#[tokio::test]
async fn the_selection_comes_back_facing_the_way_it_was_made() {
    // Selected right to left, so the head is at the start: `<S-Left>` grows it.
    let (rpc, _clip, _inc) = start_clip("hello world\n").await;
    feed(&rpc, "<End><C-S-Left>X");
    assert_eq!(lines(&rpc).await, v(&["hello X"]));
    act(&rpc, "undo").await;
    feed(&rpc, "<S-Left>Z");
    assert_eq!(lines(&rpc).await, v(&["helloZ"]));
}

#[tokio::test]
async fn a_selection_made_after_an_undo_comes_back_too() {
    // The step it starts has changed nothing yet when the selection is made.
    let (rpc, _clip, _inc) = start_clip("\n").await;
    feed(&rpc, "foo bar");
    act(&rpc, "undo").await;
    feed(&rpc, "<S-Home>X");
    assert_eq!(lines(&rpc).await, v(&["X"]));
    act(&rpc, "undo").await;
    assert_eq!(lines(&rpc).await, v(&["foo"]));
    feed(&rpc, "Z");
    assert_eq!(lines(&rpc).await, v(&["Z"]));
}

#[tokio::test]
async fn undo_and_redo_of_an_indent_bring_back_each_side_selection() {
    let (rpc, _inc) = start_modeless("one\ntwo\nthree\n").await;
    command(&rpc, "set shiftwidth=2 expandtab").await;
    feed(&rpc, "<S-Down><S-Right><Tab>");
    assert_eq!(lines(&rpc).await, v(&["  one", "  two", "three"]));
    feed(&rpc, "<C-z>");
    assert_eq!(lines(&rpc).await, v(&["one", "two", "three"]));
    assert_eq!(mode(&rpc).await, "s");
    // Redo comes back with the whole lines the indent left selected.
    feed(&rpc, "<C-y>");
    assert_eq!(lines(&rpc).await, v(&["  one", "  two", "three"]));
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["X", "three"]));
    // And undo once more returns to the selection the indent started from.
    feed(&rpc, "<C-z><C-z>X");
    assert_eq!(lines(&rpc).await, v(&["Xwo", "three"]));
}

#[tokio::test]
async fn undoing_plain_typing_leaves_nothing_selected() {
    let (rpc, _clip, _inc) = start_clip("\n").await;
    feed(&rpc, "foo bar");
    act(&rpc, "undo").await;
    assert_eq!(mode(&rpc).await, "i");
    feed(&rpc, "!");
    assert_eq!(lines(&rpc).await, v(&["foo!"]));
}

// ===== The caret and the pointer a GUI draws ==================================

fn flag(map: &[(Value, Value)], key: &str) -> bool {
    map_get(map, key).and_then(Value::as_bool) == Some(true)
}

/// Each window's `text_pointer`, with whether it is the focused one.
fn text_pointers(map: &[(Value, Value)]) -> Vec<(bool, bool)> {
    let Some(Value::Array(wins)) = map_get(map, "windows") else {
        panic!("no windows in the redraw");
    };
    wins.iter()
        .map(|w| {
            let w = w.as_map().expect("a window is a map");
            (flag(w, "focused"), flag(w, "text_pointer"))
        })
        .collect()
}

/// The focused window's projected cursor, `(row, screen column)`.
fn cursor_cell(map: &[(Value, Value)]) -> (u64, u64) {
    let Some(Value::Array(wins)) = map_get(map, "windows") else {
        panic!("no windows in the redraw");
    };
    let w = wins
        .iter()
        .map(|w| w.as_map().expect("a window is a map"))
        .find(|w| flag(w, "focused"))
        .expect("a focused window");
    let n = |k| {
        map_get(w, k)
            .and_then(Value::as_u64)
            .expect("a cursor field")
    };
    (n("cursor_row"), n("cursor_screen_col"))
}

#[tokio::test]
async fn typing_shows_a_caret_and_selecting_puts_it_at_the_head() {
    let (rpc, mut inc) = start_modeless("hello world\nnext\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    let map = redraw_after(&rpc, &mut inc, "").await;
    assert!(flag(&map, "caret"), "typing is a caret");
    let map = redraw_after(&rpc, &mut inc, "<S-Right><S-Right>").await;
    assert!(flag(&map, "caret"), "selecting is a caret too");
    assert_eq!(
        cursor_cell(&map),
        (0, 2),
        "left to right: past the last character"
    );
    let map = redraw_after(&rpc, &mut inc, "<Esc><End><S-Left>").await;
    assert_eq!(
        cursor_cell(&map),
        (0, 10),
        "right to left: on the first one"
    );
    let map = redraw_after(&rpc, &mut inc, "<Esc><Home><S-Down>").await;
    assert_eq!(
        cursor_cell(&map),
        (1, 0),
        "over a line break: the next line's start"
    );
}

#[tokio::test]
async fn vim_mode_has_no_caret_and_a_read_only_window_has_one() {
    let (rpc, mut inc) = start_as("gui", "hello\n").await;
    let map = redraw_after(&rpc, &mut inc, "i").await;
    assert!(!flag(&map, "caret"), "vim's Insert keeps vim's cursor");
    assert_eq!(text_pointers(&map), vec![(true, false)]);
    command(&rpc, "set modeless").await;
    command(&rpc, "new").await;
    command(&rpc, "setlocal nomodifiable").await;
    let map = redraw_after(&rpc, &mut inc, "").await;
    assert!(
        flag(&map, "caret"),
        "a read-only window rests in Insert too"
    );
    let mut pointers = text_pointers(&map);
    pointers.sort();
    assert_eq!(
        pointers,
        vec![(false, true), (true, true)],
        "the I-beam over every window, read-only or not"
    );
}
#[tokio::test]
async fn page_down_and_up_slide_like_they_do_in_normal_mode() {
    let text: String = (0..200).map(|i| format!("line{i}\n")).collect();
    let (rpc, mut inc) = start_modeless(&text).await;
    fn has_scroll(m: &[(Value, Value)]) -> bool {
        matches!(field(m, "scroll"), Some(Value::Map(_)))
    }
    for key in ["<PageDown>", "<PageUp>", "<S-PageDown>"] {
        let map = redraw_after_matching(&rpc, &mut inc, key, has_scroll).await;
        assert!(has_scroll(&map), "{key} slides");
    }
}

// ===== The wheel scrolls the view, not the caret ==============================

/// A modeless session over `n` numbered lines, without the line-number gutter.
async fn start_long(n: usize) -> (Rpc, UnboundedReceiver<Incoming>) {
    let text: String = (0..n).map(|i| format!("line{i}\n")).collect();
    let (rpc, inc) = start_modeless(&text).await;
    command(&rpc, "set nonumber norelativenumber").await;
    (rpc, inc)
}

/// Focus the window that isn't focused (of two), returning the id it left.
async fn focus_other(rpc: &Rpc) -> Value {
    let left = exec_lua(rpc, "return vim.api.nvim_get_current_win()").await;
    exec_lua(
        rpc,
        "local cur = vim.api.nvim_get_current_win() \
         for _, w in ipairs(vim.api.nvim_list_wins()) do \
           if w ~= cur then btv.win.set_current(w) return end \
         end",
    )
    .await;
    let now = exec_lua(rpc, "return vim.api.nvim_get_current_win()").await;
    assert_ne!(now, left, "focus moved");
    left
}

fn wheel_down(rpc: &Rpc, notches: usize) {
    for _ in 0..notches {
        feed_mouse(rpc, "wheel", "down", 5, 5);
    }
}

/// The focused window's first visible line, 1-based.
async fn topline(rpc: &Rpc) -> i64 {
    exec_lua(rpc, "return btv.win.saveview().topline")
        .await
        .as_i64()
        .expect("a topline")
}

#[tokio::test]
async fn the_wheel_leaves_the_caret_and_the_selection_where_they_are() {
    let (rpc, mut inc) = start_long(200).await;
    feed(&rpc, "<S-Right><S-Right>");
    wheel_down(&rpc, 10);
    assert!(topline(&rpc).await > 20, "the view scrolled");
    assert_eq!(mode(&rpc).await, "s");
    assert_eq!(cursor(&rpc).await.0, 1, "the caret stayed on line 1");
    let map = redraw_after(&rpc, &mut inc, "").await;
    let Some(Value::Array(wins)) = map_get(&map, "windows") else {
        panic!("no windows");
    };
    let w = wins[0].as_map().expect("a window");
    assert!(flag(w, "cursor_hidden"), "a caret off screen isn't drawn");
    // Typing replaces the selection it shows, and brings the view back to it.
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await[0], "Xne0");
    assert_eq!(topline(&rpc).await, 1);
}

/// The slide a wheel notch starts says whether the caret is among its rows. A caret
/// already scrolled off screen is pinned to the band's edge only to keep its row
/// index small, so a client that took that row for the caret's line painted the
/// edge line's number in `CursorLineNr` for the length of the slide.
#[tokio::test]
async fn a_slide_says_when_the_caret_is_off_its_band() {
    let (rpc, mut inc) = start_long(200).await;
    fn off_band(m: &[(Value, Value)]) -> Option<bool> {
        let Some(Value::Map(s)) = field(m, "scroll") else {
            return None;
        };
        Some(map_get(s, "cursor_off_band").and_then(Value::as_bool) == Some(true))
    }
    // One wheel notch's slide: what was queued before it is dropped first, so the
    // frame read back is this notch's.
    async fn notch(rpc: &Rpc, inc: &mut UnboundedReceiver<Incoming>, dir: &str) -> Option<bool> {
        while inc.try_recv().is_ok() {}
        feed_mouse(rpc, "wheel", dir, 5, 5);
        let _ = cursor(rpc).await;
        off_band(&wait_redraw(inc, |m| off_band(m).is_some()).await)
    }
    // From the top, the caret on line 1 slides out with its line.
    assert_eq!(
        notch(&rpc, &mut inc, "down").await,
        Some(false),
        "the caret's line is in the band"
    );
    // Once it is off screen, later notches slide rows it isn't on.
    wheel_down(&rpc, 10);
    assert!(topline(&rpc).await > 20, "the view scrolled");
    assert_eq!(
        notch(&rpc, &mut inc, "down").await,
        Some(true),
        "the caret is off the band"
    );
    // Back toward the top from below, it is off the band too.
    assert_eq!(
        notch(&rpc, &mut inc, "up").await,
        Some(true),
        "the caret is off the band"
    );
}

#[tokio::test]
async fn an_arrow_after_the_wheel_moves_from_the_caret_and_shows_it() {
    let (rpc, _inc) = start_long(200).await;
    wheel_down(&rpc, 10);
    feed(&rpc, "<Down>");
    assert_eq!(cursor(&rpc).await.0, 2);
    // The view scrolls back just far enough to show the caret.
    assert_eq!(topline(&rpc).await, 2);
}

fn wheel_up(rpc: &Rpc, notches: usize) {
    for _ in 0..notches {
        feed_mouse(rpc, "wheel", "up", 5, 5);
    }
}

#[tokio::test]
async fn a_page_key_after_the_wheel_pages_from_the_caret() {
    // Where three pages down lands…
    let (rpc, _inc) = start_long(200).await;
    feed(&rpc, "<PageDown><PageDown><PageDown>");
    let want = (cursor(&rpc).await.0, topline(&rpc).await);
    // …is where two pages, a scroll back to the top, and one more page land.
    let (rpc, _inc) = start_long(200).await;
    feed(&rpc, "<PageDown><PageDown>");
    wheel_up(&rpc, 40);
    assert_eq!(topline(&rpc).await, 1, "scrolled back to the top");
    feed(&rpc, "<PageDown>");
    assert_eq!((cursor(&rpc).await.0, topline(&rpc).await), want);
}

#[tokio::test]
async fn vim_mode_pulls_the_cursor_along_and_a_read_only_window_leaves_it() {
    let text: String = (0..200).map(|i| format!("line{i}\n")).collect();
    let (rpc, _inc) = start_as("gui", &text).await;
    wheel_down(&rpc, 10);
    assert!(cursor(&rpc).await.0 > 20, "vim pulls the cursor along");
    command(&rpc, "set modeless").await;
    command(&rpc, "setlocal nomodifiable").await;
    feed(&rpc, "<C-Home>");
    assert_eq!(cursor(&rpc).await.0, 1);
    wheel_down(&rpc, 10);
    assert_eq!(
        cursor(&rpc).await.0,
        1,
        "a read-only window keeps its caret too"
    );
}

#[tokio::test]
async fn the_wheel_over_another_window_leaves_its_caret_too() {
    let (rpc, _inc) = start_long(200).await;
    // `:vsplit` focuses the new window, on the left; the pointer is over the right.
    command(&rpc, "vsplit").await;
    for _ in 0..10 {
        feed_mouse(&rpc, "wheel", "down", 5, 60);
    }
    let rows = exec_lua(
        &rpc,
        "local cur, out = vim.api.nvim_get_current_win(), {} \
         for _, w in ipairs(vim.api.nvim_list_wins()) do \
           if w ~= cur then out[#out + 1] = vim.api.nvim_win_get_cursor(w)[1] end \
         end return out[1]",
    )
    .await;
    assert_eq!(rows.as_i64(), Some(1), "its caret stayed on line 1");
}

#[tokio::test]
async fn a_caret_at_a_line_end_stays_there_across_a_window_switch() {
    let (rpc, _inc) = start_modeless("abc\n").await;
    feed(&rpc, "<End>");
    command(&rpc, "vsplit").await;
    focus_other(&rpc).await;
    focus_other(&rpc).await;
    feed(&rpc, "d");
    assert_eq!(lines(&rpc).await, v(&["abcd"]));
}

#[tokio::test]
async fn a_window_scrolled_away_stays_scrolled_when_focus_comes_back() {
    let (rpc, _inc) = start_long(200).await;
    command(&rpc, "vsplit").await;
    wheel_down(&rpc, 10);
    let top = topline(&rpc).await;
    assert!(top > 20);
    focus_other(&rpc).await;
    focus_other(&rpc).await;
    assert_eq!(topline(&rpc).await, top, "the scroll is kept");
    feed(&rpc, "X");
    assert_eq!(
        lines(&rpc).await[0],
        "Xline0",
        "and typing still goes in at the caret"
    );
    assert_eq!(topline(&rpc).await, 1);
}

#[tokio::test]
async fn a_window_scrolled_while_unfocused_keeps_its_scroll_when_focused() {
    let (rpc, _inc) = start_long(200).await;
    // `:vsplit` focuses the new window, on the left; the pointer is over the right.
    command(&rpc, "vsplit").await;
    for _ in 0..10 {
        feed_mouse(&rpc, "wheel", "down", 5, 60);
    }
    focus_other(&rpc).await;
    assert!(topline(&rpc).await > 20, "the scroll is kept");
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await[0], "Xline0");
    assert_eq!(topline(&rpc).await, 1);
}

#[tokio::test]
async fn reveal_caret_brings_a_scrolled_away_view_back() {
    // What a client asks when an input method starts composing: the composition
    // shows at the caret, so the caret has to be on screen.
    let (rpc, _inc) = start_long(200).await;
    exec_lua(&rpc, "btv.modeless.actions.reveal_caret()").await;
    assert_eq!(topline(&rpc).await, 1, "a no-op in view");
    wheel_down(&rpc, 10);
    assert!(topline(&rpc).await > 20);
    exec_lua(&rpc, "btv.modeless.actions.reveal_caret()").await;
    assert_eq!(topline(&rpc).await, 1);
    assert_eq!(cursor(&rpc).await.0, 1, "the caret didn't move");
}

// ===== Shift with the wheel scrolls sideways ==================================

/// A modeless session over `n` lines of 300 characters, without wrapping or the
/// line-number gutter.
async fn start_wide(n: usize) -> (Rpc, UnboundedReceiver<Incoming>) {
    let text: String = (0..n)
        .map(|i| format!("{i:03}{}\n", "x".repeat(297)))
        .collect();
    let (rpc, inc) = start_modeless(&text).await;
    command(&rpc, "set nonumber norelativenumber nowrap").await;
    (rpc, inc)
}

/// The focused window's first visible screen column, 0-based.
async fn leftcol(rpc: &Rpc) -> i64 {
    exec_lua(rpc, "return btv.win.saveview().leftcol")
        .await
        .as_i64()
        .expect("a leftcol")
}

#[tokio::test]
async fn shift_with_the_wheel_scrolls_sideways() {
    let (rpc, _inc) = start_wide(200).await;
    for _ in 0..5 {
        feed_mouse_mod(&rpc, "wheel", "down", "S", 5, 5);
    }
    assert_eq!(
        leftcol(&rpc).await,
        30,
        "five notches of 'mousescroll' hor:6"
    );
    assert_eq!(topline(&rpc).await, 1, "and not down");
    assert_eq!(cursor(&rpc).await, (1, 0), "the caret stayed");
    for _ in 0..2 {
        feed_mouse_mod(&rpc, "wheel", "up", "S", 5, 5);
    }
    assert_eq!(leftcol(&rpc).await, 18, "up goes left");
}

#[tokio::test]
async fn shift_with_the_wheel_still_pages_in_vim_mode() {
    let text: String = (0..200)
        .map(|i| format!("{i:03}{}\n", "x".repeat(297)))
        .collect();
    let (rpc, _inc) = start_as("gui", &text).await;
    command(&rpc, "set nowrap").await;
    feed_mouse_mod(&rpc, "wheel", "down", "S", 5, 5);
    assert!(topline(&rpc).await > 10, "a screenful down");
    assert_eq!(leftcol(&rpc).await, 0);
}

#[tokio::test]
async fn the_wheel_sideways_over_another_window_keeps_its_scroll_and_caret() {
    let (rpc, _inc) = start_wide(200).await;
    // `:vsplit` focuses the new window, on the left; the pointer is over the right.
    command(&rpc, "vsplit").await;
    for _ in 0..5 {
        feed_mouse(&rpc, "wheel", "right", 5, 60);
    }
    focus_other(&rpc).await;
    assert_eq!(leftcol(&rpc).await, 30, "the scroll is kept");
    assert_eq!(cursor(&rpc).await, (1, 0), "the caret stayed");
    feed(&rpc, "X");
    assert!(lines(&rpc).await[0].starts_with("X000"));
    // The smallest scroll that shows the caret, now on column 1.
    assert_eq!(leftcol(&rpc).await, 1, "typing brings the view back");
}

// ===== Scroll markers =========================================================

/// The focused window's `scrollbars` in `map`: its vertical `[start, shown, total]`
/// and its horizontal one, if any. `None` when the window carries no markers.
fn scrollbars(map: &[(Value, Value)]) -> Option<(Vec<u64>, Option<Vec<u64>>)> {
    let Some(Value::Array(wins)) = map_get(map, "windows") else {
        panic!("no windows in the redraw");
    };
    let w = wins
        .iter()
        .filter_map(Value::as_map)
        .find(|w| flag(w, "focused"))
        .expect("a focused window");
    let sb = map_get(w, "scrollbars")
        .filter(|v| !v.is_nil())?
        .as_map()
        .expect("a map");
    let span = |key| {
        map_get(sb, key).map(|v| {
            v.as_array()
                .expect("an array")
                .iter()
                .map(|n| n.as_u64().expect("a count"))
                .collect::<Vec<_>>()
        })
    };
    Some((span("v").expect("a vertical span"), span("h")))
}

/// The focused window's text rows.
async fn text_rows(rpc: &Rpc) -> u64 {
    exec_lua(rpc, "return vim.api.nvim_win_get_height(0)")
        .await
        .as_u64()
        .expect("a height")
}

#[tokio::test]
async fn the_markers_say_where_the_view_is_in_the_buffer() {
    let (rpc, mut inc) = start_long(200).await;
    wheel_down(&rpc, 10);
    let top = topline(&rpc).await as u64;
    let rows = text_rows(&rpc).await;
    let map = redraw_after(&rpc, &mut inc, "").await;
    let (v, h) = scrollbars(&map).expect("markers in a modeless session");
    // The wheel walks the last line up to the top row: 199 tops, then a page.
    assert_eq!(
        v,
        vec![top - 1, rows, 199 + rows],
        "from the top line, a page of the range the wheel scrolls"
    );
    let (_, h) = (v, h.expect("a sideways span under 'nowrap'"));
    assert!(
        h[1] >= h[2],
        "every line fits, so nothing to scroll to: {h:?}"
    );
}

#[tokio::test]
async fn the_sideways_marker_spans_the_widest_line_in_view() {
    let (rpc, mut inc) = start_wide(200).await;
    for _ in 0..5 {
        feed_mouse(&rpc, "wheel", "right", 5, 5);
    }
    let width = exec_lua(&rpc, "return vim.api.nvim_win_get_width(0)")
        .await
        .as_u64()
        .expect("a width");
    let map = redraw_after(&rpc, &mut inc, "").await;
    let (_, h) = scrollbars(&map).expect("markers");
    assert_eq!(h, Some(vec![30, width, 300]));
}

#[tokio::test]
async fn a_wrapped_window_has_no_sideways_marker() {
    let (rpc, mut inc) = start_wide(200).await;
    command(&rpc, "set wrap").await;
    let map = redraw_after(&rpc, &mut inc, "").await;
    let (v, h) = scrollbars(&map).expect("markers");
    assert_eq!(v[..2], [0, text_rows(&rpc).await]);
    assert_eq!(h, None);
}

#[tokio::test]
async fn vim_mode_has_no_markers() {
    let text: String = (0..200).map(|i| format!("line{i}\n")).collect();
    let (rpc, mut inc) = start_as("gui", &text).await;
    let map = redraw_after(&rpc, &mut inc, "").await;
    assert_eq!(scrollbars(&map), None);
}

#[tokio::test]
async fn a_closed_fold_counts_every_line_it_hides() {
    let (rpc, mut inc) = start_long(200).await;
    // A fold on the last row on screen, hiding the rest of the buffer.
    let rows = text_rows(&rpc).await;
    exec_lua(&rpc, &format!("btv.cmd('normal! {rows}GzfGgg')")).await;
    let map = redraw_after(&rpc, &mut inc, "").await;
    let (v, _) = scrollbars(&map).expect("markers");
    assert_eq!(v, vec![0, rows, rows], "the whole buffer is in view");
}

/// The focused window's vertical span after the wheel scrolled `notches` down.
async fn vertical_after_wheel(
    rpc: &Rpc,
    inc: &mut UnboundedReceiver<Incoming>,
    notches: usize,
) -> Vec<u64> {
    wheel_down(rpc, notches);
    let map = redraw_after(rpc, inc, "").await;
    scrollbars(&map).expect("markers").0
}

#[tokio::test]
async fn the_thumb_keeps_its_size_down_to_the_end_of_the_buffer() {
    let (rpc, mut inc) = start_long(200).await;
    let first = vertical_after_wheel(&rpc, &mut inc, 1).await;
    // Far enough to walk the last line up the window, past the buffer's end.
    let last = vertical_after_wheel(&rpc, &mut inc, 100).await;
    assert_eq!(
        (last[1], last[2]),
        (first[1], first[2]),
        "a page of the same length: {first:?} then {last:?}"
    );
    assert_eq!(
        last[0] + last[1],
        last[2],
        "at the end of the track: {last:?}"
    );
}

#[tokio::test]
async fn wrapped_lines_do_not_resize_the_thumb() {
    // Every other line wraps onto a few rows.
    let text: String = (0..200)
        .map(|i| {
            if i % 2 == 0 {
                format!("{i}\n")
            } else {
                format!("{i}{}\n", "x".repeat(200))
            }
        })
        .collect();
    let (rpc, mut inc) = start_modeless(&text).await;
    command(&rpc, "set nonumber norelativenumber wrap").await;
    let mut spans = Vec::new();
    for _ in 0..4 {
        spans.push(vertical_after_wheel(&rpc, &mut inc, 1).await);
    }
    let sizes: Vec<_> = spans.iter().map(|s| (s[1], s[2])).collect();
    assert!(
        sizes.windows(2).all(|w| w[0] == w[1]),
        "one size throughout: {spans:?}"
    );
}

// ===== Leaving and coming back keeps the caret and the selection ==============

/// Two files, the first one current with `ha` selected on its second line.
async fn select_then_open_another() -> (Rpc, FakeClipboard, UnboundedReceiver<Incoming>, String) {
    let (rpc, clip, inc) = start_clip("alpha one\nalpha two\n").await;
    let other = write_temp("modeless_other", "txt", "beta\n");
    feed(&rpc, "<Down><Right><Right><Right><S-Right><S-Right>");
    assert_eq!(mode(&rpc).await, "s");
    (rpc, clip, inc, other)
}

#[tokio::test]
async fn switching_buffers_and_back_keeps_the_selection() {
    let (rpc, _clip, _inc, other) = select_then_open_another().await;
    command(&rpc, &format!("edit {other}")).await;
    assert_eq!(mode(&rpc).await, "i");
    feed(&rpc, "<C-S-Tab>");
    assert_eq!(mode(&rpc).await, "s", "the selection is back");
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["alpha one", "alpX two"]));
    // And again with the keys both ways.
    feed(&rpc, "<S-Left>");
    feed(&rpc, "<C-Tab>");
    feed(&rpc, "<C-S-Tab>");
    feed(&rpc, "Y");
    assert_eq!(lines(&rpc).await, v(&["alpha one", "alpY two"]));
}

#[tokio::test]
async fn switching_buffers_and_back_keeps_a_caret_at_the_line_end() {
    let (rpc, _clip, _inc, other) = select_then_open_another().await;
    feed(&rpc, "<Up><End>");
    command(&rpc, &format!("edit {other}")).await;
    feed(&rpc, "<C-S-Tab>");
    feed(&rpc, "!");
    assert_eq!(lines(&rpc).await, v(&["alpha one!", "alpha two"]));
}

#[tokio::test]
async fn switching_tabs_and_back_keeps_the_selection() {
    let (rpc, _clip, _inc, other) = select_then_open_another().await;
    command(&rpc, &format!("tabedit {other}")).await;
    assert_eq!(mode(&rpc).await, "i");
    command(&rpc, "tabprevious").await;
    assert_eq!(mode(&rpc).await, "s", "the selection is back");
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["alpha one", "alpX two"]));
}

#[tokio::test]
async fn switching_windows_and_back_keeps_the_selection() {
    let (rpc, _clip, _inc, other) = select_then_open_another().await;
    let first = exec_lua(&rpc, "return btv.win.current()").await;
    command(&rpc, &format!("vsplit {other}")).await;
    assert_eq!(lines(&rpc).await, v(&["beta"]));
    exec_lua(&rpc, &format!("btv.win.set_current({first})")).await;
    assert_eq!(mode(&rpc).await, "s", "the selection is back");
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["alpha one", "alpX two"]));
}

#[tokio::test]
async fn a_selection_left_behind_is_not_restored_over_a_move() {
    // Something that moves the caret between the switch and the settle wins.
    let (rpc, _clip, _inc, other) = select_then_open_another().await;
    command(&rpc, &format!("edit {other}")).await;
    exec_lua(&rpc, "btv.cmd('bprevious'); btv.cmd('normal! gg')").await;
    assert_eq!(mode(&rpc).await, "i");
    assert_eq!(cursor(&rpc).await, (1, 0));
}

#[tokio::test]
async fn ctrl_w_in_a_second_tab_on_the_same_buffer_closes_it() {
    let (rpc, _clip, _inc) = start_clip("alpha\n").await;
    command(&rpc, "tab split").await;
    feed(&rpc, "<C-w>");
    assert_eq!(lines(&rpc).await, v(&[""]), "a fresh empty buffer is left");
    assert_eq!(mode(&rpc).await, "i");
}

// ===== The context menu (docs/plans/2026-10-25-modeless-context-menu.md) ======

#[tokio::test]
async fn a_right_click_outside_the_selection_drops_it_for_a_caret_at_the_click() {
    let (rpc, _inc) = start_modeless("hello world\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed(&rpc, "<C-S-Right>");
    assert_eq!(mode(&rpc).await, "s");
    feed_mouse(&rpc, "right", "press", 0, 9);
    assert_eq!(mode(&rpc).await, "i");
    assert_eq!(cursor(&rpc).await, (1, 9));
    feed(&rpc, "<Esc>!"); // dismiss the context menu first
    assert_eq!(lines(&rpc).await, v(&["hello wor!ld"]));
}

#[tokio::test]
async fn a_right_click_inside_the_selection_keeps_it() {
    let (rpc, _inc) = start_modeless("hello world\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed(&rpc, "<C-S-Right>");
    feed_mouse(&rpc, "right", "press", 0, 2);
    feed(&rpc, "<Esc>"); // dismiss the context menu
    assert_eq!(mode(&rpc).await, "s");
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["X world"]));
}

#[tokio::test]
async fn a_right_click_past_the_line_end_puts_the_caret_at_the_end() {
    let (rpc, _inc) = start_modeless("hello world\nnext\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "right", "press", 0, 30);
    assert_eq!(mode(&rpc).await, "i");
    assert_eq!(cursor(&rpc).await, (1, 11));
}

/// Register a `MenuPopup` handler that records where each fire saw the click.
async fn record_menu_popup(rpc: &Rpc) {
    exec_lua(
        rpc,
        "_G.popups = {}
         btv.on('MenuPopup', {}, function()
           local p = btv.getmousepos()
           table.insert(_G.popups, { p.line, p.column, btv.mode().mode })
         end)",
    )
    .await;
}

async fn popups(rpc: &Rpc) -> Value {
    exec_lua(rpc, "return _G.popups").await
}

#[tokio::test]
async fn a_right_click_fires_menu_popup_after_placing_the_caret() {
    let (rpc, _inc) = start_modeless("hello world\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    record_menu_popup(&rpc).await;
    feed_mouse(&rpc, "right", "press", 0, 4);
    assert!(
        poll_true(&rpc, "return #_G.popups == 1").await,
        "MenuPopup fired"
    );
    let fired = popups(&rpc).await;
    let first = &fired.as_array().unwrap()[0];
    let got: Vec<String> = first
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.to_string().trim_matches('"').to_string())
        .collect();
    assert_eq!(
        got,
        ["1", "5", "i"],
        "line 1, column 5 (1-based), in Insert"
    );
    assert_eq!(
        cursor(&rpc).await,
        (1, 4),
        "the caret moved before the fire"
    );
}

#[tokio::test]
async fn menu_popup_fires_in_a_vim_session_too() {
    let (rpc, _inc) = start_as("gui", "hello world\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    record_menu_popup(&rpc).await;
    feed_mouse(&rpc, "right", "press", 0, 4);
    assert!(
        poll_true(&rpc, "return #_G.popups == 1").await,
        "MenuPopup fired"
    );
}

#[tokio::test]
async fn a_mapped_right_click_or_mousemodel_extend_fires_no_menu_popup() {
    let (rpc, _inc) = start_modeless("hello world\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    record_menu_popup(&rpc).await;
    command(&rpc, "set mousemodel=extend").await;
    // Cells apart, so the second press isn't counted as a `<2-RightMouse>`.
    feed_mouse(&rpc, "right", "press", 0, 2);
    command(&rpc, "set mousemodel=popup_setpos").await;
    exec_lua(
        &rpc,
        "_G.mapped = 0
         btv.keymap.set('modeless', '<RightMouse>', function() _G.mapped = _G.mapped + 1 end)",
    )
    .await;
    feed_mouse(&rpc, "right", "press", 0, 8);
    assert!(
        poll_true(&rpc, "return _G.mapped == 1").await,
        "the map fired"
    );
    assert_eq!(exec_lua(&rpc, "return #_G.popups").await.as_u64(), Some(0));
}

/// The labels and right-hand details of the open menu: `""` is a separator row's
/// label, and `None` a row without a detail.
fn menu_rows(menu: &[(Value, Value)]) -> Vec<(String, Option<String>)> {
    let labels = menu_items(menu);
    let kinds: Vec<Option<String>> = match map_get(menu, "kinds") {
        Some(Value::Array(a)) => a.iter().map(|v| v.as_str().map(str::to_string)).collect(),
        _ => vec![None; labels.len()],
    };
    labels.into_iter().zip(kinds).collect()
}

/// Wait for a menu row's verb to reach the clipboard: the chosen row runs from the
/// menu's promise, a tick after the click.
async fn clip_eventually(clip: &FakeClipboard) -> Option<(String, bool)> {
    for _ in 0..50 {
        if let Some(got) = clip.peek() {
            return Some(got);
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    None
}

/// Wait for the buffer to read `want`, for the same reason.
async fn lines_eventually(rpc: &Rpc, want: &[&str]) -> Vec<String> {
    let mut got = lines(rpc).await;
    for _ in 0..50 {
        if got == v(want) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        got = lines(rpc).await;
    }
    got
}

#[tokio::test]
async fn a_right_click_opens_the_context_menu_at_the_pointer() {
    let (rpc, mut inc) = start_modeless("hello world\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "right", "press", 0, 4);
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("a menu opens"));
    assert_eq!(
        map_get(&menu, "editor_relative"),
        Some(&Value::Boolean(true))
    );
    let at = |k: &str| map_get(&menu, k).and_then(Value::as_u64);
    assert_eq!((at("row"), at("col")), (Some(0), Some(4)));
    let d = |s: &str| Some(s.to_string());
    assert_eq!(
        menu_rows(&menu),
        [
            ("Undo".into(), d("Ctrl+Z")),
            ("Redo".into(), d("Ctrl+Shift+Z")),
            ("".into(), None),
            ("Cut".into(), d("Ctrl+X")),
            ("Copy".into(), d("Ctrl+C")),
            ("Paste".into(), d("Ctrl+V")),
            ("Select All".into(), d("Ctrl+A")),
            ("".into(), None),
            ("Command Palette".into(), d("Ctrl+Shift+P")),
        ]
    );
}

#[tokio::test]
async fn choosing_copy_from_the_menu_copies_the_selection_clicked_in() {
    let (rpc, clip, mut inc) = start_clip("hello world\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed(&rpc, "<C-S-Right>");
    feed_mouse(&rpc, "right", "press", 0, 2);
    poll_menu(&rpc, &mut inc).await.expect("a menu opens");
    // The box's border is on (0, 2); Copy is its fifth row.
    feed_mouse(&rpc, "left", "press", 5, 5);
    feed_mouse(&rpc, "left", "release", 5, 5);
    assert_eq!(
        clip_eventually(&clip).await,
        Some(("hello".to_string(), false))
    );
    assert_eq!(lines(&rpc).await, v(&["hello world"]));
    assert_eq!(mode(&rpc).await, "s", "the selection stays");
}

#[tokio::test]
async fn choosing_paste_from_the_menu_pastes_where_you_clicked() {
    let (rpc, clip, mut inc) = start_clip("hello world\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    clip.seed("XY", false);
    feed_mouse(&rpc, "right", "press", 0, 6);
    poll_menu(&rpc, &mut inc).await.expect("a menu opens");
    // Paste is the sixth row of the box bordered at (0, 6).
    feed_mouse(&rpc, "left", "press", 6, 8);
    feed_mouse(&rpc, "left", "release", 6, 8);
    assert_eq!(
        lines_eventually(&rpc, &["hello XYworld"]).await,
        v(&["hello XYworld"])
    );
}

#[tokio::test]
async fn the_menu_shows_a_rebound_key_and_a_row_you_add() {
    let (rpc, mut inc) = start_modeless("hello world\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(
        &rpc,
        "btv.keymap.set('modeless', '<C-c>', function() end)
         btv.keymap.set('modeless', '<C-k>', btv.modeless.actions.copy)
         _G.shouted = 0
         table.insert(btv.contextmenu.groups.commands.items, {
           label = 'Shout', detail = 'loud', action = function() _G.shouted = _G.shouted + 1 end,
         })",
    )
    .await;
    feed_mouse(&rpc, "right", "press", 0, 4);
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("a menu opens"));
    let rows = menu_rows(&menu);
    assert_eq!(rows[4], ("Copy".into(), Some("Ctrl+K".into())));
    assert_eq!(rows.last().unwrap(), &("Shout".into(), Some("loud".into())));
    feed(&rpc, "G<CR>");
    assert!(poll_true(&rpc, "return _G.shouted == 1").await);
}

#[tokio::test]
async fn on_a_mac_the_details_use_the_command_symbols() {
    let path = write_temp("modeless", "txt", "hello world\n");
    let (rpc, mut inc) = spawn(ServerInit {
        file: Some(path),
        ..Default::default()
    });
    let mut mac = caps("gui");
    mac.push((Value::from("platform"), Value::from("macos")));
    attach_with_caps(&rpc, 80, 24, mac).await;
    command(&rpc, "set modeless nonumber norelativenumber").await;
    feed_mouse(&rpc, "right", "press", 0, 4);
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("a menu opens"));
    let rows = menu_rows(&menu);
    assert_eq!(rows[1], ("Redo".into(), Some("\u{21e7}\u{2318}Z".into())));
    assert_eq!(rows[4], ("Copy".into(), Some("\u{2318}C".into())));
}

/// The LSP rows show while a server is attached, and keys step over the ones no
/// attached server answers. The stand-in client answers by method, the way a real
/// one's capabilities do.
#[tokio::test]
async fn lsp_rows_show_with_a_server_and_skip_what_it_cant_do() {
    let (rpc, mut inc) = start_modeless("hello world\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(
        &rpc,
        "btv.lsp.clients = function()
           return { { supports_method = function(_, m) return m ~= 'textDocument/rename' end } }
         end",
    )
    .await;
    feed_mouse(&rpc, "right", "press", 0, 4);
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("a menu opens"));
    let labels = menu_items(&menu);
    let at = labels
        .iter()
        .position(|l| l == "Go to Definition")
        .expect("LSP rows");
    assert_eq!(
        labels[at..at + 5],
        [
            "Go to Definition",
            "Find References",
            "Rename Symbol",
            "Format Document",
            "Code Actions"
        ]
    );
    // Each shows the VS Code key the layout gives its verb.
    let hints: Vec<Option<String>> = menu_rows(&menu)[at..at + 5]
        .iter()
        .map(|(_, d)| d.clone())
        .collect();
    let d = |s: &str| Some(s.to_string());
    assert_eq!(
        hints,
        [
            d("F12"),
            d("Shift+F12"),
            d("F2"),
            d("Alt+Shift+F"),
            d("Ctrl+.")
        ]
    );
    // The first `j` lights Undo; each next one steps to the next row that can be
    // chosen. Undo, Redo, Cut, Copy, Paste, Select All, Go to Definition, Find
    // References, then Format Document: the separators and Rename Symbol are skipped.
    feed(&rpc, &"j".repeat(9));
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("still open"));
    assert_eq!(
        map_get(&menu, "selected").and_then(Value::as_u64),
        Some(at as u64 + 3),
        "keys stepped over Rename Symbol onto Format Document"
    );
}

#[tokio::test]
async fn without_a_server_there_are_no_lsp_rows() {
    let (rpc, mut inc) = start_modeless("hello world\n").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "right", "press", 0, 4);
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("a menu opens"));
    assert!(!menu_items(&menu).iter().any(|l| l.starts_with("Go to")));
}

// A read-only buffer rests in Insert and selects in Select, like an editable one.
// The verbs that don't edit work there; the rest refuse with `E21`.

/// A modeless GUI session with a clipboard, over a read-only buffer.
async fn start_read_only(content: &str) -> (Rpc, FakeClipboard, UnboundedReceiver<Incoming>) {
    let (rpc, clip, inc) = start_clip(content).await;
    command(&rpc, "set nonumber norelativenumber nomodifiable").await;
    assert_eq!(mode(&rpc).await, "i");
    (rpc, clip, inc)
}

#[tokio::test]
async fn ctrl_c_in_a_read_only_buffer_copies_the_selection_and_keeps_it() {
    let (rpc, clip, _inc) = start_read_only("hello world\n").await;
    feed_sync(&rpc, "<S-Right><S-Right><S-Right><C-c>").await;
    assert_eq!(clip.peek(), Some(("hel".to_string(), false)));
    assert_eq!(mode(&rpc).await, "s");
}
#[tokio::test]
async fn copy_in_a_read_only_buffer_with_nothing_selected_takes_the_line() {
    let (rpc, clip, _inc) = start_read_only("hello world\nnext\n").await;
    act(&rpc, "copy").await;
    assert_eq!(clip.peek(), Some(("hello world\n".to_string(), true)));
}

#[tokio::test]
async fn select_all_in_a_read_only_buffer_selects_everything() {
    let (rpc, clip, _inc) = start_read_only("hello world\nnext\n").await;
    feed(&rpc, "<C-a>");
    assert_eq!(mode(&rpc).await, "s");
    act(&rpc, "copy").await;
    assert_eq!(clip.peek(), Some(("hello world\nnext".to_string(), false)));
}

#[tokio::test]
async fn find_next_in_a_read_only_buffer_selects_the_match() {
    let (rpc, clip, _inc) = start_read_only("one two\ntwo\n").await;
    feed(&rpc, "<C-f>two<CR>");
    assert_eq!(mode(&rpc).await, "s");
    feed_sync(&rpc, "<C-g><C-c>").await;
    assert_eq!(clip.peek(), Some(("two".to_string(), false)));
    assert_eq!(cursor(&rpc).await.0, 2, "the second match, on line 2");
}
#[tokio::test]
async fn the_editing_verbs_refuse_in_a_read_only_buffer() {
    let (rpc, _clip, mut inc) = start_read_only("hello world\n").await;
    for verb in ["cut", "paste", "undo", "duplicate_line"] {
        act(&rpc, verb).await;
        let msg = message_after(&rpc, &mut inc, "").await;
        assert!(msg.starts_with("E21"), "{verb}: {msg:?}");
    }
    for key in ["<C-x>", "<C-v>", "<C-z>", "<C-S-d>", "<A-Down>"] {
        let msg = message_after(&rpc, &mut inc, key).await;
        assert!(msg.starts_with("E21"), "{key}: {msg:?}");
    }
    assert_eq!(lines(&rpc).await, v(&["hello world"]));
}
#[tokio::test]
async fn a_read_only_buffer_offers_copy_and_select_all_with_their_keys() {
    let (rpc, clip, mut inc) = start_read_only("hello world\nnext\n").await;
    let d = |s: &str| Some(s.to_string());
    feed_mouse(&rpc, "right", "press", 0, 4);
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("a menu opens"));
    assert_eq!(
        menu_rows(&menu),
        [
            ("Copy".into(), d("Ctrl+C")),
            ("Select All".into(), d("Ctrl+A"))
        ]
    );
    // Select All is the second row under the border at (0, 4).
    feed_mouse(&rpc, "left", "press", 2, 6);
    feed_mouse(&rpc, "left", "release", 2, 6);
    assert!(
        poll_true(&rpc, "return btv.mode().mode == 's'").await,
        "Select All selects"
    );
    feed_mouse(&rpc, "right", "press", 1, 2);
    poll_menu(&rpc, &mut inc).await.expect("a menu opens again");
    feed_mouse(&rpc, "left", "press", 2, 4);
    feed_mouse(&rpc, "left", "release", 2, 4);
    assert_eq!(
        clip_eventually(&clip).await,
        Some(("hello world\nnext".to_string(), false))
    );
    assert_eq!(mode(&rpc).await, "s", "the selection stays");
    assert_eq!(lines(&rpc).await, v(&["hello world", "next"]));
}
/// A bracketed paste (the GUI's Cmd+V) over a modeless selection replaces it and
/// leaves the caret after the pasted text, as typing over it does.
#[tokio::test]
async fn a_bracketed_paste_over_the_selection_leaves_the_caret_after_it() {
    let (rpc, _inc) = start_modeless("hello world\n").await;
    feed(&rpc, "<C-S-Right>");
    assert_eq!(mode(&rpc).await, "s");
    feed(&rpc, "<PasteStart>XY<PasteEnd>");
    assert_eq!(mode(&rpc).await, "i");
    feed(&rpc, "!");
    assert_eq!(lines(&rpc).await, v(&["XY! world"]));
}

/// Sideways wheel notches over a viewport of very long lines stay cheap: each notch
/// measures the visible lines only as far as it can scroll (plus the sideways
/// marker's bounded span), never every visible line in full. A regression walks
/// ~24 MB per notch and blows the budget by an order of magnitude.
#[tokio::test]
async fn sideways_wheel_over_huge_lines_is_bounded() {
    let line = "x".repeat(1_000_000);
    let text: String = (0..24).map(|_| format!("{line}\n")).collect();
    let (rpc, _inc) = start_modeless(&text).await;
    command(&rpc, "set nonumber norelativenumber nowrap").await;
    let started = std::time::Instant::now();
    for _ in 0..40 {
        feed_mouse(&rpc, "wheel", "right", 5, 5);
    }
    let lc = leftcol(&rpc).await;
    let took = started.elapsed();
    assert_eq!(lc, 240, "forty notches of 'mousescroll' hor:6");
    assert!(
        took < std::time::Duration::from_secs(5),
        "40 sideways notches over 24 MB of visible text took {took:?}"
    );
}

/// A modeless caret move (`<C-Left>`) starts a new insert where it lands, as an
/// arrow key does, so `<C-u>` stops there. It used to keep the stale start after
/// the `F`, which the `中` typed before it then straddled — `<C-u>` sliced the line
/// inside the character and panicked the server.
#[tokio::test]
async fn a_caret_move_restarts_the_insert_for_ctrl_u() {
    let (rpc, _inc) = start_modeless("hello world\n").await;
    feed(&rpc, "<C-a>FA<C-Left>\u{4e2d}x<C-u>");
    assert_eq!(lines(&rpc).await, vec!["FA"]);
}

/// `<C-]>` indents the caret's line under an open insert: where the insert began
/// moves right with the text (as vim's change_indent moves it), so `<C-u>` still
/// removes only the typed `x`. Left at byte 3 it fell inside the shifted `中`.
#[tokio::test]
async fn indenting_shifts_the_insert_start_with_the_text() {
    let (rpc, _inc) = start_modeless("\u{4e2d}\n").await;
    feed(&rpc, "<End>x<C-]><C-u>");
    assert_eq!(lines(&rpc).await, vec!["\t\u{4e2d}"]);
}

/// A chunk switching models turns one off and the other on, in the order it wrote them,
/// though options and Helix actions drain from queues of their own.
#[tokio::test]
async fn switching_between_modeless_and_helix_in_one_chunk() {
    let (rpc, _incoming) = start_as("gui", "text\n").await;
    exec_lua(&rpc, "btv.o.modeless = true").await;
    assert_eq!(mode(&rpc).await, "i");

    exec_lua(&rpc, "btv.o.modeless = false; btv.helix.enable()").await;
    assert_eq!(mode(&rpc).await, "hn", "modeless to Helix");

    exec_lua(&rpc, "btv.helix.disable(); btv.o.modeless = true").await;
    assert_eq!(mode(&rpc).await, "i", "Helix to modeless");
}

// ===== Bundled read-only surfaces =============================================
// Each first-party read-only surface binds its keys in the `modeless` layer too, so
// a modeless session drives it with the same keys a vim one does.

/// Poll until the current buffer's lines satisfy `pred` (an async listing or jump).
async fn wait_for_lines(rpc: &Rpc, what: &str, pred: impl Fn(&[String]) -> bool) -> Vec<String> {
    for _ in 0..150 {
        let got = lines(rpc).await;
        if pred(&got) {
            return got;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}: {:?}", lines(rpc).await);
}

/// Put the cursor on the first line that reads `text`.
async fn cursor_to_line(rpc: &Rpc, text: &str) {
    let row = lines(rpc)
        .await
        .iter()
        .position(|l| l == text)
        .unwrap_or_else(|| panic!("no line {text:?}"));
    exec_lua(rpc, &format!("btv.cursor.set({{ {}, 0 }})", row + 1)).await;
}

#[tokio::test]
async fn enter_in_the_quickfix_list_jumps() {
    let (rpc, _inc) = start_modeless("one\ntwo\n").await;
    exec_lua(
        &rpc,
        "btv.qf.setqflist({ { filename = btv.buf.name(0), lnum = 2, text = 'x' } })",
    )
    .await;
    command(&rpc, "copen").await;
    feed_sync(&rpc, "<CR>").await;
    assert_eq!(cursor(&rpc).await.0, 2, "on the entry's line");
    assert_eq!(lines(&rpc).await, v(&["one", "two"]), "in the file");
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn q_and_esc_dismiss_a_panel() {
    let (rpc, _inc) = start_modeless("one\n").await;
    for key in ["q", "<Esc>"] {
        command(&rpc, "registers").await;
        assert!(panel_is_open(&rpc).await, "{key}: the listing opened");
        feed_sync(&rpc, key).await;
        assert!(!panel_is_open(&rpc).await, "{key} closed it");
        assert_eq!(mode(&rpc).await, "i");
    }
}

#[tokio::test]
async fn enter_in_the_buffer_list_switches_to_the_buffer() {
    let other = write_temp("modeless_other", "txt", "other\n");
    let (rpc, _inc) = start_modeless("one\n").await;
    command(&rpc, &format!("edit {other}")).await;
    command(&rpc, "buffer 1").await;
    assert_eq!(lines(&rpc).await, v(&["one"]));
    command(&rpc, "ls").await;
    let listing = lines(&rpc).await;
    let row = listing
        .iter()
        .position(|l| l.contains("modeless_other"))
        .unwrap_or_else(|| panic!("the other buffer is listed: {listing:?}"));
    exec_lua(&rpc, &format!("btv.cursor.set({{ {}, 0 }})", row + 1)).await;
    feed_sync(&rpc, "<CR>").await;
    wait_for_lines(&rpc, "the other buffer", |ls| ls == ["other"]).await;
    assert!(!panel_is_open(&rpc).await);
}

#[tokio::test]
async fn c_in_the_message_log_clears_it() {
    let (rpc, _inc) = start_modeless("one\n").await;
    exec_lua(&rpc, "btv.notify('remember me')").await;
    command(&rpc, "messages").await;
    assert!(lines(&rpc).await.iter().any(|l| l.contains("remember me")));
    feed_sync(&rpc, "C").await;
    wait_for_lines(&rpc, "an empty log", |ls| {
        !ls.iter().any(|l| l.contains("remember me"))
    })
    .await;
}

#[tokio::test]
async fn enter_and_double_click_confirm_a_view_row() {
    let (rpc, clock, _inc) = start_clocked_init(ServerInit::default()).await;
    command(&rpc, "set modeless").await;
    exec_lua(
        &rpc,
        r#"vw = btv.view.create{}
           vw:set_lines{ "alpha", "beta", "gamma" }
           vw:on_select(function(line) _G.picked = line end)
           vw:mount{ dock = "left", size = 20 }"#,
    )
    .await;
    assert_eq!(mode(&rpc).await, "i");
    feed_sync(&rpc, "<Down><CR>").await;
    assert_eq!(exec_lua(&rpc, "return _G.picked").await.as_u64(), Some(2));
    clock.set_ms(1_000);
    feed_mouse(&rpc, "left", "press", 2, 2);
    feed_mouse(&rpc, "left", "release", 2, 2);
    clock.set_ms(1_100);
    feed_mouse(&rpc, "left", "press", 2, 2);
    feed_mouse(&rpc, "left", "release", 2, 2);
    assert!(
        poll_true(&rpc, "return _G.picked == 3").await,
        "the double-click confirmed row 3"
    );
}

#[tokio::test]
async fn the_explorer_opens_entries_and_goes_up() {
    let dir = bemtvi_test_harness::temp_dir("modeless_explorer");
    std::fs::write(dir.join("alpha.txt"), "a\n").expect("write alpha");
    std::fs::create_dir_all(dir.join("sub")).expect("mkdir sub");
    std::fs::write(dir.join("sub").join("inner.txt"), "i\n").expect("write inner");
    let (rpc, _inc) = start_modeless("one\n").await;
    let sub = dir.join("sub").display().to_string().replace('\\', "/");
    exec_lua(&rpc, &format!("btv.cmd('edit {sub}')")).await;
    wait_for_lines(&rpc, "the sub listing", |ls| {
        ls.iter().any(|l| l == "inner.txt")
    })
    .await;
    assert_eq!(mode(&rpc).await, "i");
    feed_sync(&rpc, "-").await;
    wait_for_lines(&rpc, "the parent listing", |ls| {
        ls.iter().any(|l| l == "alpha.txt")
    })
    .await;
    cursor_to_line(&rpc, "alpha.txt").await;
    feed_sync(&rpc, "<CR>").await;
    wait_for_lines(&rpc, "alpha.txt opened", |ls| ls == ["a"]).await;
    assert_eq!(mode(&rpc).await, "i");
}

// ===== The VS Code language keys ==============================================
// The language verbs are Normal-mode letters in vim (`gd`, `gr`, `K`, `]d`), which a
// modeless session can't reach. The layout gives them VS Code's keys instead.

/// Replace each `btv.lsp` verb with a recorder, so a test sees which verb a key ran.
async fn record_lsp_verbs(rpc: &Rpc) {
    exec_lua(
        rpc,
        "_G.ran = {} \
         for _, v in ipairs({ 'definition', 'implementation', 'references', 'hover', \
           'signature_help', 'rename', 'code_action', 'format', 'document_symbol', \
           'workspace_symbol' }) do \
           btv.lsp[v] = function() table.insert(_G.ran, v) end \
         end",
    )
    .await;
}

#[tokio::test]
async fn the_vs_code_keys_run_the_language_verbs() {
    let (rpc, _inc) = start_modeless("hello world\n").await;
    record_lsp_verbs(&rpc).await;
    for (key, verb) in [
        ("<F12>", "definition"),
        ("<C-F12>", "implementation"),
        ("<S-F12>", "references"),
        ("<C-k><C-i>", "hover"),
        ("<C-S-Space>", "signature_help"),
        ("<F2>", "rename"),
        ("<C-.>", "code_action"),
        ("<S-A-f>", "format"),
        ("<C-S-o>", "document_symbol"),
        ("<C-t>", "workspace_symbol"),
    ] {
        exec_lua(&rpc, "_G.ran = {}").await;
        feed_sync(&rpc, key).await;
        let ran = exec_lua(&rpc, "return table.concat(_G.ran, ',')").await;
        assert_eq!(ran.as_str(), Some(verb), "{key}");
    }
    assert_eq!(lines(&rpc).await, v(&["hello world"]), "nothing typed");
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn f8_and_shift_f8_walk_the_diagnostics() {
    let (rpc, _inc) = start_modeless("one\ntwo\nthree\nfour\n").await;
    // A modeless session is always in Insert, where the default holds diagnostic
    // updates until typing goes quiet; land these at once rather than wait it out.
    exec_lua(&rpc, "btv.diagnostic.config({ update_in_insert = true })").await;
    exec_lua(
        &rpc,
        "local ns = btv.ns.create('modeless-f8') \
         btv.diagnostic.set(ns, 0, { \
           { lnum = 1, col = 0, message = 'a', severity = 1 }, \
           { lnum = 3, col = 0, message = 'b', severity = 1 }, \
         })",
    )
    .await;
    feed_sync(&rpc, "<F8>").await;
    assert_eq!(cursor(&rpc).await.0, 2, "F8: the next diagnostic, line 2");
    feed_sync(&rpc, "<F8>").await;
    assert_eq!(cursor(&rpc).await.0, 4, "F8 again: line 4");
    feed_sync(&rpc, "<S-F8>").await;
    assert_eq!(cursor(&rpc).await.0, 2, "Shift+F8: back to line 2");
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn the_language_keys_work_in_a_read_only_buffer_too() {
    let (rpc, _inc) = start_modeless("hello\n").await;
    command(&rpc, "setlocal nomodifiable").await;
    record_lsp_verbs(&rpc).await;
    feed_sync(&rpc, "<F12>").await;
    let ran = exec_lua(&rpc, "return table.concat(_G.ran, ',')").await;
    assert_eq!(ran.as_str(), Some("definition"));
}

// ===== Read-only refusal: keys that reach the core from a span ==================

#[tokio::test]
async fn a_mapping_that_types_cannot_edit_a_read_only_buffer() {
    // A mapping's right-hand side runs inside an atomic span, where typing rules
    // stand down; the edit must still be refused.
    let (rpc, _inc) = start_modeless("one\ntwo\n").await;
    command(&rpc, "setlocal nomodifiable").await;
    exec_lua(
        &rpc,
        "btv.keymap.set('modeless', '<F5>', 'zz<CR>', { buffer = 0 })",
    )
    .await;
    feed_sync(&rpc, "<F5>").await;
    assert_eq!(lines(&rpc).await, v(&["one", "two"]));
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn fed_keys_cannot_edit_a_read_only_buffer() {
    let (rpc, _inc) = start_modeless("one\ntwo\n").await;
    command(&rpc, "setlocal nomodifiable").await;
    exec_lua(&rpc, "btv._feedkeys('abc', false, false)").await;
    assert_eq!(lines(&rpc).await, v(&["one", "two"]));
    feed(&rpc, "<S-Right><S-Right>");
    assert_eq!(mode(&rpc).await, "s");
    exec_lua(&rpc, "btv._feedkeys('q', false, false)").await;
    assert_eq!(lines(&rpc).await, v(&["one", "two"]), "no type-over either");
}

#[tokio::test]
async fn focusing_a_live_terminal_from_insert_rests_in_the_job() {
    let (rpc, _inc) = start_modeless("one\n").await;
    let cat = if cfg!(windows) {
        "cmd /c type con"
    } else {
        "cat"
    };
    command(&rpc, "split").await;
    command(&rpc, &format!("terminal {cat}")).await;
    assert_eq!(mode(&rpc).await, "t");
    focus_other(&rpc).await;
    assert_eq!(mode(&rpc).await, "i");
    focus_other(&rpc).await;
    assert_eq!(mode(&rpc).await, "t", "back on the terminal");
}

#[tokio::test]
async fn a_middle_click_cannot_paste_into_a_read_only_buffer() {
    let (rpc, clip, mut inc) = start_clip("one\ntwo\n").await;
    command(&rpc, "setlocal nomodifiable").await;
    clip.seed("PASTED", false);
    feed_mouse(&rpc, "middle", "press", 0, 1);
    feed_mouse(&rpc, "middle", "release", 0, 1);
    let msg = message_after(&rpc, &mut inc, "<Right>").await;
    assert_eq!(lines(&rpc).await, v(&["one", "two"]));
    assert!(msg.starts_with("E21"), "{msg:?}");
}

// ===== Turning 'modeless' off brings the view back to the cursor ===============

#[tokio::test]
async fn turning_it_off_after_scrolling_away_brings_the_cursor_into_view() {
    // Vim keeps its cursor in view; a modeless scroll that left the caret behind
    // must not carry over once the session is a vim one again.
    let (rpc, _inc) = start_wide(200).await;
    for _ in 0..30 {
        feed_mouse_mod(&rpc, "wheel", "down", "S", 5, 5);
    }
    assert!(
        leftcol(&rpc).await > 100,
        "scrolled sideways, caret left behind"
    );
    for _ in 0..10 {
        wheel_down(&rpc, 1);
    }
    assert!(topline(&rpc).await > 1, "and down");
    exec_lua(&rpc, "btv.o.modeless = false").await;
    assert_eq!(mode(&rpc).await, "n");
    assert_eq!(cursor(&rpc).await, (1, 0));
    assert_eq!(leftcol(&rpc).await, 0, "the view came back to the cursor");
    assert_eq!(topline(&rpc).await, 1);
}

// ===== Read-only refusal: keys that don't edit still run from a span ============

#[tokio::test]
async fn a_mapping_through_ctrl_o_still_moves_in_a_read_only_buffer() {
    // `<C-o>` runs one Normal-mode command, whose own edits check 'modifiable'; it
    // edits nothing itself, so a read-only buffer's map may use it to reach a
    // Normal-mode motion.
    let (rpc, mut inc) = start_modeless("one\ntwo\nthree\n").await;
    command(&rpc, "setlocal nomodifiable").await;
    exec_lua(
        &rpc,
        "btv.keymap.set('modeless', '<F6>', '<C-o>G', { buffer = 0 })",
    )
    .await;
    let msg = message_after(&rpc, &mut inc, "<F6>").await;
    assert!(!msg.starts_with("E21"), "{msg:?}");
    assert_eq!(cursor(&rpc).await.0, 3, "on the last line");
    assert_eq!(lines(&rpc).await, v(&["one", "two", "three"]));
    assert_eq!(mode(&rpc).await, "i");
    // A Normal command that edits is still refused, by Normal's own check.
    exec_lua(
        &rpc,
        "btv.keymap.set('modeless', '<F7>', '<C-o>dd', { buffer = 0 })",
    )
    .await;
    let msg = message_after(&rpc, &mut inc, "<F7>").await;
    assert!(msg.starts_with("E21"), "{msg:?}");
    assert_eq!(lines(&rpc).await, v(&["one", "two", "three"]));
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn turning_it_off_over_a_read_only_selection_cannot_type_over_it() {
    // A modeless selection held when the session goes back to the vim model must
    // not carry the modeless type-over with it, where no read-only guard applies.
    let (rpc, _inc) = start_modeless("one\ntwo\n").await;
    command(&rpc, "setlocal nomodifiable").await;
    feed(&rpc, "<S-Right><S-Right>");
    assert_eq!(mode(&rpc).await, "s");
    exec_lua(&rpc, "btv.o.modeless = false").await;
    assert_eq!(mode(&rpc).await, "n");
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["one", "two"]));
    assert_eq!(mode(&rpc).await, "n");
}

#[tokio::test]
async fn a_map_that_turns_it_off_leaves_insert_once_it_returns() {
    // A map's Lua runs inside the mapping's span, where the mode stays as its keys
    // set it; turning 'modeless' off there must still end in Normal, or a read-only
    // buffer is left in a vim Insert nothing guards.
    let (rpc, _inc) = start_modeless("one\ntwo\n").await;
    command(&rpc, "setlocal nomodifiable").await;
    exec_lua(
        &rpc,
        "btv.keymap.set('modeless', '<F9>', function() btv.o.modeless = false end, { buffer = 0 })",
    )
    .await;
    feed_sync(&rpc, "<F9>").await;
    assert!(!modeless_on(&rpc).await);
    assert_eq!(mode(&rpc).await, "n");
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, v(&["one", "two"]));
}

#[tokio::test]
async fn ctrl_o_undo_cannot_edit_a_read_only_buffer() {
    // `<C-o>` reaches a Normal command; Normal's `u` must refuse there as the
    // layout's undo does.
    let (rpc, mut inc) = start_modeless("one\n").await;
    feed(&rpc, "abc");
    assert_eq!(lines(&rpc).await, v(&["abcone"]));
    command(&rpc, "setlocal nomodifiable").await;
    let msg = message_after(&rpc, &mut inc, "<C-o>u").await;
    assert!(msg.starts_with("E21"), "{msg:?}");
    assert_eq!(lines(&rpc).await, v(&["abcone"]));
    let msg = message_after(&rpc, &mut inc, "<C-o><C-r>").await;
    assert!(msg.starts_with("E21"), "{msg:?}");
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn a_completion_accept_cannot_edit_a_read_only_buffer() {
    // A read-only buffer rests in Insert here, so an Insert map that accepts a
    // completion reaches it, as does the manual trigger; the accept writes the
    // buffer without passing a key the refusal sees.
    let (rpc, mut inc) = start_modeless("AN_OTHER\nAN\n").await;
    exec_lua(
        &rpc,
        "btv.complete.setup { sources = { { 'buffer', min_chars = 2 } }, auto = false } \
         btv.keymap.set('i', '<C-j>', function() btv.complete.accept('insert') end)",
    )
    .await;
    command(&rpc, "setlocal nomodifiable").await;
    feed(&rpc, "<Down><End>");
    exec_lua(&rpc, "btv.complete.trigger()").await;
    poll_menu(&rpc, &mut inc).await.expect("the popup opens");
    feed(&rpc, "<Down>");
    let msg = message_after(&rpc, &mut inc, "<C-j>").await;
    assert!(msg.starts_with("E21"), "{msg:?}");
    assert_eq!(lines(&rpc).await, v(&["AN_OTHER", "AN"]));
}

#[tokio::test]
async fn a_map_that_turns_it_off_then_enters_insert_stays_in_insert() {
    // The leave a span owes is for the resting Insert it turned the option off in.
    // A map whose own keys left that Insert and then entered Insert again asked for
    // the Insert it ends in, as any vim map may.
    let (rpc, _inc) = start_modeless("one\ntwo\n").await;
    exec_lua(
        &rpc,
        "btv.keymap.set('modeless', '<F4>', '<Esc>:set nomodeless<CR>i')",
    )
    .await;
    feed_sync(&rpc, "<F4>").await;
    assert!(!modeless_on(&rpc).await);
    assert_eq!(mode(&rpc).await, "i");
    feed(&rpc, "Z");
    assert_eq!(lines(&rpc).await, v(&["Zone", "two"]));
}

#[tokio::test]
async fn redo_and_undo_tree_travel_cannot_edit_a_read_only_buffer() {
    // A change undone while the buffer could still change leaves something to redo,
    // and a history to travel; neither may rewrite the text once it is read-only.
    let (rpc, mut inc) = start_modeless("one\n").await;
    feed(&rpc, "abc");
    assert_eq!(lines(&rpc).await, v(&["abcone"]));
    command(&rpc, "undo").await;
    assert_eq!(lines(&rpc).await, v(&["one"]));
    command(&rpc, "setlocal nomodifiable").await;
    let msg = message_after(&rpc, &mut inc, "<C-o><C-r>").await;
    assert!(msg.starts_with("E21"), "{msg:?}");
    assert_eq!(lines(&rpc).await, v(&["one"]), "redo");
    for travel in ["undo 1", "later", "later 1f"] {
        command(&rpc, travel).await;
        assert_eq!(lines(&rpc).await, v(&["one"]), ":{travel}");
    }
    command(&rpc, "setlocal modifiable").await;
    command(&rpc, "redo").await;
    assert_eq!(lines(&rpc).await, v(&["abcone"]), "the redo is still there");
}

#[tokio::test]
async fn turning_it_off_while_ctrl_o_waits_cannot_resume_typing_into_a_read_only_buffer() {
    // `<C-o>`'s one command resumes the Insert it left. Once the session is a vim
    // one that Insert is no modeless rest, but over a read-only buffer it still
    // refuses each typed key, as vim's does.
    let (rpc, mut inc) = start_modeless("one two\n").await;
    command(&rpc, "setlocal nomodifiable").await;
    feed(&rpc, "<C-o>");
    exec_lua(&rpc, "btv.o.modeless = false").await;
    feed(&rpc, "l");
    assert_eq!(mode(&rpc).await, "i");
    let msg = message_after(&rpc, &mut inc, "D").await;
    assert!(msg.starts_with("E21"), "{msg:?}");
    assert_eq!(lines(&rpc).await, v(&["one two"]));
    feed(&rpc, "<C-[>");
    assert_eq!(mode(&rpc).await, "n", "<C-[> still leaves");
}

#[tokio::test]
async fn a_vim_insert_refuses_typing_into_a_buffer_ctrl_o_made_read_only() {
    let (rpc, _inc) = start_as("gui", "one\n").await;
    feed_sync(&rpc, "i<C-o>:setlocal nomodifiable<CR>abc<CR><BS>").await;
    assert_eq!(lines(&rpc).await, v(&["one"]));
    assert_eq!(mode(&rpc).await, "i");
    feed(&rpc, "<Esc>");
    assert_eq!(mode(&rpc).await, "n");
}

#[tokio::test]
async fn turning_it_off_while_the_command_line_is_open_closes_it_to_normal() {
    // A command line the layout opened from the resting Insert goes back to it.
    // Once the session is a vim one there is no resting Insert to go back to, and
    // over a read-only buffer a vim Insert has no guard.
    let (rpc, _inc) = start_modeless("one\n").await;
    command(&rpc, "setlocal nomodifiable").await;
    feed(&rpc, "<C-;>");
    assert_eq!(mode(&rpc).await, "c");
    exec_lua(&rpc, "btv.o.modeless = false").await;
    feed(&rpc, "<Esc>");
    assert_eq!(mode(&rpc).await, "n");
    feed(&rpc, "\"abc");
    assert_eq!(lines(&rpc).await, v(&["one"]));
}

#[tokio::test]
async fn ctrl_o_does_not_make_the_typing_before_it_a_repeatable_command() {
    // The resting Insert was never entered with `i`, so the text typed in it is no
    // Normal-mode change: `.` must not replay it as commands (`dd` would delete).
    let (rpc, _inc) = start_modeless("one\ntwo\n").await;
    feed(&rpc, "dd<C-o>l");
    assert_eq!(lines(&rpc).await, v(&["ddone", "two"]));
    feed(&rpc, "<C-o>.");
    assert_eq!(lines(&rpc).await, v(&["ddone", "two"]), "<C-o>.");
    exec_lua(&rpc, "btv.o.modeless = false").await;
    feed(&rpc, ".");
    assert_eq!(lines(&rpc).await, v(&["ddone", "two"]), "a vim `.`");
}

#[tokio::test]
async fn ctrl_o_still_repeats_the_last_normal_change() {
    let (rpc, _inc) = start_modeless("abc\n").await;
    feed(&rpc, "<C-o>~");
    assert_eq!(lines(&rpc).await, v(&["Abc"]));
    feed(&rpc, "xy<C-o>.");
    assert_eq!(lines(&rpc).await, v(&["AxyBc"]));
}
