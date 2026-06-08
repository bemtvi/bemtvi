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

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, serial_lock as test_lock, start_attached};
use tokio::sync::mpsc::UnboundedReceiver;

const COLS: u16 = 80;
const ROWS: u16 = 24;

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
    start_attached(
        ServerInit {
            file: None,
            ..Default::default()
        },
        COLS,
        ROWS - 2,
    )
    .await
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

// ----- high-level vim.treesitter (vendored Lua over the primitives) ---------

/// `vim.treesitter.get_string_parser(...):parse()` returns a tree whose root and
/// children match — the vendored `LanguageTree` running on our primitives.
#[tokio::test]
async fn string_parser_parses_and_walks() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let (rpc, _rx) = start().await;

    let v = exec_lua(
        &rpc,
        r#"
        local p = vim.treesitter.get_string_parser('fn main() {}', 'rust')
        local root = p:parse()[1]:root()
        return root:type() .. '|' .. root:named_child(0):type()
        "#,
    )
    .await;

    assert_eq!(v.as_str(), Some("source_file|function_item"));
}

/// `query.parse` + `iter_captures` with an `#eq?` predicate — the full query
/// pipeline (the ffi cursor + match + the vendored `query.lua` predicate eval).
/// Only the `foo` function's name satisfies `(#eq? @name "foo")`.
#[tokio::test]
async fn query_iter_captures_eq_predicate() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let (rpc, _rx) = start().await;

    let v = exec_lua(
        &rpc,
        r#"
        local src = 'fn foo() {}\nfn bar() {}\n'
        local q = vim.treesitter.query.parse('rust',
          '((function_item name: (identifier) @name) (#eq? @name "foo"))')
        local root = vim.treesitter.get_string_parser(src, 'rust'):parse()[1]:root()
        local names = {}
        for id, node in q:iter_captures(root, src) do
          names[#names + 1] = vim.treesitter.get_node_text(node, src)
        end
        return table.concat(names, ',')
        "#,
    )
    .await;

    assert_eq!(v.as_str(), Some("foo"));
}

/// `iter_matches` yields one match per `function_item`, each carrying its `@name`
/// capture — exercises `match:captures()`/`match:info()` and `get_node_text`.
#[tokio::test]
async fn query_iter_matches_lists_all() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let (rpc, _rx) = start().await;

    let v = exec_lua(
        &rpc,
        r#"
        local src = 'fn foo() {}\nfn bar() {}\n'
        local q = vim.treesitter.query.parse('rust',
          '(function_item name: (identifier) @name)')
        local root = vim.treesitter.get_string_parser(src, 'rust'):parse()[1]:root()
        local names = {}
        for _, match in q:iter_matches(root, src) do
          for id, nodes in pairs(match) do
            for _, node in ipairs(nodes) do
              names[#names + 1] = vim.treesitter.get_node_text(node, src)
            end
          end
        end
        table.sort(names)
        return table.concat(names, ',')
        "#,
    )
    .await;

    assert_eq!(v.as_str(), Some("bar,foo"));
}

/// A `#match?` predicate filters by vim-regex — the path where unfiltered cursor
/// iteration matters (predicates are evaluated in `query.lua`/`vim.regex`, not by
/// tree-sitter). Only names starting with `f` survive `(#match? @name "^f")`.
#[tokio::test]
async fn query_match_predicate_uses_vim_regex() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let (rpc, _rx) = start().await;

    let v = exec_lua(
        &rpc,
        r#"
        local src = 'fn foo() {}\nfn bar() {}\nfn fizz() {}\n'
        local q = vim.treesitter.query.parse('rust',
          '((function_item name: (identifier) @name) (#match? @name "^f"))')
        local root = vim.treesitter.get_string_parser(src, 'rust'):parse()[1]:root()
        local names = {}
        for id, node in q:iter_captures(root, src) do
          names[#names + 1] = vim.treesitter.get_node_text(node, src)
        end
        table.sort(names)
        return table.concat(names, ',')
        "#,
    )
    .await;

    assert_eq!(v.as_str(), Some("fizz,foo"));
}

/// `get_parser(buf):parse()` over a real buffer reads the pushed snapshot, and a
/// re-parse after an edit reflects the change (the snapshot-refresh adapter).
#[tokio::test]
async fn buffer_parser_reflects_edits() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let (rpc, _rx) = start().await;

    let v = exec_lua(
        &rpc,
        r#"
        vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'fn a() {}' })
        local p = vim.treesitter.get_parser(0, 'rust')
        local c1 = p:parse()[1]:root():named_child_count()
        vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'fn a() {}', 'fn b() {}' })
        local c2 = p:parse()[1]:root():named_child_count()
        return c1 .. ',' .. c2
        "#,
    )
    .await;

    assert_eq!(v.as_str(), Some("1,2"));
}

/// A real consumer end to end: a user-Lua routine that selects every function
/// name in a buffer via a query — the platform's acceptance test.
#[tokio::test]
async fn real_consumer_collects_function_names() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let (rpc, _rx) = start().await;

    let v = exec_lua(
        &rpc,
        r#"
        vim.api.nvim_buf_set_lines(0, 0, -1, false, {
          'fn alpha() {}',
          'struct S;',
          'fn beta() { let x = 1; }',
        })
        local parser = vim.treesitter.get_parser(0, 'rust')
        local root = parser:parse()[1]:root()
        local q = vim.treesitter.query.parse('rust',
          '(function_item name: (identifier) @fn)')
        local out = {}
        for id, node in q:iter_captures(root, 0) do
          out[#out + 1] = vim.treesitter.get_node_text(node, 0)
        end
        table.sort(out)
        return table.concat(out, ',')
        "#,
    )
    .await;

    assert_eq!(v.as_str(), Some("alpha,beta"));
}

