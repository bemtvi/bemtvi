//! `'indentdetect'` — a file's own indentation deciding that buffer's `'expandtab'` and
//! `'shiftwidth'` (vim-sleuth's behavior, built in). On by default.
//!
//! Two halves, both asserted here: the **verdict** (what the detector reads off a given
//! file) and the **consequence** (what pressing `>>` / `<Tab>` in that buffer actually
//! inserts). A test that only read the options back would pass against a detector wired
//! to nothing, so every style case also types into the file and asserts the bytes.

use crate::support::*;

async fn open(content: &str) -> (Rpc, UnboundedReceiver<Incoming>, String) {
    let path = write_temp("indentdetect", "txt", content);
    let (rpc, incoming) = start(Some(path.clone())).await;
    (rpc, incoming, path)
}

/// The `(expandtab, shiftwidth)` pair a buffer ended up with, as Lua reads them.
async fn style(rpc: &Rpc) -> (bool, i64) {
    let et = exec_lua(rpc, "return btv.bo[0].expandtab").await;
    let sw = exec_lua(rpc, "return btv.bo[0].shiftwidth").await;
    (
        et.as_bool().expect("expandtab is a boolean"),
        sw.as_i64().expect("shiftwidth is a number"),
    )
}

// ---- the verdict ----------------------------------------------------------

#[tokio::test]
async fn tab_indented_file_turns_expandtab_off() {
    // Configure the *opposite* first — a spaces config — because the built-in default is
    // already `noexpandtab`, and a test that opened the file against the default would
    // pass with the detector deleted. The point is that the FILE beats the config.
    let path = write_temp(
        "idt_tabs",
        "txt",
        "fn main() {\n\tlet x = 1;\n\tif x {\n\t\tuse(x);\n\t}\n}\n",
    );
    let (rpc, mut incoming) = start(None).await;
    redraw_after(&rpc, &mut incoming, ":set expandtab shiftwidth=2<CR>").await;
    redraw_after(&rpc, &mut incoming, &format!(":e {path}<CR>")).await;
    // shiftwidth 0 is bemtvi's "follow tabstop" sentinel: one indent level in a
    // tab-indented file is exactly one tab.
    assert_eq!(style(&rpc).await, (false, 0));
}

/// The startup file arg is read at editor construction — *before* `init.lua` runs and
/// writes the current buffer's `'expandtab'` — so it needs the post-config reconcile to
/// land on the same side as every later `:e`. Without it, the one file you named on the
/// command line would be the only file the config out-ranked.
#[tokio::test]
async fn the_startup_file_beats_a_config_that_sets_the_opposite() {
    let dir = temp_dir("idt_startup_cfg");
    let file = dir.join("tabbed.txt");
    std::fs::write(&file, "a\n\tb\n\t\tc\n\td\n").expect("write tab-indented file");
    let (rpc, _incoming) = start_with_file_and_config(
        &dir,
        file.to_str().expect("utf-8 path"),
        "vim.o.expandtab = true\nvim.o.shiftwidth = 2\n",
    )
    .await;
    assert_eq!(style(&rpc).await, (false, 0));
}

/// …and the same config still stands when the startup file has nothing to say.
#[tokio::test]
async fn a_config_still_configures_a_startup_file_with_no_indentation() {
    let dir = temp_dir("idt_startup_flat");
    let file = dir.join("flat.txt");
    std::fs::write(&file, "alpha\nbeta\n").expect("write flat file");
    let (rpc, _incoming) = start_with_file_and_config(
        &dir,
        file.to_str().expect("utf-8 path"),
        "vim.o.expandtab = true\nvim.o.shiftwidth = 2\n",
    )
    .await;
    assert_eq!(style(&rpc).await, (true, 2));
}

#[tokio::test]
async fn two_space_file_sets_expandtab_and_shiftwidth_two() {
    let (rpc, _incoming, _p) = open("a\n  b\n    c\n  d\ne\n  f\n").await;
    assert_eq!(style(&rpc).await, (true, 2));
}

#[tokio::test]
async fn four_space_file_sets_shiftwidth_four() {
    let (rpc, _incoming, _p) = open("a\n    b\n        c\n    d\ne\n    f\n").await;
    assert_eq!(style(&rpc).await, (true, 4));
}

