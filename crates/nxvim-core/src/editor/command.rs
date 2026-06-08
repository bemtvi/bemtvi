//! The normal/visual command grammar **and** its executor.
//!
//! The key sequence is parsed in two clean halves. [`parse_step`] (pure: no
//! buffer, no `&mut`) is the *grammar* — it decides whether a key extends,
//! completes, or aborts a command, and emits a typed [`ResolvedCommand`].
//! [`Editor::execute`] is the *effect* — it applies that command through the
//! editing helpers. The typed motion / object / find enums are the contract
//! between the two: a new built-in is a new variant the compiler forces into
//! both arms, so they can never silently drift (this is what lets the keymap
//! matcher reuse `parse_step` as a read-only command oracle).

use super::*;
use crate::input::{Key, KeyCode};
use crate::mode::Mode;

/// A `<C-w>` window command: the second key after the `<C-w>` prefix, resolved by
/// [`parse_step`] and applied in [`Editor::execute`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowCmd {
    /// `<C-w>s` (horizontal) / `<C-w>v` (vertical) — split the focused window.
    Split(SplitDir),
    /// `<C-w>h/j/k/l` — move focus to the window in that direction.
    FocusDir(WinDir),
    /// `<C-w>w` (forward) / `<C-w>W` (backward) — cyclic focus.
    FocusCycle(bool),
    /// `<C-w>c` — close the focused window (refuses the last one).
    Close,
    /// `<C-w>o` — keep only the focused window.
    Only,
    /// `<C-w>q` — `:q`: close the focused window, or quit if it is the last.
    Quit,
    /// `<C-w>T` — move the focused window to a new tab page.
    ToNewTab,
    /// `<C-w>=` — equalize every window's size.
    Equalize,
    /// `<C-w>+` (grow) / `<C-w>-` (shrink) — change the focused window's height
    /// by the count (default 1).
    ResizeHeight(bool),
    /// `<C-w>>` (grow) / `<C-w><` (shrink) — change the focused window's width by
    /// the count (default 1).
    ResizeWidth(bool),
    /// `<C-w>_` — maximize the focused window's height.
    MaxHeight,
    /// `<C-w>|` — maximize the focused window's width.
    MaxWidth,
}

/// Resolve the key *after* `<C-w>` into a [`WindowCmd`], or `None` for a key that
/// is not a window command.
fn window_command(key: Key) -> Option<WindowCmd> {
    // A bare `<C-w><C-w>` is the same as `<C-w>w` (cyclic focus) in vim.
    if key.ctrl {
        return match key.code {
            KeyCode::Char('w') => Some(WindowCmd::FocusCycle(true)),
            _ => None,
        };
    }
    Some(match key.as_char()? {
        's' => WindowCmd::Split(SplitDir::Horizontal),
        'v' => WindowCmd::Split(SplitDir::Vertical),
        'w' => WindowCmd::FocusCycle(true),
        'W' => WindowCmd::FocusCycle(false),
        'h' => WindowCmd::FocusDir(WinDir::Left),
        'j' => WindowCmd::FocusDir(WinDir::Down),
        'k' => WindowCmd::FocusDir(WinDir::Up),
        'l' => WindowCmd::FocusDir(WinDir::Right),
        'c' => WindowCmd::Close,
        'o' => WindowCmd::Only,
        'q' => WindowCmd::Quit,
        'T' => WindowCmd::ToNewTab,
        '=' => WindowCmd::Equalize,
        '+' => WindowCmd::ResizeHeight(true),
        '-' => WindowCmd::ResizeHeight(false),
        '>' => WindowCmd::ResizeWidth(true),
        '<' => WindowCmd::ResizeWidth(false),
        '_' => WindowCmd::MaxHeight,
        '|' => WindowCmd::MaxWidth,
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum MotionKind {
    Exclusive,
    Inclusive,
    Linewise,
}

/// How a motion places the cursor when used as plain movement (not as an
/// operator's range). This is what drives vim's `curswant` column memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MoveAxis {
    /// Horizontal move: the resulting column becomes the new desired column.
    Horizontal,
    /// `$`/`End`: stick to end-of-line until a horizontal move clears it.
    EndOfLine,
    /// `gg`/`G`/etc.: jump to a line's first non-blank; resets desired column.
    LineAnchor,
    /// `j`/`k`: change line but keep the remembered desired column.
    VerticalKeep,
}

pub(crate) struct MotionResult {
    pub(crate) target: usize,
    pub(crate) kind: MotionKind,
    pub(crate) axis: MoveAxis,
}

impl MotionResult {
    /// A horizontal in-line motion (`h`/`l`/`0`/`^`/`w`/`b`/`e`/search-operator),
    /// with the caller-chosen exclusive/inclusive `kind`.
    pub(crate) fn horizontal(target: usize, kind: MotionKind) -> Self {
        Self {
            target,
            kind,
            axis: MoveAxis::Horizontal,
        }
    }

    /// The common exclusive horizontal motion (`h`, `l`, `0`, `^`, `w`, `b`).
    pub(crate) fn exclusive(target: usize) -> Self {
        Self::horizontal(target, MotionKind::Exclusive)
    }

    /// An inclusive horizontal motion (`e`, and `cw` acting like `ce`).
    pub(crate) fn inclusive(target: usize) -> Self {
        Self::horizontal(target, MotionKind::Inclusive)
    }

    /// A linewise motion to the start of `target`'s line, with the given `axis`
    /// (`VerticalKeep` for `j`/`k`, `LineAnchor` for `gg`/`G`/doubled operators).
    pub(crate) fn linewise(target: usize, axis: MoveAxis) -> Self {
        Self {
            target,
            kind: MotionKind::Linewise,
            axis,
        }
    }
}

/// A normal/visual cursor motion. The motion *alphabet* (which keys are motions)
/// lives in [`classify_motion`]; where each motion *lands* lives in
/// [`Editor::resolve_motion`]. Note `w`/`W`, `b`/`B`, `e`/`E` collapse to one
/// variant each — nxvim does not yet implement `WORD` motions, so the big-word
/// keys behave identically to their small-word counterparts (preserved here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Motion {
    Left,                         // h, <Left>, <BS>
    Right,                        // l, <Right>, <Space>
    LineStart,                    // 0, <Home>
    FirstNonBlank,                // ^
    LineEnd,                      // $, <End>
    Down,                         // j, <Down>
    Up,                           // k, <Up>
    GotoLine,                     // G  (count = target line, default last)
    GotoTop,                      // gg (count = target line, default first)
    Word,                         // w / W
    BackWord,                     // b / B
    EndWord,                      // e / E
    Find(FindKind, char),         // f/t/F/T {char}
    FindRepeat { reverse: bool }, // ; (same) / , (reversed)
    MarkJumpExact(char),          // `{mark}  (charwise exclusive)
    MarkJumpLine(char),           // '{mark}  (linewise, first non-blank)
}

/// The four find-char motions. `f`/`t` search forward, `F`/`T` backward; `t`/`T`
/// stop one grapheme short of the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FindKind {
    Find,     // f
    Till,     // t
    FindBack, // F
    TillBack, // T
}

