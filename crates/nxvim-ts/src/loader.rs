//! Dynamic grammar loading — the installable-grammar half of the engine.
//!
//! Grammars are **not** linked into nxvim. They are loaded at runtime from a
//! data directory laid out exactly like neovim's `runtimepath`, so an existing
//! nvim-treesitter `parser/` + `queries/` tree is drop-in usable:
//!
//! ```text
//! <data>/parser/<lang>.{so,dylib,dll}    # exports tree_sitter_<lang>()
//! <data>/queries/<lang>/highlights.scm
//! <data>/queries/<lang>/indents.scm      # optional — treesitter indentation
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use tree_sitter::{Language, Parser, Query};

/// The query-text overrides the engine holds, keyed by `(lang, query_name)` —
/// the store [`Grammar::load`] consults in place of the on-disk query, and the
/// landing point of the query-resolution bridge (Lua resolves the merged string,
/// the engine compiles + caches + executes it). Only `highlights` / `indents`
/// (the paint-driving names) are ever present. See
/// [`Engine::set_query`](crate::engine::Engine::set_query).
pub type QueryOverrides = HashMap<(String, String), String>;

/// Why [`Grammar::load`] didn't return a grammar, so the engine (and the editor
/// above it) can stay silent for an uninstalled language but surface a real load
/// failure once.
pub enum LoadError {
    /// No parser library is installed for this language under the data dir. The
    /// expected, silent case — the buffer is simply not highlighted.
    NotInstalled,
    /// A parser **is** installed but couldn't be loaded or used: an invalid
    /// language name, a dlopen failure, a missing symbol, an ABI mismatch, or a
    /// missing/unparseable highlights query. Worth telling the user about.
    Failed(anyhow::Error),
}

/// A loaded grammar: the dynamic library (kept alive because `language` borrows
/// code inside it), the `Language`, the compiled highlights `Query`, and an
/// optional compiled indents `Query` (treesitter indentation; absent when the
/// language ships no `indents.scm`).
pub struct Grammar {
    // Field order matters: `language`/`query`/`indents` drop before `_lib`, so the
    // loaded code outlives anything pointing into it.
    pub language: Language,
    pub query: Query,
    pub indents: Option<Query>,
    _lib: libloading::Library,
}

/// Just the parser half of a grammar: the `Language` and the dynamic library it
/// lives in, with **no** query files required. The `vim.treesitter` Lua platform
/// stands on this — a plugin creates a parser or compiles its own query without
/// the editor's `highlights.scm`/`indents.scm`, which only the [`Grammar`]
/// highlighter needs. The library is held so the language's code outlives every
/// tree and node derived from it (see the Lua binding's lifetime model).
pub struct LoadedLanguage {
    // Field order matters: `language` drops before `_lib`.
    pub language: Language,
    _lib: libloading::Library,
}

impl LoadedLanguage {
    /// Load just the `Language` for `lang` from `data_dir` — the parser library,
    /// dlopen'd and ABI-probed, without touching any query file. Same
    /// missing-vs-broken distinction as [`Grammar::load`].
    pub fn load(data_dir: &Path, lang: &str) -> Result<LoadedLanguage, LoadError> {
        let (language, lib) = open_language(data_dir, lang)?;
        Ok(LoadedLanguage {
            language,
            _lib: lib,
        })
    }
}

impl Grammar {
    /// Load the grammar for `lang` from `data_dir`. Distinguishes a *missing*
    /// parser ([`LoadError::NotInstalled`], silent) from a parser that is present
    /// but broken ([`LoadError::Failed`], worth echoing).
    ///
    /// `overrides` is the query-resolution bridge's store: a `(lang, name)` entry
    /// supplies the query text directly (a `vim.treesitter.query.set` /
    /// `after/queries` / `;extends` merge resolved by Lua) in place of the on-disk
    /// `highlights.scm` / `indents.scm`. With no entry the disk file is read
    /// exactly as before, so the common no-customization case is byte-identical.
    pub fn load(
        data_dir: &Path,
        lang: &str,
        overrides: &QueryOverrides,
    ) -> Result<Grammar, LoadError> {
        let (language, lib) = open_language(data_dir, lang)?;

        let hl_src = match overrides.get(&(lang.to_string(), "highlights".to_string())) {
            Some(text) => text.clone(),
            None => {
                let hl_path = query_path(data_dir, lang, "highlights.scm");
                std::fs::read_to_string(&hl_path)
                    .with_context(|| format!("reading {}", hl_path.display()))
                    .map_err(LoadError::Failed)?
            }
        };
        let query = Query::new(&language, &hl_src)
            .with_context(|| format!("compiling {lang} highlights"))
            .map_err(LoadError::Failed)?;

        // `indents.scm` is optional: a language with no indent query simply has no
        // treesitter indentation (the editor falls back). A *present* file that
        // fails to compile is a real error, surfaced like a broken highlights query.
        let indents = match overrides.get(&(lang.to_string(), "indents".to_string())) {
            Some(text) => Some(
                Query::new(&language, text)
                    .with_context(|| format!("compiling {lang} indents"))
                    .map_err(LoadError::Failed)?,
            ),
            None => {
                let indents_path = query_path(data_dir, lang, "indents.scm");
                match std::fs::read_to_string(&indents_path) {
                    Ok(src) => Some(
                        Query::new(&language, &src)
                            .with_context(|| format!("compiling {lang} indents"))
                            .map_err(LoadError::Failed)?,
                    ),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        return Err(LoadError::Failed(
                            anyhow::Error::new(e)
                                .context(format!("reading {}", indents_path.display())),
                        ))
                    }
                }
            }
        };

