//! The native completion engine's **core** half (`nx.complete`, Phase 4-A of the
//! unified float-list widget — `docs/specs/2026-06-14-nx-ui-float-widget.md`).
//!
//! Completion is the widget's fourth orchestration and the one that inverts the
//! input model: **the buffer is the query**. The menu floats over the text
//! ([`MenuKind::Complete`]) but does *not* grab input — keystrokes keep editing
//! the document, and after each edit the engine recomputes the word prefix left of
//! the cursor and re-ranks candidates (here, the native `buffer` word-scan source).
//! Only the configured control keys (navigate / accept / abort) are intercepted,
//! and only while the menu is open. No async, no Lua per keystroke (ADR 0002
//! rule 4) — the whole `buffer`-source path is pure core, so it works identically
//! on every front end and on the wasm build.
//!
//! The server half (the `lsp` / `snippets` / plugin sources, debounce, generation
//! tokens) lands in later sub-phases; see
//! `docs/plans/2026-06-15-nx-complete-completion-engine.md`.

use std::collections::HashSet;

use super::menu::MenuKind;
use super::*;
use crate::input::{Key, KeyCode};

/// A buffer word is a maximal run of these — the same class vim's `iskeyword`
/// default and the motion engine ([`super::motions`]) treat as a word char.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The four engine control keys, resolved to concrete [`Key`]s. The server parses
/// any user-supplied notation (`nx.complete.setup{ keys = … }`) into these; the
/// defaults follow vim's insert-completion convention (`<C-n>`/`<C-p>` move,
/// `<C-y>` accepts, `<C-e>` aborts) plus `<Tab>`/`<S-Tab>` and the arrows.
#[derive(Clone, Debug)]
pub struct CompleteKeys {
    pub next: Vec<Key>,
    pub prev: Vec<Key>,
    pub confirm: Vec<Key>,
    pub abort: Vec<Key>,
}

impl Default for CompleteKeys {
    fn default() -> Self {
        let shift_tab = Key {
            code: KeyCode::Tab,
            ctrl: false,
            alt: false,
            shift: true,
        };
        CompleteKeys {
            next: vec![
                Key::ctrl('n'),
                Key::new(KeyCode::Tab),
                Key::new(KeyCode::Down),
            ],
            prev: vec![Key::ctrl('p'), shift_tab, Key::new(KeyCode::Up)],
            // `<CR>` is deliberately *not* a default confirm key: with the menu
            // auto-open, binding accept to `<CR>` would swallow newlines. Vim's
            // `<C-y>` is the accept key; a user who wants `<CR>` sets it explicitly.
            confirm: vec![Key::ctrl('y')],
            abort: vec![Key::ctrl('e')],
        }
    }
}

/// Engine configuration, set by `nx.complete.setup{}`. Disabled until a config
/// arrives, so an editor with no completion config behaves exactly as before.
#[derive(Clone, Debug)]
pub struct CompleteConfig {
    pub enabled: bool,
    /// Complete as you type (the engine opens/refreshes on each word keystroke).
    pub auto: bool,
    /// The prefix must be at least this many characters before the menu opens.
    pub min_chars: usize,
    pub keys: CompleteKeys,
}

impl Default for CompleteConfig {
    fn default() -> Self {
        CompleteConfig {
            enabled: false,
            auto: true,
            min_chars: 1,
            keys: CompleteKeys::default(),
        }
    }
}

/// Which engine control key a keystroke matched (while a completion menu is open).
#[derive(Clone, Copy)]
pub(crate) enum CompleteAction {
    Next,
    Prev,
    Confirm,
    Abort,
}

impl Editor {
    /// Whether a completion menu is open and grabbing the engine's control keys.
    pub fn completion_active(&self) -> bool {
        self.menu_kind() == Some(MenuKind::Complete)
    }

    /// Apply an `nx.complete.setup{}` configuration (the server has already parsed
    /// any key notation into [`Key`]s).
    pub fn configure_complete(&mut self, config: CompleteConfig) {
        self.complete_config = config;
    }

    /// Server-synced: whether the bespoke LSP completion pmenu is open. While it
    /// is, the engine stands down (and any open engine popup is closed) so the two
    /// never stack. Phase 4-C retires the bespoke pmenu and this flag.
    pub fn set_lsp_pmenu_open(&mut self, open: bool) {
        self.lsp_pmenu_open = open;
        if open {
            self.close_completion();
        }
    }

