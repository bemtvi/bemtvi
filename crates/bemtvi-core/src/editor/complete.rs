//! The native completion engine's **core** half (`btv.complete`, Phase 4-A of the
//! unified float-list widget — `docs/specs/2026-06-14-btv-ui-float-widget.md`).
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
//! `docs/plans/2026-06-15-btv-complete-completion-engine.md`.

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
/// any user-supplied notation (`btv.complete.setup{ keys = … }`) into these; the
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
            // `<C-y>` (vim's accept) and `<CR>` both accept the **highlighted** row.
            // `<CR>` is safe as a default because the popup opens *noselect*: with
            // nothing highlighted (a just-auto-opened popup) `complete_accept` resolves
            // nothing and the key falls through to a newline (see `insert.rs`'s Confirm
            // arm). It only accepts once you've navigated to a row — so Enter picks the
            // highlighted completion without swallowing newlines you meant to type.
            confirm: vec![Key::ctrl('y'), Key::new(KeyCode::Enter)],
            abort: vec![Key::ctrl('e')],
        }
    }
}

/// How accepting a completion treats the text to the *right* of the cursor when the
/// caret sits in the middle of a word. `Insert` keeps the suffix — it replaces only
/// the typed prefix `[anchor .. cursor)`, so completing `AN_EX|AMPLE` with `AN_OTHER`
/// yields `AN_OTHERAMPLE`. `Replace` (the default) swaps the whole word
/// `[anchor .. word_end)`, yielding `AN_OTHER`. The default confirm keys use the
/// engine's configured behavior; a plugin can bind a second key to the *other* one
/// via `btv.complete.accept{ behavior = … }`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AcceptBehavior {
    /// Replace only the typed prefix, leaving any word suffix past the cursor.
    Insert,
    /// Replace the whole word the cursor is inside.
    #[default]
    Replace,
}

/// Engine configuration, set by `btv.complete.setup{}`. Disabled until a config
/// arrives, so an editor with no completion config behaves exactly as before.
#[derive(Clone, Debug)]
pub struct CompleteConfig {
    pub enabled: bool,
    /// Complete as you type (the engine opens/refreshes on each word keystroke).
    pub auto: bool,
    /// The **global open gate**: the popup opens once the prefix reaches this many
    /// characters — the *minimum* `min_chars` across every configured source (Lua
    /// resolves per-source `min_chars` and sends the min here), so the lowest-threshold
    /// source can show. Each source then contributes only once *its own* threshold is
    /// met (the native `buffer` seed by [`buffer_min_chars`](Self::buffer_min_chars),
    /// the async/native sources by their own gate server-side).
    pub min_chars: usize,
    /// Whether the native `buffer` word source contributes at all. `sources` is the
    /// list of sources to draw from, so a `setup{}` that names others and omits
    /// `buffer` gets no buffer words — the scan used to run regardless of whether it
    /// was listed, and a config offering only its own candidates saw them competing
    /// with every word already in the file.
    pub buffer_source: bool,
    /// The native `buffer` word source's own `min_chars`: its candidates are seeded
    /// only once the prefix reaches this length (independent of the global open gate,
    /// so `buffer` can sit at 3 while a snippet source shows from 2). A manual trigger
    /// (`<C-Space>`) bypasses it, like the global gate.
    pub buffer_min_chars: usize,
    pub keys: CompleteKeys,
    /// What the confirm keys do when the caret sits mid-word: replace the whole word
    /// (default) or only the typed prefix. See [`AcceptBehavior`].
    pub accept: AcceptBehavior,
    /// At least one configured source needs **off-input-path dispatch** — a Lua
    /// `complete` function or the built-in `lsp` source. When set, a trigger emits a
    /// `(gen, ctx)` onto [`Editor::complete_query_changes`] for the server to
    /// dispatch (debounced/async, generation-gated); a buffer-only config never
    /// does, so the whole keystroke path stays pure core.
    pub has_async: bool,
    /// Merge priority of the native `buffer` source — stamped onto its rows so the
    /// merged view ranks higher-priority sources (e.g. `lsp`) first. `0` when the
    /// `buffer` source is not configured.
    pub buffer_priority: i32,
    /// Whether the confirm key accepts the **first** row when nothing is selected yet
    /// (Enter-to-accept). The popup opens *noselect* — nothing highlighted until you
    /// navigate. With this off (default), confirm is inert until you pick a row, so a
    /// mapped `<CR>` still inserts a newline while the popup is up. With it on, confirm
    /// takes the top row even without navigating (an explicit selection still wins). A
    /// manual trigger preselects row 0 regardless, so this only changes the auto-typed,
    /// un-navigated case.
    pub confirm_first: bool,
    /// Show the **docs sidebar** beside the popup (the widget-spec `preview =
    /// "markdown"` kind): the selected item's documentation, rendered by the server
    /// from its LSP item cache (`completionItem/resolve` for lazy docs). On by
    /// default; a `buffer`-only config simply never has docs to show.
    pub docs: bool,
    /// Wrap a doc line wider than the docs float within the float (default on) rather
    /// than truncating it at the right edge. Sets the docs-float window's `wrap`
    /// option; height still clamps and the wheel scrolls vertically. Off ⇒ long lines
    /// truncate at the float's edge.
    pub docs_wrap: bool,
    /// The union of every configured source's **trigger chars** (`btv.complete.source
    /// { trigger = { chars = { ":" } } }`, Phase 4-E). When the char immediately left
    /// of the word being completed is one of these, the engine folds it into the
    /// prefix/anchor (so a source matches `:smi` and accept replaces from the `:`),
    /// opens regardless of `min_chars`, and skips the native `buffer` seed — the
    /// "wake a source only after its char" gate. Empty ⇒ no trigger-char sources, so
    /// the prefix is the plain word run (the 4-A/B/C/D behavior, unchanged).
    pub trigger_chars: Vec<char>,
}