impl FindKind {
    fn from_key(c: char) -> Option<FindKind> {
        Some(match c {
            'f' => FindKind::Find,
            't' => FindKind::Till,
            'F' => FindKind::FindBack,
            'T' => FindKind::TillBack,
            _ => return None,
        })
    }

    /// `f`/`t` go forward (and feed an operator inclusively); `F`/`T` go backward.
    pub(crate) fn forward(self) -> bool {
        matches!(self, FindKind::Find | FindKind::Till)
    }

    /// `t`/`T` stop short of the target ("till"); `f`/`F` land on it.
    pub(crate) fn till(self) -> bool {
        matches!(self, FindKind::Till | FindKind::TillBack)
    }

    /// The direction-flipped kind used by `,` (and by `;` after a `,`): f↔F, t↔T.
    pub(crate) fn reversed(self) -> FindKind {
        match self {
            FindKind::Find => FindKind::FindBack,
            FindKind::FindBack => FindKind::Find,
            FindKind::Till => FindKind::TillBack,
            FindKind::TillBack => FindKind::Till,
        }
    }
}

/// The two mark-jump motions: `` `{x} `` lands on the mark's exact byte position
/// (charwise exclusive), `'{x}` on the first non-blank of its line (linewise).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkJumpKind {
    Exact, // `
    Line,  // '
}

/// A text-object kind (`iw`, `a(`, `i"`, `ap`, …). The object *alphabet* lives
/// in [`ObjectKind::from_key`]; the range search lives in
/// [`Editor::text_object_range`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjectKind {
    Word(bool),       // w (false) / W (true)
    Pair(char, char), // (), {}, [], <>  (incl. b/B aliases)
    Quote(char),      // " ' `
    Sentence,         // s
    Paragraph,        // p
}

impl ObjectKind {
    fn from_key(c: char) -> Option<ObjectKind> {
        Some(match c {
            'w' => ObjectKind::Word(false),
            'W' => ObjectKind::Word(true),
            '(' | ')' | 'b' => ObjectKind::Pair('(', ')'),
            '{' | '}' | 'B' => ObjectKind::Pair('{', '}'),
            '[' | ']' => ObjectKind::Pair('[', ']'),
            '<' | '>' => ObjectKind::Pair('<', '>'),
            '"' | '\'' | '`' => ObjectKind::Quote(c),
            's' => ObjectKind::Sentence,
            'p' => ObjectKind::Paragraph,
            _ => return None,
        })
    }
}

/// A terminal single-key normal/visual command (everything that is neither a
/// motion, an operator, a text object, nor `r{char}`). Classified in
/// [`parse_command`], applied in [`Editor::execute_normal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NormalCmd {
    InsertBefore,                                    // i
    InsertLineStart,                                 // I
    InsertAfter,                                     // a
    InsertLineEnd,                                   // A
    OpenBelow,                                       // o
    OpenAbove,                                       // O
    DeleteUnder,                                     // x
    DeleteBefore,                                    // X
    DeleteToEol,                                     // D
    ChangeToEol,                                     // C
    SubstituteChar,                                  // s
    EnterReplace,                                    // R
    PasteAfter,                                      // p
    PasteBefore,                                     // P
    Undo,                                            // u
    Redo,                                            // <C-r>
    Join,                                            // J
    ToggleCase,                                      // ~
    EnterVisual,                                     // v
    EnterVisualLine,                                 // V
    EnterCommand,                                    // :
    EnterSearch(SearchDir),                          // / ?
    SearchNext,                                      // n
    SearchPrev,                                      // N
    SearchWord { dir: SearchDir, whole_word: bool }, // * # (g* g# drop boundaries)
    ScrollHalf(bool),                                // <C-d> (true) / <C-u> (false)
    ScrollPage(bool),                                // <C-f> (true) / <C-b> (false)
    ScrollLine(bool),                                // <C-e> (true) / <C-y> (false)
    AltBuffer,                                       // <C-^> / <C-6>
    TabNext(Option<usize>),                          // gt  ({count}gt → tab number)
    TabPrev(Option<usize>),                          // gT  ({count}gT → count back)
    DotRepeat,                                       // .  (replay the last change)
}

/// The stage of a partially-typed command — what the *next* key means. The
/// `g`-prefix, find-char, replace, and text-object sub-states were eight
/// scattered `Editor` booleans/options; they are one enum now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Stage {
    /// Accumulating a count and/or awaiting a command, operator, or motion.
    #[default]
    Start,
    /// Saw a lone `g`; the next key may complete `gg`/`g*`/`g#`.
    GPending,
    /// Saw `f`/`t`/`F`/`T`; the next key is the target char.
    FindPending(FindKind),
    /// Saw `r`; the next key is the replacement char.
    ReplacePending,
    /// Saw `i`/`a` (operator-pending or visual); the next key is the object kind.
    TextObjectPending(char),
    /// Saw the `<C-w>` window prefix; the next key is the window command.
    WindowPending,
    /// Saw `"`; the next key names the register for the coming yank/delete/paste.
    RegisterPending,
    /// Saw `m`; the next key names the mark to set at the cursor.
    MarkSetPending,
    /// Saw `` ` `` (`Exact`) or `'` (`Line`); the next key names the mark to jump to.
    MarkJumpPending(MarkJumpKind),
}

