//! The treesitter engine bridge (synchronous highlights) and the indentation
//! helpers (`autoindent`, `:set` shiftwidth).

use super::*;
use crate::syntax::{IndentParams, OpenOutcome, Span, SyntaxEngine};

/// Whitespace that advances the virtual column from `start` to `target`
/// (`target >= start`), the fill an inserted `<Tab>` lays down. With `expandtab`
/// it is all spaces; otherwise real tabs that jump to each `tabstop` boundary,
/// then spaces for any remainder past the last boundary — vim's tab/space mix.
pub(crate) fn fill_indent(start: usize, target: usize, tabstop: usize, expandtab: bool) -> String {
    if expandtab {
        return " ".repeat(target - start);
    }
    let mut s = String::new();
    let mut col = start;
    loop {
        let next_tab = col - (col % tabstop) + tabstop;
        if next_tab <= target {
            s.push('\t');
            col = next_tab;
        } else {
            break;
        }
    }
    s.push_str(&" ".repeat(target - col));
    s
}

/// Visual-column width of `line`'s leading whitespace: spaces count one cell, a
/// tab advances to the next `tabstop` boundary. The inverse of [`fill_indent`],
/// used to read a line's existing indent depth for copy-previous autoindent.
pub(crate) fn indent_width(line: &str, tabstop: usize) -> usize {
    let mut col = 0;
    for b in line.bytes() {
        match b {
            b' ' => col += 1,
            b'\t' => col = col - (col % tabstop) + tabstop,
            _ => break,
        }
    }
    col
}

impl Editor {
    /// Install the in-process syntax backend. The server constructs the concrete
    /// `nxvim-ts` engine at startup and hands it over; a bare-core test leaves it
    /// `None` and simply has no highlighting / treesitter indentation.
    pub fn set_syntax_engine(&mut self, engine: Box<dyn SyntaxEngine>) {
        self.syntax = Some(engine);
    }

    /// Bring the engine's parse state for `buf` up to date before a query: open
    /// it (first sync, or the path's language changed) from full text, otherwise
    /// drain the buffer's edit journal and reparse incrementally. A buffer whose
    /// path has no known grammar is left alone (the query returns nothing).
    ///
    /// Accesses `syntax` / `buffers` / `syntax_opened` as disjoint fields so the
    /// engine's `&mut` never collides with the buffer borrow it reads text from.
    fn sync_syntax_engine(&mut self, buf: BufferId) {
        // The language the engine highlights this buffer as, or `None` when there
        // is nothing to parse (no grammar for the path, or `ts_highlight` off).
        let Some(language) = self.ts_language_for(buf) else {
            return;
        };
        let Some(engine) = self.syntax.as_mut() else {
            return;
        };
        let Some(open_buf) = self.buffers.map.get_mut(&buf) else {
            return;
        };
        let buffer = &mut open_buf.buffer;

        // Capture a load failure to echo *after* the engine/buffer field borrows
        // end (so the `&mut self` echo doesn't collide with them).
        let mut load_failure: Option<String> = None;

        // A fresh buffer, or one whose language changed (`:w other.ext` to a new
        // extension), needs a full open; its stale journal is superseded.
        if self.syntax_opened.get(&buf).map(String::as_str) != Some(language.as_str()) {
            let _ = buffer.take_edits();
            if let OpenOutcome::LoadFailed(reason) =
                engine.open(buf, &language, &buffer.text.to_string())
            {
                load_failure = Some(reason);
            }
            self.syntax_opened.insert(buf, language.clone());
        } else {
            // Already open in this language: feed it just what changed. A
            // whole-rope replacement (undo/redo, reload) re-opens; ordinary deltas
            // reparse incrementally.
            let batch = buffer.take_edits();
            if !batch.is_empty() {
                if batch.resync {
                    engine.open(buf, &language, &buffer.text.to_string());
                } else {
                    engine.edit(buf, &batch.edits);
                }
            }
        }

        // A grammar that is *installed but broken* (bad ABI, unparseable query) is
        // worth surfacing — once per language, so opening many files of it doesn't
        // spam. A *missing* grammar yields `OpenOutcome::Ok` and stays silent:
        // highlighting is best-effort, not a missing feature.
        if let Some(reason) = load_failure {
            if self.syntax_failed.insert(language.clone()) {
                self.echo(format!(
                    "treesitter: grammar '{language}' failed to load: {reason}"
                ));
            }
        }
    }

