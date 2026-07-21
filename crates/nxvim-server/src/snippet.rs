//! The built-in **`snippets` completion source** and the snippet-engine effect
//! drains. The native snippet engine itself (parsing + the tabstop session) lives
//! in `nxvim-core`; this module is the thin server glue: it stores the snippets
//! `nx.snippet.add` registers, offers them as completion candidates for the
//! buffer's filetype, and expands the chosen body on accept.
//!
//! Like the `lsp` source, snippet rows carry `source_accept = true` so accept is
//! delegated back here — but their `MenuItem.key` is offset by
//! [`SNIPPET_COMPLETE_KEY_BASE`] so the accept drain can tell a snippet row from an
//! LSP row (the only other delegated-accept source) and route it to
//! [`EditHost::complete_snippet_accept`].

use crate::EditHost;

/// One registered snippet: the trigger word and its LSP-syntax body (string bodies
/// only in this phase; function bodies are rejected loud at `nx.snippet.add`).
#[derive(Clone, Debug)]
pub(crate) struct SnippetEntry {
    pub trigger: String,
    pub body: String,
}

/// `MenuItem.key` offset for `snippets`-source rows, disjoint from the `lsp`
/// source's keys (raw indices) so the delegated-accept drain can route by source.
/// Far above any plausible LSP item count, yet within a 32-bit `usize` (the wasm
/// edit-host's pointer width), so it can't overflow there.
pub(crate) const SNIPPET_COMPLETE_KEY_BASE: usize = 1 << 28;

/// `MenuItem.key` offset for a **plugin `on_accept`** row (a `nx.complete.source`
/// item carrying an `on_accept` callback), disjoint from and *above* the snippet
/// base so the delegated-accept drain routes by range — plugin first, then snippet,
/// then the `lsp` source's raw-index keys. `1 << 29` leaves the whole `[1<<28, 1<<29)`
/// band to snippets and stays within a 32-bit `usize` (the wasm edit-host's pointer
/// width), so it can't overflow there.
pub(crate) const PLUGIN_ACCEPT_KEY_BASE: usize = 1 << 29;

impl EditHost {
    /// Push the registered snippets for the current buffer's filetype into the open
    /// completion menu at generation `gen`. The engine's matcher ranks them against
    /// the typed prefix and merges them with the other sources by priority. Records
    /// the pushed entries so a delegated accept can find the body by key.
    pub(crate) fn complete_snippet_dispatch(&mut self, gen: u64) {
        if !self.complete_snippets_active {
            return;
        }
        let buffer = self.editor.current_buffer_id();
        let ft = self.editor.buffer_filetype(buffer).unwrap_or_default();
        let Some(entries) = self.snippet_store.get(&ft) else {
            self.snippet_complete.clear();
            return;
        };
        self.snippet_complete = entries.clone();
        let priority = self.complete_snippets_priority;
        let items: Vec<nxvim_core::MenuItem> = self
            .snippet_complete
            .iter()
            .enumerate()
            .map(|(i, e)| nxvim_core::MenuItem {
                label: e.trigger.clone(),
                key: SNIPPET_COMPLETE_KEY_BASE + i,
                preview: None,
                insert: Some(e.trigger.clone()),
                priority,
                source_accept: true,
                // Snippets carry no docs sidebar / lazy-resolve.
                doc: None,
                resolve: None,
                // The snippet trigger replaces the buffer prefix, not a cmdline span.
                replace: None,
            })
            .collect();
        if !items.is_empty() {
            self.editor.menu_push(items, gen);
        }
    }

    /// Apply a delegated `snippets` accept: expand entry `idx`'s body over the typed
    /// trigger word, entering the tabstop session. A malformed / unsupported body
    /// errors loud (echoed) and inserts nothing rather than dumping raw `$1` text.
    pub(crate) fn complete_snippet_accept(&mut self, idx: usize) {
        // A `Replace`-behavior accept (caret mid-word) hands us the word end; taken up
        // front so an early return can't leak it into the next accept.
        let extend_to = self.editor.complete_accept_extend_to.take();
        let Some(entry) = self.snippet_complete.get(idx).cloned() else {
            return;
        };
        let parsed = match nxvim_core::parse_snippet(&entry.body) {
            Ok(p) => p,
            Err(e) => {
                self.editor
                    .echo(format!("E5900: snippet '{}': {e}", entry.trigger));
                return;
            }
        };
        // Replace the typed trigger word (word_start..cursor) with the expansion.
        let row = self.editor.cursor.line;
        let col = self.editor.cursor.col;
        let line = self.editor.buffer().line(row);
        let word_start = trigger_word_start(&line, col);
        let line_start = self.editor.buffer().line_start(row);
        // Extend the replaced span over the rest of the word under a `Replace` accept.
        let end = extend_to.map_or(line_start + col, |e| (line_start + col).max(e));
        self.editor
            .expand_snippet(line_start + word_start, end, parsed);
    }

    /// Apply a `nx.snippet.add` registration to the per-filetype store.
    pub(crate) fn snippet_add(
        &mut self,
        filetype: String,
        triggers: Vec<String>,
        bodies: Vec<String>,
    ) {
        let list = self.snippet_store.entry(filetype).or_default();
        for (trigger, body) in triggers.into_iter().zip(bodies) {
            list.push(SnippetEntry { trigger, body });
        }
    }
}

/// Byte offset within `line` where the word ending at `cursor` begins — the run of
/// `[A-Za-z0-9_]` immediately left of the cursor (its start, or `cursor` if the
/// char left isn't a word char). The trigger word the `snippets` accept replaces
/// (and the plugin `on_accept` range — see [`EditHost::complete_plugin_accept`]).
pub(crate) fn trigger_word_start(line: &str, cursor: usize) -> usize {
    let col = cursor.min(line.len());
    line[..col]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| c.is_alphanumeric() || c == '_')
        .last()
        .map_or(col, |(i, _)| i)
}
