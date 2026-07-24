//! Helix's selection-first editing grammar (opt-in — see
//! `docs/plans/2026-07-21-helix-editing-model.md`).
//!
//! Where vim is verb→noun on a point cursor, Helix is noun→verb on a persistent
//! `anchor..head` **range**: a motion *re-selects* on every keystroke and a later
//! verb acts on the current selection immediately, with no operator-pending wait.
//! So this is a **separate parse step** ([`Editor::handle_helix`]) rather than a
//! remap onto the vim grammar's [`PendingCommand`] — the two disagree about what a
//! motion does. It shares the *engine*, though: the range set is the Phase-1
//! [`Selections`](crate::editor) seam, and each motion's landing is computed by the
//! same [`Editor::resolve_motion`]/[`Editor::apply_movement`] the vim grammar uses.
//!
//! **Scope.** The single-key grammar is hardwired here so Helix mode is usable and
//! testable without a plugin: counts, the motion alphabet (h/j/k/l + arrows,
//! `w`/`b`/`e` + the WORD variants `W`/`B`/`E`, a `f`/`t`/`F`/`T` find-target
//! stage, `0`/`^`/`$`, `G`, Home/End),
//! the immediate-apply verbs (`d`/`c`/`y`/`>`/`<`/`=`/`~`), a `r`-replace stage
//! (overwrite each selected char), `R` (replace with the yank), `J` (join the
//! selected lines), the selection verbs
//! (`x`/`X`/`%`/`_`/`;`/`,`/`Alt-;`/`Alt-,`/`(`/`)`/`Alt-(`/`Alt-)`/`&`/`C`/`Alt-C`),
//! register select
//! (`"{reg}` sets the register the next `y`/`d`/`c`/`p`/`P`/`R` uses), the view menu
//! (`z` — `zt`/`zz`/`zb` reposition the viewport), match mode (`m` — `mm` goto match,
//! `mi`/`ma` text objects, `ms`/`md`/`mr` surround add/delete/replace), the
//! regex prompts
//! (`s`/`S`/`K`/`Alt-K`), document search (`/`/`?`/`n`/`N` — the match becomes the
//! selection in normal mode, or is *added* as a new selection in select mode, see
//! [`Editor::run_search`]), and paste (`p`/`P`). Every verb key dispatches **by name**
//! through the same registry the bundled `nx.helix` plugin binds
//! ([`Editor::apply_helix_action`], see [`helix_key_action`]) — one dispatch, so a
//! hardwired key and its rebindable name can never drift. The plugin only layers
//! what a bare key can't reach (insert entry, the `g`/space menus, undo/redo) via
//! the `helix` keymap bucket. `v` toggles select (extend) mode; `<Esc>` leaves
//! select back to normal, or in normal collapses the selection to a point and
//! drops any secondaries.
//!
//! **Move-and-select vs. extend.** In [`Mode::HelixNormal`], a *word/find* motion
//! selects (`anchor` ← the old head, `head` ← where the motion landed) while a
//! plain *character/line* motion collapses the range to a 1-wide block at the
//! target. In [`Mode::HelixSelect`] every motion only moves `head`, keeping
//! `anchor` — growing or shrinking the existing selection.
//!
//! **Word motions re-select a region** (`w`/`b`/`e`, see
//! [`Editor::helix_word_step`]). Unlike a plain head-move they set *both* ends and are
//! computed by direct scanning, one line at a time, so a selection never spans a line
//! break. `w` re-anchors word-by-word (a repeat always advances, even across adjacent
//! runs like `on.`); `e` lands the head on the next word end and captures the leading
//! whitespace when it starts off a word end; `b` lands the head on the previous word
//! start, keeping the char it started on unless that char begins a word. When a line
//! has no word left in the motion's direction the head jumps to the next / previous
//! **non-empty** line (blank lines are skipped) and selects a fresh word there — so the
//! motions walk the whole document without ever selecting the newline. A line's leading
//! whitespace counts as its own word (Helix's rule): `w` onto an indented line selects
//! the indentation first, then the actual word. `W`/`B`/`E` are the WORD (long-word)
//! variants: the same scan with the word/punct classes collapsed, so a run of any
//! non-blank characters is one WORD.

use super::command::{
    CommandContinuation, CommandPending, FindKind, Motion, ObjectKind, ViewPlace,
};
use super::motions::{char_class, CharClass};
use super::search::SearchDir;
use super::selection::{Range, Selections};
use super::{CmdlineKind, Cursor, Editor, HelixRegexOp};

/// The in-flight Helix match-mode (`m`) sub-state — which key the sequence is
/// waiting on. Entered by `m`; each variant is the state *after* the keys typed so
/// far. See [`Editor::handle_helix_match`].
#[derive(Clone, Copy)]
pub(crate) enum HelixMatch {
    /// After `m` — awaiting `m` (goto match), `i`/`a` (text object), or
    /// `s`/`d`/`r` (surround add / delete / replace).
    Start,
    /// After `mi` — awaiting the text-object key (`(`, `"`, `w`, `p`, …).
    Inside,
    /// After `ma` — awaiting the text-object key.
    Around,
    /// After `ms` — awaiting the delimiter to wrap the selection with.
    Surround,
    /// After `md` — awaiting the surrounding delimiter to remove.
    SurroundDelete,
    /// After `mr` — awaiting the surrounding delimiter to replace.
    SurroundReplaceFrom,
    /// After `mr{from}` — awaiting the replacement delimiter.
    SurroundReplaceTo(char),
}
use crate::input::{Key, KeyCode};
use crate::mode::Mode;
use crate::search::SearchRegex;
use crate::unicode;

/// The Helix normal-mode single-key motion alphabet. Deliberately *not*
/// [`classify_motion`](super::command) — that encodes vim's aliases (Space→right,
/// `<CR>`→down, `;`/`,`→find-repeat) which Helix repurposes (Space is the leader,
/// `;` collapses the selection). Returns `None` for any other key.
fn helix_motion_key(key: Key) -> Option<Motion> {
    match (key.code, key.as_char()) {
        (KeyCode::Left, _) | (_, Some('h')) => Some(Motion::Left),
        (KeyCode::Right, _) | (_, Some('l')) => Some(Motion::Right),
        (KeyCode::Down, _) | (_, Some('j')) => Some(Motion::Down),
        (KeyCode::Up, _) | (_, Some('k')) => Some(Motion::Up),
        (KeyCode::Home, _) | (_, Some('0')) => Some(Motion::LineStart),
        (_, Some('^')) => Some(Motion::FirstNonBlank),
        (KeyCode::End, _) | (_, Some('$')) => Some(Motion::LineEnd),
        // `W`/`B`/`E` are the WORD (long-word) variants — the `big` flag collapses
        // the word/punct classes in the Helix scanner (see `helix_word_big`), so a
        // run of any non-blank chars is one WORD, as in vim's grammar.
        (_, Some('w')) => Some(Motion::Word(false)),
        (_, Some('W')) => Some(Motion::Word(true)),
        (_, Some('b')) => Some(Motion::BackWord(false)),
        (_, Some('B')) => Some(Motion::BackWord(true)),
        (_, Some('e')) => Some(Motion::EndWord(false)),
        (_, Some('E')) => Some(Motion::EndWord(true)),
        (_, Some('G')) => Some(Motion::GotoLine),
        _ => None,
    }
}

/// The named action a bare (unmodified) Helix verb key fires — the hardwired key
/// layout, expressed against the [`Editor::apply_helix_action`] registry so the
/// key dispatch and the plugin-facing names share one implementation. Motions,
/// counts, `v`, `:`, `f`/`t`, and `<Esc>` are grammar (stateful), not actions, and
/// stay in [`Editor::handle_helix`]; the Alt-modified verbs have their own table
/// there too (an alt key has no `as_char`). Returns `None` for any other key.
fn helix_key_action(ch: char) -> Option<&'static str> {
    Some(match ch {
        // Selection-set verbs — transform the range set itself, no text edit.
        'x' => "extend_line_below",
        'X' => "extend_line_above",
        '%' => "select_all",
        '_' => "trim_selections",
        ';' => "collapse_selection",
        ',' => "keep_primary_selection",
        'C' => "copy_selection_on_next_line",
        ')' => "rotate_selections_forward",
        '(' => "rotate_selections_backward",
        // Align every selection's start onto the same column.
        '&' => "align_selections",
        // Join the lines each selection spans.
        'J' => "join_selections",
        // Selection-regex prompts: select-within / split / keep.
        's' => "select_regex",
        'S' => "split_selection",
        'K' => "keep_selections",
        // Replace the whole selection with the last yank.
        'R' => "replace_selections_with_yanked",
        // Immediate-apply operators — act on the current selection *now*.
        'd' => "delete_selection",
        'c' => "change_selection",
        'y' => "yank",
        '>' => "indent",
        '<' => "unindent",
        '=' => "format_selections",
        '~' => "switch_case",
        // Paste after / before the selection.
        'p' => "paste_after",
        'P' => "paste_before",
        _ => return None,
    })
}

/// Build which-key continuations from `(key, desc)` pairs (all leaf commands — no
/// group prefixes), the Helix twin of `command.rs::conts`.
fn hx_conts(entries: &[(&str, &str)]) -> Vec<CommandContinuation> {
    entries
        .iter()
        .map(|&(key, desc)| CommandContinuation {
            key: key.to_string(),
            desc: desc.to_string(),
            group: false,
        })
        .collect()
}

/// Continuations of a lone `m` (match mode) — kept beside [`Editor::handle_helix_match`]
/// so a new match-mode key appears in both. `mm` completes; `mi`/`ma`/`ms`/`md`/`mr`
/// only *arm* a further stage, so they render as `+prefix` groups.
fn hx_match_continuations() -> Vec<CommandContinuation> {
    let group = |key: &str, desc: &str| CommandContinuation {
        key: key.to_string(),
        desc: desc.to_string(),
        group: true,
    };
    vec![
        CommandContinuation {
            key: "m".to_string(),
            desc: "Goto matching bracket".to_string(),
            group: false,
        },
        group("i", "Inside text object"),
        group("a", "Around text object"),
        group("s", "Surround add"),
        group("d", "Surround delete"),
        group("r", "Surround replace"),
    ]
}

/// The text-object alphabet offered after `mi` / `ma` — the keys
/// [`Editor::resolve_text_object`] accepts (vim objects + the tree-sitter captures),
/// kept beside it.
fn hx_object_continuations() -> Vec<CommandContinuation> {
    hx_conts(&[
        ("w", "Word"),
        ("W", "WORD"),
        ("p", "Paragraph"),
        ("s", "Sentence"),
        ("(", "Parentheses"),
        ("{", "Braces"),
        ("[", "Brackets"),
        ("<", "Angle brackets"),
        ("\"", "Double-quoted"),
        ("'", "Single-quoted"),
        ("`", "Backtick-quoted"),
        ("f", "Function"),
        ("a", "Argument"),
        ("c", "Comment"),
        ("t", "Class"),
    ])
}

/// Continuations of a lone `z` (view menu) — the Helix `z` keys (`zt`/`zz`/`zb`),
/// kept beside the `helix_view` handler.
fn hx_view_continuations() -> Vec<CommandContinuation> {
    hx_conts(&[
        ("t", "Cursor line to top"),
        ("z", "Cursor line to center"),
        ("b", "Cursor line to bottom"),
    ])
}