    /// Re-resolve the grammar for `lang` after it was just installed
    /// (`:TSInstall`). Evicts the engine's cached load verdict (so a prior "not
    /// installed" becomes a fresh load), clears any one-shot load-failure latch,
    /// and drops the per-buffer "opened in this language" markers so the next
    /// highlight/indent sync re-opens each affected buffer against the new parser.
    pub fn reload_ts_language(&mut self, lang: &str) {
        if let Some(engine) = self.syntax.as_mut() {
            engine.reload_grammar(lang);
        }
        self.syntax_failed.remove(lang);
        self.syntax_opened
            .retain(|_buf, opened| opened.as_str() != lang);
    }

    /// The buffer's **filetype** — the *language* noun, independent of whether
    /// treesitter paints. An explicit override (`nx.bo.filetype` / `:set ft` /
    /// `:setf`) wins; otherwise the path's extension decides. `None` when the
    /// filetype is explicitly empty or the extension has no known grammar. This is
    /// what LSP / indent / the statusline key off, even with highlighting disabled.
    pub fn buffer_filetype(&self, buf: BufferId) -> Option<String> {
        match self.ts_filetype.get(&buf) {
            Some(ft) if ft.is_empty() => None, // explicit "no filetype"
            Some(ft) => Some(ft.clone()),
            None => {
                let path = self.buffer_of(buf)?.path.as_deref();
                language_of_path(path).map(str::to_string)
            }
        }
    }

    /// Whether treesitter highlighting is enabled for `buf` — the *whether* noun.
    /// Defaults on; `ts_highlight = false` (or the `stop` verb) turns it off
    /// without touching the filetype.
    pub fn ts_highlight_enabled(&self, buf: BufferId) -> bool {
        self.ts_enabled.get(&buf).copied().unwrap_or(true)
    }

    /// The language the engine actually highlights `buf` as: the [filetype]
    /// [`Self::buffer_filetype`], **gated** by the [enable]
    /// [`Self::ts_highlight_enabled`] flag. `None` when highlighting is off or no
    /// language resolves — the engine treats both identically (nothing to paint).
    /// The server reads this on the async side to know which language's queries to
    /// resolve before the buffer's first highlight.
    pub fn ts_language_for(&self, buf: BufferId) -> Option<String> {
        if !self.ts_highlight_enabled(buf) {
            return None;
        }
        self.buffer_filetype(buf)
    }

    /// Set `buf`'s explicit filetype (the language noun). `""` means "no
    /// filetype". Drops stale parse state so the next sync re-opens against the
    /// new language (or, if it now resolves to nothing, paints nothing).
    pub fn set_filetype(&mut self, buf: BufferId, ft: &str) {
        self.ts_filetype.insert(buf, ft.to_string());
        self.refresh_syntax(buf);
    }

    /// Reset `buf` to its extension-derived filetype (`:set filetype&`).
    pub fn reset_filetype(&mut self, buf: BufferId) {
        self.ts_filetype.remove(&buf);
        self.refresh_syntax(buf);
    }

    /// Enable or disable treesitter highlighting for `buf` (the `ts_highlight`
    /// noun / the `start`/`stop` verbs). Leaves the filetype untouched, so a
    /// disabled buffer still reports its language to LSP/indent.
    pub fn set_ts_highlight(&mut self, buf: BufferId, on: bool) {
        self.ts_enabled.insert(buf, on);
        self.refresh_syntax(buf);
    }

