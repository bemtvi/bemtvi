//! `:TSInstall <lang>` end to end, through the real server, **offline**.
//!
//! The installer (`nxvim_ts::install`) fetches nvim-treesitter's `parsers.lua`,
//! the grammar source tarball, and the queries over HTTPS. For a hermetic test we
//! set `$NXVIM_TS_MIRROR`, which redirects every GET to a local directory tree
//! (`<mirror>/<host>/<path>`), and `$NXVIM_CC=cc` to pin the compiler to the
//! system one (no Zig download). The mirror's grammar tarball is built from the
//! `tree-sitter-rust` crate source cargo already unpacked for us — so the test
//! does a *real* gunzip → untar → C compile → `dlopen`, then drives the editor to
//! prove indentation works against the freshly-installed parser.
//!
//! These tests share process-global env (`$NXVIM_DATA_DIR`, `$NXVIM_TS_*`), so
//! they serialize on the shared lock and use a dir distinct from the other ts
//! fixtures.

use std::path::{Path, PathBuf};
use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    cursor, drain_to_latest_redraw, exec_lua, feed, lines, message, mode, serial_lock as test_lock,
    start_attached, window0_field, write_temp,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// The fake nvim-treesitter ref + grammar revision the fixture pins. Arbitrary —
/// they only have to agree between the env, the fixture `parsers.lua`, and the
/// tarball's top-level directory name.
const REF: &str = "testref";
const REV: &str = "deadbeef0000";

/// A minimal `parsers.lua` in nvim-treesitter's exact shape, naming only `rust`
/// at `revision`.
fn parsers_lua(revision: &str) -> String {
    format!(
        "return {{
  rust = {{
    install_info = {{
      revision = '{revision}',
      url = 'https://github.com/tree-sitter/tree-sitter-rust',
    }},
    maintainers = {{ '@amaanq' }},
    tier = 2,
  }},
}}
"
    )
}

/// nvim-treesitter's Rust `indents.scm`, trimmed to the captures this test needs
/// (block open indents, closing brace dedents) — enough to drive `o` / `<CR>`.
const RUST_INDENTS: &str = "\
[
  (block)
  (match_block)
  (arguments)
  (declaration_list)
  (field_declaration_list)
] @indent.begin

(block \"}\" @indent.end)
";

// ----- fixture --------------------------------------------------------------

/// Build a fresh `$NXVIM_DATA_DIR` + `$NXVIM_TS_MIRROR` for one test and export
/// the env the installer reads. Returns the (data_dir, mirror) roots. The grammar
/// is pinned at the default sha-style revision whose archive dir is the obvious
/// `tree-sitter-rust-<rev>`.
fn fixture(tag: &str) -> (PathBuf, PathBuf) {
    fixture_rev(tag, REV, &format!("tree-sitter-rust-{REV}"))
}

/// Like [`fixture`] but with an explicit grammar `revision` and the archive's
/// top-level directory name — which GitHub does *not* always make
/// `tree-sitter-rust-<revision>` (a `vX.Y.Z` tag's dir drops the `v`).
fn fixture_rev(tag: &str, revision: &str, archive_top_dir: &str) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("nxvim-tsinstall-{tag}"));
    let _ = std::fs::remove_dir_all(&base);
    let data_dir = base.join("data");
    let mirror = base.join("mirror");
    std::fs::create_dir_all(&data_dir).unwrap();

    // raw.githubusercontent.com/<repo>/<ref>/lua/nvim-treesitter/parsers.lua
    let nt = mirror
        .join("raw.githubusercontent.com/nvim-treesitter/nvim-treesitter")
        .join(REF);
    write_under(
        &nt.join("lua/nvim-treesitter/parsers.lua"),
        parsers_lua(revision).as_bytes(),
    );
    // runtime/queries/rust/{indents,highlights}.scm
    let q = nt.join("runtime/queries/rust");
    write_under(&q.join("indents.scm"), RUST_INDENTS.as_bytes());
    write_under(
        &q.join("highlights.scm"),
        tree_sitter_rust::HIGHLIGHTS_QUERY.as_bytes(),
    );
    // github.com/tree-sitter/tree-sitter-rust/archive/<rev>.tar.gz
    let tarball = mirror
        .join("github.com/tree-sitter/tree-sitter-rust/archive")
        .join(format!("{revision}.tar.gz"));
    build_source_tarball(&tarball, archive_top_dir);

    std::env::set_var("NXVIM_DATA_DIR", &data_dir);
    std::env::set_var("NXVIM_TS_MIRROR", &mirror);
    std::env::set_var("NXVIM_TS_REF", REF);
    std::env::set_var(
        "NXVIM_CC",
        std::env::var("CC").unwrap_or_else(|_| "cc".into()),
    );
    (data_dir, mirror)
}

