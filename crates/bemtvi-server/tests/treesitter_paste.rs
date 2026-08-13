//! Bracketed paste under **treesitter** auto-indentation, over a real rust parse.
//!
//! The `editing::paste` suite covers the paste guard on the grammar-free indents
//! (`smartindent` / `autoindent`); this one covers the branch that fronts them —
//! a real `indents.scm` verdict from the syntax engine — because that is the
//! configuration the bug was reported on: pasting already-indented code into a
//! treesitter-indented buffer re-indented every line on top of the indentation it
//! already carried, so each line drifted further right.
//!
//! Hermetic, like `treesitter_textobjects.rs`: the rust grammar's C sources
//! compile out of the cargo registry (a dev-dependency, so they are present) — no
//! network `:TSInstall` — and `BEMTVI_DATA_DIR` pins the engine's search path to a
//! temp dir. It does need a C compiler; absent one, the test skips (the
//! external-dependency convention).

use std::path::{Path, PathBuf};

use bemtvi_server::ServerInit;
use bemtvi_test_harness::*;

/// A compact real rust `indents.scm` — enough of the nvim-treesitter query for a
/// braced block to indent its contents and for the closing brace to come back out.
const RUST_INDENTS: &str = r#"
[
  (block)
  (declaration_list)
  (field_declaration_list)
  (arguments)
] @indent.begin

[
  "}"
  ")"
] @indent.branch @indent.end
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
/// highlights + our indents query — the hermetic install the engine reads.
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
    std::fs::write(qdir.join("indents.scm"), RUST_INDENTS).unwrap();
    true
}

/// The file every test opens: a function body with one blank line inside it, where
/// the cursor lands to type or paste.
const SAMPLE: &str = "\
fn main() {
    let a = 1;
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
        feed(&rpc, ":set expandtab<CR>");
        (rpc, incoming, guard, TempKeep(data))
    }};
}

/// The control: the grammar's indent query really is driving the indent, so the
/// paste assertions below are not passing vacuously (e.g. because the grammar
/// failed to load and every verdict came back "none"). Opening a line under
/// `fn main() {` must indent it one level *into* the block — the grammar-free
/// fallbacks would copy the opener's own column (0) instead, so a `4` here is the
/// `indents.scm` verdict and nothing else.
#[tokio::test]
async fn a_typed_open_line_is_treesitter_indented() {
    let (rpc, _incoming, _guard, _keep) = open_sample!("ts_paste_control");
    feed(&rpc, "ob();<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {", "    b();", "    let a = 1;", "}"],
    );
}

#[tokio::test]
async fn pasted_code_keeps_its_own_treesitter_indentation() {
    let (rpc, _incoming, _guard, _keep) = open_sample!("ts_paste_indent");
    // Open a line inside the body (that `o` auto-indents to column 4, before the
    // paste starts — as it should) and paste an already-indented nested snippet.
    // Every line of the payload must land at the column it carried: the pasted
    // `<CR>`s take no treesitter verdict at all.
    feed(&rpc, "o");
    feed(
        &rpc,
        &bemtvi_view::encode_paste("if a > 0 {\n    a += 1;\n}"),
    );
    feed(&rpc, "<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec![
            "fn main() {",
            "    if a > 0 {",
            "    a += 1;",
            "}",
            "    let a = 1;",
            "}",
        ],
    );
}
