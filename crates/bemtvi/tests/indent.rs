//! Treesitter indentation, end to end through the real stack. The server owns an
//! in-process treesitter engine that loads a Rust grammar **and** an `indents.scm`
//! we compile into a temp `BEMTVI_DATA_DIR` fixture (vendored from
//! nvim-treesitter). Indentation is synchronous — pressing `o`/`<CR>`/`==`/`gg=G`
//! lands the right columns in the same frame — so these are plain barrier tests:
//! feed keys, then read `nvim_buf_get_lines` / `nvim_win_get_cursor`.
//!
//! These tests share process-global env (`BEMTVI_DATA_DIR`), so they serialize on
//! a single lock and build the grammar fixture once.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    cursor, feed, lines, serial_lock as test_lock, start_attached, write_temp,
};
use tokio::sync::mpsc::UnboundedReceiver;

// ----- fixture grammar ------------------------------------------------------

/// The indent query the fixture installs — nvim-treesitter's Rust `indents.scm`,
/// reused verbatim as pure data (we never run its Lua). It exercises the core
/// captures this phase ports: `@indent.begin` / `.end` / `.dedent` / `.branch`
/// / `.ignore` / `.auto`, plus the `indent.immediate` / `indent.start_at_same_line`
/// `#set!` directives.
const RUST_INDENTS: &str = r#"
[
  (mod_item)
  (struct_item)
  (enum_item)
  (impl_item)
  (struct_expression)
  (struct_pattern)
  (tuple_struct_pattern)
  (tuple_expression)
  (tuple_type)
  (tuple_pattern)
  (match_block)
  (call_expression)
  (assignment_expression)
  (arguments)
  (block)
  (where_clause)
  (use_list)
  (array_expression)
  (ordered_field_declaration_list)
  (field_declaration_list)
  (enum_variant_list)
  (parameters)
  (token_tree)
  (token_repetition)
  (macro_definition)
] @indent.begin

(macro_definition
  [
    ")"
    "}"
    "]"
  ] @indent.end)

(trait_item
  body: (_) @indent.begin)

(string_literal
  (escape_sequence)) @indent.begin

(block
  "}" @indent.end)

(enum_item
  body: (enum_variant_list
    "}" @indent.end))

(impl_item
  body: (declaration_list
    "}" @indent.end))

(match_expression
  body: (match_block
    "}" @indent.end))

(mod_item
  body: (declaration_list
    "}" @indent.end))

(struct_item
  body: (field_declaration_list
    "}" @indent.end))

(struct_expression
  body: (field_initializer_list
    "}" @indent.end))

(struct_pattern
  "}" @indent.end)

(tuple_struct_pattern
  ")" @indent.end)

(tuple_type
  ")" @indent.end)

(tuple_pattern
  ")" @indent.end)

(trait_item
  body: (declaration_list
    "}" @indent.end))

(impl_item
  (where_clause) @indent.dedent)

[
  "where"
  ")"
  "]"
  "}"
] @indent.branch

(impl_item
  (declaration_list) @indent.branch)

[
  (line_comment)
  (string_literal)
] @indent.ignore

(raw_string_literal) @indent.auto
"#;

/// Build (once) a `BEMTVI_DATA_DIR` containing a compiled Rust grammar, its
/// highlights query, and `indents.scm`; point the engine at it. Mirrors how a
/// user installs a parser + queries, but hermetic and in a dir distinct from the
/// highlighting fixture so the two test binaries never race on disk.
fn fixture_data_dir() -> &'static Path {
    static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
    DATA_DIR.get_or_init(|| {
        let dir = bemtvi_test_harness::temp_root().join("bemtvi-ts-indent-fixture");
        let parser_dir = dir.join("parser");
        let query_dir = dir.join("queries").join("rust");
        std::fs::create_dir_all(&parser_dir).unwrap();
        std::fs::create_dir_all(&query_dir).unwrap();

        // Compile the grammar's C sources into parser/rust.so (named `.so` on
        // every OS, which our loader tries first), via the system C compiler.
        let src = grammar_src_dir().join("src");
        let out = parser_dir.join("rust.so");
        let compiler = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
        let status = std::process::Command::new(compiler)
            .args(["-shared", "-fPIC", "-O1"])
            .arg("-I")
            .arg(&src)
            .arg(src.join("parser.c"))
            .arg(src.join("scanner.c"))
            .arg("-o")
            .arg(&out)
            .status()
            .expect("run C compiler");
        assert!(status.success(), "compiling rust grammar fixture failed");

        std::fs::write(
            query_dir.join("highlights.scm"),
            tree_sitter_rust::HIGHLIGHTS_QUERY,
        )
        .unwrap();
        std::fs::write(query_dir.join("indents.scm"), RUST_INDENTS).unwrap();

        std::env::set_var("BEMTVI_DATA_DIR", &dir);
        dir
    })
}

