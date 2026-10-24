use crate::support::*;

// ----- Undo grouping --------------------------------------------------------
//
// An insert session is not one undo step: typing breaks into word-sized steps the
// way a GUI editor's does (docs/plans/2026-10-21-standard-undo-grouping.md). A
// step ends where a word ends (the separator starts the next step), after a line
// break, and where typing turns into deleting or back. Whole operations — a
// paste, a dot-repeat, a macro, `:normal`, `:g` — stay one step.

/// The paste notation a client sends (`<PasteStart>…<PasteEnd>`).
fn paste(text: &str) -> String {
    bemtvi_view::encode_paste(text)
}

/// Feed `u` and return the buffer it leaves.
async fn undo(rpc: &Rpc) -> Vec<String> {
    feed_sync(rpc, "u").await;
    lines(rpc).await
}

/// Feed `<C-r>` and return the buffer it leaves.
async fn redo(rpc: &Rpc) -> Vec<String> {
    feed_sync(rpc, "<C-r>").await;
    lines(rpc).await
}

fn v(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn typing_undoes_a_word_at_a_time() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar baz<Esc>");
    assert_eq!(lines(&rpc).await, v(&["foo bar baz"]));

    assert_eq!(
        undo(&rpc).await,
        v(&["foo bar"]),
        "the separator goes with the word after it"
    );
    assert_eq!(undo(&rpc).await, v(&["foo"]));
    assert_eq!(undo(&rpc).await, v(&[""]));
}

#[tokio::test]
async fn redo_replays_the_same_word_steps() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar<Esc>uu");
    assert_eq!(lines(&rpc).await, v(&[""]));

    assert_eq!(redo(&rpc).await, v(&["foo"]));
    assert_eq!(redo(&rpc).await, v(&["foo bar"]));
}

#[tokio::test]
async fn punctuation_ends_a_word() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo.bar(x)<Esc>");

    assert_eq!(undo(&rpc).await, v(&["foo.bar(x"]));
    assert_eq!(undo(&rpc).await, v(&["foo.bar"]));
    assert_eq!(undo(&rpc).await, v(&["foo"]));
}

#[tokio::test]
async fn a_run_of_separators_stays_with_the_next_word() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo   bar<Esc>");

    assert_eq!(
        undo(&rpc).await,
        v(&["foo"]),
        "only the first separator after a word starts a step"
    );
}

#[tokio::test]
async fn a_line_break_closes_the_step_it_ends() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<Esc>");

    assert_eq!(
        undo(&rpc).await,
        v(&["foo", ""]),
        "the next line is its own step"
    );
    assert_eq!(
        undo(&rpc).await,
        v(&[""]),
        "the break went with the line it ended"
    );
}

#[tokio::test]
async fn deleting_and_typing_are_separate_steps() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<BS><BS>x<Esc>");
    assert_eq!(lines(&rpc).await, v(&["fx"]));

    assert_eq!(undo(&rpc).await, v(&["f"]), "the typing after the deletes");
    assert_eq!(undo(&rpc).await, v(&["foo"]), "both deletes, as one step");
    assert_eq!(undo(&rpc).await, v(&[""]));
}

#[tokio::test]
async fn forward_delete_is_a_delete_step_too() {
    let (rpc, _incoming) = start_with_file("abcdef\n").await;
    feed(&rpc, "i<Del><Del>xy<Esc>");
    assert_eq!(lines(&rpc).await, v(&["xycdef"]));

    assert_eq!(undo(&rpc).await, v(&["cdef"]));
    assert_eq!(undo(&rpc).await, v(&["abcdef"]));
}

#[tokio::test]
async fn a_no_op_delete_does_not_make_an_empty_step() {
    // `<BS>` at the very start of the buffer deletes nothing, so the typing after
    // it has nothing to be split from: one step, not an empty one plus the word.
    let (rpc, _incoming) = start_with_file("abc\n").await;
    feed(&rpc, "i<BS>xy<Esc>");
    assert_eq!(lines(&rpc).await, v(&["xyabc"]));

    let seq_last = exec_lua(&rpc, "return btv.undotree.get().seq_last").await;
    assert_eq!(
        seq_last.as_u64(),
        Some(1),
        "one state, no empty one before it"
    );
    assert_eq!(undo(&rpc).await, v(&["abc"]));
}