/// The accumulated, not-yet-complete normal/visual command — one value in place
/// of the old scattered `count`/`op_count`/`operator`/`gpending`/… fields.
#[derive(Debug, Clone, Default)]
pub(crate) struct PendingCommand {
    /// Count typed after any operator (`d`**2**`w`), or the sole count (`3j`).
    pub(crate) count: Option<usize>,
    /// Count typed before an operator (`2`d`w`), stashed when the operator armed.
    pub(crate) op_count: Option<usize>,
    /// Pending operator (`d`/`c`/`y`) awaiting its motion / text object.
    pub(crate) operator: Option<char>,
    /// Register selected by a leading `"x` for the next yank/delete/paste
    /// (`"ayy`, `"_dd`, `"0p`). `None` ⇒ the unnamed register. Carried through
    /// count/operator accumulation, cleared by [`Editor::reset_pending`].
    pub(crate) register: Option<char>,
    /// What the next key continues; see [`Stage`].
    pub(crate) stage: Stage,
}

impl PendingCommand {
    /// At a clean command boundary: no count, operator, register, or argument
    /// stage pending — the next key starts a fresh command. The dot-repeat
    /// recorder uses this to bracket one command's key stream (see
    /// [`crate::editor::Editor::input`]).
    pub(crate) fn is_clean(&self) -> bool {
        self.count.is_none()
            && self.op_count.is_none()
            && self.operator.is_none()
            && self.register.is_none()
            && self.stage == Stage::Start
    }
}

/// A fully-resolved normal/visual command, ready for [`Editor::execute`].
enum ResolvedCommand {
    /// A motion — plain movement, or (when `pending.operator` is set) its range.
    Motion(Motion),
    /// A doubled operator over `count` lines (`dd`/`cc`/`yy`).
    DoubledOperator(char),
    /// An operator awaiting a search motion (`d/`, `c?`): open the search prompt.
    OperatorSearch { op: char, dir: SearchDir },
    /// A text object in operator-pending or visual mode (`diw`, `va(`).
    TextObject { ia: char, kind: ObjectKind },
    /// `r{char}`.
    Replace(char),
    /// `m{mark}` — set a mark at the cursor.
    SetMark(char),
    /// A visual-mode operator on the current selection (`d`/`y`/`c`).
    VisualOperate(char),
    /// A terminal single-key command (insert, paste, scroll, …).
    Normal(NormalCmd),
    /// A `<C-w>` window command (split, focus, close, …).
    Window(WindowCmd),
}

/// What [`parse_step`] decides for `(pending, key)`. Pure: no buffer, no
/// mutation — the matcher's command oracle folds over this same function.
enum ParseStep {
    /// Incomplete: keep accumulating with this updated pending state.
    Prefix(PendingCommand),
    /// A whole command, ready to execute.
    Complete(ResolvedCommand),
    /// `<Esc>`: reset all pending state, and leave visual mode.
    Cancel,
    /// Reset all pending state (a dead-end key / `r`-cancel); mode unchanged.
    Reset,
    /// An object/find key with no match: in visual keep the selection (and any
    /// count), else reset — mirroring the old find / text-object abort paths.
    AbortObject,
}

/// Valid `"x` register names so far: the named `a`–`z`/`A`–`Z`, the numbered
/// `0`–`9`, the small-delete `-`, the black hole `_`, the unnamed `"`, and the
/// read-only specials `%` (filename), `/` (last search), `:` (last command), `.`
/// (last insert), and the system-clipboard `+` / `*`. The remaining specials —
/// `=` and the alternate-file `#` — are rejected until their phases land, so
/// selecting one is a loud dead-end, never a silent no-op.
fn is_register_name(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '"' | '%' | '/' | ':' | '.' | '+' | '*')
}

/// The length of the leading count prefix in a recorded key stream — used by
/// dot-repeat to strip the recorded command's own count before a `[count].`
/// override prepends a new one. Mirrors [`parse_step`]'s count rule: a run of
/// ASCII digits at the very front, except a leading `0` (the column-zero motion)
/// does not start a count. `dw` → 0, `3x` → 1, `12dd` → 2.
fn leading_count_len(keys: &[Key]) -> usize {
    let mut len = 0;
    for key in keys {
        match key.as_char() {
            Some(c) if c.is_ascii_digit() && !(c == '0' && len == 0) => len += 1,
            _ => break,
        }
    }
    len
}

/// Whether `m` is a **jump** in vim's sense — a motion that records the
/// previous-context mark (`` `` `` / `''`) before it moves: `gg`/`G` and the mark
/// jumps themselves. Ordinary `h`/`j`/`k`/`l`/word/find motions are *not* jumps,
/// so they don't stash the context (search and `:line` record it on their own
/// paths). Used by [`Editor::execute`].
fn is_jump_motion(m: Motion) -> bool {
    matches!(
        m,
        Motion::GotoTop | Motion::GotoLine | Motion::MarkJumpExact(_) | Motion::MarkJumpLine(_)
    )
}

/// The registers vim refuses to *write* (yank/delete into): the last-search
/// `/`, last-insert `.`, filename `%`, last-command `:`, expression `=`, and
/// alternate-file `#`. They are readable (paste, `:registers`) but a yank/delete
/// targeting one is aborted — vim beeps and does nothing (`register.c`
/// `valid_yank_reg(.., writing=true)` → `beep_flush`). nxvim has no bell, so the
/// abort is the whole of the signal.
pub(crate) fn is_readonly_register(c: char) -> bool {
    matches!(c, '/' | '.' | '%' | ':' | '=' | '#')
}

/// Whether `reg` selects a system-clipboard register (`"+` or `"*`). These don't
/// live in the in-memory register file — they route to the injected
/// [`crate::clipboard::Clipboard`] provider — so yank/delete/paste special-case
/// them. `"*` and `"+` map to the one provider in v1 (no X11 PRIMARY split).
pub(crate) fn is_clipboard_register(reg: Option<char>) -> bool {
    matches!(reg, Some('+') | Some('*'))
}

