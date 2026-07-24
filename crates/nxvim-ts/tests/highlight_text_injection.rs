//! `Engine::highlight_text` — the **stateless off-buffer highlighter** behind the
//! picker preview pane — must build injected child layers, exactly like the open
//! buffer path (`highlights`). Before this it parsed only the host grammar, so a
//! fenced code block in a previewed file showed the host's flat block colour with
//! no per-language tokens; the help preview's `>lua` blocks were the motivating
//! case (a `.txt` help doc is never an open buffer).
//!
//! Host: markdown. Injected: rust, via markdown's bundled `injections.scm` (the
//! fenced-code info string). The markdown highlights query never emits a `keyword`
//! capture, so a `keyword` span on the code line is proof the injected rust layer
//! parsed and painted through the stateless path.
//!
//! Hermetic: both grammars compile out of the cargo registry (no network);
//! `NXVIM_DATA_DIR` pins the search path to the fixture dir.

mod fixture;

use fixture::{install_markdown_grammar, install_rust_grammar, TempDir};
use nxvim_ts::Engine;

#[test]
fn highlight_text_paints_injected_language_in_a_fenced_block() {
    let data = TempDir::new("highlight_text_inj");
    install_rust_grammar(&data.0);
    install_markdown_grammar(&data.0);
    std::env::set_var("NXVIM_DATA_DIR", &data.0);

    let mut engine = Engine::new(data.0.clone());

    // A fenced rust block: markdown's injections.scm reads `rust` off the info
    // string and injects it over the fence content (`fn f() {}` on line 1).
    let text = "```rust\nfn f() {}\n```\n";
    let spans = engine.highlight_text("markdown", text, 0, 3);

    // Sanity: the host markdown layer paints *something* (the fence delimiters /
    // block), so an empty `keyword` below is the injection being skipped, not
    // highlighting being off entirely.
    assert!(
        !spans.is_empty(),
        "the markdown host should paint the fenced block; got no spans"
    );
    // `keyword` (rust's `\"fn\" @keyword`) can only come from the injected rust
    // layer — the markdown highlights query has no such capture. Its presence on
    // the code line proves the stateless highlighter built the child layer.
    assert!(
        spans.iter().any(|s| s.group == "keyword" && s.line == 1),
        "highlight_text must inject rust and paint `fn` as keyword on line 1; got {spans:?}"
    );
}

/// The stateless twin of the open-buffer `line_background` regression: a fenced
/// code block's `@markup.raw.block` background is a *line* layer, so
/// `highlight_text_bg` must report every block line. Without it the injected tokens
/// (foreground-only) overwrite the block background in the per-cell spans and the
/// preview tint survives only on the whitespace between tokens — the reported bug.
#[test]
fn highlight_text_bg_reports_fenced_block_background_lines() {
    let data = TempDir::new("highlight_text_bg");
    install_rust_grammar(&data.0);
    install_markdown_grammar(&data.0);
    std::env::set_var("NXVIM_DATA_DIR", &data.0);

    let mut engine = Engine::new(data.0.clone());
    // Pin the block capture to `@markup.raw.block` (the group the engine treats as a
    // line background), as the real nvim-treesitter install does.
    engine
        .set_query(
            "markdown",
            "highlights",
            Some("((fenced_code_block) @markup.raw.block)".to_string()),
        )
        .expect("install the fenced-block background highlights query");

    // Line 1 (`0..9`) is fully covered by rust tokens with no gap — the case that lost
    // its tint entirely in the span-only rendering.
    let text = "```rust\n0..9\nlet x = 1\n```\n";
    let (spans, bg) = engine.highlight_text_bg("markdown", text, 0, 4);
    let bg: std::collections::HashSet<usize> = bg.into_iter().collect();

    // The injected rust paints foreground-only tokens over the content cells — proof
    // those cells would lose the block background without a separate line layer.
    assert!(
        spans
            .iter()
            .any(|s| (s.line == 1 || s.line == 2) && s.group != "markup.raw.block"),
        "the rust injection must paint tokens over the block content; got {spans:?}"
    );
    // Every line of the fenced block is reported as a background line.
    for line in 0..=3 {
        assert!(
            bg.contains(&line),
            "line {line} of the fenced block must be a background line; got {bg:?}"
        );
    }
}
