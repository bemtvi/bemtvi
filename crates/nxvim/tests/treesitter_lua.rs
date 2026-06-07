//! The `vim.treesitter` Lua platform, end to end through the real stack. The
//! server installs the low-level primitives (`vim._create_ts_parser` & co.) onto
//! its Lua VM, backed by the same in-process grammar loader the highlight engine
//! uses. A `:lua`/`nvim_exec_lua` chunk drives those primitives and writes a
//! value back, which the test asserts on — the black-box rule (no unit tests).
//!
//! Unlike the highlight/indent fixtures, this one installs **only** the parser
//! `.so` — no `highlights.scm`/`indents.scm`. The platform creates parsers and
//! compiles queries from the grammar's `Language` alone, so a parser-only
//! install is enough; proving that is part of the point.
//!
//! These tests share process-global env (`NXVIM_DATA_DIR`), so they serialize on
//! a single lock.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::Mutex;

const COLS: u16 = 80;
const ROWS: u16 = 24;

/// Serializes the tests (shared `NXVIM_DATA_DIR` env + worker lifecycle).
fn test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

// ----- fixture grammar ------------------------------------------------------

/// Build (once) an `NXVIM_DATA_DIR` containing only a compiled Rust parser —
/// `parser/rust.so`, no query files — and point the env at it. Mirrors a
/// parser-only grammar install, hermetically.
fn fixture_data_dir() -> &'static Path {
    static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
    DATA_DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join("nxvim-ts-lua-fixture");
        let parser_dir = dir.join("parser");
        std::fs::create_dir_all(&parser_dir).unwrap();

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

        std::env::set_var("NXVIM_DATA_DIR", &dir);
        dir
    })
}

/// Locate the unpacked `tree-sitter-rust` crate source in the cargo registry (a
/// dev-dependency, so cargo guarantees it is present).
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

/// Start a server (no file open) and attach a UI. The fixture env must already
/// be set so the VM's treesitter primitives resolve grammars from it.
async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (server_end, client_end) = tokio::io::duplex(1 << 16);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("server runtime");
        let _ = runtime.block_on(run_server(
            server_end,
            ServerInit {
                file: None,
                ..Default::default()
            },
        ));
    });
    let (reader, writer) = tokio::io::split(client_end);
    let (rpc, incoming) = connect(reader, writer);
    rpc.request(
        "nvim_ui_attach",
        vec![
            Value::from(COLS as u64),
            Value::from((ROWS - 2) as u64),
            Value::Map(vec![]),
        ],
    )
    .await
    .expect("ui attach");
    (rpc, incoming)
}

/// Evaluate a Lua chunk on the server and return its value (`nvim_exec_lua`).
async fn exec_lua(rpc: &Rpc, code: &str) -> Value {
    rpc.request(
        "nvim_exec_lua",
        vec![Value::from(code), Value::Array(vec![])],
    )
    .await
    .expect("nvim_exec_lua")
}

// ----- tests ----------------------------------------------------------------

/// Parse a string, walk root → named child, and read its type and range — the
/// core primitive surface.
#[tokio::test]
async fn parse_and_walk_nodes() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let (rpc, _rx) = start().await;

    let v = exec_lua(
        &rpc,
        r#"
        local p = vim._create_ts_parser('rust')
        local tree = p:parse(nil, 'fn main() {}\n')
        local root = tree:root()
        local fi = root:named_child(0)
        local sr, sc, er, ec = fi:range()
        return root:type() .. '|' .. fi:type() .. '|' .. sr .. ',' .. sc .. ',' .. er .. ',' .. ec
        "#,
    )
    .await;

    assert_eq!(v.as_str(), Some("source_file|function_item|0,0,0,12"));
}