/// Map a key to its [`Motion`], or `None` if it is not a (g-free) motion key.
/// This is the motion alphabet; [`Editor::resolve_motion`] is its counterpart
/// that computes where each lands. `f`/`t`/`F`/`T` are not here — they open a
/// [`Stage::FindPending`] first — and `gg` is handled by the g-prefix stage.
fn classify_motion(key: Key) -> Option<Motion> {
    let ch = key.as_char();
    Some(match (key.code, ch) {
        (KeyCode::Left, _) | (_, Some('h')) | (KeyCode::Backspace, _) => Motion::Left,
        (KeyCode::Right, _) | (_, Some('l')) | (_, Some(' ')) => Motion::Right,
        (_, Some('0')) | (KeyCode::Home, _) => Motion::LineStart,
        (_, Some('^')) => Motion::FirstNonBlank,
        (_, Some('$')) | (KeyCode::End, _) => Motion::LineEnd,
        (KeyCode::Down, _) | (_, Some('j')) | (_, Some('\r')) => Motion::Down,
        (KeyCode::Up, _) | (_, Some('k')) => Motion::Up,
        (_, Some('G')) => Motion::GotoLine,
        (_, Some('w')) | (_, Some('W')) => Motion::Word,
        (_, Some('b')) | (_, Some('B')) => Motion::BackWord,
        (_, Some('e')) | (_, Some('E')) => Motion::EndWord,
        (_, Some(';')) => Motion::FindRepeat { reverse: false },
        (_, Some(',')) => Motion::FindRepeat { reverse: true },
        _ => return None,
    })
}

/// THE normal/visual grammar — the single source of truth shared by the editor's
/// executor and (in a later phase) the keymap matcher's oracle. Pure: it reads
/// only `mode`, the accumulated `pending`, and `key`, and never touches the
/// buffer. Mirrors, arm for arm, the dispatch the old `handle_normal` /
/// `handle_normal_command` performed inline.
fn parse_step(mode: Mode, pending: &PendingCommand, key: Key) -> ParseStep {
    use ParseStep::*;

    // `r{char}` is checked before `<Esc>` (as in `handle_normal`): `r<Esc>` (or a
    // non-char key) cancels with no replacement and no mode change.
    if let Stage::ReplacePending = pending.stage {
        return match key.as_char() {
            Some(c) => Complete(ResolvedCommand::Replace(c)),
            None => Reset,
        };
    }

    // `<Esc>` cancels pending state and leaves visual mode — ahead of the find /
    // text-object argument stages, exactly as the old top-level branch was.
    if key.code == KeyCode::Esc {
        return Cancel;
    }

    // Argument stages: the next key is consumed as data, not re-parsed.
    match pending.stage {
        Stage::FindPending(kind) => {
            return match key.as_char() {
                Some(target) => Complete(ResolvedCommand::Motion(Motion::Find(kind, target))),
                None => AbortObject,
            };
        }
        Stage::TextObjectPending(ia) => {
            return match key.as_char().and_then(ObjectKind::from_key) {
                Some(kind) => Complete(ResolvedCommand::TextObject { ia, kind }),
                None => AbortObject,
            };
        }
        Stage::WindowPending => {
            return match window_command(key) {
                Some(cmd) => Complete(ResolvedCommand::Window(cmd)),
                None => Reset,
            };
        }
        Stage::RegisterPending => {
            // The next key names the register. An unsupported name is a loud
            // dead-end (`Reset`), exactly like a missed find/text-object arg.
            return match key.as_char() {
                Some(name) if is_register_name(name) => {
                    let mut next = pending.clone();
                    next.register = Some(name);
                    next.stage = Stage::Start;
                    Prefix(next)
                }
                _ => Reset,
            };
        }
        Stage::MarkSetPending => {
            // The next key names the mark. Any printable name resolves to `SetMark`,
            // which `execute` validates: a settable `a`–`z`/`A`–`Z` is set, a
            // read-only special (or other junk) errors loudly (vim's *E191*) rather
            // than the old silent dead-end. A non-char key is still a `Reset`.
            return match key.as_char() {
                Some(name) => Complete(ResolvedCommand::SetMark(name)),
                None => Reset,
            };
        }
        Stage::MarkJumpPending(kind) => {
            // The next key names the mark to jump to: a settable name or a read-only
            // automatic special (`` ` ``/`'`/`.`/`^`/`[`/`]`/`<`/`>`). An unsupported
            // name (a digit, untracked punctuation) is a loud dead-end (`Reset`).
            return match key.as_char() {
                Some(name) if marks::is_jumpable_mark(name) => {
                    Complete(ResolvedCommand::Motion(match kind {
                        MarkJumpKind::Exact => Motion::MarkJumpExact(name),
                        MarkJumpKind::Line => Motion::MarkJumpLine(name),
                    }))
                }
                _ => Reset,
            };
        }
        Stage::Start | Stage::GPending => {}
        Stage::ReplacePending => unreachable!("handled above"),
    }

    // Count accumulation (Start and GPending only). `0` is a motion unless a
    // count is already building.
    if let Some(c) = key.as_char() {
        if c.is_ascii_digit() && !(c == '0' && pending.count.is_none()) {
            let d = c as usize - '0' as usize;
            let mut next = pending.clone();
            next.count = Some(pending.count.unwrap_or(0) * 10 + d);
            return Prefix(next);
        }
    }

    let gpending = pending.stage == Stage::GPending;

    // `"` selects the register for the next yank/delete/paste. It precedes the
    // operator (vim: `"add`, never `d"a`), so only at a command boundary with no
    // operator pending — and not mid-`g`.
    if !gpending && pending.operator.is_none() && key.as_char() == Some('"') {
        let mut next = pending.clone();
        next.stage = Stage::RegisterPending;
        return Prefix(next);
    }

    // `m{mark}` sets a mark at the cursor; the name follows. Like `"`, `m` is not
    // a motion and never follows an operator, so it arms only at a command
    // boundary with no operator pending (and not mid-`g`).
    if !gpending && pending.operator.is_none() && key.as_char() == Some('m') {
        let mut next = pending.clone();
        next.stage = Stage::MarkSetPending;
        return Prefix(next);
    }

    // `g` prefix: a lone `g` arms it; a second `g` is `gg`; `gt`/`gT` cycle tabs.
    if gpending {
        match key.as_char() {
            Some('g') => return Complete(ResolvedCommand::Motion(Motion::GotoTop)),
            Some('t') => {
                return Complete(ResolvedCommand::Normal(NormalCmd::TabNext(pending.count)))
            }
            Some('T') => {
                return Complete(ResolvedCommand::Normal(NormalCmd::TabPrev(pending.count)))
            }
            _ => {}
        }
    } else if key.as_char() == Some('g') {
        let mut next = pending.clone();
        next.stage = Stage::GPending;
        return Prefix(next);
    }

    // `i`/`a` introduce a text object when an operator is pending or in visual.
    if pending.operator.is_some() || mode.is_visual() {
        if let Some(c @ ('i' | 'a')) = key.as_char() {
            let mut next = pending.clone();
            next.stage = Stage::TextObjectPending(c);
            return Prefix(next);
        }
    }

    // `f`/`t`/`F`/`T` begin a find-char motion (the target follows).
    if let Some(kind) = key.as_char().and_then(FindKind::from_key) {
        let mut next = pending.clone();
        next.stage = Stage::FindPending(kind);
        return Prefix(next);
    }

    // `` `{mark} `` / `'{mark}` begin a mark jump (the name follows). Like the
    // find motions they arm whether or not an operator is pending, so `` `a `` is
    // a plain jump and `` d`a `` / `d'a` take the mark as the operator's range.
    if !gpending {
        let jump = match key.as_char() {
            Some('`') => Some(MarkJumpKind::Exact),
            Some('\'') => Some(MarkJumpKind::Line),
            _ => None,
        };
        if let Some(kind) = jump {
            let mut next = pending.clone();
            next.stage = Stage::MarkJumpPending(kind);
            return Prefix(next);
        }
    }

    // Direct motions — but while g-pending only `gg` (handled above) is a motion,
    // matching `resolve_motion`'s old `if self.gpending { … }` short-circuit.
    if !gpending {
        if let Some(m) = classify_motion(key) {
            return Complete(ResolvedCommand::Motion(m));
        }
    }

    parse_command(mode, pending, key, gpending)
}