    /// Reset `buf`'s highlight-enable to the default (on).
    pub fn reset_ts_highlight(&mut self, buf: BufferId) {
        self.ts_enabled.remove(&buf);
        self.refresh_syntax(buf);
    }

    /// After a filetype/enable change, drop the buffer's "opened in language"
    /// marker so the next sync re-opens it, and — when it now resolves to no
    /// highlight language (disabled, or filetype with no grammar) — close the
    /// engine's parse so a `highlights` query returns nothing rather than the
    /// stale tree.
    fn refresh_syntax(&mut self, buf: BufferId) {
        self.syntax_opened.remove(&buf);
        if self.ts_language_for(buf).is_none() {
            if let Some(engine) = self.syntax.as_mut() {
                engine.close(buf);
            }
        }
    }

    /// Force highlighting for `buf` as `lang` — set the filetype **and** enable.
    /// A buffer the extension table misses, or one previously stopped, gets
    /// painted as `lang`. (Treesitter is otherwise controlled declaratively
    /// through the `nx.bo.filetype` / `nx.bo.ts_highlight` buffer options.)
    pub fn ts_start(&mut self, buf: BufferId, lang: String) {
        self.set_filetype(buf, &lang);
        self.set_ts_highlight(buf, true);
    }

    /// Stop highlighting `buf` — disable the enable noun, keeping the filetype so
    /// LSP/indent still see the language.
    pub fn ts_stop(&mut self, buf: BufferId) {
        self.set_ts_highlight(buf, false);
    }

    /// Reset `buf` to defaults — extension-derived filetype, highlighting on.
    pub fn ts_reset(&mut self, buf: BufferId) {
        self.reset_filetype(buf);
        self.reset_ts_highlight(buf);
    }

    /// Install (or clear, with `text = None`) a resolved treesitter query for
    /// `(lang, name)`, the editor seam of the query-resolution bridge (ADR 0001,
    /// bridge #4). The server resolves the merged string via the vendored Lua and
    /// hands it down; the engine compiles + caches it. A compile failure echoes
    /// loud (no-silent-stubs) rather than leaving a broken override unmentioned.
    pub fn set_ts_query(&mut self, lang: &str, name: &str, text: Option<String>) {
        let Some(engine) = self.syntax.as_mut() else {
            return;
        };
        if let Err(reason) = engine.set_query(lang, name, text) {
            self.echo(format!(
                "treesitter: query '{lang}/{name}' failed to compile: {reason}"
            ));
        }
    }

    /// Install a *resolved on-disk* query overlay for `(lang, name)` at buffer-open
    /// — the pure-`after/queries` / `;extends` half of the query bridge, with no
    /// explicit `query.set` behind it. Lua resolved `text` by merging the
    /// runtimepath; the engine installs it **only if it differs** from the base
    /// file it would otherwise read off disk, so a language with no customization
    /// stays on the byte-identical disk path and pays nothing. A compile failure
    /// echoes loud, like [`Self::set_ts_query`].
    pub fn set_resolved_ts_query(&mut self, lang: &str, name: &str, text: Option<String>) {
        let Some(engine) = self.syntax.as_mut() else {
            return;
        };
        if let Err(reason) = engine.set_query_overlay(lang, name, text) {
            self.echo(format!(
                "treesitter: query '{lang}/{name}' failed to compile: {reason}"
            ));
        }
    }

    /// The engine's **base** `(lang, name)` query text — what it compiles with no
    /// override. The server reads this to compose a runtimepath `queries/` /
    /// `after/queries` overlay (base ⧺ extensions) before pushing the merged string
    /// through [`Self::set_resolved_ts_query`]. `None` with no engine or no base
    /// file (a config-only query such as an `injections.scm` for a language whose
    /// bundled grammar ships none).
    pub fn ts_base_query(&self, lang: &str, name: &str) -> Option<String> {
        self.syntax
            .as_ref()
            .and_then(|engine| engine.base_query(lang, name).ok().flatten())
    }

