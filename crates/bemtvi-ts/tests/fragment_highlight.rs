//! `Engine::highlight_fragment` — the **fragment** twin of the stateless
//! off-buffer highlighter, behind the LSP doc floats (hover / completion docs).
//!
//! The problem it exists for: a hover code block is not a program. It is either a
//! *fragment* of the real language (a struct field, a bare statement) or an
//! annotation dialect the server invented for display (`lua_ls` puts
//! `function f(t: table)` in a ` ```lua ` fence). Handed to a whole-file
//! highlighter, the second kind doesn't degrade — it comes out **confidently
//! wrong**, because a structural query matched a construct that isn't there
//! (`Vec` in `field: Vec<String>` paints as `constructor`).
//!
//! Fragment mode trusts structure only where the parse is sound: inside an `ERROR`
//! region the host layer's captures are dropped and the region is repainted from
//! the leaves' own token kinds (keywords, strings, numbers, comments, punctuation),
//! which survive error recovery. A fragment that parses cleanly is untouched.
//!
//! Hermetic: the rust grammar compiles out of the cargo registry (no network) into
//! a temp fixture dir the engine is pointed at directly.

mod fixture;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use bemtvi_ts::Engine;
use fixture::{install_rust_grammar, write_query, TempDir};

/// The fixture data dir, built **once** for this binary and reused across runs.
/// Compiling the rust grammar is a full `cc` of `parser.c`, and these tests run
/// alongside the rest of the workspace suite — one compile per run, not one per
/// test, so the suite's timing-sensitive tests don't contend with a compiler
/// storm. A fixed path (the same convention as `crates/bemtvi/tests/syntax.rs`)
/// so repeat runs overwrite rather than accumulate.
fn fixture_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join("bemtvi-ts-fragment-fixture");
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        install_rust_grammar(&dir);
        dir
    })
}

/// `(text, group)` for every span, so an assertion can name the token rather than
/// its byte offsets.
fn painted(engine: &mut Engine, text: &str, fragment: bool) -> Vec<(String, String)> {
    let n = text.lines().count();
    let spans = if fragment {
        engine.highlight_fragment("rust", text, 0, n)
    } else {
        engine.highlight_text("rust", text, 0, n)
    };
    spans
        .iter()
        .map(|s| {
            let line = text.lines().nth(s.line).unwrap_or("");
            (
                line[s.start_byte.min(line.len())..s.end_byte.min(line.len())].to_string(),
                s.group.clone(),
            )
        })
        .collect()
}

fn group_of<'a>(spans: &'a [(String, String)], token: &str) -> Option<&'a str> {
    spans
        .iter()
        .find(|(t, _)| t == token)
        .map(|(_, g)| g.as_str())
}

fn engine() -> Engine {
    Engine::new(fixture_dir().to_path_buf())
}

/// The motivating mispaint: a field hover (`field: Vec<String>`) is 94% `ERROR`
/// bytes, and the recovered parse paints `Vec` / `String` as `constructor` — a
/// confident lie. Fragment mode must drop those, and repaint the region from the
/// tokens it can actually vouch for (the `:` delimiter, the `<` / `>` operators —
/// the latter aren't painted at all today).
#[test]
fn a_capture_recovered_from_inside_an_error_is_dropped_and_repainted() {
    let mut engine = engine();
    let text = "field: Vec<String>\n";

    let whole_file = painted(&mut engine, text, false);
    assert_eq!(
        group_of(&whole_file, "Vec"),
        Some("constructor"),
        "precondition: the whole-file path mispaints `Vec` as a constructor; got {whole_file:?}"
    );

    let frag = painted(&mut engine, text, true);
    assert_eq!(
        group_of(&frag, "Vec"),
        None,
        "a capture recovered from inside an ERROR must not survive; got {frag:?}"
    );
    assert_eq!(
        group_of(&frag, "String"),
        None,
        "a capture recovered from inside an ERROR must not survive; got {frag:?}"
    );
    assert_eq!(
        group_of(&frag, ":"),
        Some("punctuation.delimiter"),
        "the token repaint must paint the `:` delimiter; got {frag:?}"
    );
    assert_eq!(
        group_of(&frag, "<"),
        Some("operator"),
        "the token repaint must paint `<` (unpainted by the whole-file path); got {frag:?}"
    );
}

