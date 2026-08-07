//! `; inherits:` — a query file that builds on another language's same-named query.
//!
//! nvim-treesitter splits shared grammars: `javascript/folds.scm` is *literally* one
//! line, `; inherits: ecma,jsx`, and every real pattern lives in `ecma/folds.scm`.
//! The engine reads one file per language, so without following that modeline it
//! compiles an empty query — which is why tree-sitter folding was dead on every
//! `.js` buffer, and why highlighting a javascript snippet through a bare `Engine`
//! paints almost nothing.
//!
//! Resolution belongs here, in the layer that reads the file: the modeline is part
//! of the query-file format, the inherited files sit in the same root (the installer
//! fetches them for exactly this reason), and doing it here means every query kind —
//! `folds` and `textobjects` included — gets it, not just the ones some caller
//! remembered to merge.
//!
//! Hermetic: the rust grammar compiles out of the cargo registry (no network), and
//! the inherit chain is synthesized as extra query dirs over that one parser.

mod fixture;

use fixture::{install_rust_grammar, write_query, TempDir};
use nxvim_core::{BufferId, SyntaxEngine};
use nxvim_ts::Engine;

/// A data dir with the rust parser installed, plus whatever query files the test
/// writes. The parser is compiled once per binary and copied, not recompiled.
fn data_dir(tag: &str) -> TempDir {
    let dir = TempDir::new(tag);
    install_rust_grammar(&dir.0);
    dir
}

const SRC: &str = "fn zzz() {\n    let x = 1;\n}\n";

/// The motivating bug: a `folds.scm` that is *only* a modeline — exactly what
/// `javascript/folds.scm` is — must still fold, by pulling the inherited language's
/// patterns. Folds are the sharpest case because nothing else merged them: the
/// server's runtimepath bridge only resolved highlights / indents / injections /
/// textobjects, so a js buffer had no fold query at all.
#[test]
fn a_folds_query_that_only_inherits_still_folds() {
    let data = data_dir("inherit_folds");
    write_query(&data.0, "rust", "folds", "; inherits: rustbase\n");
    write_query(&data.0, "rustbase", "folds", "(function_item) @fold\n");
    let mut engine = Engine::new(data.0.clone());

    let buf = BufferId(1);
    engine.open(buf, "rust", SRC);
    let folds = engine.folds(buf);

    assert!(
        !folds.is_empty(),
        "a modeline-only folds.scm must inherit its patterns; got {folds:?}"
    );
    assert_eq!(
        (folds[0].start, folds[0].end),
        (0, 2),
        "the inherited `(function_item) @fold` must cover the whole function; got {folds:?}"
    );
}

/// The same for the paint: an inheriting `highlights.scm` gets the inherited
/// patterns *and* keeps its own, with its own last so a capture it redefines wins.
#[test]
fn an_inheriting_highlights_query_merges_both_sides() {
    let data = data_dir("inherit_highlights");
    write_query(
        &data.0,
        "rust",
        "highlights",
        "; inherits: rustbase\n(integer_literal) @number\n",
    );
    write_query(&data.0, "rustbase", "highlights", "\"fn\" @keyword\n");
    let mut engine = Engine::new(data.0.clone());

    let spans = engine.highlight_text("rust", SRC, 0, 3);
    let groups: Vec<&str> = spans.iter().map(|s| s.group.as_str()).collect();

    assert!(
        groups.contains(&"keyword"),
        "the inherited pattern must paint; got {spans:?}"
    );
    assert!(
        groups.contains(&"number"),
        "the file's own pattern must still paint; got {spans:?}"
    );
}

/// Inheritance is transitive and declared order is merge order, so a chain
/// `rust -> mid -> base` pulls both ancestors.
#[test]
fn inheritance_is_transitive() {
    let data = data_dir("inherit_chain");
    write_query(&data.0, "rust", "highlights", "; inherits: mid\n");
    write_query(
        &data.0,
        "mid",
        "highlights",
        "; inherits: base\n(integer_literal) @number\n",
    );
    write_query(&data.0, "base", "highlights", "\"fn\" @keyword\n");
    let mut engine = Engine::new(data.0.clone());

    let groups: Vec<String> = engine
        .highlight_text("rust", SRC, 0, 3)
        .into_iter()
        .map(|s| s.group)
        .collect();

    assert!(
        groups.iter().any(|g| g == "keyword") && groups.iter().any(|g| g == "number"),
        "both ancestors must contribute; got {groups:?}"
    );
}

/// A cycle in the inherit graph must terminate rather than recurse forever, and
/// still paint what it found.
#[test]
fn a_cycle_terminates() {
    let data = data_dir("inherit_cycle");
    write_query(
        &data.0,
        "rust",
        "highlights",
        "; inherits: other\n\"fn\" @keyword\n",
    );
    write_query(&data.0, "other", "highlights", "; inherits: rust\n");
    let mut engine = Engine::new(data.0.clone());

    let groups: Vec<String> = engine
        .highlight_text("rust", SRC, 0, 3)
        .into_iter()
        .map(|s| s.group)
        .collect();
    assert!(
        groups.iter().any(|g| g == "keyword"),
        "a cyclic chain still paints its own patterns; got {groups:?}"
    );
}

/// The server composes runtimepath overlays on top of the engine's base and needs
/// the chain to pull `queries/<inherited>/…` files the engine can't see. So the
/// engine reports the resolved chain, and its merged base keeps the modeline at the
/// top where a reader still finds it.
#[test]
fn the_resolved_chain_is_reported_for_the_runtimepath_bridge() {
    let data = data_dir("inherit_chain_report");
    write_query(&data.0, "rust", "highlights", "; inherits: mid\n");
    write_query(&data.0, "mid", "highlights", "; inherits: base\n");
    write_query(&data.0, "base", "highlights", "\"fn\" @keyword\n");
    let engine = Engine::new(data.0.clone());

    assert_eq!(
        engine.query_inherits("rust", "highlights"),
        vec!["base".to_string(), "mid".to_string()],
        "the transitive chain, deepest ancestor first"
    );
    let base = engine
        .base_query("rust", "highlights")
        .expect("read the base")
        .expect("rust has a highlights query");
    assert!(
        base.starts_with("; inherits: mid"),
        "the modeline must survive at the top of the merged base; got {base:?}"
    );
    assert!(
        base.contains("@keyword"),
        "…and the merged base must carry the inherited patterns; got {base:?}"
    );
    assert!(
        engine.query_inherits("base", "highlights").is_empty(),
        "a language that inherits nothing reports an empty chain"
    );
}
