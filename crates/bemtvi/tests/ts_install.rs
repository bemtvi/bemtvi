//! `:TSInstall <lang>` end to end, through the real server, **offline**.
//!
//! The installer (`bemtvi_ts::install`) fetches nvim-treesitter's `parsers.lua`,
//! the grammar source tarball, and the queries over HTTPS. For a hermetic test we
//! set `$BEMTVI_TS_MIRROR`, which redirects every GET to a local directory tree
//! (`<mirror>/<host>/<path>`), and `$BEMTVI_CC=cc` to pin the compiler to the
//! system one (no Zig download). The mirror's grammar tarball is built from the
//! `tree-sitter-rust` crate source cargo already unpacked for us — so the test
//! does a *real* gunzip → untar → C compile → `dlopen`, then drives the editor to
//! prove indentation works against the freshly-installed parser.
//!
//! These tests share process-global env (`$BEMTVI_DATA_DIR`, `$BEMTVI_TS_*`), so
//! they serialize on the shared lock and use a dir distinct from the other ts
//! fixtures.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    cursor, drain_to_latest_redraw, exec_lua, feed, lines, message, mode, poll_true,
    serial_lock as test_lock, start_attached, window0_field, write_temp,
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

/// Build a fresh `$BEMTVI_DATA_DIR` + `$BEMTVI_TS_MIRROR` for one test and export
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
    let base = bemtvi_test_harness::temp_root().join(format!("bemtvi-tsinstall-{tag}"));
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

    std::env::set_var("BEMTVI_DATA_DIR", &data_dir);
    std::env::set_var("BEMTVI_TS_MIRROR", &mirror);
    std::env::set_var("BEMTVI_TS_REF", REF);
    std::env::set_var("BEMTVI_CC", bemtvi_test_harness::c_compiler_cmd());
    (data_dir, mirror)
}

fn write_under(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// `cp -R from to`, in-process: no `cp` on a stock Windows `PATH`.
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).expect("read grammar src") {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), &dest).expect("copy grammar src file");
        }
    }
}

/// Pack the cargo-unpacked `tree-sitter-rust` `src/` into a `.tar.gz` whose top
/// dir is `top_dir/` — exactly what GitHub's archive endpoint serves, so the
/// installer's untar + `src/parser.c` discovery hit the same layout as
/// production. `top_dir` is a parameter because GitHub mangles it (a `vX.Y.Z` tag
/// becomes `…-X.Y.Z`), which the installer must not assume. Uses the system `tar`
/// (the harness already shells out to `cc`), staging via [`copy_dir`].
fn build_source_tarball(out: &Path, top_dir: &str) {
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let stage = out.parent().unwrap().join(format!(
        "stage-{}",
        out.file_name().unwrap().to_string_lossy()
    ));
    let _ = std::fs::remove_dir_all(&stage);
    let pkg = stage.join(top_dir);
    std::fs::create_dir_all(&pkg).unwrap();
    copy_dir(&grammar_src_dir().join("src"), &pkg.join("src"));
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
    wait_for_message_within(rpc, incoming, needle, Duration::from_secs(30)).await
}

