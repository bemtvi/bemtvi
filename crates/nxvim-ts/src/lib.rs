//! In-process treesitter for nxvim.
//!
//! This crate is a plain **library**: it loads **installable** grammars from
//! disk by language (see [`loader`]) and parses **incrementally** (see
//! [`engine`]), exposing a synchronous [`Engine`] the editor owns (via
//! `nxvim-core`'s `SyntaxEngine` trait) and queries directly, in the same frame
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

/// Resolve nxvim's data directory (where `parser/` and `queries/` live).
/// `$NXVIM_DATA_DIR` overrides everything (used by tests); otherwise the
/// platform's standard per-user data location, suffixed `nxvim`.
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("NXVIM_DATA_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    if let Ok(dir) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(dir).join("nxvim");
    }
    #[cfg(not(windows))]
    {
        if let Ok(dir) = std::env::var("XDG_DATA_HOME") {
            return PathBuf::from(dir).join("nxvim");
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(".local/share/nxvim");
        }
    }
    PathBuf::from(".nxvim")
}

/// One installed grammar discovered on the search path (what `:TSInstallInfo`
/// lists).
#[derive(Debug, Clone)]
pub struct InstalledParser {
    /// The grammar / filetype name (the `parser/<lang>.*` stem).
    pub lang: String,
    /// The search root it resolves from — nxvim's [`data_dir`], or a borrowed
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
pub fn installed_parsers() -> Vec<InstalledParser> {
    let mut roots = vec![data_dir()];
    roots.extend(extra_roots());

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for root in &roots {
        let mut langs = parser_langs_in(root);
        langs.sort();
        for lang in langs {
            if !seen.insert(lang.clone()) {
                continue; // an earlier (higher-precedence) root already has it
            }
            let queries = query_names_in(root, &lang);
            out.push(InstalledParser {
                lang,
                root: root.clone(),
                queries,
            });
        }
    }
    out.sort_by(|a, b| a.lang.cmp(&b.lang));
    out
}

/// The grammar names installed under `<root>/parser/` — files named `<lang>.<ext>`
/// for a loadable extension (`.so` on every OS, plus the platform-native one).
fn parser_langs_in(root: &Path) -> Vec<String> {
    let native = loader::native_lib_ext();
    let Ok(entries) = std::fs::read_dir(root.join("parser")) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| {
            let path = e.ok()?.path();
            let ext = path.extension()?.to_str()?;
            if ext != "so" && ext != native {
                return None;
            }
            Some(path.file_stem()?.to_str()?.to_string())
        })
        .collect()
}

/// Query file basenames under `<root>/queries/<lang>/` (without the `.scm`),
/// sorted.
fn query_names_in(root: &Path, lang: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root.join("queries").join(lang)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| {
            let path = e.ok()?.path();
            (path.extension()?.to_str()? == "scm")
                .then(|| path.file_stem().and_then(|s| s.to_str()).map(String::from))?
        })
        .collect();
    names.sort();
    names
}

/// Extra, **read-only** roots searched after nxvim's own [`data_dir`] when
/// resolving a grammar — currently an existing **neovim** install's
/// `site/` (`parser/<lang>.*` + `queries/<lang>/`), so a user who already ran
/// nvim-treesitter gets those grammars for free with no `:TSInstall`. Only
/// existing directories are returned; nxvim never writes here (installs always
/// land in [`data_dir`]). `$NXVIM_DATA_DIR` (the test override) suppresses these
/// so fixtures stay hermetic.
pub fn extra_roots() -> Vec<PathBuf> {
    if std::env::var_os("NXVIM_DATA_DIR").is_some() {
        return Vec::new();
    }
    let mut roots = Vec::new();
    let mut push = |p: PathBuf| {
        if p.is_dir() {
            roots.push(p);
        }
    };
    #[cfg(windows)]
    if let Some(dir) = std::env::var_os("LOCALAPPDATA") {
        push(PathBuf::from(dir).join("nvim-data").join("site"));
    }
    #[cfg(not(windows))]
    {
        if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
            push(PathBuf::from(dir).join("nvim").join("site"));
        } else if let Some(home) = std::env::var_os("HOME") {
            push(PathBuf::from(home).join(".local/share/nvim/site"));
        }
    }
    roots
}