/// The terminal / operator-continuation half of [`parse_step`]: everything that
/// is not a count, g-prefix, text-object intro, find intro, or direct motion.
/// `gpending` only affects whether `*`/`#` keep word boundaries.
fn parse_command(mode: Mode, pending: &PendingCommand, key: Key, gpending: bool) -> ParseStep {
    use NormalCmd as N;
    use ParseStep::*;
    use ResolvedCommand as RC;

    // With an operator pending only a doubled operator, a search-motion hand-off,
    // or a cancel reaches here (motions were resolved just above).
    if let Some(op) = pending.operator {
        return match key.as_char() {
            Some(c) if c == op => Complete(RC::DoubledOperator(op)),
            Some('/') => Complete(RC::OperatorSearch {
                op,
                dir: SearchDir::Forward,
            }),
            Some('?') => Complete(RC::OperatorSearch {
                op,
                dir: SearchDir::Backward,
            }),
            _ => Reset,
        };
    }

    // Ctrl-keyed scrolling, redo, the alternate-buffer toggle, and the `<C-w>`
    // window-command prefix.
    if key.ctrl {
        // `<C-w>` opens the window-command stage (normal mode only — in visual it
        // is left unbound, as in vim). The second key resolves the command.
        if key.code == KeyCode::Char('w') && !mode.is_visual() {
            let mut next = pending.clone();
            next.stage = Stage::WindowPending;
            return Prefix(next);
        }
        return match key.code {
            KeyCode::Char('d') => Complete(RC::Normal(N::ScrollHalf(true))),
            KeyCode::Char('u') => Complete(RC::Normal(N::ScrollHalf(false))),
            KeyCode::Char('f') => Complete(RC::Normal(N::ScrollPage(true))),
            KeyCode::Char('b') => Complete(RC::Normal(N::ScrollPage(false))),
            KeyCode::Char('e') => Complete(RC::Normal(N::ScrollLine(true))),
            KeyCode::Char('y') => Complete(RC::Normal(N::ScrollLine(false))),
            KeyCode::Char('r') => Complete(RC::Normal(N::Redo)),
            KeyCode::Char('^') | KeyCode::Char('6') => Complete(RC::Normal(N::AltBuffer)),
            _ => Reset,
        };
    }

    // PageDown / PageUp default to a half-page scroll, like <C-d> / <C-u>.
    match key.code {
        KeyCode::PageDown => return Complete(RC::Normal(N::ScrollHalf(true))),
        KeyCode::PageUp => return Complete(RC::Normal(N::ScrollHalf(false))),
        _ => {}
    }

    let c = match key.as_char() {
        Some(c) => c,
        None => return Reset,
    };

    // Visual-mode operators act on the selection immediately.
    if mode.is_visual() {
        match c {
            'd' | 'x' => return Complete(RC::VisualOperate('d')),
            'y' => return Complete(RC::VisualOperate('y')),
            'c' | 's' => return Complete(RC::VisualOperate('c')),
            '=' => return Complete(RC::VisualOperate('=')),
            'v' => return Complete(RC::Normal(N::EnterVisual)),
            'V' => return Complete(RC::Normal(N::EnterVisualLine)),
            ':' => return Complete(RC::Normal(N::EnterCommand)),
            _ => {}
        }
    }

    match c {
        'i' => Complete(RC::Normal(N::InsertBefore)),
        'I' => Complete(RC::Normal(N::InsertLineStart)),
        'a' => Complete(RC::Normal(N::InsertAfter)),
        'A' => Complete(RC::Normal(N::InsertLineEnd)),
        'o' => Complete(RC::Normal(N::OpenBelow)),
        'O' => Complete(RC::Normal(N::OpenAbove)),
        'x' => Complete(RC::Normal(N::DeleteUnder)),
        'X' => Complete(RC::Normal(N::DeleteBefore)),
        'D' => Complete(RC::Normal(N::DeleteToEol)),
        'C' => Complete(RC::Normal(N::ChangeToEol)),
        's' => Complete(RC::Normal(N::SubstituteChar)),
        'R' => Complete(RC::Normal(N::EnterReplace)),
        'd' | 'c' | 'y' | '=' => {
            // Begin an operator (prefix): move count → op_count, drop g-pending.
            // `=` is the reindent operator (`==`, `=motion`, `gg=G`).
            let mut next = pending.clone();
            next.operator = Some(c);
            next.op_count = pending.count;
            next.count = None;
            next.stage = Stage::Start;
            Prefix(next)
        }
        'r' => {
            let mut next = pending.clone();
            next.stage = Stage::ReplacePending;
            Prefix(next)
        }
        'p' => Complete(RC::Normal(N::PasteAfter)),
        'P' => Complete(RC::Normal(N::PasteBefore)),
        '.' => Complete(RC::Normal(N::DotRepeat)),
        'u' => Complete(RC::Normal(N::Undo)),
        'J' => Complete(RC::Normal(N::Join)),
        '~' => Complete(RC::Normal(N::ToggleCase)),
        'v' => Complete(RC::Normal(N::EnterVisual)),
        'V' => Complete(RC::Normal(N::EnterVisualLine)),
        ':' => Complete(RC::Normal(N::EnterCommand)),
        '/' => Complete(RC::Normal(N::EnterSearch(SearchDir::Forward))),
        '?' => Complete(RC::Normal(N::EnterSearch(SearchDir::Backward))),
        'n' => Complete(RC::Normal(N::SearchNext)),
        'N' => Complete(RC::Normal(N::SearchPrev)),
        '*' => Complete(RC::Normal(N::SearchWord {
            dir: SearchDir::Forward,
            whole_word: !gpending,
        })),
        '#' => Complete(RC::Normal(N::SearchWord {
            dir: SearchDir::Backward,
            whole_word: !gpending,
        })),
        _ => Reset,
    }
}