/// [`wait_for_message`] with an explicit time budget — the live test may first
/// download the pinned Zig, which can take minutes.
async fn wait_for_message_within(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    needle: &str,
    budget: Duration,
) -> String {
    let last = std::cell::RefCell::new(String::new());
    for _ in 0..budget.as_millis() / 100 {
        let _ = mode(rpc).await; // barrier: cycle the server loop
        if let Some(map) = drain_to_latest_redraw(incoming, |m| {
            let msg = message(m);
            let hit = msg.contains(needle);
            if !msg.is_empty() {
                *last.borrow_mut() = msg;
            }
            hit
        }) {
            return message(&map);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let last = last.into_inner();
    panic!("timed out waiting for message containing {needle:?}; last message: {last:?}");
}

/// Wait for a `:TSInstall`'s outcome — `TSInstall: installed …` or `TSInstall: <spec>
/// failed: …` — skipping the transient `installing…` line. Returns the message.
async fn wait_for_install_result(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> String {
    let last = std::cell::RefCell::new(String::new());
    for _ in 0..300 {
        let _ = mode(rpc).await; // barrier: cycle the server loop
        if let Some(map) = drain_to_latest_redraw(incoming, |m| {
            let msg = message(m);
            let hit = msg.starts_with("TSInstall:")
                && (msg.contains("installed ") || msg.contains(" failed"));
            if !msg.is_empty() {
                *last.borrow_mut() = msg;
            }
            hit
        }) {
            return message(&map);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let last = last.into_inner();
    panic!("timed out waiting for a :TSInstall result; last message: {last:?}");
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
    assert!(
        poll_true(
            &rpc,
            "local all = table.concat(btv.buf.lines(0, 0, -1), '\\n') \
             return all:find('rust') ~= nil and all:find('indents') ~= nil"
        )
        .await,
        "TSInstallInfo listing missing rust/indents: {:?}",
        lines(&rpc).await
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
    let base = bemtvi_test_harness::temp_root().join("bemtvi-tsinstall-live");
    let _ = std::fs::remove_dir_all(&base);
    let data_dir = base.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::env::set_var("BEMTVI_DATA_DIR", &data_dir);
    std::env::remove_var("BEMTVI_TS_MIRROR");
    std::env::remove_var("BEMTVI_TS_REF");
    // The production path: build with the fetched pinned Zig, unless `$CC` pins one.
    let pinned_cc = std::env::var("CC").ok();
    match &pinned_cc {
        Some(cc) => std::env::set_var("BEMTVI_CC", cc),
        None => std::env::remove_var("BEMTVI_CC"),
    }

    let file = write_temp("tsinstall_live", "rs", "fn main() {\n}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    feed(&rpc, ":set expandtab<CR>");
    feed(&rpc, ":TSInstall rust<CR>");
    // "installed" is not a substring of the transient "installing…" echo.
    let msg = wait_for_message_within(
        &rpc,
        &mut incoming,
        "installed rust",
        Duration::from_secs(600),
    )
    .await;
    assert!(
        msg.contains("installed rust"),
        "live install failed: {msg:?}"
    );
    if pinned_cc.is_none() {
        assert!(
            msg.contains("[zig (fetched)]"),
            "expected the pinned Zig: {msg:?}"
        );
    }
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
    let root = bemtvi_test_harness::temp_root().join("bemtvi-tsinstall-inherits");
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
    std::env::set_var("BEMTVI_DATA_DIR", &data_dir);
    std::env::set_var("BEMTVI_TS_MIRROR", &mirror);
    std::env::set_var("BEMTVI_TS_REF", REF);
    std::env::set_var("BEMTVI_CC", bemtvi_test_harness::c_compiler_cmd());

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

/// The `; inherits:` names come from downloaded query text and are joined into
/// `<data>/queries/<lang>/`: a `../..` there must not write query files outside the
/// data dir. Only a plain grammar identifier is followed.
#[tokio::test]
async fn ts_install_ignores_an_inherited_name_that_escapes_the_query_dir() {
    let _guard = test_lock().lock().await;
    let root = bemtvi_test_harness::temp_root().join("bemtvi-tsinstall-inherit-escape");
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
        b"; inherits: ../../escaped\n(identifier) @variable\n",
    );
    // Where the mirror resolves `runtime/queries/../../escaped/highlights.scm`.
    write_under(
        &nt.join("escaped/highlights.scm"),
        b"(line_comment) @comment\n",
    );
    build_source_tarball(
        &mirror
            .join("github.com/tree-sitter/tree-sitter-rust/archive")
            .join(format!("{REV}.tar.gz")),
        &format!("tree-sitter-rust-{REV}"),
    );
    std::env::set_var("BEMTVI_DATA_DIR", &data_dir);
    std::env::set_var("BEMTVI_TS_MIRROR", &mirror);
    std::env::set_var("BEMTVI_TS_REF", REF);
    std::env::set_var("BEMTVI_CC", bemtvi_test_harness::c_compiler_cmd());

    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, ":TSInstall rust<CR>");
    let msg = wait_for_message(&rpc, &mut incoming, "TSInstall: installed").await;
    assert!(
        !root.join("escaped").exists(),
        "an inherited `../..` name wrote outside the data dir ({msg:?})"
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

/// With no `$BEMTVI_CC`, the installer must compile with the pinned, fetched Zig
/// under `<data>/zig/` — never a `cc`/`gcc`/`clang`/`zig` that happens to be on
/// PATH. The "fetched" Zig is pre-seeded as a non-executable stub so no download
/// happens and running it fails: the install must then fail loud naming that stub,
/// where probing PATH would have silently built with the system compiler.
#[tokio::test]
async fn ts_install_never_uses_a_compiler_from_path() {
    let _guard = test_lock().lock().await;
    let (data_dir, _mirror) = fixture("nopathcc");
    std::env::remove_var("BEMTVI_CC");
    let stem = format!(
        "zig-{}-{}-{}",
        std::env::consts::ARCH,
        std::env::consts::OS,
        bemtvi_test_harness::ZIG_VERSION
    );
    let fake_zig = data_dir
        .join("zig")
        .join(bemtvi_test_harness::ZIG_VERSION)
        .join(&stem)
        .join(format!("zig{}", std::env::consts::EXE_SUFFIX));
    write_under(&fake_zig, b"not a zig binary");

    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, ":TSInstall rust<CR>");
    // Wait for either outcome: success (a PATH compiler was used) or failure.
    let mut msg = String::new();
    for _ in 0..300 {
        let _ = mode(&rpc).await;
        let done = |m: &[(Value, Value)]| {
            let t = message(m);
            t.contains("TSInstall: rust failed") || t.contains("TSInstall: installed rust")
        };
        if let Some(map) = drain_to_latest_redraw(&mut incoming, done) {
            msg = message(&map);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    std::env::set_var("BEMTVI_CC", bemtvi_test_harness::c_compiler_cmd());
    assert!(
        msg.contains("TSInstall: rust failed") && msg.contains(&stem),
        "expected the build to run the pinned Zig ({stem}), got: {msg:?}"
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
    copy_dir(&grammar_src_dir().join("src"), &pkg.join("src"));
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
    let base = bemtvi_test_harness::temp_root().join(format!("bemtvi-tsinstall-repo-{tag}"));
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
    std::env::set_var("BEMTVI_DATA_DIR", &data_dir);
    std::env::set_var("BEMTVI_TS_MIRROR", &mirror);
    std::env::set_var("BEMTVI_CC", bemtvi_test_harness::c_compiler_cmd());
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
/// helpers), generated by the in-process QuickJS host.
const DEMO_GRAMMAR_JS: &str = "module.exports = grammar({\n\
     \x20 name: 'demo',\n\
     \x20 rules: {\n\
     \x20   source_file: $ => repeat($.word),\n\
     \x20   word: $ => /[A-Za-z]+/,\n\
     \x20 }\n\
     });\n";

const DEMO_TS_JSON: &str = "{ \"grammars\": [ { \"name\": \"demo\", \"scope\": \"source.demo\", \
     \"file-types\": [\"demo\"] } ], \"metadata\": { \"version\": \"0.0.1\" } }";

/// The demo grammar as `tree-sitter generate` writes `src/grammar.json`.
const DEMO_GRAMMAR_JSON: &str = r#"{
  "name": "demo",
  "rules": {
    "source_file": { "type": "REPEAT", "content": { "type": "SYMBOL", "name": "word" } },
    "word": { "type": "PATTERN", "value": "[A-Za-z]+" }
  },
  "extras": [ { "type": "PATTERN", "value": "\\s" } ]
}"#;

/// Build a repo tarball holding exactly `files` (paths relative to the repo root),
/// with no generated `src/parser.c` unless one is listed — exercising generation.
fn build_files_tarball(out: &Path, top_dir: &str, files: &[(&str, &str)]) {
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let stage = out.parent().unwrap().join(format!(
        "stage-{}",
        out.file_name().unwrap().to_string_lossy()
    ));
    let _ = std::fs::remove_dir_all(&stage);
    let pkg = stage.join(top_dir);
    std::fs::create_dir_all(&pkg).unwrap();
    for (rel, body) in files {
        write_under(&pkg.join(rel), body.as_bytes());
    }
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

/// A fresh data dir + mirror serving `acme/tree-sitter-demo` at `HEAD` with `files`,
/// and the env pointing at them. `$BEMTVI_TREE_SITTER_CLI` is cleared, so the
/// in-process generator is what runs.
fn generate_fixture(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    let base = bemtvi_test_harness::temp_root().join(format!("bemtvi-tsinstall-gen-{tag}"));
    let _ = std::fs::remove_dir_all(&base);
    let data_dir = base.join("data");
    let mirror = base.join("mirror");
    std::fs::create_dir_all(&data_dir).unwrap();
    build_files_tarball(
        &mirror.join("github.com/acme/tree-sitter-demo/archive/HEAD.tar.gz"),
        "tree-sitter-demo-HEAD",
        files,
    );
    std::env::set_var("BEMTVI_DATA_DIR", &data_dir);
    std::env::set_var("BEMTVI_TS_MIRROR", &mirror);
    std::env::set_var("BEMTVI_CC", bemtvi_test_harness::c_compiler_cmd());
    std::env::remove_var("BEMTVI_TREE_SITTER_CLI");
    data_dir
}

/// Install `acme/tree-sitter-demo` into a buffer of demo words and wait for the
/// terminal message. Returns it, plus the session for further checks.
async fn install_demo(tag: &str) -> (String, Rpc, UnboundedReceiver<Incoming>) {
    let file = write_temp(tag, "txt", "hello world of demo\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    feed(&rpc, ":set filetype=demo<CR>");
    feed(&rpc, ":TSInstall acme/tree-sitter-demo<CR>");
    let msg = wait_for_install_result(&rpc, &mut incoming).await;
    (msg, rpc, incoming)
}

#[tokio::test]
async fn ts_install_repo_generates_parser_from_grammar_js() {
    let _guard = test_lock().lock().await;
    // The repo commits only `grammar.js`: no `src/` at all. bemtvi evaluates it in
    // its embedded QuickJS and generates the parser itself — no CLI, no node.
    let data_dir = generate_fixture(
        "js",
        &[
            ("grammar.js", DEMO_GRAMMAR_JS),
            ("tree-sitter.json", DEMO_TS_JSON),
            ("queries/highlights.scm", "(word) @variable\n"),
        ],
    );
    let file = write_temp("tsinstall_gen", "txt", "hello world of demo\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    feed(&rpc, ":set filetype=demo<CR>");
    feed(&rpc, ":TSInstall acme/tree-sitter-demo<CR>");
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
    assert!(spans > 0, "the generated grammar produced no highlights");
}

#[tokio::test]
async fn ts_install_generates_a_monorepo_grammar_that_requires_shared_code() {
    let _guard = test_lock().lock().await;
    // The grammar lives in a subdir and `require`s a helper from a sibling dir of
    // the repo, as monorepos (typescript/tsx, markdown/inline) share rules.
    let ts_json = r#"{ "grammars": [ { "name": "demo", "path": "grammars/demo" } ],
        "metadata": { "version": "0.0.1" } }"#;
    let grammar = "const common = require('../../common/rules');\n\
                   module.exports = grammar({\n\
                   \x20 name: 'demo',\n\
                   \x20 rules: { source_file: $ => repeat($.word), word: $ => common.WORD },\n\
                   });\n";
    let data_dir = generate_fixture(
        "require",
        &[
            ("tree-sitter.json", ts_json),
            ("grammars/demo/grammar.js", grammar),
            (
                "common/rules.js",
                "module.exports = { WORD: /[A-Za-z]+/ };\n",
            ),
            ("grammars/demo/queries/highlights.scm", "(word) @variable\n"),
        ],
    );
    let (msg, rpc, mut incoming) = install_demo("tsinstall_gen_req").await;
    assert!(msg.contains("installed demo"), "install failed: {msg:?}");
    assert!(data_dir.join("parser/demo.so").exists());
    assert!(wait_for_highlights(&rpc, &mut incoming).await > 0);
}

#[tokio::test]
async fn ts_install_generates_from_a_committed_grammar_json() {
    let _guard = test_lock().lock().await;
    // `src/grammar.json` but no `parser.c` (and no grammar.js): generated directly,
    // without evaluating any JS.
    let data_dir = generate_fixture(
        "json",
        &[
            ("tree-sitter.json", DEMO_TS_JSON),
            ("src/grammar.json", DEMO_GRAMMAR_JSON),
            ("queries/highlights.scm", "(word) @variable\n"),
        ],
    );
    let (msg, rpc, mut incoming) = install_demo("tsinstall_gen_json").await;
    assert!(msg.contains("installed demo"), "install failed: {msg:?}");
    assert!(data_dir.join("parser/demo.so").exists());
    assert!(wait_for_highlights(&rpc, &mut incoming).await > 0);
}

#[tokio::test]
async fn ts_install_generate_fails_loud_on_a_module_it_cannot_reach() {
    let _guard = test_lock().lock().await;
    // A grammar that needs an npm package (not in the tarball) and one that reaches
    // outside the source tree both fail, naming the module, and build nothing. The
    // escape target is a valid module (in the fixture dir, four levels above the
    // unpacked grammar at `data/.ts-build/<repo>/<top>/`), so only the sandbox stops
    // it: without the check the install would succeed.
    for (tag, module) in [
        ("npm", "tree-sitter-c/grammar"),
        ("escape", "../../../../outside.js"),
    ] {
        let grammar = format!(
            "const base = require('{module}');\nmodule.exports = grammar({{ name: 'demo', \
             rules: {{ source_file: $ => base.WORD }} }});\n"
        );
        let data_dir = generate_fixture(
            &format!("missing-{tag}"),
            &[("grammar.js", &grammar), ("tree-sitter.json", DEMO_TS_JSON)],
        );
        std::fs::write(
            data_dir.parent().unwrap().join("outside.js"),
            "module.exports = { WORD: /x/ };\n",
        )
        .unwrap();
        let (msg, _rpc, _incoming) = install_demo(&format!("tsinstall_gen_{tag}")).await;
        assert!(
            msg.contains("failed") && msg.contains(module),
            "an unreachable module must fail loud naming {module:?}: {msg:?}"
        );
        assert!(
            !data_dir.join("parser/demo.so").exists(),
            "nothing was built"
        );
    }
}

#[tokio::test]
async fn ts_install_catalog_generate_entry_builds_from_grammar_js() {
    let _guard = test_lock().lock().await;
    // A catalog language nvim-treesitter marks `generate = true` (latex, swift, perl,
    // …): its repo commits grammar.js only. `:TSInstall demo` generates it.
    let base = bemtvi_test_harness::temp_root().join("bemtvi-tsinstall-gen-catalog");
    let _ = std::fs::remove_dir_all(&base);
    let data_dir = base.join("data");
    let mirror = base.join("mirror");
    std::fs::create_dir_all(&data_dir).unwrap();
    let nt = mirror
        .join("raw.githubusercontent.com/nvim-treesitter/nvim-treesitter")
        .join(REF);
    write_under(
        &nt.join("lua/nvim-treesitter/parsers.lua"),
        format!(
            "return {{
  demo = {{
    install_info = {{
      generate = true,
      revision = '{REV}',
      url = 'https://github.com/acme/tree-sitter-demo',
    }},
    tier = 2,
  }},
}}
"
        )
        .as_bytes(),
    );
    write_under(
        &nt.join("runtime/queries/demo/highlights.scm"),
        b"(word) @variable\n",
    );
    build_files_tarball(
        &mirror
            .join("github.com/acme/tree-sitter-demo/archive")
            .join(format!("{REV}.tar.gz")),
        &format!("tree-sitter-demo-{REV}"),
        &[("grammar.js", DEMO_GRAMMAR_JS)],
    );
    std::env::set_var("BEMTVI_DATA_DIR", &data_dir);
    std::env::set_var("BEMTVI_TS_MIRROR", &mirror);
    std::env::set_var("BEMTVI_TS_REF", REF);
    std::env::set_var("BEMTVI_CC", bemtvi_test_harness::c_compiler_cmd());
    std::env::remove_var("BEMTVI_TREE_SITTER_CLI");

    let file = write_temp("tsinstall_gen_catalog", "txt", "hello world of demo\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    feed(&rpc, ":set filetype=demo<CR>");
    feed(&rpc, ":TSInstall demo<CR>");
    let msg = wait_for_install_result(&rpc, &mut incoming).await;
    assert!(
        msg.contains("installed"),
        "catalog generate install failed: {msg:?}"
    );
    assert!(data_dir.join("parser/demo.so").exists());
    assert!(wait_for_highlights(&rpc, &mut incoming).await > 0);
}

/// A repo's `tree-sitter.json` `path` names the grammar's subdir *inside* the repo. An
/// absolute or `..` path would point the build (and the generator, which
/// writes) at a directory outside the unpacked source, so it fails loud instead.
#[tokio::test]
async fn ts_install_repo_rejects_a_grammar_path_outside_the_repo() {
    let _guard = test_lock().lock().await;
    let outside = grammar_src_dir();
    for (tag, path) in [
        ("abs", outside.to_string_lossy().into_owned()),
        ("dotdot", "../../..".to_string()),
    ] {
        let json = format!(
            r#"{{ "grammars": [ {{ "name": "rust", "path": {} }} ] }}"#,
            serde_json::Value::String(path.clone())
        );
        let data_dir = repo_fixture(&format!("badpath-{tag}"), "HEAD", &json);
        let (rpc, mut incoming) = start(None).await;
        feed(&rpc, ":TSInstall acme/tree-sitter-demo<CR>");
        let msg = wait_for_message(&rpc, &mut incoming, "TSInstall: acme/tree-sitter-demo").await;
        assert!(
            msg.contains("failed") && msg.contains("path"),
            "a grammar path of {path:?} must fail loud: {msg:?}"
        );
        assert!(
            !data_dir.join("parser/rust.so").exists(),
            "nothing was built"
        );
    }
}

// ----- `lang@ref` -----------------------------------------------------------

/// `git` with no user or system config and a fixed identity, so the fixture repo
/// doesn't depend on the developer's git setup (signing, hooks, templates).
fn git(cwd: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "bemtvi test")
        .env("GIT_AUTHOR_EMAIL", "test@bemtvi")
        .env("GIT_COMMITTER_NAME", "bemtvi test")
        .env("GIT_COMMITTER_EMAIL", "test@bemtvi")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed in {cwd:?}: {out:?}"
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Serve `tree-sitter-rust` at `HEAD` from the mirror the way GitHub does: a
/// `git archive` tarball (which carries the commit id in a pax global header) with
/// a `<repo>-<sha>/` top dir. Returns the commit.
fn serve_rust_head(mirror: &Path) -> String {
    let repo = mirror.parent().unwrap().join("rust-repo");
    let _ = std::fs::remove_dir_all(&repo);
    copy_dir(&grammar_src_dir().join("src"), &repo.join("src"));
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "grammar"]);
    let sha = git(&repo, &["rev-parse", "HEAD"]);
    let out = mirror.join("github.com/tree-sitter/tree-sitter-rust/archive/HEAD.tar.gz");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    git(
        &repo,
        &[
            "archive",
            "--format=tar.gz",
            &format!("--prefix=tree-sitter-rust-{sha}/"),
            "-o",
            out.to_str().unwrap(),
            "HEAD",
        ],
    );
    sha
}

#[tokio::test]
async fn ts_install_lang_at_ref_builds_the_ref_and_ts_update_follows_it() {
    let _guard = test_lock().lock().await;
    let (data_dir, mirror) = fixture("follow");
    let sha = serve_rust_head(&mirror);
    let record = data_dir.join("parser-info/rust.json");
    let (rpc, mut incoming) = start(None).await;

    // `rust@HEAD` builds the HEAD tarball and names the commit HEAD resolved to.
    feed(&rpc, ":TSInstall rust@HEAD<CR>");
    let msg = wait_for_install_result(&rpc, &mut incoming).await;
    assert!(
        msg.contains("installed rust") && msg.contains(&sha[..9]) && msg.contains("following HEAD"),
        "expected rust built at HEAD ({sha}): {msg:?}"
    );
    assert!(data_dir.join("parser/rust.so").exists());
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record).expect("a record")).unwrap();
    assert_eq!(json["ref"], "HEAD");
    assert_eq!(json["commit"], sha.as_str());

    // `:TSUpdate rust` follows the record. With the pinned revision's tarball gone
    // from the mirror, going back to it would fail; following HEAD succeeds.
    let pinned = mirror.join(format!(
        "github.com/tree-sitter/tree-sitter-rust/archive/{REV}.tar.gz"
    ));
    let pinned_bytes = std::fs::read(&pinned).unwrap();
    std::fs::remove_file(&pinned).unwrap();
    std::fs::remove_file(data_dir.join("parser/rust.so")).unwrap();
    feed(&rpc, ":TSUpdate rust<CR>");
    let msg = wait_for_install_result(&rpc, &mut incoming).await;
    assert!(
        msg.contains("installed rust") && msg.contains("following HEAD"),
        ":TSUpdate must rebuild at the recorded ref: {msg:?}"
    );
    assert!(data_dir.join("parser/rust.so").exists());

    // `:TSInstallInfo` shows what it follows.
    feed(&rpc, ":TSInstallInfo<CR>");
    let mut listed = Vec::new();
    for _ in 0..100 {
        listed = lines(&rpc).await;
        if listed
            .iter()
            .any(|l| l.contains("following HEAD") && l.contains(&sha[..9]))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        listed
            .iter()
            .any(|l| l.contains("following HEAD") && l.contains(&sha[..9])),
        "TSInstallInfo should list the followed ref: {listed:?}"
    );

    // A plain `:TSInstall rust` goes back to the pinned revision and forgets HEAD.
    std::fs::write(&pinned, pinned_bytes).unwrap();
    feed(&rpc, ":TSInstall rust<CR>");
    let msg = wait_for_install_result(&rpc, &mut incoming).await;
    assert!(
        msg.contains(&format!("installed rust @ {}", &REV[..9])) && !msg.contains("following"),
        "a plain install builds the pinned revision: {msg:?}"
    );
    assert!(!record.exists(), "a plain install forgets the followed ref");
}

#[tokio::test]
async fn ts_install_lang_at_a_missing_ref_fails_loud_and_records_nothing() {
    let _guard = test_lock().lock().await;
    let (data_dir, _mirror) = fixture("follow-missing");
    let (rpc, mut incoming) = start(None).await;
    for (spec, why) in [
        ("rust@nosuchbranch", "nosuchbranch"),
        ("rust@a..b", "invalid ref"),
    ] {
        feed(&rpc, &format!(":TSInstall {spec}<CR>"));
        let msg = wait_for_install_result(&rpc, &mut incoming).await;
        assert!(
            msg.contains("failed") && msg.contains(why),
            "{spec} must fail loud mentioning {why:?}: {msg:?}"
        );
    }
    assert!(!data_dir.join("parser/rust.so").exists());
    assert!(!data_dir.join("parser-info/rust.json").exists());
}

// ----- review fixes ---------------------------------------------------------

#[tokio::test]
async fn ts_install_ref_with_url_characters_is_refused_before_any_fetch() {
    let _guard = test_lock().lock().await;
    // A ref goes into the archive URL (`archive/<ref>.tar.gz`), so `?` / `#` / `%`
    // would turn it into a query or fragment. Refs are limited to what git allows.
    let (data_dir, _mirror) = fixture("ref-url-chars");
    let (rpc, mut incoming) = start(None).await;
    for spec in ["rust@x?foo=bar", "rust@x#frag", "rust@x%2e", "rust@x\\y"] {
        feed(&rpc, &format!(":TSInstall {spec}<CR>"));
        let msg = wait_for_install_result(&rpc, &mut incoming).await;
        assert!(
            msg.contains("failed") && msg.contains("invalid ref"),
            "{spec} must be refused as an invalid ref: {msg:?}"
        );
    }
    assert!(!data_dir.join("parser/rust.so").exists());
}

#[tokio::test]
async fn ts_install_from_a_repo_forgets_a_followed_catalog_ref() {
    let _guard = test_lock().lock().await;
    // `rust@HEAD` records the ref; a later repo install of a grammar named `rust`
    // replaces that build, so the record must go (else :TSUpdate rebuilds the catalog
    // grammar over the repo one).
    let (data_dir, mirror) = fixture("repo-forgets-ref");
    serve_rust_head(&mirror);
    build_repo_tarball(
        &mirror.join("github.com/acme/tree-sitter-demo/archive/HEAD.tar.gz"),
        "tree-sitter-demo-HEAD",
        r#"{ "grammars": [ { "name": "rust", "scope": "source.rust" } ] }"#,
    );
    let record = data_dir.join("parser-info/rust.json");
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, ":TSInstall rust@HEAD<CR>");
    let msg = wait_for_install_result(&rpc, &mut incoming).await;
    assert!(msg.contains("following HEAD"), "{msg:?}");
    assert!(record.exists());
    feed(&rpc, ":TSInstall acme/tree-sitter-demo<CR>");
    let msg = wait_for_install_result(&rpc, &mut incoming).await;
    assert!(msg.contains("installed rust"), "{msg:?}");
    assert!(
        !record.exists(),
        "the repo install must forget the followed ref"
    );
}

#[tokio::test]
async fn ts_install_info_survives_a_non_ascii_commit_in_the_record() {
    let _guard = test_lock().lock().await;
    // The record is a file on disk; `:TSInstallInfo` shortens its commit for display
    // and must not panic on a multi-byte one.
    let (data_dir, _mirror) = fixture("info-non-ascii");
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, ":TSInstall rust<CR>");
    let msg = wait_for_install_result(&rpc, &mut incoming).await;
    assert!(msg.contains("installed rust"), "{msg:?}");
    std::fs::create_dir_all(data_dir.join("parser-info")).unwrap();
    std::fs::write(
        data_dir.join("parser-info/rust.json"),
        r#"{ "ref": "main", "commit": "ééééééééééé" }"#,
    )
    .unwrap();
    feed(&rpc, ":TSInstallInfo<CR>");
    let mut listed = Vec::new();
    for _ in 0..100 {
        listed = lines(&rpc).await;
        if listed.iter().any(|l| l.contains("following main")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        listed
            .iter()
            .any(|l| l.contains("following main @ ééééééééé")),
        "the listing shortens the commit by characters: {listed:?}"
    );
}

/// With `$BEMTVI_TREE_SITTER_CLI` set, a repo that commits only `src/grammar.json`
/// must hand that file to the CLI (bare `generate` reads `grammar.js`). The fake CLI
/// records its arguments and fails, which is all this needs to see.
#[cfg(unix)]
#[tokio::test]
async fn ts_install_cli_override_is_given_a_committed_grammar_json() {
    use std::os::unix::fs::PermissionsExt;
    let _guard = test_lock().lock().await;
    let data_dir = generate_fixture(
        "cli-json",
        &[
            ("tree-sitter.json", DEMO_TS_JSON),
            ("src/grammar.json", DEMO_GRAMMAR_JSON),
        ],
    );
    let base = data_dir.parent().unwrap();
    let args_file = base.join("cli-args.txt");
    let cli = base.join("fake-tree-sitter");
    std::fs::write(
        &cli,
        format!(
            "#!/bin/sh\necho \"$@\" > '{}'\nexit 1\n",
            args_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("BEMTVI_TREE_SITTER_CLI", &cli);
    let (msg, _rpc, _incoming) = install_demo("tsinstall_cli_json").await;
    std::env::remove_var("BEMTVI_TREE_SITTER_CLI");
    assert!(msg.contains("failed"), "the fake CLI fails: {msg:?}");
    let args = std::fs::read_to_string(&args_file).expect("the CLI ran");
    assert!(
        args.starts_with("generate") && args.contains("src/grammar.json"),
        "the CLI must be given src/grammar.json: {args:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn ts_install_refuses_a_tarball_symlink_that_leaves_the_archive() {
    let _guard = test_lock().lock().await;
    // A grammar.js-only repo whose `src` is a symlink to a directory outside the
    // unpacked tree: generating would write parser.c & co. there. The unpack refuses it.
    let base = bemtvi_test_harness::temp_root().join("bemtvi-tsinstall-gen-symlink");
    let _ = std::fs::remove_dir_all(&base);
    let data_dir = base.join("data");
    let mirror = base.join("mirror");
    let outside = base.join("outside");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let stage = base.join("stage");
    let top = stage.join("tree-sitter-demo-HEAD");
    write_under(&top.join("grammar.js"), DEMO_GRAMMAR_JS.as_bytes());
    write_under(&top.join("tree-sitter.json"), DEMO_TS_JSON.as_bytes());
    std::os::unix::fs::symlink(&outside, top.join("src")).unwrap();
    let out = mirror.join("github.com/acme/tree-sitter-demo/archive/HEAD.tar.gz");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let status = std::process::Command::new("tar")
        .arg("czf")
        .arg(&out)
        .arg("-C")
        .arg(&stage)
        .arg("tree-sitter-demo-HEAD")
        .status()
        .unwrap();
    assert!(status.success());
    std::env::set_var("BEMTVI_DATA_DIR", &data_dir);
    std::env::set_var("BEMTVI_TS_MIRROR", &mirror);
    std::env::set_var("BEMTVI_CC", bemtvi_test_harness::c_compiler_cmd());
    std::env::remove_var("BEMTVI_TREE_SITTER_CLI");

    let (msg, _rpc, _incoming) = install_demo("tsinstall_gen_symlink").await;
    assert!(
        msg.contains("failed") && msg.contains("symlink"),
        "an escaping symlink must be refused: {msg:?}"
    );
    assert!(
        std::fs::read_dir(&outside).unwrap().next().is_none(),
        "nothing may be written outside the archive"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn ts_install_refuses_a_symlink_that_escapes_through_another_symlink() {
    let _guard = test_lock().lock().await;
    // Each link looks contained on its own, read as text: `a/b/l -> ../../..` lands on
    // the unpack root, and `src -> a/b/l/../../../outside` never climbs above the
    // archive when its `..`s are folded lexically. On disk, though, `a/b/l` is the
    // unpack root, so three `..` past it leave the build dir: `src` is `outside`.
    let base = bemtvi_test_harness::temp_root().join("bemtvi-tsinstall-gen-symlink-chain");
    let _ = std::fs::remove_dir_all(&base);
    let data_dir = base.join("data");
    let mirror = base.join("mirror");
    let outside = base.join("outside");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let stage = base.join("stage");
    let top = stage.join("tree-sitter-demo-HEAD");
    write_under(&top.join("grammar.js"), DEMO_GRAMMAR_JS.as_bytes());
    write_under(&top.join("tree-sitter.json"), DEMO_TS_JSON.as_bytes());
    std::fs::create_dir_all(top.join("a/b")).unwrap();
    std::os::unix::fs::symlink("../../..", top.join("a/b/l")).unwrap();
    std::os::unix::fs::symlink("a/b/l/../../../outside", top.join("src")).unwrap();
    let out = mirror.join("github.com/acme/tree-sitter-demo/archive/HEAD.tar.gz");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let status = std::process::Command::new("tar")
        .arg("czf")
        .arg(&out)
        .arg("-C")
        .arg(&stage)
        .arg("tree-sitter-demo-HEAD")
        .status()
        .unwrap();
    assert!(status.success());
    std::env::set_var("BEMTVI_DATA_DIR", &data_dir);
    std::env::set_var("BEMTVI_TS_MIRROR", &mirror);
    std::env::set_var("BEMTVI_CC", bemtvi_test_harness::c_compiler_cmd());
    std::env::remove_var("BEMTVI_TREE_SITTER_CLI");

    let (msg, _rpc, _incoming) = install_demo("tsinstall_gen_symlink_chain").await;
    assert!(
        msg.contains("failed") && msg.contains("symlink"),
        "a symlink escaping through another must be refused: {msg:?}"
    );
    assert!(
        std::fs::read_dir(&outside).unwrap().next().is_none(),
        "nothing may be written outside the archive"
    );
}

#[tokio::test]
async fn ts_install_lang_at_ref_fires_ts_installed_with_the_language() {
    let _guard = test_lock().lock().await;
    let (_data_dir, mirror) = fixture("follow-autocmd");
    serve_rust_head(&mirror);
    let (rpc, mut incoming) = start(None).await;
    exec_lua(
        &rpc,
        "_G.seen = {} btv.on('TSInstalled', {}, function(a) \
         table.insert(_G.seen, a.match .. '|' .. a.data.lang) end)",
    )
    .await;
    // The pattern is the language (docs/autocmd-events.md), not the `rust@HEAD` spec.
    feed(&rpc, ":TSInstall rust@HEAD<CR>");
    let msg = wait_for_install_result(&rpc, &mut incoming).await;
    assert!(msg.contains("installed rust"), "{msg:?}");
    let seen = exec_lua(&rpc, "return table.concat(_G.seen, ',')").await;
    assert_eq!(seen.as_str(), Some("rust|rust"));
}

#[tokio::test]
async fn ts_install_grammar_js_that_never_finishes_is_stopped() {
    let _guard = test_lock().lock().await;
    // A grammar.js that loops forever must not pin a core and wedge the install (and
    // the editor's exit, which waits for it). The evaluation has a deadline.
    let data_dir = generate_fixture(
        "loop",
        &[
            ("grammar.js", "while (true) {}\n"),
            ("tree-sitter.json", DEMO_TS_JSON),
        ],
    );
    std::env::set_var("BEMTVI_GRAMMAR_JS_TIMEOUT_MS", "500");
    let (msg, _rpc, _incoming) = install_demo("tsinstall_gen_loop").await;
    std::env::remove_var("BEMTVI_GRAMMAR_JS_TIMEOUT_MS");
    assert!(
        msg.contains("failed") && msg.contains("took longer than"),
        "a runaway grammar.js must fail loud: {msg:?}"
    );
    assert!(!data_dir.join("parser/demo.so").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn ts_install_keeps_a_symlink_that_stays_inside_the_archive() {
    let _guard = test_lock().lock().await;
    // The other side of the escaping-symlink check: a link within the repo (here
    // `queries` -> `q`) still unpacks, and the install reads through it.
    let base = bemtvi_test_harness::temp_root().join("bemtvi-tsinstall-gen-symlink-ok");
    let _ = std::fs::remove_dir_all(&base);
    let data_dir = base.join("data");
    let mirror = base.join("mirror");
    std::fs::create_dir_all(&data_dir).unwrap();
    let stage = base.join("stage");
    let top = stage.join("tree-sitter-demo-HEAD");
    write_under(&top.join("grammar.js"), DEMO_GRAMMAR_JS.as_bytes());
    write_under(&top.join("tree-sitter.json"), DEMO_TS_JSON.as_bytes());
    write_under(&top.join("q/highlights.scm"), b"(word) @variable\n");
    std::os::unix::fs::symlink("q", top.join("queries")).unwrap();
    let out = mirror.join("github.com/acme/tree-sitter-demo/archive/HEAD.tar.gz");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let status = std::process::Command::new("tar")
        .arg("czf")
        .arg(&out)
        .arg("-C")
        .arg(&stage)
        .arg("tree-sitter-demo-HEAD")
        .status()
        .unwrap();
    assert!(status.success());
    std::env::set_var("BEMTVI_DATA_DIR", &data_dir);
    std::env::set_var("BEMTVI_TS_MIRROR", &mirror);
    std::env::set_var("BEMTVI_CC", bemtvi_test_harness::c_compiler_cmd());
    std::env::remove_var("BEMTVI_TREE_SITTER_CLI");

    let (msg, rpc, mut incoming) = install_demo("tsinstall_gen_symlink_ok").await;
    assert!(
        msg.contains("installed demo") && msg.contains("highlights"),
        "{msg:?}"
    );
    assert!(wait_for_highlights(&rpc, &mut incoming).await > 0);
}

#[tokio::test]
async fn ts_install_same_language_twice_at_once_both_succeed() {
    let _guard = test_lock().lock().await;
    // `:TSInstall rust rust@HEAD` runs two installs of one language on parallel
    // workers. They share the build dir and the output, so they take turns.
    let (data_dir, mirror) = fixture("same-lang-twice");
    serve_rust_head(&mirror);
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, ":TSInstall rust rust@HEAD<CR>");
    let first = wait_for_install_result(&rpc, &mut incoming).await;
    let mut second = String::new();
    for _ in 0..300 {
        let _ = mode(&rpc).await;
        if let Some(map) = drain_to_latest_redraw(&mut incoming, |m| {
            let msg = message(m);
            msg.starts_with("TSInstall:")
                && msg != first
                && (msg.contains("installed ") || msg.contains(" failed"))
        }) {
            second = message(&map);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        first.contains("installed rust") && second.contains("installed rust"),
        "both installs must succeed: {first:?} / {second:?}"
    );
    assert!(data_dir.join("parser/rust.so").exists());
    // The record matches whichever finished last.
    let record = data_dir.join("parser-info/rust.json").exists();
    assert_eq!(record, second.contains("following HEAD"), "{second:?}");
}
