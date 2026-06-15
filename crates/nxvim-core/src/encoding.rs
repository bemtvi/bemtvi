//! File encoding (`'fileencoding'`): the charset the bytes on disk are in.
//!
//! nxvim's internal text model is **always UTF-8** (the rope; see
//! `architecture.md` → *Text model*). This type names the *on-disk* form so the
//! read/write seam can transcode between the two — read latin1/utf-16/… into the
//! UTF-8 rope, and write the rope back out in the buffer's encoding. The actual
//! transcoding (and the round-trip-safe handling of undecodable bytes) is wired
//! in later phases; this module is the name/registry layer those phases and the
//! `'fileencoding'` / `'fileencodings'` options build on.
//!
//! Backed by [`encoding_rs`] (the WHATWG Encoding Standard, pure Rust), so label
//! parsing matches what browsers accept: `"latin1"` / `"iso-8859-1"` resolve to
//! `windows-1252` (true ISO-8859-1 is almost never what a file actually is).

use std::fmt;

/// A file encoding — a cheap, copyable handle into `encoding_rs`' static tables.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Encoding(&'static encoding_rs::Encoding);

impl Encoding {
    /// UTF-8 — nxvim's default `'fileencoding'` and the internal rope encoding.
    pub const UTF8: Encoding = Encoding(encoding_rs::UTF_8);

    /// Parse a vim/WHATWG encoding label (`"utf-8"`, `"latin1"`, `"utf-16le"`, …),
    /// or `None` for an unknown label so the caller can fail loud (`E474`). The
    /// `"ucs-bom"` *detection* pseudo-entry of `'fileencodings'` is **not** an
    /// encoding and is rejected here — [`is_fileencodings_entry`] accepts it.
    pub fn from_label(label: &str) -> Option<Encoding> {
        encoding_rs::Encoding::for_label(label.as_bytes()).map(Encoding)
    }

    /// The underlying `encoding_rs` handle, for the decode/encode helpers.
    pub fn inner(self) -> &'static encoding_rs::Encoding {
        self.0
    }
}

impl Default for Encoding {
    fn default() -> Self {
        Encoding::UTF8
    }
}

impl fmt::Display for Encoding {
    /// A vim-style lowercase name (`utf-8`, `latin1`, `utf-16le`), so `:set fenc?`
    /// reads back the way a user would spell it. Encodings without a vim alias
    /// fall back to the lowercased WHATWG canonical name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.name() {
            "UTF-8" => f.write_str("utf-8"),
            "windows-1252" => f.write_str("latin1"),
            "UTF-16LE" => f.write_str("utf-16le"),
            "UTF-16BE" => f.write_str("utf-16be"),
            other => write!(f, "{}", other.to_ascii_lowercase()),
        }
    }
}

impl fmt::Debug for Encoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Encoding({})", self)
    }
}

/// Whether `entry` is valid in a `'fileencodings'` detection list: either the
/// `"ucs-bom"` BOM-sniff pseudo-entry or a real encoding label. A list of these
/// is tried in order on read (wired in a later phase).
pub fn is_fileencodings_entry(entry: &str) -> bool {
    entry == "ucs-bom" || Encoding::from_label(entry).is_some()
}
