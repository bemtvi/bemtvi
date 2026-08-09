//! The injection layers are re-derived **incrementally** on an edit.
//!
//! `Engine::edit` used to run the host grammar's injection query over the *entire*
//! tree on every edit — a `QueryCursor` with no byte range, unlike the highlight
//! extraction next to it, which clips to the viewport. On a per-keystroke path that
//! made typing cost grow with the whole buffer: 83% of tree-sitter's typing cost on
//! an 8000-line file, in a document containing no injected regions at all. It now
//! re-queries only the byte ranges the reparse reports as changed and shifts the
//! rest of the cached region list through the edit. See
//! `docs/plans/2026-08-08-per-keystroke-costs-round-2.md`.
//!
//! The failure mode of "only re-query what changed" is an injection that silently
//! stops updating, so every test here asserts against what actually *paints*: a rust
//! `keyword` span, which the host markdown layer never produces, so its presence (or
//! absence) is proof the injected layer was built (or not).
//!
//! Hermetic: both grammars compile out of the cargo registry (no network);
//! `NXVIM_DATA_DIR` pins the search path to the fixture dir.

mod fixture;

use fixture::{install_markdown_grammar, install_rust_grammar, TempDir};
use nxvim_core::{Buffer, BufferId, OpenOutcome};
use nxvim_ts::Engine;

/// A non-combined injection: every fenced block's content parses as rust. Kept
/// deliberately free of `injection.combined` — a combined pattern accumulates one
/// region-set across matches spread over the whole document, which cannot be
/// re-derived from a byte range, so the engine falls back to the full walk for it
/// (exercised separately below).
const RUST_IN_FENCES: &str = "((fenced_code_block (code_fence_content) @injection.content) \
     (#set! injection.language \"rust\"))";

/// A realistic rust injection that this test file's fixture text never matches — the
/// common real-world shape, where a language *has* an injection query and a given
/// file happens to inject nothing.
const MARKDOWN_IN_HTML_MACRO: &str =
    "((macro_invocation macro: (identifier) @_m (token_tree) @injection.content) \
     (#eq? @_m \"html\") (#set! injection.language \"markdown\"))";

/// An engine plus a real [`Buffer`], driven in lockstep so the edits the engine sees
/// are the ones the editor itself would journal — points, byte offsets and all.
struct Doc {
    engine: Engine,
    buf: Buffer,
    id: BufferId,
}

impl Doc {
    fn new(data: &std::path::Path, query: Option<&str>, text: &str) -> Self {
        let mut engine = Engine::new(data.to_path_buf());
        if let Some(q) = query {
            engine
                .set_query("markdown", "injections", Some(q.to_string()))
                .expect("install the injection query");
        }
        let mut buf = Buffer::empty();
        buf.insert(0, text);
        buf.normalize();
        let _ = buf.take_edits();
        let id = BufferId(1);
        assert!(matches!(
            engine.open(id, "markdown", &buf.text.to_string()),
            OpenOutcome::Ok
        ));
        Doc { engine, buf, id }
    }

    /// Apply an edit through the buffer's own journal — the production path.
    fn edit(&mut self, f: impl FnOnce(&mut Buffer)) {
        f(&mut self.buf);
        self.buf.normalize();
        let batch = self.buf.take_edits();
        self.engine.edit(self.id, &batch.edits);
    }

    fn insert(&mut self, byte: usize, s: &str) {
        self.edit(|b| b.insert(byte, s));
    }

    fn remove(&mut self, range: std::ops::Range<usize>) {
        self.edit(|b| b.remove(range));
    }

    /// The highlight groups painted over the whole document.
    fn groups(&mut self) -> Vec<String> {
        let last = self.buf.line_count();
        self.engine
            .highlights(self.id, 0, last)
            .into_iter()
            .map(|s| s.group)
            .collect()
    }

    /// The 0-based lines carrying a rust `keyword` span — proof the injected layer
    /// built, and *where*.
    fn keyword_lines(&mut self) -> Vec<usize> {
        let last = self.buf.line_count();
        let mut lines: Vec<usize> = self
            .engine
            .highlights(self.id, 0, last)
            .into_iter()
            .filter(|s| s.group.starts_with("keyword"))
            .map(|s| s.line)
            .collect();
        lines.sort_unstable();
        lines.dedup();
        lines
    }

    fn byte_of_line(&self, line: usize) -> usize {
        self.buf.line_start(line)
    }
}

/// Prose, then a fenced rust block — the injected `fn` is on line 4.
fn doc_with_block() -> String {
    "# Title\n\nsome prose here\n\n```rust\nfn f() {}\n```\n\nmore prose\n".to_string()
}

fn setup(tag: &str) -> TempDir {
    let data = TempDir::new(tag);
    install_rust_grammar(&data.0);
    install_markdown_grammar(&data.0);
    std::env::set_var("NXVIM_DATA_DIR", &data.0);
    data
}

