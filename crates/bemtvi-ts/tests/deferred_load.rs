//! The engine can hand its grammar loads to the host instead of running them on the
//! tick that first needs a language.
//!
//! Loading is dominated by compiling the language's queries — tens to hundreds of ms,
//! uninterruptible — so an inline load freezes the frame that wanted the language.
//! Deferred, the engine asks ([`Engine::take_grammar_requests`]), paints what it can,
//! and takes the grammar back when the host has it. Off by default: it only works for
//! a host that drives the other half, which the server does and an embedder need not.
//!
//! Hermetic: the grammars compile out of the cargo registry (no network);
//! `BEMTVI_DATA_DIR` pins the search path to the fixture dir.

mod fixture;

use bemtvi_core::BufferId;
use bemtvi_ts::Engine;
use fixture::{install_markdown_grammar, install_rust_grammar, write_query, TempDir};

const BUF: BufferId = BufferId(1);
const CODE: &str = "fn f() -> u32 {\n    let x = 1;\n    x + 1\n}\n";

/// Play the host's part: run every queued load and hand the results back. Returns how
/// many grammars it loaded.
fn serve_loads(engine: &mut Engine) -> usize {
    let requests = engine.take_grammar_requests();
    let served = requests.len();
    for request in requests {
        let loaded = bemtvi_ts::load_requested(request.payload);
        engine.install_grammar(&request.language, loaded);
    }
    served
}

/// An engine over the fixture dir that defers its loads.
fn deferred(data: &std::path::Path) -> Engine {
    let mut engine = Engine::new(data.to_path_buf());
    engine.defer_loads(true);
    engine
}

/// The handshake: a buffer opened before its grammar exists paints nothing and asks;
/// once the host answers, the same buffer paints — the editor re-opens it, which is
/// what the `syntax_opened` marker it drops on install is for.
#[test]
fn a_deferred_open_asks_for_its_grammar_and_paints_once_it_lands() {
    let data = TempDir::new("defer_handshake");
    install_rust_grammar(&data.0);
    std::env::set_var("BEMTVI_DATA_DIR", &data.0);

    let mut engine = deferred(&data.0);
    engine.open(BUF, "rust", CODE);
    assert!(
        engine.highlights(BUF, 0, 4).is_empty(),
        "nothing can be painted before the grammar is loaded"
    );

    assert_eq!(serve_loads(&mut engine), 1, "the engine must have asked");
    engine.open(BUF, "rust", CODE);
    assert!(
        !engine.highlights(BUF, 0, 4).is_empty(),
        "the buffer must paint once its grammar lands"
    );
    // Asked once, not once a frame: the request is remembered while it is in flight.
    assert_eq!(serve_loads(&mut engine), 0);
}

/// A query override that arrives *while* the grammar is loading must still take
/// effect. This is the ordinary case, not a rare race: the server resolves a
/// language's runtimepath queries around the same tick the grammar is asked for, so
/// the load usually compiles against a snapshot that predates the resolved query.
#[test]
fn an_override_installed_while_loading_is_applied_when_the_grammar_lands() {
    let data = TempDir::new("defer_override");
    install_rust_grammar(&data.0);
    // On disk: a fold query that folds nothing. The override below is what the
    // resolution bridge would install.
    write_query(&data.0, "rust", "folds", "(line_comment) @fold\n");
    std::env::set_var("BEMTVI_DATA_DIR", &data.0);

    let mut engine = deferred(&data.0);
    engine.open(BUF, "rust", CODE); // asks for rust
    engine
        .set_query("rust", "folds", Some("(function_item) @fold\n".into()))
        .expect("install the resolved query");
    serve_loads(&mut engine);
    engine.open(BUF, "rust", CODE);
    let _ = engine.highlights(BUF, 0, 4);

    let folds = engine.folds(BUF);
    assert_eq!(
        folds.len(),
        1,
        "the resolved fold query must be the one that ran, not the on-disk one \
         the load happened to snapshot: {folds:?}"
    );
    assert_eq!(folds[0].start, 0, "the function, not a comment: {folds:?}");
}

/// An injected region whose child grammar is still loading is *pending*, not dropped:
/// the engine reports work outstanding, so the host keeps repainting and the layer
/// builds on the frame after the grammar lands — no edit required.
#[test]
fn an_injected_language_still_loading_keeps_its_region_pending() {
    let data = TempDir::new("defer_injection");
    install_markdown_grammar(&data.0);
    install_rust_grammar(&data.0);
    std::env::set_var("BEMTVI_DATA_DIR", &data.0);

    let doc = "# hi\n\n```rust\nfn f() {}\n```\n";
    let mut engine = deferred(&data.0);
    engine.open(BUF, "markdown", doc);
    // The host grammar first: only once markdown parses is the rust region found.
    serve_loads(&mut engine);
    engine.open(BUF, "markdown", doc);
    let _ = engine.highlights(BUF, 0, 5);

    assert!(
        engine.parse_pending(BUF),
        "the injected region must be pending while its grammar loads, not dropped"
    );
    // More than one: the bundled markdown query also injects `markdown_inline` and
    // friends, which aren't installed here and come back as "no parser".
    assert!(
        serve_loads(&mut engine) >= 1,
        "the child grammar was asked for"
    );

    // The repaint the pending work asked for: now it paints. The markdown query never
    // emits `keyword`, so a `keyword` span is the injected rust layer.
    let painted = engine
        .highlights(BUF, 0, 5)
        .iter()
        .any(|s| s.group == "keyword");
    assert!(
        painted,
        "the injected layer must build once its grammar lands"
    );
    assert!(!engine.parse_pending(BUF), "and then converge");
}

/// The default is unchanged: an engine nobody told to defer loads inline, because
/// there is no host to defer *to*.
#[test]
fn an_engine_that_was_not_told_to_defer_loads_inline() {
    let data = TempDir::new("defer_off");
    install_rust_grammar(&data.0);
    std::env::set_var("BEMTVI_DATA_DIR", &data.0);

    let mut engine = Engine::new(data.0.clone());
    engine.open(BUF, "rust", CODE);
    assert!(
        !engine.highlights(BUF, 0, 4).is_empty(),
        "the synchronous path must still paint on the first pull"
    );
    assert!(engine.take_grammar_requests().is_empty());
}
