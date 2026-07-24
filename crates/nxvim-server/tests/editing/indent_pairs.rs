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
