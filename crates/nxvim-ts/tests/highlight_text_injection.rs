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
