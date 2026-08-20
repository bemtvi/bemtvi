//! Grammar-free indentation (`autoindent` / `smartindent`) and the auto-pairs
//! editing surface (`autopairs`) — all opt-in buffer options, off by default.
//!
//! These run on a plain (no-grammar) buffer, so treesitter never has a verdict
//! and the fallback chain under test is exactly what fires. `:set expandtab`
//! keeps the inserted indents as spaces so the assertions read literally.

use crate::support::*;

// ===== autoindent ============================================================

#[tokio::test]
async fn autoindent_copies_previous_indent() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab autoindent<CR>");
    feed(&rpc, "i    indented<CR>next<Esc>");
    assert_eq!(lines(&rpc).await, vec!["    indented", "    next"]);
}

#[tokio::test]
async fn autoindent_off_by_default_stays_at_column_zero() {
    // The neovim default: no autoindent, so a new line starts at column 0 even
    // below an indented one.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab<CR>");
    feed(&rpc, "i    indented<CR>next<Esc>");
    assert_eq!(lines(&rpc).await, vec!["    indented", "next"]);
}

// ===== smartindent ===========================================================

#[tokio::test]
async fn smartindent_indents_after_open_brace() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab smartindent<CR>");
    feed(&rpc, "iif x {<CR>body<Esc>");
    assert_eq!(lines(&rpc).await, vec!["if x {", "    body"]);
}

#[tokio::test]
async fn smartindent_indents_after_open_paren_and_bracket() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab smartindent<CR>");
    feed(&rpc, "ifoo(<CR>arg<Esc>");
    assert_eq!(lines(&rpc).await, vec!["foo(", "    arg"]);

    feed(&rpc, "obar[<CR>elem<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["foo(", "    arg", "    bar[", "        elem"],
    );
}

#[tokio::test]
async fn smartindent_dedents_a_typed_closing_brace() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab smartindent<CR>");
    // The `}` is typed on its own (still-indented) line and snaps back to the
    // opener's column.
    feed(&rpc, "iif x {<CR>body<CR>}<Esc>");
    assert_eq!(lines(&rpc).await, vec!["if x {", "    body", "}"]);
}

// ===== autopairs: insertion ==================================================

#[tokio::test]
async fn autopairs_closes_brackets_and_parks_between() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set autopairs<CR>");
    feed(&rpc, "ifoo(bar<Esc>");
    // The `)` was auto-inserted and `bar` landed before it.
    assert_eq!(lines(&rpc).await, vec!["foo(bar)"]);
}

#[tokio::test]
async fn autopairs_closes_quotes() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set autopairs<CR>");
    feed(&rpc, "i\"<Esc>");
    assert_eq!(lines(&rpc).await, vec!["\"\""]);
}

#[tokio::test]
async fn autopairs_skips_over_an_existing_closer() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set autopairs<CR>");
    // Typing the closer of the auto-inserted pair steps past it, not doubles it.
    feed(&rpc, "i()<Esc>");
    assert_eq!(lines(&rpc).await, vec!["()"]);
}

#[tokio::test]
async fn autopairs_does_not_pair_an_opener_before_a_word() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set autopairs<CR>");
    feed(&rpc, "ifoo<Esc>0i(<Esc>");
    assert_eq!(lines(&rpc).await, vec!["(foo"]);
}

#[tokio::test]
async fn autopairs_does_not_pair_an_apostrophe_inside_a_word() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set autopairs<CR>");
    feed(&rpc, "idon't<Esc>");
    assert_eq!(lines(&rpc).await, vec!["don't"]);
}

// ===== autopairs: backspace & newline ========================================

#[tokio::test]
async fn autopairs_backspace_deletes_an_empty_pair() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set autopairs<CR>");
    feed(&rpc, "i(<BS><Esc>");
    assert_eq!(lines(&rpc).await, vec![""]);
}

#[tokio::test]
async fn autopairs_newline_expands_a_bracket_block() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab autopairs<CR>");
    // `{` auto-pairs to `{}`, then <CR> between them lays the closer on its own
    // line and parks the cursor one level deeper.
    feed(&rpc, "i{<CR><Esc>");
    assert_eq!(lines(&rpc).await, vec!["{", "    ", "}"]);
}