#[tokio::test]
async fn change_keeps_its_delete_with_the_first_word() {
    let (rpc, _incoming) = start_with_file("old word\n").await;
    feed(&rpc, "cwnew text<Esc>");
    assert_eq!(lines(&rpc).await, v(&["new text word"]));

    assert_eq!(undo(&rpc).await, v(&["new word"]));
    assert_eq!(
        undo(&rpc).await,
        v(&["old word"]),
        "the `cw` delete and the first word typed are one step"
    );
}

#[tokio::test]
async fn replace_mode_breaks_on_words_too() {
    let (rpc, _incoming) = start_with_file("xxxxxx\n").await;
    feed(&rpc, "Rab cd<Esc>");
    assert_eq!(lines(&rpc).await, v(&["ab cdx"]));

    assert_eq!(undo(&rpc).await, v(&["abxxxx"]));
    assert_eq!(undo(&rpc).await, v(&["xxxxxx"]));
}

#[tokio::test]
async fn each_word_is_one_node_in_the_tree() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar baz<Esc>");

    let seq_last: u64 = exec_lua(&rpc, "return btv.undotree.get().seq_last")
        .await
        .as_u64()
        .expect("seq_last is a number");
    assert_eq!(seq_last, 3, "three words, three states");
}

#[tokio::test]
async fn time_travel_walks_the_word_steps() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar<Esc>");

    feed_sync(&rpc, "g-").await;
    assert_eq!(lines(&rpc).await, v(&["foo"]));
    feed_sync(&rpc, "g+").await;
    assert_eq!(lines(&rpc).await, v(&["foo bar"]));
}

// ----- whole operations stay one step ----------------------------------------

#[tokio::test]
async fn a_paste_is_one_step_of_its_own() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo");
    feed(&rpc, &paste(" bar baz"));
    feed(&rpc, "qux<Esc>");
    assert_eq!(lines(&rpc).await, v(&["foo bar bazqux"]));

    assert_eq!(
        undo(&rpc).await,
        v(&["foo bar baz"]),
        "typing after a paste is a new step"
    );
    assert_eq!(
        undo(&rpc).await,
        v(&["foo"]),
        "the whole paste, as one step"
    );
    assert_eq!(undo(&rpc).await, v(&[""]));
}

#[tokio::test]
async fn a_multi_line_paste_is_one_step() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i");
    feed(&rpc, &paste("one two\nthree four\n"));
    feed(&rpc, "<Esc>");

    assert_eq!(undo(&rpc).await, v(&[""]));
}

#[tokio::test]
async fn dot_repeat_undoes_as_one_step() {
    let (rpc, _incoming) = start_with_file("a\nb\n").await;
    feed(&rpc, "Afoo bar<Esc>j.");
    assert_eq!(lines(&rpc).await, v(&["afoo bar", "bfoo bar"]));

    assert_eq!(
        undo(&rpc).await,
        v(&["afoo bar", "b"]),
        "`u` after `.` undoes the whole repeat"
    );
}

#[tokio::test]
async fn a_macro_undoes_as_one_step() {
    let (rpc, _incoming) = start_with_file("a\nb\n").await;
    feed(&rpc, "<F2>aAfoo bar<Esc>j<F2>");
    feed(&rpc, "<F3>a");
    assert_eq!(lines(&rpc).await, v(&["afoo bar", "bfoo bar"]));

    assert_eq!(
        undo(&rpc).await,
        v(&["afoo bar", "b"]),
        "a replayed macro's insert is one step"
    );
}