/// Suppression is about *structure*, not about the whole region: inside an `ERROR`
/// the lexer still worked, so a capture that merely classifies a token must
/// survive — and survive with the grammar's own group (rust calls an integer
/// `constant.builtin`), not the coarse one the repaint would have guessed.
#[test]
fn a_literals_own_capture_survives_inside_an_error() {
    let mut engine = engine();
    let frag = painted(&mut engine, "value: \"hi\" 42 // note\n", true);

    assert_eq!(
        group_of(&frag, "\"hi\""),
        Some("string"),
        "a string keeps its capture, quotes included; got {frag:?}"
    );
    assert_eq!(
        group_of(&frag, "42"),
        Some("constant.builtin"),
        "the grammar's own literal capture wins over the repaint's coarse `number`; \
         got {frag:?}"
    );
    assert_eq!(
        group_of(&frag, "// note"),
        Some("comment"),
        "a comment keeps its colour; got {frag:?}"
    );
}

/// The `lua_ls` case, reproduced hermetically: a grammar that captures its keyword
/// **structurally** (`(function_item "fn" @keyword.function)`, the shape lua's
/// query uses) loses that keyword the moment the construct fails to parse — which
/// is exactly what a `function f(a: string)` hover does. The repaint recovers it
/// from the anonymous token itself, with no per-language table.
#[test]
fn a_structurally_captured_keyword_stranded_in_an_error_is_repainted() {
    // A second data dir over the *same* compiled parser (copied, not recompiled) with
    // a one-pattern query: `fn` is painted only as part of a well-formed function.
    let data = TempDir::new("fragment_structural_kw");
    std::fs::create_dir_all(data.0.join("parser")).unwrap();
    std::fs::copy(
        fixture_dir().join("parser/rust.so"),
        data.0.join("parser/rust.so"),
    )
    .expect("copy the compiled fixture parser");
    write_query(
        &data.0,
        "rust",
        "highlights",
        "(function_item \"fn\" @keyword.function)\n",
    );
    let mut engine = Engine::new(data.0.clone());

    // A body-less signature *with* a display-only annotation prefix — the shape a
    // hover arrives in. The `function_item` never forms, so the query cannot fire.
    let text = "(method) fn bar(x: u32)\n";

    let whole_file = painted(&mut engine, text, false);
    assert_eq!(
        group_of(&whole_file, "fn"),
        None,
        "precondition: the structural capture cannot fire under an ERROR, so the \
         whole-file path leaves `fn` plain; got {whole_file:?}"
    );

    let frag = painted(&mut engine, text, true);
    assert_eq!(
        group_of(&frag, "fn"),
        Some("keyword"),
        "the repaint recovers the stranded keyword from the token itself; got {frag:?}"
    );
}

/// The no-regression half: a fragment that parses cleanly (most rust-analyzer
/// hovers, every `:help` example) has no `ERROR` region at all, so fragment mode
/// must be byte-identical to the whole-file path — no suppression, no repaint.
#[test]
fn a_clean_fragment_is_identical_to_the_whole_file_path() {
    let mut engine = engine();
    for text in [
        "pub fn frob(x: &str) -> bool\n", // a signature with no body: MISSING, not ERROR
        "let x = some_call(a, b)\n",
        "fn f(x: u32) -> bool { x > 0 }\n",
    ] {
        let whole_file = painted(&mut engine, text, false);
        let frag = painted(&mut engine, text, true);
        assert_eq!(
            whole_file, frag,
            "a cleanly-parsed fragment must highlight identically; {text:?}"
        );
        assert!(
            !frag.is_empty(),
            "sanity: {text:?} should paint something at all"
        );
    }
}

// ---- the framing ladder ----------------------------------------------------

/// A fragment that doesn't stand on its own gets its **structure** back when a
/// framing makes it whole: `field: Vec<String>` inside `struct __btv { … }` parses
/// cleanly, so `Vec` comes back as a real `@type` — not the `@constructor` the
/// unframed recovery invented, and not merely the unpainted identifier Phase 1's
/// conservative repaint leaves.
#[test]
fn a_framing_that_parses_cleanly_recovers_the_real_structure() {
    let mut engine = engine();
    let text = "field: Vec<String>\n";

    // Without a framing: the repaint refuses to guess, so the type is unpainted.
    assert_eq!(
        group_of(&painted(&mut engine, text, true), "Vec"),
        None,
        "precondition: with no fragment context the repaint leaves `Vec` plain"
    );

    engine.set_fragment_context("rust", vec!["struct __btv {\n%s\n}".to_string()]);
    let frag = painted(&mut engine, text, true);

    assert_eq!(
        group_of(&frag, "Vec"),
        Some("type"),
        "the framed parse recovers the real type; got {frag:?}"
    );
    assert_eq!(
        group_of(&frag, "String"),
        Some("type"),
        "the framed parse recovers the real type; got {frag:?}"
    );
    assert_eq!(
        group_of(&frag, "field"),
        Some("property"),
        "…and the field name it could not have known unframed; got {frag:?}"
    );
    // Nothing from the framing itself leaks in, and the columns are the snippet's.
    assert_eq!(
        group_of(&frag, "struct"),
        None,
        "the framing's own tokens must not reach the caller; got {frag:?}"
    );
    assert!(
        frag.iter().all(|(t, _)| text.contains(t.as_str())),
        "every span must land on the snippet's own text; got {frag:?}"
    );
}