/// The key notation of a pending Helix find (`f`/`t`/`F`/`T`), for the which-key title.
fn hx_find_key(kind: FindKind) -> &'static str {
    match kind {
        FindKind::Find => "f",
        FindKind::Till => "t",
        FindKind::FindBack => "F",
        FindKind::TillBack => "T",
    }
}

/// The `(open, close)` delimiter pair a Helix surround (`ms`/`md`/`mr`) key names:
/// the four bracket kinds (either half selects the pair), and any other character —
/// a quote, `*`, … — as a same-char pair, matching Helix's `surround` alphabet.
fn surround_pair(c: char) -> (char, char) {
    match c {
        '(' | ')' => ('(', ')'),
        '{' | '}' => ('{', '}'),
        '[' | ']' => ('[', ']'),
        '<' | '>' => ('<', '>'),
        other => (other, other),
    }
}

/// A surrounding delimiter pair located around one selection's head (Helix
/// `md`/`mr`), as byte offsets: the opener is `[lo, io)`, the inner content
/// `[io, cl)`, the closer `[cl, hi)`.
#[derive(Clone, Copy)]
struct SurroundPair {
    lo: usize,
    io: usize,
    cl: usize,
    hi: usize,
}

/// One delimiter edit of a multi-selection `md`/`mr`: replace the bytes
/// `[at, at + old_len)` with `text` (empty `text` for a delete). Collected across
/// all selections, then applied high-offset-first (see [`Editor::apply_surround_ops`]).
struct SurroundOp {
    at: usize,
    old_len: usize,
    text: String,
}

impl SurroundOp {
    /// A plain delete (`md`): the `old_len` delimiter bytes at `at` are removed.
    fn delete(at: usize, old_len: usize) -> Self {
        SurroundOp {
            at,
            old_len,
            text: String::new(),
        }
    }
    /// A replacement (`mr`): the `old_len` delimiter bytes at `at` become `text`.
    fn text(at: usize, old_len: usize, text: String) -> Self {
        SurroundOp { at, old_len, text }
    }
    /// The net byte-length change this op introduces (`text.len() - old_len`).
    fn delta(&self) -> isize {
        self.text.len() as isize - self.old_len as isize
    }
}

/// Where byte offset `p` lands after all surround `ops` are applied: `p` plus the
/// net length change of every op that ends at or before it. (An op region never
/// contains a tracked selection boundary — those sit on inner content or outside any
/// pair — so the "at or before" test is exact.)
fn surround_shift(p: usize, ops: &[SurroundOp]) -> usize {
    let mut d = 0isize;
    for op in ops {
        if op.at + op.old_len <= p {
            d += op.delta();
        }
    }
    (p as isize + d).max(0) as usize
}

/// Whether a Helix word motion is a WORD (long-word) variant — `W`/`B`/`E`, the
/// `big` flag the vim grammar threads through [`Motion::Word`] and siblings. The
/// Helix scanner consumes it by collapsing the word/punct classes into one
/// (see [`Editor::hx_class`]). `false` for every non-word motion.
fn helix_word_big(m: Motion) -> bool {
    matches!(
        m,
        Motion::Word(true) | Motion::BackWord(true) | Motion::EndWord(true)
    )
}

/// A selection's pre-edit span in a multi-span splice-and-refit transform:
/// `(lo, hi_excl, orig_idx, head_high)` — the grapheme-extended byte bounds, the
/// index back into the unsorted range set, and whether the head sat at the high
/// end. Built (sorted ascending) by [`Editor::selection_spans`].
type SelSpan = (usize, usize, usize, bool);

/// Where a Helix insert-entry action ([`Editor::helix_enter_insert`]) collapses each
/// selection before opening Insert: `i` before the selection, `a` one grapheme past
/// its end, `I` at the line's first non-blank, `A` at end of line.
#[derive(Clone, Copy)]
enum HelixInsert {
    Before,
    After,
    LineStart,
    LineEnd,
}

impl Editor {
    /// Enter Helix's selection-first normal mode ([`Mode::HelixNormal`]) from
    /// vim's Normal mode: start from a single point selection at the cursor.
    pub(crate) fn enter_helix(&mut self) {
        self.reset_pending();
        self.reset_helix_pending();
        self.clear_secondary_cursors();
        self.helix = true;
        self.mode = Mode::HelixNormal;
        self.visual_anchor = self.cursor;
        self.clamp_cursor();
    }

    /// Leave Helix mode back to vim's Normal mode, collapsing to the single primary
    /// cursor. The inverse of [`Editor::enter_helix`] (the `:helix` toggle).
    pub(crate) fn leave_helix(&mut self) {
        self.reset_helix_pending();
        self.clear_secondary_cursors();
        self.helix = false;
        self.mode = Mode::Normal;
        self.clamp_cursor();
    }

    /// Clear every in-flight Helix sub-grammar state — the pending count, the
    /// `f`/`t` find target, `r` replace, `z` view, `"` register, `m` match mode,
    /// and the surround-edit origin. The shared teardown of entering/leaving Helix
    /// and `<Esc>` ([`helix_escape`](Self::helix_escape)).
    fn reset_helix_pending(&mut self) {
        self.helix_count = None;
        self.helix_find = None;
        self.helix_replace = false;
        self.helix_view = false;
        self.helix_register = false;
        self.helix_match = None;
        self.helix_surround_orig = None;
    }

    /// The Normal-family mode to return to when a mode ends (Insert, an ex command
    /// line) — [`Mode::HelixNormal`] inside a Helix session, else vim's
    /// [`Mode::Normal`]. So a Helix `c` that opens Insert resumes Helix on `<Esc>`.
    pub(crate) fn base_normal_mode(&self) -> Mode {
        if self.helix {
            Mode::HelixNormal
        } else {
            Mode::Normal
        }
    }

