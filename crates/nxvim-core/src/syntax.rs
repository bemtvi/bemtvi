//! The synchronous syntax-engine seam.
//!
//! `nxvim-core` defines the *interface* — and the plain data it exchanges — for
//! an in-process treesitter backend; the implementation lives in `nxvim-ts`.
//! Keeping only the interface here preserves core's invariant (no tree-sitter,
//! no C, no I/O, no async) while letting the editor own a `Box<dyn SyntaxEngine>`
//! and query highlights and indentation **directly, in the same frame** as the
//! keypress that changed the buffer.
//!
//! A front end with no engine (a bare-core test) simply has no highlighting and
//! no treesitter indentation.

use crate::buffer::BufferEdit;
use crate::editor::BufferId;

/// One highlight span, in buffer coordinates: a byte range **within line `line`**
/// and the capture-group name to paint it as (e.g. `"keyword"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub line: usize,
    /// Byte column within the line (inclusive start).
    pub start_byte: usize,
    /// Byte column within the line (exclusive end).
    pub end_byte: usize,
    /// Capture name, e.g. `"keyword"`.
    pub group: String,
}

/// The outcome of [`SyntaxEngine::open`]. Lets the editor surface a *genuine*
/// grammar load failure once, while staying silent for the common, expected case
/// of no grammar being installed for a language (best-effort highlighting).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenOutcome {
    /// Parsed, **or** there is simply no grammar installed for this language —
    /// both are silent (a buffer with no grammar just isn't highlighted).
    Ok,
    /// A grammar **is** installed but failed to load (bad ABI, missing symbol,
    /// unparseable query, …). Carries a human-readable reason worth echoing.
    LoadFailed(String),
}

/// The editor's effective indent settings, passed to [`SyntaxEngine::indent`] so
/// the engine can turn an indent *level* into a target column width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndentParams {
    /// Resolved `shiftwidth` (sw → ts → default).
    pub shiftwidth: usize,
    pub tabstop: usize,
}

/// Synchronous, in-process syntax backend. The editor owns one and calls it
/// directly; a front end with none simply has no highlighting or ts-indent.
///
/// The engine keeps its **own shadow text** per buffer, so its methods never
/// borrow the editor's buffers: `edit` takes deltas by value, and
/// `highlights`/`indent` query the engine's own shadow.
pub trait SyntaxEngine {
    /// (Re)initialize `buffer` from full `text` in `language` and parse it. Used
    /// on open and on a whole-rope replacement (undo/redo, reload). The
    /// [`OpenOutcome`] tells the editor whether an installed grammar failed to
    /// load (worth echoing) versus the silent no-grammar / parsed-fine cases.
    fn open(&mut self, buffer: BufferId, language: &str, text: &str) -> OpenOutcome;

    /// Apply edit deltas to `buffer` and reparse **incrementally**.
    fn edit(&mut self, buffer: BufferId, edits: &[BufferEdit]);

    /// Forget a buffer's parse state (the editor deleted it).
    fn close(&mut self, buffer: BufferId);

    /// Highlight spans for the line range `[first, last)`.
    fn highlights(&mut self, buffer: BufferId, first: usize, last: usize) -> Vec<Span>;

    /// Target indent **width in columns** for `line`, or `None` when there is no
    /// grammar / no `indents.scm` / the query is inconclusive — in which case the
    /// caller falls back (copy-previous-line autoindent, then column 0).
    fn indent(&mut self, buffer: BufferId, line: usize, p: &IndentParams) -> Option<usize>;

    /// Whether treesitter indentation is *available* for `buffer` — a grammar with
    /// an `indents.scm` is loaded. Lets the editor read a `None` from [`indent`] as
    /// an *inconclusive query* (fall back to copy-previous-line autoindent) when
    /// indentation is active, versus *no ts-indent at all* (keep vim's
    /// autoindent-off default of column 0) when it isn't.
    fn indents_available(&self, buffer: BufferId) -> bool;
}
