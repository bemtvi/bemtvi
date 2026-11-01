//! In-process treesitter for bemtvi.
//!
//! This crate is a plain **library**: it loads **installable** grammars from
//! disk by language (see [`loader`]) and parses **incrementally** (see
//! [`engine`]), exposing a synchronous [`Engine`] the editor owns (via
//! `bemtvi-core`'s `SyntaxEngine` trait) and queries directly, in the same frame
//! as the keypress. There is no transport and no process boundary: a poison
//! grammar can crash the editor (neovim's posture), bounded only by the engine's
//! parse deadline — the tradeoff that buys synchronous highlights and indent.
//!
//! Grammars are loaded at runtime from a data directory laid out like neovim's
//! `runtimepath`, so an existing nvim-treesitter `parser/` + `queries/` tree is
//! drop-in usable.

pub mod engine;
pub mod install;
pub mod loader;
pub mod lua_pattern;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub use engine::{load_requested, Engine};
pub use loader::{Grammar, LoadedLanguage};

/// Resolve bemtvi's data directory (where `parser/` and `queries/` live).
/// `$BEMTVI_DATA_DIR` overrides everything (used by tests); otherwise
/// `stdpath("data")` — [`bemtvi_core::stdpath`] owns that policy, so the grammar
/// root is the same directory `btv.stdpath("data")` reports and plugins install
/// under, on every platform. `None` for a process with no home and no XDG or
/// platform base: grammars are native libraries, and loading them from a directory
/// relative to the working directory (a checked-out project) is not an option.
pub fn data_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("BEMTVI_DATA_DIR") {
        return Some(PathBuf::from(dir));
    }
    bemtvi_core::stdpath::data_dir()
}

/// One installed grammar discovered on the search path (what `:TSInstallInfo`
/// lists).
#[derive(Debug, Clone)]
pub struct InstalledParser {
    /// The grammar / filetype name (the `parser/<lang>.*` stem).
    pub lang: String,
    /// The search root it resolves from — bemtvi's [`data_dir`], or a borrowed
    /// neovim `site/` ([`extra_roots`]).
    pub root: PathBuf,
    /// Query file basenames present under `<root>/queries/<lang>/` (e.g.
    /// `highlights`, `indents`), sorted. Empty means the parser loads but has no
    /// queries (no highlighting / ts-indent).
    pub queries: Vec<String>,
}

/// Enumerate installed parsers across the data-dir search path
/// ([`data_dir`] first, then [`extra_roots`]), in the same precedence
/// [`Engine`] loads them: a grammar present in several roots is reported once,
/// from the first root that has it. Sorted by language. Used by `:TSInstallInfo`.
///
/// A directory that doesn't exist has nothing in it. One that can't be listed is an
/// error naming it, not an empty answer: "not installed" would send a caller to
/// reinstall what's there.
pub fn installed_parsers() -> std::io::Result<Vec<InstalledParser>> {
    let mut roots: Vec<PathBuf> = data_dir().into_iter().collect();
    roots.extend(extra_roots());

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for root in &roots {
        let mut langs = parser_langs_in(root)?;
        langs.sort();
        for lang in langs {
            if !seen.insert(lang.clone()) {
                continue; // an earlier (higher-precedence) root already has it
            }
            let queries = query_names_in(root, &lang)?;
            out.push(InstalledParser {
                lang,
                root: root.clone(),
                queries,
            });
        }
    }
    out.sort_by(|a, b| a.lang.cmp(&b.lang));
    Ok(out)
}

