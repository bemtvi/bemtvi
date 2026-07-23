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

/// One foldable region from the tree-sitter `folds.scm` query: an inclusive
/// 0-based buffer-line span `[start, end]` of a `@fold`-captured node. The editor
/// turns the set of ranges into per-line fold levels by containment (deeper
/// containment = deeper level), then into nested folds — so the engine only
/// reports *where* the foldable nodes are, not the fold tree itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldRange {
    /// First line the foldable node spans (0-based).
    pub start: usize,
    /// Last line the foldable node spans (0-based, inclusive).
    pub end: usize,
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

    /// Drop any cached load result for `lang` so the next `open` re-resolves the
    /// grammar from disk. Called after `:TSInstall` writes a new parser, turning a
    /// cached "not installed" verdict back into a fresh load attempt.
    fn reload_grammar(&mut self, lang: &str);

    /// Highlight spans for the line range `[first, last)`.
    fn highlights(&mut self, buffer: BufferId, first: usize, last: usize) -> Vec<Span>;

    /// Buffer lines (0-based, absolute) a **line-background** capture covers — the
    /// markdown fenced/indented code block's `@markup.raw.block`. These feed the
    /// separate `line_bg` layer the client paints *under* the text, so the whole
    /// block reads as a solid region even on cells where a narrower injected token
    /// overwrites the block's own background (the paint is winner-takes-cell, so
    /// the background would otherwise survive only on the un-tokenized cells —
    /// spaces). Reflects the range of the **most recent [`highlights`] call** for
    /// this buffer (the server reads it immediately after `highlights`); an engine
    /// that paints no such backgrounds returns nothing (the default).
    ///
    /// [`highlights`]: Self::highlights
    fn line_background_lines(&self, _buffer: BufferId) -> Vec<usize> {
        Vec::new()
    }

    /// Whether `buffer`'s parse is still in progress — a large file whose parse was
    /// cancelled by the engine's per-frame deadline and is being resumed across
    /// frames. The server polls this after a redraw to keep scheduling frames (each
    /// of which resumes the parse via [`highlights`](Self::highlights)) until it
    /// converges, so a big file colours in progressively. The default is `false` —
    /// an engine that parses synchronously (or highlights elsewhere, like the wasm
    /// JS-side highlighter) is never "pending".
    fn parse_pending(&self, _buffer: BufferId) -> bool {
        false
    }

    /// Highlight an **off-buffer** snippet — `text` in `language`, over the line
    /// range `[first, last)` — without registering a [`BufferId`]. A stateless,
    /// full parse (no incremental reuse, no injections) for read-only surfaces like
    /// the picker preview pane, which highlight a file that is not an open buffer.
    /// Spans are in `text` coordinates (line indices count from `text`'s first
    /// line). The default returns nothing — an engine that can't parse a detached
    /// snippet (e.g. the wasm JS-side highlighter) simply leaves the surface plain.
    fn highlight_text(
        &mut self,
        _language: &str,
        _text: &str,
        _first: usize,
        _last: usize,
    ) -> Vec<Span> {
        Vec::new()
    }

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

    /// Foldable node ranges for `buffer` — the `@fold` captures of the grammar's
    /// `folds.scm` run over the current parse, each as an inclusive line span. The
    /// editor builds per-line fold levels from these (by containment) and then the
    /// nested fold tree, the tree-sitter source behind `foldmethod=expr` with the
    /// native `nx.treesitter.foldexpr`. Empty when there is no grammar, no
    /// `folds.scm`, or no parse yet. The default returns nothing — an engine with no
    /// fold query (the wasm JS-side path until 4b) supplies no tree-sitter folds.
    fn folds(&mut self, _buffer: BufferId) -> Vec<FoldRange> {
        Vec::new()
    }

    /// Whether tree-sitter folds are *available* for `buffer` — a grammar with a
    /// `folds.scm` is loaded. Lets the editor tell "the fold query is loaded but
    /// found nothing" (keep an empty fold set) apart from "the grammar isn't ready
    /// yet" (leave folds untouched and retry), so a still-loading parser doesn't
    /// transiently clear folds. The default is `false`.
    fn folds_available(&self, _buffer: BufferId) -> bool {
        false
    }

    /// Install (or, with `text = None`, clear) a resolved query override for
    /// `(lang, name)` — the engine half of the query-resolution bridge. The Lua
    /// API has already merged `query.set` / `after/queries` / `;extends` into the
    /// final `text`; the engine compiles + caches it, consulting it in place of
    /// the on-disk query. Only `highlights` / `indents` affect the paint; other
    /// names are no-ops. `Err(reason)` on a compile failure, for a loud echo.
    fn set_query(&mut self, lang: &str, name: &str, text: Option<String>) -> Result<(), String>;

    /// Install a *resolved on-disk overlay* for `(lang, name)` at buffer-open: the
    /// same as [`set_query`](Self::set_query), but the override is kept **only if
    /// `text` differs** from the base file the engine reads off disk. A language
    /// with no `after/queries` / `;extends` customization resolves back to its own
    /// base file, so this is a no-op and the engine stays on the byte-identical
    /// disk-read path. `Err(reason)` on a compile failure, for a loud echo.
    fn set_query_overlay(
        &mut self,
        lang: &str,
        name: &str,
        text: Option<String>,
    ) -> Result<(), String>;

    /// The engine's **base** `(lang, name)` query — the on-disk text it would
    /// compile with no override. The server reads this to compose an
    /// `after/queries` / runtimepath overlay (base ⧺ extensions) before handing the
    /// merged string back via [`set_query_overlay`](Self::set_query_overlay).
    /// `Ok(None)` when there is no base file (an engine that has none, or a
    /// language with no bundled query — e.g. a config-only `injections.scm`). The
    /// default returns `Ok(None)` for engines with no on-disk base (the wasm
    /// JS-side highlighter).
    fn base_query(&self, _lang: &str, _name: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
}