fn write_under(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// Pack the cargo-unpacked `tree-sitter-rust` `src/` into a `.tar.gz` whose top
/// dir is `top_dir/` — exactly what GitHub's archive endpoint serves, so the
/// installer's untar + `src/parser.c` discovery hit the same layout as
/// production. `top_dir` is a parameter because GitHub mangles it (a `vX.Y.Z` tag
/// becomes `…-X.Y.Z`), which the installer must not assume. Uses the system `tar`
/// (the harness already shells out to `cc`), staging via `cp -R`.
fn build_source_tarball(out: &Path, top_dir: &str) {
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let stage = out.parent().unwrap().join(format!(
        "stage-{}",
        out.file_name().unwrap().to_string_lossy()
    ));
    let _ = std::fs::remove_dir_all(&stage);
    let pkg = stage.join(top_dir);
    std::fs::create_dir_all(&pkg).unwrap();
    let status = std::process::Command::new("cp")
        .arg("-R")
        .arg(grammar_src_dir().join("src"))
        .arg(pkg.join("src"))
        .status()
        .expect("cp -R grammar src");
    assert!(status.success(), "staging grammar src failed");
    let status = std::process::Command::new("tar")
        .arg("czf")
        .arg(out)
        .arg("-C")
        .arg(&stage)
        .arg(top_dir)
        .status()
        .expect("tar czf");
    assert!(status.success(), "building source tarball failed");
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

// ----- harness --------------------------------------------------------------

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

/// Like [`start`] but records the remote daemon's installed parser languages
/// (`ServerInit.ts_autoinstall`) — so opening a buffer of one of those filetypes
/// triggers a lazy compile, with no `:TSInstall`.
async fn start_with_remote_langs(
    file: Option<String>,
    langs: Vec<String>,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(
        ServerInit {
            file,
            ts_autoinstall: langs,
            ..Default::default()
        },
        80,
        24,
    )
    .await
}

/// Drive the server loop while waiting (up to ~30s) for a message-line frame
/// whose text contains `needle`. `:TSInstall` runs on a blocking worker and
/// reports back on a `select!` arm, so we poll: a `mode` round-trip cycles the
/// loop (letting the completion arm fire) and a sleep lets the off-thread compile
/// make progress. Returns the matching message, or panics on timeout.
async fn wait_for_message(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    needle: &str,
) -> String {
    for _ in 0..300 {
        let _ = mode(rpc).await; // barrier: cycle the server loop
        if let Some(map) = drain_to_latest_redraw(incoming, |m| message(m).contains(needle)) {
            return message(&map);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for message containing {needle:?}");
}

/// Total treesitter highlight spans across all rows in a redraw frame
/// (`windows[0].highlights` is one array per row, each a list of spans).
fn total_highlight_spans(map: &[(Value, Value)]) -> usize {
    window0_field(map, "highlights")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(Value::as_array).map(Vec::len).sum())
        .unwrap_or(0)
}

/// Drive the loop until a redraw frame carries at least one highlight span, or
/// time out (returning 0). Used to assert that a buffer open *before* its grammar
/// existed lights up the instant the install lands — with no edit to bump the
/// changedtick — i.e. that the install drops the server's highlight memo.
async fn wait_for_highlights(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> usize {
    for _ in 0..50 {
        let _ = mode(rpc).await; // barrier: cycle the server loop + repaint
        if let Some(map) = drain_to_latest_redraw(incoming, |m| total_highlight_spans(m) > 0) {
            return total_highlight_spans(&map);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    0
}

// ----- tests ----------------------------------------------------------------

#[tokio::test]
async fn ts_install_compiles_grammar_and_enables_indent() {
    let _guard = test_lock().lock().await;
    let (data_dir, _mirror) = fixture("indent");
    // Open a .rs buffer *before* the grammar exists: the engine caches a
    // "not installed" verdict, so the install must invalidate it (the reload) for
    // indentation to start working without a manual `:e`.
    let file = write_temp("tsinstall", "rs", "fn main() {\n}\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    // Sanity: with no grammar, `<CR>` inside the block carries no ts-indent.
    feed(&rpc, ":set expandtab<CR>");

    feed(&rpc, ":TSInstall rust<CR>");
    let msg = wait_for_message(&rpc, &mut incoming, "TSInstall: installed").await;
    assert!(msg.contains("rust"), "unexpected install message: {msg:?}");

    // The compiled parser landed in the data dir.
    assert!(
        data_dir.join("parser").join("rust.so").exists(),
        "parser/rust.so was not written"
    );

    // And indentation now works: a real <CR> after the `{` opens an indented line.
    feed(&rpc, "ggA<CR>let x = 1;<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {", "    let x = 1;", "}"]
    );
    let _ = cursor(&rpc).await;

    // `:TSInstallInfo` lists the parser we just installed (in a read-only scratch
    // buffer, now the focused window), with its queries.
    feed(&rpc, ":TSInstallInfo<CR>");
    let info = lines(&rpc).await;
    let joined = info.join("\n");
    assert!(
        joined.contains("rust") && joined.contains("indents"),
        "TSInstallInfo listing missing rust/indents: {info:?}"
    );
}

/// Lazy install of a remote parser: with the daemon's parser set recorded
/// (`ServerInit.ts_autoinstall`) but **no** `:TSInstall`, opening a buffer of that
/// filetype for the first time compiles its grammar in the background and lights the
/// buffer up — the FileType-settle hook drives the same offline compile path. A buffer
/// of a filetype the remote *didn't* have stays untouched.
#[tokio::test]
async fn lazy_install_compiles_a_remote_parser_on_first_open() {
    let _guard = test_lock().lock().await;
    let (data_dir, _mirror) = fixture("lazy");
    // The remote had `rust` installed; this client doesn't yet. No `:TSInstall` issued —
    // opening the .rs buffer is the only trigger.
    let file = write_temp("tslazy", "rs", "fn main() {\n    let x = 1;\n}\n");
    let (rpc, mut incoming) = start_with_remote_langs(Some(file), vec!["rust".to_string()]).await;

    // The lazy compile, kicked off when the .rs filetype settled at open, completes.
    let msg = wait_for_message(&rpc, &mut incoming, "TSInstall: installed").await;
    assert!(
        msg.contains("rust"),
        "unexpected completion message: {msg:?}"
    );
    assert!(
        data_dir.join("parser").join("rust.so").exists(),
        "the lazy compile did not write parser/rust.so"
    );
    // The already-open buffer lights up once the grammar lands (no edit needed).
    assert!(
        wait_for_highlights(&rpc, &mut incoming).await > 0,
        "the buffer never highlighted after the lazy install"
    );
}

#[tokio::test]
async fn ts_install_highlights_an_already_open_buffer_immediately() {
    let _guard = test_lock().lock().await;
    let (_data_dir, _mirror) = fixture("highlight");
    // Open a .rs buffer *before* the grammar exists. The server memoizes the
    // (empty) highlight spans keyed on (changedtick, viewport); neither changes on
    // install, so without dropping that memo the buffer stays blank until the next
    // edit/scroll/`:e`. Real neovim lights it up at once — so must we.
    let file = write_temp("tsinstall_hl", "rs", "fn main() {\n    let x = 1;\n}\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    feed(&rpc, ":TSInstall rust<CR>");
    let msg = wait_for_message(&rpc, &mut incoming, "TSInstall: installed").await;
    assert!(msg.contains("rust"), "unexpected install message: {msg:?}");

    // No edit, no scroll: highlighting must appear from the install alone.
    let spans = wait_for_highlights(&rpc, &mut incoming).await;
    assert!(
        spans > 0,
        "buffer open before :TSInstall got no highlights after the grammar landed \
         (server highlight memo not invalidated on grammar reload)"
    );
}

#[tokio::test]
async fn ts_install_handles_a_version_tag_revision() {
    let _guard = test_lock().lock().await;
    // Revision is a version tag `v9.9.9`; GitHub serves the archive with its top
    // dir as `tree-sitter-rust-9.9.9` (leading `v` stripped). The installer must
    // discover that dir, not assume `tree-sitter-rust-v9.9.9` — the python bug.
    let (data_dir, _m) = fixture_rev("vtag", "v9.9.9", "tree-sitter-rust-9.9.9");
    let (rpc, mut incoming) = start(None).await;

    feed(&rpc, ":TSInstall rust<CR>");
    let msg = wait_for_message(&rpc, &mut incoming, "installed rust").await;
    assert!(
        msg.contains("installed rust"),
        "tag install failed: {msg:?}"
    );
    assert!(data_dir.join("parser/rust.so").exists());
}

#[tokio::test]
async fn bare_ts_install_uses_current_buffer_language() {
    let _guard = test_lock().lock().await;
    let (data_dir, _mirror) = fixture("barebuf");
    // A `.rs` buffer resolves to the `rust` filetype, so a bare `:TSInstall` (no args)
    // installs *that* language — the convenience for "grab the grammar for the file I'm
    // editing" instead of having to retype its name.
    let file = write_temp("tsinstall_bare", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    feed(&rpc, ":TSInstall<CR>");
    let msg = wait_for_message(&rpc, &mut incoming, "TSInstall: installed").await;
    assert!(
        msg.contains("rust"),
        "bare :TSInstall didn't resolve the current buffer's language: {msg:?}"
    );
    assert!(
        data_dir.join("parser").join("rust.so").exists(),
        "bare :TSInstall did not compile the current buffer's grammar"
    );
}

#[tokio::test]
async fn bare_ts_install_without_a_filetype_shows_usage() {
    let _guard = test_lock().lock().await;
    let (_data_dir, _mirror) = fixture("bareempty");
    // An unnamed buffer has no extension → no filetype, so a bare `:TSInstall` has nothing
    // to resolve. It must surface the usage hint, not silently no-op.
    let (rpc, mut incoming) = start(None).await;

    feed(&rpc, ":TSInstall<CR>");
    let msg = wait_for_message(&rpc, &mut incoming, "open a file to install").await;
    assert!(
        msg.contains("usage"),
        "expected the usage hint, got: {msg:?}"
    );
}

/// The real thing: no mirror, hit GitHub + the pinned nvim-treesitter ref live,
/// and prove the production URLs + `parsers.lua` format actually resolve and
/// compile. `#[ignore]`d so the normal suite stays offline/deterministic; run it
/// deliberately with `--ignored` to validate the live path.
#[tokio::test]
#[ignore = "hits the network (GitHub + nvim-treesitter); run with --ignored"]
async fn ts_install_rust_over_the_real_network() {
    let _guard = test_lock().lock().await;
    let base = std::env::temp_dir().join("nxvim-tsinstall-live");
    let _ = std::fs::remove_dir_all(&base);
    let data_dir = base.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::env::set_var("NXVIM_DATA_DIR", &data_dir);
    std::env::remove_var("NXVIM_TS_MIRROR");
    std::env::remove_var("NXVIM_TS_REF");
    std::env::set_var(
        "NXVIM_CC",
        std::env::var("CC").unwrap_or_else(|_| "cc".into()),
    );

    let file = write_temp("tsinstall_live", "rs", "fn main() {\n}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    feed(&rpc, ":set expandtab<CR>");
    feed(&rpc, ":TSInstall rust<CR>");
    // "installed" is not a substring of the transient "installing…" echo.
    let msg = wait_for_message(&rpc, &mut incoming, "installed rust").await;
    assert!(
        msg.contains("installed rust"),
        "live install failed: {msg:?}"
    );
    assert!(data_dir.join("parser/rust.so").exists());

    feed(&rpc, "ggA<CR>let x = 1;<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {", "    let x = 1;", "}"]
    );
}

#[tokio::test]
async fn ts_install_fetches_inherited_query_sets() {
    let _guard = test_lock().lock().await;
    // A grammar whose `highlights.scm` opens with `; inherits: base` must drag the
    // inherited language's query files onto disk too — the native engine reads one
    // file per language and the query bridge merges the inherit chain at runtime, so
    // without this base highlighting of an inherits-based grammar (js → ecma) is
    // missing the parent's patterns. We reuse the rust grammar source but mirror a
    // `rust/highlights.scm` that inherits a synthetic `base`, and serve `base`'s query.
    let root = std::env::temp_dir().join("nxvim-tsinstall-inherits");
    let _ = std::fs::remove_dir_all(&root);
    let data_dir = root.join("data");
    let mirror = root.join("mirror");
    std::fs::create_dir_all(&data_dir).unwrap();
    let nt = mirror
        .join("raw.githubusercontent.com/nvim-treesitter/nvim-treesitter")
        .join(REF);
    write_under(
        &nt.join("lua/nvim-treesitter/parsers.lua"),
        parsers_lua(REV).as_bytes(),
    );
    write_under(
        &nt.join("runtime/queries/rust/highlights.scm"),
        b"; inherits: base\n(identifier) @variable\n",
    );
    write_under(
        &nt.join("runtime/queries/base/highlights.scm"),
        b"(line_comment) @comment\n",
    );
    build_source_tarball(
        &mirror
            .join("github.com/tree-sitter/tree-sitter-rust/archive")
            .join(format!("{REV}.tar.gz")),
        &format!("tree-sitter-rust-{REV}"),
    );
    std::env::set_var("NXVIM_DATA_DIR", &data_dir);
    std::env::set_var("NXVIM_TS_MIRROR", &mirror);
    std::env::set_var("NXVIM_TS_REF", REF);
    std::env::set_var(
        "NXVIM_CC",
        std::env::var("CC").unwrap_or_else(|_| "cc".into()),
    );

    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, ":TSInstall rust<CR>");
    let msg = wait_for_message(&rpc, &mut incoming, "TSInstall: installed").await;
    assert!(
        msg.contains("+inherited[base]"),
        "install should report the inherited fetch: {msg:?}"
    );
    assert!(
        data_dir.join("queries/base/highlights.scm").exists(),
        "inherited `base` query files were not fetched to disk"
    );
}

#[tokio::test]
async fn ts_install_unknown_language_fails_loud() {
    let _guard = test_lock().lock().await;
    let _ = fixture("unknown");
    let (rpc, mut incoming) = start(None).await;

    // The fixture parsers.lua only knows `rust`; installing another fails loud
    // with the language named — never a silent no-op.
    feed(&rpc, ":TSInstall nonesuch<CR>");
    let msg = wait_for_message(&rpc, &mut incoming, "TSInstall: nonesuch failed").await;
    assert!(
        msg.contains("no parser named 'nonesuch'"),
        "expected a named failure, got: {msg:?}"
    );
}

#[tokio::test]
async fn ts_install_defers_to_a_registered_user_command() {
    let _guard = test_lock().lock().await;
    let _ = fixture("defer");
    let (rpc, _incoming) = start(None).await;

    // Simulate an nvim-treesitter plugin owning :TSInstall. Because the native arm
    // is guarded on `!has_user_command`, the plugin's command must win (no silent
    // native shadow) — it writes a global we can read back.
    let _ = exec_lua(
        &rpc,
        "vim.g.ts_marker = nil\n\
         vim.api.nvim_create_user_command('TSInstall', function() vim.g.ts_marker = 'plugin' end, {})",
    )
    .await;
    feed(&rpc, ":TSInstall rust<CR>");

    let marker = exec_lua(&rpc, "return vim.g.ts_marker").await;
    assert_eq!(
        marker.as_str(),
        Some("plugin"),
        "native :TSInstall shadowed the registered user command"
    );
}

// ----- repo install (`:TSInstall owner/repo`) -------------------------------

/// Build a GitHub *repo* tarball fixture for `:TSInstall owner/repo`: the rust
/// grammar `src/` (a real, pre-generated `parser.c` to compile) plus the repo's
/// **own** `tree-sitter.json` (declaring name + file-types) and
/// `queries/highlights.scm`, packed with `top_dir/` on top — exactly what
/// `github.com/<owner>/<repo>/archive/<ref>.tar.gz` serves. Unlike the catalog
/// fixture there is no `parsers.lua`: the repo is the sole source of truth.
fn build_repo_tarball(out: &Path, top_dir: &str, tree_sitter_json: &str) {
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let stage = out.parent().unwrap().join(format!(
        "stage-{}",
        out.file_name().unwrap().to_string_lossy()
    ));
    let _ = std::fs::remove_dir_all(&stage);
    let pkg = stage.join(top_dir);
    std::fs::create_dir_all(&pkg).unwrap();
    let status = std::process::Command::new("cp")
        .arg("-R")
        .arg(grammar_src_dir().join("src"))
        .arg(pkg.join("src"))
        .status()
        .expect("cp -R grammar src");
    assert!(status.success(), "staging grammar src failed");
    write_under(&pkg.join("tree-sitter.json"), tree_sitter_json.as_bytes());
    write_under(
        &pkg.join("queries/highlights.scm"),
        tree_sitter_rust::HIGHLIGHTS_QUERY.as_bytes(),
    );
    let status = std::process::Command::new("tar")
        .arg("czf")
        .arg(out)
        .arg("-C")
        .arg(&stage)
        .arg(top_dir)
        .status()
        .expect("tar czf");
    assert!(status.success(), "building repo tarball failed");
}

/// Mirror a repo at `acme/tree-sitter-demo` at `git_ref` (the archive filename and
/// the tarball's top dir both key off the ref, as GitHub serves them) with the given
/// `tree-sitter.json`, and export the installer env. Returns `data_dir`.
fn repo_fixture(tag: &str, git_ref: &str, tree_sitter_json: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("nxvim-tsinstall-repo-{tag}"));
    let _ = std::fs::remove_dir_all(&base);
    let data_dir = base.join("data");
    let mirror = base.join("mirror");
    std::fs::create_dir_all(&data_dir).unwrap();
    build_repo_tarball(
        &mirror
            .join("github.com/acme/tree-sitter-demo/archive")
            .join(format!("{git_ref}.tar.gz")),
        &format!("tree-sitter-demo-{git_ref}"),
        tree_sitter_json,
    );
    std::env::set_var("NXVIM_DATA_DIR", &data_dir);
    std::env::set_var("NXVIM_TS_MIRROR", &mirror);
    std::env::set_var(
        "NXVIM_CC",
        std::env::var("CC").unwrap_or_else(|_| "cc".into()),
    );
    data_dir
}

#[tokio::test]
async fn ts_install_from_github_repo_reads_declared_file_types() {
    let _guard = test_lock().lock().await;
    // `owner/repo` (no `@ref` → default branch, `HEAD`). The grammar declares its
    // own name `demo` and `file-types: ["demo"]` in tree-sitter.json — nothing about
    // it is in nvim-treesitter's catalog. The reader honours that declaration.
    // The grammar declares `name: "rust"` (its parser.c exports `tree_sitter_rust`)
    // with a custom `file-types` — proving both that the declared file-types are read
    // and that the declared `name` wins over the repo dir name (`tree-sitter-demo`).
    let data_dir = repo_fixture(
        "declared",
        "HEAD",
        r#"{ "grammars": [ { "name": "rust", "scope": "source.rust", "file-types": ["rz", "rzz"] } ] }"#,
    );

    // Rust source opened as plain `.txt` (Phase 1 doesn't auto-detect yet — that's
    // Phase 2), set to the `rust` filetype *before* the grammar exists. The install
    // then reloads `rust` and drops the highlight memo, so the already-open buffer
    // lights up from the install alone — proof the parser + queries actually loaded.
    let file = write_temp("tsinstall_repo", "txt", "fn main() {\n    let x = 1;\n}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    feed(&rpc, ":set filetype=rust<CR>");

    feed(&rpc, ":TSInstall acme/tree-sitter-demo<CR>");
    let msg = wait_for_message(&rpc, &mut incoming, "TSInstall: installed").await;
    assert!(
        msg.contains("installed rust"),
        "repo install should use the declared grammar name: {msg:?}"
    );
    assert!(
        msg.contains("file-types[rz, rzz]"),
        "install must surface the grammar's declared file-types: {msg:?}"
    );
    assert!(
        data_dir.join("parser/rust.so").exists(),
        "repo grammar didn't compile to parser/rust.so"
    );
    assert!(
        data_dir.join("queries/rust/highlights.scm").exists(),
        "the repo's own queries weren't copied under the declared language name"
    );

    let spans = wait_for_highlights(&rpc, &mut incoming).await;
    assert!(
        spans > 0,
        "custom repo grammar produced no highlights after install"
    );
}

#[tokio::test]
async fn ts_install_repo_without_declared_name_derives_from_repo() {
    let _guard = test_lock().lock().await;
    // A repo whose tree-sitter.json omits `name`/`file-types` (or ships none): the
    // language falls back to the repo name minus `tree-sitter-` (`demo`), and the
    // install still succeeds — it just reports no declared file-types.
    let data_dir = repo_fixture(
        "noname",
        "main",
        r#"{ "grammars": [ { "scope": "source.x" } ] }"#,
    );
    let (rpc, mut incoming) = start(None).await;

    feed(&rpc, ":TSInstall acme/tree-sitter-demo@main<CR>");
    let msg = wait_for_message(&rpc, &mut incoming, "TSInstall: installed").await;
    assert!(
        msg.contains("installed demo") && !msg.contains("file-types["),
        "expected a name derived from the repo and no declared file-types: {msg:?}"
    );
    assert!(data_dir.join("parser/demo.so").exists());
}

/// A minimal self-contained tree-sitter grammar definition (no external DSL
/// helpers), so the CLI generates it with just its built-in JS engine.
const DEMO_GRAMMAR_JS: &str = "module.exports = grammar({\n\
     \x20 name: 'demo',\n\
     \x20 rules: {\n\
     \x20   source_file: $ => repeat($.word),\n\
     \x20   word: $ => /[A-Za-z]+/,\n\
     \x20 }\n\
     });\n";

const DEMO_TS_JSON: &str = "{ \"grammars\": [ { \"name\": \"demo\", \"scope\": \"source.demo\", \
     \"file-types\": [\"demo\"] } ], \"metadata\": { \"version\": \"0.0.1\" } }";

/// Build a repo tarball that ships `grammar.js` + `tree-sitter.json` + a query but
/// **no** generated `src/` — exercising the `tree-sitter generate` fallback.
fn build_grammar_js_repo_tarball(out: &Path, top_dir: &str) {
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let stage = out.parent().unwrap().join(format!(
        "stage-{}",
        out.file_name().unwrap().to_string_lossy()
    ));
    let _ = std::fs::remove_dir_all(&stage);
    let pkg = stage.join(top_dir);
    std::fs::create_dir_all(&pkg).unwrap();
    write_under(&pkg.join("grammar.js"), DEMO_GRAMMAR_JS.as_bytes());
    write_under(&pkg.join("tree-sitter.json"), DEMO_TS_JSON.as_bytes());
    write_under(&pkg.join("queries/highlights.scm"), b"(word) @variable\n");
    let status = std::process::Command::new("tar")
        .arg("czf")
        .arg(out)
        .arg("-C")
        .arg(&stage)
        .arg(top_dir)
        .status()
        .expect("tar czf");
    assert!(status.success(), "building grammar.js repo tarball failed");
}

/// Whether the `tree-sitter` CLI is on PATH — the generate path needs it.
fn tree_sitter_cli_present() -> bool {
    std::process::Command::new("tree-sitter")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn ts_install_repo_generates_parser_from_grammar_js_via_cli() {
    let _guard = test_lock().lock().await;
    // The repo commits only `grammar.js` (no generated parser). nxvim can't generate
    // one itself, but shells out to the user's `tree-sitter` CLI when present. Skip
    // if it isn't installed (the convention for an unavailable external dependency).
    if !tree_sitter_cli_present() {
        eprintln!("skip: `tree-sitter` CLI not installed (needed for the generate path)");
        return;
    }
    let base = std::env::temp_dir().join("nxvim-tsinstall-repo-generate");
    let _ = std::fs::remove_dir_all(&base);
    let data_dir = base.join("data");
    let mirror = base.join("mirror");
    std::fs::create_dir_all(&data_dir).unwrap();
    build_grammar_js_repo_tarball(
        &mirror.join("github.com/acme/tree-sitter-demo/archive/HEAD.tar.gz"),
        "tree-sitter-demo-HEAD",
    );
    std::env::set_var("NXVIM_DATA_DIR", &data_dir);
    std::env::set_var("NXVIM_TS_MIRROR", &mirror);
    std::env::set_var(
        "NXVIM_CC",
        std::env::var("CC").unwrap_or_else(|_| "cc".into()),
    );

    let file = write_temp("tsinstall_gen", "txt", "hello world of demo\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    feed(&rpc, ":set filetype=demo<CR>");
    feed(&rpc, ":TSInstall acme/tree-sitter-demo<CR>");

    // "installed" | "failed" is the terminal state (not the transient "installing…").
    let msg = wait_for_message(&rpc, &mut incoming, "TSInstall: installed").await;
    assert!(
        msg.contains("installed demo"),
        "generate-from-grammar.js install failed: {msg:?}"
    );
    assert!(
        data_dir.join("parser/demo.so").exists(),
        "the generated grammar didn't compile to parser/demo.so"
    );
    // Proof the generated + compiled parser actually loads and highlights.
    let spans = wait_for_highlights(&rpc, &mut incoming).await;
    assert!(
        spans > 0,
        "the CLI-generated grammar produced no highlights"
    );
}
