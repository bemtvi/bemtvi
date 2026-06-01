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
}

impl Default for Options {
    fn default() -> Self {
        // nxvim ships with the hybrid number column on: the cursor line shows
        // its document line number, every other line shows its distance from
        // the cursor.
        Options {
            number: true,
            relativenumber: true,
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
        _ => None,
    }
}
