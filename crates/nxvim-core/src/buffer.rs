//! Text buffers, backed by a rope.

use std::path::{Path, PathBuf};

use anyhow::Result;
use ropey::{LineType, Rope};

/// The line-break convention nxvim tracks. `LF_CR` recognizes both Unix (`\n`)
/// and DOS (`\r\n`) breaks, so files of either `fileformat` split into lines
/// correctly. (Available via ropey's default `metric_lines_lf_cr` feature.)
const LINE_TYPE: LineType = LineType::LF_CR;

/// A text buffer.
///
/// Indices are **byte offsets** into the underlying UTF-8 (ropey 2.0's native
/// metric — and the same model vim uses for columns). Invariant: the rope
/// always ends with a trailing `\n`, so an empty buffer is `"\n"` (a single
/// empty editable line) and the number of *editable* lines is
/// `rope.len_lines() - 1`; the final phantom line is never edited or displayed.
pub struct Buffer {
    pub text: Rope,
    pub path: Option<PathBuf>,
    pub modified: bool,
}

impl Default for Buffer {
    fn default() -> Self {
        Buffer::empty()
    }
}

impl Buffer {
    pub fn empty() -> Self {
        Buffer {
            text: Rope::from_str("\n"),
            path: None,
            modified: false,
        }
    }

    /// Load a buffer from `path`. A missing file yields an empty buffer bound to
    /// that path (written on first save), matching `vim file-that-does-not-exist`.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let mut text = if path.exists() {
            Rope::from_str(&std::fs::read_to_string(path)?)
        } else {
            Rope::new()
        };
        ensure_trailing_newline(&mut text);
        Ok(Buffer {
            text,
            path: Some(path.to_path_buf()),
            modified: false,
        })
    }

    /// Number of editable lines (excludes the phantom final line).
    pub fn line_count(&self) -> usize {
        self.text.len_lines(LINE_TYPE).saturating_sub(1)
    }

    /// Contents of editable line `idx`, without its trailing newline.
    pub fn line(&self, idx: usize) -> String {
        if idx >= self.line_count() {
            return String::new();
        }
        let mut s = self.text.line(idx, LINE_TYPE).to_string();
        if s.ends_with('\n') {
            s.pop();
            if s.ends_with('\r') {
                s.pop();
            }
        }
        s
    }

    /// Number of bytes in editable line `idx`, excluding the newline.
    pub fn line_len(&self, idx: usize) -> usize {
        self.line(idx).len()
    }

    /// Byte offset at the start of editable line `idx`.
    pub fn line_start(&self, idx: usize) -> usize {
        self.text.line_to_byte_idx(idx, LINE_TYPE)
    }

    /// Editable line containing byte offset `byte_idx`.
    pub fn byte_to_line(&self, byte_idx: usize) -> usize {
        self.text.byte_to_line_idx(byte_idx, LINE_TYPE)
    }

    /// Byte offset for `(line, col)`, where `col` is a byte offset within the line.
    pub fn byte_at(&self, line: usize, col: usize) -> usize {
        self.line_start(line) + col
    }

    pub fn len_bytes(&self) -> usize {
        self.text.len()
    }

    /// All editable lines as owned strings (used by the API `get_lines`).
    pub fn lines(&self) -> Vec<String> {
        (0..self.line_count()).map(|i| self.line(i)).collect()
    }

    /// Re-establish the trailing-newline invariant after a mutation.
    pub fn normalize(&mut self) {
        ensure_trailing_newline(&mut self.text);
    }

    /// Write the buffer to `path` (or its bound path). Returns `(bytes, lines)`.
    pub fn write(&mut self, path: Option<PathBuf>) -> Result<(usize, usize)> {
        let target = path
            .or_else(|| self.path.clone())
            .ok_or_else(|| anyhow::anyhow!("E32: No file name"))?;
        let contents = self.text.to_string();
        std::fs::write(&target, &contents)?;
        let lines = self.line_count();
        self.path = Some(target);
        self.modified = false;
        Ok((contents.len(), lines))
    }
}

fn ensure_trailing_newline(text: &mut Rope) {
    let n = text.len();
    if n == 0 {
        text.insert_char(0, '\n');
    } else if text.get_char(n - 1).map(|c| c != '\n').unwrap_or(true) {
        text.insert_char(n, '\n');
    }
}
