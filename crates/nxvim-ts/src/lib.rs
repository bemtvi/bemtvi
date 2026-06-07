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
pub mod loader;

use std::path::PathBuf;

pub use engine::Engine;
pub use loader::Grammar;

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
