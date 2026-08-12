//! Tree-sitter text objects (`vif`, `daf`, `dia`, `d2af`, …) end-to-end over a
//! *real* rust parse: a grammar with a `textobjects.scm` is installed into a temp
//! data dir, a `.rs` file is opened, and the keystroke path — the `ObjectKind`
//! grammar → `resolve_text_object` → the syntax engine query → the shared
//! text-object applier — must select the syntactic construct at the cursor. This
//! is the server-level twin of the `bemtvi-ts` engine test (`text_objects_at`),
//! covering the dispatch the engine test cannot: the `i`/`a` → `.inner`/`.outer`
//! suffix, the `count`-th enclosing scope, and both operator and visual modes.
//!
//! Hermetic: the rust grammar's C sources compile out of the cargo registry (a
//! dev-dependency, so they are present) — no network `:TSInstall` — and
//! `BEMTVI_DATA_DIR` pins the engine's search path to the temp dir. It does need a
//! C compiler; absent one, the test skips (the external-dependency convention).

use std::path::{Path, PathBuf};

use bemtvi_server::ServerInit;
use bemtvi_test_harness::*;

/// A compact real rust `textobjects.scm` — the phase-1 subset of
/// nvim-treesitter-textobjects. `@function.inner` is captured across the body's
/// statements (`_+`), which the engine unions into the whole inner region.
const RUST_TEXTOBJECTS: &str = r#"
(function_item) @function.outer

(function_item
  body: (block
    .
    "{"
    _+ @function.inner
    "}"))

(line_comment) @comment.outer

(parameters
  (parameter) @parameter.inner @parameter.outer)
"#;

/// Locate an unpacked crate source dir in the cargo registry by its
/// `<name>-<version>` folder (a dev-dependency, so cargo guarantees presence).
fn registry_crate_dir(crate_dir: &str) -> PathBuf {
    let cargo_home = std::env::var("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").expect("HOME or CARGO_HOME must be set"))
                .join(".cargo")
        });
    let registry = cargo_home.join("registry").join("src");
    for index in std::fs::read_dir(&registry).expect("read cargo registry src") {
        let candidate = index.unwrap().path().join(crate_dir);
        if candidate.is_dir() {
            return candidate;
        }
    }
    panic!("{crate_dir} source not found under {registry:?}");
}

/// Compile the rust grammar's C sources into `<root>/parser/rust.so` and write its
/// highlights + our textobjects query — the hermetic install the engine reads.
/// Returns `false` when no C compiler is available (the test then skips).
fn install_rust_grammar(root: &Path) -> bool {
    let src = registry_crate_dir("tree-sitter-rust-0.24.2").join("src");
    let parser_dir = root.join("parser");
    std::fs::create_dir_all(&parser_dir).unwrap();
    let compiler = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let status = std::process::Command::new(&compiler)
        .args(["-shared", "-fPIC", "-O1"])
        .arg("-I")
        .arg(&src)
        .arg(src.join("parser.c"))
        .arg(src.join("scanner.c"))
        .arg("-o")
        .arg(parser_dir.join("rust.so"))
        .status();
    match status {
        Ok(s) if s.success() => {}
        _ => return false,
    }
    let qdir = root.join("queries").join("rust");
    std::fs::create_dir_all(&qdir).unwrap();
    std::fs::write(
        qdir.join("highlights.scm"),
        tree_sitter_rust::HIGHLIGHTS_QUERY,
    )
    .unwrap();
    std::fs::write(qdir.join("textobjects.scm"), RUST_TEXTOBJECTS).unwrap();
    true
}

/// The nested-function sample every test opens. `1` occurs only as the inner
/// body literal (`i32` has no `1`), so `/1<CR>` reliably parks the cursor there.
const SAMPLE: &str = "\
fn outer(alpha: i32) -> i32 {
    fn inner() -> i32 {
        1
    }
    alpha
}
";

