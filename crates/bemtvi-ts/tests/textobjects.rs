//! The tree-sitter text-object query source (`Engine::text_objects_at`), over a
//! *real* rust parse: a grammar with a `textobjects.scm` is installed, a function
//! is opened, and the engine must return the byte ranges the `vif` / `daf` / `dia`
//! objects select — the smallest `@function.inner/outer` / `@parameter.*` /
//! `@comment.outer` region **containing** a given byte, innermost first, so a
//! `count` walks outward through nested scopes.
//!
//! Hermetic: the rust grammar compiles out of the cargo registry (no network);
//! `BEMTVI_DATA_DIR` pins the engine's search path to the fixture dir. This is the
//! `bemtvi-ts`-side twin of the server keystroke e2e (`vif`, `daf`, …).

mod fixture;

use bemtvi_core::{BufferId, OpenOutcome};
use bemtvi_ts::Engine;
use fixture::{install_rust_grammar, write_query, TempDir};

/// A compact real rust `textobjects.scm` — the relevant subset of
/// nvim-treesitter-textobjects for the four phase-1 objects. `@function.inner` is
/// captured across the body's statements (`_+`), which the engine unions into the
/// whole inner region.
const RUST_TEXTOBJECTS: &str = r#"
(function_item) @function.outer

(function_item
  body: (block
    .
    "{"
    _+ @function.inner
    "}"))

(struct_item) @class.outer

(line_comment) @comment.outer

(parameters
  (parameter) @parameter.inner @parameter.outer)
"#;

/// Byte offset of the first occurrence of `needle` in `src` (panics if absent).
fn at(src: &str, needle: &str) -> usize {
    src.find(needle)
        .unwrap_or_else(|| panic!("`{needle}` not in source"))
}

/// Open a rust buffer with `RUST_TEXTOBJECTS` installed; returns the engine, the
/// buffer id, and the source it was opened with.
fn open_rust(tag: &str, src: &str) -> (Engine, BufferId, TempDir) {
    let data = TempDir::new(tag);
    install_rust_grammar(&data.0);
    write_query(&data.0, "rust", "textobjects", RUST_TEXTOBJECTS);
    std::env::set_var("BEMTVI_DATA_DIR", &data.0);

    let mut engine = Engine::new(data.0.clone());
    let buf = BufferId(1);
    assert!(matches!(engine.open(buf, "rust", src), OpenOutcome::Ok));
    (engine, buf, data)
}

#[test]
fn function_outer_returns_nested_scopes_innermost_first() {
    let src = "\
fn outer(a: i32, b: i32) -> i32 {
    fn inner() -> i32 {
        1
    }
    a + b
}
";
    let (engine, buf, _data) = open_rust("to_fn_outer", src);

    // Cursor on the `1` inside `inner`'s body: it sits within *both* functions.
    let byte = at(src, "1");
    let ranges = engine.text_objects_at(buf, "function.outer", byte);
    assert_eq!(ranges.len(), 2, "cursor is inside two nested functions");

    // Innermost first: the smaller range is `inner`, the larger is `outer`.
    let (lo0, hi0) = ranges[0];
    let (lo1, hi1) = ranges[1];
    assert!(
        src[lo0..hi0].starts_with("fn inner"),
        "innermost is the inner fn, got {:?}",
        &src[lo0..hi0]
    );
    assert!(
        src[lo1..hi1].starts_with("fn outer") && src[lo1..hi1].contains("a + b"),
        "outermost is the whole outer fn, got {:?}",
        &src[lo1..hi1]
    );
    // Nesting: the inner range is strictly inside the outer one.
    assert!(lo1 <= lo0 && hi0 <= hi1 && (hi0 - lo0) < (hi1 - lo1));
}

#[test]
fn function_inner_unions_the_body_statements() {
    let src = "\
fn f() {
    let x = 1;
    x + 2
}
";
    let (engine, buf, _data) = open_rust("to_fn_inner", src);

    let byte = at(src, "let x");
    let ranges = engine.text_objects_at(buf, "function.inner", byte);
    assert_eq!(ranges.len(), 1, "one enclosing function body");
    let (lo, hi) = ranges[0];
    let inner = &src[lo..hi];
    // The inner region spans *all* the body between the braces (both statements),
    // and excludes the `fn f()` signature and the braces themselves.
    assert!(
        inner.contains("let x = 1;"),
        "inner has the first stmt: {inner:?}"
    );
    assert!(
        inner.contains("x + 2"),
        "inner spans to the last stmt: {inner:?}"
    );
    assert!(
        !inner.contains("fn f"),
        "inner excludes the signature: {inner:?}"
    );
    assert!(
        !inner.starts_with('{') && !inner.ends_with('}'),
        "inner excludes braces: {inner:?}"
    );
}

#[test]
fn parameter_object_targets_the_argument_under_the_cursor() {
    let src = "fn f(alpha: i32, beta: i32) {}\n";
    let (engine, buf, _data) = open_rust("to_param", src);

    let byte = at(src, "beta");
    let ranges = engine.text_objects_at(buf, "parameter.inner", byte);
    assert!(!ranges.is_empty(), "a parameter surrounds the cursor");
    let (lo, hi) = ranges[0];
    assert_eq!(
        &src[lo..hi],
        "beta: i32",
        "the innermost parameter is `beta: i32`"
    );
}

#[test]
fn comment_object_targets_the_line_comment() {
    let src = "\
fn f() {
    // a note
    1
}
";
    let (engine, buf, _data) = open_rust("to_comment", src);

    let byte = at(src, "note");
    let ranges = engine.text_objects_at(buf, "comment.outer", byte);
    assert!(!ranges.is_empty(), "a comment surrounds the cursor");
    let (lo, hi) = ranges[0];
    assert_eq!(&src[lo..hi], "// a note");
}

#[test]
fn no_object_off_the_construct_returns_empty() {
    let src = "\
fn f() {
    1
}

const X: i32 = 0;
";
    let (engine, buf, _data) = open_rust("to_empty", src);

    // On the top-level `const` there is no function / comment / parameter.
    let byte = at(src, "const");
    assert!(engine
        .text_objects_at(buf, "function.outer", byte)
        .is_empty());
    assert!(engine
        .text_objects_at(buf, "comment.outer", byte)
        .is_empty());
    assert!(engine
        .text_objects_at(buf, "parameter.inner", byte)
        .is_empty());
}