#[tokio::test]
async fn normal_command_undoes_as_one_step() {
    let (rpc, _incoming) = start(None).await;
    // `\27` is the `<Esc>` that ends the insert inside the command.
    exec_lua(&rpc, r#"btv.cmd("normal ifoo bar baz\27")"#).await;
    assert_eq!(lines(&rpc).await, v(&["foo bar baz"]));

    assert_eq!(undo(&rpc).await, v(&[""]));
}

#[tokio::test]
async fn global_normal_undoes_as_one_step() {
    let (rpc, _incoming) = start_with_file("a\nb\n").await;
    exec_lua(&rpc, r#"btv.cmd("g/./normal Afoo bar\27")"#).await;
    assert_eq!(lines(&rpc).await, v(&["afoo bar", "bfoo bar"]));

    assert_eq!(undo(&rpc).await, v(&["a", "b"]));
}

#[tokio::test]
async fn a_multi_cursor_insert_breaks_once_for_every_cursor() {
    let (rpc, _incoming) = start_with_file("abc\nabc\n").await;
    feed(&rpc, "<A-c>jc<Esc>Ix y<Esc>");
    assert_eq!(lines(&rpc).await, v(&["x yabc", "x yabc"]));

    assert_eq!(
        undo(&rpc).await,
        v(&["xabc", "xabc"]),
        "one step spans every cursor"
    );
    assert_eq!(undo(&rpc).await, v(&["abc", "abc"]));
}

#[tokio::test]
async fn ranged_normal_with_an_insert_undoes_as_one_step() {
    // The `<Esc>` that ends each line's insert must not close the batch's step:
    // the whole range run is one change.
    let (rpc, _incoming) = start_with_file("a\nb\nc\n").await;
    exec_lua(&rpc, r#"btv.cmd("%normal Afoo bar\27")"#).await;
    assert_eq!(lines(&rpc).await, v(&["afoo bar", "bfoo bar", "cfoo bar"]));

    assert_eq!(undo(&rpc).await, v(&["a", "b", "c"]));
}

// ----- pauses and cursor moves -------------------------------------------------

#[tokio::test]
async fn a_pause_in_typing_starts_a_new_step() {
    let (rpc, clock, _incoming) = start_clocked().await;
    clock.set_ms(1_000);
    feed_sync(&rpc, "ifoo").await;
    // A word is still going: a pause inside it splits it anyway.
    clock.set_ms(2_500);
    feed_sync(&rpc, "bar<Esc>").await;
    assert_eq!(lines(&rpc).await, v(&["foobar"]));

    assert_eq!(undo(&rpc).await, v(&["foo"]), "the typing after the pause");
    assert_eq!(undo(&rpc).await, v(&[""]));
}

#[tokio::test]
async fn typing_without_a_pause_does_not_split_a_word() {
    let (rpc, clock, _incoming) = start_clocked().await;
    clock.set_ms(1_000);
    feed_sync(&rpc, "ifoo").await;
    // Under the pause threshold: the word keeps going.
    clock.set_ms(1_900);
    feed_sync(&rpc, "bar<Esc>").await;

    assert_eq!(undo(&rpc).await, v(&[""]));
}

#[tokio::test]
async fn moving_the_cursor_with_an_arrow_starts_a_new_step() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoobar<Left><Left><Left>X<Esc>");
    assert_eq!(lines(&rpc).await, v(&["fooXbar"]));

    assert_eq!(
        undo(&rpc).await,
        v(&["foobar"]),
        "the edit made somewhere else"
    );
    assert_eq!(undo(&rpc).await, v(&[""]));
}

#[tokio::test]
async fn home_and_end_start_a_new_step() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<Home>X<End>Y<Esc>");
    assert_eq!(lines(&rpc).await, v(&["XfooY"]));

    assert_eq!(undo(&rpc).await, v(&["Xfoo"]));
    assert_eq!(undo(&rpc).await, v(&["foo"]));
    assert_eq!(undo(&rpc).await, v(&[""]));
}

#[tokio::test]
async fn a_cursor_moved_from_lua_starts_a_new_step() {
    // Anything that moves the cursor between two typed edits counts, not only the
    // keys this mode knows about: a plugin jump, a click.
    let (rpc, _incoming) = start_with_file("abc\n").await;
    feed_sync(&rpc, "A1").await;
    exec_lua(&rpc, "btv.win.set_cursor(0, 1, 0)").await;
    feed_sync(&rpc, "2<Esc>").await;
    assert_eq!(lines(&rpc).await, v(&["2abc1"]));

    assert_eq!(undo(&rpc).await, v(&["abc1"]));
    assert_eq!(undo(&rpc).await, v(&["abc"]));
}

#[tokio::test]
async fn a_ctrl_o_command_is_its_own_step() {
    // `<C-o>x` runs one Normal command mid-insert; it doesn't move the cursor, but
    // it is a separate change from the typing on either side of it.
    let (rpc, _incoming) = start_with_file("Z\n").await;
    feed(&rpc, "ifoo<C-o>xbar<Esc>");
    assert_eq!(lines(&rpc).await, v(&["foobar"]));

    assert_eq!(undo(&rpc).await, v(&["foo"]), "the typing after `<C-o>x`");
    assert_eq!(undo(&rpc).await, v(&["fooZ"]), "the `x`");
    assert_eq!(undo(&rpc).await, v(&["Z"]));
}

#[tokio::test]
async fn a_register_insert_is_one_step_of_its_own() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione two<Esc>yy");
    feed(&rpc, "oab<C-r>0cd<Esc>");
    assert_eq!(lines(&rpc).await, v(&["one two", "abone two", "cd"]));

    assert_eq!(
        undo(&rpc).await,
        v(&["one two", "abone two", ""]),
        "the typing after"
    );
    assert_eq!(
        undo(&rpc).await,
        v(&["one two", "ab"]),
        "the register text, whole"
    );
}