#[tokio::test]
async fn a_file_with_no_indentation_leaves_the_configured_style_alone() {
    // No evidence must mean no opinion — not a silent reset to some built-in.
    let path = write_temp("idt_flat", "txt", "alpha\nbeta\ngamma\n");
    let (rpc, mut incoming) = start(Some(path)).await;
    redraw_after(&rpc, &mut incoming, ":set expandtab shiftwidth=7<CR>").await;
    // Re-read the same file: the detector runs again and still has nothing to say.
    redraw_after(&rpc, &mut incoming, ":e!<CR>").await;
    assert_eq!(style(&rpc).await, (true, 7));
}

#[tokio::test]
async fn tab_indent_with_space_alignment_still_reads_as_tabs() {
    // The tabs-for-indent / spaces-for-alignment style: the leading run starts with a
    // tab, and the spaces after it align a continuation *inside* the line.
    let (rpc, _incoming, _p) = open("call(a,\n\t  b,\n\t  c);\n\tnext();\n\tmore();\n").await;
    assert_eq!(style(&rpc).await, (false, 0));
}

#[tokio::test]
async fn block_comment_bodies_do_not_drag_the_width_to_one() {
    // ` * ` continuation lines sit one column in. Counting them would make the file
    // look 1-space indented; the real convention here is 4.
    let (rpc, _incoming, _p) =
        open("/*\n * A doc comment.\n * Another line.\n */\nvoid f() {\n    g();\n    h();\n}\n")
            .await;
    assert_eq!(style(&rpc).await, (true, 4));
}

/// A one-column step is never adopted as `'shiftwidth'`. It is far more often a stray
/// line, a wrapped continuation or prose than a deliberate convention, and a silent
/// `shiftwidth=1` is a much worse outcome than leaving the configured width in place.
/// Such a file is still recognised as space-indented — only its *width* is declined.
/// One indented line is not a convention. A file with a single (possibly stray) indented
/// line is still read as space-indented, but its width is not adopted — `'shiftwidth'`
/// stays whatever the config chose.
#[tokio::test]
async fn a_single_indented_line_does_not_set_the_width() {
    let path = write_temp(
        "idt_lone",
        "txt",
        "fn main() {\nlet x = 1;\n        let y = 2;\n}\n",
    );
    let (rpc, mut incoming) = start(None).await;
    redraw_after(&rpc, &mut incoming, ":set shiftwidth=4<CR>").await;
    redraw_after(&rpc, &mut incoming, &format!(":e {path}<CR>")).await;
    assert_eq!(style(&rpc).await, (true, 4));
}

#[tokio::test]
async fn a_one_space_step_sets_expandtab_but_not_the_width() {
    let path = write_temp(
        "idt_one",
        "txt",
        "def f():\n return 1\n\ndef g():\n return 2\n",
    );
    let (rpc, mut incoming) = start(None).await;
    redraw_after(&rpc, &mut incoming, ":set noexpandtab shiftwidth=4<CR>").await;
    redraw_after(&rpc, &mut incoming, &format!(":e {path}<CR>")).await;
    assert_eq!(style(&rpc).await, (true, 4));
}

#[tokio::test]
async fn block_comments_do_not_outvote_a_tab_indented_file() {
    // The case the `*`-continuation skip is really for: a tab-indented C file whose
    // doc comment has more ` * ` body lines than the function has statements. Those
    // bodies are indented with a *space* (to align the star under the `/*`), so counting
    // them as space evidence would flip a tab-indented file to `expandtab`.
    let (rpc, _incoming, _p) =
        open("/*\n * one\n * two\n * three\n * four\n * five\n */\nvoid f() {\n\tg();\n}\n").await;
    assert_eq!(style(&rpc).await, (false, 0));
}

#[tokio::test]
async fn detection_is_per_buffer_not_global() {
    // Opening a tab file must not reindent the spaces file already open beside it.
    let spaces = write_temp("idt_sp", "txt", "a\n  b\n    c\n  d\n");
    let tabs = write_temp("idt_tb", "txt", "a\n\tb\n\t\tc\n\tb\n");
    let (rpc, mut incoming) = start(Some(spaces)).await;
    assert_eq!(style(&rpc).await, (true, 2));
    redraw_after(&rpc, &mut incoming, &format!(":e {tabs}<CR>")).await;
    assert_eq!(style(&rpc).await, (false, 0));
    redraw_after(&rpc, &mut incoming, ":b#<CR>").await;
    assert_eq!(
        style(&rpc).await,
        (true, 2),
        "the spaces buffer keeps its own verdict"
    );
}