/// The ladder is ordered and stops at the **first** framing that parses cleanly, so
/// a config can put its most likely framing first.
#[test]
fn the_first_framing_that_parses_cleanly_wins() {
    let mut engine = engine();
    // A statement is clean inside a function body and broken inside a struct, so
    // whichever order is given, the *function* framing is the one that can win.
    engine.set_fragment_context(
        "rust",
        vec![
            "struct __btv {\n%s\n}".to_string(), // tried first, cannot parse a `let`
            "fn __btv() {\n%s\n}".to_string(),
        ],
    );
    let frag = painted(&mut engine, "let x = 1;\n", true);
    assert_eq!(
        group_of(&frag, "let"),
        Some("keyword"),
        "the ladder skips the framing that fails and takes the next; got {frag:?}"
    );
    assert_eq!(
        group_of(&frag, "1"),
        Some("constant.builtin"),
        "the framed parse is a real one, captures and all; got {frag:?}"
    );
}

/// A same-line framing (`"return %s"`, the shape an *expression* needs) maps back
/// too: the column shift comes off the first line, not just the line index.
#[test]
fn a_same_line_framing_maps_columns_back() {
    let mut engine = engine();
    engine.set_fragment_context("rust", vec!["fn __btv() { %s; }".to_string()]);
    let text = "some_call(1)\n";
    let frag = painted(&mut engine, text, true);

    assert_eq!(
        group_of(&frag, "some_call"),
        Some("function"),
        "the framed expression is highlighted as a real call; got {frag:?}"
    );
    assert_eq!(
        group_of(&frag, "fn"),
        None,
        "the framing's own tokens stay out; got {frag:?}"
    );

    // The columns are the snippet's own, not the offsets inside the framing: the
    // call starts at byte 0 of line 0, though the framed text put it at byte 12.
    let spans = engine.highlight_fragment("rust", text, 0, 1);
    let call = spans
        .iter()
        .find(|s| s.group == "function")
        .expect("a function span");
    assert_eq!(
        (call.line, call.start_byte, call.end_byte),
        (0, 0, 9),
        "the same-line prefix width must come off the first line's columns"
    );
}

/// No framing fits an annotation dialect — it is not a fragment of the language at
/// all — so the ladder falls through to the Phase 1 repaint rather than forcing a
/// framing that merely fails differently.
#[test]
fn a_dialect_no_framing_fits_falls_back_to_the_repaint() {
    let mut engine = engine();
    engine.set_fragment_context(
        "rust",
        vec![
            "fn __btv() {\n%s\n}".to_string(),
            "struct __btv {\n%s\n}".to_string(),
        ],
    );
    let frag = painted(&mut engine, "(method) Foo::bar(x: u32) -> bool\n", true);

    assert!(
        !frag.is_empty(),
        "the repaint still paints what it can vouch for; got {frag:?}"
    );
    assert!(
        frag.iter().all(|(t, _)| t != "Foo"),
        "and still refuses to name a construct it cannot see; got {frag:?}"
    );
}

/// An empty context list turns the ladder off for the language, and a template
/// with no `%s` is not a framing at all (it would wrap nothing) — neither may
/// panic or paint through the framing.
#[test]
fn a_malformed_or_empty_context_list_is_inert() {
    let mut engine = engine();
    let text = "field: Vec<String>\n";

    engine.set_fragment_context("rust", vec!["struct __btv { }".to_string()]); // no `%s`
    assert_eq!(
        group_of(&painted(&mut engine, text, true), "Vec"),
        None,
        "a template with no `%s` is dropped, so the repaint still runs"
    );

    engine.set_fragment_context("rust", Vec::new());
    assert_eq!(
        group_of(&painted(&mut engine, text, true), "Vec"),
        None,
        "an empty list turns the ladder off"
    );
}