/// `vim.treesitter.get_node` resolves the smallest named node at a position —
/// the path through `named_node_for_range` / `tree_for_range`, which calls
/// `vim.nonnil` (a stdlib helper that must exist, else the call fails loud).
#[tokio::test]
async fn get_node_resolves_node_at_position() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let (rpc, _rx) = start().await;

    let v = exec_lua(
        &rpc,
        r#"
        vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'fn main() {}' })
        -- Parse first so the tree is available, then resolve the node at (0, 3),
        -- which sits on `main` (the function's name identifier).
        vim.treesitter.get_parser(0, 'rust'):parse()
        local node = vim.treesitter.get_node({ bufnr = 0, lang = 'rust', pos = { 0, 3 } })
        return node and node:type() or 'nil'
        "#,
    )
    .await;

    assert_eq!(v.as_str(), Some("identifier"));
}

/// `get_node` with no `pos` resolves the node under the **window cursor** — the
/// `:TSNodeAt`-style pattern. It only works against a *parsed* tree, and since
/// neovim's parser cache is weak-valued (and nxvim has no background highlighter
/// holding it), the consumer must parse first *and* keep a strong ref to the
/// parser across the `get_node` call — exactly what the example does.
#[tokio::test]
async fn get_node_at_cursor_after_parse() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let (rpc, _rx) = start().await;

    let v = exec_lua(
        &rpc,
        r#"
        vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'fn main() {}' })
        -- Strong `parser` ref keeps the weak-cached parser alive across get_node,
        -- so get_node's internal get_parser returns this freshly-parsed instance.
        local parser = vim.treesitter.get_parser(0, 'rust')
        parser:parse()
        -- Cursor is at (1, 0) (1-based row); get_node reads it via nvim_win_get_cursor.
        local node = vim.treesitter.get_node({ bufnr = 0, lang = 'rust' })
        return node and node:type() or 'nil'
        "#,
    )
    .await;

    assert_eq!(v.as_str(), Some("function_item"));
}

/// `vim.treesitter.language.inspect` surfaces the grammar's symbols/fields/ABI
/// via the `_ts_inspect_language` primitive.
#[tokio::test]
async fn language_inspect_reports_symbols() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let (rpc, _rx) = start().await;

    let v = exec_lua(
        &rpc,
        r#"
        local info = vim.treesitter.language.inspect('rust')
        return tostring(info.symbols['function_item'] == true)
          .. ',' .. tostring(info.abi_version >= 13)
        "#,
    )
    .await;

    assert_eq!(v.as_str(), Some("true,true"));
}

// ----- injected child trees on the platform (injections Phase 4) ------------
// The vendored `LanguageTree` runs `_get_injections` over nxvim's snapshot
// primitives — `parse(true)` builds injected child trees, `children()` lists them,
// and `language_for_range` / `get_node(…, ignore_injections=false)` resolve the
// *injected* language at a position. Self-injection (rust-in-rust) exercises the
// whole path with the one fixture grammar.

/// `parse(true)` builds an injected child `LanguageTree`, and `language_for_range`
/// inside the injected region resolves to the child language.
#[tokio::test]
async fn parser_builds_injected_children_and_resolves_language_for_range() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let (rpc, _rx) = start().await;

    let v = exec_lua(
        &rpc,
        r#"
        vim.treesitter.query.set('rust', 'injections',
          '((string_content) @injection.content (#set! injection.language "rust"))')
        vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'const S: &str = "fn z() {}";' })
        local p = vim.treesitter.get_parser(0, 'rust')
        p:parse(true) -- include injected children
        local kids = {}
        for lang in pairs(p:children()) do kids[#kids + 1] = lang end
        -- column 17 sits in the string body (the injected rust region).
        local child = p:language_for_range({ 0, 17, 0, 17 })
        return #kids .. '|' .. table.concat(kids, ',') .. '|' .. (child and child:lang() or 'nil')
        "#,
    )
    .await;

    assert_eq!(v.as_str(), Some("1|rust|rust"));
}

/// `get_node(…, ignore_injections = false)` descends into the injected child tree
/// and returns the *injected* grammar's node, where the default (host) resolution
/// stops at the host's flat node.
#[tokio::test]
async fn get_node_descends_into_the_injected_tree() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let (rpc, _rx) = start().await;

    let v = exec_lua(
        &rpc,
        r#"
        vim.treesitter.query.set('rust', 'injections',
          '((string_content) @injection.content (#set! injection.language "rust"))')
        vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'const S: &str = "fn z() {}";' })
        local p = vim.treesitter.get_parser(0, 'rust')
        p:parse(true)
        -- (0,17) is the `f` of the injected `fn`. The host paints the whole body as
        -- one `string_content`; descending into the injection yields the rust item.
        local host = vim.treesitter.get_node({ bufnr = 0, lang = 'rust', pos = { 0, 17 } })
        local inj = vim.treesitter.get_node(
          { bufnr = 0, lang = 'rust', pos = { 0, 17 }, ignore_injections = false })
        return (host and host:type() or 'nil') .. '|' .. (inj and inj:type() or 'nil')
        "#,
    )
    .await;

    assert_eq!(v.as_str(), Some("string_content|function_item"));
}