// ----- mappings ------------------------------------------------------------------
//
// One key the user pressed is one step, however much text its mapping produces:
// a string RHS, an `<expr>` RHS, a Lua RHS that feeds keys. And the typing after
// it is a step of its own, like after a paste.

#[tokio::test]
async fn a_string_mapping_is_one_step() {
    let (rpc, _incoming) = start(None).await;
    exec_lua(&rpc, "btv.keymap.set('i', '<F5>', 'foo bar')").await;
    feed(&rpc, "ix<F5>y<Esc>");
    assert_eq!(lines(&rpc).await, v(&["xfoo bary"]));

    assert_eq!(
        undo(&rpc).await,
        v(&["xfoo bar"]),
        "typing after the mapping"
    );
    assert_eq!(undo(&rpc).await, v(&["x"]), "the whole RHS, as one step");
    assert_eq!(undo(&rpc).await, v(&[""]));
}

#[tokio::test]
async fn an_expr_mapping_is_one_step() {
    let (rpc, _incoming) = start(None).await;
    exec_lua(
        &rpc,
        "btv.keymap.set('i', '<F6>', function() return 'one two' end, { expr = true })",
    )
    .await;
    feed(&rpc, "i<F6><Esc>");
    assert_eq!(lines(&rpc).await, v(&["one two"]));

    assert_eq!(undo(&rpc).await, v(&[""]));
}

#[tokio::test]
async fn keys_a_lua_mapping_feeds_are_one_step() {
    let (rpc, _incoming) = start(None).await;
    exec_lua(
        &rpc,
        "btv.keymap.set('i', '<F7>', function() btv._feedkeys('one two', false, false) end)",
    )
    .await;
    // Fed keys drain after the batch that fired the mapping, so the `<Esc>` goes
    // in a batch of its own.
    feed_sync(&rpc, "i<F7>").await;
    feed_sync(&rpc, "<Esc>").await;
    assert_eq!(lines(&rpc).await, v(&["one two"]));

    assert_eq!(undo(&rpc).await, v(&[""]));
}

#[tokio::test]
async fn keys_fed_as_typed_still_break_on_words() {
    // `typed` feeds are the keyboard (the `btv.test` runner's `t:feed`), so they
    // undo the way typing does.
    let (rpc, _incoming) = start(None).await;
    exec_lua(&rpc, "btv._feedkeys('ione two<Esc>', false, false, true)").await;
    assert_eq!(lines(&rpc).await, v(&["one two"]));

    assert_eq!(undo(&rpc).await, v(&["one"]));
}

#[tokio::test]
async fn a_mapping_that_changes_nothing_does_not_split_a_word() {
    let (rpc, _incoming) = start(None).await;
    exec_lua(&rpc, "btv.keymap.set('i', '<F8>', function() end)").await;
    feed(&rpc, "ifo<F8>o<Esc>");
    assert_eq!(lines(&rpc).await, v(&["foo"]));

    assert_eq!(undo(&rpc).await, v(&[""]));
}

#[tokio::test]
async fn an_insert_that_typed_nothing_leaves_no_undo_step() {
    // `i<Esc>` changes nothing, so it must not record a state: the next `u` takes
    // back the text typed before it, not an empty step.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>i<Esc>");
    assert_eq!(undo(&rpc).await, v(&[""]));
    let seq = exec_lua(&rpc, "return btv.undotree.get().seq_last").await;
    assert_eq!(seq.as_u64(), Some(1));
}

#[tokio::test]
async fn an_open_insert_that_typed_nothing_shows_no_pending_state() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>i");
    let seq = exec_lua(&rpc, "return btv.undotree.get().seq_last").await;
    assert_eq!(seq.as_u64(), Some(1));
}
