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
