//! Where bemtvi's per-user files live — the single place the platform's directory
//! policy is written down.
//!
//! Config, plugins, shada, grammars and logs are all resolved from here, by every
//! crate that needs one (`bemtvi-lua`'s `btv.stdpath`, the server's config
//! discovery and shada dir, `bemtvi-ts`'s grammar root, the LSP log, the GUI's `~`
//! expansion). It exists because the policy used to be re-derived at a dozen call
//! sites, each reading `$HOME` directly — which resolves to nothing on Windows,
//! where the variable simply isn't set. Every one of those sites then fell back to
//! a *relative* directory, so a Windows install wrote its settings, shada and
//! plugins into whatever directory the editor happened to be launched from, and
//! never found `init.lua` at all.
//!
//! The layout follows neovim's, so a vim user's muscle memory for "where does my
//! config go" holds:
//!
//! ```text
//!            XDG override      unix default              windows default
//! config     $XDG_CONFIG_HOME  ~/.config/bemtvi          %LOCALAPPDATA%\bemtvi
//! data       $XDG_DATA_HOME    ~/.local/share/bemtvi     %LOCALAPPDATA%\bemtvi-data
//! state      $XDG_STATE_HOME   ~/.local/state/bemtvi     %LOCALAPPDATA%\bemtvi-data
//! cache      $XDG_CACHE_HOME   ~/.cache/bemtvi           %TEMP%\bemtvi-data
//! ```
//!
//! The `XDG_*` variables win on **every** platform (neovim honors them on Windows
//! too, and the test suites lean on them for hermetic runs); the Windows column is
//! only what they default to. Windows keeps hand-edited config and managed data
//! apart with a `-data` suffix because both would otherwise land in the one
//! `%LOCALAPPDATA%` root — the split neovim spells `nvim` / `nvim-data`.

use std::path::PathBuf;

/// The suffix the *managed* dirs (data, state, cache) take — see the module docs
/// for why Windows separates them from config by name rather than by base dir.
#[cfg(windows)]
const DATA_SUFFIX: &str = "bemtvi-data";
#[cfg(not(windows))]
const DATA_SUFFIX: &str = "bemtvi";

/// The suffix the config dir takes. Never `-data`-suffixed: this is the directory
/// a user hand-edits and puts under version control.
const CONFIG_SUFFIX: &str = "bemtvi";

/// An environment variable's value, treating **empty as unset**. Windows hands out
/// empty strings for variables that were never really set (a service account with
/// no profile, a stripped CI environment), and `PathBuf::from("")` is a relative
/// path that silently resolves against the cwd — the exact failure this module
/// exists to prevent.
fn non_empty(var: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(var).filter(|v| !v.is_empty())
}

/// The user's home directory: `$HOME`, else — on Windows, which does not set it —
/// `%USERPROFILE%`, else `%HOMEDRIVE%%HOMEPATH%`. `None` when the process has no
/// home at all, which callers must handle rather than substituting a relative path.
///
/// This is what a leading `~` expands against, and what the XDG defaults below are
/// rooted at.
pub fn home_dir() -> Option<PathBuf> {
    if let Some(home) = non_empty("HOME") {
        return Some(PathBuf::from(home));
    }
    #[cfg(windows)]
    {
        if let Some(profile) = non_empty("USERPROFILE") {
            return Some(PathBuf::from(profile));
        }
        // The legacy pair, still set by domain logins. `HOMEPATH` is root-relative
        // (`\Users\dave`), so it is *concatenated* onto the drive — joining it as a
        // path component would discard the drive and leave a rootless path.
        if let (Some(drive), Some(path)) = (non_empty("HOMEDRIVE"), non_empty("HOMEPATH")) {
            let mut joined = drive;
            joined.push(path);
            return Some(PathBuf::from(joined));
        }
    }
    None
}

/// The base directory a std dir hangs off: `$<xdg_var>` if set, else the Windows
/// default `%<win_var>%`, else `$HOME/<unix_sub…>`.
fn base(xdg_var: &str, win_var: &str, unix_sub: &[&str]) -> Option<PathBuf> {
    if let Some(dir) = non_empty(xdg_var) {
        return Some(PathBuf::from(dir));
    }
    #[cfg(windows)]
    if let Some(dir) = non_empty(win_var) {
        return Some(PathBuf::from(dir));
    }
    #[cfg(not(windows))]
    let _ = win_var;
    // Also the Windows last resort when `%LOCALAPPDATA%` / `%TEMP%` are missing: a
    // dotted dir under the profile beats no directory at all.
    let mut dir = home_dir()?;
    for part in unix_sub {
        dir.push(part);
    }
    Some(dir)
}

/// bemtvi's config directory — where `init.lua` and the runtimepath root live.
///
/// `$BEMTVI_CONFIG` overrides everything (the documented discovery order, and how
/// `examples/` and the test suites point the editor at a throwaway tree); otherwise
/// `$XDG_CONFIG_HOME/bemtvi`, else the platform default.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = non_empty("BEMTVI_CONFIG") {
        return Some(PathBuf::from(dir));
    }
    base("XDG_CONFIG_HOME", "LOCALAPPDATA", &[".config"]).map(|b| b.join(CONFIG_SUFFIX))
}

/// bemtvi's data directory — managed artifacts: installed plugins, tree-sitter
/// grammars and their queries, the system-plugin tier.
pub fn data_dir() -> Option<PathBuf> {
    base("XDG_DATA_HOME", "LOCALAPPDATA", &[".local", "share"]).map(|b| b.join(DATA_SUFFIX))
}

/// bemtvi's state directory — things regenerated across sessions rather than
/// installed: shada, logs.
pub fn state_dir() -> Option<PathBuf> {
    base("XDG_STATE_HOME", "LOCALAPPDATA", &[".local", "state"]).map(|b| b.join(DATA_SUFFIX))
}

/// bemtvi's cache directory — discardable scratch (the staged remote config).
pub fn cache_dir() -> Option<PathBuf> {
    base("XDG_CACHE_HOME", "TEMP", &[".cache"]).map(|b| b.join(DATA_SUFFIX))
}
