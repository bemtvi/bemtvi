//! Regression: a node tagged with both a highlight capture and the tree-sitter
//! `@spell` metadata capture must paint with the highlight group, not `spell`.
//!
//! Grammars tag comments (and strings) as `(comment) @comment @spell` — `@comment`
//! is the visual highlight, `@spell` only marks the region for spell-checking.
//! Both captures cover the identical node, so they tie on layer/width/start; with
//! `@spell` sorted last, the painter's last-write-wins pass used to overwrite
//! `@comment` with `spell`. A `spell` span resolves to no colour in any theme and
//! no client fallback paints it, so comments rendered as plain text. The fix skips
//! the metadata captures (`spell` / `nospell` / `conceal`) the way it already skips
//! `_`-prefixed internal captures, so `@comment` wins.
//!
//! `#[ignore]`d, not hermetic: it installs a real grammar (bash) into a temp data
//! dir, which needs network + a C compiler — the opt-in posture of the other
//! treesitter e2e tests. Run with:
//!
//! ```sh
//! cargo test -p nxvim-ts --test spell_capture -- --ignored --nocapture
//! ```

use std::path::PathBuf;

use nxvim_ts::Engine;

/// A unique temp dir under the system temp root (the harness convention — no
/// tempfile dep), removed on drop.
struct TempDataDir(PathBuf);

impl TempDataDir {
    fn new() -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("nxvim_ts_spell_{pid}_{nanos}"));
        std::fs::create_dir_all(&dir).expect("create temp data dir");
        TempDataDir(dir)
    }
}

impl Drop for TempDataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "needs network + a C compiler to install a real grammar; opt-in like PTY e2e"]
fn comment_with_spell_capture_paints_as_comment_not_spell() {
    let data = TempDataDir::new();

    // Install the toml grammar (parser .so + queries). Its `highlights.scm` tags
    // comments as `(comment) @comment @spell`, the shape this regression covers —
    // and, unlike bash, toml has no `#lua-match?`-gated rule competing for the
    // comment node, so the `@comment` group is the unambiguous expected winner.
    nxvim_ts::install::install(&data.0, "toml")
        .expect("install toml grammar (network + C compiler required)");

    let mut engine = Engine::new(data.0.clone());

    // A one-line toml comment. `highlight_text` is the stateless paint path (it
    // runs the same `extract_spans` painter the live buffer does).
    let spans = engine.highlight_text("toml", "# a comment\n", 0, 1);
    assert!(
        !spans.is_empty(),
        "the toml grammar should produce highlight spans for a comment line"
    );

    // The `@spell` metadata capture must never reach the paint as a group.
    assert!(
        spans.iter().all(|s| s.group != "spell"),
        "no span should carry the `spell` metadata capture as its group; got {spans:?}"
    );
    // The comment must paint as `comment` (the `#` at column 0 is inside it).
    assert!(
        spans
            .iter()
            .any(|s| s.group == "comment" && s.start_byte == 0),
        "the comment should paint with the `comment` group; got {spans:?}"
    );
}

#[test]
#[ignore = "needs network + a C compiler to install a real grammar; opt-in like PTY e2e"]
fn lua_match_predicate_gates_the_bash_shebang_rule() {
    let data = TempDataDir::new();

    // bash's `highlights.scm` carries both `(comment) @comment @spell` and a
    // shebang rule `((comment) @keyword.directive @nospell (#lua-match? … "^#![ \t]*/"))`.
    // Without `#lua-match?` enforcement the shebang rule leaked onto *every*
    // comment, so an ordinary `# comment` painted as `keyword.directive` (and the
    // `@nospell` metadata capture wiped it out entirely before that).
    nxvim_ts::install::install(&data.0, "bash")
        .expect("install bash grammar (network + C compiler required)");

    let mut engine = Engine::new(data.0.clone());

    // An ordinary comment does NOT match `^#!/`, so the shebang rule must not fire:
    // it paints as `comment`, never `keyword.directive` / `spell` / `nospell`.
    let plain = engine.highlight_text("bash", "# a comment\n", 0, 1);
    assert!(
        plain
            .iter()
            .any(|s| s.group == "comment" && s.start_byte == 0),
        "an ordinary bash comment should paint as `comment`; got {plain:?}"
    );
    assert!(
        plain
            .iter()
            .all(|s| !matches!(s.group.as_str(), "keyword.directive" | "spell" | "nospell")),
        "the `#lua-match?`-gated shebang rule must not fire on a non-shebang comment; got {plain:?}"
    );

    // A real shebang DOES match `^#!/`, so the predicate is satisfied and the line
    // paints as `keyword.directive` — proving the predicate gates rather than blanks.
    let shebang = engine.highlight_text("bash", "#!/bin/bash\n", 0, 1);
    assert!(
        shebang
            .iter()
            .any(|s| s.group == "keyword.directive" && s.start_byte == 0),
        "a real shebang should paint as `keyword.directive`; got {shebang:?}"
    );
}
