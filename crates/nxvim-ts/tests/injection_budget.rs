//! An injected layer that misses one frame's parse budget must still end up
//! painted — without the frame it missed having stalled to paint it.
//!
//! [`INJECTION_DEADLINE`] bounds *all* of a buffer's child work on one refresh —
//! grammar loads included — so a cold `dlopen` plus a `.scm` compile (hundreds of ms
//! for typescript) can't stall the frame any more than a pathological grammar can.
//! The file paints first and its injections colour in over the frames after it, the
//! way the host's own over-budget parse already did. What must never happen is the
//! third outcome: the region is *dropped*, so nothing repaints and the injected
//! language stays flat until the user types.
//!
//! That was the bug. A cancelled child parse with no previous tree was discarded,
//! `parse_pending` reported "converged", the server's highlight memo hit on every
//! later frame, and only an edit rebuilt the layers — a vue file's
//! `<script setup lang="ts">` sat uncoloured until the first keystroke.
//!
//! The host markdown query never emits a `keyword` capture, so a `keyword` span is
//! proof the injected rust layer built and painted.
//!
//! Hermetic: both grammars compile out of the cargo registry (no network);
//! `NXVIM_DATA_DIR` pins the search path to the fixture dir.

mod fixture;

use fixture::{install_markdown_grammar, install_rust_grammar, install_rust_grammar_as, TempDir};
use nxvim_core::BufferId;
use nxvim_ts::Engine;

const RUST_IN_FENCES: &str = "((fenced_code_block (code_fence_content) @injection.content) \
     (#set! injection.language \"rust\"))";

const BUF: BufferId = BufferId(1);

/// Stand-in languages for the multi-language fence test, each backed by its own copy
/// of the rust grammar (see [`install_rust_grammar_as`]) so each costs a real load.
const LANGS: [&str; 5] = ["langa", "langb", "langc", "langd", "lange"];

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

/// Drive the server's loop — repaint while the engine reports work pending — and
/// return how many frames it took to converge, or `None` if it never did.
fn frames_to_converge(engine: &mut Engine, last_line: usize) -> Option<usize> {
    let mut frames = 0;
    while engine.parse_pending(BUF) {
        frames += 1;
        if frames >= 500 {
            return None;
        }
        let _ = engine.highlights(BUF, 0, last_line);
    }
    Some(frames)
}

/// A cold child grammar must not be *dropped* when its load leaves no budget for the
/// parse that follows: the region stays pending, so the server keeps repainting, and
/// it colours in with no edit. A *warm* engine paints the same region on the first
/// pull — proof this is about the load and not the region's size.
#[test]
fn a_cold_child_grammar_paints_without_an_edit_once_its_load_is_paid_for() {
    let data = TempDir::new("inj_budget_cold");
    install_rust_grammar(&data.0);
    install_markdown_grammar(&data.0);
    std::env::set_var("NXVIM_DATA_DIR", &data.0);

    let text = doc(60);

    // Warm: proof the region, the query and the budget are all comfortably fine
    // once the grammar is cached — one pull, painted.
    let mut warm = engine(&data.0);
    warm.open(BufferId(9), "markdown", "```rust\nfn a() {}\n```\n");
    let _ = warm.highlights(BufferId(9), 0, 5);
    warm.open(BUF, "markdown", &text);
    assert!(
        paints_rust(&mut warm, 70),
        "fixture is wrong: a warm engine must paint this region on the first pull"
    );

    // Cold: the same open on an engine that has never loaded the rust grammar. The
    // load may well eat the whole budget, so this is allowed to take a few frames —
    // it is not allowed to converge with the region unpainted.
    let mut cold = engine(&data.0);
    cold.open(BUF, "markdown", &text);
    let _ = cold.highlights(BUF, 0, 70);
    let frames = frames_to_converge(&mut cold, 70).expect("the parse never converged");
    assert!(
        paints_rust(&mut cold, 70),
        "the injected rust never painted after {frames} frames: a cold child parse \
         was dropped rather than kept pending, and nothing re-attempted it"
    );
}

/// A frame that has already spent its budget must not then pay a cold grammar load.
/// A document injecting several languages used to pay every one of their loads on
/// the single frame that discovered them — hundreds of ms of dead editor, none of it
/// interruptible. Now the first load a frame can afford is the last it takes, and the
/// rest arrive on the frames after.
///
/// The fixture's cold load is ~60ms — comfortably over the budget it must not be
/// charged to, and ~1000x the warm path, so "did this frame load a grammar" is
/// legible in the wall clock. The threshold is calibrated against a load measured on
/// *this* machine rather than hardcoded.
#[test]
fn a_cold_grammar_load_is_deferred_when_the_frame_is_already_over_budget() {
    let data = TempDir::new("inj_budget_defer");
    install_markdown_grammar(&data.0);
    install_rust_grammar(&data.0);
    for lang in LANGS {
        install_rust_grammar_as(&data.0, lang);
    }
    std::env::set_var("NXVIM_DATA_DIR", &data.0);

    // What one cold load costs here: a fresh engine opening a one-line buffer does a
    // `dlopen` plus a query compile and essentially no parsing.
    let load = {
        let mut probe = Engine::new(data.0.clone());
        let started = std::time::Instant::now();
        probe.open(BufferId(7), "rust", "fn a() {}\n");
        let _ = probe.highlights(BufferId(7), 0, 1);
        started.elapsed()
    };

    // One fence per language, each big enough to matter, under the *bundled* markdown
    // injection query (which reads the fence's info string), so this is one document
    // injecting `LANGS.len()` uncached grammars.
    let mut text = String::from("# hi\n\n");
    for lang in LANGS {
        text.push_str(&format!("```{lang}\n"));
        for i in 0..200 {
            text.push_str(&format!("fn f{i}() -> u32 {{ let x = {i}; x + 1 }}\n"));
        }
        text.push_str("```\n\n");
    }
    // A realistic viewport: the server only ever queries the visible lines, and
    // extracting spans for a thousand of them would swamp the signal here.
    let viewport = 80;
    let mut engine = Engine::new(data.0.clone());
    engine.open(BUF, "markdown", &text);

    let mut worst = std::time::Duration::ZERO;
    let mut frames = 0;
    loop {
        frames += 1;
        assert!(frames < 500, "the parse never converged in {frames} frames");
        let started = std::time::Instant::now();
        let _ = engine.highlights(BUF, 0, viewport);
        worst = worst.max(started.elapsed());
        if !engine.parse_pending(BUF) {
            break;
        }
    }

    // One load is allowed on a frame (the one it starts under budget); the slack over
    // it covers that frame's own parsing, which the budget already bounds. Paying
    // them all would take `LANGS.len()` loads.
    let ceiling = load + std::time::Duration::from_millis(100);
    assert!(
        worst < ceiling,
        "a frame took {worst:?} (ceiling {ceiling:?}, one cold load is {load:?}): an \
         over-budget frame paid cold grammar loads instead of deferring them"
    );
    // The point of deferring is that they still all arrive.
    for lang in LANGS {
        assert!(
            engine.injected_languages(BUF).iter().any(|l| l == lang),
            "{lang} never got its injected layer"
        );
    }
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