    /// The **single-file** base for `(lang, name)` — no `; inherits:` resolution.
    /// The server composes from these raw links when a runtimepath file replaces one
    /// language of a chain. `None` with no engine or no such file.
    pub fn ts_base_query_raw(&self, lang: &str, name: &str) -> Option<String> {
        self.syntax
            .as_ref()
            .and_then(|engine| engine.base_query_raw(lang, name).ok().flatten())
    }

    /// The languages `(lang, name)`'s **bundled** query inherits, transitively, in
    /// merge order — the chain the engine already folded into
    /// [`ts_base_query`](Self::ts_base_query). The server walks it to pull the
    /// *runtimepath* overlays of the same languages, which the engine cannot see.
    /// Empty with no engine.
    pub fn ts_query_inherits(&self, lang: &str, name: &str) -> Vec<String> {
        self.syntax
            .as_ref()
            .map(|engine| engine.query_inherits(lang, name))
            .unwrap_or_default()
    }

    /// Highlight spans for the line range `[first, last)` of buffer `buf`,
    /// synced to the buffer's current content. Empty when there is no engine or
    /// no grammar for the buffer.
    pub fn highlights(&mut self, buf: BufferId, first: usize, last: usize) -> Vec<Span> {
        self.sync_syntax_engine(buf);
        match self.syntax.as_mut() {
            Some(engine) => engine.highlights(buf, first, last),
            None => Vec::new(),
        }
    }

    /// The buffer lines a line-background capture (`@markup.raw.block`) covers, from
    /// the engine's most recent [`highlights`](Self::highlights) call for `buf` — the
    /// source of the `line_bg` layer under a markdown fenced code block. Empty when
    /// there is no engine. Read right after `highlights`, so it needs no sync.
    pub fn line_background_lines(&self, buf: BufferId) -> Vec<usize> {
        match self.syntax.as_ref() {
            Some(engine) => engine.line_background_lines(buf),
            None => Vec::new(),
        }
    }

    /// The languages `buf` has treesitter *injected* layers for (the typescript of a
    /// vue `<script setup lang="ts">`). Only known once the engine has parsed, so the
    /// server reads it right after [`highlights`](Self::highlights) — see
    /// [`SyntaxEngine::injected_languages`]. Empty with no engine.
    pub fn ts_injected_languages(&self, buf: BufferId) -> Vec<String> {
        match self.syntax.as_ref() {
            Some(engine) => engine.injected_languages(buf),
            None => Vec::new(),
        }
    }

    /// [`ts_injected_languages`](Self::ts_injected_languages) for the most recent
    /// **preview** highlight ([`preview_highlights`](Self::preview_highlights) and
    /// friends), which has no buffer to key off. Read immediately after that call.
    /// Empty with no engine.
    pub fn ts_preview_injected_languages(&self) -> Vec<String> {
        match self.syntax.as_ref() {
            Some(engine) => engine.text_injected_languages(),
            None => Vec::new(),
        }
    }

    /// Highlight an **off-buffer** snippet — `text` in `language`, over `[first,
    /// last)` — without registering a buffer. For read-only surfaces (the picker
    /// preview pane) that paint a file which is not an open buffer. Empty when there
    /// is no engine, no grammar for `language`, or the engine can't parse a detached
    /// snippet (the wasm JS-side highlighter). Spans are in `text` coordinates.
    pub fn preview_highlights(
        &mut self,
        language: &str,
        text: &str,
        first: usize,
        last: usize,
    ) -> Vec<Span> {
        match self.syntax.as_mut() {
            Some(engine) => engine.highlight_text(language, text, first, last),
            None => Vec::new(),
        }
    }