/// How a key run relates to the normal/visual command grammar, as seen by the
/// keymap matcher's disambiguation oracle ([`command_status`]). Because it is a
/// fold over the very same [`parse_step`] the executor runs, "is this a complete
/// built-in?" can never drift from what actually executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandStatus {
    /// The run is a whole number of finished commands — it ends exactly on a
    /// command boundary (e.g. `gg`, `dd`, `fx`).
    Complete,
    /// The run ends mid-command; more keys could still complete it (e.g. a lone
    /// `g`, or `f` awaiting its target char).
    Prefix,
    /// No command runs this way — a dead-end key, a cancel, or an aborted
    /// find/text-object argument.
    Invalid,
}

/// Classify a key run against the normal/visual command grammar for `mode` by
/// folding [`parse_step`] from a clean command boundary. This is the keymap
/// matcher's **read-only** oracle: it consults the exact grammar the executor
/// runs (never a mirror), so a built-in can never silently lag behind a colliding
/// user mapping for want of being recognized.
///
/// Fold rule: start from a default [`PendingCommand`]; for each key call
/// `parse_step`. On `Prefix(p)` carry `p` forward; on `Complete(_)` **reset to a
/// fresh default** and keep folding the remainder (so a run of *several* finished
/// commands still ends clean); on any cancel/reset/abort short-circuit to
/// `Invalid`. The run is [`Complete`](CommandStatus::Complete) iff it ends on a
/// boundary (nothing carried) and [`Prefix`](CommandStatus::Prefix) iff it ends
/// mid-command.
pub fn command_status(mode: Mode, keys: &[Key]) -> CommandStatus {
    let mut pending = PendingCommand::default();
    let mut at_boundary = true;
    for &key in keys {
        match parse_step(mode, &pending, key) {
            ParseStep::Prefix(p) => {
                pending = p;
                at_boundary = false;
            }
            ParseStep::Complete(_) => {
                pending = PendingCommand::default();
                at_boundary = true;
            }
            ParseStep::Cancel | ParseStep::Reset | ParseStep::AbortObject => {
                return CommandStatus::Invalid;
            }
        }
    }
    if at_boundary {
        CommandStatus::Complete
    } else {
        CommandStatus::Prefix
    }
}

impl Editor {
    /// Drive one key through the normal/visual grammar. A thin loop: the pure
    /// [`parse_step`] decides; [`Editor::execute`] (and the cancel arms here)
    /// apply. All the old inline pending-state bookkeeping now lives in
    /// `parse_step`, so this is the *only* place a normal-mode key enters and the
    /// grammar has exactly one home.
    pub(crate) fn handle_normal(&mut self, key: Key) {
        self.message.clear();
        match parse_step(self.mode, &self.pending, key) {
            ParseStep::Prefix(p) => self.pending = p,
            ParseStep::Complete(cmd) => self.execute(cmd),
            ParseStep::Cancel => {
                self.reset_pending();
                if self.mode.is_visual() {
                    // Leaving Visual mode stamps the `` `< `` / `` `> `` selection
                    // marks (what `gv` and the angle-mark jumps read) before we drop
                    // back to Normal.
                    self.record_visual_marks();
                    self.mode = Mode::Normal;
                    self.clamp_cursor();
                }
            }
            ParseStep::Reset => self.reset_pending(),
            ParseStep::AbortObject => {
                // A find/text-object miss: in visual the selection (and count)
                // survive — only the half-typed object is dropped; otherwise the
                // whole pending command, operator included, is cancelled.
                if self.mode.is_visual() {
                    self.pending.stage = Stage::Start;
                } else {
                    self.reset_pending();
                }
            }
        }
    }

