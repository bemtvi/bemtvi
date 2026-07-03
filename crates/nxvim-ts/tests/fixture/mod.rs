//! Shared hermetic grammar fixture for the engine integration tests: compiles a
//! real grammar's C sources out of the cargo registry (dev-dependencies, so the
//! sources are guaranteed present) into a temp dir laid out like the engine's
//! data dir — the same fixture pattern as `crates/nxvim/tests/syntax.rs`, with
//! no network and no `:TSInstall`.

// Each test binary includes this module and uses a subset of the helpers.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// A unique temp dir under the system temp root (the harness convention — no
/// tempfile dep), removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("nxvim_ts_{tag}_{pid}_{nanos}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Compile a grammar's C sources (`parser.c` + the always-present `scanner.c`)
/// from `src_dir` into `<root>/parser/<lang>.so` via the system C compiler —
/// mirroring how a user installs a parser, but hermetic.
pub fn compile_grammar(root: &Path, lang: &str, src_dir: &Path) {
    let dir = root.join("parser");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join(format!("{lang}.so"));
    let compiler = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let status = std::process::Command::new(compiler)
        .args(["-shared", "-fPIC", "-O1"])
        .arg("-I")
        .arg(src_dir)
        .arg(src_dir.join("parser.c"))
        .arg(src_dir.join("scanner.c"))
        .arg("-o")
        .arg(&out)
        .status()
        .expect("run C compiler");
    assert!(status.success(), "compiling {lang} grammar fixture failed");
}

/// Write `<root>/queries/<lang>/<name>.scm`.
pub fn write_query(root: &Path, lang: &str, name: &str, scm: &str) {
    let dir = root.join("queries").join(lang);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{name}.scm")), scm).unwrap();
}

/// Locate an unpacked crate source directory in the cargo registry by its
/// `<name>-<version>` folder name (a dev-dependency, so cargo guarantees
/// presence).
pub fn registry_crate_dir(crate_dir: &str) -> PathBuf {
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

/// Install the Rust grammar (compiled parser + its bundled highlights query)
/// under `root`.
pub fn install_rust_grammar(root: &Path) {
    let src = registry_crate_dir("tree-sitter-rust-0.24.2").join("src");
    compile_grammar(root, "rust", &src);
    write_query(
        root,
        "rust",
        "highlights",
        tree_sitter_rust::HIGHLIGHTS_QUERY,
    );
}

/// Install the Markdown block grammar + its bundled queries under `root` — a
/// host grammar whose `(section)` nodes nest, for the injection tests.
pub fn install_markdown_grammar(root: &Path) {
    let md = registry_crate_dir("tree-sitter-md-0.5.3").join("tree-sitter-markdown");
    compile_grammar(root, "markdown", &md.join("src"));
    for name in ["highlights", "injections"] {
        let scm = std::fs::read_to_string(md.join("queries").join(format!("{name}.scm")))
            .expect("read markdown query");
        write_query(root, "markdown", name, &scm);
    }
}