    /// [`preview_highlights`](Self::preview_highlights) for a snippet that is **not a
    /// whole program**: a fenced code block inside LSP documentation. A hover block is
    /// a fragment (a struct field, a body-less signature) or an annotation dialect the
    /// server invented for display (`lua_ls` writes `function f(t: table)` into a
    /// ` ```lua ` fence), and a whole-file parse doesn't merely under-highlight those
    /// — it paints them *confidently wrong*. Empty with no engine / grammar.
    pub fn preview_highlights_fragment(
        &mut self,
        language: &str,
        text: &str,
        first: usize,
        last: usize,
    ) -> Vec<Span> {
        match self.syntax.as_mut() {
            Some(engine) => engine.highlight_fragment(language, text, first, last),
            None => Vec::new(),
        }
    }

    /// Install the **fragment contexts** for `language` — the framings the fragment
    /// highlighter tries, in order, when an LSP doc block doesn't parse on its own
    /// (`"struct __nx {\n%s\n}"`). Behind `nx.treesitter.fragment_context`, which
    /// also ships the per-language defaults. No-op with no engine.
    pub fn set_ts_fragment_context(&mut self, language: &str, templates: Vec<String>) {
        if let Some(engine) = self.syntax.as_mut() {
            engine.set_fragment_context(language, templates);
        }
    }

    /// [`preview_highlights`](Self::preview_highlights) plus the 0-based lines a
    /// full-line-background capture (`@markup.raw.block`) touches — the preview's
    /// `line_bg` under-layer, so a fenced code block's background survives the
    /// per-cell token merge (and the injected `>lua` syntax) instead of showing
    /// only on the whitespace between tokens. Empty backgrounds with no engine /
    /// grammar.
    pub fn preview_highlights_bg(
        &mut self,
        language: &str,
        text: &str,
        first: usize,
        last: usize,
    ) -> (Vec<crate::syntax::Span>, Vec<usize>) {
        match self.syntax.as_mut() {
            Some(engine) => engine.highlight_text_bg(language, text, first, last),
            None => (Vec::new(), Vec::new()),
        }
    }

    /// Tree-sitter foldable ranges for `buf` (`folds.scm` `@fold` captures),
    /// synced to the buffer's current content. Empty when there is no engine, no
    /// grammar, or no `folds.scm`. The fold source (`editor::fold`) turns these into
    /// per-line levels and then the nested fold tree.
    pub(crate) fn ts_folds(&mut self, buf: BufferId) -> Vec<crate::syntax::FoldRange> {
        self.sync_syntax_engine(buf);
        let folds = match self.syntax.as_mut() {
            Some(engine) => engine.folds(buf),
            None => Vec::new(),
        };
        self.echo_ts_query_errors();
        folds
    }

    /// Whether tree-sitter folds are *available* for `buf` — a grammar with a
    /// `folds.scm` is loaded. Lets the fold source tell "the query loaded but found
    /// nothing" apart from "the grammar isn't ready yet" (retry rather than clear).
    pub(crate) fn ts_folds_available(&self, buf: BufferId) -> bool {
        self.syntax
            .as_ref()
            .is_some_and(|engine| engine.folds_available(buf))
    }

    /// Byte ranges of `buf`'s `textobjects.scm` nodes captured as `capture` (e.g.
    /// `"function.inner"`) that contain `byte`, innermost first — the tree-sitter
    /// text-object source (`vif`, `daf`, …). Syncs the engine's shadow to the latest
    /// edits first, like every other engine query. Empty with no engine / grammar /
    /// query. See [`Editor::ts_text_object_range`](crate::Editor::ts_text_object_range).
    pub(crate) fn ts_text_objects_at(
        &mut self,
        buf: BufferId,
        capture: &str,
        byte: usize,
    ) -> Vec<(usize, usize)> {
        self.sync_syntax_engine(buf);
        let objects = match self.syntax.as_mut() {
            Some(engine) => engine.text_objects_at(buf, capture, byte),
            None => Vec::new(),
        };
        self.echo_ts_query_errors();
        objects
    }

