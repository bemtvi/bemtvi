//! What bemtvi says about its version: `--version`, `:version` and the intro screen.
//!
//! A build with the vim regular expression engine (the `vim-regex` feature,
//! `bemtvi-regex`) contains modified Vim code, and the Vim license (II.3) asks for a
//! message saying so "at least in the output of the `:version` command and in the
//! intro screen". [`NOTICES`] is that message, and every surface shows it from here.

/// The notice text, as a macro so [`LONG`] can `concat!` it into a constant.
macro_rules! vim_regex_notice {
    () => {
        "Includes a modified Vim regex engine (Vim license)."
    };
}

/// The version, as `"bemtvi x.y.z"`.
pub const VERSION: &str = concat!("bemtvi ", env!("CARGO_PKG_VERSION"));

/// The notices this build must show beside its version: empty without the vim
/// regular expression engine.
#[cfg(feature = "vim-regex")]
pub const NOTICES: &[&str] = &[vim_regex_notice!()];
#[cfg(not(feature = "vim-regex"))]
pub const NOTICES: &[&str] = &[];

/// The binaries' `--version` text after their name: the version number, then the
/// notices. (`-V` stays the bare number.)
#[cfg(feature = "vim-regex")]
pub const LONG: &str = concat!(env!("CARGO_PKG_VERSION"), "\n\n", vim_regex_notice!());
#[cfg(not(feature = "vim-regex"))]
pub const LONG: &str = env!("CARGO_PKG_VERSION");