#[test]
fn an_injection_far_from_the_edit_keeps_painting() {
    // The core risk of re-querying only what changed: an injection nowhere near the
    // edit must survive, at the right place, edit after edit.
    let data = setup("inj_far");
    let mut doc = Doc::new(&data.0, Some(RUST_IN_FENCES), &doc_with_block());
    assert_eq!(
        doc.keyword_lines(),
        vec![5],
        "the fenced rust block paints a keyword to begin with: {:?}",
        doc.groups()
    );

    // Twenty edits on line 2, far above the block.
    for _ in 0..20 {
        let at = doc.byte_of_line(2);
        doc.insert(at, "x");
        assert_eq!(
            doc.keyword_lines(),
            vec![5],
            "the injection stopped painting after an edit above it",
        );
    }
}

#[test]
fn an_injection_moves_with_text_inserted_above_it() {
    // The cached regions are byte ranges; text inserted above shifts them. Getting
    // the shift wrong points the child parse at the wrong bytes, which shows up as
    // the keyword moving to the wrong line (or vanishing).
    let data = setup("inj_shift");
    let mut doc = Doc::new(&data.0, Some(RUST_IN_FENCES), &doc_with_block());
    assert_eq!(doc.keyword_lines(), vec![5]);

    let at = doc.byte_of_line(1);
    doc.insert(at, "added one\nadded two\n");
    assert_eq!(
        doc.keyword_lines(),
        vec![7],
        "two lines inserted above must carry the injected block down by two",
    );

    // …and back up again when they are removed.
    let from = doc.byte_of_line(1);
    let to = doc.byte_of_line(3);
    doc.remove(from..to);
    assert_eq!(doc.keyword_lines(), vec![5]);
}

#[test]
fn a_newly_typed_injection_starts_painting() {
    // A region that did not exist before must be *found* by the incremental
    // re-query. This is the direction a purely "shift what we had" update misses.
    let data = setup("inj_new");
    let mut doc = Doc::new(&data.0, Some(RUST_IN_FENCES), "# Title\n\nprose\n");
    assert!(
        doc.keyword_lines().is_empty(),
        "no injection to start with: {:?}",
        doc.groups()
    );

    let at = doc.byte_of_line(3);
    doc.insert(at, "\n```rust\nfn added() {}\n```\n");
    assert_eq!(
        doc.keyword_lines(),
        vec![5],
        "a freshly typed fenced block must start painting: {:?}",
        doc.groups()
    );
}

#[test]
fn an_injection_grown_line_by_line_paints_as_it_is_typed() {
    // The realistic shape of the above: the block is *typed*, one edit at a time, so
    // the region only becomes a valid injection partway through.
    let data = setup("inj_typed");
    let mut doc = Doc::new(&data.0, Some(RUST_IN_FENCES), "# Title\n\nprose\n");
    for chunk in ["```rust\n", "fn typed", "() {}\n", "```\n"] {
        let end = doc.buf.line_start(doc.buf.line_count());
        doc.insert(end, chunk);
    }
    let typed = doc.keyword_lines();
    let final_text = doc.buf.text.to_string();
    let mut fresh = Doc::new(&data.0, Some(RUST_IN_FENCES), &final_text);
    assert!(
        !typed.is_empty(),
        "the block must paint once it is complete: {:?}",
        doc.groups()
    );
    assert_eq!(
        typed,
        fresh.keyword_lines(),
        "a block reached by typing must paint exactly where the same text opened \
         fresh does\ntext: {final_text:?}",
    );
}

#[test]
fn a_deleted_injection_stops_painting() {
    // The other direction a stale cache gets wrong: a region that is gone must stop
    // being parsed, not keep painting from an edit-shifted leftover.
    let data = setup("inj_del");
    let mut doc = Doc::new(&data.0, Some(RUST_IN_FENCES), &doc_with_block());
    assert_eq!(doc.keyword_lines(), vec![5]);

    let from = doc.byte_of_line(4);
    let to = doc.byte_of_line(7);
    doc.remove(from..to);
    assert!(
        doc.keyword_lines().is_empty(),
        "the deleted block must stop painting: {:?}",
        doc.groups()
    );
}

#[test]
fn editing_inside_an_injection_keeps_it_painting() {
    // The edit lands *within* the injected region, so the region itself is in the
    // changed range and gets re-derived rather than shifted.
    let data = setup("inj_inside");
    let mut doc = Doc::new(&data.0, Some(RUST_IN_FENCES), &doc_with_block());
    assert_eq!(doc.keyword_lines(), vec![5]);

    let at = doc.byte_of_line(6);
    doc.insert(at, "fn g() {}\n");
    assert_eq!(
        doc.keyword_lines(),
        vec![5, 6],
        "both functions in the block must paint: {:?}",
        doc.groups()
    );
}

