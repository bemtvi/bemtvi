//! The grammar cache's lifecycle: retiring a replaced grammar, freeing a retired one
//! once nothing points into it, and not re-running a load whose verdict is already in.
//!
//! A loaded grammar's dlopen'd library is referenced by every `Parser` and `Tree` built
//! from its `Language` — including a parser left mid-parse, whose external scanner is
//! freed *through* the library when it drops. So a grammar the cache evicts cannot
//! simply be dropped: it is retired, and stays mapped until the last buffer that names
//! its language is gone. `drop_order.rs` / `reload_grammar.rs` are the `#[ignore]`d
//! SIGSEGV guards for that (they need a real external-scanner grammar); this file is
//! the hermetic half — the bookkeeping around them.
//!
//! Hermetic: the grammars compile out of the cargo registry (no network);
//! `BEMTVI_DATA_DIR` pins the search path to the fixture dir.

mod fixture;

use bemtvi_core::{BufferId, IndentParams, SyntaxEngine};
use bemtvi_ts::Engine;
use fixture::{install_rust_grammar, write_query, TempDir};

/// The indent knobs a typical 4-space buffer carries.
const INDENT: IndentParams = IndentParams {
    shiftwidth: 4,
    tabstop: 4,
};

const BUF: BufferId = BufferId(1);
const CODE: &str = "fn f() -> u32 {\n    let x = 1;\n    x + 1\n}\n";

fn serve_loads(engine: &mut Engine) -> usize {
    let requests = engine.take_grammar_requests();
    let served = requests.len();
    for request in requests {
        let loaded = bemtvi_ts::load_requested(request.payload);
        engine.install_grammar(&request.language, loaded);
    }
    served
}

fn deferred(data: &std::path::Path) -> Engine {
    let mut engine = Engine::new(data.to_path_buf());
    engine.defer_loads(true);
    engine
}

/// A forced synchronous load (an indent ask, which cannot wait) can land a real
/// grammar while the deferred worker is still running for the same language. The
/// worker's result then REPLACES it.
///
/// What this asserts is the BOOKKEEPING around that replacement: the buffer still
/// paints afterwards, so the swap left the cache and the buffer's parse state
/// consistent. It does NOT assert the memory safety half — dropping the replaced
/// grammar instead of retiring it would unmap its library out from under the buffer,
/// and reproducing that needs a grammar with a real external scanner (see
/// `reload_grammar.rs`, which is `#[ignore]`d for exactly that reason).
#[test]
fn a_forced_load_racing_the_worker_retires_the_grammar_it_replaces() {
    let data = TempDir::new("ts_race_retire");
    install_rust_grammar(&data.0);
    std::env::set_var("BEMTVI_DATA_DIR", &data.0);

    let mut engine = deferred(&data.0);
    engine.open(BUF, "rust", CODE); // queues a deferred load
                                    // The forced path: an indent ask cannot wait for the worker, so it loads inline
                                    // and fills the slot the worker is still working on.
    let _ = engine.indent(BUF, 2, &INDENT);
    // …and now the worker's result lands on top of it.
    serve_loads(&mut engine);

    engine.open(BUF, "rust", CODE);
    let spans = engine.highlights(BUF, 0, 4);
    assert!(
        spans.iter().any(|s| s.group.starts_with("keyword")),
        "the buffer must still paint after the replacement: {spans:?}"
    );
}

/// Retiring and then collecting a grammar leaves the engine working: reloading the
/// language mid-session and then closing its last buffer must not strand the cache.
///
/// Note what is NOT claimed: that the library was actually freed. Nothing observable
/// says so — the collection is a memory-footprint property, and asserting it would
/// mean reaching into the engine. This guards the path around it.
#[test]
fn closing_the_last_buffer_of_a_language_lets_its_retired_grammar_go() {
    let data = TempDir::new("ts_retire_free");
    install_rust_grammar(&data.0);
    std::env::set_var("BEMTVI_DATA_DIR", &data.0);

    let mut engine = Engine::new(data.0.clone());
    engine.open(BUF, "rust", CODE);
    let _ = engine.highlights(BUF, 0, 4);
    // Retire the loaded grammar (the `:TSInstall` path) while the buffer still holds
    // it, then close the buffer: the retired copy is now unreachable.
    engine.reload_grammar("rust");
    engine.close(BUF);

    // The engine keeps working afterwards — a fresh buffer loads a fresh grammar.
    engine.open(BufferId(2), "rust", CODE);
    let spans = engine.highlights(BufferId(2), 0, 4);
    assert!(
        spans.iter().any(|s| s.group.starts_with("keyword")),
        "a reopened buffer must paint from the reloaded grammar: {spans:?}"
    );
}

