//! `file://` URI ⇄ path conversion that respects **whose** path it is.
//!
//! A local session's paths follow this process's own rules, so `Url::from_file_path` /
//! `to_file_path` are exactly right. A daemon session's paths are the *daemon's*, spelled
//! in its announced [`PathStyle`] — and `url`'s helpers apply the *client's* rules, so on a
//! Windows client `Url::from_file_path("/srv/x")` fails outright (no drive) and a
//! `file:///srv/x` the server hands back has no Windows path. Every LSP path ⇄ URI hop goes
//! through these two functions with the session's remote style (`None` = local).

use std::path::{Path, PathBuf};

use bemtvi_core::pathstyle::PathStyle;
use lsp_types::Url;

/// The `file://` URI for the absolute `path`. `style` is the remote filesystem's path
/// syntax in a daemon session, `None` for a local one (`Url::from_file_path`). `None`
/// when no URI can be formed (a relative path).
pub fn path_to_uri(path: &Path, style: Option<PathStyle>) -> Option<Url> {
    match style {
        None => Url::from_file_path(path).ok(),
        Some(style) => Url::parse(&style.to_file_uri(&path.to_string_lossy())?).ok(),
    }
}

/// The path a `file://` URI names — the inverse of [`path_to_uri`] — or `None` for a
/// non-file URI. A remote path comes back as the daemon's string, held verbatim in a
/// `PathBuf` (never re-interpreted by this process's `std::path`).
pub fn uri_to_path(uri: &Url, style: Option<PathStyle>) -> Option<PathBuf> {
    match style {
        None => uri.to_file_path().ok(),
        Some(style) => {
            if uri.scheme() != "file" {
                return None;
            }
            style.from_file_uri(uri.as_str()).map(PathBuf::from)
        }
    }
}