#[tokio::test]
async fn noindentdetect_leaves_the_configured_style_untouched() {
    let path = write_temp("idt_off", "txt", "a\n\tb\n\t\tc\n");
    let (rpc, mut incoming) = start(None).await;
    redraw_after(
        &rpc,
        &mut incoming,
        ":set noindentdetect expandtab shiftwidth=3<CR>",
    )
    .await;
    redraw_after(&rpc, &mut incoming, &format!(":e {path}<CR>")).await;
    assert_eq!(
        style(&rpc).await,
        (true, 3),
        "with detection off the tab-indented file must not flip expandtab"
    );
}

#[tokio::test]
async fn a_later_setlocal_wins_over_the_detected_style() {
    // Detection runs at read time, before `BufReadPost` — so an autocmd, an
    // `.editorconfig`, or the user typing `:set` all still have the last word.
    let (rpc, mut incoming, _p) = open("a\n\tb\n\t\tc\n").await;
    assert_eq!(style(&rpc).await, (false, 0));
    redraw_after(&rpc, &mut incoming, ":set expandtab shiftwidth=2<CR>").await;
    assert_eq!(style(&rpc).await, (true, 2));
}

#[tokio::test]
async fn a_bufreadpost_autocmd_sees_and_can_override_the_detected_style() {
    let tabs = write_temp("idt_acmd", "txt", "a\n\tb\n\t\tc\n\td\n");
    let (rpc, mut incoming) = start(None).await;
    exec_lua(
        &rpc,
        "btv.g.seen_et = nil
         btv.on('BufReadPost', function()
           btv.g.seen_et = btv.bo[0].expandtab
           btv.cmd('setlocal expandtab shiftwidth=8')
         end)",
    )
    .await;
    redraw_after(&rpc, &mut incoming, &format!(":e {tabs}<CR>")).await;
    assert!(
        poll_true(&rpc, "return btv.g.seen_et ~= nil").await,
        "the deferred open never fired BufReadPost"
    );
    let seen = exec_lua(&rpc, "return btv.g.seen_et").await;
    assert_eq!(
        seen.as_bool(),
        Some(false),
        "BufReadPost must run AFTER detection, seeing the file's own verdict"
    );
    assert_eq!(
        style(&rpc).await,
        (true, 8),
        "and an autocmd that sets the style overrides it"
    );
}

// ---- the consequence: what typing actually inserts -------------------------

#[tokio::test]
async fn a_two_space_file_indents_by_two_spaces() {
    let (rpc, mut incoming, _path) = open("a\n  b\n    c\n  d\n").await;
    // `>>` on the first line inserts one indent step.
    redraw_after(&rpc, &mut incoming, "gg>>").await;
    assert_eq!(lines(&rpc).await[0], "  a");
    // …and so does a <Tab> in insert mode.
    redraw_after(&rpc, &mut incoming, "Go<Tab>x<Esc>").await;
    assert_eq!(lines(&rpc).await.last().map(String::as_str), Some("  x"));
}

#[tokio::test]
async fn a_tab_indented_file_indents_with_a_real_tab() {
    let (rpc, mut incoming, _path) = open("a\n\tb\n\t\tc\n\td\n").await;
    redraw_after(&rpc, &mut incoming, "gg>>").await;
    assert_eq!(
        lines(&rpc).await[0],
        "\ta",
        "a tab-indented file grows tabs, whatever the config's default was"
    );
}

#[tokio::test]
async fn the_detected_style_survives_a_write_and_reload() {
    let (rpc, mut incoming, path) = open("a\n  b\n    c\n  d\n").await;
    redraw_after(&rpc, &mut incoming, "gg>>:w<CR>").await;
    assert_eq!(
        std::fs::read_to_string(&path).expect("re-read"),
        "  a\n  b\n    c\n  d\n",
        "the write put spaces on disk, not a tab"
    );
    redraw_after(&rpc, &mut incoming, ":e!<CR>").await;
    assert_eq!(style(&rpc).await, (true, 2));
}

// ---- the option itself -----------------------------------------------------

#[tokio::test]
async fn indentdetect_defaults_on_and_round_trips_through_set_and_lua() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, ":set indentdetect?<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("indentdetect"),
        "'indentdetect' is on by default"
    );
    let v = exec_lua(&rpc, "return btv.o.indentdetect").await;
    assert_eq!(v.as_bool(), Some(true));

    // The abbreviation, the `no` form, and the Lua write all reach the same value.
    redraw_after(&rpc, &mut incoming, ":set noidt<CR>").await;
    let v = exec_lua(&rpc, "return btv.o.indentdetect").await;
    assert_eq!(v.as_bool(), Some(false));
    exec_lua(&rpc, "vim.o.indentdetect = true").await;
    let map = redraw_after(&rpc, &mut incoming, ":set idt?<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("indentdetect")
    );
}