/// A grammar whose load FAILS still costs a full load to find that out — the
/// expensive queries compile before the broken one is reached. So the failure verdict
/// has to be cached: the forced path (an indent ask, which cannot wait) must not
/// re-run the whole load on every keystroke that forces.
///
/// A *missing* grammar is not the case that matters here — deciding "not installed"
/// is a stat, and re-deciding it is cheap. A broken one is where re-attempting hurts.
#[test]
fn a_failed_grammars_verdict_is_not_re_attempted_on_every_forced_ask() {
    let data = TempDir::new("ts_verdict_cached");
    install_rust_grammar(&data.0);
    // `highlights` is real (and slow to compile); `injections` is not a query at all,
    // so the load pays the expensive compile and *then* fails.
    write_query(&data.0, "rust", "injections", "(this is not a query\n");
    std::env::set_var("BEMTVI_DATA_DIR", &data.0);

    let mut engine = deferred(&data.0);
    engine.open(BUF, "rust", CODE);
    // `load_language_now` is the forced door — what the editor calls in front of an
    // indent or a text-object ask, which cannot wait a frame for the worker.
    let first = std::time::Instant::now();
    assert!(
        !engine.load_language_now("rust"),
        "a grammar whose queries do not compile never becomes available"
    );
    let one = first.elapsed();

    let many = std::time::Instant::now();
    for _ in 0..40 {
        let _ = engine.load_language_now("rust");
    }
    let rest = many.elapsed();

    let budget = one.max(std::time::Duration::from_millis(2)) * 10;
    assert!(
        rest < budget,
        "40 forced asks after a failed load cost {rest:?} against {one:?} for the \
         first (budget {budget:?}) — the terminal verdict is being re-attempted, \
         recompiling every query each time"
    );
}

/// The override snapshot the worker compiled against is diffed against the LIVE map
/// when the grammar lands, in both directions. The `Some` direction is covered in
/// `deferred_load.rs`; this is the direction that was missing — an override CLEARED
/// while the load was in flight must fall back to the on-disk query, not stay applied.
#[test]
fn an_override_cleared_while_loading_falls_back_to_the_on_disk_query() {
    let data = TempDir::new("ts_override_cleared");
    install_rust_grammar(&data.0);
    // On disk: fold the function.
    write_query(&data.0, "rust", "folds", "(function_item) @fold\n");
    std::env::set_var("BEMTVI_DATA_DIR", &data.0);

    let mut engine = deferred(&data.0);
    // An override that folds nothing is installed, then withdrawn — both while the
    // grammar is still loading, so the worker's snapshot disagrees with the live map.
    engine
        .set_query("rust", "folds", Some("(line_comment) @fold\n".into()))
        .expect("install the override");
    engine.open(BUF, "rust", CODE);
    engine
        .set_query("rust", "folds", None)
        .expect("withdraw the override");
    serve_loads(&mut engine);
    engine.open(BUF, "rust", CODE);
    let _ = engine.highlights(BUF, 0, 4);

    let folds = engine.folds(BUF);
    assert_eq!(
        folds.len(),
        1,
        "the on-disk query must be back in force once the override is withdrawn: \
         {folds:?}"
    );
    assert_eq!(folds[0].start, 0, "the function, not a comment: {folds:?}");
}

/// An override that does not compile has no channel to report through while the
/// grammar is still `Loading` — the `set_query` that stored it recompiles nothing.
/// The install must therefore surface the error, or a broken override silently leaves
/// the on-disk query in place and the user never learns why.
#[test]
fn a_broken_override_installed_while_loading_reports_when_the_grammar_lands() {
    let data = TempDir::new("ts_override_broken");
    install_rust_grammar(&data.0);
    std::env::set_var("BEMTVI_DATA_DIR", &data.0);

    let mut engine = deferred(&data.0);
    engine.open(BUF, "rust", CODE);
    // Stored while `Loading`: nothing can compile it yet, so nothing can report it.
    engine
        .set_query("rust", "folds", Some("(this is not a query".into()))
        .expect("storing an override is not where it is compiled");
    assert!(
        engine.take_query_errors().is_empty(),
        "nothing can be reported before the grammar exists"
    );

    serve_loads(&mut engine);
    let errors = engine.take_query_errors();
    assert!(
        !errors.is_empty(),
        "the install must surface the broken override rather than silently keeping \
         the on-disk query"
    );
    assert!(
        errors.iter().any(|e| e.contains("folds")),
        "the report must name the query that failed: {errors:?}"
    );
}