#[test]
fn changing_the_text_a_predicate_reads_re_derives_the_injection() {
    // The subtle one, and the query markdown actually ships: the injected language is
    // read from the fence's info string, so the text the match depends on lives
    // *outside* the region it injects. Rewriting `rust` as `ruby` is a same-length,
    // same-shape token substitution — the content range does not move and the tree
    // structure is unchanged; only the bytes naming the language differ.
    //
    // A cached region can therefore only be invalidated here if the dirty set covers
    // the *whole match* — every node it matched on — rather than just the content it
    // injects. Nothing else in this file exercises that.
    let data = setup("inj_pred");
    let by_info = "(fenced_code_block (info_string (language) @injection.language) \
         (code_fence_content) @injection.content)";
    let text = "# Title\n\n```rust\nfn f() {}\n```\n";
    let mut doc = Doc::new(&data.0, Some(by_info), text);
    assert_eq!(
        doc.keyword_lines(),
        vec![3],
        "the rust fence paints to begin with: {:?}",
        doc.groups()
    );

    // `rust` -> `ruby`: same length, same shape, different predicate result.
    let at = doc.byte_of_line(2) + 3;
    doc.remove(at..at + 4);
    doc.insert(at, "ruby");
    assert_eq!(doc.buf.text.to_string().lines().nth(2), Some("```ruby"));
    assert!(
        doc.keyword_lines().is_empty(),
        "the fence is no longer rust, so the rust layer must stop painting: {:?}",
        doc.groups()
    );

    // …and back again.
    doc.remove(at..at + 4);
    doc.insert(at, "rust");
    assert_eq!(
        doc.keyword_lines(),
        vec![3],
        "and start again when the info string says rust: {:?}",
        doc.groups()
    );
}

#[test]
fn a_combined_injection_query_still_updates_after_an_edit() {
    // A combined pattern gathers ranges from across the whole document into one
    // region-set, so it cannot be re-derived from a byte window and the engine falls
    // back to the full walk. That fallback must still be *correct* after edits — it
    // is the path every markdown buffer takes.
    let data = setup("inj_combined");
    let combined = "((fenced_code_block (code_fence_content) @injection.content) \
         (#set! injection.language \"rust\") (#set! injection.combined))";
    let mut doc = Doc::new(&data.0, Some(combined), &doc_with_block());
    assert_eq!(
        doc.keyword_lines(),
        vec![5],
        "combined injection paints to begin with: {:?}",
        doc.groups()
    );

    let at = doc.byte_of_line(1);
    doc.insert(at, "added\n");
    assert_eq!(
        doc.keyword_lines(),
        vec![6],
        "the combined region must follow the edit: {:?}",
        doc.groups()
    );
}

#[test]
fn an_injection_query_does_not_cost_a_walk_of_the_whole_tree() {
    // The regression guard, and the purest statement of the bug: the query below
    // matches *nothing* in this document — there is no `html!` macro in it — yet
    // running it over the whole tree on every edit cost 15x the same edits with no
    // injection query installed at all (176 ms → 2.69 s over 200 edits on a
    // ~2000-line file). Most real files are exactly this case: an injection query
    // exists for the language and the file happens to inject nothing.
    //
    // The baseline is the identical document and edits with no injection query,
    // which the bundled rust grammar ships as — so both halves do the same reparse
    // work on the same text in the same process, and only the injection handling
    // differs.
    let data = setup("inj_perf");
    let mut body = String::new();
    for i in 0..330 {
        body.push_str(&format!(
            "fn f_{i}(a: u32, b: &str) -> u32 {{\n    let t = (a, b, \"lit{i}\");\n    \
             if a > 1 {{\n        return a + 1;\n    }}\n    a\n}}\n"
        ));
    }

    let elapsed = |query: Option<&str>| {
        let mut engine = Engine::new(data.0.clone());
        if let Some(q) = query {
            engine
                .set_query("rust", "injections", Some(q.to_string()))
                .expect("install the injection query");
        }
        let mut buf = Buffer::empty();
        buf.insert(0, &body);
        buf.normalize();
        let _ = buf.take_edits();
        let id = BufferId(1);
        assert!(matches!(
            engine.open(id, "rust", &buf.text.to_string()),
            OpenOutcome::Ok
        ));
        let at = buf.line_start(1);
        let started = std::time::Instant::now();
        for _ in 0..200 {
            buf.insert(at, "z");
            buf.normalize();
            let batch = buf.take_edits();
            engine.edit(id, &batch.edits);
        }
        started.elapsed()
    };

    let baseline = elapsed(None);
    let injected = elapsed(Some(MARKDOWN_IN_HTML_MACRO));
    let ratio = injected.as_secs_f64() / baseline.as_secs_f64().max(0.000_001);
    assert!(
        ratio < 3.0,
        "200 edits with an injection query that matches nothing cost {ratio:.1}x the \
         same edits with no query ({injected:?} vs {baseline:?}) — the injection query \
         is walking the whole tree on every edit again",
    );
}