    /// Apply a fully-resolved command to the buffer, delegating to the existing
    /// effect helpers. The grammar is gone from here — `execute` only dispatches
    /// on the typed [`ResolvedCommand`] `parse_step` produced, so the parse and
    /// the effect cannot drift.
    fn execute(&mut self, cmd: ResolvedCommand) {
        match cmd {
            ResolvedCommand::Motion(m) => {
                // `f`/`t`/`F`/`T` and `;`/`,` set the find memory even on a miss,
                // matching the old `pending_find` block.
                if let Motion::Find(kind, target) = m {
                    self.last_find = Some((kind, target));
                }
                // A global mark `A`–`Z` may point into another buffer. That jump
                // can't be a within-buffer motion offset (and operating across
                // files is meaningless), so it's handled here: switch to the mark's
                // buffer, then land. Marks resolving into the current buffer (every
                // lowercase mark, and a global whose buffer is current) fall through
                // to the ordinary motion path, so `` d`a `` still operates. An unset
                // or closed-buffer mark resolves to `None` here and falls through to
                // the loud *E20* miss below.
                if let Motion::MarkJumpExact(name) | Motion::MarkJumpLine(name) = m {
                    if let Some(loc) = self.mark_location(name) {
                        if loc.buf != self.cur_buffer() {
                            // A jump still records the pre-jump context in the source
                            // buffer (so `` `` `` returns), then crosses over.
                            self.record_jump_context();
                            self.jump_to_mark_buffer(loc, matches!(m, Motion::MarkJumpLine(_)));
                            self.reset_pending();
                            return;
                        }
                    }
                }
                match self.resolve_motion(m) {
                    Some(mr) => {
                        // A jump-class motion (not an operator's range) stashes the
                        // pre-jump cursor in the previous-context mark (`` `` ``/`''`)
                        // *after* the target is resolved — so `` `` `` itself reads the
                        // old context before overwriting it — and before the move.
                        if is_jump_motion(m) && self.pending.operator.is_none() {
                            self.record_jump_context();
                        }
                        self.apply_resolved_motion(mr);
                    }
                    // A find/`;`/`,` that doesn't match (or `;`/`,` with no prior
                    // find): an *execution* miss, not a grammar one. Cancel as the
                    // old failed-motion paths did — visual `f`-miss keeps the
                    // count, every other miss resets.
                    None => match m {
                        Motion::Find(..) if self.mode.is_visual() => {
                            self.pending.stage = Stage::Start;
                        }
                        // A jump to a mark that was never set: report it loudly
                        // (vim's *E20: Mark not set*) instead of silently leaving
                        // the cursor — and abort any pending operator with it.
                        Motion::MarkJumpExact(_) | Motion::MarkJumpLine(_) => {
                            self.echo("E20: Mark not set");
                            self.reset_pending();
                        }
                        _ => self.reset_pending(),
                    },
                }
            }
            ResolvedCommand::DoubledOperator(op) => self.begin_operator(op),
            ResolvedCommand::OperatorSearch { op, dir } => {
                let count = self.effective_count();
                self.search_operator = Some(op);
                self.enter_search(dir, count);
            }
            ResolvedCommand::TextObject { ia, kind } => {
                let count = self.effective_count();
                if let Some((lo, hi, linewise)) = self.text_object_range(ia, kind, count) {
                    self.apply_text_object(lo, hi, linewise);
                } else if self.mode.is_visual() {
                    // No object at the cursor: keep the selection (and count).
                    self.pending.stage = Stage::Start;
                } else {
                    self.reset_pending();
                }
            }
            ResolvedCommand::Replace(c) => {
                let count = self.effective_count();
                self.replace_char(c, count);
                self.reset_pending();
            }
            ResolvedCommand::SetMark(name) => {
                // Only the named marks are settable; the automatic specials (and any
                // other name) are read-only and error loudly, never a silent no-op.
                if marks::is_settable_mark(name) {
                    self.set_mark(name);
                } else {
                    self.echo("E191: Argument must be a letter or forward/backward quote");
                }
                self.reset_pending();
            }
            ResolvedCommand::VisualOperate(op) => self.visual_operate(op),
            ResolvedCommand::Normal(cmd) => self.execute_normal(cmd),
            ResolvedCommand::Window(cmd) => self.execute_window(cmd),
        }
    }

    /// Apply a `<C-w>` window command. Pending state is already a clean boundary
    /// (the prefix consumed it), so each arm just drives the window layout.
    fn execute_window(&mut self, cmd: WindowCmd) {
        // The resize family honors a leading count (`3<C-w>+`); read it before
        // the pending state is cleared.
        let count = self.effective_count() as isize;
        self.reset_pending();
        match cmd {
            WindowCmd::Split(dir) => self.split(dir),
            WindowCmd::FocusDir(dir) => self.focus_dir(dir),
            WindowCmd::FocusCycle(forward) => self.focus_cycle(forward),
            WindowCmd::Close => self.close_window(),
            WindowCmd::Only => self.only_window(),
            WindowCmd::Quit => self.ex_quit(false),
            WindowCmd::ToNewTab => self.window_to_new_tab(),
            WindowCmd::Equalize => self.equalize_windows(),
            WindowCmd::ResizeHeight(grow) => {
                self.resize_window(SplitDir::Horizontal, if grow { count } else { -count })
            }
            WindowCmd::ResizeWidth(grow) => {
                self.resize_window(SplitDir::Vertical, if grow { count } else { -count })
            }
            WindowCmd::MaxHeight => self.maximize_window(SplitDir::Horizontal),
            WindowCmd::MaxWidth => self.maximize_window(SplitDir::Vertical),
        }
    }