/// The languages with a parser and a `highlights` query to paint with, sorted: what
/// `btv.treesitter.installed` lists. The query can sit beside the parser, in a `queries/`
/// or `after/queries/` of `runtimepath`, or be held in memory (`overridden`, the languages
/// `btv.treesitter.set_query` gave one): the places a highlight resolves from.
///
/// This touches the disk, so it runs off the editor thread.
pub fn highlightable_langs(
    runtimepath: &[PathBuf],
    overridden: &HashSet<String>,
) -> std::io::Result<Vec<String>> {
    let on_runtimepath = |lang: &str| {
        runtimepath.iter().any(|dir| {
            ["queries", "after/queries"]
                .iter()
                .any(|sub| dir.join(sub).join(lang).join("highlights.scm").is_file())
        })
    };
    Ok(installed_parsers()?
        .into_iter()
        .filter(|p| {
            p.queries.iter().any(|q| q == "highlights")
                || overridden.contains(&p.lang)
                || on_runtimepath(&p.lang)
        })
        .map(|p| p.lang)
        .collect())
}

/// The entries of `dir`, with none for a `dir` that doesn't exist. Other failures carry
/// the path.
fn dir_paths(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let named =
        |e: std::io::Error| std::io::Error::new(e.kind(), format!("{}: {e}", dir.display()));
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(named(e)),
    };
    entries
        .map(|entry| entry.map(|e| e.path()).map_err(named))
        .collect()
}

/// The grammar names installed under `<root>/parser/` — files named `<lang>.<ext>`
/// for a loadable extension (`.so` on every OS, plus the platform-native one).
fn parser_langs_in(root: &Path) -> std::io::Result<Vec<String>> {
    let native = loader::native_lib_ext();
    Ok(dir_paths(&root.join("parser"))?
        .iter()
        .filter_map(|path| {
            let ext = path.extension()?.to_str()?;
            if ext != "so" && ext != native {
                return None;
            }
            let lang = path.file_stem()?.to_str()?;
            // A name the engine refuses to load (`foo.old.so`) isn't an installed grammar.
            loader::is_valid_language(lang).then(|| lang.to_string())
        })
        .collect())
}

/// Query file basenames under `<root>/queries/<lang>/` (without the `.scm`),
/// sorted.
fn query_names_in(root: &Path, lang: &str) -> std::io::Result<Vec<String>> {
    let mut names: Vec<String> = dir_paths(&root.join("queries").join(lang))?
        .iter()
        .filter_map(|path| {
            (path.extension()?.to_str()? == "scm")
                .then(|| path.file_stem().and_then(|s| s.to_str()).map(String::from))?
        })
        .collect();
    names.sort();
    Ok(names)
}

/// `$XDG_DATA_HOME` when set to an absolute path. Empty or relative values are
/// ignored (the XDG spec), so a grammar is never loaded from a cwd-relative dir.
fn xdg_data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute())
}

/// Extra, **read-only** roots searched after bemtvi's own [`data_dir`] when
/// resolving a grammar — currently an existing **neovim** install's
/// `site/` (`parser/<lang>.*` + `queries/<lang>/`), so a user who already ran
/// nvim-treesitter gets those grammars for free with no `:TSInstall`. Only
/// existing directories are returned; bemtvi never writes here (installs always
/// land in [`data_dir`]). `$BEMTVI_DATA_DIR` (the test override) suppresses these
/// so fixtures stay hermetic.
pub fn extra_roots() -> Vec<PathBuf> {
    if std::env::var_os("BEMTVI_DATA_DIR").is_some() {
        return Vec::new();
    }
    let mut roots = Vec::new();
    let mut push = |p: PathBuf| {
        if p.is_dir() {
            roots.push(p);
        }
    };
    // Neovim's *own* data dir, not bemtvi's — the same bases (so `$XDG_DATA_HOME`
    // still wins) but neovim's suffix: `nvim`, or `nvim-data` on Windows, where
    // config and data share the one `%LOCALAPPDATA%` root.
    #[cfg(windows)]
    {
        if let Some(dir) = xdg_data_home() {
            push(dir.join("nvim-data").join("site"));
        } else if let Some(dir) = std::env::var_os("LOCALAPPDATA") {
            push(PathBuf::from(dir).join("nvim-data").join("site"));
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(dir) = xdg_data_home() {
            push(dir.join("nvim").join("site"));
        } else if let Some(home) = bemtvi_core::stdpath::home_dir() {
            push(home.join(".local").join("share").join("nvim").join("site"));
        }
    }
    roots
}
