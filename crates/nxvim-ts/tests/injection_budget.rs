//! An injected layer that misses one frame's parse budget must still end up
//! painted — the two ways it used to end up permanently unpainted instead.
//!
//! [`INJECTION_DEADLINE`] bounds *all* of a buffer's child parses on one refresh so
//! a pathological grammar can't stall the edit path. Two defects turned that bound
//! into "the injected language never highlights until you type something":
//!
//! 1. **The clock started before the child grammar was loaded.** The first region of
//!    a language pays a cold `dlopen` plus a compile of every `.scm` it ships —
//!    tens of milliseconds, all of it charged to the parse budget, so the parse that
//!    followed was cancelled before it began. That is a vue file's
//!    `<script setup lang="ts">`: the host paints, the injected typescript doesn't.
//!
//! 2. **A cancelled child parse with no previous tree was dropped, not resumed.**
//!    The host's own over-budget parse is resumed a budget at a time by
//!    [`Engine::highlights`] (the server keeps redrawing while `parse_pending`), so a
//!    large file colours in progressively. Child parses had no such path: the region
//!    was dropped, `parse_pending` said "converged", the server's highlight memo hit
//!    on every later frame, and nothing re-attempted until an edit.
//!
//! The host markdown query never emits a `keyword` capture, so a `keyword` span is
//! proof the injected rust layer built and painted.
//!
//! Hermetic: both grammars compile out of the cargo registry (no network);
//! `NXVIM_DATA_DIR` pins the search path to the fixture dir.

mod fixture;

use fixture::{install_markdown_grammar, install_rust_grammar, TempDir};
use nxvim_core::BufferId;
use nxvim_ts::Engine;

const RUST_IN_FENCES: &str = "((fenced_code_block (code_fence_content) @injection.content) \
     (#set! injection.language \"rust\"))";

const BUF: BufferId = BufferId(1);

/// A markdown document whose single fence holds `lines` of real rust.
fn doc(lines: usize) -> String {
    let mut s = String::from("# hi\n\n```rust\n");
    for i in 0..lines {
        s.push_str(&format!("fn f{i}() -> u32 {{ let x = {i}; x + 1 }}\n"));
    }
    s.push_str("```\n");
    s
}

/// An engine over the fixture grammars with rust injected into markdown fences.
fn engine(data: &std::path::Path) -> Engine {
    let mut engine = Engine::new(data.to_path_buf());
    engine
        .set_query("markdown", "injections", Some(RUST_IN_FENCES.to_string()))
        .expect("install the injection query");
    engine
}

fn paints_rust(engine: &mut Engine, last_line: usize) -> bool {
    engine
        .highlights(BUF, 0, last_line)
        .iter()
        .any(|s| s.group == "keyword")
}

/// Defect 1: the child grammar's cold load must not be charged to the parse budget.
/// A *fresh* engine (nothing cached) has to paint the injected language on the very
/// first highlight pull, exactly as a warm one does.
#[test]
fn a_cold_child_grammar_load_does_not_consume_the_injection_parse_budget() {
    let data = TempDir::new("inj_budget_cold");
    install_rust_grammar(&data.0);
    install_markdown_grammar(&data.0);
    std::env::set_var("NXVIM_DATA_DIR", &data.0);

    let text = doc(60);

    // Warm: proof the region, the query and the budget are all comfortably fine
    // once the grammar is cached — so a cold failure is about the load, not the size.
    let mut warm = engine(&data.0);
    warm.open(BufferId(9), "markdown", "```rust\nfn a() {}\n```\n");
    let _ = warm.highlights(BufferId(9), 0, 5);
    warm.open(BUF, "markdown", &text);
    assert!(
        paints_rust(&mut warm, 70),
        "fixture is wrong: a warm engine must paint this region"
    );

    // Cold: the same open on an engine that has never loaded the rust grammar.
    let mut cold = engine(&data.0);
    cold.open(BUF, "markdown", &text);
    assert!(
        paints_rust(&mut cold, 70),
        "the injected rust never painted: the cold grammar load ate the parse budget"
    );
}

/// Defect 2: a child parse too big for one budget resumes across frames, the way the
/// host's does — no edit required. Drives the loop the server drives: repaint while
/// `parse_pending`, which is what advances the parse.
#[test]
fn an_over_budget_injection_colours_in_across_frames_with_no_edit() {
    let data = TempDir::new("inj_budget_resume");
    install_rust_grammar(&data.0);
    install_markdown_grammar(&data.0);
    std::env::set_var("NXVIM_DATA_DIR", &data.0);

    // Large enough that neither the host nor the child parse fits in one budget, so
    // both take the progressive path.
    let text = doc(4000);
    let mut engine = engine(&data.0);
    engine.open(BUF, "markdown", &text);

    // The server's loop: while the engine reports work pending, repaint (which
    // resumes the parse a budget further). Bounded so a *non*-converging engine
    // fails the test instead of hanging it.
    let mut frames = 0;
    while engine.parse_pending(BUF) {
        frames += 1;
        assert!(frames < 500, "the parse never converged in {frames} frames");
        let _ = engine.highlights(BUF, 0, 80);
    }
    assert!(
        frames > 1,
        "fixture is wrong: this should not fit in one budget"
    );

    assert!(
        paints_rust(&mut engine, 80),
        "the injected rust never painted: an over-budget child parse was dropped, \
         not resumed, and nothing re-attempted it without an edit"
    );
}
