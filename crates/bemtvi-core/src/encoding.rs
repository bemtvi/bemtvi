//! File encoding (`'fileencoding'`): the charset the bytes on disk are in.
//!
//! bemtvi's internal text model is **always UTF-8** (the rope; see
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

use anyhow::{bail, Result};
use std::fmt;

/// The default `'fileencodings'`: BOM sniff, then strict UTF-8, then latin1 as the
/// always-succeeds terminal fallback. (neovim's is `ucs-bom,utf-8,default,latin1`;
/// `default` is its locale guess, which bemtvi folds into the `latin1` fallback.)
/// The single source for the option default ([`crate::options`]) and the buffer
/// constructors that read a file before any `'fileencodings'` is configured.
pub const DEFAULT_FILEENCODINGS: &str = "ucs-bom,utf-8,latin1";

/// A file encoding — a cheap, copyable handle into `encoding_rs`' static tables.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Encoding(&'static encoding_rs::Encoding);

impl Encoding {
    /// UTF-8 — bemtvi's default `'fileencoding'` and the internal rope encoding.
    pub const UTF8: Encoding = Encoding(encoding_rs::UTF_8);

    /// `latin1` — resolved (browser-style) to `windows-1252`. This is the terminal
    /// fallback of [`decode_to_rope`]: `encoding_rs`'s windows-1252 is a *total,
    /// bijective* single-byte codec (all 256 byte values decode to distinct scalars
    /// and encode back exactly — even the historically-undefined `0x81`/`0x8d`/… map
    /// to pass-through C1 controls), so it opens *any* byte stream and reproduces it
    /// byte-for-byte on write. That totality is what makes invalid-UTF-8 files
    /// round-trip safely (see [`decode_to_rope`]).
    pub const LATIN1: Encoding = Encoding(encoding_rs::WINDOWS_1252);

    /// Parse a vim/WHATWG encoding label (`"utf-8"`, `"latin1"`, `"utf-16le"`,
    /// `"shift_jis"`, `"cp932"`, …), or `None` for an unknown label so the caller can
    /// fail loud (`E474`). The `"ucs-bom"` *detection* pseudo-entry of
    /// `'fileencodings'` is **not** an encoding and is rejected here —
    /// [`is_fileencodings_entry`] accepts it.
    ///
    /// vim's muscle-memory CJK codepage spellings (`cp932`/`cp936`/`cp949`/`cp950`,
    /// `euc-cn`) aren't WHATWG labels, so they're aliased to the equivalent
    /// `encoding_rs` codec via [`vim_cjk_alias`]. The WHATWG `replacement` codec is
    /// rejected: it decodes any input to a single `U+FFFD` (pure data loss), so it's
    /// never a real `'fileencoding'` and accepting it would silently destroy a buffer.
    pub fn from_label(label: &str) -> Option<Encoding> {
        let normalized = label.trim().to_ascii_lowercase();
        let resolved = vim_cjk_alias(&normalized).unwrap_or(label);
        let enc = encoding_rs::Encoding::for_label(resolved.as_bytes())?;
        if enc == encoding_rs::REPLACEMENT {
            return None;
        }
        Some(Encoding(enc))
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
/// is tried in order on read by [`decode_to_rope`].
pub fn is_fileencodings_entry(entry: &str) -> bool {
    entry == "ucs-bom" || Encoding::from_label(entry).is_some()
}

/// Decode raw file `bytes` into the UTF-8 text the rope holds, choosing the encoding
/// by trying each `'fileencodings'` entry (a comma-separated list) in order — the
/// read half of the byte↔rope seam (`docs/plans/2026-06-14-encoding-and-invalid-utf8.md`).
/// Returns the decoded text, the encoding it landed on (the buffer's
/// `'fileencoding'`), and whether the file carried a BOM (the buffer's `'bomb'`).
///
/// Each entry is tried *strictly* — a candidate that can't decode the bytes without
/// loss is skipped, so a file is only ever read as an encoding it genuinely is:
/// - `"ucs-bom"` sniffs a leading BOM ([`encoding_rs::Encoding::for_bom`]); a match
///   decodes the body in that encoding with `bomb = true`, no match falls through.
/// - a real label (`"utf-8"`, `"utf-16le"`, …) decodes the whole stream and is taken
///   only if it is loss-free (e.g. strict UTF-8 fails on the first invalid byte).
///
/// **Resilience guarantee:** the function always succeeds. If every configured
/// encoding rejects the bytes (or the list is empty/garbage), it falls back to
/// `latin1` ([`Encoding::LATIN1`] = windows-1252), a total, bijective single-byte
/// codec, so an invalid-UTF-8 (or any) file always opens *and* round-trips exactly
/// on write — no lossy `from_utf8_lossy` that would corrupt the file on the next
/// `:w`. (This is why the original PUA-escape scheme the plan sketched is
/// unnecessary: windows-1252 has no undecodable bytes to escape.)
pub fn decode_to_rope(bytes: &[u8], fileencodings: &str) -> (String, Encoding, bool) {
    for entry in fileencodings
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        if entry == "ucs-bom" {
            if let Some((enc, bom_len)) = encoding_rs::Encoding::for_bom(bytes) {
                if let Some(text) =
                    enc.decode_without_bom_handling_and_without_replacement(&bytes[bom_len..])
                {
                    return (text.into_owned(), Encoding(enc), true);
                }
            }
            continue;
        }
        let Some(enc) = Encoding::from_label(entry) else {
            continue;
        };
        if let Some(text) = enc
            .inner()
            .decode_without_bom_handling_and_without_replacement(bytes)
        {
            return (text.into_owned(), enc, false);
        }
    }
    // The terminal fallback: windows-1252 is total (every byte maps), so this never
    // errors and always reproduces the original bytes on write.
    let text = Encoding::LATIN1
        .inner()
        .decode_without_bom_handling_and_without_replacement(bytes)
        .expect("windows-1252 decodes every byte stream")
        .into_owned();
    (text, Encoding::LATIN1, false)
}

/// Encode the rope's UTF-8 `text` back to `enc`'s bytes for `:w` — the write half of
/// the seam. Prepends `enc`'s BOM when `bomb`. **Fails loud** (`E513`, naming the
/// offending scalar and its byte offset) on the first character `enc` can't
/// represent, rather than letting `encoding_rs` silently emit an HTML numeric
/// character reference — corrupting a file on save is exactly what the project's
/// fail-loud rule forbids. A latin1/invalid-UTF-8 buffer (decoded by
/// [`decode_to_rope`]) always re-encodes byte-for-byte, since windows-1252 is
/// bijective.
pub fn encode_from_str(text: &str, enc: Encoding, bomb: bool) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    if bomb {
        out.extend_from_slice(bom_bytes(enc));
    }
    // `encoding_rs::encode` cannot *emit* UTF-16 (its `output_encoding` is UTF-8 for
    // the UTF-16 families), so encode those code-unit by code-unit ourselves. Every
    // scalar is representable in UTF-16, so this never fails.
    if let Some(little_endian) = utf16_endianness(enc) {
        out.reserve(text.len() * 2);
        for unit in text.encode_utf16() {
            out.extend_from_slice(&if little_endian {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            });
        }
        return Ok(out);
    }
    let (bytes, _, had_unmappable) = enc.inner().encode(text);
    if had_unmappable {
        // Re-scan to name the first offending scalar (only on the error path, so the
        // common loss-free write pays nothing). One char at a time pinpoints it.
        for (offset, ch) in text.char_indices() {
            let mut buf = [0u8; 4];
            let (_, _, bad) = enc.inner().encode(ch.encode_utf8(&mut buf));
            if bad {
                bail!(
                    "E513: conversion failed (cannot represent U+{:04X} {:?} in {}) at byte {}",
                    ch as u32,
                    ch,
                    enc,
                    offset
                );
            }
        }
        // `encode` flagged a loss but the per-char scan found none — never expected,
        // but fail loud rather than write the NCR-mangled bytes.
        bail!("E513: conversion failed (unrepresentable character in {enc})");
    }
    out.extend_from_slice(&bytes);
    Ok(out)
}