#[tokio::test]
async fn autopairs_newline_expansion_keeps_typed_body_indented() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab autopairs<CR>");
    feed(&rpc, "ifn() {<CR>body<Esc>");
    // `(` and `{` both paired; the <CR> after `{` expands, `body` lands on the
    // middle line, and the `}` sits back at column 0.
    assert_eq!(lines(&rpc).await, vec!["fn() {", "    body", "}"]);
}

// ===== dot-repeat ============================================================

#[tokio::test]
async fn autopairs_survives_dot_repeat() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set autopairs<CR>");
    feed(&rpc, "i(<Esc>");
    assert_eq!(lines(&rpc).await, vec!["()"]);
    // `.` replays the raw `i(<Esc>` keystrokes, which re-runs auto-pairs and
    // produces a second full pair (not a lone `(`).
    feed(&rpc, ".");
    assert_eq!(lines(&rpc).await, vec!["()()"]);
}

// ===== option plumbing =======================================================

#[tokio::test]
async fn smartindent_option_reads_back_through_vim_bo() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set smartindent<CR>");
    let v = exec_lua(&rpc, "return vim.bo.smartindent").await;
    assert_eq!(v.as_bool(), Some(true), "vim.bo.smartindent reflects :set");
}

#[tokio::test]
async fn autopairs_option_writes_through_vim_bo() {
    let (rpc, _incoming) = start(None).await;
    exec_lua(&rpc, "vim.bo.autopairs = true").await;
    // A write through vim.bo reaches the live editor: typing `(` now pairs.
    feed(&rpc, "i(<Esc>");
    assert_eq!(lines(&rpc).await, vec!["()"]);
}

#[tokio::test]
async fn config_vim_o_defaults_enable_smart_indent_end_to_end() {
    // A config's `vim.o` writes (the `examples/smart-indent` recipe) set the
    // buffer-local defaults, so auto-pairs + smartindent work in a fresh buffer
    // with no further `:set`.
    let init = "vim.o.expandtab = true\n\
                vim.o.tabstop = 2\n\
                vim.o.smartindent = true\n\
                vim.o.autopairs = true\n";
    let dir = temp_dir("smart_indent_example");
    let (rpc, _incoming) = start_with_config(&dir, init).await;

    // Auto-pairs closes the `{`, and `<CR>` between the pair expands a
    // two-space-indented block with the `}` laid back at column 0 (the example
    // sets expandtab + tabstop=2).
    feed(&rpc, "iif x {<CR>body<Esc>");
    assert_eq!(lines(&rpc).await, vec!["if x {", "  body", "}"]);
    // `o` carries the block's indent (smartindent copy-previous) and `(` pairs.
    feed(&rpc, "obar(<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["if x {", "  body", "  bar()", "}"],
        "smartindent carried the block indent and autopairs closed the paren",
    );
}

// ===== `=` reindent: blank lines =============================================
//
// The `=` operator mirrors neovim's `op_reindent`: a blank line is forced to
// column 0 and never handed to the indent source (which would return the
// enclosing block's indent), so `=` never leaves whitespace on an empty line.
// The `indentemptylines` opt-in restores the old fill-the-block behavior.

/// Seed the buffer with a brace block wrapping one blank line — `{` / `` / `}`
/// — with `expandtab shiftwidth=4` so the assertions read as literal spaces.
/// Typed with no indent source active, so the three lines land verbatim.
async fn seed_brace_block_with_blank(rpc: &Rpc) {
    feed(rpc, ":set expandtab shiftwidth=4<CR>");
    feed(rpc, "i{<CR><CR>}<Esc>");
    assert_eq!(lines(rpc).await, vec!["{", "", "}"], "seed");
}

#[tokio::test]
async fn reindent_leaves_blank_lines_empty_by_default() {
    let (rpc, _incoming) = start(None).await;
    seed_brace_block_with_blank(&rpc).await;
    // smartindent would indent the blank middle line to the block's depth; `=`
    // must instead leave it at column 0 (the reported bug added `    ` there).
    feed(&rpc, ":set smartindent<CR>");
    feed(&rpc, "gg=G");
    assert_eq!(lines(&rpc).await, vec!["{", "", "    }"]);
}

