//! Editor options (`:set ...`), the rust-native analogue of neovim's
//! `option.c`. Kept deliberately small for now — only the options nxvim
//! actually honors live here, and they grow alongside the features that read
//! them.

/// Window/buffer options that affect rendering and editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Show the absolute line number in the number column.
    pub number: bool,
    /// Show line numbers relative to the cursor line. Combined with
    /// [`Options::number`] this gives vim's "hybrid" gutter: the absolute
    /// number on the cursor line, relative numbers elsewhere.
    pub relativenumber: bool,
    /// Ignore case when searching (`/`, `?`, `n`, `N`).
    pub ignorecase: bool,
    /// Override [`Options::ignorecase`] for a pattern that contains an uppercase
    /// character, making such a search case-sensitive. Only consulted when
    /// `ignorecase` is on (vim's `smartcase`).
    pub smartcase: bool,
    /// Wrap searches around the ends of the buffer (vim's `wrapscan`). When off,
    /// a forward search past the last match fails with `E385` rather than
    /// continuing from the top (and `E384` for backward).
    pub wrapscan: bool,
    /// Highlight all matches of the last search pattern. (Honored in a later
    /// phase; stored here so `:set` accepts it now.)
    pub hlsearch: bool,
    /// Preview the match incrementally while typing the search. (Honored in a
    /// later phase; stored here so `:set` accepts it now.)
    pub incsearch: bool,
}

impl Default for Options {
    fn default() -> Self {
        // nxvim ships with the hybrid number column on: the cursor line shows
        // its document line number, every other line shows its distance from
        // the cursor.
        Options {
            number: true,
            relativenumber: true,
            // Search defaults match modern neovim: case-sensitive unless asked
            // otherwise, but wrapping, highlighting, and incremental preview on.
            ignorecase: false,
            smartcase: false,
            wrapscan: true,
            hlsearch: true,
            incsearch: true,
        }
    }
}

/// What a `:set` token does to a boolean option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    On,
    Off,
    Toggle,
    Query,
}

/// Resolve a single `:set` token (e.g. `number`, `nonu`, `rnu!`, `invnumber`,
/// `number?`) into the canonical option name and the operation it requests.
/// Returns `None` for unknown options.
///
/// The canonical name is resolved *before* the `no`/`inv` prefixes are tried,
/// so a real option name that happens to start with `no` (none yet, but vim has
/// them) is never mis-parsed as a negation.
pub fn resolve_set(tok: &str) -> Option<(&'static str, SetOp)> {
    if let Some(name) = tok.strip_suffix('?') {
        return canonical(name).map(|c| (c, SetOp::Query));
    }
    if let Some(name) = tok.strip_suffix('!') {
        return canonical(name).map(|c| (c, SetOp::Toggle));
    }
    if let Some(c) = canonical(tok) {
        return Some((c, SetOp::On));
    }
    if let Some(name) = tok.strip_prefix("no") {
        return canonical(name).map(|c| (c, SetOp::Off));
    }
    if let Some(name) = tok.strip_prefix("inv") {
        return canonical(name).map(|c| (c, SetOp::Toggle));
    }
    None
}

/// Map an option name or its standard abbreviation to its canonical spelling.
fn canonical(name: &str) -> Option<&'static str> {
    match name {
        "number" | "nu" => Some("number"),
        "relativenumber" | "rnu" => Some("relativenumber"),
        "ignorecase" | "ic" => Some("ignorecase"),
        "smartcase" | "scs" => Some("smartcase"),
        "wrapscan" | "ws" => Some("wrapscan"),
        "hlsearch" | "hls" => Some("hlsearch"),
        "incsearch" | "is" => Some("incsearch"),
        _ => None,
    }
}
