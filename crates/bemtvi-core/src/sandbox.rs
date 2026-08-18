//! The **bounded compute sandbox** seam.
//!
//! `bemtvi-core` stays pure and synchronous, so it cannot link a Lua VM — yet the
//! synchronous editing paths (`:s` replacement expressions today; `indentexpr`,
//! `foldtext` and picker re-ranking later) need to call user-supplied code and
//! *use the answer in the same tick*. The main Lua VM cannot serve them: it lives
//! in the server, is driven asynchronously, and is reached only through the
//! pushed mirror.
//!
//! So the core hosts a second, deliberately tiny VM the same way it hosts
//! tree-sitter — through a trait it defines and the server implements (see
//! [`crate::syntax::SyntaxEngine`] for the established shape). The sandbox is:
//!
//! - **pure** — no editor state, no mirror, no I/O, no `btv.*`. It takes values
//!   and returns a value; it cannot mutate anything.
//! - **bounded** — every call carries a wall-clock budget, so a runaway
//!   expression costs one aborted command rather than a frozen editor.
//! - **loud** — every failure is a typed [`SandboxError`] the caller reports.
//!   Nothing here ever degrades to a fake value.

use std::fmt;
use std::time::Duration;

/// A handle to a function compiled into the sandbox.
///
/// Opaque to the core: the engine assigns the id and owns the compiled chunk.
/// Released with [`SandboxEngine::release`] when the caller is done, so a long
/// editing session does not accumulate one chunk per `:s` invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SandboxFn(pub u64);

/// Why a sandbox call did not produce a value.
///
/// Every variant is reported to the user — per the no-silent-stubs rule, a
/// failing expression aborts its command rather than yielding empty text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxError {
    /// The source did not compile (a Lua syntax error).
    Compile(String),
    /// The call raised at runtime.
    Runtime(String),
    /// The call exceeded its wall-clock budget and was abandoned.
    Deadline(Duration),
    /// The call returned something that is not a string or a number — a bug in
    /// the expression, not a value to coerce. Carries the offending type name.
    BadReturn(String),
    /// No sandbox engine is installed (a bare-core test, or a front end that
    /// ships without one). Never silently treated as "no expression".
    Unavailable,
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compile(m) => write!(f, "invalid expression: {m}"),
            Self::Runtime(m) => write!(f, "expression failed: {m}"),
            Self::Deadline(d) => {
                write!(f, "expression exceeded its {}ms budget", d.as_millis())
            }
            Self::BadReturn(t) => {
                write!(f, "expression returned {t}, expected a string or number")
            }
            Self::Unavailable => write!(f, "no expression sandbox is available"),
        }
    }
}

/// One decoration a paint expression asked for, in the units an extmark takes:
/// **0-based, end-exclusive byte** offsets into the line, plus whatever the span
/// draws.
///
/// Lua hands back `{ first, last, group }` with 1-based *inclusive* columns —
/// `string.find`'s convention, so a match drops straight in — and the engine
/// converts here, at the boundary, rather than leaving two conventions in play.
///
/// [`group`](Self::group) and [`decor`](Self::decor) are each optional but not
/// both: a span draws a highlight over its range, or virtual text / a sign / a
/// line background anchored at its start, or both. A span that draws neither is a
/// bug in the expression, not an empty decoration, and is refused.
///
/// `decor` is the extmark's own [`VirtDecor`](crate::extmark::VirtDecor) rather
/// than a parallel vocabulary, so the paint reaches every client through the
/// projection that already exists — and a decoration key added later needs no
/// change here at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaintSpan {
    pub start: usize,
    pub end: usize,
    pub group: Option<String>,
    pub decor: Option<Box<crate::extmark::VirtDecor>>,
}

/// One quickfix entry as a sandbox expression sees it.
///
/// The field names are the keys `btv.qf.getqflist()` already returns, so a render
/// block reads the vocabulary a plugin knows rather than a third spelling of an
/// entry, and it travels in **both** directions: a `btv.qf.text` block is handed
/// one, a `btv.qf.parse` block returns one.
///
/// `lnum`/`col` are vim's 1-based positions (`0` = none) and `typ` is the error
/// type letter as a string (`"E"`/`"W"`/`"I"`/`"N"`, `""` for none) — the string
/// form the Lua side uses, not the `u8` the core stores.
///
/// Coming *out* of a parser, an omitted key takes its zero value, except `valid`,
/// which defaults to vim's rule — an entry with a line number is jumpable — so a
/// parser that fills in `lnum` need not also say so.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QfFields {
    pub filename: String,
    pub bufnr: i64,
    pub module: String,
    pub lnum: i64,
    pub end_lnum: i64,
    pub col: i64,
    pub end_col: i64,
    pub vcol: bool,
    pub nr: i64,
    pub pattern: String,
    pub text: String,
    pub typ: String,
    pub valid: bool,
}

