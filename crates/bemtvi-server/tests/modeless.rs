//! The modeless editing model (`'modeless'`): a GUI and web session where a
//! modifiable buffer rests in Insert. See `docs/plans/2026-10-22-modeless-editing.md`.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    attach_with_caps, command, cursor, exec_lua, feed, feed_mouse, feed_mouse_mod, feed_sync,
    field, lines, map_get, menu_items, menu_of, message_after, mode, poll_menu, poll_true,
    redraw_after, redraw_after_matching, spawn, start_clocked_init, write_temp, FakeClipboard,
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
async fn a_read_only_buffer_rests_in_normal() {
    let (rpc, _inc) = start_modeless("one\ntwo\n").await;
    command(&rpc, "setlocal nomodifiable").await;
    assert_eq!(mode(&rpc).await, "n");
    feed_sync(&rpc, "j").await;
    assert_eq!(
        lines(&rpc).await,
        v(&["one", "two"]),
        "j moved, didn't type"
    );
    command(&rpc, "setlocal modifiable").await;
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn the_quickfix_window_rests_in_normal_and_closing_it_returns_to_insert() {
    let (rpc, _inc) = start_modeless("one\ntwo\n").await;
    exec_lua(
        &rpc,
        "btv.qf.setqflist({ { filename = btv.buf.name(0), lnum = 2, text = 'x' } })",
    )
    .await;
    command(&rpc, "copen").await;
    assert_eq!(mode(&rpc).await, "n");
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
    // Typed fast, `:cclose<CR>x` arrives as one message: the cmdline closes the list
    // and lands back in the file in Normal, and the `x` right behind it must type,
    // not delete, so the mode has to settle between the keys of one message.
    let (rpc, _inc) = start_modeless("one\ntwo\n").await;
    exec_lua(
        &rpc,
        "btv.qf.setqflist({ { filename = btv.buf.name(0), lnum = 2, text = 'x' } })",
    )
    .await;
    command(&rpc, "copen").await;
    assert_eq!(mode(&rpc).await, "n");
    feed(&rpc, ":cclose<CR>x");
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
async fn vim_mode_and_a_read_only_window_have_no_caret() {
    let (rpc, mut inc) = start_as("gui", "hello\n").await;
    let map = redraw_after(&rpc, &mut inc, "i").await;
    assert!(!flag(&map, "caret"), "vim's Insert keeps vim's cursor");
    assert_eq!(text_pointers(&map), vec![(true, false)]);
    command(&rpc, "set modeless").await;
    command(&rpc, "new").await;
    command(&rpc, "setlocal nomodifiable").await;
    let map = redraw_after(&rpc, &mut inc, "").await;
    assert!(!flag(&map, "caret"), "a read-only window rests in Normal");
    let mut pointers = text_pointers(&map);
    pointers.sort();
    assert_eq!(
        pointers,
        vec![(false, true), (true, false)],
        "the I-beam over the editable window only, focused or not"
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
async fn vim_mode_and_a_read_only_window_keep_the_cursor_in_view() {
    let text: String = (0..200).map(|i| format!("line{i}\n")).collect();
    let (rpc, _inc) = start_as("gui", &text).await;
    wheel_down(&rpc, 10);
    assert!(cursor(&rpc).await.0 > 20, "vim pulls the cursor along");
    command(&rpc, "set modeless").await;
    command(&rpc, "setlocal nomodifiable").await;
    feed(&rpc, "gg");
    wheel_down(&rpc, 10);
    assert!(cursor(&rpc).await.0 > 20, "a read-only window does too");
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
    assert_eq!(
        v,
        vec![top - 1, rows, 200],
        "from the top line, a screenful of 200"
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
    assert_eq!(v[0], 0);
    assert!(v[1] < 22, "a wrapped line takes several rows: {v:?}");
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
    // A fold of 50 lines on the last row on screen.
    let rows = text_rows(&rpc).await;
    exec_lua(&rpc, &format!("btv.cmd('normal! {rows}Gzf49jgg')")).await;
    let map = redraw_after(&rpc, &mut inc, "").await;
    let (v, _) = scrollbars(&map).expect("markers");
    assert_eq!(v, vec![0, rows - 1 + 50, 200]);
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
