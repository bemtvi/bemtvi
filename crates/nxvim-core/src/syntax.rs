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

use std::any::Any;

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
/// A grammar an engine wants loaded off the editor thread
/// ([`SyntaxEngine::take_grammar_requests`]).
///
/// `payload` is opaque to everyone in between: the engine that produced it and that
/// engine's loader are the only two that read it, so the host just carries it to a
/// worker and carries the result back. That keeps the paths, query overrides and
/// grammar types of a particular engine out of the editor and the server.
/// What a finished grammar load turned out to be
/// ([`SyntaxEngine::install_grammar`]).
pub enum GrammarInstall {
    /// A usable grammar landed: the buffers waiting on it can parse now, so the frame
    /// has something new to show.
    Loaded,
    /// No parser installed for this language. Silent, and nothing to repaint — the
    /// buffer was already painting as plain text and will keep doing so.
    Missing,
    /// Installed but broken; the reason to echo, once.
    Failed(String),
}

pub struct GrammarRequest {
    /// The language, for reporting and for keying the result back.
    pub language: String,
    /// What that engine's loader needs to do the load. Opaque here.
    pub payload: Box<dyn Any + Send>,
}

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

    /// The languages `buffer` has injected layers for — the typescript inside a vue
    /// file's `<script setup lang="ts">`, the rust inside a markdown fence.
    ///
    /// Read by the server right after [`highlights`](Self::highlights) so those
    /// languages' runtimepath queries get resolved too. They cannot be resolved any
    /// earlier: an injected language is usually read out of the document
    /// (`lang="ts"` is node text), so it isn't known until the parse has run. The
    /// default is empty — an engine with no injections has nothing to report.
    fn injected_languages(&self, _buffer: BufferId) -> Vec<String> {
        Vec::new()
    }

    /// [`injected_languages`](Self::injected_languages) for the most recent
    /// **stateless** highlight ([`highlight_text`](Self::highlight_text) and
    /// friends), which injects the same way but owns no [`BufferId`] to key off.
    /// Read immediately after that call, like
    /// [`line_background_lines`](Self::line_background_lines). Empty by default.
    fn text_injected_languages(&self) -> Vec<String> {
        Vec::new()
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

    /// Like [`highlight_text`](Self::highlight_text), but also returns the 0-based
    /// lines a **full-line-background** capture (`@markup.raw.block` — a fenced code
    /// block) touches. A preview surface paints those as a separate line-background
    /// layer *under* the text, so the winner-takes-cell token spans (a `>lua`
    /// block's injected syntax among them) don't overwrite the block background on
    /// every non-blank cell — the "background only on the whitespace" artifact. The
    /// default reuses `highlight_text` and reports no backgrounds (an engine with no
    /// tree-sitter block backgrounds, e.g. the wasm JS-side highlighter).
    fn highlight_text_bg(
        &mut self,
        language: &str,
        text: &str,
        first: usize,
        last: usize,
    ) -> (Vec<Span>, Vec<usize>) {
        (self.highlight_text(language, text, first, last), Vec::new())
    }

    /// [`highlight_text`](Self::highlight_text) for a snippet that is **not a whole
    /// program** — a fenced code block inside LSP documentation (hover, completion
    /// docs), which is either a fragment of the language (a struct field, a bare
    /// statement, a body-less signature) or an annotation dialect the server invented
    /// for display (`lua_ls` puts `function f(t: table)` in a ` ```lua ` fence).
    ///
    /// Handed to the whole-file path, the second kind doesn't degrade — it comes out
    /// *confidently wrong*, because a structural query matched a construct that isn't
    /// there. An engine implementing this must trust structure only where the parse
    /// is sound. The default is [`highlight_text`](Self::highlight_text): an engine
    /// with no notion of a failed parse (the wasm JS-side highlighter) just keeps
    /// doing what it did.
    fn highlight_fragment(
        &mut self,
        language: &str,
        text: &str,
        first: usize,
        last: usize,
    ) -> Vec<Span> {
        self.highlight_text(language, text, first, last)
    }

    /// Install the **fragment contexts** for `language` — the framings
    /// [`highlight_fragment`](Self::highlight_fragment) tries, in order, when a
    /// snippet doesn't parse on its own (`"struct __nx {\n%s\n}"`, the `%s` marking
    /// where the snippet goes). Replaces any previous list; an empty list turns the
    /// ladder off for that language. The default ignores them, which is right for an
    /// engine that does no off-buffer highlighting to begin with (the wasm JS-side
    /// one): there is no ladder to configure.
    fn set_fragment_context(&mut self, _language: &str, _templates: Vec<String>) {}

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

    /// Absolute byte ranges `[start, end)` of `buffer`'s `textobjects.scm` nodes
    /// captured as exactly `capture` (e.g. `"function.inner"`, `"parameter.outer"`)
    /// that **contain** `byte` (`start <= byte < end`), **innermost (smallest span)
    /// first** — so a `count` picks successively larger enclosing scopes. The
    /// tree-sitter source behind the `vif` / `daf` / `dia` text objects: the editor
    /// selects the `count`-th range and feeds it to the shared text-object applier.
    /// Empty when there is no grammar, no `textobjects.scm`, or nothing matches. The
    /// default returns nothing — an engine that can't run the query (the wasm
    /// JS-side highlighter) simply offers no tree-sitter text objects.
    fn text_objects_at(
        &mut self,
        _buffer: BufferId,
        _capture: &str,
        _byte: usize,
    ) -> Vec<(usize, usize)> {
        Vec::new()
    }

    /// Whether tree-sitter text objects are *available* for `buffer` — a grammar
    /// with a `textobjects.scm` is loaded. Lets the editor tell "the query is loaded
    /// but matched nothing at the cursor" apart from "no tree-sitter text objects
    /// here at all", so it can fall through to nothing rather than a vim object. The
    /// default is `false`.
    fn text_objects_available(&self, _buffer: BufferId) -> bool {
        false
    }

    /// Install (or, with `text = None`, clear) a resolved query override for
    /// `(lang, name)` — the engine half of the query-resolution bridge. The Lua
    /// API has already merged `query.set` / `after/queries` / `;extends` into the
    /// final `text`; the engine compiles + caches it, consulting it in place of
    /// the on-disk query. Only `highlights` / `indents` affect the paint; other
    /// names are no-ops. `Err(reason)` on a compile failure, for a loud echo.
    /// Drain the grammars the engine wants loaded **off the editor thread**, for the
    /// host to run and hand back through [`install_grammar`](Self::install_grammar).
    ///
    /// Loading a grammar is not cheap enough to do on a tick — it is dominated by
    /// compiling the language's queries, hundreds of ms for a big grammar — and none
    /// of it is interruptible, so an engine that loads inline freezes the editor on
    /// the frame that first needs a language. An engine that defers instead paints
    /// what it can, asks here, and gets the result back a frame or two later.
    ///
    /// Empty by default, and empty for an engine the host never told to defer: the
    /// request only exists because something has a thread to run it on.
    fn take_grammar_requests(&mut self) -> Vec<GrammarRequest> {
        Vec::new()
    }

    /// Load `language` synchronously, even if this engine defers loads
    /// ([`take_grammar_requests`](Self::take_grammar_requests)) — for an ask whose
    /// answer cannot arrive a frame later.
    ///
    /// A paint or a fold self-corrects when the grammar lands: the editor repaints and
    /// recomputes. An indent or a text object cannot — they answer the keystroke that
    /// asked, and it does not come back, so a deferred load there silently degrades to
    /// the non-treesitter answer (wrong indentation, a `vif` that does nothing). The
    /// editor calls this in front of those. No-op by default, and for an engine that
    /// wasn't deferring in the first place.
    /// Reports whether this call is what made the grammar available, so the editor
    /// can re-open the buffers that were opened while it was missing.
    fn load_language_now(&mut self, _language: &str) -> bool {
        false
    }

    /// Install a grammar the host finished loading — the other half of
    /// [`take_grammar_requests`](Self::take_grammar_requests). `loaded` is the opaque
    /// payload that engine's own loader produced, so only it can read it.
    ///
    /// The verdict tells the editor both what to echo and whether the frame changed:
    /// a language with no parser installed is silent *and* leaves the buffer painting
    /// exactly as it already was, so it must not cost a repaint.
    fn install_grammar(&mut self, _language: &str, _loaded: Box<dyn Any + Send>) -> GrammarInstall {
        GrammarInstall::Missing
    }

    /// Drain the query-compile failures the engine has hit since the last call, for
    /// the editor to echo. An engine that compiles every query up front has none;
    /// one that defers a query until a keypress asks for it (the tree-sitter engine
    /// does — compiling is the whole cost of a grammar load) reports a broken one
    /// here instead of at load, once per query. Never silently swallowed: the
    /// feature degrades, and the editor says why.
    fn take_query_errors(&mut self) -> Vec<String> {
        Vec::new()
    }

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
    /// compile with no override, with any [`; inherits:`](parse_query_inherits)
    /// chain already folded in. The server reads this to compose an
    /// `after/queries` / runtimepath overlay (base ⧺ extensions) before handing the
    /// merged string back via [`set_query_overlay`](Self::set_query_overlay).
    /// `Ok(None)` when there is no base file (an engine that has none, or a
    /// language with no bundled query — e.g. a config-only `injections.scm`). The
    /// default returns `Ok(None)` for engines with no on-disk base (the wasm
    /// JS-side highlighter).
    fn base_query(&self, _lang: &str, _name: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    /// The **single-file** base for `(lang, name)` — this language's own on-disk
    /// query with *no* `; inherits:` resolution, the raw part
    /// [`base_query`](Self::base_query) composes from.
    ///
    /// The server needs the parts, not just the whole: neovim's rule is that a
    /// runtimepath query file with no `;; extends` **replaces** its language's
    /// bundled query, and replacing one link of an inherit chain means rebuilding
    /// the chain from its links. Defaults to [`base_query`](Self::base_query) — for
    /// an engine with no inherit concept the two are the same file.
    fn base_query_raw(&self, lang: &str, name: &str) -> Result<Option<String>, String> {
        self.base_query(lang, name)
    }

    /// The languages `(lang, name)`'s on-disk query **inherits**, transitively, in
    /// merge order (deepest ancestor first, `lang` itself excluded) — the chain the
    /// engine already folded into [`base_query`](Self::base_query).
    ///
    /// The engine resolves the *bundled* files; the server still has to pull
    /// `queries/<inherited>/<name>.scm` overlays out of the **runtimepath**, which
    /// the engine cannot see, so it needs to know which languages are in the chain.
    /// Empty by default, and for a language that inherits nothing.
    fn query_inherits(&self, _lang: &str, _name: &str) -> Vec<String> {
        Vec::new()
    }
}

/// The query kinds the **engine** compiles, and therefore the kinds the query
/// bridge must resolve. One list so the two can never drift: a kind added here and
/// wired in the engine is automatically resolved by the server too. (`folds` used to
/// be missing from the server's list, which left every `; inherits:`-only fold query
/// — javascript's is *only* a modeline — with no patterns at all.)
pub const ENGINE_QUERY_NAMES: &[&str] = &[
    "highlights",
    "indents",
    "injections",
    "folds",
    "textobjects",
];

/// The **modeline** bodies a query file opens with: the `;`-comment lines before
/// its first pattern, stripped of their leading `;`s and surrounding whitespace.
/// Scanning stops at the first non-comment line, so a `;`-comment deeper in the
/// file is never read as a modeline.
fn query_modelines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .map(str::trim)
        .take_while(|line| line.is_empty() || line.starts_with(';'))
        .filter(|line| !line.is_empty())
        .map(|line| line.trim_start_matches(';').trim())
}

