//! Regression: a markdown fenced code block's `@markup.raw.block` background must
//! tint the **whole** block, not only the cells an injected token leaves un-painted.
//!
//! The per-cell paint is winner-takes-cell: the host markdown layer paints the
//! block's `@markup.raw.block` background, then the injected code language paints its
//! own tokens *over* the cells they cover. Because a token span carries a foreground
//! but no background, a cell it wins loses the block's background entirely — so the
//! tint used to survive only on the un-tokenized cells (indentation, inter-token
//! spaces), and a line fully covered by tokens (a `0..9` with no gaps) got no tint at
//! all. The block's background is fundamentally a *line* layer, so the engine reports
//! the lines a line-background capture touches — recorded before the overwrite — for
//! the server to paint as the `line_bg` layer under the text.
//!
//! Hermetic: markdown + rust grammars compile out of the cargo registry (no network);
//! `BEMTVI_DATA_DIR` pins the engine's search path to the fixture dir.

mod fixture;

use bemtvi_core::{BufferId, OpenOutcome, SyntaxEngine};
use bemtvi_ts::Engine;
use fixture::{install_markdown_grammar, install_rust_grammar, TempDir};

#[test]
fn fenced_code_block_reports_every_line_as_a_background_line() {
    let data = TempDir::new("line_bg");
    install_rust_grammar(&data.0);
    install_markdown_grammar(&data.0);
    std::env::set_var("BEMTVI_DATA_DIR", &data.0);

    let mut engine = Engine::new(data.0.clone());

    // The bundled 0.5.3 markdown query tags the fenced block `@text.literal`; the real
    // bemtvi install uses nvim-treesitter's `@markup.raw.block`. Pin that name so the
    // test exercises the group the engine actually treats as a line background.
    engine
        .set_query(
            "markdown",
            "highlights",
            Some("((fenced_code_block) @markup.raw.block)".to_string()),
        )
        .expect("install the fenced-block background highlights query");

    let buf = BufferId(1);
    // Line 1 (`0..9`) is fully covered by rust tokens with no gap — the case that lost
    // its tint entirely. Line 2 (`let x = 1`) has spaces, where the background used to
    // survive. Both must be reported as background lines regardless.
    let text = "```rust\n0..9\nlet x = 1\n```\n";
    assert!(matches!(
        engine.open(buf, "markdown", text),
        OpenOutcome::Ok
    ));

    let spans = engine.highlights(buf, 0, 4);
    let bg: std::collections::HashSet<usize> =
        engine.line_background_lines(buf).into_iter().collect();

    // The rust injection paints foreground-only tokens over the content cells (e.g.
    // `0`/`9` as `constant.builtin`, `let` as `keyword`). In the span-only rendering
    // those cells would lose the block's background — the reported bug. The proof
    // those cells are overwritten:
    assert!(
        spans
            .iter()
            .any(|s| (s.line == 1 || s.line == 2) && s.group != "markup.raw.block"),
        "the rust injection must paint tokens over the block content; got {spans:?}"
    );

    // The core fix: the whole fenced block — every line of the `fenced_code_block`
    // node, delimiters (0, 3) and content (1, 2) alike — is reported as a background
    // line for the `line_bg` layer, so the tint survives under those tokens. Without
    // the fix `line_background_lines` is empty and this fails.
    for line in 0..=3 {
        assert!(
            bg.contains(&line),
            "line {line} of the fenced block must be a background line; got {bg:?}"
        );
    }
}

#[test]
fn plain_prose_has_no_background_lines() {
    let data = TempDir::new("line_bg_none");
    install_markdown_grammar(&data.0);
    std::env::set_var("BEMTVI_DATA_DIR", &data.0);

    let mut engine = Engine::new(data.0.clone());
    engine
        .set_query(
            "markdown",
            "highlights",
            Some("((fenced_code_block) @markup.raw.block)".to_string()),
        )
        .expect("install the fenced-block background highlights query");

    let buf = BufferId(1);
    let text = "# heading\n\njust prose, no code\n";
    assert!(matches!(
        engine.open(buf, "markdown", text),
        OpenOutcome::Ok
    ));

    engine.highlights(buf, 0, 3);
    assert!(
        engine.line_background_lines(buf).is_empty(),
        "a document with no fenced code block reports no background lines"
    );
}
