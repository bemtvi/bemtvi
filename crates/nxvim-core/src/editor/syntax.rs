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
fn indent_width(line: &str, tabstop: usize) -> usize {
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
        // is nothing to parse (no grammar for the path, or `vim.treesitter.stop`).
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

    /// The language the engine would highlight `buf` as, or `None` when there is
    /// nothing to parse. A `vim.treesitter.start` override wins (force a lang, or
    /// — `Some(None)` — explicitly stop); otherwise the path's extension decides.
    /// Both "stopped" and "no grammar for this path" collapse to `None` (the engine
    /// treats them identically). The server reads this on the async side to know
    /// which language's queries to resolve before the buffer's first highlight.
    pub fn ts_language_for(&self, buf: BufferId) -> Option<String> {
        match self.ts_override.get(&buf) {
            Some(Some(lang)) => Some(lang.clone()),
            Some(None) => None, // stopped via vim.treesitter.stop
            None => {
                let path = self.buffer_of(buf)?.path.as_deref();
                language_of_path(path).map(str::to_string)
            }
        }
    }

    /// Enable native treesitter highlighting for `buf` in `lang`, the bridge
    /// behind `vim.treesitter.start(buf, lang)` (ADR 0001, bridge #1). Forces
    /// `lang` regardless of the path's extension — so a buffer the extension
    /// table misses (a `.tex` file, a no-name scratch buffer) gets highlighted —
    /// and overrides a prior `stop`. The next `highlights` call opens the grammar.
    pub fn ts_start(&mut self, buf: BufferId, lang: String) {
        self.ts_override.insert(buf, Some(lang));
    }

    /// Clear any treesitter language override for `buf`, restoring the
    /// extension-derived default — the `:set filetype&` ("reset to default")
    /// path. Unlike [`Self::ts_stop`] (which records an explicit "stay dark"),
    /// this *removes* the override entirely, so a recognized extension highlights
    /// again. Drops the "opened in this language" marker so the next sync re-opens
    /// the buffer against whatever language the extension now resolves to.
    pub fn ts_reset(&mut self, buf: BufferId) {
        self.ts_override.remove(&buf);
        self.syntax_opened.remove(&buf);
        // If the buffer now resolves to *no* language (an extension the table
        // misses), drop the engine's parse so a query returns nothing — otherwise
        // the tree from the just-removed override would keep coloring it (the same
        // close `ts_stop` does, minus recording an explicit "stopped"). When it
        // resolves to a *different* language, the dropped `syntax_opened` marker
        // above makes the next sync re-open it.
        if self.ts_language_for(buf).is_none() {
            if let Some(engine) = self.syntax.as_mut() {
                engine.close(buf);
            }
        }
    }

    /// Disable native treesitter highlighting for `buf`, the bridge behind
    /// `vim.treesitter.stop(buf)`. Records an explicit "stopped" override (so the
    /// buffer stays dark even with a known extension) and drops the engine's
    /// parse state, so `highlights` returns nothing until a later `ts_start`.
    pub fn ts_stop(&mut self, buf: BufferId) {
        self.ts_override.insert(buf, None);
        if let Some(engine) = self.syntax.as_mut() {
            engine.close(buf);
        }
        self.syntax_opened.remove(&buf);
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

    /// Forget a deleted buffer's engine state (called from `:bdelete`).
    pub(crate) fn syntax_close(&mut self, id: BufferId) {
        if let Some(engine) = self.syntax.as_mut() {
            engine.close(id);
        }
        self.syntax_opened.remove(&id);
        self.ts_override.remove(&id);
    }

    /// Target indent **width in columns** for `line` of the current buffer, the
    /// single policy + fallback chain behind every auto-indent site (`o`/`O`,
    /// insert-mode `Enter`, the `=` operators). Treesitter first; then, only when
    /// ts-indent is *active* for the buffer but inconclusive for this line,
    /// copy-the-previous-non-blank-line's indent; then column 0.
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
        ts.or_else(|| available.then(|| self.autoindent_copy_prev(line)).flatten())
            .unwrap_or(0)
    }

    /// Copy the indent **width in columns** of the nearest non-blank line above
    /// `line`, or `None` if there is none — the grammar-free autoindent the
    /// engine falls back to when its query is inconclusive.
    fn autoindent_copy_prev(&self, line: usize) -> Option<usize> {
        let tabstop = self.buffer().options.effective_tabstop();
        let mut r = line.checked_sub(1)?;
        loop {
            let s = self.buffer().line(r);
            if !s.trim().is_empty() {
                return Some(indent_width(&s, tabstop));
            }
            r = r.checked_sub(1)?;
        }
    }

    /// Replace line `line`'s leading whitespace with indentation of visual width
    /// `width` (tabs/spaces per the buffer's `expandtab`/`tabstop`), returning the
    /// new leading-whitespace **byte length** — i.e. the column the first
    /// non-blank now begins at, where an auto-indent caller parks the cursor.
    pub(crate) fn set_line_indent(&mut self, line: usize, width: usize) -> usize {
        let opts = self.buffer().options;
        let s = self.buffer().line(line);
        let old_ws = s.bytes().take_while(|b| *b == b' ' || *b == b'\t').count();
        let fill = fill_indent(0, width, opts.effective_tabstop(), opts.expandtab);
        if old_ws == fill.len() && s.starts_with(&fill) {
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
