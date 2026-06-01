//! Dynamic grammar loading — the installable-grammar half of the worker.
//!
//! Grammars are **not** linked into nxvim. They are loaded at runtime from a
//! data directory laid out exactly like neovim's `runtimepath`, so an existing
//! nvim-treesitter `parser/` + `queries/` tree is drop-in usable:
//!
//! ```text
//! <data>/parser/<lang>.{so,dylib,dll}    # exports tree_sitter_<lang>()
//! <data>/queries/<lang>/highlights.scm
//! ```

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use tree_sitter::{Language, Parser, Query};

/// A loaded grammar: the dynamic library (kept alive because `language` borrows
/// code inside it), the `Language`, and the compiled highlights `Query`.
pub struct Grammar {
    // Field order matters: `language`/`query` drop before `_lib`, so the loaded
    // code outlives anything pointing into it.
    pub language: Language,
    pub query: Query,
    _lib: libloading::Library,
}

impl Grammar {
    /// Load the grammar for `lang` from `data_dir`. Returns an error (rather than
    /// panicking) for a missing parser, a missing symbol, an ABI mismatch, or an
    /// unparseable query — the worker turns that into a `ts_error` and moves on.
    pub fn load(data_dir: &Path, lang: &str) -> Result<Grammar> {
        let lib_path = parser_path(data_dir, lang)
            .ok_or_else(|| anyhow!("no parser for '{lang}' under {}", data_dir.display()))?;

        // SAFETY: loading arbitrary native code is inherently unsafe; crash
        // isolation is provided by running this in a separate process.
        let lib = unsafe { libloading::Library::new(&lib_path) }
            .with_context(|| format!("dlopen {}", lib_path.display()))?;

        let symbol = format!("tree_sitter_{}", lang.replace('-', "_"));
        let language = unsafe {
            let func: libloading::Symbol<unsafe extern "C" fn() -> *const ()> = lib
                .get(symbol.as_bytes())
                .with_context(|| format!("symbol {symbol} in {}", lib_path.display()))?;
            Language::from_raw(func() as *const _)
        };

        // Validate the grammar ABI against our tree-sitter before trusting it.
        let mut probe = Parser::new();
        probe
            .set_language(&language)
            .with_context(|| format!("grammar '{lang}' ABI incompatible"))?;

        let hl_path = query_path(data_dir, lang, "highlights.scm");
        let hl_src = std::fs::read_to_string(&hl_path)
            .with_context(|| format!("reading {}", hl_path.display()))?;
        let query = Query::new(&language, &hl_src)
            .with_context(|| format!("compiling {lang} highlights"))?;

        Ok(Grammar {
            language,
            query,
            _lib: lib,
        })
    }
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

fn query_path(data_dir: &Path, lang: &str, file: &str) -> PathBuf {
    data_dir.join("queries").join(lang).join(file)
}