    /// Echo whatever query-compile failures the engine queued during the call just
    /// made ([`SyntaxEngine::take_query_errors`]).
    ///
    /// A query nothing paints with — `folds.scm`, `indents.scm`, `textobjects.scm` —
    /// is compiled the first time a keypress asks for it rather than at grammar load,
    /// because compiling is what a load costs. A broken one therefore surfaces here,
    /// at the fold or the `vif` that wanted it, instead of as a load failure. The
    /// engine reports each one once, so this is a no-op on every later ask.
    fn echo_ts_query_errors(&mut self) {
        let Some(engine) = self.syntax.as_mut() else {
            return;
        };
        let errors = engine.take_query_errors();
        for reason in errors {
            self.echo(format!("treesitter: {reason}"));
        }
    }

    /// Whether `buf`'s treesitter parse is still in progress — a large file whose
    /// parse was cancelled by the engine's per-frame deadline and is being resumed
    /// across frames. The server reads this after a redraw to keep scheduling frames
    /// until the parse converges (progressive highlighting). `false` with no engine
    /// or no pending parse.
    pub fn ts_parse_pending(&self, buf: BufferId) -> bool {
        self.syntax
            .as_ref()
            .is_some_and(|engine| engine.parse_pending(buf))
    }

    /// Forget a deleted buffer's engine state (called from `:bdelete`).
    pub(crate) fn syntax_close(&mut self, id: BufferId) {
        if let Some(engine) = self.syntax.as_mut() {
            engine.close(id);
        }
        self.syntax_opened.remove(&id);
        self.ts_filetype.remove(&id);
        self.ts_enabled.remove(&id);
        self.commentstrings.remove(&id);
        self.foldexprs.remove(&id);
    }

    /// Target indent **width in columns** for `line` of the current buffer, the
    /// single policy + fallback chain behind every auto-indent site (`o`/`O`,
    /// insert-mode `Enter`, the `=` operators). Treesitter first (the engine's
    /// `indents.scm`, vim's `indentexpr`); with no treesitter verdict the
    /// grammar-free fallbacks take over, in vim's precedence: `smartindent`
    /// (bracket-aware) over plain `autoindent` (copy-previous). The legacy
    /// copy-previous that ts-indent leans on when its query is *available* but
    /// inconclusive for this line still fires too. Otherwise column 0.
    ///
    /// Syncs the engine first so the query sees the just-inserted `\n` (and any
    /// edits since the last redraw) — currency is required for a correct verdict.
    pub(crate) fn indent_for(&mut self, line: usize) -> usize {
        let buf = self.current_buffer_id();
        self.sync_syntax_engine(buf);
        let opts = self.buffer().options;
        let p = IndentParams {
            shiftwidth: opts.effective_shiftwidth(),
            tabstop: opts.effective_tabstop(),
        };
        // Resolve the treesitter verdict and whether ts-indent is even available,
        // both before releasing the engine borrow so the fallback can re-borrow self.
        let (ts, available) = match self.syntax.as_mut() {
            Some(s) => (s.indent(buf, line, &p), s.indents_available(buf)),
            None => (None, false),
        };
        self.echo_ts_query_errors();
        if let Some(w) = ts {
            return w;
        }
        // `smartindent` (bracket-aware) takes precedence over plain `autoindent`.
        if opts.smartindent {
            return self.smartindent_for(line);
        }
        // `autoindent`, or the ts-available-but-inconclusive copy-previous the
        // engine has always leaned on, copies the previous non-blank line's indent.
        if opts.autoindent || available {
            return self.autoindent_copy_prev(line).unwrap_or(0);
        }
        0
    }