/// Language names from a query file's `; inherits: a,b,c` modeline(s) — the
/// languages whose same-named query this one builds on, in declared order.
///
/// nvim-treesitter uses this to share one query set between related grammars:
/// `javascript/folds.scm` is *only* `; inherits: ecma,jsx`, with every pattern in
/// `ecma/folds.scm`. Empty when there is no such modeline.
///
/// Lives here because both sides of the query bridge read it: the engine folds the
/// chain into its on-disk base, and the server walks the same chain for runtimepath
/// overlays.
pub fn parse_query_inherits(text: &str) -> Vec<String> {
    query_modelines(text)
        .filter_map(|body| body.strip_prefix("inherits:"))
        .flat_map(|rest| rest.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Whether a query file carries the `;; extends` modeline — nvim-treesitter's
/// marker for "**add** these patterns to the language's query" as opposed to
/// "**be** the language's query".
///
/// This is the one bit that decides a runtimepath file's relationship to the
/// bundled one: an extending file is appended, a non-extending file *replaces* the
/// base. Without it every drop-in `queries/<lang>/<name>.scm` would silently layer
/// on top of the shipped query, so a config could add patterns but never remove or
/// redefine the set as a whole.
pub fn query_extends(text: &str) -> bool {
    query_modelines(text).any(|body| body == "extends")
}