    /// Apply a terminal single-key command. Each arm is the old `handle_normal_
    /// command` body verbatim; the dispatch is now on the typed [`NormalCmd`]
    /// rather than a re-matched raw key.
    fn execute_normal(&mut self, cmd: NormalCmd) {
        let count = self.effective_count();
        // The delete/change family writes a register; a read-only target aborts
        // the whole command (including `C`/`s`'s insert) — vim beeps, no change.
        if matches!(
            cmd,
            NormalCmd::DeleteUnder
                | NormalCmd::DeleteBefore
                | NormalCmd::DeleteToEol
                | NormalCmd::ChangeToEol
                | NormalCmd::SubstituteChar
        ) && self.pending.register.is_some_and(is_readonly_register)
        {
            self.reset_pending();
            return;
        }
        match cmd {
            NormalCmd::InsertBefore => self.enter_insert_at(self.cursor.col),
            NormalCmd::InsertLineStart => {
                let col = self.first_non_blank(self.cursor.line);
                self.enter_insert_at(col);
            }
            NormalCmd::InsertAfter => {
                let col = (self.cursor.col + 1).min(self.line_len());
                self.enter_insert_at(col);
            }
            NormalCmd::InsertLineEnd => self.enter_insert_at(self.line_len()),
            NormalCmd::OpenBelow => self.open_line(true),
            NormalCmd::OpenAbove => self.open_line(false),
            NormalCmd::DeleteUnder => self.delete_under_cursor(count),
            NormalCmd::DeleteBefore => self.delete_before_cursor(count),
            NormalCmd::DeleteToEol => self.delete_to_eol(),
            NormalCmd::ChangeToEol => {
                self.delete_to_eol();
                self.mode = Mode::Insert;
                self.snapshot_taken = true;
            }
            NormalCmd::SubstituteChar => {
                self.delete_under_cursor(count);
                self.mode = Mode::Insert;
                self.snapshot_taken = true;
            }
            NormalCmd::PasteAfter => self.paste(true, count),
            NormalCmd::PasteBefore => self.paste(false, count),
            // Undo/redo edit the buffer (bumping `changedtick`) but are not the
            // dot-repeat target: mark the command non-repeatable so the recorder
            // discards it and `.` keeps replaying the change that preceded it.
            NormalCmd::Undo => {
                self.change_not_repeatable = true;
                self.undo();
            }
            NormalCmd::Redo => {
                self.change_not_repeatable = true;
                self.redo();
            }
            NormalCmd::Join => self.join_lines(count.max(2)),
            NormalCmd::ToggleCase => self.toggle_case(count),
            // `R` enters Replace mode: snapshot for undo, then overtype until
            // `<Esc>` (the insert handler honors `Mode::Replace`).
            NormalCmd::EnterReplace => {
                self.push_undo();
                self.snapshot_taken = true;
                self.mode = Mode::Replace;
            }
            // From normal mode entering visual anchors the selection; from visual
            // mode `v`/`V` only switch the selection's shape, leaving the anchor.
            NormalCmd::EnterVisual => {
                if !self.mode.is_visual() {
                    self.visual_anchor = self.cursor;
                }
                self.mode = Mode::Visual;
            }
            NormalCmd::EnterVisualLine => {
                if !self.mode.is_visual() {
                    self.visual_anchor = self.cursor;
                }
                self.mode = Mode::VisualLine;
            }
            NormalCmd::EnterCommand => self.enter_command(),
            NormalCmd::EnterSearch(dir) => self.enter_search(dir, count),
            NormalCmd::SearchNext => self.search_repeat(true, count),
            NormalCmd::SearchPrev => self.search_repeat(false, count),
            NormalCmd::SearchWord { dir, whole_word } => {
                self.search_word_under_cursor(dir, whole_word, count)
            }
            NormalCmd::ScrollHalf(down) => self.scroll_half(down),
            NormalCmd::ScrollPage(down) => self.scroll_page(down),
            NormalCmd::ScrollLine(down) => self.scroll_line(down),
            NormalCmd::AltBuffer => self.goto_alternate(),
            NormalCmd::TabNext(n) => self.goto_tab_next(n),
            NormalCmd::TabPrev(n) => self.goto_tab_prev(n),
            NormalCmd::DotRepeat => self.repeat_change(),
        }
        self.reset_pending();
    }

    /// Replay the last buffer-changing command (`.`). Re-feeds the recorded raw
    /// key stream through [`Editor::input`] under the `replaying_change` guard, so
    /// the entire grammar (counts, operators, registers, text objects, inserted
    /// text) re-parses and re-executes exactly as typed. No-op when nothing has
    /// been recorded yet (vim beeps; nxvim has no bell).
    fn repeat_change(&mut self) {
        if self.last_change.is_empty() {
            return;
        }
        // `.` itself must never become the new last change — mark this command
        // non-repeatable so a following `.` still replays the original.
        self.change_not_repeatable = true;
        // A count typed on `.` (`3.`) *replaces* the recorded command's own count
        // (vim: `dw` then `3.` runs `3dw`): strip the recorded leading count and
        // prepend the new one. With no new count the recorded keys stand verbatim,
        // so `3x` then `.` still deletes three.
        let keys = match self.pending.count {
            Some(n) => {
                let tail = &self.last_change[leading_count_len(&self.last_change)..];
                let mut keys: Vec<Key> = n.to_string().chars().map(Key::char).collect();
                keys.extend_from_slice(tail);
                keys
            }
            None => self.last_change.clone(),
        };
        // Clear `.`'s own pending count before replaying, so a prepended count
        // digit starts a fresh count rather than accumulating onto it (`3.` would
        // otherwise feed `3` onto the existing `3` and run `33…`).
        self.reset_pending();
        self.replaying_change = true;
        for key in keys {
            self.input(key);
        }
        self.replaying_change = false;
    }

    /// Apply a doubled operator (`dd`/`cc`/`yy`): linewise over `count` lines.
    /// Only reached once the operator is already pending (the first `d`/`c`/`y`
    /// armed it in [`parse_command`]), so this is purely the doubled path.
    fn begin_operator(&mut self, op: char) {
        let count = self.effective_count();
        let last = self.cursor.line + count - 1;
        let target = self.buffer().line_start(last.min(self.last_line()));
        // axis is unused for the operator path, but the field is required.
        let m = MotionResult::linewise(target, MoveAxis::LineAnchor);
        self.pending.operator = None;
        self.apply_operator(op, m);
        self.reset_pending();
    }

    /// Apply a resolved motion: as an operator's range if one is pending,
    /// otherwise as plain cursor movement (recording the pre-move position so
    /// an off-screen jump animates its scroll, like the explicit scrolls).
    fn apply_resolved_motion(&mut self, m: MotionResult) {
        if let Some(op) = self.pending.operator.take() {
            self.apply_operator(op, m);
        } else {
            self.scroll_from = Some((self.top, self.cursor.line));
            self.apply_movement(m);
        }
        // A completed motion clears the whole pending command. (The old movement
        // path only cleared `count`, but operator/g-prefix/find were already
        // cleared before it ran, so a full reset is equivalent — and now correct,
        // since `parse_step` leaves the stage set until the command finishes.)
        self.reset_pending();
    }
}
