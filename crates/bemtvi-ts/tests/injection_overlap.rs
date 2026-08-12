//! Regression: a **combined** injection whose pattern matches *nested* nodes
//! produces overlapping content ranges — one region-set with both the outer and
//! the inner node's range. `Parser::set_included_ranges` rejects overlapping
//! ranges, so the engine used to drop the whole child layer silently: the
//! injected language painted nothing, every frame, with no error anywhere.
//! The engine now merges overlapping ranges into their union (identical
//! coverage for the child parse and the painter's clipping), so the layer
//! builds and paints.
//!
//! The trigger here is markdown's nesting `(section)` node: a `#` section
//! contains its `##` subsection, so one combined `(section) @injection.content`
//! pattern yields two overlapping ranges. The injected language is rust — the
//! host markdown query never emits a `keyword` capture, so a `keyword` span is
//! proof the injected rust layer painted.
//!
//! Hermetic: both grammars compile out of the cargo registry (no network);
//! `BEMTVI_DATA_DIR` pins the search path to the fixture dir.

mod fixture;

use bemtvi_core::{BufferId, OpenOutcome};
use bemtvi_ts::Engine;
use fixture::{install_markdown_grammar, install_rust_grammar, TempDir};

#[test]
fn combined_injection_with_nested_matches_still_builds_the_layer() {
    let data = TempDir::new("inj_overlap");
    install_rust_grammar(&data.0);
    install_markdown_grammar(&data.0);
    std::env::set_var("BEMTVI_DATA_DIR", &data.0);

    let mut engine = Engine::new(data.0.clone());

    // One combined pattern over the nesting `(section)` node: the `#` section's
    // range contains the `##` subsection's range, so the combined region-set
    // holds two overlapping ranges.
    engine
        .set_query(
            "markdown",
            "injections",
            Some(
                "((section) @injection.content \
                   (#set! injection.language \"rust\") \
                   (#set! injection.combined) \
                   (#set! injection.include-children))"
                    .to_string(),
            ),
        )
        .expect("install the combined section injection query");

    let buf = BufferId(1);
    let text = "# a\n## b\nfn f() {}\n";
    assert!(matches!(
        engine.open(buf, "markdown", text),
        OpenOutcome::Ok
    ));

    let spans = engine.highlights(buf, 0, 3);
    // Sanity: the host markdown layer paints (headings), so an empty `keyword`
    // below is the layer being dropped, not highlighting being off entirely.
    assert!(
        !spans.is_empty(),
        "the markdown host should paint the headings"
    );
    // `keyword` (rust's `\"fn\" @keyword`) can only come from the injected rust
    // layer — the markdown highlights query has no such capture.
    assert!(
        spans.iter().any(|s| s.group == "keyword" && s.line == 2),
        "the combined rust injection (overlapping nested section ranges) must \
         merge and paint `fn` as keyword; got {spans:?}"
    );
}