/// How many surviving rows a picker re-ranker is applied to, at most.
///
/// The scorer runs on the *filtered* set, never `all_items` — but a loose query
/// over a huge candidate list can still leave tens of thousands of survivors, and
/// at roughly 3us a call that would blow the frame. Only the top slice by native
/// score is re-ranked; the tail keeps native order, which is invisible in practice
/// because nobody scrolls past it. Bounded at a few milliseconds either way.
pub const RERANK_LIMIT: usize = 1000;

/// The wall-clock budget one sandbox call may consume before it is abandoned.
///
/// Sized against the same reasoning as the tree-sitter parse deadline: long
/// enough that no reasonable expression trips it, short enough that a runaway
/// one costs a visible hiccup rather than a hang. A `:s` over many matches pays
/// this *per match*, but a single expiry aborts the whole command, so the total
/// is bounded by the first runaway rather than by the match count.
pub const CALL_DEADLINE: Duration = Duration::from_millis(50);

/// The pure, bounded compute VM the synchronous editing paths call into.
///
/// Implemented by `bemtvi-sandbox` and installed via
/// [`Editor::set_sandbox_engine`](crate::editor::Editor::set_sandbox_engine).
/// Later phases add sibling `call_*` methods (a scorer, an indent function);
/// each is typed rather than generic, so the marshalling stays explicit and the
/// deadline contract is stated per call shape.
pub trait SandboxEngine {
    /// Compile `src` as a **Lua expression** (not a statement block) whose value
    /// is the result, callable with `params` in scope — the engine wraps it as
    /// `function(<params>) return (<src>) end`.
    ///
    /// The parameter list is the call shape's contract: `["m", "lnum"]` for a
    /// `:s` replacement, `["label", "query", "score"]` for a picker re-ranker.
    fn compile_expr(&mut self, src: &str, params: &[&str]) -> Result<SandboxFn, SandboxError>;

    /// Call a compiled `:s` replacement expression for one match.
    ///
    /// `groups[0]` is the whole match and `groups[1..]` the capture groups, with
    /// `None` for a group that did not participate (it arrives as `nil`).
    /// `lnum` is the 1-based line the match sits on.
    fn call_subst(
        &mut self,
        f: SandboxFn,
        groups: &[Option<&str>],
        lnum: usize,
    ) -> Result<String, SandboxError>;

    /// Call a compiled picker re-ranker for one surviving row.
    ///
    /// `score` is the native fuzzy score the row already earned, so an expression
    /// can *nudge* the order (`score + bonus`) rather than having to reinvent
    /// matching. The result is the new sort key — **higher sorts first**.
    fn call_score(
        &mut self,
        f: SandboxFn,
        label: &str,
        query: &str,
        score: i64,
    ) -> Result<f64, SandboxError>;

    /// Call a compiled `'foldtext'` expression for one closed fold.
    ///
    /// `first` is the fold's first line, `lines` how many it covers, `lnum` the
    /// 1-based line it starts on. Returns the text the collapsed row shows.
    fn call_fold_text(
        &mut self,
        f: SandboxFn,
        first: &str,
        lines: i64,
        lnum: i64,
    ) -> Result<String, SandboxError>;

    /// Call a compiled filetype sniffer for one buffer.
    ///
    /// `name` is the file's basename, `ext` its extension (`""` when it has
    /// none), `head` the first few lines of content. `Ok(None)` when the
    /// expression declines (returns `nil` or `""`) — declining is a normal
    /// answer here, not an error, so the built-in name/pattern/extension tables
    /// still get their say.
    fn call_filetype(
        &mut self,
        f: SandboxFn,
        name: &str,
        ext: &str,
        head: &str,
    ) -> Result<Option<String>, SandboxError>;