        Ok(Grammar {
            language,
            query,
            indents,
            _lib: lib,
        })
    }
}

/// dlopen the parser library for `lang` under `data_dir`, resolve its
/// `tree_sitter_<lang>` export, and ABI-probe it — returning the `Language` and
/// the library that must outlive it. The shared core of [`Grammar::load`] and
/// [`LoadedLanguage::load`]; the only place that executes installed native code.
fn open_language(
    data_dir: &Path,
    lang: &str,
) -> Result<(Language, libloading::Library), LoadError> {
    // Security boundary: `lang` flows into the parser `.so` path and the query
    // directory, and we then `dlopen` that path — i.e. execute native code. A
    // name containing `.`, `/`, `\`, or path components could escape `data_dir`
    // (traversal / absolute path) and load an arbitrary shared object. Reject
    // anything that isn't a plain grammar identifier before touching the
    // filesystem. Callers only ever *should* pass names from the fixed filetype
    // table (or a plugin's `get_parser(lang)`), but must not assume that.
    if !is_valid_language(lang) {
        return Err(LoadError::Failed(anyhow!("invalid language name '{lang}'")));
    }

    // No parser file at all is the common, expected case — not a failure.
    let Some(lib_path) = parser_path(data_dir, lang) else {
        return Err(LoadError::NotInstalled);
    };

    // SAFETY: loading arbitrary native code is inherently unsafe. A poison
    // grammar can segfault the process (neovim's posture); the ABI probe below
    // is the load-time mitigation.
    let lib = unsafe { libloading::Library::new(&lib_path) }
        .with_context(|| format!("dlopen {}", lib_path.display()))
        .map_err(LoadError::Failed)?;

    let symbol = format!("tree_sitter_{}", lang.replace('-', "_"));
    let language = unsafe {
        let func: libloading::Symbol<unsafe extern "C" fn() -> *const ()> = lib
            .get(symbol.as_bytes())
            .with_context(|| format!("symbol {symbol} in {}", lib_path.display()))
            .map_err(LoadError::Failed)?;
        Language::from_raw(func() as *const _)
    };

    // Validate the grammar ABI against our tree-sitter before trusting it.
    let mut probe = Parser::new();
    probe
        .set_language(&language)
        .with_context(|| format!("grammar '{lang}' ABI incompatible"))
        .map_err(LoadError::Failed)?;

    Ok((language, lib))
}

/// A grammar identifier: non-empty and only ASCII letters, digits, `_` or `-`
/// (e.g. `rust`, `c`, `cpp`, `c_sharp`, `tsx`). Excluding `.`, `/`, `\` and the
/// empty string is what makes path traversal and absolute-path escapes
/// impossible when the name is joined into `data_dir`.
fn is_valid_language(lang: &str) -> bool {
    !lang.is_empty()
        && lang
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// First existing `parser/<lang>.<ext>` over the platform's candidate
/// extensions. `.so` is tried first on every OS because nvim-treesitter names
/// its parsers `<lang>.so` even on macOS.
fn parser_path(data_dir: &Path, lang: &str) -> Option<PathBuf> {
    let dir = data_dir.join("parser");
    let native = if cfg!(target_os = "windows") {
        "dll"
    } else if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    ["so", native]
        .into_iter()
        .map(|ext| dir.join(format!("{lang}.{ext}")))
        .find(|p| p.exists())
}

pub(crate) fn query_path(data_dir: &Path, lang: &str, file: &str) -> PathBuf {
    data_dir.join("queries").join(lang).join(file)
}
