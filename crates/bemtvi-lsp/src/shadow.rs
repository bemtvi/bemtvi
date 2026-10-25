//! The text a language server currently holds for one document — what the editor
//! last sent it — kept so incremental `didChange` ranges can be computed against
//! the document *as the server sees it* (neovim's `prev_lines`).
//!
//! A rope, not a `String`: a `didChange` batch replays every journaled edit over
//! the shadow, resolving each edit's endpoints to a `(line, character)` and then
//! splicing it in. Over a `String` both steps cost the whole document (a prefix
//! scan to count lines, a memmove of the tail), so a batch of one edit per line —
//! a `:%s` over a large file — was O(edits × document) and froze the editor tick.
//! The rope makes each step logarithmic plus the length of the one line whose
//! column is converted.

use std::ops::Range;

use lsp_types::Position;
use ropey::{LineType, Rope};

use crate::PositionEncoding;

/// Line breaks as the editor's own buffer counts them (and as the LSP base
/// protocol does): `\n`, `\r\n` and a lone `\r`.
const LINE_TYPE: LineType = LineType::LF_CR;

/// One server's copy of one document. See the module docs.
#[derive(Clone, Debug, Default)]
pub struct DocShadow {
    rope: Rope,
}

impl DocShadow {
    /// Replace the whole shadow with `text` (a `didOpen` or full-document push).
    pub fn set(&mut self, text: &str) {
        self.rope = Rope::from_str(text);
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.rope.len()
    }

    /// Whether the shadow is empty.
    pub fn is_empty(&self) -> bool {
        self.rope.len() == 0
    }

    /// Whether the last byte is a `\n`.
    pub fn ends_with_newline(&self) -> bool {
        let len = self.rope.len();
        len > 0 && self.rope.byte(len - 1) == b'\n'
    }

    /// Whether `byte` (≤ [`len`](Self::len)) falls on a character boundary.
    pub fn is_char_boundary(&self, byte: usize) -> bool {
        byte <= self.rope.len() && self.rope.is_char_boundary(byte)
    }

    /// The LSP [`Position`] of byte offset `byte` (clamped to the length; must be a
    /// character boundary) in `encoding`.
    pub fn position(&self, byte: usize, encoding: PositionEncoding) -> Position {
        let byte = byte.min(self.rope.len());
        let line = self.rope.byte_to_line_idx(byte, LINE_TYPE);
        let line_start = self.rope.line_to_byte_idx(line, LINE_TYPE).min(byte);
        let character = match encoding {
            PositionEncoding::Utf8 => byte - line_start,
            PositionEncoding::Utf16 => self
                .rope
                .slice(line_start..byte)
                .chars()
                .map(char::len_utf16)
                .sum(),
            PositionEncoding::Utf32 => self.rope.slice(line_start..byte).chars().count(),
        };
        Position {
            line: line as u32,
            character: character as u32,
        }
    }

    /// Replace `range` with `text`. `false` (and the shadow untouched) when the
    /// range is out of bounds, inverted, or splits a character — the shadow no
    /// longer matches the edit journal, so the caller must resend the document.
    pub fn replace(&mut self, range: Range<usize>, text: &str) -> bool {
        if range.start > range.end
            || !self.is_char_boundary(range.start)
            || !self.is_char_boundary(range.end)
        {
            return false;
        }
        if range.end > range.start && self.rope.try_remove(range.clone()).is_err() {
            return false;
        }
        text.is_empty() || self.rope.try_insert(range.start, text).is_ok()
    }
}
