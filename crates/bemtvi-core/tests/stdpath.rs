//! Where bemtvi's per-user files land — [`bemtvi_core::stdpath`], the one place the
//! policy lives.
//!
//! The regression this guards is a Windows install writing its settings, shada and
//! plugins into whatever directory the editor was launched from, and never finding
//! `init.lua`. Every std dir used to be re-derived at its own call site from
//! `$HOME`, which Windows does not set; each site then fell back to a *relative*
//! directory (`"bemtvi"`, `".bemtvi"`) or, for config discovery, to `None` — "there
//! is no config". So the invariants asserted here are: a std dir is **absolute or
//! absent**, never quietly relative, and the four dirs are distinct so data can't
//! land on top of hand-edited config.
//!
//! Its own test binary: these variables are process-global, so the cases run
//! serially within it (an [`Env`] holds a lock for its whole scope) and nothing
//! else may read the environment alongside them.

use bemtvi_core::stdpath;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Serializes the cases: `set_var` is process-global, so two cases building
/// different environments at once would each see the other's. Held for the whole
/// lifetime of an [`Env`] — i.e. the whole case, restore included.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Every environment variable the resolution consults, saved on construction and
/// put back on drop, so a case can build an environment from a known-empty slate.
struct Env {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _lock: MutexGuard<'static, ()>,
}

const VARS: &[&str] = &[
    "BEMTVI_CONFIG",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "HOME",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "LOCALAPPDATA",
    "TEMP",
];

impl Env {
    /// Snapshot every variable and clear the lot — the slate each case builds on.
    fn cleared() -> Self {
        let _lock = env_lock();
        let saved = VARS
            .iter()
            .map(|&v| (v, std::env::var_os(v)))
            .collect::<Vec<_>>();
        for &v in VARS {
            std::env::remove_var(v);
        }
        Self { saved, _lock }
    }

    fn set(&self, var: &str, value: &str) {
        std::env::set_var(var, value);
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for (var, value) in &self.saved {
            match value {
                Some(v) => std::env::set_var(var, v),
                None => std::env::remove_var(var),
            }
        }
    }
}

/// The home a case installs, in the shape the running platform actually uses — so
/// the assertions below describe one policy rather than two.
fn set_home(env: &Env, home: &str) {
    #[cfg(windows)]
    {
        env.set("USERPROFILE", home);
        env.set("LOCALAPPDATA", &format!("{home}\\AppData\\Local"));
    }
    #[cfg(not(windows))]
    env.set("HOME", home);
}

/// A plausible absolute home for the running platform (never touched on disk —
/// these functions only do string work).
fn a_home() -> &'static str {
    if cfg!(windows) {
        "C:\\Users\\tester"
    } else {
        "/home/tester"
    }
}

fn all_dirs() -> Vec<(&'static str, Option<PathBuf>)> {
    vec![
        ("config", stdpath::config_dir()),
        ("data", stdpath::data_dir()),
        ("state", stdpath::state_dir()),
        ("cache", stdpath::cache_dir()),
    ]
}

#[test]
fn every_std_dir_is_absolute_when_a_home_exists() {
    let env = Env::cleared();
    set_home(&env, a_home());

    for (what, dir) in all_dirs() {
        let dir = dir.unwrap_or_else(|| {
            panic!(
                "stdpath::{what}_dir() resolved to nothing \
             even though the platform's home variable is set"
            )
        });
        assert!(
            dir.is_absolute(),
            "stdpath::{what}_dir() returned the relative path {dir:?} — settings would \
             be written under whatever directory the editor was launched from",
        );
        assert!(
            dir.starts_with(a_home()),
            "stdpath::{what}_dir() ({dir:?}) is not under the home it was given",
        );
    }
}

#[test]
fn config_never_shares_a_directory_with_managed_data() {
    let env = Env::cleared();
    set_home(&env, a_home());

    let config = stdpath::config_dir().expect("a config dir");
    for (what, dir) in all_dirs() {
        if what == "config" {
            continue;
        }
        let dir = dir.expect("a std dir");
        assert_ne!(
            dir, config,
            "stdpath::{what}_dir() is the config dir — installed plugins / shada would \
             be written into the tree the user hand-edits and version-controls",
        );
    }
}

#[test]
fn xdg_overrides_win_on_every_platform() {
    let env = Env::cleared();
    set_home(&env, a_home());
    // Neovim honors the XDG variables on Windows too, and the test suites lean on
    // them for hermetic runs — so they must win over the platform default.
    let root = if cfg!(windows) { "C:\\xdg" } else { "/xdg" };
    env.set(
        "XDG_CONFIG_HOME",
        &format!("{root}{sep}c", sep = std::path::MAIN_SEPARATOR),
    );
    env.set(
        "XDG_DATA_HOME",
        &format!("{root}{sep}d", sep = std::path::MAIN_SEPARATOR),
    );
    env.set(
        "XDG_STATE_HOME",
        &format!("{root}{sep}s", sep = std::path::MAIN_SEPARATOR),
    );
    env.set(
        "XDG_CACHE_HOME",
        &format!("{root}{sep}k", sep = std::path::MAIN_SEPARATOR),
    );

    for (what, dir) in all_dirs() {
        let dir = dir.expect("a std dir");
        assert!(
            dir.starts_with(root),
            "stdpath::{what}_dir() ({dir:?}) ignored its XDG_* override",
        );
        assert_eq!(
            dir.parent().map(PathBuf::from),
            Some(PathBuf::from(format!(
                "{root}{sep}{}",
                match what {
                    "config" => "c",
                    "data" => "d",
                    "state" => "s",
                    _ => "k",
                },
                sep = std::path::MAIN_SEPARATOR
            ))),
            "stdpath::{what}_dir() took the wrong XDG base",
        );
    }
}