/// Locate the unpacked `tree-sitter-rust` crate source in the cargo registry
/// (a dev-dependency, so cargo guarantees it is present).
fn grammar_src_dir() -> PathBuf {
    let cargo_home = std::env::var("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".cargo"));
    let registry = cargo_home.join("registry").join("src");
    for index in std::fs::read_dir(&registry).expect("read cargo registry src") {
        let candidate = index.unwrap().path().join("tree-sitter-rust-0.24.2");
        if candidate.is_dir() {
            return candidate;
        }
    }
    panic!("tree-sitter-rust-0.24.2 source not found under {registry:?}");
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .expect("HOME")
}

// ----- server harness -------------------------------------------------------

async fn start(file: Option<String>) -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(
        ServerInit {
            file,
            ..Default::default()
        },
        80,
        24,
    )
    .await
}

// ----- tests ----------------------------------------------------------------

#[tokio::test]
async fn o_opens_an_indented_line_inside_a_block() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = write_temp("o_block", "rs", "fn main() {\n}\n");
    let (rpc, _incoming) = start(Some(file)).await;

    // expandtab → indentation is spaces; one level is the (default) 4-cell tabstop.
    feed(&rpc, ":set expandtab<CR>");
    // Open a line inside the function body and type a statement.
    feed(&rpc, "ggolet x = 1;<Esc>");

    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {", "    let x = 1;", "}"]
    );
}

#[tokio::test]
async fn enter_in_insert_carries_treesitter_indent() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = write_temp("enter", "rs", "fn main() {\n}\n");
    let (rpc, _incoming) = start(Some(file)).await;

    feed(&rpc, ":set expandtab<CR>");
    // Append at the end of `fn main() {`, then a real <CR> splits to a new,
    // indented line where we type the statement.
    feed(&rpc, "ggA<CR>let x = 1;<Esc>");

    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {", "    let x = 1;", "}"]
    );
}

#[tokio::test]
async fn o_nests_one_level_deeper_inside_an_inner_block() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    // Cursor starts on the inner `if` opener; `o` must land two levels deep (8).
    let file = write_temp("nested", "rs", "fn main() {\n    if a {\n    }\n}\n");
    let (rpc, _incoming) = start(Some(file)).await;

    feed(&rpc, ":set expandtab<CR>");
    feed(&rpc, "2ggoy();<Esc>");

    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {", "    if a {", "        y();", "    }", "}"]
    );
}

#[tokio::test]
async fn o_on_a_closing_brace_line_dedents() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = write_temp("dedent", "rs", "fn main() {\n    let x = 1;\n}\n");
    let (rpc, _incoming) = start(Some(file)).await;

    feed(&rpc, ":set expandtab<CR>");
    // Open below the closing brace: the new line is outside the block → column 0.
    feed(&rpc, "Gofn other() {}<Esc>");

    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {", "    let x = 1;", "}", "fn other() {}"]
    );
}

#[tokio::test]
async fn double_equal_reindents_the_current_line() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    // A statement jammed to column 0 inside the body.
    let file = write_temp("eqeq", "rs", "fn main() {\nlet x = 1;\n}\n");
    let (rpc, _incoming) = start(Some(file)).await;

    feed(&rpc, ":set expandtab<CR>");
    feed(&rpc, "2gg==");

    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {", "    let x = 1;", "}"]
    );
    // `=` settles on the first non-blank of the reindented line.
    assert_eq!(cursor(&rpc).await, (2, 4));
}

#[tokio::test]
async fn gg_equal_g_reindents_the_whole_buffer() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = write_temp(
        "ggeqg",
        "rs",
        "fn main() {\nlet x = 1;\n        let y = 2;\n}\n",
    );
    let (rpc, _incoming) = start(Some(file)).await;

    feed(&rpc, ":set expandtab<CR>");
    feed(&rpc, "gg=G");

    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {", "    let x = 1;", "    let y = 2;", "}"]
    );
}

#[tokio::test]
async fn equal_uses_tabs_when_noexpandtab() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = write_temp("tabs", "rs", "fn main() {\nlet x = 1;\n}\n");
    let (rpc, _incoming) = start(Some(file)).await;

    // Default is noexpandtab: one indent level is a literal tab (tabstop 4).
    feed(&rpc, "2gg==");

    assert_eq!(lines(&rpc).await, vec!["fn main() {", "\tlet x = 1;", "}"]);
}

#[tokio::test]
async fn o_on_a_plain_buffer_stays_at_column_zero() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    // A non-source buffer has no grammar → no ts-indent. The copy-previous-line
    // fallback is gated on ts-indent being *available*, so `o` keeps vim's
    // autoindent-off default of column 0 even below an indented line.
    let file = write_temp("plain", "txt", "    indented line\n");
    let (rpc, _incoming) = start(Some(file)).await;

    feed(&rpc, ":set expandtab<CR>");
    feed(&rpc, "ggofresh<Esc>");

    assert_eq!(lines(&rpc).await, vec!["    indented line", "fresh"]);
}