/// A node held across a reparse — with its source tree dropped and garbage
/// collected — stays valid. This is the lifetime model's load-bearing test: the
/// node co-owns its tree via the `Rc`, so the parser moving on can't invalidate
/// it.
#[tokio::test]
async fn node_survives_reparse_and_gc() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let (rpc, _rx) = start().await;

    let v = exec_lua(
        &rpc,
        r#"
        local p = vim._create_ts_parser('rust')
        local t1 = p:parse(nil, 'fn a() {}\n')
        local root1 = t1:root()
        -- Drop the tree handle and force a GC: only `root1`'s Rc keeps it alive.
        t1 = nil
        collectgarbage('collect')
        -- Reparse a different source so the parser advances.
        local _ = p:parse(nil, 'fn bbbbbbbb() {}\n')
        collectgarbage('collect')
        -- root1 must still resolve against its original (unchanged) tree.
        return root1:named_child(0):type() .. '/' .. tostring(root1:child_count())
        "#,
    )
    .await;

    assert_eq!(v.as_str(), Some("function_item/1"));
}

/// Incremental reparse: edit the old tree to mark the change, pass it back to
/// `:parse`, and the new tree reflects the edited source. Exercises both
/// `tree:edit` (clone-and-edit, returning a new tree) and incremental reuse.
#[tokio::test]
async fn incremental_reparse_reflects_edit() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let (rpc, _rx) = start().await;

    let v = exec_lua(
        &rpc,
        r#"
        local p = vim._create_ts_parser('rust')
        -- 'fn a() {}\n' is 10 bytes; append 'fn b() {}\n' to reach 20.
        local t1 = p:parse(nil, 'fn a() {}\n')
        local t1e = t1:edit(10, 10, 20, 1, 0, 1, 0, 2, 0)
        local t2 = p:parse(t1e, 'fn a() {}\nfn b() {}\n')
        return t2:root():named_child_count()
        "#,
    )
    .await;

    assert_eq!(v.as_i64(), Some(2));
}

/// `iter_children` yields every child of a node.
#[tokio::test]
async fn iter_children_yields_all() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let (rpc, _rx) = start().await;

    let v = exec_lua(
        &rpc,
        r#"
        local tree = vim._create_ts_parser('rust'):parse(nil, 'fn a() {}\nfn b() {}\n')
        local n = 0
        for _child in tree:root():iter_children() do n = n + 1 end
        return n
        "#,
    )
    .await;

    assert_eq!(v.as_i64(), Some(2));
}

/// `vim._ts_parse_query` compiles a valid query and fails loud on a malformed
/// one (no silent `nil`).
#[tokio::test]
async fn parse_query_compiles_and_errors() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let (rpc, _rx) = start().await;

    let v = exec_lua(
        &rpc,
        r#"
        local q = vim._ts_parse_query('rust', '(function_item) @func')
        local bad_ok = pcall(vim._ts_parse_query, 'rust', '(((')
        return tostring(q ~= nil) .. ',' .. tostring(bad_ok)
        "#,
    )
    .await;

    assert_eq!(v.as_str(), Some("true,false"));
}

/// `vim._ts_has_language` reports installed vs missing grammars.
#[tokio::test]
async fn has_language_reports_installed() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let (rpc, _rx) = start().await;

    let installed = exec_lua(&rpc, "return vim._ts_has_language('rust')").await;
    let missing = exec_lua(&rpc, "return vim._ts_has_language('nonesuch')").await;

    assert_eq!(installed.as_bool(), Some(true));
    assert_eq!(missing.as_bool(), Some(false));
}

/// Asking for a parser that isn't installed is a loud error, not a silent `nil`.
#[tokio::test]
async fn create_parser_missing_grammar_errors() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let (rpc, _rx) = start().await;

    let v = exec_lua(
        &rpc,
        r#"
        local ok, err = pcall(vim._create_ts_parser, 'nonesuch')
        if ok then return 'unexpectedly-ok' end
        return tostring(err):find('nonesuch') and 'named' or 'unnamed'
        "#,
    )
    .await;

    // pcall failed (loud error) and the message names the missing language.
    assert_eq!(v.as_str(), Some("named"));
}