#[tokio::test]
async fn reindent_indents_blank_lines_when_indentemptylines_set() {
    let (rpc, _incoming) = start(None).await;
    seed_brace_block_with_blank(&rpc).await;
    // Opt in: `=` now fills the blank line to the enclosing block's indent.
    feed(&rpc, ":set smartindent indentemptylines<CR>");
    feed(&rpc, "gg=G");
    assert_eq!(lines(&rpc).await, vec!["{", "    ", "    }"]);
}

#[tokio::test]
async fn indentemptylines_option_reads_back_through_vim_bo() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set indentemptylines<CR>");
    let v = exec_lua(&rpc, "return vim.bo.indentemptylines").await;
    assert_eq!(
        v.as_bool(),
        Some(true),
        "vim.bo.indentemptylines reflects :set"
    );
    // The `iel` abbrev round-trips too.
    feed(&rpc, ":set noiel<CR>");
    let v = exec_lua(&rpc, "return vim.bo.indentemptylines").await;
    assert_eq!(v.as_bool(), Some(false), ":set noiel clears it");
}

// ===== `=` reindent: no change => no modification ============================
//
// neovim's `op_reindent` only calls `changed_lines()` when `set_indent()` actually
// rewrote a line: reindenting already-correctly-indented text leaves `'modified'`
// (and the undo history) alone. The same holds for `>`/`<` via `shift_line`.

/// Whether the current buffer reports `'modified'`.
async fn modified(rpc: &Rpc) -> bool {
    lua_bool(rpc, "return vim.bo.modified").await == Some(true)
}

#[tokio::test]
async fn reindent_without_a_change_leaves_the_buffer_unmodified() {
    // No grammar and no autoindent/smartindent, so `=` wants every line at column
    // 0 — which is where this file's lines already are. Nothing is rewritten, so
    // the buffer must stay clean (and `u` must have nothing to undo).
    let path = write_temp("reindent_clean", "txt", "alpha\nbeta\ngamma\n");
    let (rpc, _incoming) = start(Some(path)).await;
    feed(&rpc, ":set expandtab shiftwidth=4<CR>");
    feed(&rpc, "gg=G");
    assert_eq!(
        lines(&rpc).await,
        vec!["alpha", "beta", "gamma"],
        "`=` changed nothing here"
    );
    assert!(
        !modified(&rpc).await,
        "a no-op `=` must not mark the buffer modified"
    );

    // …and it pushed no undo state either: `u` has nothing to revert.
    feed(&rpc, "u");
    assert_eq!(
        lines(&rpc).await,
        vec!["alpha", "beta", "gamma"],
        "a no-op `=` must not leave an undo step behind"
    );
}

#[tokio::test]
async fn reindent_that_changes_a_line_still_marks_modified() {
    // The positive control for the test above: here `=` really does rewrite the
    // indent (to column 0), so the buffer becomes modified as usual.
    let path = write_temp("reindent_dirty", "txt", "alpha\n    beta\n");
    let (rpc, _incoming) = start(Some(path)).await;
    feed(&rpc, ":set expandtab shiftwidth=4<CR>");
    feed(&rpc, "gg=G");
    assert_eq!(
        lines(&rpc).await,
        vec!["alpha", "beta"],
        "`=` stripped the indent"
    );
    assert!(
        modified(&rpc).await,
        "a `=` that rewrites a line marks the buffer modified"
    );
}

#[tokio::test]
async fn shift_without_a_change_leaves_the_buffer_unmodified() {
    // `<<` on a line already at column 0 shifts nothing (vim's `shift_line` only
    // reports a change when `set_indent` rewrote the line).
    let path = write_temp("shift_clean", "txt", "alpha\nbeta\n");
    let (rpc, _incoming) = start(Some(path)).await;
    feed(&rpc, ":set expandtab shiftwidth=4<CR>");
    feed(&rpc, "gg<G");
    assert_eq!(
        lines(&rpc).await,
        vec!["alpha", "beta"],
        "`<` changed nothing"
    );
    assert!(
        !modified(&rpc).await,
        "a no-op `<` must not mark the buffer modified"
    );
}

// ===== insert `<CR>`: the line left behind ===================================

