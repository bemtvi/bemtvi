//! Compiling a query is what loading a grammar costs, so a query nothing paints
//! with is compiled the first time something asks for it.
//!
//! `dlopen`-ing a parser is ~0.1ms; one `Query::new` against a rust-sized grammar is
//! ~60ms, and it is the *grammar* that sets that price — `ts_query_new` analyzes
//! every pattern against the parse table, so halving a query's source barely moves
//! it. A load therefore costs *(query files) x (a per-grammar constant)*, and a
//! language shipping all five (`highlights`, `injections`, `indents`, `folds`,
//! `textobjects` — python, lua and friends do) used to pay five of them before its
//! first paint. Painting needs two: `highlights`, and `injections` to find the child
//! layers. The other three answer an `=`, a `foldmethod=expr` and a `vif`.
//!
//! The trade is that a broken `folds.scm` no longer fails the *load* — the grammar
//! loads and highlights fine, and the failure surfaces at the fold that asks for it.
//! That has to stay as loud as the load failure it replaced, which is the second
//! test here.
//!
//! Hermetic: the grammar compiles out of the cargo registry (no network);
//! `BEMTVI_DATA_DIR` pins the search path to the fixture dir.

mod fixture;

use bemtvi_core::BufferId;
use bemtvi_ts::loader::{Grammar, QueryOverrides};
use bemtvi_ts::Engine;
use fixture::{install_rust_grammar, write_query, TempDir};

const BUF: BufferId = BufferId(1);

/// The five query names the engine executes, in the layout a real language ships.
const ALL_QUERIES: [&str; 5] = [
    "highlights",
    "injections",
    "indents",
    "folds",
    "textobjects",
];

/// A rust file with a foldable, indentable function in it.
const CODE: &str = "fn f() -> u32 {\n    let x = 1;\n    x + 1\n}\n";

/// What one `Query::new` costs against this grammar on this machine — the unit every
/// load is some multiple of.
fn one_compile(data: &std::path::Path) -> std::time::Duration {
    // Held, not destructured: the `Language` points into the loaded library, so
    // dropping the `LoadedLanguage` out from under it segfaults the compile.
    let loaded = bemtvi_ts::loader::LoadedLanguage::load(data, "rust")
        .ok()
        .expect("dlopen the fixture grammar");
    let started = std::time::Instant::now();
    let compiled = tree_sitter::Query::new(&loaded.language, tree_sitter_rust::HIGHLIGHTS_QUERY);
    assert!(compiled.is_ok(), "the fixture query must compile");
    started.elapsed()
}

/// Loading a language that ships all five queries must not compile all five. The
/// ceiling is calibrated against a compile measured here rather than hardcoded, and
/// sits between what two compiles cost and what five do.
#[test]
fn a_grammar_load_compiles_only_the_queries_a_paint_needs() {
    let data = TempDir::new("lazy_load_cost");
    install_rust_grammar(&data.0);
    // Every optional query is the highlights source again: any valid query costs the
    // same per-grammar analysis, and this test is about how many are compiled.
    for name in ALL_QUERIES {
        write_query(&data.0, "rust", name, tree_sitter_rust::HIGHLIGHTS_QUERY);
    }
    std::env::set_var("BEMTVI_DATA_DIR", &data.0);

    let unit = one_compile(&data.0);
    let started = std::time::Instant::now();
    let loaded = Grammar::load(&data.0, "rust", &QueryOverrides::new()).ok();
    let load = started.elapsed();
    assert!(loaded.is_some(), "the fixture grammar must load");

    let ceiling = 3 * unit;
    assert!(
        load < ceiling,
        "loading took {load:?} (ceiling {ceiling:?}, one compile is {unit:?}): a load \
         compiled the queries only a keypress needs"
    );
}

/// A query the load no longer compiles is compiled on the first ask — and works.
#[test]
fn a_deferred_query_still_answers_when_something_asks() {
    let data = TempDir::new("lazy_deferred_works");
    install_rust_grammar(&data.0);
    write_query(
        &data.0,
        "rust",
        "folds",
        "(function_item body: (block) @fold)",
    );
    std::env::set_var("BEMTVI_DATA_DIR", &data.0);

    let mut engine = Engine::new(data.0.clone());
    engine.open(BUF, "rust", CODE);
    assert!(
        !engine.highlights(BUF, 0, 4).is_empty(),
        "fixture is wrong: the grammar must highlight"
    );

    let folds = engine.folds(BUF);
    assert!(
        !folds.is_empty(),
        "the deferred fold query never compiled: {folds:?}"
    );
    assert!(
        engine.take_query_errors().is_empty(),
        "a query that compiles must report nothing"
    );
}

/// A broken `folds.scm` no longer stops its grammar from loading — so it must be
/// loud at the fold that asks for it, exactly once, and the buffer must still paint.
#[test]
fn a_broken_deferred_query_reports_once_and_costs_only_its_own_feature() {
    let data = TempDir::new("lazy_broken");
    install_rust_grammar(&data.0);
    write_query(&data.0, "rust", "folds", "(function_item @fold");
    std::env::set_var("BEMTVI_DATA_DIR", &data.0);

    let mut engine = Engine::new(data.0.clone());
    engine.open(BUF, "rust", CODE);

    // The grammar loaded despite the broken query: highlighting is unaffected.
    assert!(
        !engine.highlights(BUF, 0, 4).is_empty(),
        "a broken fold query must not cost the buffer its highlights"
    );
    assert!(
        engine.take_query_errors().is_empty(),
        "nothing has asked for folds yet"
    );

    // The fold that asks gets no ranges — and says why.
    assert!(engine.folds(BUF).is_empty());
    let reported = engine.take_query_errors();
    assert_eq!(
        reported.len(),
        1,
        "a broken deferred query must be reported: {reported:?}"
    );
    assert!(
        reported[0].contains("rust folds"),
        "the report must name the query that failed: {:?}",
        reported[0]
    );

    // Asking again must not re-echo — a fold keypress happens every frame.
    assert!(engine.folds(BUF).is_empty());
    assert!(
        engine.take_query_errors().is_empty(),
        "the failure must be reported once, not on every ask"
    );
}

/// A broken query the paint *does* need still fails the whole load, as it always
/// has: the grammar is unusable, not degraded, and that is a different report.
#[test]
fn a_broken_paint_query_still_fails_the_load() {
    let data = TempDir::new("lazy_broken_paint");
    install_rust_grammar(&data.0);
    write_query(
        &data.0,
        "rust",
        "injections",
        "((string_literal) @injection",
    );
    std::env::set_var("BEMTVI_DATA_DIR", &data.0);

    let mut engine = Engine::new(data.0.clone());
    assert!(
        matches!(
            engine.open(BUF, "rust", CODE),
            bemtvi_core::syntax::OpenOutcome::LoadFailed(_)
        ),
        "a broken injections query must still fail the load"
    );
}