#[test]
fn bemtvi_config_overrides_everything() {
    let env = Env::cleared();
    set_home(&env, a_home());
    let explicit = if cfg!(windows) {
        "C:\\somewhere\\else"
    } else {
        "/somewhere/else"
    };
    env.set(
        "XDG_CONFIG_HOME",
        if cfg!(windows) { "C:\\xdg" } else { "/xdg" },
    );
    env.set("BEMTVI_CONFIG", explicit);

    assert_eq!(
        stdpath::config_dir(),
        Some(PathBuf::from(explicit)),
        "$BEMTVI_CONFIG is the documented first entry in the discovery order",
    );
    // It scopes *config* only — pointing an example or a test at a throwaway config
    // tree must not redirect where plugins and shada are managed.
    assert!(
        !stdpath::data_dir().unwrap().starts_with(explicit),
        "$BEMTVI_CONFIG must not move the data dir",
    );
}

#[test]
fn an_empty_variable_counts_as_unset() {
    let env = Env::cleared();
    set_home(&env, a_home());
    // Windows hands out empty strings for variables that were never really set (a
    // service account with no profile, a stripped CI environment).
    // `PathBuf::from("")` is *relative*, so taking it at face value puts the config
    // dir at `bemtvi/` under the cwd — the original bug, wearing a different hat.
    env.set("XDG_CONFIG_HOME", "");
    env.set("BEMTVI_CONFIG", "");

    let config = stdpath::config_dir().expect("a config dir despite the empty vars");
    assert!(
        config.is_absolute() && config.starts_with(a_home()),
        "an empty XDG_CONFIG_HOME / BEMTVI_CONFIG must fall through to the platform \
         default, not resolve against the cwd (got {config:?})",
    );
}

#[test]
fn no_home_and_no_bases_resolves_to_nothing_rather_than_a_relative_path() {
    let _env = Env::cleared();
    // The contract that lets each caller decide *loudly* what to do (the remote
    // config cache errors; `btv.stdpath` falls back to a documented `.bemtvi`).
    // Returning a relative path here is what silently scattered state into the cwd.
    for (what, dir) in all_dirs() {
        assert_eq!(
            dir, None,
            "with no home and no platform base, stdpath::{what}_dir() must resolve to \
             nothing rather than a cwd-relative path",
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_defaults_to_localappdata_and_separates_data_from_config() {
    let env = Env::cleared();
    env.set("USERPROFILE", "C:\\Users\\tester");
    env.set("LOCALAPPDATA", "C:\\Users\\tester\\AppData\\Local");
    env.set("TEMP", "C:\\Temp");

    assert_eq!(
        stdpath::config_dir(),
        Some(PathBuf::from("C:\\Users\\tester\\AppData\\Local\\bemtvi")),
    );
    assert_eq!(
        stdpath::data_dir(),
        Some(PathBuf::from(
            "C:\\Users\\tester\\AppData\\Local\\bemtvi-data"
        )),
    );
    assert_eq!(
        stdpath::state_dir(),
        Some(PathBuf::from(
            "C:\\Users\\tester\\AppData\\Local\\bemtvi-data"
        )),
    );
    assert_eq!(
        stdpath::cache_dir(),
        Some(PathBuf::from("C:\\Temp\\bemtvi-data")),
    );
}

#[cfg(windows)]
#[test]
fn windows_home_comes_from_the_profile_variables() {
    let env = Env::cleared();
    // `$HOME` is not a Windows variable; `~` has to resolve without it.
    env.set("USERPROFILE", "C:\\Users\\tester");
    assert_eq!(
        stdpath::home_dir(),
        Some(PathBuf::from("C:\\Users\\tester")),
    );

    // The legacy domain-login pair, concatenated (HOMEPATH is drive-relative).
    std::env::remove_var("USERPROFILE");
    env.set("HOMEDRIVE", "C:");
    env.set("HOMEPATH", "\\Users\\tester");
    assert_eq!(
        stdpath::home_dir(),
        Some(PathBuf::from("C:\\Users\\tester")),
    );
}

#[cfg(not(windows))]
#[test]
fn unix_defaults_to_the_xdg_layout_under_home() {
    let env = Env::cleared();
    env.set("HOME", "/home/tester");

    assert_eq!(
        stdpath::config_dir(),
        Some(PathBuf::from("/home/tester/.config/bemtvi")),
    );
    assert_eq!(
        stdpath::data_dir(),
        Some(PathBuf::from("/home/tester/.local/share/bemtvi")),
    );
    assert_eq!(
        stdpath::state_dir(),
        Some(PathBuf::from("/home/tester/.local/state/bemtvi")),
    );
    assert_eq!(
        stdpath::cache_dir(),
        Some(PathBuf::from("/home/tester/.cache/bemtvi")),
    );
}
