//! Text buffers, backed by a rope.

use std::path::{Path, PathBuf};

use anyhow::Result;
use ropey::Rope;

/// A text buffer.
///
/// Invariant: the rope always ends with a trailing `\n`, so an empty buffer is
/// `"\n"` (a single empty editable line). The number of *editable* lines is
/// therefore `rope.len_lines() - 1`; the final phantom line after the last
/// newline is never edited or displayed.
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
        Buffer { text: Rope::from_str("\n"), path: None, modified: false }
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
        Ok(Buffer { text, path: Some(path.to_path_buf()), modified: false })
    }

    /// Number of editable lines (excludes the phantom final line).
    pub fn line_count(&self) -> usize {
        self.text.len_lines().saturating_sub(1)
    }

    /// Contents of editable line `idx`, without its trailing newline.
    pub fn line(&self, idx: usize) -> String {
        if idx >= self.line_count() {
            return String::new();
        }
        let mut s = self.text.line(idx).to_string();
        if s.ends_with('\n') {
            s.pop();
            if s.ends_with('\r') {
                s.pop();
            }
        }
        s
    }

    /// Number of characters in editable line `idx`, excluding the newline.
    pub fn line_len(&self, idx: usize) -> usize {
        if idx >= self.line_count() {
            return 0;
        }
        let slice = self.text.line(idx);
        let mut len = slice.len_chars();
        if len > 0 && slice.char(len - 1) == '\n' {
            len -= 1;
            if len > 0 && slice.char(len - 1) == '\r' {
                len -= 1;
            }
        }
        len
    }

    /// Char index at the start of editable line `idx`.
    pub fn line_start(&self, idx: usize) -> usize {
        self.text.line_to_char(idx)
    }

    /// Char index for `(line, col)`.
    pub fn char_at(&self, line: usize, col: usize) -> usize {
        self.text.line_to_char(line) + col
    }

    pub fn len_chars(&self) -> usize {
        self.text.len_chars()
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
    let n = text.len_chars();
    if n == 0 {
        text.insert_char(0, '\n');
    } else if text.char(n - 1) != '\n' {
        text.insert_char(n, '\n');
    }
}