    /// The Helix counterpart of [`Editor::command_pending`] — the **source-B** oracle
    /// for the `nx.on_key_pending` (which-key) signal while a *native* Helix sub-grammar
    /// is mid-sequence. Helix's multi-key states (`m` match mode, `z` view, the
    /// `f`/`t`/`F`/`T` find-target, `r` replace-char, `"` register) are driven by
    /// [`handle_helix`](Self::handle_helix)'s own pending fields, not the vim
    /// [`PendingCommand`], so [`Editor::command_pending`] can't see them; the server
    /// calls this instead when the mode is Helix. `None` at a clean Helix boundary
    /// (nothing withheld). Finite prefixes (`m`, `mi`/`ma`, `z`) enumerate
    /// `continuations`; the any-character leaves (find/replace/surround delimiter) carry
    /// only a `label`, and `"` reuses the live register list.
    pub fn helix_command_pending(&self) -> Option<CommandPending> {
        let count = self.helix_count.map(|n| n.to_string()).unwrap_or_default();
        // `(suffix, label, continuations)` for the active stage; `keys` is `count + suffix`.
        let (suffix, label, continuations): (String, &'static str, Vec<CommandContinuation>) =
            if let Some(stage) = self.helix_match {
                match stage {
                    HelixMatch::Start => ("m".into(), "Match", hx_match_continuations()),
                    HelixMatch::Inside => ("mi".into(), "Inside object", hx_object_continuations()),
                    HelixMatch::Around => ("ma".into(), "Around object", hx_object_continuations()),
                    HelixMatch::Surround => ("ms".into(), "Surround add", Vec::new()),
                    HelixMatch::SurroundDelete => ("md".into(), "Surround delete", Vec::new()),
                    HelixMatch::SurroundReplaceFrom => {
                        ("mr".into(), "Surround replace", Vec::new())
                    }
                    HelixMatch::SurroundReplaceTo(from) => {
                        (format!("mr{from}"), "Replace with", Vec::new())
                    }
                }
            } else if self.helix_view {
                ("z".into(), "View", hx_view_continuations())
            } else if let Some(kind) = self.helix_find {
                (hx_find_key(kind).into(), "Find character", Vec::new())
            } else if self.helix_replace {
                ("r".into(), "Replace character", Vec::new())
            } else if self.helix_register {
                ("\"".into(), "Register", self.register_continuations())
            } else {
                return None;
            };
        Some(CommandPending {
            label,
            keys: format!("{count}{suffix}"),
            continuations,
        })
    }

    /// The Helix parse step — the noun→verb counterpart of [`Editor::handle_normal`].
    /// Grammar (stateful) keys are handled inline: counts, the motion alphabet
    /// (incl. the `f`/`t` find-target stage), `v` (toggle select), `:`, and `<Esc>`.
    /// Every verb key dispatches by name through [`Editor::apply_helix_action`]
    /// (see [`helix_key_action`]) so the hardwired layout and the plugin-rebindable
    /// names share one implementation.
    pub(crate) fn handle_helix(&mut self, key: Key) {
        self.message.clear();

        // A pending `f`/`t`/`F`/`T` consumes this key as its target character.
        if let Some(kind) = self.helix_find.take() {
            if key.code == KeyCode::Esc {
                self.helix_count = None;
            } else if let Some(target) = key.as_char() {
                let count = self.helix_count.take();
                self.apply_helix_motion(Motion::Find(kind, target), count);
            } else {
                self.helix_count = None;
            }
            return;
        }

        // A pending `r` consumes this key as the replacement character, overwriting
        // every selected character with it (a non-char key or `<Esc>` cancels).
        if self.helix_replace {
            self.helix_replace = false;
            self.helix_count = None;
            if key.code != KeyCode::Esc {
                if let Some(c) = key.as_char() {
                    self.helix_replace_selection(c);
                }
            }
            return;
        }

        // A pending `z` (view menu) consumes this key as its viewport placement:
        // `zt` top, `zz` center, `zb` bottom — cursor (and so the selection) stays
        // put. Any other key cancels.
        if self.helix_view {
            self.helix_view = false;
            let place = key.as_char().and_then(|c| match c {
                't' => Some(ViewPlace::Top),
                'z' => Some(ViewPlace::Center),
                'b' => Some(ViewPlace::Bottom),
                _ => None,
            });
            if let Some(place) = place {
                self.view_reposition(place, false, None);
            }
            return;
        }

        // A pending `"` consumes this key as the register name for the *next* verb:
        // `"ay` yanks into register `a`, `"ap` pastes from it. The following
        // register-writing/reading verb clears `pending.register` again (see the
        // verbs in `operators.rs`). A non-character key cancels.
        if self.helix_register {
            self.helix_register = false;
            self.pending.register = key.as_char();
            return;
        }

        // Match mode (`m`) is a multi-key sub-grammar (`mm`/`mi(`/`ma"`/`ms)`/`md(`/
        // `mr([`): each key advances or completes the pending state. `<Esc>` (a
        // non-character key) is handled inside as a cancel.
        if let Some(stage) = self.helix_match.take() {
            self.handle_helix_match(stage, key);
            return;
        }

        if key.code == KeyCode::Esc && !key.ctrl && !key.alt {
            self.helix_escape();
            return;
        }

        // Alt-modified verbs — matched on the raw `code` (an alt-modified key has no
        // `as_char`, and this is ahead of the `ch` filter besides). `Alt-;` flips
        // anchor/head; `Alt-,` drops the primary selection (keeping the rest);
        // `Alt-C`/`Alt-K` (`<A-S-c>`/`<A-S-k>` — shift carried by the flag, not the
        // letter's case) copy onto the previous line / keep only selections *without*
        // a match.
        if key.alt {
            let name = match key.code {
                KeyCode::Char(';') => Some("flip_selections"),
                KeyCode::Char(',') => Some("remove_primary_selection"),
                // `Alt-)` / `Alt-(` rotate the selection *contents* (the text moves
                // between selections; the ranges stay). No shift flag — `(`/`)` are
                // already their own characters.
                KeyCode::Char(')') => Some("rotate_selection_contents_forward"),
                KeyCode::Char('(') => Some("rotate_selection_contents_backward"),
                KeyCode::Char('c') if key.shift => Some("copy_selection_on_prev_line"),
                KeyCode::Char('k') if key.shift => Some("remove_selections"),
                _ => None,
            };
            if let Some(name) = name {
                self.fire_helix_key_action(name);
                return;
            }
        }

        // Viewport scrolling — exactly like vim: `<C-d>`/`<C-u>` half-page,
        // `<C-f>`/`<C-b>` full-page, and PageDown/PageUp half-page. Matched on the
        // raw `code` (a ctrl key has no `as_char` past the filter below).
        if key.ctrl && !key.alt {
            match key.code {
                KeyCode::Char('d') => return self.helix_scroll(true, true),
                KeyCode::Char('u') => return self.helix_scroll(true, false),
                KeyCode::Char('f') => return self.helix_scroll(false, true),
                KeyCode::Char('b') => return self.helix_scroll(false, false),
                _ => {}
            }
        }
        match key.code {
            KeyCode::PageDown => return self.helix_scroll(true, true),
            KeyCode::PageUp => return self.helix_scroll(true, false),
            _ => {}
        }

        let ch = key.as_char().filter(|_| !key.ctrl && !key.alt);

        // Count digits — `0` is a digit only once a count is already building (else
        // it's the line-start motion), matching the vim grammar.
        if let Some(d) = ch.and_then(|c| c.to_digit(10)) {
            if d != 0 || self.helix_count.is_some() {
                self.helix_count = Some(self.helix_count.unwrap_or(0) * 10 + d as usize);
                return;
            }
        }

        // `:` opens the ex command line (so `:helix` can toggle back out, and
        // ex-commands stay reachable). The line resumes Helix mode on close.
        if ch == Some(':') {
            self.helix_count = None;
            self.enter_command();
            return;
        }

        // Document search — `/`/`?` open the search prompt (which resumes Helix on
        // close, selecting the match), `n`/`N` repeat the last search forward /
        // backward. The count prefixes the search (`3/pat` finds the 3rd match).
        match ch {
            Some('/') => {
                let count = self.helix_count.take().unwrap_or(1);
                self.enter_search(SearchDir::Forward, count);
                return;
            }
            Some('?') => {
                let count = self.helix_count.take().unwrap_or(1);
                self.enter_search(SearchDir::Backward, count);
                return;
            }
            Some('n') => {
                let count = self.helix_count.take().unwrap_or(1);
                self.search_repeat(true, count);
                return;
            }
            Some('N') => {
                let count = self.helix_count.take().unwrap_or(1);
                self.search_repeat(false, count);
                return;
            }
            _ => {}
        }

        // `v` toggles select (extend) mode.
        if ch == Some('v') {
            self.mode = if self.mode == Mode::HelixSelect {
                Mode::HelixNormal
            } else {
                Mode::HelixSelect
            };
            self.helix_count = None;
            return;
        }

        // `f`/`t`/`F`/`T` arm a find awaiting its target character.
        if let Some(kind) = ch.and_then(FindKind::from_key) {
            self.helix_find = Some(kind);
            return;
        }

        // `r` arms replace: the next key is the character that overwrites the
        // selection. A char-argument verb (like `f`/`t`), so it stays grammar here
        // rather than in the count-only [`Editor::apply_helix_action`] registry.
        if ch == Some('r') {
            self.helix_replace = true;
            self.helix_count = None;
            return;
        }

        // `m` opens match mode — a multi-key sub-grammar consumed above on the next
        // key(s). Multi-key with char arguments, so (like `r`/`f`/`t`) it lives here,
        // not in the count-only action registry.
        if ch == Some('m') {
            self.helix_match = Some(HelixMatch::Start);
            return;
        }

        // `z` opens the view menu — the next key (`z`/`t`/`b`) repositions the
        // viewport, consumed above. A two-key grammar, so it lives here too.
        if ch == Some('z') {
            self.helix_view = true;
            return;
        }

        // `"` selects the register for the next verb — the register name is consumed
        // above. Two-key with a char argument, so it lives here (like `r`/`z`).
        if ch == Some('"') {
            self.helix_register = true;
            return;
        }

        if let Some(m) = helix_motion_key(key) {
            let count = self.helix_count.take();
            self.apply_helix_motion(m, count);
            return;
        }

        // The verb keys — each dispatched by its rebindable name (see
        // [`helix_key_action`]); the registry reads (and clears) `helix_count`.
        if let Some(name) = ch.and_then(helix_key_action) {
            self.fire_helix_key_action(name);
            return;
        }

        // Any other key is inert (unbound); drop a half-typed count so it doesn't
        // bleed into the next command.
        self.helix_count = None;
    }

    /// Viewport scroll in a Helix mode — the `<C-d>`/`<C-u>`/`<C-f>`/`<C-b>` and
    /// PageUp/PageDown keys, delegating to the same [`scroll_half`](Editor::scroll_half)
    /// / [`scroll_page`](Editor::scroll_page) vim uses (so the cursor rides the
    /// scroll). The primary selection then follows: collapsed to a point at the new
    /// cursor in [`Mode::HelixNormal`] (a scroll is a plain move), or extended —
    /// anchor held — in [`Mode::HelixSelect`], like every other motion.
    fn helix_scroll(&mut self, half: bool, down: bool) {
        self.helix_count = None;
        if half {
            self.scroll_half(down);
        } else {
            self.scroll_page(down);
        }
        if self.mode != Mode::HelixSelect {
            self.visual_anchor = self.cursor;
        }
        self.clamp_cursor();
    }

    /// Fire a hardwired-key verb through the named-action registry. The names come
    /// from the static tables in [`Editor::handle_helix`] / [`helix_key_action`], so
    /// a failure is a table/registry mismatch — a programmer error worth a loud
    /// panic (we are in a Helix mode, the registry's only other error case).
    fn fire_helix_key_action(&mut self, name: &'static str) {
        self.apply_helix_action(name, None)
            .expect("hardwired helix key names a registered action");
    }

    /// `<Esc>` in a Helix mode: from select mode, return to normal (keeping the
    /// selection); from normal, collapse the primary range to a point and drop any
    /// secondary selections.
    fn helix_escape(&mut self) {
        self.reset_helix_pending();
        if self.mode == Mode::HelixSelect {
            self.mode = Mode::HelixNormal;
            return;
        }
        self.clear_secondary_cursors();
        self.visual_anchor = self.cursor;
    }

    /// Apply Helix motion `m` (optionally counted) to **every** selection, replacing
    /// the range set. The word motions (`w`/`b`/`e`) have Helix-specific range
    /// semantics — they *re-select* a whole word region, not just move the head — so
    /// they route through [`Editor::apply_helix_word_motion`]; the find motions
    /// (`f`/`t`/`F`/`T`) re-select cross-line via [`Editor::apply_helix_find`]. Every
    /// other (plain character/line) motion moves the head via the shared
    /// [`resolve_motion`](Editor::resolve_motion)/[`apply_movement`](Editor::apply_movement)
    /// engine (so curswant, fold-awareness, and EOL stickiness match vim); its anchor
    /// follows the move-and-select vs. extend rule (see the module docs).
    fn apply_helix_motion(&mut self, m: Motion, count: Option<usize>) {
        match m {
            Motion::Word(_) | Motion::BackWord(_) | Motion::EndWord(_) => {
                self.apply_helix_word_motion(m, count.unwrap_or(1));
                return;
            }
            // Find motions *re-select* to the target and — unlike vim — scan the
            // whole document, so they have their own cross-line handler.
            Motion::Find(kind, target) => {
                self.apply_helix_find(kind, target, count.unwrap_or(1));
                return;
            }
            _ => {}
        }
        let extend = self.mode == Mode::HelixSelect;
        let sel = self.selections();
        let mut ranges = Vec::with_capacity(sel.ranges.len());
        for r in &sel.ranges {
            let old_head = r.head;
            self.cursor = old_head;
            // The engine reads the count from the vim pending state; feed the Helix
            // count in (a bare `None` so an un-counted `G`/`gg` still means
            // last/first line), then clear it below.
            self.pending.count = count;
            let landed = match self.resolve_motion(m) {
                Some(mr) => {
                    self.apply_movement(mr);
                    self.cursor
                }
                None => old_head,
            };
            // These are the plain character/line motions (`h`/`l`/`j`/`k`/`0`/`$`/`G`/…):
            // collapse the range to a point at the target, or keep the anchor in extend
            // mode. (Word and find motions have their own re-selecting handlers above.)
            let anchor = if extend { r.anchor } else { landed };
            ranges.push(Range {
                anchor,
                head: landed,
            });
        }
        // Clear the borrowed count back out of the vim pending state.
        self.pending.count = None;

        let primary = sel.primary;
        self.set_selections(&Selections { ranges, primary });
        self.clamp_cursor();
    }

    /// Apply a Helix find-char motion (`f`/`t`/`F`/`T`, optionally counted) to every
    /// selection. Unlike vim's line-confined find, this scans the **whole document**
    /// for the target ([`Editor::helix_find_target`]), so a match on any later/earlier
    /// line is reached. Each selection *re-selects* from its old head to the target
    /// (in [`Mode::HelixSelect`] the anchor is held instead); a miss leaves that
    /// selection untouched, as vim's find-miss is a no-op.
    fn apply_helix_find(&mut self, kind: FindKind, target: char, count: usize) {
        let extend = self.mode == Mode::HelixSelect;
        let sel = self.selections();
        let mut ranges = Vec::with_capacity(sel.ranges.len());
        for r in &sel.ranges {
            let from = self.anchor_byte(r.head);
            match self.helix_find_target(kind, target, count, from) {
                Some(b) => ranges.push(Range {
                    anchor: if extend { r.anchor } else { r.head },
                    head: self.cursor_at_byte(b),
                }),
                None => ranges.push(*r),
            }
        }
        let primary = sel.primary;
        self.set_selections(&Selections { ranges, primary });
        self.last_find = Some((kind, target));
        self.clamp_cursor();
    }

    /// Byte offset of a Helix find-char motion from byte `from`, scanning the whole
    /// document in the motion's direction (Helix's `f`/`t`/`F`/`T` are *not* confined
    /// to the current line — a newline is just a non-matching char to skip over).
    /// `f`/`F` land on the `count`-th `target`; `t`/`T` stop one grapheme short of it.
    /// `None` when the target does not occur in that direction.
    fn helix_find_target(
        &self,
        kind: FindKind,
        target: char,
        count: usize,
        from: usize,
    ) -> Option<usize> {
        let count = count.max(1);
        let till = kind.till();
        if kind.forward() {
            let end = self.buffer().len_bytes();
            let mut i = self.next_grapheme_idx(from);
            let mut found = 0;
            while i < end {
                if self.char_at(i) == target {
                    found += 1;
                    if found == count {
                        return Some(if till { self.prev_grapheme_idx(i) } else { i });
                    }
                }
                i = self.next_grapheme_idx(i);
            }
            None
        } else {
            if from == 0 {
                return None;
            }
            let mut i = self.prev_grapheme_idx(from);
            let mut found = 0;
            loop {
                if self.char_at(i) == target {
                    found += 1;
                    if found == count {
                        return Some(if till { self.next_grapheme_idx(i) } else { i });
                    }
                }
                if i == 0 {
                    return None;
                }
                i = self.prev_grapheme_idx(i);
            }
        }
    }

    /// Apply a Helix word motion (`w`/`b`/`e`, optionally counted) to every
    /// selection. Unlike a plain head-move, each of these *re-selects* a word region
    /// — anchor and head both computed per [`Editor::helix_word_step`] — so a repeat
    /// walks word-by-word instead of getting stuck. In extend mode ([`Mode::HelixSelect`])
    /// only the head moves; the anchor stays put, growing the selection.
    fn apply_helix_word_motion(&mut self, m: Motion, count: usize) {
        let extend = self.mode == Mode::HelixSelect;
        let sel = self.selections();
        let mut ranges = Vec::with_capacity(sel.ranges.len());
        for r in &sel.ranges {
            let mut anchor_b = self.anchor_byte(r.anchor);
            let mut head_b = self.anchor_byte(r.head);
            for _ in 0..count {
                let (a, h) = self.helix_word_step(m, head_b);
                anchor_b = a;
                head_b = h;
            }
            let anchor = if extend {
                r.anchor
            } else {
                self.cursor_at_byte(anchor_b)
            };
            ranges.push(Range {
                anchor,
                head: self.cursor_at_byte(head_b),
            });
        }
        let primary = sel.primary;
        self.set_selections(&Selections { ranges, primary });
        self.clamp_cursor();
    }

    /// One step of a Helix word motion from byte `from`, returning the `(anchor, head)`
    /// byte pair (head inclusive) of the re-selected word region.
    ///
    /// The scan is done a line at a time ([`Editor::helix_word_step_in_line`]) so a
    /// selection never spans a line break. When the current line has no word left in the
    /// motion's direction, the head jumps to the **next / previous non-empty line** and
    /// selects a fresh word there (skipping blank lines) — so a repeat keeps walking the
    /// document, never getting stuck at end-of-line, but the newline is never selected.
    fn helix_word_step(&self, m: Motion, from: usize) -> (usize, usize) {
        let (anchor, head) = self.helix_word_step_in_line(m, from);
        let progressed = match m {
            Motion::Word(_) | Motion::EndWord(_) => head > from,
            Motion::BackWord(_) => head < from,
            _ => true,
        };
        if progressed {
            return (anchor, head);
        }
        // No word left on this line in the motion's direction: jump to the adjacent
        // non-empty line and select a fresh word there (never selecting the line break).
        let forward = matches!(m, Motion::Word(_) | Motion::EndWord(_));
        match self.helix_adjacent_content_line(from, forward) {
            Some(line) if forward => self.helix_first_word_on_line(m, line),
            Some(line) => self.helix_last_word_on_line(line, helix_word_big(m)),
            None => (anchor, head),
        }
    }

    /// One step of a Helix word motion **bounded to `from`'s line** — never crossing a
    /// line break or selecting the newline; a blank line has nothing to select. Each
    /// treats a run of the same [`CharClass`](super::motions::CharClass) (word / punct)
    /// as a "word", whitespace as the gap between them — and for the WORD variants
    /// (`W`/`B`/`E`, the motion's `big` flag) the two non-blank classes collapse, so a
    /// run of any non-blank chars is one WORD. The line-local core of
    /// [`Editor::helix_word_step`], which layers next-/prev-line jumping on top.
    ///
    /// - `w` — from mid-word, the rest of that word + trailing spaces; otherwise (on a
    ///   word end or whitespace) the *next* word + its trailing spaces. Head lands just
    ///   before the following word, so a repeat always advances (never stuck on a
    ///   boundary like `on.`). A line's leading whitespace is treated as a word too, so
    ///   from inside the indentation `w` selects the rest of it before the first word.
    /// - `e` — head on the next word end; anchor at `from`, or one past it when `from`
    ///   is itself a word end (so `e` off a word end captures the leading whitespace).
    /// - `b` — head on the previous word start; anchor at `from`, or one back when
    ///   `from` is itself a word start (so `b` keeps the char it started on unless that
    ///   char begins a word).
    fn helix_word_step_in_line(&self, m: Motion, from: usize) -> (usize, usize) {
        let (ls, le) = self.hx_line_bounds(from);
        // A blank / empty line (no content before the newline): nothing to select.
        if ls >= le {
            return (from, from);
        }
        let last = self.prev_grapheme_idx(le); // the line's last real char
        let big = helix_word_big(m);
        match m {
            Motion::Word(_) => {
                // A line's leading whitespace (indentation) is its own word, exactly
                // like a real one: from inside it `w` selects the rest of the
                // indentation, and only a `w` from its last char moves on to the first
                // word. (Whitespace *between* words is a gap and is skipped, as below.)
                let fw = self.hx_skip_blanks(ls, le);
                if from < fw {
                    let indent_end = self.prev_grapheme_idx(fw);
                    if from < indent_end {
                        return (from, indent_end.min(last));
                    }
                    // `from` is the last indent char — fall through to the first word.
                }
                // Anchor: mid-word keeps the rest of the current word; on a word end or
                // whitespace, advance to the next word start (within the line).
                let mut a = from;
                let mid_word = !self.hx_blank(a)
                    && a < last
                    && self.hx_class(a, big) == self.hx_class(self.next_grapheme_idx(a), big);
                if !mid_word {
                    if !self.hx_blank(a) {
                        a = self.next_grapheme_idx(a);
                    }
                    a = self.hx_skip_blanks(a, le);
                }
                let anchor = a.min(last);
                // Head: the word from the anchor + its trailing whitespace, landing just
                // before the next word.
                let mut i = anchor;
                if !self.hx_blank(i) {
                    i = self.next_grapheme_idx(self.hx_run_last(i, le, big));
                }
                i = self.hx_skip_blanks(i, le);
                let head = if i > anchor {
                    self.prev_grapheme_idx(i)
                } else {
                    anchor
                };
                (anchor, head.min(last))
            }
            Motion::EndWord(_) => {
                let anchor = if self.helix_line_word_end(from, le, big) {
                    self.next_grapheme_idx(from).min(last)
                } else {
                    from
                };
                // Advance one, skip whitespace, then run to the word's last char.
                let mut i = from;
                if i < last {
                    i = self.next_grapheme_idx(i);
                }
                i = self.hx_skip_blanks(i, le);
                if i < le {
                    i = self.hx_run_last(i, le, big);
                }
                (anchor, i.min(last))
            }
            Motion::BackWord(_) => {
                let anchor = if self.helix_line_word_start(from, ls, big) {
                    if from > ls {
                        self.prev_grapheme_idx(from)
                    } else {
                        ls
                    }
                } else {
                    from
                };
                // Step back one, skip whitespace, then run to the word's first char.
                let mut i = from;
                if i > ls {
                    i = self.prev_grapheme_idx(i);
                }
                while i > ls && self.hx_blank(i) {
                    i = self.prev_grapheme_idx(i);
                }
                if !self.hx_blank(i) {
                    i = self.hx_run_first(i, ls, big);
                }
                (anchor, i)
            }
            _ => (from, from),
        }
    }

    /// The `[start, content_end)` byte bounds of `from`'s line. The content end is
    /// the position of the trailing newline — the bound every within-line word scan
    /// stops at, so no motion selects the line break.
    fn hx_line_bounds(&self, from: usize) -> (usize, usize) {
        let line = self.buffer().byte_to_line(from);
        let ls = self.buffer().line_start(line);
        (ls, ls + self.buffer().line(line).len())
    }

    /// The first non-blank byte at or after `i`, bounded by `le` (returned when the
    /// rest of the line is blank).
    fn hx_skip_blanks(&self, i: usize, le: usize) -> usize {
        let mut j = i;
        while j < le && self.hx_blank(j) {
            j = self.next_grapheme_idx(j);
        }
        j
    }

    /// The last char of the same-[`CharClass`] run containing non-blank `i`,
    /// scanning forward bounded by the line-content end `le`.
    fn hx_run_last(&self, i: usize, le: usize, big: bool) -> usize {
        let cat = self.hx_class(i, big);
        let mut j = i;
        while self.next_grapheme_idx(j) < le && self.hx_class(self.next_grapheme_idx(j), big) == cat
        {
            j = self.next_grapheme_idx(j);
        }
        j
    }

    /// The first char of the same-[`CharClass`] run containing non-blank `i`,
    /// scanning backward bounded by the line start `ls`.
    fn hx_run_first(&self, i: usize, ls: usize, big: bool) -> usize {
        let cat = self.hx_class(i, big);
        let mut j = i;
        while j > ls && self.hx_class(self.prev_grapheme_idx(j), big) == cat {
            j = self.prev_grapheme_idx(j);
        }
        j
    }

    /// The nearest **non-empty** line in the `forward` (else backward) direction from
    /// `from`'s line, or `None` when there is none. A line counts as empty when its
    /// content (excluding the newline) is all whitespace, so blank lines are skipped and
    /// a word motion jumps straight to the next line that actually has a word.
    fn helix_adjacent_content_line(&self, from: usize, forward: bool) -> Option<usize> {
        let cur = self.buffer().byte_to_line(from);
        if forward {
            ((cur + 1)..=self.last_line()).find(|&l| !self.buffer().line(l).trim().is_empty())
        } else {
            (0..cur)
                .rev()
                .find(|&l| !self.buffer().line(l).trim().is_empty())
        }
    }

    /// The `(anchor, head)` selecting the **first word** on `line` — the target of a
    /// forward (`w`/`e`) jump onto a fresh line. The anchor is the first non-blank char
    /// A line's **leading whitespace is its own word** (as in Helix): `w` onto an
    /// indented line selects just that indentation (a later `w` takes the actual first
    /// word); `e` folds the leading whitespace into the first word (anchor at the line
    /// start). With no indentation, `w` takes the first word + its trailing whitespace
    /// and `e` the first word alone. The anchor is picked so the whole first word is
    /// selected even when it is a single character.
    fn helix_first_word_on_line(&self, m: Motion, line: usize) -> (usize, usize) {
        let (ls, le) = self.hx_line_bounds(self.buffer().line_start(line));
        if ls >= le {
            return (ls, ls);
        }
        let last = self.prev_grapheme_idx(le);
        let big = helix_word_big(m);
        // The first non-blank char (the line has content, so this exists).
        let fw = self.hx_skip_blanks(ls, le);
        // `w` onto an indented line selects the leading whitespace run only.
        if matches!(m, Motion::Word(_)) && fw > ls {
            return (ls, self.prev_grapheme_idx(fw).min(last));
        }
        // Run to the last char of the first word.
        let word_end = if fw < le {
            self.hx_run_last(fw, le, big)
        } else {
            fw
        };
        if !matches!(m, Motion::Word(_)) {
            // `e` anchors at the line start, folding any leading whitespace in.
            return (ls, word_end.min(last));
        }
        // `w` (no indentation): the first word + its trailing whitespace.
        let j = self.hx_skip_blanks(self.next_grapheme_idx(word_end), le);
        let head = self.prev_grapheme_idx(j).max(word_end);
        (fw, head.min(last))
    }

    /// The `(anchor, head)` selecting the **last word** on `line` — the target of a
    /// backward (`b`) jump onto a fresh line: head on the word's first char, anchor on
    /// its last, so the whole word is selected with the cursor at its start.
    fn helix_last_word_on_line(&self, line: usize, big: bool) -> (usize, usize) {
        let (ls, le) = self.hx_line_bounds(self.buffer().line_start(line));
        if ls >= le {
            return (ls, ls);
        }
        let mut anchor = self.prev_grapheme_idx(le); // last real char
        while anchor > ls && self.hx_blank(anchor) {
            anchor = self.prev_grapheme_idx(anchor);
        }
        let head = if self.hx_blank(anchor) {
            anchor
        } else {
            self.hx_run_first(anchor, ls, big)
        };
        (anchor, head)
    }

    /// Whether byte `pos` (< line end `le`) is the last char of a word/punct run on its
    /// line — non-blank with a different-class (or line-end) successor.
    fn helix_line_word_end(&self, pos: usize, le: usize, big: bool) -> bool {
        if pos >= le || self.hx_blank(pos) {
            return false;
        }
        let next = self.next_grapheme_idx(pos);
        next >= le || self.hx_class(next, big) != self.hx_class(pos, big)
    }

    /// Whether byte `pos` is the first char of a word/punct run on its line — non-blank
    /// with a different-class predecessor (or at the line start `ls`).
    fn helix_line_word_start(&self, pos: usize, ls: usize, big: bool) -> bool {
        if self.hx_blank(pos) {
            return false;
        }
        pos <= ls || self.hx_class(self.prev_grapheme_idx(pos), big) != self.hx_class(pos, big)
    }

    /// Whether the char at byte `i` is whitespace (a word-motion gap).
    fn hx_blank(&self, i: usize) -> bool {
        char_class(self.char_at(i)) == CharClass::Blank
    }

    /// The [`CharClass`](super::motions::CharClass) of the char at byte `i`. With
    /// `big` (a WORD motion — `W`/`B`/`E`), word and punctuation collapse into one
    /// class, so a run of any non-blank chars scans as a single WORD.
    fn hx_class(&self, i: usize, big: bool) -> CharClass {
        match char_class(self.char_at(i)) {
            CharClass::Punct if big => CharClass::Word,
            c => c,
        }
    }

    // ----- selection-set verbs (no text edit) --------------------------------

    /// The byte column of the last character on `line` (where a charwise head sits
    /// to select through end-of-line), or 0 for an empty line.
    fn line_last_col(&self, line: usize) -> usize {
        let s = self.buffer().line(line);
        if s.is_empty() {
            0
        } else {
            unicode::prev_grapheme(&s, s.len())
        }
    }

    /// `Alt-;` — flip anchor and head at every selection, so the end that was
    /// moving is now fixed. The spans are unchanged; only which end moves.
    fn helix_flip(&mut self) {
        let mut sel = self.selections();
        for r in &mut sel.ranges {
            *r = r.flipped();
        }
        self.set_selections(&sel);
        self.clamp_cursor();
    }

    /// `x` (`down`) / `X` (`!down`) — extend each selection line-wise to whole
    /// lines. When it already covers whole lines, a repeat grows one line further in
    /// the motion's direction (`count` applies): `x` extends the bottom downward
    /// (Helix `extend_line_below`), `X` the top upward (`extend_line_above`). The
    /// head sits on the growing end — the last line for `x`, the first for `X`.
    fn helix_extend_line(&mut self, count: usize, down: bool) {
        let last_line = self.last_line();
        let mut sel = self.selections();
        for r in &mut sel.ranges {
            let mut a = r.anchor.line.min(r.head.line);
            let mut b = r.anchor.line.max(r.head.line);
            // Already covering whole line(s) start→end? Compare byte extents so the
            // check is independent of which end is anchor vs. head (they may share a
            // line). Grow in the direction if full; else snap to the line span first.
            let ab = self.anchor_byte(r.anchor);
            let hb = self.anchor_byte(r.head);
            let full = ab.min(hb) == self.buffer().line_start(a)
                && ab.max(hb) == self.buffer().byte_at(b, self.line_last_col(b));
            if full {
                if down {
                    b = (b + count).min(last_line);
                } else {
                    a = a.saturating_sub(count);
                }
            }
            let top = Cursor { line: a, col: 0 };
            let bot = Cursor {
                line: b,
                col: self.line_last_col(b),
            };
            // Head on the growing end so a repeat keeps extending that way.
            (r.anchor, r.head) = if down { (top, bot) } else { (bot, top) };
        }
        self.set_selections(&sel);
        self.clamp_cursor();
    }

    /// `%` — select the whole file as one selection (dropping any others).
    fn helix_select_all(&mut self) {
        let last = self.last_line();
        let range = Range {
            anchor: Cursor { line: 0, col: 0 },
            head: Cursor {
                line: last,
                col: self.line_last_col(last),
            },
        };
        self.set_selections(&Selections {
            ranges: vec![range],
            primary: 0,
        });
        self.clamp_cursor();
    }

    /// `_` — trim each selection to its non-whitespace content: drop leading and
    /// trailing whitespace, preserving which end is the head. A selection that is
    /// all whitespace collapses to a point at its head.
    fn helix_trim_selections(&mut self) {
        let mut sel = self.selections();
        for r in &mut sel.ranges {
            let a = self.anchor_byte(r.anchor);
            let h = self.anchor_byte(r.head);
            let (lo, hi) = (a.min(h), a.max(h)); // inclusive of both ends
            let mut first = lo;
            while first <= hi && self.char_at(first).is_whitespace() {
                first = self.next_grapheme_idx(first);
            }
            if first > hi {
                // All whitespace — collapse to a point at the head.
                r.anchor = r.head;
                continue;
            }
            let mut lastc = hi;
            while lastc > first && self.char_at(lastc).is_whitespace() {
                lastc = self.prev_grapheme_idx(lastc);
            }
            let (lo_c, hi_c) = (self.cursor_at_byte(first), self.cursor_at_byte(lastc));
            // Preserve direction: keep the head on the same side it was.
            if h >= a {
                r.anchor = lo_c;
                r.head = hi_c;
            } else {
                r.anchor = hi_c;
                r.head = lo_c;
            }
        }
        self.set_selections(&sel);
        self.clamp_cursor();
    }

    /// `;` — collapse every selection to a 1-wide cursor at its head.
    pub(crate) fn helix_collapse_to_cursor(&mut self) {
        let mut sel = self.selections();
        for r in &mut sel.ranges {
            r.anchor = r.head;
        }
        self.set_selections(&sel);
        self.clamp_cursor();
    }

    /// `,` — drop every selection but the primary.
    fn helix_keep_primary(&mut self) {
        let sel = self.selections();
        self.set_selections(&Selections {
            ranges: vec![sel.primary()],
            primary: 0,
        });
        self.clamp_cursor();
    }

    /// `Alt-,` — drop the primary selection, keeping the rest; the selection that
    /// followed it in document order becomes the new primary (clamped to the last).
    /// A no-op with a single selection (Helix keeps at least one).
    fn helix_remove_primary(&mut self) {
        let sel = self.selections();
        if sel.ranges.len() <= 1 {
            return;
        }
        let prim_head = self.anchor_byte(sel.primary().head);
        // Document order (by head byte); remove the primary, keep its slot's index as
        // the new primary so the *next* selection takes over (as Helix does).
        let mut ordered = sel.ranges.clone();
        ordered.sort_by_key(|r| self.anchor_byte(r.head));
        let pos = ordered
            .iter()
            .position(|r| self.anchor_byte(r.head) == prim_head)
            .unwrap_or(0);
        ordered.remove(pos);
        let primary = pos.min(ordered.len() - 1);
        self.set_selections(&Selections {
            ranges: ordered,
            primary,
        });
        self.clamp_cursor();
    }

    /// Clamp `col` onto `line` — never past its last character (a Helix head sits
    /// *on* a character), collapsing to 0 for an empty line.
    fn clamp_col_on_line(&self, line: usize, col: usize) -> Cursor {
        Cursor {
            line,
            col: col.min(self.line_last_col(line)),
        }
    }

    /// `C` / `Alt-C` — copy the primary selection onto the next (`down`) / previous
    /// line(s), one per `count`, each becoming the new primary so a repeat keeps
    /// walking. A copy whose line would fall off the buffer stops the run; columns
    /// clamp onto shorter target lines. This is how a Helix session grows a
    /// multi-selection without a regex.
    fn helix_copy_selection(&mut self, down: bool, count: usize) {
        let mut sel = self.selections();
        let src = sel.primary();
        let last = self.last_line() as i64;
        let step = if down { 1 } else { -1 };
        for k in 1..=count as i64 {
            let al = src.anchor.line as i64 + k * step;
            let hl = src.head.line as i64 + k * step;
            if al < 0 || hl < 0 || al > last || hl > last {
                break;
            }
            let anchor = self.clamp_col_on_line(al as usize, src.anchor.col);
            let head = self.clamp_col_on_line(hl as usize, src.head.col);
            sel.ranges.push(Range { anchor, head });
            sel.primary = sel.ranges.len() - 1;
        }
        self.set_selections(&sel);
        self.clamp_cursor();
    }

    // ----- selection-regex verbs (`s`/`S`/`K`/`Alt-K`) -----------------------

    /// Open the regex prompt for a selection transform (`s`/`S`/`K`/`Alt-K`). The
    /// typed pattern is applied on `<CR>` via [`Editor::helix_apply_regex`]; the
    /// line resumes [`Mode::HelixNormal`] on close (so the transform reads the live
    /// selection set). Mirrors [`Editor::enter_command`] for the Helix regex kind.
    pub(crate) fn enter_helix_regex(&mut self, op: HelixRegexOp) {
        // Capture the selection byte ranges *now* (still in a Helix mode, so
        // `selections` reads the anchors) for the live preview — the selection can't
        // change while the prompt is open.
        let sel = self.selections();
        self.helix_regex_ranges = sel
            .ranges
            .iter()
            .map(|r| {
                let a = self.anchor_byte(r.anchor);
                let h = self.anchor_byte(r.head);
                (a.min(h), self.next_grapheme_idx(a.max(h)))
            })
            .collect();
        self.cmdline_return_mode = Mode::HelixNormal;
        self.mode = Mode::Command;
        self.cmdline.clear();
        self.cmdline_col = 0;
        self.cmdline_kind = CmdlineKind::HelixRegex(op);
        self.hist_idx = None;
        self.message.clear();
        self.reset_pending();
    }

    /// Apply a compiled selection-regex transform (see [`HelixRegexOp`]) to the
    /// current selection set. `Select` replaces each selection with one selection
    /// per match within it; `Split` keeps the gaps between matches; `Keep`/`Remove`
    /// filter selections by whether they contain a match. An empty pattern or a
    /// compile error is a no-op (the latter echoes `E383`); a transform that would
    /// leave no selection is refused, keeping the current set.
    pub(crate) fn helix_apply_regex(&mut self, op: HelixRegexOp, pattern: &str) {
        // The prompt is closing — the live-preview ranges are no longer needed.
        self.helix_regex_ranges = Vec::new();
        if pattern.is_empty() {
            return;
        }
        let re = match self.compile_search(pattern) {
            Ok(re) => re,
            Err(e) => {
                self.echo(&e);
                return;
            }
        };
        let sel = self.selections();
        let mut ranges: Vec<Range> = Vec::new();
        for r in &sel.ranges {
            let a = self.anchor_byte(r.anchor);
            let h = self.anchor_byte(r.head);
            let lo = a.min(h);
            // The head byte is the *start* of the head character; the selection's
            // exclusive end is one grapheme past it.
            let end = self.next_grapheme_idx(a.max(h));
            let matches = self.helix_matches_in_range(&re, lo, end);
            match op {
                HelixRegexOp::Select => {
                    for (ms, me) in matches {
                        ranges.extend(self.range_from_bytes(ms, me));
                    }
                }
                HelixRegexOp::Split => {
                    let mut at = lo;
                    for (ms, me) in matches {
                        ranges.extend(self.range_from_bytes(at, ms));
                        at = me;
                    }
                    ranges.extend(self.range_from_bytes(at, end));
                }
                HelixRegexOp::Keep => {
                    if !matches.is_empty() {
                        ranges.push(*r);
                    }
                }
                HelixRegexOp::Remove => {
                    if matches.is_empty() {
                        ranges.push(*r);
                    }
                }
            }
        }
        if ranges.is_empty() {
            self.echo("no selections remaining");
            return;
        }
        self.set_selections(&Selections { ranges, primary: 0 });
        self.clamp_cursor();
    }

    /// Every non-empty match of `re` fully inside the byte range `[lo, end)`, as
    /// `(start, end)` byte ranges. Matching is line-by-line (like `/` search), so a
    /// match never crosses a line break; `^`/`$` anchor to the line edges.
    fn helix_matches_in_range(
        &self,
        re: &SearchRegex,
        lo: usize,
        end: usize,
    ) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        if lo >= end {
            return out;
        }
        let buf = self.buffer();
        let last_line = buf.byte_to_line(end - 1);
        for line in buf.byte_to_line(lo)..=last_line {
            let ls = buf.line_start(line);
            let text = buf.line(line);
            for (ms, me) in re.find_all(&text) {
                let (abs_s, abs_e) = (ls + ms, ls + me);
                if me > ms && abs_s >= lo && abs_e <= end {
                    out.push((abs_s, abs_e));
                }
            }
        }
        out
    }