/// A framing whose opener is **pure indentation** (`"class __btv:\n    %s"`, what an
/// indentation-sensitive language needs) indents *every* fragment line, not just the
/// first — and takes that width back off every line's columns on the way out. Rust
/// can't show the parse half (it ignores whitespace), but it pins the mechanism:
/// spans on the second line must come back at the fragment's own columns, which only
/// holds if the indent was inserted there and mapped back off.
#[test]
fn an_indenting_framing_indents_and_maps_back_every_line() {
    let mut engine = engine();
    engine.set_fragment_context("rust", vec!["struct __btv {\n    %s\n}".to_string()]);
    let text = "first: u32,\nsecond: Vec<String>,\n";
    let spans = engine.highlight_fragment("rust", text, 0, 2);

    let second: Vec<_> = spans.iter().filter(|s| s.line == 1).collect();
    assert!(
        !second.is_empty(),
        "the second fragment line must be highlighted too; got {spans:?}"
    );
    // `second` is at bytes 0..6 of its own line; inside the framing it sat at 4..10.
    let name = second
        .iter()
        .find(|s| s.group == "property")
        .expect("the field name on line 1");
    assert_eq!(
        (name.start_byte, name.end_byte),
        (0, 6),
        "line 1's columns must be the fragment's own, not the indented ones: {spans:?}"
    );
    // And every span lands within its line, so nothing carries the framing's offset.
    let lens = [
        text.lines().next().unwrap().len(),
        text.lines().nth(1).unwrap().len(),
    ];
    assert!(
        spans.iter().all(|s| s.end_byte <= lens[s.line]),
        "no span may run past its own line: {spans:?}"
    );
}

// ---- the display annotation ------------------------------------------------

/// A hover's leading `(kind) ` is the server's own display annotation, not code:
/// `pyright` writes `(method) join(self, x: str) -> str`, `tsserver`
/// `(property) Foo.bar: number`. It makes the *rest* — a fragment that would have
/// framed cleanly — unparseable, so the whole line drops to the repaint and the
/// signature loses its structure. Peeling the annotation before the ladder gets it
/// back, at the fragment's own columns.
#[test]
fn a_display_annotation_is_peeled_off_before_the_ladder() {
    let mut engine = engine();
    engine.set_fragment_context("rust", vec!["struct __btv {\n%s\n}".to_string()]);
    let text = "(field) count: Vec<String>\n";

    let frag = painted(&mut engine, text, true);
    assert_eq!(
        group_of(&frag, "count"),
        Some("property"),
        "the framed parse of the peeled fragment names the field; got {frag:?}"
    );
    assert_eq!(
        group_of(&frag, "Vec"),
        Some("type"),
        "…and its real type; got {frag:?}"
    );
    assert_eq!(
        group_of(&frag, "(field)"),
        Some("comment"),
        "the annotation itself is painted as the non-code text it is; got {frag:?}"
    );

    // The peel is a coordinate shift, not a rewrite: `count` sits at bytes 8..13 of
    // the line the *caller* handed in, annotation included.
    let spans = engine.highlight_fragment("rust", text, 0, 1);
    let name = spans
        .iter()
        .find(|s| s.group == "property")
        .expect("the field name");
    assert_eq!(
        (name.line, name.start_byte, name.end_byte),
        (0, 8, 13),
        "the annotation's width must go back onto the columns: {spans:?}"
    );
}

/// The peel is all-or-nothing. When what's left still fits no framing — an
/// annotation dialect whose body isn't a fragment of the language either — the
/// snippet falls to the repaint whole, and no annotation span is invented over text
/// the peel didn't actually explain.
#[test]
fn an_annotation_whose_body_still_fails_leaves_no_trace() {
    let mut engine = engine();
    engine.set_fragment_context("rust", vec!["fn __btv() {\n%s\n}".to_string()]);
    let frag = painted(&mut engine, "(method) Foo::bar(x: u32) -> bool\n", true);

    assert!(
        !frag.iter().any(|(_, g)| g == "comment"),
        "an unexplained annotation must not be painted; got {frag:?}"
    );
    assert!(
        frag.iter().all(|(t, _)| t != "Foo"),
        "and the repaint still refuses to name a construct; got {frag:?}"
    );
}

// ---- a block of items ------------------------------------------------------