/// Holds the temp data dir alive for the test and removes it (and clears the env)
/// on drop, so parallel binaries don't see a stale `BEMTVI_DATA_DIR`.
struct TempKeep(PathBuf);
impl Drop for TempKeep {
    fn drop(&mut self) {
        std::env::remove_var("BEMTVI_DATA_DIR");
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Take the process-global `serial_lock`, install the rust grammar into a fresh
/// `BEMTVI_DATA_DIR`, and open `SAMPLE` as a `.rs` buffer — binding
/// `(rpc, _incoming, _guard, _keep)`. `return`s early (skips the test) when there
/// is no C compiler. A macro, not a fn, so the un-exported `Rpc`/`Incoming` types
/// are inferred rather than named.
macro_rules! open_sample {
    ($tag:expr) => {{
        let guard = serial_lock().lock().await;
        let data = temp_dir($tag);
        if !install_rust_grammar(&data) {
            eprintln!("skip: no C compiler to build the rust grammar fixture");
            return;
        }
        std::env::set_var("BEMTVI_DATA_DIR", &data);
        let file = write_temp($tag, "rs", SAMPLE);
        let (rpc, incoming) = start_attached(
            ServerInit {
                file: Some(file),
                ..Default::default()
            },
            80,
            24,
        )
        .await;
        (rpc, incoming, guard, TempKeep(data))
    }};
}

#[tokio::test]
async fn daf_deletes_the_innermost_function() {
    let (rpc, _incoming, _g, _d) = open_sample!("to_daf");
    // Cursor on the inner body literal `1`, then delete-around-function.
    feed(&rpc, "/1<CR>");
    feed(&rpc, "daf");
    let text = lines(&rpc).await.join("\n");
    assert!(!text.contains("fn inner"), "inner fn removed: {text:?}");
    assert!(text.contains("fn outer"), "outer fn kept: {text:?}");
    assert!(text.contains("alpha"), "outer body kept: {text:?}");
}

#[tokio::test]
async fn count_af_deletes_the_enclosing_function() {
    let (rpc, _incoming, _g, _d) = open_sample!("to_count");
    // From inside `inner`, `d2af` targets the *2nd* enclosing function — `outer`,
    // which contains everything, so the buffer empties.
    feed(&rpc, "/1<CR>");
    feed(&rpc, "d2af");
    let text = lines(&rpc).await.join("\n");
    assert!(!text.contains("fn outer"), "outer fn removed: {text:?}");
    assert!(
        !text.contains("fn inner"),
        "inner fn removed with it: {text:?}"
    );
}

#[tokio::test]
async fn dif_deletes_the_inner_function_body() {
    let (rpc, _incoming, _g, _d) = open_sample!("to_dif");
    feed(&rpc, "/1<CR>");
    feed(&rpc, "dif");
    let text = lines(&rpc).await.join("\n");
    // The signature and braces survive; only the body statement (`1`) is gone.
    assert!(
        text.contains("fn inner() -> i32 {"),
        "inner signature kept: {text:?}"
    );
    assert!(
        !text.contains('1'),
        "inner body statement deleted: {text:?}"
    );
}

#[tokio::test]
async fn dia_deletes_the_argument_under_the_cursor() {
    let (rpc, _incoming, _g, _d) = open_sample!("to_dia");
    // `/alpha<CR>` lands on the signature's `alpha` (first occurrence).
    feed(&rpc, "/alpha<CR>");
    feed(&rpc, "dia");
    let first = lines(&rpc).await[0].clone();
    assert_eq!(first, "fn outer() -> i32 {", "parameter deleted: {first:?}");
}

#[tokio::test]
async fn vif_then_delete_matches_the_operator_form() {
    let (rpc, _incoming, _g, _d) = open_sample!("to_vif");
    // Visual path: `vif` selects the inner function body, `d` deletes the
    // selection — same effect as `dif`, proving the visual branch resolves too.
    feed(&rpc, "/1<CR>");
    feed(&rpc, "vifd");
    let text = lines(&rpc).await.join("\n");
    assert!(
        text.contains("fn inner() -> i32 {"),
        "inner signature kept: {text:?}"
    );
    assert!(
        !text.contains('1'),
        "inner body deleted via visual selection: {text:?}"
    );
}

#[tokio::test]
async fn user_registered_object_key_resolves() {
    let (rpc, _incoming, _g, _d) = open_sample!("to_map_new");
    // Bind a brand-new key `g` (no built-in object meaning) to @function.outer.
    exec_lua(&rpc, "btv.textobject.map('ig', '@function.outer')").await;
    // Cursor in the inner fn, `dig` deletes the whole innermost function.
    feed(&rpc, "/1<CR>");
    feed(&rpc, "dig");
    let text = lines(&rpc).await.join("\n");
    assert!(
        !text.contains("fn inner"),
        "user-mapped `ig` deleted the fn: {text:?}"
    );
    assert!(text.contains("fn outer"), "outer fn kept: {text:?}");
}

#[tokio::test]
async fn user_registry_overrides_a_builtin_verbatim() {
    let (rpc, _incoming, _g, _d) = open_sample!("to_map_override");
    // Override the built-in `if` (normally the function *inner* body) to the *outer*
    // capture — proving the registry wins over the built-in AND uses the capture
    // verbatim (no i→.inner suffixing).
    exec_lua(&rpc, "btv.textobject.map('if', '@function.outer')").await;
    feed(&rpc, "/1<CR>");
    feed(&rpc, "dif");
    let text = lines(&rpc).await.join("\n");
    // Built-in `dif` would delete only `1`; the override deletes the whole inner fn.
    assert!(
        !text.contains("fn inner"),
        "override deleted the whole fn: {text:?}"
    );
    assert!(text.contains("fn outer"), "outer fn kept: {text:?}");
}

#[tokio::test]
async fn unmap_reverts_to_the_builtin() {
    let (rpc, _incoming, _g, _d) = open_sample!("to_unmap");
    exec_lua(&rpc, "btv.textobject.map('if', '@function.outer')").await;
    exec_lua(&rpc, "btv.textobject.unmap('if')").await;
    // With the override removed, `dif` is the built-in inner-body object again:
    // deletes only the body statement, keeping the signature.
    feed(&rpc, "/1<CR>");
    feed(&rpc, "dif");
    let text = lines(&rpc).await.join("\n");
    assert!(
        text.contains("fn inner() -> i32 {"),
        "inner signature kept (built-in dif): {text:?}"
    );
    assert!(!text.contains('1'), "only the inner body deleted: {text:?}");
    assert!(text.contains("fn outer"), "outer fn untouched: {text:?}");
}

// ----- Helix match-mode tree-sitter text objects ---------------------------
//
// The Helix editing model reaches the same syntactic objects through match mode
// (`maf` / `mif` / `mia`), routed through the shared `resolve_text_object`
// dispatch — so the tree-sitter captures and the `btv.textobject.map` registry
// work identically under `:helix`. In Helix, `mi`/`ma` *select* the object at
// each selection's head; a following `d` deletes the selection.

#[tokio::test]
async fn helix_maf_selects_the_innermost_function() {
    let (rpc, _incoming, _g, _d) = open_sample!("to_hx_maf");
    // Cursor on the inner body literal `1`, enter Helix, select-around-function,
    // then delete the selection.
    feed(&rpc, "/1<CR>:helix<CR>");
    feed(&rpc, "mafd");
    let text = lines(&rpc).await.join("\n");
    assert!(!text.contains("fn inner"), "inner fn removed: {text:?}");
    assert!(text.contains("fn outer"), "outer fn kept: {text:?}");
    assert!(text.contains("alpha"), "outer body kept: {text:?}");
}

#[tokio::test]
async fn helix_count_maf_selects_the_enclosing_function() {
    let (rpc, _incoming, _g, _d) = open_sample!("to_hx_count");
    // From inside `inner`, `2maf` selects the *2nd* enclosing function — `outer`,
    // which contains everything, so deleting the selection empties the buffer.
    feed(&rpc, "/1<CR>:helix<CR>");
    feed(&rpc, "2mafd");
    let text = lines(&rpc).await.join("\n");
    assert!(!text.contains("fn outer"), "outer fn removed: {text:?}");
    assert!(
        !text.contains("fn inner"),
        "inner fn removed with it: {text:?}"
    );
}

#[tokio::test]
async fn helix_mia_selects_the_argument_under_the_cursor() {
    let (rpc, _incoming, _g, _d) = open_sample!("to_hx_mia");
    // `/alpha<CR>` lands on the signature's `alpha`; `mia` selects the parameter.
    feed(&rpc, "/alpha<CR>:helix<CR>");
    feed(&rpc, "miad");
    let first = lines(&rpc).await[0].clone();
    assert_eq!(first, "fn outer() -> i32 {", "parameter deleted: {first:?}");
}

#[tokio::test]
async fn helix_user_registered_object_key_resolves() {
    let (rpc, _incoming, _g, _d) = open_sample!("to_hx_map");
    // The `btv.textobject.map` registry feeds Helix match mode too: bind `g` to
    // @function.outer, then `mig` deletes the whole innermost function.
    exec_lua(&rpc, "btv.textobject.map('ig', '@function.outer')").await;
    feed(&rpc, "/1<CR>:helix<CR>");
    feed(&rpc, "migd");
    let text = lines(&rpc).await.join("\n");
    assert!(
        !text.contains("fn inner"),
        "user-mapped `ig` selected the fn: {text:?}"
    );
    assert!(text.contains("fn outer"), "outer fn kept: {text:?}");
}