    /// An inclusive-head [`Range`] over the exclusive byte span `[start, end)`
    /// (a regex match / split gap), or `None` when the span is empty. The head sits
    /// on the last grapheme of the span, matching Helix's inclusive convention.
    fn range_from_bytes(&self, start: usize, end: usize) -> Option<Range> {
        if end <= start {
            return None;
        }
        Some(Range {
            anchor: self.cursor_at_byte(start),
            head: self.cursor_at_byte(self.prev_grapheme_idx(end)),
        })
    }

    /// `)` / `(` — rotate which selection is primary, forward / backward through
    /// document order (by head position). A no-op with a single selection.
    fn helix_rotate_primary(&mut self, forward: bool) {
        let sel = self.selections();
        let n = sel.ranges.len();
        if n <= 1 {
            return;
        }
        // Order the selections by head position, find where the current primary
        // sits, and step to the neighbour.
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by_key(|&i| self.anchor_byte(sel.ranges[i].head));
        let pos = order
            .iter()
            .position(|&i| i == sel.primary)
            .expect("primary index present");
        let next = if forward {
            (pos + 1) % n
        } else {
            (pos + n - 1) % n
        };
        let primary = order[next];
        self.set_selections(&Selections {
            ranges: sel.ranges,
            primary,
        });
        self.clamp_cursor();
    }