#[tokio::test]
async fn double_enter_leaves_no_trailing_whitespace() {
    // Pressing `<CR>` twice inside an indented block leaves the middle line
    // *truly* empty — the block stays indented, the hole doesn't carry the
    // auto-indent whitespace (vim's autoindent behavior).
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab shiftwidth=4 smartindent<CR>");
    feed(&rpc, "iif x {<CR>foo<CR><CR>bar<Esc>");
    assert_eq!(lines(&rpc).await, vec!["if x {", "    foo", "", "    bar"]);
}

#[tokio::test]
async fn double_enter_keeps_indent_when_indentemptylines_set() {
    // With the opt-in, the auto-indent is left on the blank line, matching the
    // pre-fix behavior for users who want it.
    let (rpc, _incoming) = start(None).await;
    feed(
        &rpc,
        ":set expandtab shiftwidth=4 smartindent indentemptylines<CR>",
    );
    feed(&rpc, "iif x {<CR>foo<CR><CR>bar<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["if x {", "    foo", "    ", "    bar"]
    );
}

// ===== `o` / `<CR>` then immediate `<Esc>`: the did_ai scrub =================

#[tokio::test]
async fn open_line_then_escape_leaves_no_trailing_whitespace() {
    // `o` opens an auto-indented line; pressing `<Esc>` without typing scrubs
    // the indent so the line is truly empty (vim's did_ai), not `    `.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab shiftwidth=4 smartindent<CR>");
    feed(&rpc, "iif x {<Esc>");
    feed(&rpc, "o<Esc>");
    assert_eq!(lines(&rpc).await, vec!["if x {", ""]);
}

#[tokio::test]
async fn enter_then_escape_leaves_no_trailing_whitespace() {
    // The `<CR>`-then-immediate-`<Esc>` variant: the freshly opened line is
    // scrubbed the same way.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab shiftwidth=4 smartindent<CR>");
    feed(&rpc, "iif x {<CR><Esc>");
    assert_eq!(lines(&rpc).await, vec!["if x {", ""]);
}

#[tokio::test]
async fn open_line_then_type_keeps_the_indent() {
    // Typing on the opened line clears the did_ai arm, so the indent stays —
    // only an *untouched* auto-indent is scrubbed.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab shiftwidth=4 smartindent<CR>");
    feed(&rpc, "iif x {<Esc>");
    feed(&rpc, "obody<Esc>");
    assert_eq!(lines(&rpc).await, vec!["if x {", "    body"]);
}

#[tokio::test]
async fn escape_does_not_scrub_a_preexisting_whitespace_line() {
    // Entering insert on a line that already holds whitespace (not auto-indent)
    // and leaving without typing preserves it — did_ai only fires for indent the
    // open *itself* generated, matching vim.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab shiftwidth=4<CR>");
    // Build a whitespace-only line without any auto-indent in play.
    feed(&rpc, "i    <Esc>");
    assert_eq!(lines(&rpc).await, vec!["    "], "seed a spaces-only line");
    // Re-enter insert on it and leave without typing: the spaces survive.
    feed(&rpc, "A<Esc>");
    assert_eq!(lines(&rpc).await, vec!["    "]);
}

#[tokio::test]
async fn open_line_then_escape_keeps_indent_when_indentemptylines_set() {
    let (rpc, _incoming) = start(None).await;
    feed(
        &rpc,
        ":set expandtab shiftwidth=4 smartindent indentemptylines<CR>",
    );
    feed(&rpc, "iif x {<Esc>");
    feed(&rpc, "o<Esc>");
    assert_eq!(lines(&rpc).await, vec!["if x {", "    "]);
}

// ===== soft tabs: `<BS>` deletes indentation by the unit ====================
//
// Ground truth is neovim (`ts=4 sw=4 sts=-1 expandtab`, checked against a real
// `nvim`): with a soft-tab unit in effect, `<BS>` on whitespace deletes back to
// the previous unit boundary — *whoever* put the whitespace there (auto-indent,
// the file on disk, a typed run of spaces), stopping at the first non-blank.

#[tokio::test]
async fn backspace_deletes_auto_indent_one_unit_at_a_time() {
    // The reported bug: `o` on a deeply indented line opens an auto-indented
    // line, and `<BS>` used to peel the indent off one *space* at a time because
    // no `<Tab>` had typed it.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab shiftwidth=4 autoindent<CR>");
    feed(&rpc, "i        deep<Esc>"); // 8 spaces of indent
    feed(&rpc, "o"); // auto-indent copies the 8 spaces
    feed(&rpc, "<BS>x<Esc>");
    assert_eq!(lines(&rpc).await, vec!["        deep", "    x"]);
}

#[tokio::test]
async fn backspace_snaps_existing_indent_to_the_unit_boundary() {
    // Whitespace that was already in the buffer (never typed this session) also
    // deletes a unit at a time, and a partial unit snaps to the boundary rather
    // than jumping a whole one.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab shiftwidth=4<CR>");
    feed(&rpc, "i     five<Esc>"); // 5 spaces
    feed(&rpc, "^i<BS><Esc>"); // cursor before `f`, at virtual column 5
    assert_eq!(lines(&rpc).await, vec!["    five"], "5 spaces snap to 4");
    feed(&rpc, "^i<BS><Esc>");
    assert_eq!(lines(&rpc).await, vec!["five"], "the last unit clears");
}

#[tokio::test]
async fn backspace_over_a_typed_space_run_deletes_the_whole_unit() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab shiftwidth=4<CR>");
    feed(&rpc, "i    <BS>X<Esc>");
    assert_eq!(lines(&rpc).await, vec!["X"]);
}