    /// `smartindent` target indent **width in columns** for a freshly-opened
    /// `line`: the previous non-blank line's indent, plus one shiftwidth when that
    /// line *opens* a block — its last non-blank character is `{`, `(`, or `[`.
    /// The bracket-aware grammar-free autoindent (vim's `smartindent`), built on
    /// the same copy-previous base as [`Self::autoindent_copy_prev`]. The matching
    /// dedent when a *closing* bracket is typed lives on the insert path
    /// (`smartindent_close`), since it reacts to a keystroke, not a new line.
    fn smartindent_for(&self, line: usize) -> usize {
        let opts = self.buffer().options;
        let tabstop = opts.effective_tabstop();
        let Some(prev) = self.prev_nonblank_line(line) else {
            return 0;
        };
        let text = self.buffer().line(prev);
        let base = indent_width(&text, tabstop);
        if text.trim_end().ends_with(['{', '(', '[']) {
            base + opts.effective_shiftwidth()
        } else {
            base
        }
    }

    /// The nearest non-blank line strictly above `line`, or `None` if there is
    /// none — the shared scan behind both grammar-free autoindents.
    fn prev_nonblank_line(&self, line: usize) -> Option<usize> {
        let mut r = line.checked_sub(1)?;
        loop {
            if !self.buffer().line(r).trim().is_empty() {
                return Some(r);
            }
            r = r.checked_sub(1)?;
        }
    }

    /// Copy the indent **width in columns** of the nearest non-blank line above
    /// `line`, or `None` if there is none — the grammar-free autoindent the
    /// engine falls back to when its query is inconclusive.
    fn autoindent_copy_prev(&self, line: usize) -> Option<usize> {
        let tabstop = self.buffer().options.effective_tabstop();
        let prev = self.prev_nonblank_line(line)?;
        Some(indent_width(&self.buffer().line(prev), tabstop))
    }

    /// What re-indenting line `line` to visual width `width` would do: the
    /// whitespace run it would lay down, the byte length of the line's existing
    /// leading whitespace, and whether the two already agree (so the edit would
    /// rewrite nothing). The one place the "already correct" test is spelled.
    fn indent_plan(&self, line: usize, width: usize) -> (String, usize, bool) {
        let opts = self.buffer().options;
        let s = self.buffer().line(line);
        let old_ws = s.bytes().take_while(|b| *b == b' ' || *b == b'\t').count();
        let fill = fill_indent(0, width, opts.effective_tabstop(), opts.expandtab);
        let same = old_ws == fill.len() && s.starts_with(&fill);
        (fill, old_ws, same)
    }

    /// Is line `line` already indented to exactly what `width` would lay down —
    /// i.e. would [`set_line_indent`](Self::set_line_indent) rewrite nothing? The
    /// operators ask before editing: a `=` / `>` / `<` run that changes no line must
    /// leave `'modified'` and the undo history alone (neovim's `op_reindent` only
    /// calls `changed_lines()` for lines `set_indent()` reported as changed).
    pub(crate) fn line_indent_matches(&self, line: usize, width: usize) -> bool {
        self.indent_plan(line, width).2
    }

    /// Replace line `line`'s leading whitespace with indentation of visual width
    /// `width` (tabs/spaces per the buffer's `expandtab`/`tabstop`), returning the
    /// new leading-whitespace **byte length** — i.e. the column the first
    /// non-blank now begins at, where an auto-indent caller parks the cursor.
    pub(crate) fn set_line_indent(&mut self, line: usize, width: usize) -> usize {
        let (fill, old_ws, same) = self.indent_plan(line, width);
        if same {
            return fill.len(); // already correct — no edit (keeps `=` idempotent)
        }
        let start = self.buffer().line_start(line);
        if old_ws > 0 {
            self.buffer_mut().remove(start..start + old_ws);
        }
        if !fill.is_empty() {
            self.buffer_mut().insert(start, &fill);
        }
        self.buffer_mut().normalize();
        fill.len()
    }
}