    /// Every selection's pre-edit span as `(lo, hi_excl, orig_idx, head_high)`,
    /// sorted ascending by `lo` (document order) — the shape the multi-span
    /// splice-and-refit transforms share: edits then apply descending (so raw byte
    /// offsets stay valid) and [`refit_selections_over_spans`]
    /// (Self::refit_selections_over_spans) walks the running shift ascending.
    fn selection_spans(&self, sel: &Selections) -> Vec<SelSpan> {
        let mut spans: Vec<SelSpan> = sel
            .ranges
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let a = self.anchor_byte(r.anchor);
                let h = self.anchor_byte(r.head);
                (a.min(h), self.next_grapheme_idx(a.max(h)), i, h >= a)
            })
            .collect();
        spans.sort_by_key(|&(lo, ..)| lo);
        spans
    }

    /// Re-place the selection set after a per-span edit pass, in one shared walk of
    /// the offset math (the highest silent-corruption risk of the transform family
    /// — keep exactly one copy). For ascending span `k`, `fit(k, span)` describes
    /// its edit: `(own_shift, new_len, delta)` — how far the span's own edit pushed
    /// its start right (pad inserted *before* it), the span content's new byte
    /// length, and the net buffer-length change it contributes to every later
    /// span's position. Each selection keeps its original index and head
    /// orientation; empty new content collapses to a point (Helix's width-1
    /// minimum applies otherwise).
    fn refit_selections_over_spans(
        &mut self,
        sel: &Selections,
        spans: &[SelSpan],
        mut fit: impl FnMut(usize, SelSpan) -> (usize, usize, i64),
    ) {
        let mut ranges = sel.ranges.clone();
        let mut cum: i64 = 0;
        for (k, &span) in spans.iter().enumerate() {
            let (lo, _hi, idx, head_high) = span;
            let (own_shift, new_len, delta) = fit(k, span);
            let start = (lo as i64 + cum) as usize + own_shift;
            let end = start + new_len;
            let (low, high) = if new_len == 0 {
                let p = self.cursor_at_byte(start);
                (p, p)
            } else {
                (
                    self.cursor_at_byte(start),
                    self.cursor_at_byte(self.prev_grapheme_idx(end)),
                )
            };
            ranges[idx] = if head_high {
                Range {
                    anchor: low,
                    head: high,
                }
            } else {
                Range {
                    anchor: high,
                    head: low,
                }
            };
            cum += delta;
        }
        self.set_selections(&Selections {
            ranges,
            primary: sel.primary,
        });
        self.clamp_cursor();
    }

    /// `Alt-)` / `Alt-(` — rotate the *text contents* among the selections: forward
    /// moves each selection's text to the next selection in document order (wrapping),
    /// backward the other way. Unlike `)`/`(` (which move only *which* selection is
    /// primary), the ranges stay put — only what they hold moves, re-fitted to the
    /// rotated content's byte length. A no-op with a single selection. The whole
    /// rotation is one undo group.
    fn helix_rotate_contents(&mut self, forward: bool) {
        let sel = self.selections();
        let n = sel.ranges.len();
        if n <= 1 {
            return;
        }
        if !self.modifiable() {
            self.refuse_edit();
            return;
        }
        let spans = self.selection_spans(&sel);
        // The current text of each span, in document order.
        let texts: Vec<String> = spans
            .iter()
            .map(|&(lo, hi, ..)| self.buffer().text.slice(lo..hi).to_string())
            .collect();
        // Rotated assignment: forward makes each span *receive* from its predecessor
        // (so content moves forward to the next span); backward from its successor.
        let rotated: Vec<&str> = (0..n)
            .map(|i| {
                let src = if forward {
                    (i + n - 1) % n
                } else {
                    (i + 1) % n
                };
                texts[src].as_str()
            })
            .collect();
        self.push_undo();
        // Replace descending (highest span first) so lower byte offsets stay valid.
        for k in (0..n).rev() {
            let (lo, hi, ..) = spans[k];
            self.buffer_mut().remove(lo..hi);
            self.buffer_mut().insert(lo, rotated[k]);
        }
        self.buffer_mut().modified = true;
        // Each span holds its rotated content in place: no pad before it, the new
        // length is the rotated text's, and later spans shift by the length change.
        self.refit_selections_over_spans(&sel, &spans, |k, (lo, hi, ..)| {
            (
                0,
                rotated[k].len(),
                rotated[k].len() as i64 - (hi - lo) as i64,
            )
        });
    }

    /// `&` — align every selection's start onto the same column by inserting spaces
    /// before each: the target column is the largest selection-start column, and each
    /// shorter-column selection is padded to reach it (the classic "align the `=`
    /// signs" transform). The selection stays on its original content — the spaces sit
    /// before it. Columns are byte columns within the line (exact for the ASCII code
    /// this serves; a preceding wide/multibyte char would offset the visual column).
    /// A no-op when every start already shares a column. One undo group.
    fn helix_align_selections(&mut self) {
        let sel = self.selections();
        let spans = self.selection_spans(&sel);
        // Each span's byte column within its line, parallel to `spans`.
        let cols: Vec<usize> = spans
            .iter()
            .map(|&(lo, ..)| {
                let line = self.buffer().byte_to_line(lo);
                lo - self.buffer().byte_at(line, 0)
            })
            .collect();
        let target = cols.iter().copied().max().unwrap_or(0);
        if cols.iter().all(|&col| col == target) {
            return;
        }
        if !self.modifiable() {
            self.refuse_edit();
            return;
        }
        self.push_undo();
        // Insert descending so lower byte offsets stay valid.
        for (&(lo, ..), &col) in spans.iter().zip(&cols).rev() {
            let pad = target - col;
            if pad > 0 {
                self.buffer_mut().insert(lo, &" ".repeat(pad));
            }
        }
        self.buffer_mut().modified = true;
        // Each span keeps its content but its own pad is inserted *before* it, so
        // the pad shifts its start and every later span alike.
        self.refit_selections_over_spans(&sel, &spans, |k, (lo, hi, ..)| {
            let pad = target - cols[k];
            (pad, hi - lo, pad as i64)
        });
    }

    // ----- match mode (`m`): goto match / text objects / surround ------------

    /// Advance the match-mode (`m`) sub-grammar by one key (see [`HelixMatch`]).
    /// `mm` jumps to the matching bracket; `mi`/`ma` select a text object;
    /// `ms`/`md`/`mr` add / delete / replace a surrounding pair. The intermediate
    /// stages just re-arm [`Editor::helix_match`]; a non-character key (or `<Esc>`,
    /// which has no `as_char`) cancels, restoring any `mr` delimiter preview.
    fn handle_helix_match(&mut self, stage: HelixMatch, key: Key) {
        let Some(c) = key.as_char() else {
            self.helix_count = None;
            self.helix_cancel_surround_preview();
            return;
        };
        match stage {
            HelixMatch::Start => match c {
                'm' => self.helix_match_bracket(),
                'i' => self.helix_match = Some(HelixMatch::Inside),
                'a' => self.helix_match = Some(HelixMatch::Around),
                's' => self.helix_match = Some(HelixMatch::Surround),
                'd' => self.helix_match = Some(HelixMatch::SurroundDelete),
                'r' => self.helix_match = Some(HelixMatch::SurroundReplaceFrom),
                _ => self.helix_count = None,
            },
            HelixMatch::Inside => self.helix_textobject('i', c),
            HelixMatch::Around => self.helix_textobject('a', c),
            HelixMatch::Surround => self.helix_surround_add(c),
            HelixMatch::SurroundDelete => self.helix_surround_delete(c),
            // `mr{from}` doesn't apply yet — it lights up the `{from}` delimiters that
            // will be replaced (see [`Editor::helix_surround_preview`]); the `{to}`
            // key applies the swap and restores the original selection.
            HelixMatch::SurroundReplaceFrom => {
                self.helix_surround_preview(c);
                self.helix_match = Some(HelixMatch::SurroundReplaceTo(c));
            }
            HelixMatch::SurroundReplaceTo(from) => self.helix_surround_replace(from, c),
        }
    }

    /// `mi{obj}` / `ma{obj}` — replace every selection with the inner (`i`) or
    /// around (`a`) text object at its head, reusing the shared text-object
    /// dispatch ([`Editor::resolve_text_object`]) so the object alphabet is exactly
    /// vim's: the vim objects (`w`/`W`/`p`/`s`/pairs/quotes), the tree-sitter
    /// captures (`f`/`a`/`c`/`t`), and any `nx.textobject.map` registry key. An
    /// unknown object key or a head with no such object leaves that selection
    /// unchanged.
    fn helix_textobject(&mut self, ia: char, key: char) {
        let count = self.helix_count.take().unwrap_or(1);
        let sel = self.selections();
        let mut ranges = Vec::with_capacity(sel.ranges.len());
        for r in &sel.ranges {
            // The engine works from `self.cursor`; point it at this selection's head.
            self.cursor = r.head;
            self.clamp_cursor();
            match self.resolve_text_object(ia, key, count) {
                Some((lo, hi, _linewise)) if hi > lo => ranges.push(Range {
                    anchor: self.cursor_at_byte(lo),
                    head: self.cursor_at_byte(self.prev_grapheme_idx(hi)),
                }),
                _ => ranges.push(*r),
            }
        }
        self.set_selections(&Selections {
            ranges,
            primary: sel.primary,
        });
        self.clamp_cursor();
    }

    /// `mm` — move every selection's head to the bracket matching the one under it
    /// (like vim's `%`): from an opener to its closer, from a closer to its opener,
    /// honoring nesting. Collapses to a point at the match in [`Mode::HelixNormal`],
    /// extends (anchor held) in [`Mode::HelixSelect`]; a head not on a bracket is
    /// left where it is.
    fn helix_match_bracket(&mut self) {
        self.helix_count = None;
        const PAIRS: [(char, char); 4] = [('(', ')'), ('{', '}'), ('[', ']'), ('<', '>')];
        let extend = self.mode == Mode::HelixSelect;
        let sel = self.selections();
        let mut ranges = Vec::with_capacity(sel.ranges.len());
        for r in &sel.ranges {
            let at = self.anchor_byte(r.head);
            let c = self.char_at(at);
            let target = PAIRS.iter().find_map(|&(o, cl)| {
                if c == o {
                    self.find_match_close(o, cl, at)
                } else if c == cl {
                    self.find_unmatched_open(o, cl, at)
                } else {
                    None
                }
            });
            match target {
                Some(b) => {
                    let head = self.cursor_at_byte(b);
                    let anchor = if extend { r.anchor } else { head };
                    ranges.push(Range { anchor, head });
                }
                None => ranges.push(*r),
            }
        }
        self.set_selections(&Selections {
            ranges,
            primary: sel.primary,
        });
        self.clamp_cursor();
    }

    /// `ms{char}` — wrap **every** selection with the delimiter pair for `char`
    /// (`(`/`)`/`b` → `()`, a quote → itself, any other char → a same-char pair). As
    /// in Helix, the inserted delimiters become **part of** each selection (the head
    /// lands on the closer, the anchor on the opener), so a following verb acts on
    /// the whole wrapped span.
    fn helix_surround_add(&mut self, delim: char) {
        if !self.modifiable() {
            self.refuse_edit();
            return;
        }
        let (open, close) = surround_pair(delim);
        let (o, c) = (open.len_utf8(), close.len_utf8());
        let sel = self.selections();
        let spans = self.selection_spans(&sel);
        // Insert descending (highest span first) so each selection's original byte
        // offsets stay valid until it is wrapped.
        self.push_undo();
        for &(lo, hi, ..) in spans.iter().rev() {
            self.buffer_mut().insert(hi, &close.to_string());
            self.buffer_mut().insert(lo, &open.to_string());
        }
        self.buffer_mut().modified = true;
        // Each span grows by its own delimiters — which become part of it: the new
        // span starts at the opener (no shift before it) and its length includes
        // both delimiter chars, so the head lands on the closer.
        self.refit_selections_over_spans(&sel, &spans, |_, (lo, hi, ..)| {
            (0, hi - lo + o + c, (o + c) as i64)
        });
    }

    /// `md{char}` — delete the `char` pair surrounding **each** selection (a bracket
    /// or quote via the vim text object, or *any other* character — `*`, `|`, a
    /// letter — via a nearest-occurrence scan, matching what `ms{char}` can add).
    /// The original selection is preserved (shifted for the deletions), not moved to
    /// the inner content. Selections with no such pair are left untouched but shift
    /// along; a no-op when no selection has a matching pair.
    fn helix_surround_delete(&mut self, delim: char) {
        let sel = self.selections();
        let pairs: Vec<Option<SurroundPair>> =
            sel.ranges.iter().map(|r| self.pair_at(r, delim)).collect();
        // The delete operations, one per *unique* pair (two selections inside the same
        // pair share it): remove the opener `[lo, io)` and the closer `[cl, hi)`.
        let mut ops: Vec<SurroundOp> = Vec::new();
        let mut seen: Vec<usize> = Vec::new();
        for p in pairs.iter().flatten() {
            if seen.contains(&p.lo) {
                continue;
            }
            seen.push(p.lo);
            ops.push(SurroundOp::delete(p.lo, p.io - p.lo)); // opener → nothing
            ops.push(SurroundOp::delete(p.cl, p.hi - p.cl)); // closer → nothing
        }
        if ops.is_empty() {
            return;
        }
        if !self.modifiable() {
            self.refuse_edit();
            return;
        }
        self.apply_surround_ops(&sel, &ops);
    }

    /// `mr{from}` (before `{to}`) — light up the `{from}` delimiters that will be
    /// replaced: stash the real selection ([`Editor::helix_surround_orig`]) and make
    /// the live selection the two delimiter characters of every resolved pair, so the
    /// next repaint shows exactly what the pending `{to}` will swap. With no pair to
    /// replace, nothing is stashed and the selection is left as-is.
    fn helix_surround_preview(&mut self, from: char) {
        let sel = self.selections();
        let pairs: Vec<Option<SurroundPair>> =
            sel.ranges.iter().map(|r| self.pair_at(r, from)).collect();
        if pairs.iter().all(Option::is_none) {
            return;
        }
        // Highlight each unique pair's opener and closer as two point selections.
        let mut ranges: Vec<Range> = Vec::new();
        let mut seen: Vec<usize> = Vec::new();
        for p in pairs.iter().flatten() {
            if seen.contains(&p.lo) {
                continue;
            }
            seen.push(p.lo);
            let open = self.cursor_at_byte(p.lo);
            let close = self.cursor_at_byte(p.cl);
            ranges.push(Range {
                anchor: open,
                head: open,
            });
            ranges.push(Range {
                anchor: close,
                head: close,
            });
        }
        self.helix_surround_orig = Some(sel);
        self.set_selections(&Selections { ranges, primary: 0 });
        self.clamp_cursor();
    }

    /// Undo a [`Editor::helix_surround_preview`] without applying — restore the
    /// stashed real selection. A no-op when no `mr` preview is in flight.
    fn helix_cancel_surround_preview(&mut self) {
        if let Some(orig) = self.helix_surround_orig.take() {
            self.set_selections(&orig);
            self.clamp_cursor();
        }
    }

    /// `mr{from}{to}` — replace the `from` pair surrounding **each** selection with
    /// the `to` pair. Like [`Editor::helix_surround_delete`] but the delimiters are
    /// swapped rather than removed; `from`/`to` may be any character (same
    /// nearest-scan fallback), and the original selection is preserved (shifted).
    /// Entered only after [`Editor::helix_surround_preview`] lit up the `{from}`
    /// delimiters, which this restores before swapping.
    fn helix_surround_replace(&mut self, from: char, to: char) {
        // Restore the real selection stashed while the `{from}` delimiters previewed;
        // the swap acts on it, not on the transient delimiter highlight.
        if let Some(orig) = self.helix_surround_orig.take() {
            self.set_selections(&orig);
        }
        let (open2, close2) = surround_pair(to);
        let (open2, close2) = (open2.to_string(), close2.to_string());
        let sel = self.selections();
        let pairs: Vec<Option<SurroundPair>> =
            sel.ranges.iter().map(|r| self.pair_at(r, from)).collect();
        let mut ops: Vec<SurroundOp> = Vec::new();
        let mut seen: Vec<usize> = Vec::new();
        for p in pairs.iter().flatten() {
            if seen.contains(&p.lo) {
                continue;
            }
            seen.push(p.lo);
            ops.push(SurroundOp::text(p.lo, p.io - p.lo, open2.clone())); // opener → open2
            ops.push(SurroundOp::text(p.cl, p.hi - p.cl, close2.clone())); // closer → close2
        }
        if ops.is_empty() {
            return;
        }
        if !self.modifiable() {
            self.refuse_edit();
            return;
        }
        self.apply_surround_ops(&sel, &ops);
    }

    /// Apply the surround delete/replace `ops` (each a splice at a byte offset) to the
    /// buffer, then restore **the original selection set** — each selection kept where
    /// it was, shifted by the net length change the ops introduce before each end
    /// ([`surround_shift`]). This is Helix's behavior: `md`/`mr` edit the delimiters
    /// but leave your selection where it was, rather than jumping to the inner content.
    /// Edits run high-offset-first so raw offsets stay valid.
    fn apply_surround_ops(&mut self, sel: &Selections, ops: &[SurroundOp]) {
        // Capture each selection's endpoints as *pre-edit* byte offsets — the
        // `(line, col)` cursors would resolve against the wrong buffer once the
        // delimiters are spliced out.
        let pre: Vec<(usize, usize)> = sel
            .ranges
            .iter()
            .map(|r| (self.anchor_byte(r.anchor), self.anchor_byte(r.head)))
            .collect();
        self.push_undo();
        // Splice highest-offset first: a lower splice never shifts a higher one that
        // has not run yet.
        let mut order: Vec<&SurroundOp> = ops.iter().collect();
        order.sort_by_key(|op| std::cmp::Reverse(op.at));
        for op in order {
            self.buffer_mut().remove(op.at..op.at + op.old_len);
            if !op.text.is_empty() {
                self.buffer_mut().insert(op.at, &op.text);
            }
        }
        self.buffer_mut().modified = true;

        let mut ranges = sel.ranges.clone();
        for (idx, &(a, h)) in pre.iter().enumerate() {
            ranges[idx] = Range {
                anchor: self.cursor_at_byte(surround_shift(a, ops)),
                head: self.cursor_at_byte(surround_shift(h, ops)),
            };
        }
        self.set_selections(&Selections {
            ranges,
            primary: sel.primary,
        });
        self.clamp_cursor();
    }

    /// The pair of `delim` delimiters surrounding selection `range`, as a
    /// [`SurroundPair`] of byte offsets, or `None` when there is none. A bracket or
    /// quote resolves through the vim `a` text object (exactly as `da(` / `da"` do);
    /// any *other* character is treated as a same-char pair (`ms{char}` accepts the
    /// same alphabet) and found by scanning outward from the selection for the
    /// nearest occurrence on each side.
    fn pair_at(&mut self, range: &Range, delim: char) -> Option<SurroundPair> {
        if let Some(kind) = ObjectKind::from_key(delim) {
            return self.pair_at_object(range.head, kind);
        }
        // Arbitrary same-char delimiter: nearest occurrence before / after the span.
        let a = self.anchor_byte(range.anchor);
        let h = self.anchor_byte(range.head);
        let open = self.scan_char_back(delim, a.min(h))?;
        let close = self.scan_char_fwd(delim, a.max(h))?;
        if close <= open {
            return None;
        }
        Some(SurroundPair {
            lo: open,
            io: self.next_grapheme_idx(open),
            cl: close,
            hi: self.next_grapheme_idx(close),
        })
    }

    /// The nearest byte offset at or before `from` whose character is `ch`, or `None`.
    fn scan_char_back(&self, ch: char, from: usize) -> Option<usize> {
        let mut i = from.min(self.last_char_idx());
        loop {
            if self.char_at(i) == ch {
                return Some(i);
            }
            if i == 0 {
                return None;
            }
            i = self.prev_grapheme_idx(i);
        }
    }

    /// The nearest byte offset at or after `from` whose character is `ch`, or `None`.
    fn scan_char_fwd(&self, ch: char, from: usize) -> Option<usize> {
        let end = self.buffer().len_bytes();
        let mut i = from;
        while i < end {
            if self.char_at(i) == ch {
                return Some(i);
            }
            i = self.next_grapheme_idx(i);
        }
        None
    }

    /// [`Editor::pair_at`] for a known bracket/quote object `kind`, via the vim `a`
    /// text object at `head`.
    fn pair_at_object(&mut self, head: Cursor, kind: ObjectKind) -> Option<SurroundPair> {
        // The engine reads `self.cursor`; point it at this selection's head.
        self.cursor = head;
        self.clamp_cursor();
        let (lo, hi, _linewise) = self.text_object_range('a', kind, 1)?;
        if hi <= lo {
            return None;
        }
        Some(SurroundPair {
            lo,
            io: self.next_grapheme_idx(lo),
            cl: self.prev_grapheme_idx(hi),
            hi,
        })
    }

    // ----- named-action registry (Phase 5) -----------------------------------

    /// The named-verb seam the Helix-keymap plugin binds to (`nx._helix_action` →
    /// this). Where [`Editor::handle_helix`] hardwires the single-key grammar so
    /// Helix mode is usable without a plugin, this exposes every verb *by name* so a
    /// keymap can bind it (`nx.helix.actions.<name>`) — the goto/space menus and any
    /// user rebinding route through here. Each name maps to the same method the
    /// hardwired key fires; `count` is the explicit count a map passes (almost always
    /// `None`), falling back to the digits typed before the verb (`self.helix_count`,
    /// which the fall-through digits in `handle_helix` accumulate). Unknown names
    /// **fail loud** (per the no-silent-stubs rule) — the server surfaces the `Err`.
    ///
    /// `enable_helix` / `disable_helix` toggle the model, and `smart_case_on` /
    /// `smart_case_off` set the self-contained search-case default — these four work
    /// from any mode; every other action requires an active Helix mode and errors otherwise.
    pub fn apply_helix_action(&mut self, name: &str, count: Option<usize>) -> Result<(), String> {
        match name {
            "enable_helix" => {
                if !self.mode.is_helix() {
                    self.enter_helix();
                }
                return Ok(());
            }
            "disable_helix" => {
                if self.mode.is_helix() {
                    self.leave_helix();
                }
                return Ok(());
            }
            // The self-contained smart-case search toggle (`nx.helix.smart_case` /
            // `nx.helix.enable{ smart_case = … }`). Settable from any mode — it only
            // changes the default a later Helix search reads (see
            // [`Editor::search_ignorecase`]); it does *not* touch global options.
            "smart_case_on" => {
                self.helix_smart_case = true;
                return Ok(());
            }
            "smart_case_off" => {
                self.helix_smart_case = false;
                return Ok(());
            }
            _ => {}
        }
        if !self.mode.is_helix() {
            return Err(format!("helix action `{name}` requires Helix mode"));
        }
        // One take of the pending count: `n` for the counted verbs, `taken` (the raw
        // option) for the goto motions where `None` still means last/first line.
        let taken = count.or_else(|| self.helix_count.take());
        let n = taken.unwrap_or(1);
        match name {
            // Mode / insert entry.
            "normal_mode" => self.mode = Mode::HelixNormal,
            "select_mode" => self.mode = Mode::HelixSelect,
            "insert_mode" => self.helix_enter_insert(HelixInsert::Before),
            "append_mode" => self.helix_enter_insert(HelixInsert::After),
            "insert_at_line_start" => self.helix_enter_insert(HelixInsert::LineStart),
            "insert_at_line_end" => self.helix_enter_insert(HelixInsert::LineEnd),
            "open_below" => self.helix_open(true),
            "open_above" => self.helix_open(false),
            // Goto (the `g` menu).
            "goto_file_start" => self.apply_helix_motion(Motion::GotoLine, taken.or(Some(1))),
            "goto_last_line" => self.apply_helix_motion(Motion::GotoLine, None),
            "goto_line_start" => self.apply_helix_motion(Motion::LineStart, None),
            "goto_line_end" => self.apply_helix_motion(Motion::LineEnd, None),
            "goto_first_nonwhitespace" => self.apply_helix_motion(Motion::FirstNonBlank, None),
            // Selection-set verbs (no text edit).
            "flip_selections" => self.helix_flip(),
            "extend_line_below" => self.helix_extend_line(n, true),
            "extend_line_above" => self.helix_extend_line(n, false),
            "select_all" => self.helix_select_all(),
            "collapse_selection" => self.helix_collapse_to_cursor(),
            "keep_primary_selection" => self.helix_keep_primary(),
            "remove_primary_selection" => self.helix_remove_primary(),
            "trim_selections" => self.helix_trim_selections(),
            "copy_selection_on_next_line" => self.helix_copy_selection(true, n),
            "copy_selection_on_prev_line" => self.helix_copy_selection(false, n),
            "rotate_selections_forward" => self.helix_rotate_primary(true),
            "rotate_selections_backward" => self.helix_rotate_primary(false),
            "rotate_selection_contents_forward" => self.helix_rotate_contents(true),
            "rotate_selection_contents_backward" => self.helix_rotate_contents(false),
            "align_selections" => self.helix_align_selections(),
            "join_selections" => self.helix_join(n),
            "replace_selections_with_yanked" => self.helix_replace_with_yanked(),
            // Selection-regex prompts.
            "select_regex" => self.enter_helix_regex(HelixRegexOp::Select),
            "split_selection" => self.enter_helix_regex(HelixRegexOp::Split),
            "keep_selections" => self.enter_helix_regex(HelixRegexOp::Keep),
            "remove_selections" => self.enter_helix_regex(HelixRegexOp::Remove),
            // Immediate-apply operators.
            "delete_selection" => self.helix_operate('d'),
            "change_selection" => self.helix_operate('c'),
            "yank" => self.helix_operate('y'),
            "indent" => self.helix_operate('>'),
            "unindent" => self.helix_operate('<'),
            "format_selections" => self.helix_operate('='),
            "switch_case" => self.helix_operate('~'),
            // Paste (Helix's before/after-selection semantics).
            "paste_after" => self.helix_paste(true, n),
            "paste_before" => self.helix_paste(false, n),
            // Undo / redo (unreachable from the hardwired grammar — the plugin binds
            // `u`/`U` to these).
            "undo" => self.undo(),
            "redo" => self.redo(),
            _ => return Err(format!("unknown helix action: `{name}`")),
        }
        // A register selected with `"` applies to exactly one verb — clear it once
        // the verb that read/wrote it has run, so it doesn't leak to the next.
        if matches!(
            name,
            "delete_selection"
                | "change_selection"
                | "yank"
                | "paste_after"
                | "paste_before"
                | "replace_selections_with_yanked"
        ) {
            self.pending.register = None;
        }
        Ok(())
    }

    /// Helix insert-entry (`i`/`a`/`I`/`A`): collapse every selection to its insert
    /// point (see [`HelixInsert`]), then open Insert at all of them at once so
    /// multi-cursor insert survives. `<Esc>` resumes [`Mode::HelixNormal`] via
    /// [`Editor::base_normal_mode`].
    fn helix_enter_insert(&mut self, place: HelixInsert) {
        let mut sel = self.selections();
        for r in &mut sel.ranges {
            let a = self.anchor_byte(r.anchor);
            let h = self.anchor_byte(r.head);
            let (lo, hi) = (a.min(h), a.max(h));
            let pt = match place {
                HelixInsert::Before => lo,
                HelixInsert::After => self.next_grapheme_idx(hi),
                HelixInsert::LineStart => {
                    let line = self.buffer().byte_to_line(lo);
                    self.buffer().line_start(line) + self.first_non_blank(line)
                }
                HelixInsert::LineEnd => {
                    let line = self.buffer().byte_to_line(hi);
                    self.buffer().line_start(line) + self.buffer().line(line).len()
                }
            };
            let c = self.cursor_at_byte(pt);
            r.anchor = c;
            r.head = c;
        }
        self.set_selections(&sel);
        self.helix_count = None;
        // Keep the collapsed insert column (identity target), clamped to the append
        // column by `enter_insert_each`.
        self.enter_insert_each(|ed| ed.cursor.col);
    }

    /// Helix `o` / `O`: open a fresh line below / above **every** selection and enter
    /// multi-cursor Insert — the same per-selection open `i`/`a`/`I`/`A` give, reusing
    /// the vim per-cursor fan-out (`edit_each_cursor` + `open_line`) exactly as
    /// Normal-mode `o`/`O` do (`edit_each_cursor` short-circuits to a single
    /// `open_line` when there are no secondaries). The freshly-opened line moves each
    /// head off the line its anchor sat on, so the now-stale per-cursor anchors are
    /// dropped afterwards — each selection is then a caret on its new line (a secondary
    /// with no anchor mark reads back as a 1-wide selection). `<Esc>` resumes
    /// [`Mode::HelixNormal`] via the `helix` session flag, collapsing the primary too.
    fn helix_open(&mut self, below: bool) {
        self.helix_count = None;
        // Open a blank line at every selection and enter multi-cursor Insert, fanned
        // out per cursor exactly as Normal-mode `o`/`O` do (`edit_each_cursor`
        // short-circuits to a single `open_line` with no secondaries) — one undo
        // group. `open_line` leaves each head on its fresh line, already in Insert.
        // Leaving Insert collapses every selection to a caret on its new line (the
        // Helix Esc path clears the per-cursor anchors), as `i`/`a`/`I`/`A` do.
        self.edit_each_cursor(|ed| ed.open_line(below));
    }
}
