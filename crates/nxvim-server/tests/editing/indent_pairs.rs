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
async fn example_config_enables_smart_indent_end_to_end() {
    // Load the shipped `examples/smart-indent/init.lua` verbatim and confirm its
    // `vim.o` defaults reach the live buffer — auto-pairs + smartindent both work
    // with no further `:set`. This guards the example against bit-rot.
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/smart-indent/init.lua");
    let init = std::fs::read_to_string(&example).expect("read example init.lua");
    let dir = temp_dir("smart_indent_example");
    let (rpc, _incoming) = start_with_config(&dir, &init).await;

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