#[tokio::test]
async fn backspace_over_blanks_stops_at_the_first_non_blank() {
    // "a" + 3 spaces: the unit boundary is column 0, but the delete stops at the
    // `a` — it never eats a non-blank character.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab shiftwidth=4<CR>");
    feed(&rpc, "ia   <BS>X<Esc>");
    assert_eq!(lines(&rpc).await, vec!["aX"]);
}

#[tokio::test]
async fn backspace_after_a_non_blank_still_deletes_one_character() {
    // The unit delete is whitespace-only: `<BS>` after a word rubs out exactly
    // one character, and a word's own characters never snap to a boundary.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab shiftwidth=4<CR>");
    feed(&rpc, "iword<BS><Esc>");
    assert_eq!(lines(&rpc).await, vec!["wor"]);
}

#[tokio::test]
async fn backspace_over_blanks_mid_line_snaps_to_the_boundary() {
    // Not just the indent: a run of spaces between words snaps to the unit
    // boundary too (neovim with `softtabstop` in effect does the same).
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab shiftwidth=4<CR>");
    feed(&rpc, "iab    cd<Esc>");
    feed(&rpc, "0fci<BS><Esc>"); // cursor before `c`, virtual column 6
    assert_eq!(lines(&rpc).await, vec!["ab  cd"]);
}

#[tokio::test]
async fn backspace_pads_back_a_tab_that_straddles_the_boundary() {
    // `noexpandtab tabstop=8 softtabstop=4`: the real tab in the file spans two
    // soft-tab units, so deleting it overshoots the boundary — the remainder is
    // padded back out with spaces, exactly as vim's `ins_bs` does.
    let path = write_temp("softtab_bs", "txt", "\tindented\n");
    let (rpc, _incoming) = start(Some(path)).await;
    feed(&rpc, ":set noexpandtab tabstop=8 softtabstop=4<CR>");
    feed(&rpc, "^i<BS>X<Esc>"); // cursor before `i`, at virtual column 8
    assert_eq!(lines(&rpc).await, vec!["    Xindented"]);
}

/// With `'noexpandtab'` and a `'softtabstop'` narrower than `'tabstop'`, a run of
/// soft tabs that reaches a real tabstop is **re-tabbed**: the spaces collapse into
/// the tab they add up to. vim's `ins_tab` does this pass over the whole whitespace
/// run before the cursor after inserting the fill; without it the file ends up with
/// `"    \t"` where every other editor — and the next `:retab` — has `"\t"`.
#[tokio::test]
async fn soft_tabs_collapse_into_a_real_tab_at_the_tabstop() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set noexpandtab tabstop=8 softtabstop=4<CR>");
    // One `<Tab>` is half a tabstop: spaces, because no tab fits yet.
    feed(&rpc, "i<Tab><Esc>");
    assert_eq!(lines(&rpc).await, vec!["    "], "half a tabstop is spaces");
    // The second reaches column 8, so the pair becomes the single tab it spans.
    feed(&rpc, "A<Tab><Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["\t"],
        "two soft tabs are one real tab"
    );
    // A third goes back to a partial fill past it…
    feed(&rpc, "A<Tab><Esc>");
    assert_eq!(lines(&rpc).await, vec!["\t    "]);
    // …and the fourth collapses again.
    feed(&rpc, "A<Tab><Esc>");
    assert_eq!(lines(&rpc).await, vec!["\t\t"]);
}