    /// Call a compiled `'indentexpr'` for one line.
    ///
    /// `Ok(None)` when the expression declines (`nil`), leaving the next indent
    /// source — `smartindent`, then `autoindent` — to answer.
    fn call_indent(
        &mut self,
        f: SandboxFn,
        prev: &str,
        line: &str,
        lnum: i64,
        sw: i64,
        previndent: i64,
    ) -> Result<Option<i64>, SandboxError>;

    /// Call a compiled generic `'foldexpr'` for one line.
    ///
    /// Returns vim's fold-level *value* as a string (`"0"`, `"1"`, `">1"`,
    /// `"a1"`, `"s1"`, `"="`, …) — the caller parses it with the same grammar it
    /// always has. A number is accepted and stringified, since a bare level is
    /// the common answer.
    fn call_foldexpr(
        &mut self,
        f: SandboxFn,
        line: &str,
        lnum: i64,
    ) -> Result<String, SandboxError>;

    /// Call a compiled completion re-ranker for one row of the popup.
    ///
    /// `score` is the **blended** native key the popup already sorted on (the
    /// fuzzy score plus the source's `priority` bias), so nudging it composes with
    /// the source order rather than fighting it. `kind` is the row's kind label
    /// (`"Snippet"`, an LSP `CompletionItemKind` name, `""` for a plain buffer
    /// word). The result is the new sort key — **higher sorts first**.
    fn call_complete_score(
        &mut self,
        f: SandboxFn,
        label: &str,
        query: &str,
        score: i64,
        kind: &str,
    ) -> Result<f64, SandboxError>;

    /// Compile `src` as a Lua **block** — a function *body*, not a single
    /// expression — callable with `params` in scope: the engine wraps it as
    /// `function(<params>) <src> end`, so the source ends in its own `return`.
    ///
    /// The same closed environment, deadline and memory ceiling as
    /// [`SandboxEngine::compile_expr`]; only the wrapper differs. It exists for the
    /// one call shape whose natural form is a loop — a per-line paint walking the
    /// matches on the line — where an expression would have to be an immediately
    /// invoked closure.
    fn compile_block(&mut self, src: &str, params: &[&str]) -> Result<SandboxFn, SandboxError>;

    /// Call a compiled paint block for one visible line.
    ///
    /// Returns the spans to draw, converted from the 1-based inclusive columns Lua
    /// returned. An element that is not a table, carries a column that is not an
    /// integer, names an unknown key, or draws nothing at all is a
    /// [`SandboxError::BadReturn`] — never a silently dropped span.
    fn call_paint(
        &mut self,
        f: SandboxFn,
        line: &str,
        lnum: i64,
    ) -> Result<Vec<PaintSpan>, SandboxError>;

    /// Call a compiled **expression register** (`"=` / `<C-r>=`) once.
    ///
    /// `line` is the cursor's line text, `lnum` its 1-based line number and `col`
    /// its 1-based column — vim's expression register has the whole Vimscript
    /// environment to reach for, so the pure equivalent is handed what a computed
    /// insert actually wants. Returns the text to insert.
    fn call_eval(
        &mut self,
        f: SandboxFn,
        line: &str,
        lnum: i64,
        col: i64,
    ) -> Result<String, SandboxError>;

    /// Call a compiled quickfix **line parser** for one line of build output.
    ///
    /// `lnum` is the line's 1-based position in the output, not in any file.
    /// `Ok(None)` when the parser declines (returns `nil`) — a normal answer,
    /// meaning "not an error line", which the caller keeps as an *invalid* entry
    /// carrying the raw text, the way `'errorformat'` does.
    ///
    /// An unknown key in the returned table is a [`SandboxError::BadReturn`]: a
    /// parser that misspells `lnum` would otherwise silently produce entries that
    /// cannot be jumped to.
    fn call_qf_parse(
        &mut self,
        f: SandboxFn,
        line: &str,
        lnum: i64,
    ) -> Result<Option<QfFields>, SandboxError>;

    /// Call a compiled quickfix **render** expression for one list entry.
    ///
    /// `item` is marshalled into a Lua table keyed as [`QfFields`] documents;
    /// `idx` is the entry's 1-based position in the list, so a block can number
    /// its rows. Returns the text of that row in the `:copen` window.
    fn call_qf_text(
        &mut self,
        f: SandboxFn,
        item: &QfFields,
        idx: i64,
    ) -> Result<String, SandboxError>;

    /// Drop a compiled chunk. Releasing an unknown handle is a no-op.
    fn release(&mut self, f: SandboxFn);
}