    /// Classify `key` against the configured control keys, when a completion menu
    /// is open. `None` ⇒ the key is not an engine control key (it edits the
    /// document and re-triggers the engine).
    pub(crate) fn complete_action(&self, key: &Key) -> Option<CompleteAction> {
        let k = &self.complete_config.keys;
        if k.next.contains(key) {
            Some(CompleteAction::Next)
        } else if k.prev.contains(key) {
            Some(CompleteAction::Prev)
        } else if k.confirm.contains(key) {
            Some(CompleteAction::Confirm)
        } else if k.abort.contains(key) {
            Some(CompleteAction::Abort)
        } else {
            None
        }
    }

    /// Recompute the completion popup from the current cursor: derive the word
    /// prefix immediately left of the cursor, gather + rank candidates from the
    /// `buffer` source, and open / refresh / close the menu accordingly. Called
    /// after each insert-mode edit when `auto` is on. A no-op (closing any open
    /// completion menu) when the engine is disabled, the prefix is shorter than
    /// `min_chars`, the bespoke LSP pmenu is up, or nothing matches.
    pub(crate) fn complete_trigger(&mut self) {
        if !self.complete_config.enabled || self.lsp_pmenu_open {
            self.close_completion();
            return;
        }
        let (anchor, prefix) = self.complete_prefix();
        if prefix.chars().count() < self.complete_config.min_chars {
            self.close_completion();
            return;
        }
        let candidates = self.buffer_candidates(&prefix);
        if candidates.is_empty() {
            self.close_completion();
            return;
        }
        self.set_complete_menu(anchor, &prefix, candidates);
    }

    /// Accept the highlighted completion: replace the typed prefix
    /// `[anchor .. cursor)` with the row's insert text and park the cursor just
    /// past it. A no-op when no completion menu is open. The edit groups into the
    /// surrounding insert session (the snapshot is already held).
    pub(crate) fn complete_accept(&mut self) {
        let Some((anchor, insert)) = self.complete_take_accept() else {
            return;
        };
        let cursor_byte = self.cursor_char();
        // `anchor` is always ≤ the cursor (the prefix is the word chars left of
        // it), so this replaces exactly the typed prefix.
        self.buffer_mut().remove(anchor..cursor_byte);
        self.buffer_mut().insert(anchor, &insert);
        self.buffer_mut().normalize();
        self.buffer_mut().modified = true;
        self.set_cursor_char_insert(anchor + insert.len());
    }

    /// The word prefix being completed: `(anchor, prefix)`, where `anchor` is the
    /// absolute byte offset in the buffer at which the run of word chars left of
    /// the cursor begins, and `prefix` is that run's text. An empty prefix (the
    /// char left of the cursor is not a word char) returns `(cursor, "")`.
    fn complete_prefix(&self) -> (usize, String) {
        let line = self.buffer().line(self.cursor.line);
        let col = self.cursor.col.min(line.len());
        let before = &line[..col];
        let start = before
            .char_indices()
            .rev()
            .take_while(|&(_, c)| is_word_char(c))
            .last()
            .map_or(col, |(i, _)| i);
        let prefix = before[start..].to_string();
        let anchor = self.buffer().line_start(self.cursor.line) + start;
        (anchor, prefix)
    }

    /// Unique words in the current buffer that fuzzy-match `prefix` are gathered
    /// here as raw candidates (the ranking + match spans are computed in
    /// [`Editor::set_complete_menu`]). The partial word being typed (a candidate
    /// equal to `prefix`) is excluded — it can't complete to itself. Capped at a
    /// generous bound so a pathological buffer can't stall the keystroke.
    fn buffer_candidates(&self, prefix: &str) -> Vec<String> {
        const MAX_CANDIDATES: usize = 5000;
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<String> = Vec::new();
        'lines: for row in 0..self.buffer().line_count() {
            let s = self.buffer().line(row);
            let mut start = None;
            for (i, c) in s.char_indices() {
                match (is_word_char(c), start) {
                    (true, None) => start = Some(i),
                    (false, Some(st)) => {
                        if push_word(&s[st..i], prefix, &mut seen, &mut out)
                            && out.len() >= MAX_CANDIDATES
                        {
                            break 'lines;
                        }
                        start = None;
                    }
                    _ => {}
                }
            }
            if let Some(st) = start {
                push_word(&s[st..], prefix, &mut seen, &mut out);
            }
            if out.len() >= MAX_CANDIDATES {
                break;
            }
        }
        out
    }
}

/// Record `word` as a candidate if it is new and not the prefix itself; returns
/// whether it was added (so the caller can stop at the cap).
fn push_word(word: &str, prefix: &str, seen: &mut HashSet<String>, out: &mut Vec<String>) -> bool {
    if word.is_empty() || word == prefix || !seen.insert(word.to_string()) {
        return false;
    }
    out.push(word.to_string());
    true
}