/// Map a (lowercased, trimmed) vim CJK codepage spelling to the WHATWG label
/// `encoding_rs` knows for the same codec. vim names the legacy double-byte codecs
/// by Windows codepage (`cp932`, …) or Unix convention (`euc-cn`); the WHATWG
/// Encoding Standard names the same codec differently, so these aliases bridge the
/// muscle memory. (`euc-jp`/`euc-kr`/`gbk`/`big5`/`koi8-r`/`cp1251` already *are*
/// WHATWG labels and need no alias.) The display spelling reads back as the
/// canonical WHATWG-lowercased name (e.g. `cp932` → `shift_jis`), per [`Encoding`]'s
/// `Display`.
fn vim_cjk_alias(label: &str) -> Option<&'static str> {
    Some(match label {
        "cp932" => "shift_jis",      // Japanese (Windows / sjis)
        "cp936" | "euc-cn" => "gbk", // Simplified Chinese (GBK supersets GB2312/EUC-CN)
        "cp949" => "euc-kr",         // Korean (WHATWG's euc-kr decoder *is* cp949)
        "cp950" => "big5",           // Traditional Chinese
        _ => return None,
    })
}

/// The byte-order mark for `enc`, or empty for an encoding that has none. Used to
/// re-emit the BOM on write when the buffer's `'bomb'` is set.
fn bom_bytes(enc: Encoding) -> &'static [u8] {
    match enc.inner().name() {
        "UTF-8" => &[0xEF, 0xBB, 0xBF],
        "UTF-16LE" => &[0xFF, 0xFE],
        "UTF-16BE" => &[0xFE, 0xFF],
        _ => &[],
    }
}

/// `Some(true)` for UTF-16LE, `Some(false)` for UTF-16BE, `None` otherwise — the
/// encodings [`encode_from_str`] must emit by hand (see there).
fn utf16_endianness(enc: Encoding) -> Option<bool> {
    match enc.inner().name() {
        "UTF-16LE" => Some(true),
        "UTF-16BE" => Some(false),
        _ => None,
    }
}