/// A doc block is often a *list* of items rather than one: `ty` sends every
/// overload of a function as its own signature line. Together they are not a
/// fragment of anything — no framing takes both — so the whole block used to drop
/// to the repaint and lose the structure each line has on its own. Each line is
/// framed in its own right instead, and may take a different rung of the ladder.
#[test]
fn a_block_of_items_is_framed_item_by_item() {
    let mut engine = engine();
    engine.set_fragment_context(
        "rust",
        vec![
            "struct __btv {\n%s\n}".to_string(),
            "fn __btv() {\n%s\n}".to_string(),
        ],
    );
    // A statement and a field: clean in *different* framings, in neither together.
    let text = "let x = 1;\nfield: Vec<String>,\n";

    let frag = painted(&mut engine, text, true);
    assert_eq!(
        group_of(&frag, "let"),
        Some("keyword"),
        "line 0 is framed as a statement; got {frag:?}"
    );
    assert_eq!(
        group_of(&frag, "field"),
        Some("property"),
        "line 1 takes a different rung and is framed as a field; got {frag:?}"
    );
    assert_eq!(
        group_of(&frag, "Vec"),
        Some("type"),
        "…which is structure the whole-block repaint cannot recover; got {frag:?}"
    );

    // Every span lands on its own line, at that line's own columns.
    let spans = engine.highlight_fragment("rust", text, 0, 2);
    let lens = [10, 19];
    assert!(
        spans.iter().all(|s| s.end_byte <= lens[s.line]),
        "no span may run past its own line: {spans:?}"
    );
}

/// A blank line between items is skipped rather than failing the split, and an item
/// carrying its own display annotation is peeled inside it — the two shapes a
/// python hover arrives in (`ty` blank-separates its overloads, `pyright` annotates
/// each one).
#[test]
fn blank_lines_and_per_item_annotations_ride_the_split() {
    let mut engine = engine();
    engine.set_fragment_context(
        "rust",
        vec![
            "struct __btv {\n%s\n}".to_string(),
            "fn __btv() {\n%s\n}".to_string(),
        ],
    );
    let text = "let x = 1;\n\n(field) count: Vec<String>\n";
    let frag = painted(&mut engine, text, true);

    assert_eq!(group_of(&frag, "let"), Some("keyword"), "got {frag:?}");
    assert_eq!(group_of(&frag, "count"), Some("property"), "got {frag:?}");
    assert_eq!(group_of(&frag, "(field)"), Some("comment"), "got {frag:?}");

    let spans = engine.highlight_fragment("rust", text, 0, 3);
    assert!(
        spans.iter().any(|s| s.line == 2),
        "the item after the blank line keeps its own line index: {spans:?}"
    );
    assert!(
        !spans.iter().any(|s| s.line == 1),
        "nothing is painted on the blank line: {spans:?}"
    );
}

/// The split is all-or-nothing too: one line that isn't a whole item of its own
/// means the block isn't a list, and forcing it would highlight the lines that
/// happen to parse out of a context the parse says isn't there. It falls back to the
/// whole-block repaint — which still refuses to name a construct.
#[test]
fn one_unresolvable_line_drops_the_whole_split() {
    let mut engine = engine();
    engine.set_fragment_context("rust", vec!["struct __btv {\n%s\n}".to_string()]);
    let frag = painted(&mut engine, "field: Vec<String>,\n@@@\n", true);

    assert_eq!(
        group_of(&frag, "Vec"),
        None,
        "the split is dropped, so line 0 is not framed either; got {frag:?}"
    );
    assert!(
        !frag.is_empty(),
        "the repaint still paints what it can vouch for; got {frag:?}"
    );
}

/// A same-line opener is *not* indentation, so it stays a first-line-only shift: a
/// second line must not have the opener's width taken off it.
#[test]
fn a_same_line_opener_does_not_shift_later_lines() {
    let mut engine = engine();
    engine.set_fragment_context("rust", vec!["fn __btv() { let __v = %s; }".to_string()]);
    // A two-line expression: the opener shares line 0 only.
    let text = "Some(\n42)\n";
    let spans = engine.highlight_fragment("rust", text, 0, 2);
    let on_line_1: Vec<_> = spans.iter().filter(|s| s.line == 1).collect();
    assert!(
        on_line_1
            .iter()
            .all(|s| s.end_byte <= text.lines().nth(1).unwrap().len()),
        "line 1 keeps its own columns: {spans:?}"
    );
    assert!(
        on_line_1.iter().any(|s| s.start_byte == 0),
        "the `42` starts at column 0 of line 1, unshifted: {spans:?}"
    );
}