impl Default for CompleteConfig {
    fn default() -> Self {
        CompleteConfig {
            enabled: false,
            auto: true,
            min_chars: 1,
            buffer_source: true,
            buffer_min_chars: 1,
            keys: CompleteKeys::default(),
            accept: AcceptBehavior::default(),
            has_async: false,
            buffer_priority: 0,
            confirm_first: false,
            docs: true,
            docs_wrap: true,
            trigger_chars: Vec::new(),
        }
    }
}

/// A snapshot of the completion site handed to an **async** source's `complete`
/// callback. It is a *copy*, never live editor state — the source runs off the
/// input path (debounced), by which point the cursor may have moved; the
/// generation token ([`Editor::complete_query_changes`]) is what keeps a stale
/// reply from landing, not this snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompleteCtx {
    /// The buffer the completion fired in (the current buffer's number).
    pub buf: u64,
    /// 0-based cursor row at trigger time.
    pub row: usize,
    /// 0-based cursor byte column at trigger time.
    pub col: usize,
    /// The word prefix left of the cursor being completed (also the match query
    /// the streamed candidates are ranked against, in core).
    pub prefix: String,
    /// A **manual** trigger (`<C-Space>` / `btv.complete.trigger()`) — it offers
    /// whatever is there, so every source bypasses its `min_chars` gate (both the Lua
    /// `btv._complete_run` gate and the native `lsp`/`snippets` gates read this).
    pub manual: bool,
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

    /// Apply an `btv.complete.setup{}` configuration (the server has already parsed
    /// any key notation into [`Key`]s).
    pub fn configure_complete(&mut self, config: CompleteConfig) {
        self.complete_config = config;
    }

    /// Whether the **open** completion popup belongs to a manual session — one an
    /// explicit trigger started, which therefore keeps following the prefix as the
    /// user types even with `auto` off. False when no popup is open, so a session can
    /// never outlive its popup and resurrect a later auto-opened one.
    pub(crate) fn complete_manual_session(&self) -> bool {
        self.complete_manual_session && self.completion_active()
    }

    /// End any manual completion session — called wherever the completion popup
    /// closes (abort, accept, a typed key that matched nothing, leaving insert).
    pub(crate) fn end_complete_manual_session(&mut self) {
        self.complete_manual_session = false;
    }

    /// Whether the docs float should wrap a doc line wider than its width (the
    /// configured `docs_wrap`, default on) rather than truncating at the edge.
    pub fn complete_docs_wrap(&self) -> bool {
        self.complete_config.docs_wrap
    }

    /// Whether the completion docs float is enabled (`btv.complete.setup{ docs = … }`,
    /// on by default). Off ⇒ the server never opens a docs float beside the popup.
    pub fn complete_docs_enabled(&self) -> bool {
        self.complete_config.docs
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
        if !self.complete_config.enabled {
            self.close_completion();
            return;
        }
        let (anchor, prefix) = self.complete_prefix();
        // A trigger char (`:` for the emoji example) wakes its source immediately,
        // regardless of `min_chars` — the char *is* the signal. A plain word prefix
        // still has to reach `min_chars` before the popup opens.
        let min_chars = if self.prefix_triggered(&prefix) {
            1
        } else {
            self.complete_config.min_chars
        };
        if prefix.chars().count() < min_chars {
            self.close_completion();
            return;
        }
        // Auto-typing is noselect — nothing highlighted until the user navigates,
        // so `<CR>` stays a newline.
        self.refresh_complete(anchor, prefix, false);
    }

    /// Manually open (or refresh) the completion popup — the `<C-x><C-n>` /
    /// `btv.complete.trigger()` path. Unlike the auto-trigger, this ignores both
    /// `auto` and `min_chars`: an explicit request completes whatever prefix is
    /// there, even an empty one (which offers every buffer word). Still a no-op
    /// when the engine is disabled, the LSP pmenu is up, or we are not in insert
    /// mode. No matches closes any open popup.
    ///
    /// Opening this way starts a **manual session**: the popup then follows the
    /// prefix through the edits that follow — narrowing as you type, widening as you
    /// backspace — even with `auto = false`, until it closes (accept, abort, `<Esc>`,
    /// or a prefix nothing matches). Each of those refreshes runs back through here,
    /// so the whole session keeps the manual contract: `min_chars` stays bypassed and
    /// the top row stays preselected, so a confirm key accepts without a separate
    /// navigation step.
    pub fn complete_manual_trigger(&mut self) {
        if !self.complete_config.enabled || !self.mode.is_insert() {
            return;
        }
        let (anchor, prefix) = self.complete_prefix();
        // An explicit trigger preselects the first match (vim-like) so `<C-y>` /
        // `<CR>` accept it immediately, without a separate navigation step.
        self.refresh_complete(anchor, prefix, true);
    }

    /// Shared open/refresh for both the auto and the manual trigger: bump the
    /// completion generation, seed the synchronous `buffer`-source candidates into
    /// a [`MenuKind::Complete`] menu anchored at the prefix, and — when at least one
    /// **async** source is configured — emit a `(gen, ctx)` onto
    /// [`Editor::complete_query_changes`] so the server dispatches that source off
    /// the input path. The popup stays open with just the buffer rows while async
    /// candidates stream in (they append, generation-gated, via
    /// [`Editor::menu_push`]); a buffer-only config with no match closes it as
    /// before. `preselect` highlights the first row up front (manual trigger).
    fn refresh_complete(&mut self, anchor: usize, prefix: String, preselect: bool) {
        self.complete_gen += 1;
        let gen = self.complete_gen;
        let has_async = self.complete_config.has_async;
        // In a *trigger* context (the prefix leads with a trigger char like `:`) the
        // native `buffer` source is suppressed — buffer words can't contain the
        // trigger char, so they'd never match anyway, and skipping the rope scan
        // hands the popup cleanly to the trigger-char source(s). A plain prefix seeds
        // the buffer words as before.
        // The native `buffer` source has its own `min_chars`: seed its words only once
        // the prefix is long enough (a manual trigger bypasses, offering everything).
        // The global open gate above is the *min* across sources, so the popup can be
        // open — for a lower-threshold source — before `buffer` contributes.
        let buffer_gated = !self.complete_config.buffer_source
            || (!preselect && prefix.chars().count() < self.complete_config.buffer_min_chars);
        let candidates = if self.prefix_triggered(&prefix) || buffer_gated {
            Vec::new()
        } else {
            self.buffer_candidates(&prefix)
        };
        // `keep_open` keeps an *empty* popup alive when an async source will stream
        // into it; without one, no buffer match closes the popup (the 4-A path).
        let buffer_priority = self.complete_config.buffer_priority;
        self.set_complete_menu(
            anchor,
            &prefix,
            candidates,
            preselect,
            gen,
            has_async,
            buffer_priority,
        );
        // A manual open starts (or continues) a manual *session*: the popup follows the
        // prefix through the edits that follow, even with `auto` off. Derived here from
        // what actually opened, so a manual trigger that matched nothing leaves no
        // session behind for the next keystroke to resurrect.
        self.complete_manual_session = preselect && self.completion_active();
        if has_async && self.completion_active() {
            self.complete_query_changes.push((
                gen,
                CompleteCtx {
                    buf: self.cur_buffer().0,
                    row: self.cursor.line,
                    col: self.cursor.col,
                    prefix,
                    manual: preselect,
                },
            ));
        }
    }

    /// An async source finished streaming (all sources for `gen` called `done()`):
    /// if `gen` is still the live generation and nothing matched, the completion
    /// popup is now confirmed-empty, so close it (completion has no prompt to keep
    /// up — an empty popup just lingers). A no-op for a superseded generation, or
    /// when rows did arrive. Driven by the server from the Lua `done()` reduction.
    pub fn complete_finish(&mut self, gen: u64) {
        if self.completion_active() && self.menu_generation() == gen && self.menu_view_is_empty() {
            self.close_completion();
        }
    }

    /// Accept the highlighted completion under the engine's configured
    /// [`AcceptBehavior`] (the default confirm-key path). See
    /// [`Editor::complete_accept_with`].
    pub fn complete_accept(&mut self) -> bool {
        self.complete_accept_with(self.complete_config.accept)
    }

    /// Accept the highlighted completion under an explicit `behavior` — the path a
    /// key bound to `btv.complete.accept{ behavior = … }` takes. For a native
    /// (`buffer`) row, replace the word span with the row's insert text and park the
    /// cursor past it; the span is `[anchor .. cursor)` under [`AcceptBehavior::Insert`]
    /// and `[anchor .. word_end)` under [`AcceptBehavior::Replace`] (swapping the whole
    /// word the caret sits inside, not just the typed prefix). For a **delegated**
    /// (`source_accept`) row — the `lsp` / `snippets` source — core can't apply the edit
    /// (it is LSP/encoding-agnostic), so it records the row's `key` on
    /// [`Editor::complete_accept_request`] and, under `Replace`, the word end on
    /// [`Editor::complete_accept_extend_to`] for the server to apply, and closes the
    /// menu. Returns whether anything was accepted — `false` when no menu is open or
    /// **nothing is selected yet** (noselect), so the caller lets the key fall through
    /// (e.g. `<CR>` makes a newline). The native edit groups into the surrounding
    /// insert session (the snapshot is already held).
    pub fn complete_accept_with(&mut self, behavior: AcceptBehavior) -> bool {
        let Some(acc) = self.complete_take_accept() else {
            return false;
        };
        let cursor_byte = self.cursor_char();
        // Under `Replace`, extend the replaced span rightward over the rest of the
        // word the caret sits inside; `Insert` stops at the cursor. `anchor` is always
        // ≤ the cursor (the prefix is the word chars left of it), so the span is valid.
        let remove_end = match behavior {
            AcceptBehavior::Insert => cursor_byte,
            AcceptBehavior::Replace => self.word_end_at_cursor(),
        };
        if acc.source_accept {
            // The server applies the source's edit (textEdit + additionalTextEdits)
            // after this key returns; the menu is already closed by `take_accept`. Hand
            // it the word end so a `Replace` accept swaps the whole word (core doesn't
            // apply the delegated edit); `None` ⇒ the server stops at the cursor.
            self.complete_accept_request = Some(acc.key);
            self.complete_accept_extend_to = (remove_end > cursor_byte).then_some(remove_end);
            return true;
        }
        self.buffer_mut().remove(acc.anchor..remove_end);
        self.buffer_mut().insert(acc.anchor, &acc.insert);
        self.buffer_mut().normalize();
        self.buffer_mut().modified = true;
        self.set_cursor_char_insert(acc.anchor + acc.insert.len());
        true
    }

    /// Absolute byte offset of the end of the word the cursor sits in: the cursor
    /// extended right over the run of word chars on its line. Equals the cursor when
    /// the char at the cursor is not a word char (caret at a word's end, or on a
    /// non-word char). The [`AcceptBehavior::Replace`] accept replaces up to here.
    fn word_end_at_cursor(&self) -> usize {
        let line = self.buffer().line(self.cursor.line);
        let col = self.cursor.col.min(line.len());
        let end = line[col..]
            .char_indices()
            .take_while(|&(_, c)| is_word_char(c))
            .last()
            .map_or(col, |(i, c)| col + i + c.len_utf8());
        self.buffer().line_start(self.cursor.line) + end
    }

    /// The word prefix being completed: `(anchor, prefix)`, where `anchor` is the
    /// absolute byte offset in the buffer at which the run of word chars left of
    /// the cursor begins, and `prefix` is that run's text. An empty prefix (the
    /// char left of the cursor is not a word char) returns `(cursor, "")`.
    fn complete_prefix(&self) -> (usize, String) {
        let line = self.buffer().line(self.cursor.line);
        let col = self.cursor.col.min(line.len());
        let before = &line[..col];
        let mut start = before
            .char_indices()
            .rev()
            .take_while(|&(_, c)| is_word_char(c))
            .last()
            .map_or(col, |(i, _)| i);
        // A trigger char (`btv.complete.source { trigger = { chars } }`) immediately
        // left of the word run is folded into the prefix — so an emoji source sees
        // `:smi`, not `smi`, and accepting it replaces from the `:`. Only one such
        // char is absorbed (the marker, not a run of them); a leading word with no
        // trigger char in front keeps the plain anchor.
        if !self.complete_config.trigger_chars.is_empty() {
            if let Some((i, c)) = before[..start].char_indices().next_back() {
                if self.complete_config.trigger_chars.contains(&c) {
                    start = i;
                }
            }
        }
        let prefix = before[start..].to_string();
        let anchor = self.buffer().line_start(self.cursor.line) + start;
        (anchor, prefix)
    }

    /// Whether `prefix` begins with a configured trigger char — a "trigger context"
    /// (`:smi` for the emoji source). Such a prefix opens the popup regardless of
    /// `min_chars`, suppresses the native `buffer` seed, and (Lua-side) routes only
    /// to the trigger-char source(s). Always `false` when no source declared a
    /// trigger char (`trigger_chars` empty).
    fn prefix_triggered(&self, prefix: &str) -> bool {
        prefix
            .chars()
            .next()
            .is_some_and(|c| self.complete_config.trigger_chars.contains(&c))
    }

    /// Whether the completion site at the cursor is in a **trigger context** (the
    /// word being completed is preceded by a configured trigger char). The server
    /// reads this to skip the built-in `lsp` source in a trigger context (an `:emoji`
    /// completion is not a language-server request). `false` when the engine has no
    /// trigger-char sources, so the `lsp` path is unchanged without them.
    pub fn completion_prefix_triggered(&self) -> bool {
        let (_, prefix) = self.complete_prefix();
        self.prefix_triggered(&prefix)
    }

    /// Unique words in the current buffer that fuzzy-match `prefix` are gathered
    /// here as raw candidates (the ranking + match spans are computed in
    /// [`Editor::set_complete_menu`]). Two occurrences are excluded so a word never
    /// completes to itself: the partial word being typed (a candidate equal to
    /// `prefix`), and — crucially when completing in the *middle* of a word — the
    /// exact word instance the cursor sits inside (e.g. the caret parked inside
    /// `AN_EXAMPLE` must not be offered `AN_EXAMPLE`). Only that one instance is
    /// skipped, keyed on its byte range, so a *distinct* occurrence of the same
    /// spelling elsewhere in the buffer is still a valid suggestion. Capped at a
    /// generous bound so a pathological buffer can't stall the keystroke.
    fn buffer_candidates(&self, prefix: &str) -> Vec<String> {
        const MAX_CANDIDATES: usize = 5000;
        // Absolute byte offset of the cursor: the word run that spans it is the one
        // being typed, so its single occurrence is skipped below.
        let cursor_byte = self.cursor_char();
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<String> = Vec::new();
        'lines: for row in 0..self.buffer().line_count() {
            let s = self.buffer().line(row);
            let line_off = self.buffer().line_start(row);
            let mut start = None;
            let mut push = |st: usize, end: usize, out: &mut Vec<String>| -> bool {
                // The word under the cursor is `[st, end]` with the caret anywhere in
                // that inclusive range (end included: the caret at the word's end is
                // still "inside" the word being typed).
                let under_cursor = line_off + st <= cursor_byte && cursor_byte <= line_off + end;
                !under_cursor && push_word(&s[st..end], prefix, &mut seen, out)
            };
            for (i, c) in s.char_indices() {
                match (is_word_char(c), start) {
                    (true, None) => start = Some(i),
                    (false, Some(st)) => {
                        if push(st, i, &mut out) && out.len() >= MAX_CANDIDATES {
                            break 'lines;
                        }
                        start = None;
                    }
                    _ => {}
                }
            }
            if let Some(st) = start {
                push(st, s.len(), &mut out);
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