/// The pass re-tabs the whitespace run it lands in, not just what this keypress
/// added — the spaces may have come from the file, from autoindent, or from an
/// earlier session.
#[tokio::test]
async fn the_retab_pass_covers_whitespace_it_did_not_type() {
    let path = write_temp("softtab_retab", "txt", "    existing\n");
    let (rpc, _incoming) = start(Some(path)).await;
    feed(&rpc, ":set noexpandtab tabstop=8 softtabstop=4<CR>");
    // Insert before `existing`, at virtual column 4: one `<Tab>` reaches 8.
    feed(&rpc, "^i<Tab><Esc>");
    assert_eq!(lines(&rpc).await, vec!["\texisting"]);
}

/// It stops at the first non-blank: a run of spaces after a word re-tabs on its
/// own terms, and the word is never touched.
#[tokio::test]
async fn the_retab_pass_stops_at_the_first_non_blank() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set noexpandtab tabstop=8 softtabstop=4<CR>");
    feed(&rpc, "iab<Tab><Tab><Esc>");
    // `ab` is 2 columns; two soft tabs land on 4 then 8, and the run from column
    // 2 becomes a tab (which spans 2→8) with the `ab` untouched.
    assert_eq!(lines(&rpc).await, vec!["ab\t"]);
}

/// `'softtabstop'` off is what turns the pass off, exactly as in vim: a `<Tab>`
/// with `sts=0` is a literal tab character wherever the cursor is, and the spaces
/// before it stay spaces.
#[tokio::test]
async fn without_softtabstop_a_tab_is_literal_and_nothing_is_retabbed() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set noexpandtab tabstop=8 softtabstop=0<CR>");
    feed(&rpc, "i    <Tab>X<Esc>");
    assert_eq!(lines(&rpc).await, vec!["    \tX"]);
}

/// …and `'expandtab'` likewise: the fill is spaces, and no tab is ever produced.
#[tokio::test]
async fn expandtab_never_retabs() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab tabstop=8 softtabstop=4<CR>");
    feed(&rpc, "i<Tab><Tab>X<Esc>");
    assert_eq!(lines(&rpc).await, vec!["        X"]);
}

/// The re-tab rewrites whitespace, so the `".` last-insert register — and the
/// dot-repeat that replays the session — has to end up holding the tab the run
/// collapsed into rather than the spaces it replaced.
#[tokio::test]
async fn the_retab_pass_keeps_the_last_insert_register_honest() {
    let path = write_temp("softtab_reg", "txt", "one\ntwo\n");
    let (rpc, _incoming) = start(Some(path)).await;
    feed(&rpc, ":set noexpandtab tabstop=8 softtabstop=4<CR>");
    feed(&rpc, "gg0i<Tab><Tab><Esc>");
    assert_eq!(lines(&rpc).await, vec!["\tone", "two"]);
    let reg = exec_lua(&rpc, r#"return vim.fn.getreg('.')"#).await;
    assert_eq!(reg.as_str(), Some("\t"), "the `\".` register");
    // …and `.` replays the same keys on the next line, re-tab and all.
    feed(&rpc, "j0.");
    assert_eq!(lines(&rpc).await, vec!["\tone", "\ttwo"]);
}

/// The pass rewrites the whitespace run and nothing else: text after the cursor
/// stays where it is, and the cursor lands past the fill.
#[tokio::test]
async fn a_retabbed_tab_leaves_the_rest_of_the_line_alone() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set noexpandtab tabstop=8 softtabstop=4<CR>");
    feed(&rpc, "iab    cd<Esc>");
    // Cursor before `c` (virtual column 6): one `<Tab>` reaches 8, and the run
    // from column 2 becomes a tab — with `cd` untouched right after it.
    feed(&rpc, "0fci<Tab><Esc>");
    assert_eq!(lines(&rpc).await, vec!["ab\tcd"]);
}
