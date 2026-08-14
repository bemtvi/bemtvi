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

/// The internal operator char for `gc`/`gcc` (toggle line comment). Operators are
/// represented by a single `char` in [`PendingCommand::operator`]; the comment
/// operator's mnemonic is the two-key `gc`, so it needs a sentinel that can never
/// collide with a real keystroke. A private-use code point is never produced by
/// [`Key::as_char`] from actual input, so it is safe to compare against. The `gcc`
/// doubling (the second `c`) and visual `gc` are handled explicitly in
/// [`parse_step`]; everything else (motions, text objects, counts) flows through
/// the generic operator machinery keyed on this char.
pub(crate) const COMMENT_OP: char = '\u{E000}';

/// The private-use char standing in for the fold-create operator (`zf{motion}`),
/// the fold sibling of [`COMMENT_OP`]. `zf` arms it like `gc` arms the comment
/// operator; the following motion's (always linewise) range names the lines to
/// fold. Routed through the generic operator machinery in
/// [`Editor::apply_operator_to_range`].
pub(crate) const FOLD_OP: char = '\u{E001}';

/// A fold command resolved from the `z` prefix (the fold half of the `z` family,
/// beside the viewport [`ViewPlace`] commands). Each maps to an `Editor::fold_*`
/// method; [`CreateLines`](FoldCmd::CreateLines) carries its line count via the
/// command's resolved count (`zF` / `{count}zF`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FoldCmd {
    Open,                 // zo
    Close,                // zc
    Toggle,               // za
    OpenRecursive,        // zO
    CloseRecursive,       // zC
    OpenAll,              // zR
    CloseAll,             // zM
    Delete,               // zd
    DeleteAll,            // zE
    CreateLines,          // zF — fold `count` lines from the cursor
    Enable(Option<bool>), // zN(Some true) / zn(Some false) / zi(None: toggle)
    Next,                 // zj — move to the next fold's start
    Prev,                 // zk — move to the previous fold's end
}

/// A `<C-w>` window command: the second key after the `<C-w>` prefix, resolved by
/// [`parse_step`] and applied in [`Editor::execute`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowCmd {
    /// `<C-w>s` (horizontal) / `<C-w>v` (vertical) — split the focused window.
    Split(SplitDir),
    /// `<C-w>h/j/k/l` — move focus to the window in that direction.
    FocusDir(WinDir),
    /// `<C-w>H/J/K/L` — swap the focused window's buffer (and its view) with the
    /// nearest window in that direction, then follow it there. A no-op with no
    /// neighbor on that side. See [`Editor::swap_window_dir`].
    SwapDir(WinDir),
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
    /// `<C-w>d` / `<C-w><C-d>` — show the diagnostics under the cursor in a float
    /// (neovim's built-in default). The float is a server surface, so core only
    /// records the request ([`Editor::take_diagnostic_float`]); the server opens it
    /// in `run_pending`. Independent of the focused layer — it acts on the current
    /// buffer's diagnostics.
    ShowDiagnostics,
}

/// A `<C-w><C-w>` *layer* command — the doubled-prefix grammar that crosses
/// between the main window area and the permanent docks (bemtvi's repurposing of
/// vim's `<C-w><C-w>`, which there just cycles focus). The following key is read
/// like an ordinary window command but applied to the *other* layer; see
/// [`Editor::execute_window_layer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayerWindowCmd {
    /// `<C-w><C-w>{h,j,k,l}` — cross by edge: from the main area focus the dock on
    /// that side; from a dock return to the main area.
    CrossDir(WinDir),
    /// `<C-w><C-w>{H,J,K,L}` — *move* the focused buffer to the layer on that edge:
    /// from the main area to the dock on that side (a no-op if it is closed), from a
    /// dock back to the main area. The source window falls back to a sibling buffer
    /// in its own layer. See [`Editor::move_buffer_to_layer`].
    MoveDir(WinDir),
    /// `<C-w><C-w>{v,s,c,…}` — cross to the other layer (the last-focused dock from
    /// the main area, or back to main from a dock) and run this window command
    /// there.
    CrossThenWindow(WindowCmd),
}

/// Resolve the key *after* `<C-w>` into a [`WindowCmd`], or `None` for a key that
/// is not a window command. (A second `<C-w>` is intercepted earlier, in the
/// [`Stage::WindowPending`] arm, as the dock layer-switch prefix — it is *not* a
/// `WindowCmd`.)
fn window_command(key: Key) -> Option<WindowCmd> {
    if key.ctrl {
        return match key.code {
            KeyCode::Char('w') => Some(WindowCmd::FocusCycle(true)),
            // `<C-w><C-d>` — the control-key twin of `<C-w>d` (neovim maps both).
            KeyCode::Char('d') => Some(WindowCmd::ShowDiagnostics),
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
        'H' => WindowCmd::SwapDir(WinDir::Left),
        'J' => WindowCmd::SwapDir(WinDir::Down),
        'K' => WindowCmd::SwapDir(WinDir::Up),
        'L' => WindowCmd::SwapDir(WinDir::Right),
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
        'd' => WindowCmd::ShowDiagnostics,
        _ => return None,
    })
}

/// Resolve the key *after* `<C-w><C-w>` into a [`LayerWindowCmd`], or `None` for a
/// key that names neither a direction nor a window command. `h/j/k/l` are the
/// layer cross (by edge); every other window key crosses and then runs there.
fn layer_window_command(key: Key) -> Option<LayerWindowCmd> {
    if !key.ctrl {
        match key.as_char() {
            Some('h') => return Some(LayerWindowCmd::CrossDir(WinDir::Left)),
            Some('j') => return Some(LayerWindowCmd::CrossDir(WinDir::Down)),
            Some('k') => return Some(LayerWindowCmd::CrossDir(WinDir::Up)),
            Some('l') => return Some(LayerWindowCmd::CrossDir(WinDir::Right)),
            // Capitals *move* the buffer to that layer (the dock on that edge, or
            // back to main from a dock) — distinct from the lowercase focus cross.
            Some('H') => return Some(LayerWindowCmd::MoveDir(WinDir::Left)),
            Some('J') => return Some(LayerWindowCmd::MoveDir(WinDir::Down)),
            Some('K') => return Some(LayerWindowCmd::MoveDir(WinDir::Up)),
            Some('L') => return Some(LayerWindowCmd::MoveDir(WinDir::Right)),
            _ => {}
        }
    }
    window_command(key).map(LayerWindowCmd::CrossThenWindow)
}

/// Where the cross-mode `<C-w><C-w>` dock-navigation chord stands. In Normal /
/// MultiCursor mode the command grammar ([`parse_step`]) already owns `<C-w>`
/// (both the single-`<C-w>` window prefix and the doubled layer cross). This
/// tiny state machine, checked ahead of mode dispatch in [`Editor::input`],
/// gives the *other* modes — insert, replace, visual, command, terminal — the
/// same `<C-w><C-w>{cmd}` reach into the docks, so the chord works in any mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum DockChord {
    /// No chord in progress.
    #[default]
    Idle,
    /// Held one `<C-w>` (in a non-grammar mode); the next key may complete it.
    FirstCw,
    /// Saw `<C-w><C-w>`; the next key is the layer command (cross / cross-then-run).
    SecondCw,
}

/// Whether `key` is the `<C-w>` that the dock chord is built from.
fn is_dock_chord_key(key: Key) -> bool {
    key.ctrl && key.code == KeyCode::Char('w')
}

/// Where a `z`-family command parks the cursor's line within the window's text
/// area. Resolved by [`view_command`] and applied in [`Editor::view_reposition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewPlace {
    /// `zt` / `z<CR>` — cursor line at the top row.
    Top,
    /// `zz` / `z.` — cursor line centered.
    Center,
    /// `zb` / `z-` — cursor line at the bottom row.
    Bottom,
}

/// Resolve the key *after* `z` into a viewport placement and whether the cursor
/// also moves to the line's first non-blank (`z<CR>`/`z.`/`z-`, vs. the
/// keep-column `zt`/`zz`/`zb`). `None` for a key that is not a `z` command.
fn view_command(key: Key) -> Option<(ViewPlace, bool)> {
    // `z<CR>` tops the line and jumps to its first non-blank. Match the `Enter`
    // key code as well as a raw `\r` so the notation `<CR>` and a terminal's bare
    // carriage return both work.
    if key.code == KeyCode::Enter {
        return Some((ViewPlace::Top, true));
    }
    Some(match key.as_char()? {
        't' => (ViewPlace::Top, false),
        'z' => (ViewPlace::Center, false),
        'b' => (ViewPlace::Bottom, false),
        '\r' => (ViewPlace::Top, true),
        '.' => (ViewPlace::Center, true),
        '-' => (ViewPlace::Bottom, true),
        _ => return None,
    })
}

/// Resolve the key after `z` into a **fold** command, or `None` when the key is
/// not a fold command (so the caller falls through to [`view_command`]). `zf`
/// behaves like `gc`: in visual mode it folds the selection immediately, in
/// normal mode it arms the [`FOLD_OP`] operator for the following motion. The
/// open/close/delete keys complete immediately as a [`NormalCmd::Fold`]; `zF`
/// folds `count` lines.
fn fold_command(key: Key, pending: &PendingCommand, mode: Mode) -> Option<ParseStep> {
    use FoldCmd::*;
    use ParseStep::{Complete, Prefix};
    let fold = |fc: FoldCmd| Complete(ResolvedCommand::Normal(NormalCmd::Fold(fc)));
    Some(match key.as_char()? {
        // `zf` — create a fold over a motion's lines. Visual mode folds the
        // selection now; normal mode arms the linewise fold operator.
        'f' if mode.is_visual() => Complete(ResolvedCommand::VisualOperate(FOLD_OP)),
        'f' => {
            let mut next = pending.clone();
            next.operator = Some(FOLD_OP);
            next.op_count = pending.count;
            next.count = None;
            next.stage = Stage::Start;
            Prefix(next)
        }
        'F' => fold(CreateLines),
        'o' => fold(Open),
        'c' => fold(Close),
        'a' => fold(Toggle),
        'O' => fold(OpenRecursive),
        'C' => fold(CloseRecursive),
        'R' => fold(OpenAll),
        'M' => fold(CloseAll),
        'd' => fold(Delete),
        'E' => fold(DeleteAll),
        'v' => fold(Open), // `zv` — view cursor: open just enough to reveal it
        'j' => fold(Next),
        'k' => fold(Prev),
        'n' => fold(Enable(Some(false))),
        'N' => fold(Enable(Some(true))),
        'i' => fold(Enable(None)),
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
/// [`Editor::resolve_motion`]. The word motions carry a `big` flag: `false` for
/// the small-word keys (`w`/`b`/`e`, which stop at punctuation) and `true` for
/// the WORD keys (`W`/`B`/`E`, which treat a run of non-blank chars as one word).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Motion {
    Left,                         // h, <Left>, <BS>
    Right,                        // l, <Right>, <Space>
    LineStart,                    // 0, <Home>
    FirstNonBlank,                // ^
    LineEnd,                      // $, <End>
    Down,                         // j, <Down>
    Up,                           // k, <Up>
    DisplayDown,                  // gj — down one *display* row (soft-wrap aware)
    DisplayUp,                    // gk — up one display row
    DisplayLineStart,             // g0 — first column of the *display* row
    DisplayFirstNonBlank,         // g^ — first non-blank of the display row
    DisplayLineEnd,               // g$ — last column of the display row
    GotoLine,                     // G  (count = target line, default last)
    GotoTop,                      // gg (count = target line, default first)
    Word(bool),                   // w (small) / W (big/WORD)
    BackWord(bool),               // b (small) / B (big/WORD)
    EndWord(bool),                // e (small) / E (big/WORD)
    Find(FindKind, char),         // f/t/F/T {char}
    FindRepeat { reverse: bool }, // ; (same) / , (reversed)
    MarkJumpExact(char, bool),    // `{mark}  (charwise exclusive); bool = set jumplist
    MarkJumpLine(char, bool),     // '{mark}  (linewise, first non-blank); bool = set jumplist
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
    pub(crate) fn from_key(c: char) -> Option<FindKind> {
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

    /// The key that triggered this find-pending state (`f`/`t`/`F`/`T`) — the
    /// inverse of [`from_key`](Self::from_key), used to render the pending keys for
    /// the `btv.on_key_pending` (which-key) signal.
    pub(crate) fn as_char(self) -> char {
        match self {
            FindKind::Find => 'f',
            FindKind::Till => 't',
            FindKind::FindBack => 'F',
            FindKind::TillBack => 'T',
        }
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
    /// A tree-sitter text object, carrying the `textobjects.scm` capture *base*
    /// name (`"function"`, `"parameter"`, `"comment"`, `"class"`). `i`/`a` picks the
    /// `.inner`/`.outer` suffix; the range comes from the syntax engine via
    /// [`Editor::ts_text_object_range`]. `f`/`a`/`c`/`t` in [`ObjectKind::from_key`].
    TsCapture(&'static str),
}

impl ObjectKind {
    pub(crate) fn from_key(c: char) -> Option<ObjectKind> {
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
            // Tree-sitter text objects (Helix-style mnemonics): function, argument,
            // comment, type/class. Resolved against the buffer's `textobjects.scm`.
            'f' => ObjectKind::TsCapture("function"),
            'a' => ObjectKind::TsCapture("parameter"),
            'c' => ObjectKind::TsCapture("comment"),
            't' => ObjectKind::TsCapture("class"),
            _ => return None,
        })
    }
}

/// A terminal single-key normal/visual command (everything that is neither a
/// motion, an operator, a text object, nor `r{char}`). Classified in
/// [`parse_command`], applied in [`Editor::execute_normal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NormalCmd {
    InsertBefore,           // i
    InsertLineStart,        // I
    InsertAfter,            // a
    InsertLineEnd,          // A
    OpenBelow,              // o
    OpenAbove,              // O
    DeleteUnder,            // x
    DeleteBefore,           // X
    DeleteToEol,            // D
    ChangeToEol,            // C
    SubstituteChar,         // s
    EnterReplace,           // R
    PasteAfter,             // p
    PasteBefore,            // P
    Undo,                   // u
    Redo,                   // <C-r>
    Join,                   // J
    ToggleCase,             // ~
    EnterVisual,            // v
    EnterVisualLine,        // V
    EnterSelect(bool),      // gh / gH (Select mode; true = linewise)
    ToggleVisualSelect,     // <C-g> (toggle Visual <-> Select, keeping the selection)
    VisualSwapEnds,         // o / O (move to other end of selection)
    ReselectVisual,         // gv (reselect the last Visual selection)
    EnterCommand,           // :
    EnterSearch(SearchDir), // / ?
    SearchNext,             // n
    SearchPrev,             // N
    SearchWord {
        dir: SearchDir,
        whole_word: bool,
    }, // * # (g* g# drop boundaries)
    ScrollHalf(bool),       // <C-d> (true) / <C-u> (false)
    ScrollPage(bool),       // <C-f> (true) / <C-b> (false)
    ScrollLine(bool),       // <C-e> (true) / <C-y> (false)
    ViewScroll {
        // z-family viewport repositioning: zt/zz/zb and z<CR>/z./z-.
        place: ViewPlace,
        first_nonblank: bool,
        count: Option<usize>, // {count}z… targets that line (1-based)
    },
    Fold(FoldCmd),          // z-family fold commands: zo/zc/za/zR/zM/zd/zE/zF/zn/…
    JumpBack,               // <C-o> (older jumplist position)
    JumpForward,            // <C-i> / <Tab> (newer position)
    AltBuffer,              // <C-^> / <C-6>
    TabNext(Option<usize>), // gt  ({count}gt → tab number)
    TabPrev(Option<usize>), // gT  ({count}gT → count back)
    ChangeOlder,            // g; (older change-list position)
    ChangeNewer,            // g, (newer change-list position)
    AddCursor,              // <A-c> (enter MULTICURSOR + place)
    PlaceCursor,            // c (drop a cursor in MULTICURSOR)
    DotRepeat,              // .  (replay the last change)
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
    /// Saw the doubled `<C-w><C-w>` prefix; the next key is the dock layer command
    /// (cross to a dock / back to the main area, then optionally a window command).
    WindowLayerPending,
    /// Saw a lone `z`; the next key completes a viewport command (`zz`/`zt`/`zb`,
    /// `z.`/`z<CR>`/`z-`).
    ZPending,
    /// Saw `"`; the next key names the register for the coming yank/delete/paste.
    RegisterPending,
    /// Saw `<F2>` at a clean boundary with nothing recording; the next key names
    /// the register to record the macro into (`<F2>a`, `<F2>A` to append).
    RecordPending,
    /// Saw `<F3>`; the next key names the register to play back (`<F3>a`), or is
    /// `<F3>` again for "the last register played" (vim's `@@`).
    PlayPending,
    /// Saw `m`; the next key names the mark to set at the cursor.
    MarkSetPending,
    /// Saw `` ` `` (`Exact`) or `'` (`Line`); the next key names the mark to jump
    /// to. The `bool` is whether the jump sets the jumplist — `true` for plain
    /// `` ` ``/`'`, `false` for the `` g` ``/`g'` spellings (vim's jump-without-
    /// touching-the-jumplist).
    MarkJumpPending(MarkJumpKind, bool),
}

/// A snapshot of the built-in command grammar's "waiting for the next key" state,
/// for the `btv.on_key_pending` (which-key / showcmd) signal — **source B** of the
/// oracle. Unlike a mapped-prefix continuation list (sources A/C), the built-in
/// leaf states (`f` find-char, `r` replace, marks, registers) have an *open*
/// continuation set — any printable char answers them — so this carries a
/// human-readable [`label`](Self::label) instead of a finite key list. Built by
/// [`Editor::command_pending`] from [`PendingCommand`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandPending {
    /// The hint shown for this state (`"Find character"`, `"Replace character"`).
    pub label: &'static str,
    /// The command keys typed so far, in vim notation (`"df"`, `"2d3f"`, `` "`" ``,
    /// `"<C-w>"`) — the showcmd-style prefix a which-key draws as the popup title.
    pub keys: String,
    /// The discrete keys that complete this stage, when it has a *finite* set — the
    /// **enumerated** built-in prefixes (`g` → `gg`/`gt`/…, `z` → `zz`/`zt`/…,
    /// `<C-w>` → the window commands). Empty for the open-set leaves (find-char,
    /// replace, marks, registers, operator-pending), which show only the
    /// [`label`](Self::label). A which-key renders these like a mapped-prefix
    /// continuation list (source A); the server merges them into a withheld mapped
    /// prefix that shares the same built-in key (e.g. `g`, withheld by the LSP
    /// `gd`/`gD`/`gr` defaults). Maintained beside the grammar that resolves them
    /// ([`window_command`] / [`view_command`] / the `g`-prefix arm).
    pub continuations: Vec<CommandContinuation>,
}

/// One enumerated continuation of a finite built-in prefix, for the
/// [`CommandPending`] hint — the built-in counterpart of a mapped-prefix
/// continuation. The `desc` is editorial (a which-key label), but every `key`
/// listed resolves to a real command in [`parse_step`]; the two are kept in
/// lockstep by living next to the grammar arm that consumes the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandContinuation {
    /// The continuation key in vim notation (`"g"`, `"<CR>"`, `"<C-w>"`).
    pub key: String,
    /// A human-readable hint for what the key does (`"Go to first line"`). Owned
    /// because the dynamic states (registers, marks) build it from live editor state
    /// (a register content preview, a mark's position), not a `'static` literal.
    pub desc: String,
    /// Whether the key only leads *deeper* (a further pending stage — `` g` `` opens
    /// a mark-jump) rather than completing a command. which-key renders a group as a
    /// `+prefix`, mirroring the server's `ContinuationKind::Group` for mapped prefixes.
    pub group: bool,
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
    /// A text object in operator-pending or visual mode (`diw`, `va(`, `dif`).
    /// Carries the raw object *key* (the char after `i`/`a`), not a resolved
    /// [`ObjectKind`]: the object is resolved at execution time by
    /// [`Editor::resolve_text_object`], which consults the user registry
    /// (`btv.textobject.map`) before the built-in alphabet.
    TextObject { ia: char, key: char },
    /// `r{char}`.
    Replace(char),
    /// `m{mark}` — set a mark at the cursor.
    SetMark(char),
    /// `<F2>{reg}` — start recording a keyboard macro into `{reg}`. The *stop*
    /// `<F2>` never reaches the pure grammar: it depends on live recording state,
    /// so [`Editor::handle_normal`] intercepts it ahead of [`parse_step`].
    MacroRecord(char),
    /// `{count}<F3>{reg}` — play a macro back. `None` is `<F3><F3>`: the last
    /// register played.
    MacroPlay(Option<char>),
    /// A visual-mode operator on the current selection (`d`/`y`/`c`).
    VisualOperate(char),
    /// A terminal single-key command (insert, paste, scroll, …).
    Normal(NormalCmd),
    /// A `<C-w>` window command (split, focus, close, …).
    Window(WindowCmd),
    /// A `<C-w><C-w>` dock layer command (cross between the main area and a dock).
    WindowLayer(LayerWindowCmd),
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
/// The register names `<F2>{reg}` will record into: the named `a`–`z` (uppercase
/// appends) and the numbered `0`–`9`. The read-only specials (`%` `/` `:` `.`)
/// and the black hole are not recordable, so `<F2>%` is a dead-end rather than a
/// recording that silently discards itself.
fn is_recordable_register(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

/// The macro-record trigger: bare `<F2>`, no modifiers (`<C-F2>` and friends stay
/// free to map). bemtvi deliberately does not use vim's `q` — see the arm in
/// [`parse_step`] that calls this.
pub(super) fn is_macro_record_key(key: Key) -> bool {
    key.code == KeyCode::Function(2) && !key.ctrl && !key.alt && !key.shift
}

/// The macro-playback trigger: bare `<F3>`, bemtvi's spelling of vim's `@`.
fn is_macro_play_key(key: Key) -> bool {
    key.code == KeyCode::Function(3) && !key.ctrl && !key.alt && !key.shift
}

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
/// paths). The `` g` ``/`g'` spellings carry `set_jump == false`, so they jump to
/// the mark *without* touching the jumplist (vim's `g`` ` / `g'`). Used by
/// [`Editor::execute`].
fn is_jump_motion(m: Motion) -> bool {
    match m {
        Motion::GotoTop | Motion::GotoLine => true,
        Motion::MarkJumpExact(_, set) | Motion::MarkJumpLine(_, set) => set,
        _ => false,
    }
}

/// The registers vim refuses to *write* (yank/delete into): the last-search
/// `/`, last-insert `.`, filename `%`, last-command `:`, expression `=`, and
/// alternate-file `#`. They are readable (paste, `:registers`) but a yank/delete
/// targeting one is aborted — vim beeps and does nothing (`register.c`
/// `valid_yank_reg(.., writing=true)` → `beep_flush`). bemtvi has no bell, so the
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
        (_, Some('w')) => Motion::Word(false),
        (_, Some('W')) => Motion::Word(true),
        (_, Some('b')) => Motion::BackWord(false),
        (_, Some('B')) => Motion::BackWord(true),
        (_, Some('e')) => Motion::EndWord(false),
        (_, Some('E')) => Motion::EndWord(true),
        (_, Some(';')) => Motion::FindRepeat { reverse: false },
        (_, Some(',')) => Motion::FindRepeat { reverse: true },
        _ => return None,
    })
}

/// The human meaning of a read-only *automatic* mark (the punctuation marks vim
/// maintains), for the mark-jump hint — so a `'` row reads "previous position", not
/// the (often unrelated-looking) line it happens to sit on. `None` for a settable
/// named mark (`a`–`z`/`A`–`Z`), whose line preview *is* the useful context. Mirrors
/// the special set in [`marks`](super::marks) (`'` is the fold target of `` ` ``).
fn special_mark_name(name: char) -> Option<&'static str> {
    Some(match name {
        '"' => "last cursor position",
        '\'' | '`' => "previous position",
        '.' => "last change",
        '^' => "last insert",
        '[' => "change/yank start",
        ']' => "change/yank end",
        '<' => "visual start",
        '>' => "visual end",
        _ => return None,
    })
}

/// A compact one-line preview of stored text (a register's contents, a mark's
/// line) for a which-key continuation `desc`: the first line, control chars
/// stripped, trimmed, truncated to a readable width with an ellipsis. A multi-line
/// value gets a trailing `⏎` so a linewise register reads as more than its first
/// line.
fn preview_text(text: &str) -> String {
    const MAX: usize = 40;
    let multiline = text.contains('\n');
    let first = text.lines().next().unwrap_or("");
    let cleaned: String = first
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.trim();
    let mut out: String = cleaned.chars().take(MAX).collect();
    if cleaned.chars().count() > MAX {
        out.push('…');
    } else if multiline {
        out.push('⏎');
    }
    out
}

/// Build a continuation list from `(key-notation, desc)` pairs that all *complete*
/// a command (`group = false`). The shared spelling for the finite built-in
/// prefixes' hints; the `g`-prefix mixes in a couple of groups by hand.
fn conts(entries: &[(&'static str, &'static str)]) -> Vec<CommandContinuation> {
    entries
        .iter()
        .map(|&(key, desc)| CommandContinuation {
            key: key.to_string(),
            desc: desc.to_string(),
            group: false,
        })
        .collect()
}

/// The enumerated continuations of a lone `z` — the [`view_command`] alphabet, in
/// vim notation. Kept beside `view_command` so a new `z`-command appears in both.
fn z_continuations() -> Vec<CommandContinuation> {
    conts(&[
        ("t", "Scroll line to top"),
        ("z", "Scroll line to center"),
        ("b", "Scroll line to bottom"),
        ("<CR>", "Top, first non-blank"),
        (".", "Center, first non-blank"),
        ("-", "Bottom, first non-blank"),
    ])
}

/// The enumerated continuations of a lone `g` — the intentional `g`-commands the
/// `g`-prefix arm of [`parse_step`] resolves. `` g` ``/`g'` only *arm* a further
/// mark-jump stage, so they are groups; the rest complete. (The accidental
/// fall-throughs a stray key takes through [`parse_command`] are not advertised.)
fn g_continuations() -> Vec<CommandContinuation> {
    let mut out = conts(&[
        ("g", "Go to first line"),
        ("j", "Down one display line"),
        ("k", "Up one display line"),
        ("t", "Next tab"),
        ("T", "Previous tab"),
        ("c", "Toggle comment"),
        (";", "Older change position"),
        (",", "Newer change position"),
        ("*", "Search word forward (partial)"),
        ("#", "Search word backward (partial)"),
    ]);
    out.push(CommandContinuation {
        key: "`".to_string(),
        desc: "Jump to mark (no jumplist)".to_string(),
        group: true,
    });
    out.push(CommandContinuation {
        key: "'".to_string(),
        desc: "Jump to mark line (no jumplist)".to_string(),
        group: true,
    });
    out
}

/// The enumerated continuations of a lone `<C-w>` — the [`window_command`] alphabet.
/// A second `<C-w>` opens the dock layer-switch prefix, so it is the one group.
fn window_continuations() -> Vec<CommandContinuation> {
    let mut out = conts(&[
        ("s", "Split horizontal"),
        ("v", "Split vertical"),
        ("w", "Focus next window"),
        ("W", "Focus previous window"),
        ("h", "Focus left"),
        ("j", "Focus down"),
        ("k", "Focus up"),
        ("l", "Focus right"),
        ("H", "Move window left"),
        ("J", "Move window down"),
        ("K", "Move window up"),
        ("L", "Move window right"),
        ("c", "Close window"),
        ("o", "Only window"),
        ("q", "Quit window"),
        ("T", "Move to new tab"),
        ("=", "Equalize sizes"),
        ("+", "Taller"),
        ("-", "Shorter"),
        (">", "Wider"),
        ("<", "Narrower"),
        ("_", "Max height"),
        ("|", "Max width"),
    ]);
    out.push(CommandContinuation {
        key: "<C-w>".to_string(),
        desc: "Dock layer".to_string(),
        group: true,
    });
    out
}

/// The enumerated continuations of `<C-w><C-w>` — the dock layer-cross directions.
/// The lowercase keys cross focus to / from a dock; the capitals move the buffer to
/// that layer. (Every *other* window key also works here — it crosses then runs the
/// [`window_command`] — but enumerating the whole window alphabet again would bury
/// the layer ops, so the card lists only the directional crosses.)
fn window_layer_continuations() -> Vec<CommandContinuation> {
    conts(&[
        ("h", "Cross to left dock"),
        ("j", "Cross to bottom dock"),
        ("k", "Cross to top dock"),
        ("l", "Cross to right dock"),
        ("H", "Move buffer to left dock"),
        ("J", "Move buffer to bottom dock"),
        ("K", "Move buffer to top dock"),
        ("L", "Move buffer to right dock"),
    ])
}

/// The human name of an operator (`d`/`c`/`y`/`=`), for the operator-pending hint's
/// label (`d` → "Delete"). The grammar's operator alphabet — kept beside the
/// operator dispatch in [`parse_command`] (`'d' | 'c' | 'y' | '='`).
fn operator_name(op: char) -> &'static str {
    match op {
        'd' => "Delete",
        'c' => "Change",
        'y' => "Yank",
        '=' => "Indent",
        '>' => "Shift right",
        '<' => "Shift left",
        _ => "Operator",
    }
}

/// The operator-range alphabet an operator (`d`/`c`/`y`/`=`) awaits — the motions
/// and object-introducers [`parse_step`] accepts after an operator. The plain
/// motions complete the range; the find / text-object / mark / `g` keys are groups
/// that arm a further stage. `op` is the pending operator, so its doubled form
/// (`dd`/`cc`/`yy`/`==`) lists as "current line(s)". Curated for legibility — the
/// common motions, not every alias — mirroring how which-key shows operator-pending.
fn operator_motion_continuations(op: char) -> Vec<CommandContinuation> {
    let mut out = conts(&[
        ("w", "to next word"),
        ("W", "to next WORD"),
        ("b", "back a word"),
        ("e", "to word end"),
        ("j", "line down (linewise)"),
        ("k", "line up (linewise)"),
        ("$", "to end of line"),
        ("0", "to line start"),
        ("^", "to first non-blank"),
        ("h", "left"),
        ("l", "right"),
        ("G", "to end of file"),
    ]);
    // The doubled operator acts on whole line(s) (`dd`/`cc`/`yy`/`==`).
    out.push(CommandContinuation {
        key: op.to_string(),
        desc: "current line(s)".to_string(),
        group: false,
    });
    // Groups: keys that arm a further stage rather than completing the range.
    for (key, desc) in [
        ("f", "find char →"),
        ("t", "till char →"),
        ("F", "find char back →"),
        ("T", "till char back →"),
        ("i", "inner object →"),
        ("a", "around object →"),
        ("g", "g-motion →"),
        ("`", "to mark →"),
        ("'", "to mark line →"),
        ("/", "search forward →"),
        ("?", "search backward →"),
    ] {
        out.push(CommandContinuation {
            key: key.to_string(),
            desc: desc.to_string(),
            group: true,
        });
    }
    out
}

/// The actions that *consume* a selected register (`"a` → these), for the
/// register-armed hint. The register modifies the next yank / delete / paste: `p`/`P`
/// and `x` complete immediately; the operators `d`/`c`/`y` are groups that still need
/// a motion (they descend into operator-pending). Mirrors how the register rides
/// through count/operator accumulation in [`PendingCommand`].
fn register_action_continuations() -> Vec<CommandContinuation> {
    let mut out = conts(&[
        ("p", "paste after"),
        ("P", "paste before"),
        ("x", "delete char into"),
    ]);
    for (key, desc) in [("y", "yank →"), ("d", "delete →"), ("c", "change →")] {
        out.push(CommandContinuation {
            key: key.to_string(),
            desc: desc.to_string(),
            group: true,
        });
    }
    out
}

/// The enumerated text objects an `i`/`a` introducer awaits — the
/// [`ObjectKind::from_key`] alphabet. Kept beside it so a new object appears in both.
fn text_object_continuations() -> Vec<CommandContinuation> {
    conts(&[
        ("w", "word"),
        ("W", "WORD"),
        ("p", "paragraph"),
        ("s", "sentence"),
        ("(", "() parentheses"),
        ("{", "{} braces"),
        ("[", "[] brackets"),
        ("<", "<> angle brackets"),
        ("\"", "double quotes"),
        ("'", "single quotes"),
        ("`", "backticks"),
        ("b", "() block"),
        ("B", "{} block"),
        ("f", "function (tree-sitter)"),
        ("a", "argument (tree-sitter)"),
        ("c", "comment (tree-sitter)"),
        ("t", "type (tree-sitter)"),
    ])
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
            // Any character is a candidate object key: the built-in alphabet and the
            // user registry (`btv.textobject.map`) are both consulted at execution
            // time (`resolve_text_object`), which the pure grammar can't see. An
            // unknown key resolves to nothing there and cancels, the same outcome as
            // `AbortObject`. A non-character key (only reachable if `<Esc>` didn't
            // already cancel above) still aborts.
            return match key.as_char() {
                Some(key) => Complete(ResolvedCommand::TextObject { ia, key }),
                None => AbortObject,
            };
        }
        Stage::WindowPending => {
            // A second `<C-w>` opens the dock layer-switch prefix (bemtvi's
            // repurposing of vim's `<C-w><C-w>`); any other key is an ordinary
            // window command in the current layer.
            if key.ctrl && key.code == KeyCode::Char('w') {
                let mut next = pending.clone();
                next.stage = Stage::WindowLayerPending;
                return Prefix(next);
            }
            return match window_command(key) {
                Some(cmd) => Complete(ResolvedCommand::Window(cmd)),
                None => Reset,
            };
        }
        Stage::WindowLayerPending => {
            return match layer_window_command(key) {
                Some(cmd) => Complete(ResolvedCommand::WindowLayer(cmd)),
                None => Reset,
            };
        }
        Stage::ZPending => {
            // Fold commands share the `z` prefix with the viewport ones; resolve
            // them first, then fall through to `zz`/`zt`/`zb`/`z.`/…
            if let Some(step) = fold_command(key, pending, mode) {
                return step;
            }
            return match view_command(key) {
                Some((place, first_nonblank)) => {
                    Complete(ResolvedCommand::Normal(NormalCmd::ViewScroll {
                        place,
                        first_nonblank,
                        count: pending.count,
                    }))
                }
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
        Stage::RecordPending => {
            // The next key names the macro register. `a`–`z` record, `A`–`Z` append
            // to the lowercase one, `0`–`9` are vim-writable too; anything else (a
            // read-only special like `%` or `:`, punctuation) is a loud dead-end,
            // as at the `"` prompt.
            return match key.as_char() {
                Some(name) if is_recordable_register(name) => {
                    Complete(ResolvedCommand::MacroRecord(name))
                }
                _ => Reset,
            };
        }
        Stage::PlayPending => {
            // `<F3>` again means "the last register played" (vim's `@@`); otherwise
            // the key names the register — any readable one, including the specials
            // (`<F3>:` re-runs the last ex command, vim's `@:`). An unknown name is
            // a loud dead-end, as at the `"` prompt.
            if is_macro_play_key(key) {
                return Complete(ResolvedCommand::MacroPlay(None));
            }
            return match key.as_char() {
                Some(name) if is_register_name(name) => {
                    Complete(ResolvedCommand::MacroPlay(Some(name)))
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
        Stage::MarkJumpPending(kind, set_jump) => {
            // The next key names the mark to jump to: a settable name or a read-only
            // automatic special (`` ` ``/`'`/`.`/`^`/`[`/`]`/`<`/`>`). An unsupported
            // name (a digit, untracked punctuation) is a loud dead-end (`Reset`).
            return match key.as_char() {
                Some(name) if marks::is_jumpable_mark(name) => {
                    Complete(ResolvedCommand::Motion(match kind {
                        MarkJumpKind::Exact => Motion::MarkJumpExact(name, set_jump),
                        MarkJumpKind::Line => Motion::MarkJumpLine(name, set_jump),
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

    // `<F2>{reg}` starts recording a macro; the register name follows. bemtvi does
    // NOT put this on vim's `q` — `q` stays a free key for the user (and for the
    // `q`-to-close convention the view/dock buffers use); `btv.keymap.set("n", "q",
    // "<F2>")` restores the vim binding for those who want it. Like `m`/`"`, the
    // trigger is not a motion and never follows an operator, so it arms only at a
    // command boundary with no operator pending (and not mid-`g`). The `<F2>` that
    // *stops* a recording is state-dependent and so cannot be decided by this pure
    // fold — `handle_normal` intercepts it before we are reached, and the oracle
    // sees a harmless prefix.
    if !gpending && pending.operator.is_none() && mode == Mode::Normal && is_macro_record_key(key) {
        let mut next = pending.clone();
        next.stage = Stage::RecordPending;
        return Prefix(next);
    }

    // `{count}<F3>{reg}` plays a macro back; the register name follows. Same
    // placement rule as `<F2>` above — a command boundary with no operator pending.
    // Any leading count is the repeat count, so it is left on `pending` for
    // `execute` to read.
    if !gpending && pending.operator.is_none() && mode == Mode::Normal && is_macro_play_key(key) {
        let mut next = pending.clone();
        next.stage = Stage::PlayPending;
        return Prefix(next);
    }

    // `z` prefix: viewport repositioning (`zz`/`zt`/`zb`, `z.`/`z<CR>`/`z-`). The
    // second key names the placement. Like `m`/`"`, `z` is not a motion and never
    // follows an operator, so it arms only at a command boundary with no operator
    // pending (and not mid-`g`). It carries any leading count as the target line.
    if !gpending && pending.operator.is_none() && key.as_char() == Some('z') {
        let mut next = pending.clone();
        next.stage = Stage::ZPending;
        return Prefix(next);
    }

    // `g` prefix: a lone `g` arms it; a second `g` is `gg`; `gt`/`gT` cycle tabs;
    // `` g` ``/`g'` jump to a mark *without* setting the jumplist.
    if gpending {
        match key.as_char() {
            Some('g') => return Complete(ResolvedCommand::Motion(Motion::GotoTop)),
            // `gc` is the comment operator. In visual mode it toggles the selection
            // immediately; in normal mode it arms the operator (a motion / text
            // object follows, or a second `c` doubles it to the current line(s)).
            Some('c') => {
                if mode.is_visual() {
                    return Complete(ResolvedCommand::VisualOperate(COMMENT_OP));
                }
                let mut next = pending.clone();
                next.operator = Some(COMMENT_OP);
                next.op_count = pending.count;
                next.count = None;
                next.stage = Stage::Start;
                return Prefix(next);
            }
            // `gj` / `gk` move by *display* line (soft-wrap aware): within a wrapped
            // line they step continuation rows; with `nowrap` they are plain `j`/`k`.
            Some('j') => return Complete(ResolvedCommand::Motion(Motion::DisplayDown)),
            Some('k') => return Complete(ResolvedCommand::Motion(Motion::DisplayUp)),
            // `g0` / `g^` / `g$` are the within-row siblings of `gj`/`gk`: they move
            // to the first column / first non-blank / last column of the cursor's
            // *display* row. With `nowrap` they collapse to plain `0`/`^`/`$`.
            Some('0') => return Complete(ResolvedCommand::Motion(Motion::DisplayLineStart)),
            Some('^') => return Complete(ResolvedCommand::Motion(Motion::DisplayFirstNonBlank)),
            Some('$') => return Complete(ResolvedCommand::Motion(Motion::DisplayLineEnd)),
            // `gh` / `gH` start Select mode (vim's charwise / linewise select) — the
            // keyboard entry to the mode `btv.win.select_range` drives programmatically.
            Some('h') => return Complete(ResolvedCommand::Normal(NormalCmd::EnterSelect(false))),
            Some('H') => return Complete(ResolvedCommand::Normal(NormalCmd::EnterSelect(true))),
            // `gv` reselects the last Visual selection (its area *and* its
            // charwise/linewise shape), read back from the `` `< `` / `` `> ``
            // marks and the buffer's recorded last-visual kind.
            Some('v') => return Complete(ResolvedCommand::Normal(NormalCmd::ReselectVisual)),
            Some('t') => {
                return Complete(ResolvedCommand::Normal(NormalCmd::TabNext(pending.count)))
            }
            Some('T') => {
                return Complete(ResolvedCommand::Normal(NormalCmd::TabPrev(pending.count)))
            }
            // `g;` / `g,` walk the change list (older / newer change positions).
            Some(';') => return Complete(ResolvedCommand::Normal(NormalCmd::ChangeOlder)),
            Some(',') => return Complete(ResolvedCommand::Normal(NormalCmd::ChangeNewer)),
            // `` g` `` / `g'` — like `` ` ``/`'` but they do not record the
            // previous-context mark / jumplist (vim's quiet jump). The name follows;
            // arm the same jump stage with `set_jump = false`.
            Some('`') => {
                let mut next = pending.clone();
                next.stage = Stage::MarkJumpPending(MarkJumpKind::Exact, false);
                return Prefix(next);
            }
            Some('\'') => {
                let mut next = pending.clone();
                next.stage = Stage::MarkJumpPending(MarkJumpKind::Line, false);
                return Prefix(next);
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
            next.stage = Stage::MarkJumpPending(kind, true);
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

    // In MULTICURSOR placement mode, `c` is the place-cursor command. A bare `c`
    // (no count) drops one cursor at the primary; a counted `{n}c{motion}` falls
    // through to the operator path below, placing `n` cursors along the motion
    // (`10cj`). `cc` doubles to a linewise place (handled in `begin_operator`).
    if mode == Mode::MultiCursor
        && pending.operator.is_none()
        && pending.count.is_none()
        && key.as_char() == Some('c')
    {
        return Complete(RC::Normal(N::PlaceCursor));
    }

    // With an operator pending only a doubled operator, a search-motion hand-off,
    // or a cancel reaches here (motions were resolved just above).
    if let Some(op) = pending.operator {
        return match key.as_char() {
            Some(c) if c == op => Complete(RC::DoubledOperator(op)),
            // `gcc`: the comment operator doubles on a second `c` (its mnemonic
            // key), not on its internal sentinel char.
            Some('c') if op == COMMENT_OP => Complete(RC::DoubledOperator(op)),
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
            // `<C-o>`/`<C-i>` walk the jump list (normal mode only — in visual,
            // vim leaves them unbound). `<C-i>` and `<Tab>` are the same key in a
            // terminal; the `<Tab>` spelling is caught below.
            KeyCode::Char('o') if mode == Mode::Normal => Complete(RC::Normal(N::JumpBack)),
            KeyCode::Char('i') if mode == Mode::Normal => Complete(RC::Normal(N::JumpForward)),
            KeyCode::Char('^') | KeyCode::Char('6') => Complete(RC::Normal(N::AltBuffer)),
            // `<C-g>` in Visual toggles to Select mode, keeping the selection (vim's
            // Visual↔Select switch). In Normal it is left unbound (falls through to
            // Reset), as the toggle only makes sense with a live selection.
            KeyCode::Char('g') if mode.is_visual() => Complete(RC::Normal(N::ToggleVisualSelect)),
            _ => Reset,
        };
    }

    // PageDown / PageUp default to a half-page scroll, like <C-d> / <C-u>.
    match key.code {
        KeyCode::PageDown => return Complete(RC::Normal(N::ScrollHalf(true))),
        KeyCode::PageUp => return Complete(RC::Normal(N::ScrollHalf(false))),
        // `<Tab>` is `<C-i>` in a terminal: jump forward in the list (normal mode).
        KeyCode::Tab if mode == Mode::Normal && !key.shift => {
            return Complete(RC::Normal(N::JumpForward))
        }
        _ => {}
    }

    // `<A-c>`/`<M-c>` enters multi-cursor placement mode and drops a cursor (and,
    // from within the mode, drops another). Checked ahead of `as_char`, which
    // rejects any alt-modified key. Not in visual mode.
    if key.alt && key.code == KeyCode::Char('c') && !mode.is_visual() {
        return Complete(RC::Normal(N::AddCursor));
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
            '>' => return Complete(RC::VisualOperate('>')),
            '<' => return Complete(RC::VisualOperate('<')),
            'v' => return Complete(RC::Normal(N::EnterVisual)),
            'V' => return Complete(RC::Normal(N::EnterVisualLine)),
            // `o`/`O` move the cursor to the other end of the selection (charwise/
            // linewise both; no visual-block corner distinction yet), so you can
            // extend the side you started from.
            'o' | 'O' => return Complete(RC::Normal(N::VisualSwapEnds)),
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
        'd' | 'c' | 'y' | '=' | '>' | '<' => {
            // Begin an operator (prefix): move count → op_count, drop g-pending.
            // `=` is the reindent operator (`==`, `=motion`, `gg=G`); `>`/`<` are
            // the shift-right / shift-left operators (`>>`, `<<`, `>j`, `>ip`).
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

/// The pending [`CommandPending`] hint a key run *would* leave the grammar in,
/// folded hypothetically from a clean boundary — the same fold as
/// [`command_status`], but it returns the hint for the carried prefix instead of a
/// coarse class. `None` when the run ends on a command boundary (nothing pending)
/// or breaks the grammar (cancel/reset/abort).
///
/// The server uses this to merge built-in continuations into a prefix the matcher
/// has **withheld** but not yet released to the editor: `g` is withheld by a
/// `g`-prefixed map (e.g. the LSP `gd`/`gD`/`gr` maps), so the editor's own pending
/// is still `Start`, yet `g` should still surface `gg`/`gt`/… A withheld leader like `<Space>` folds
/// to a *complete* motion (space → `l`), so this returns `None` and never spuriously
/// merges a built-in into a mapped-only prefix.
pub fn command_pending_after(mode: Mode, keys: &[Key]) -> Option<CommandPending> {
    let mut pending = PendingCommand::default();
    for &key in keys {
        match parse_step(mode, &pending, key) {
            ParseStep::Prefix(p) => pending = p,
            // A *completed* command mid-run means these keys are not a single built-in
            // prefix — the matcher withheld them only because they prefix a *mapping*
            // (e.g. a `<Space>g` leader group, where `<Space>` is a complete motion as
            // a built-in). Merging the trailing `g`'s continuations there would be
            // wrong, so bail. A clean lone built-in prefix (`g`, `2g`, `<C-w>`) never
            // completes before the end and is the only thing that yields a hint here.
            ParseStep::Complete(_)
            | ParseStep::Cancel
            | ParseStep::Reset
            | ParseStep::AbortObject => return None,
        }
    }
    pending_hint(&pending)
}

/// Project a [`PendingCommand`] into the which-key / showcmd hint — the pure core of
/// [`Editor::command_pending`] and [`command_pending_after`]. `None` at a clean
/// boundary with no operator armed (nothing to hint). The finite built-in prefixes
/// (`g`/`z`/`<C-w>`/`<C-w><C-w>`), operator-pending (the motion alphabet), and the
/// text-object introducer carry an enumerated `continuations` list. The **dynamic**
/// states (registers, marks) return an empty list here — they are enriched from live
/// editor state in [`Editor::command_pending`] — and the truly-any-character leaves
/// (find-char, replace) carry only a `label`.
fn pending_hint(p: &PendingCommand) -> Option<CommandPending> {
    // A clean boundary with nothing armed (no operator, no register) has nothing to
    // hint. `Stage::Start` *with* an operator is operator-pending, and *with* a
    // selected register (no operator yet) is the register-armed action menu — both
    // below.
    if p.stage == Stage::Start && p.operator.is_none() && p.register.is_none() {
        return None;
    }
    // `label` names the command in human terms (`d` → "Delete"), so a which-key
    // titles `keys — label` instead of a cryptic key. `continuations` enumerate the
    // next keys for the finite states; the register / mark states return an *empty*
    // list here and are enriched from live editor state in `Editor::command_pending`
    // (this pure projection has no editor to read them from). Find-char / replace
    // take *any* character, so they stay label-only.
    let (trigger, label, continuations): (String, &'static str, Vec<CommandContinuation>) = match p
        .stage
    {
        // `Stage::Start` carries one of two armed states (the clean case returned
        // above): an operator (`d`/`c`/`y`/`=`) awaiting its motion, or a selected
        // register (`"a`) awaiting the yank / delete / paste that will use it. The
        // armed key (operator / register) is emitted by the keys builder, so the
        // trigger is empty.
        Stage::Start => match p.operator {
            Some(op) => (
                String::new(),
                operator_name(op),
                operator_motion_continuations(op),
            ),
            // A register is selected (operator None, else the arm above): show the
            // register-consuming actions. The `"a` in `keys` says *which* register.
            None => (
                String::new(),
                "Use register",
                register_action_continuations(),
            ),
        },
        Stage::FindPending(k) => {
            let label = match k {
                FindKind::Find => "Find character",
                FindKind::Till => "Till character",
                FindKind::FindBack => "Find character backward",
                FindKind::TillBack => "Till character backward",
            };
            (k.as_char().to_string(), label, Vec::new())
        }
        Stage::ReplacePending => ("r".to_string(), "Replace character", Vec::new()),
        Stage::TextObjectPending(c) => (c.to_string(), "Text object", text_object_continuations()),
        Stage::GPending => ("g".to_string(), "Go", g_continuations()),
        Stage::ZPending => ("z".to_string(), "Scroll / fold", z_continuations()),
        Stage::RegisterPending => ("\"".to_string(), "Register", Vec::new()),
        Stage::MarkSetPending => ("m".to_string(), "Set mark", Vec::new()),
        Stage::RecordPending => ("<F2>".to_string(), "Record macro", Vec::new()),
        Stage::PlayPending => ("<F3>".to_string(), "Play macro", Vec::new()),
        Stage::MarkJumpPending(kind, set_jump) => {
            let base = match kind {
                MarkJumpKind::Exact => "`",
                MarkJumpKind::Line => "'",
            };
            // The jumplist-skipping spellings are `g`/`g'`; plain `` ` ``/`'` set it.
            let keys = if set_jump {
                base.to_string()
            } else {
                format!("g{base}")
            };
            (keys, "Jump to mark", Vec::new())
        }
        Stage::WindowPending => ("<C-w>".to_string(), "Window", window_continuations()),
        Stage::WindowLayerPending => (
            "<C-w><C-w>".to_string(),
            "Dock layer",
            window_layer_continuations(),
        ),
    };
    let mut keys = String::new();
    if let Some(r) = p.register {
        keys.push('"');
        keys.push(r);
    }
    if let Some(n) = p.op_count {
        keys.push_str(&n.to_string());
    }
    if let Some(op) = p.operator {
        keys.push(op);
    }
    if let Some(n) = p.count {
        keys.push_str(&n.to_string());
    }
    keys.push_str(&trigger);
    Some(CommandPending {
        label,
        keys,
        continuations,
    })
}

impl Editor {
    /// The built-in command grammar's current pending state for the
    /// `btv.on_key_pending` (which-key / showcmd) signal — **source B** of the
    /// oracle. `Some` whenever a key has armed an argument stage (`f`/`t`/`F`/`T`,
    /// `r`, `i`/`a`, `z`, `g`, marks, registers, `<C-w>`) *or* an operator (`d`/`c`/
    /// `y`/`=`) is awaiting its motion; `None` at a truly clean boundary, where there
    /// is nothing to hint. The finite prefixes (`g`/`z`/`<C-w>`/`<C-w><C-w>`) carry
    /// an enumerated `continuations` list; operator-pending lists the motion alphabet
    /// and the register / mark states list what is *actually* stored (enriched here
    /// from live editor state — the pure [`pending_hint`] can't read it). Only the
    /// truly-any-character leaves (find-char, replace) stay label-only. The keymap
    /// matcher's withheld mapped prefix (sources A/C) takes precedence; the server
    /// consults this directly only when no mapped prefix is live, and otherwise
    /// *merges* this state's continuations into the withheld one via
    /// [`command_pending_after`] (so `g`, withheld by the LSP defaults, still shows the
    /// built-in `gg`/`gt`/…). The returned `keys` mirror vim's showcmd: register, the
    /// pre-operator count, the operator, the post-operator count, then the stage's
    /// trigger key.
    pub fn command_pending(&self) -> Option<CommandPending> {
        let mut hint = pending_hint(&self.pending)?;
        // Enrich the dynamic states with live editor contents the pure projection
        // can't see: the registers that actually hold text, and the marks actually
        // set. `m` (set-mark) lists the *existing* marks for reference.
        match self.pending.stage {
            Stage::RegisterPending | Stage::PlayPending => {
                hint.continuations = self.register_continuations()
            }
            Stage::MarkJumpPending(..) | Stage::MarkSetPending => {
                hint.continuations = self.set_mark_continuations()
            }
            // Append the user's registered tree-sitter objects (`btv.textobject.map`)
            // to the object menu, so a bound `il`/`af`/… shows in which-key beside the
            // built-ins. A registration that overrides a built-in key replaces its row
            // (keyed dedup, the override winning) rather than duplicating it.
            Stage::TextObjectPending(ia) => {
                for (key, capture) in self.textobject_map_entries(ia) {
                    let key = key.to_string();
                    hint.continuations.retain(|c| c.key != key);
                    hint.continuations.push(CommandContinuation {
                        key,
                        desc: capture,
                        group: false,
                    });
                }
            }
            _ => {}
        }
        Some(hint)
    }

    /// The registers that currently hold text, as which-key continuations: the
    /// register name keys a short one-line preview of its contents (source for the
    /// `"`-pending hint). Empty when nothing has been yanked / deleted yet, so the
    /// hint falls back to its `Register` label.
    ///
    /// The stored writable cells come first, then the readable specials — `%`
    /// (filename), `/` (last search), `:` (last command), `.` (last insert), and
    /// the clipboard `+` / `*` — each surfaced only while it actually resolves to
    /// non-empty text (the `+`/`*` probe asks the clipboard provider). These are
    /// valid paste sources you can type at the `"` prompt, so the popup lists them
    /// alongside the stored registers; the parallel `register_mirror` (the
    /// `getreg` projection) covers the same specials.
    pub(crate) fn register_continuations(&self) -> Vec<CommandContinuation> {
        let mut out: Vec<CommandContinuation> = self
            .registers
            .entries()
            .into_iter()
            .map(|(name, text, _kind)| CommandContinuation {
                key: name.to_string(),
                desc: preview_text(text),
                group: false,
            })
            .collect();
        for name in ['%', '/', ':', '.', '+', '*'] {
            if let Some((text, _kind)) = self.register_text(Some(name)) {
                if !text.is_empty() {
                    out.push(CommandContinuation {
                        key: name.to_string(),
                        desc: preview_text(&text),
                        group: false,
                    });
                }
            }
        }
        out
    }

    /// The marks that are currently set, as which-key continuations. Every row leads
    /// with the mark's **position** (`{line}:{col}`) so it reads as a place, never a
    /// stray line of text. A read-only automatic mark (`'`/`` ` ``/`.`/`^`/…) shows
    /// its *meaning* ("previous position", "last insert") rather than its line —
    /// otherwise a `'` mark on a comment line looked like a mystery snippet. A named
    /// mark (`a`–`z`) shows a short preview of its line; a global `A`–`Z` shows the
    /// file it points into. Source for the `` ` ``/`'`/`m` hints; empty when nothing
    /// is set, so the hint falls back to its label.
    fn set_mark_continuations(&self) -> Vec<CommandContinuation> {
        let mut out = Vec::new();
        // Buffer-local marks (named `a`–`z` and the read-only specials), keyed on the
        // current buffer.
        for (&name, &(line, col)) in &self.buffer().marks {
            let pos = format!("{}:{}", line + 1, col);
            let context = match special_mark_name(name) {
                Some(meaning) => meaning.to_string(),
                None => preview_text(self.buffer().line(line.min(self.last_line())).trim()),
            };
            out.push(CommandContinuation {
                key: name.to_string(),
                desc: format!("{pos}  {context}"),
                group: false,
            });
        }
        // Global file marks (`A`–`Z`): position plus the file / buffer they point at.
        for (&name, &(buf, cur)) in &self.global_marks {
            let detail = self.buffer_fallback_name(buf);
            out.push(CommandContinuation {
                key: name.to_string(),
                desc: format!("{}:{}  {}", cur.line + 1, cur.col, detail),
                group: false,
            });
        }
        out
    }

    /// Drive one key through the normal/visual grammar. A thin loop: the pure
    /// [`parse_step`] decides; [`Editor::execute`] (and the cancel arms here)
    /// apply. All the old inline pending-state bookkeeping now lives in
    /// `parse_step`, so this is the *only* place a normal-mode key enters and the
    /// grammar has exactly one home.
    pub(crate) fn handle_normal(&mut self, key: Key) {
        self.message.clear();
        // The `<F2>` that STOPS a recording, ahead of the grammar. Whether `<F2>`
        // opens a register prompt or ends the macro depends on live state, which the
        // pure [`parse_step`] fold (shared with the matcher's `command_status`
        // oracle) cannot see — so the state-dependent half is decided here and the
        // pure half stays pure. Normal mode with a clean pending only: an `<F2>`
        // after an operator / count is that command's business.
        if self.macro_record.is_some()
            && self.mode == Mode::Normal
            && self.pending.is_clean()
            && is_macro_record_key(key)
        {
            self.stop_recording();
            return;
        }
        match parse_step(self.mode, &self.pending, key) {
            ParseStep::Prefix(p) => self.pending = p,
            ParseStep::Complete(cmd) => self.execute(cmd),
            ParseStep::Cancel => {
                if self.mode == Mode::MultiCursor {
                    // A half-typed place command (`10c…`) cancels without leaving the
                    // mode; a clean `<Esc>` finishes placement — the cursors persist
                    // into Normal, where motions/edits act on them all.
                    let had_pending = self.pending.operator.is_some()
                        || self.pending.count.is_some()
                        || self.pending.stage != Stage::Start;
                    self.reset_pending();
                    if !had_pending {
                        self.finish_multicursor();
                    }
                } else {
                    self.reset_pending();
                    if self.mode.is_visual() {
                        // Leaving Visual mode stamps the `` `< `` / `` `> `` selection
                        // marks (what `gv` and the angle-mark jumps read) before we
                        // drop back to Normal. A placed multi-cursor set survives —
                        // only the per-cursor selections collapse (a *second* `<Esc>`
                        // in Normal then drops the cursors).
                        self.record_visual_marks();
                        self.clear_anchor_marks();
                        self.mode = Mode::Normal;
                        self.clamp_cursor();
                    } else {
                        // `<Esc>` in Normal collapses any placed multi-cursor set
                        // back to the primary.
                        self.clear_secondary_cursors();
                    }
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
                // MULTICURSOR `{count}c{motion}`: the count is the motion *distance*,
                // matching vim's relative-line intuition — `3cj` drops a cursor on the
                // current line and at each of the three lines the motion visits, so the
                // bottom cursor lands on relative line 3 (where `3j` would put you), not
                // 2. A cursor is placed at the start *and* after each of `count` motion
                // steps → `count + 1` cursors (e.g. `2cw` on "one two three" covers
                // "one", "two", and "three"). Non-toggling, so a step onto an existing
                // cursor (or the entry cursor under the primary) adds without clearing it.
                if self.mode == Mode::MultiCursor && self.pending.operator == Some('c') {
                    let n = self.effective_count().max(1);
                    self.reset_pending(); // each step is a single, un-counted motion
                                          // Record once before the batch so the whole `{count}c{motion}`
                                          // run undoes as a single placement step (`3cj` → one `u`).
                    self.record_placement_undo();
                    self.ensure_cursor_here();
                    for _ in 0..n {
                        if let Some(mr) = self.resolve_motion(m) {
                            self.apply_movement(mr);
                        }
                        self.ensure_cursor_here();
                    }
                    return;
                }
                // Multi-cursor (placement finished): resolve and apply the motion
                // at every cursor — a plain motion moves them all, an
                // operator-pending motion (`dw`/`yw`/`cw`) operates at each. In a
                // visual mode the motion extends every cursor's selection (its head
                // moves, its anchor stays); operators never pend there (the
                // selection's `d`/`y`/`c` route through `VisualOperate`).
                if self.has_secondary_cursors()
                    && (self.mode == Mode::Normal || self.mode.is_visual())
                {
                    if self.pending.operator.is_some() {
                        self.edit_each_cursor(|ed| ed.apply_motion_once(m));
                    } else {
                        self.for_each_cursor(|ed| ed.apply_motion_once(m));
                    }
                    self.reset_pending();
                    return;
                }
                // A global mark `A`–`Z` may point into another buffer. That jump
                // can't be a within-buffer motion offset (and operating across
                // files is meaningless), so it's handled here: switch to the mark's
                // buffer, then land. Marks resolving into the current buffer (every
                // lowercase mark, and a global whose buffer is current) fall through
                // to the ordinary motion path, so `` d`a `` still operates. An unset
                // or closed-buffer mark resolves to `None` here and falls through to
                // the loud *E20* miss below.
                if let Motion::MarkJumpExact(name, set_jump)
                | Motion::MarkJumpLine(name, set_jump) = m
                {
                    // A numbered mark `'0`–`'9` is a cross-session position: resolve
                    // it by opening its file, then make the cross-buffer jump (it
                    // always points at a *past* session's location, never the live
                    // buffer). An unresolved digit falls through to the E20 miss.
                    if name.is_ascii_digit() {
                        if let Some(loc) = self.resolve_numbered_mark(name) {
                            if set_jump {
                                self.record_jump_context();
                            }
                            self.jump_to_mark_buffer(loc, matches!(m, Motion::MarkJumpLine(..)));
                            self.reset_pending();
                            return;
                        }
                    }
                    // A global mark restored from shada is pending until its file
                    // is first reopened — promote it (opening the file) before the
                    // location lookup, so a cross-session `` `A `` lands.
                    self.resolve_pending_global_mark(name);
                    if let Some(loc) = self.mark_location(name) {
                        if loc.buf != self.cur_buffer() {
                            // A jump still records the pre-jump context in the source
                            // buffer (so `` `` `` returns), then crosses over — unless
                            // it is the quiet `` g` ``/`g'` spelling (`set_jump`).
                            if set_jump {
                                self.record_jump_context();
                            }
                            self.jump_to_mark_buffer(loc, matches!(m, Motion::MarkJumpLine(..)));
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
                            self.beep();
                            self.pending.stage = Stage::Start;
                        }
                        // A jump to a mark that was never set: report it loudly
                        // (vim's *E20: Mark not set*) instead of silently leaving
                        // the cursor — and abort any pending operator with it.
                        Motion::MarkJumpExact(..) | Motion::MarkJumpLine(..) => {
                            self.echo("E20: Mark not set");
                            self.reset_pending();
                        }
                        // Every other execution miss — an unmatched `f{char}`, a
                        // `;` with no prior find, a vertical motion already at the
                        // buffer edge — is silent, and would beep in vim. Flag it,
                        // so a macro playing back stops here instead of grinding
                        // out the rest of its repeats against the same line.
                        _ => {
                            self.beep();
                            self.reset_pending();
                        }
                    },
                }
            }
            ResolvedCommand::DoubledOperator(op) => self.begin_operator(op),
            ResolvedCommand::OperatorSearch { op, dir } => {
                let count = self.effective_count();
                self.search_operator = Some(op);
                self.enter_search(dir, count);
            }
            ResolvedCommand::TextObject { ia, key } => {
                let count = self.effective_count();
                // Multi-cursor (Normal mode only): operate over the object at every
                // cursor, as one undo group.
                if self.has_secondary_cursors() && self.mode == Mode::Normal {
                    self.edit_each_cursor(|ed| ed.apply_text_object_once(ia, key, count));
                    self.reset_pending();
                    return;
                }
                if let Some((lo, hi, linewise)) = self.resolve_text_object(ia, key, count) {
                    self.apply_text_object(lo, hi, linewise);
                } else if self.mode.is_visual() {
                    // No object at the cursor: keep the selection (and count).
                    self.pending.stage = Stage::Start;
                } else {
                    self.reset_pending();
                }
            }
            ResolvedCommand::Replace(c) => {
                if self.mode.is_visual() {
                    // Vim's visual `r`: the whole selection becomes `c`, then
                    // visual mode exits — not the normal-mode one-char replace.
                    self.visual_replace(c);
                    return;
                }
                let count = self.effective_count();
                self.edit_each_cursor(|ed| ed.replace_char(c, count));
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
            ResolvedCommand::MacroRecord(reg) => {
                self.start_recording(reg);
                self.reset_pending();
            }
            ResolvedCommand::MacroPlay(reg) => {
                let count = self.effective_count();
                self.reset_pending();
                self.play_macro(reg, count);
            }
            ResolvedCommand::VisualOperate(op) => self.visual_operate(op),
            ResolvedCommand::Normal(cmd) => self.execute_normal(cmd),
            ResolvedCommand::Window(cmd) => self.execute_window(cmd),
            ResolvedCommand::WindowLayer(cmd) => self.execute_window_layer(cmd),
        }
    }

    /// Apply a `<C-w>` window command. Pending state is already a clean boundary
    /// (the prefix consumed it), so this just reads the count and drives the
    /// window layout of the focused layer.
    fn execute_window(&mut self, cmd: WindowCmd) {
        // The resize family honors a leading count (`3<C-w>+`); read it before
        // the pending state is cleared.
        let count = self.effective_count() as isize;
        self.reset_pending();
        self.run_window_cmd(cmd, count);
    }

    /// Mode-independent `<C-w><C-w>` dock-navigation chord, run ahead of mode
    /// dispatch in [`Editor::input`]. Returns `true` when the key was consumed by
    /// the chord — a held prefix `<C-w>`, or the completed cross — in which case
    /// `input` stops. Normal / MultiCursor mode keep their grammar path (which
    /// also handles single-`<C-w>` window commands); every other mode gets the
    /// docks here, so `<C-w><C-w>` reaches them from insert, visual, command and
    /// terminal mode too. A miss replays the held `<C-w>`(s) into the current mode
    /// so a lone `<C-w>` keeps its original meaning (e.g. sent to the PTY child).
    pub(crate) fn dock_chord_intercept(&mut self, key: Key) -> bool {
        // Normal / MultiCursor route `<C-w>` through `parse_step`, which owns both
        // the window prefix and the layer cross — don't double-handle them.
        let grammar_owns_cw = matches!(self.mode, Mode::Normal | Mode::MultiCursor);
        let cw = is_dock_chord_key(key);
        match self.dock_chord {
            DockChord::Idle => {
                if grammar_owns_cw || !cw {
                    return false;
                }
                // First `<C-w>` in a non-grammar mode: hold it for the second.
                self.dock_chord = DockChord::FirstCw;
                true
            }
            DockChord::FirstCw => {
                if cw {
                    self.dock_chord = DockChord::SecondCw;
                    return true;
                }
                // Not the chord: replay the held `<C-w>` into the current mode,
                // then let `input` handle this key normally.
                self.dock_chord = DockChord::Idle;
                self.dispatch_mode_key(Key::ctrl('w'));
                false
            }
            DockChord::SecondCw => {
                self.dock_chord = DockChord::Idle;
                if let Some(cmd) = layer_window_command(key) {
                    // `execute_window_layer` parks the current mode for resume and
                    // leaves it cleanly — the same path the Normal-mode grammar takes,
                    // so a cross *out* of insert/visual/terminal and the cross *back*
                    // (always from Normal in the dock) are symmetric.
                    self.execute_window_layer(cmd);
                    return true;
                }
                // Dead-end after `<C-w><C-w>`: replay both held `<C-w>`s, then let
                // `input` handle this key normally.
                self.dispatch_mode_key(Key::ctrl('w'));
                self.dispatch_mode_key(Key::ctrl('w'));
                false
            }
        }
    }

    /// Route one key into the handler for the current mode, bypassing the dock
    /// chord interceptor — used to replay a held `<C-w>` on a chord miss. Mirrors
    /// the mode dispatch in [`Editor::input`].
    fn dispatch_mode_key(&mut self, key: Key) {
        match self.mode {
            Mode::Terminal => self.handle_terminal_key(key),
            Mode::Insert | Mode::Replace => self.handle_insert(key),
            Mode::Command => self.handle_command(key),
            Mode::Select => self.handle_select(key),
            _ => self.handle_normal(key),
        }
    }

    /// Leave the current mode for a dock-navigation cross: finalize insert/replace
    /// (the `".`/`` `^ `` bookkeeping), drop a visual selection, close the command
    /// line, or leave terminal-job mode — so crossing into a dock never carries an
    /// open insert session or live selection with it. A no-op in Normal mode.
    fn leave_mode_for_dock_nav(&mut self) {
        match self.mode {
            Mode::Insert | Mode::Replace => self.handle_insert(Key::new(KeyCode::Esc)),
            Mode::Visual | Mode::VisualLine => self.handle_normal(Key::new(KeyCode::Esc)),
            // Select mode carries a live selection like Visual; crossing into a dock
            // drops it back to Normal (its `<Esc>`-to-Insert is not what a *nav* cross
            // wants — it should leave the buffer clean, like Visual's `<Esc>`).
            Mode::Select => {
                self.record_visual_marks();
                self.mode = Mode::Normal;
                self.clamp_cursor();
            }
            // The `<Esc>` cancel is a `cmdline` keymap now, not a `handle_command`
            // arm, so close the line directly (this path never goes through the
            // matcher).
            Mode::Command => self.cancel_cmdline(),
            Mode::Terminal => self.leave_terminal_mode(),
            _ => {}
        }
    }

    /// Apply a `<C-w><C-w>` dock layer command: cross between the main area and a
    /// dock, then (for the non-directional forms) run the window command in the
    /// now-focused layer. Shared by the Normal-mode grammar and the cross-mode
    /// chord ([`Editor::dock_chord_intercept`]), so mode parking/resume lives here:
    /// every cross parks the source window's current mode and asks the target's
    /// parked mode to resume, making the round trip mode-transparent.
    fn execute_window_layer(&mut self, cmd: LayerWindowCmd) {
        // Capture the resume mode *before* leaving it (leaving insert backsteps the
        // cursor; we want the pre-backstep column so an end-of-line append resumes
        // where it was), then finalize the current mode cleanly.
        let resume = match self.mode {
            Mode::Insert | Mode::Replace => Some(ResumeState {
                mode: self.mode,
                col: self.cursor.col,
                visual_anchor: Cursor::default(),
            }),
            Mode::Visual | Mode::VisualLine => Some(ResumeState {
                mode: self.mode,
                col: 0,
                visual_anchor: self.visual_anchor,
            }),
            Mode::Terminal => Some(ResumeState {
                mode: Mode::Terminal,
                col: self.cursor.col,
                visual_anchor: Cursor::default(),
            }),
            // A Normal-mode cross parks `None`, clearing any stale parked mode.
            _ => None,
        };
        let src_win = self.windows.current;
        self.leave_mode_for_dock_nav();
        self.windows.get_mut(src_win).resume = resume;
        self.restore_mode_on_enter = true;

        let count = self.effective_count() as isize;
        self.reset_pending();
        match cmd {
            LayerWindowCmd::CrossDir(dir) => {
                // Spatial cross: step to the open region in that direction (main or
                // a dock), wrapping past the far edge. A no-op when nothing in that
                // direction is open.
                if let Some(target) = self.cross_dir_target(dir) {
                    match target {
                        Layer::Main => self.switch_layer(Layer::Main),
                        Layer::Dock(side) => self.focus_dock(side),
                    }
                }
            }
            LayerWindowCmd::MoveDir(dir) => match self.focused_layer {
                // From the main area, move the buffer to the dock on that edge — but
                // only if that dock is open (mirrors `CrossDir`'s closed-dock no-op).
                Layer::Main => {
                    let side = DockSide::from_dir(dir);
                    if self.dock_is_open(side) {
                        self.move_buffer_to_layer(Layer::Dock(side));
                    }
                }
                // From a dock, any directional move sends the buffer back to main.
                Layer::Dock(_) => self.move_buffer_to_layer(Layer::Main),
            },
            LayerWindowCmd::CrossThenWindow(wcmd) => match self.focused_layer {
                Layer::Main => {
                    if self.dock_is_open(self.last_dock) {
                        self.switch_layer(Layer::Dock(self.last_dock));
                        self.run_window_cmd(wcmd, count);
                    }
                }
                Layer::Dock(_) => {
                    self.switch_layer(Layer::Main);
                    self.run_window_cmd(wcmd, count);
                }
            },
        }
        // If the cross was a no-op (e.g. a closed dock), no `enter_window` ran to
        // consume the resume request — clear it so it can't leak into a later focus.
        self.restore_mode_on_enter = false;
    }

    /// The layer a spatial `<C-w><C-w>`+direction cross should focus from the
    /// currently focused region, or `None` when nothing in that direction is open.
    fn cross_dir_target(&self, dir: WinDir) -> Option<Layer> {
        Self::cross_dir_candidates(self.focused_layer, dir)
            .into_iter()
            .find(|&layer| layer != self.focused_layer && self.layer_is_open(layer))
    }

    /// Ordered candidate layers for a spatial `<C-w><C-w>` cross from `from` in
    /// `dir`; the first that is *open* ([`Editor::layer_is_open`]) wins. `Main` is
    /// always open, so any list reaching it resolves there. Closed docks are
    /// skipped — which is also how a press "wraps" past the far edge: e.g. `h` from
    /// the left dock lists `[Right, Main]`, landing on the right dock when it's open
    /// and otherwise falling through.
    ///
    /// The five regions form full-width top/bottom bands around a left|main|right
    /// middle band (cf. `region_geoms`):
    /// ```text
    ///            TOP            (full width)
    ///   LEFT  |  MAIN  | RIGHT  (middle band)
    ///          BOTTOM           (full width)
    /// ```
    /// Each axis is a wrap-around ring of the regions it passes through. The
    /// *vertical* column is `[Top, Main, Bottom]`, so a side dock's up/down moves
    /// (`Left`/`Right`, whose column has no center) only ever reach top/bottom, never
    /// main. The *horizontal* row is `[Left, Main, Right]`, but the full-width
    /// top/bottom docks sit above/below the whole row, so their left/right moves
    /// target the side docks (main is their vertical neighbour, reached with j/k).
    fn cross_dir_candidates(from: Layer, dir: WinDir) -> [Layer; 2] {
        use DockSide::{Bottom, Left, Right, Top};
        use Layer::{Dock, Main};
        use WinDir as W;
        let (l, r, t, b) = (Dock(Left), Dock(Right), Dock(Top), Dock(Bottom));
        match (from, dir) {
            // Main: step to the edge dock on that side, else wrap to the opposite.
            (Main, W::Left) => [l, r],
            (Main, W::Right) => [r, l],
            (Main, W::Up) => [t, b],
            (Main, W::Down) => [b, t],
            // Left dock: right→main, up/down→top/bottom, left wraps to the right dock.
            (Dock(Left), W::Right) => [Main, r],
            (Dock(Left), W::Left) => [r, Main],
            (Dock(Left), W::Up) => [t, b],
            (Dock(Left), W::Down) => [b, t],
            // Right dock: the mirror of the left dock.
            (Dock(Right), W::Left) => [Main, l],
            (Dock(Right), W::Right) => [l, Main],
            (Dock(Right), W::Up) => [t, b],
            (Dock(Right), W::Down) => [b, t],
            // Top dock: down→main, up wraps to bottom, left/right→the side docks.
            (Dock(Top), W::Down) => [Main, b],
            (Dock(Top), W::Up) => [b, Main],
            (Dock(Top), W::Left) => [l, r],
            (Dock(Top), W::Right) => [r, l],
            // Bottom dock: the mirror of the top dock.
            (Dock(Bottom), W::Up) => [Main, t],
            (Dock(Bottom), W::Down) => [t, Main],
            (Dock(Bottom), W::Left) => [l, r],
            (Dock(Bottom), W::Right) => [r, l],
        }
    }

    /// Drive one [`WindowCmd`] on the focused layer's tree. Shared by single
    /// `<C-w>` ([`Editor::execute_window`]) and the cross-layer form
    /// ([`Editor::execute_window_layer`]). When the focused layer is a dock the
    /// per-tab commands differ: closing the dock's last window closes the dock, and
    /// `<C-w>T` (move-to-new-tab) is a no-op.
    fn run_window_cmd(&mut self, cmd: WindowCmd, count: isize) {
        if let Layer::Dock(side) = self.focused_layer {
            match cmd {
                WindowCmd::ToNewTab => return,
                WindowCmd::Close | WindowCmd::Quit => {
                    if self.windows.tiled_count() <= 1 {
                        self.close_dock(side);
                    } else {
                        self.close_window();
                    }
                    return;
                }
                _ => {}
            }
        }
        match cmd {
            WindowCmd::Split(dir) => self.split(dir),
            WindowCmd::FocusDir(dir) => self.focus_dir(dir),
            WindowCmd::SwapDir(dir) => self.swap_window_dir(dir),
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
            // The float lives in the server (it reads the LSP/client diagnostic
            // store core can't touch); just record the request for `run_pending`.
            WindowCmd::ShowDiagnostics => self.pending_diagnostic_float = true,
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
            // The insert-entry keys reposition *every* cursor to its own column
            // (line-end for `A`, first-non-blank for `I`, …) before typing — see
            // `enter_insert_each`. `o`/`O` open a line at every cursor.
            NormalCmd::InsertBefore => self.enter_insert_each(|ed| ed.cursor.col),
            NormalCmd::InsertLineStart => {
                self.enter_insert_each(|ed| ed.first_non_blank(ed.cursor.line))
            }
            NormalCmd::InsertAfter => self.enter_insert_each(|ed| ed.cursor.col + 1),
            NormalCmd::InsertLineEnd => self.enter_insert_each(|ed| ed.line_len()),
            NormalCmd::OpenBelow => self.edit_each_cursor(|ed| ed.open_line(true)),
            NormalCmd::OpenAbove => self.edit_each_cursor(|ed| ed.open_line(false)),
            NormalCmd::DeleteUnder => self.edit_each_cursor(|ed| ed.delete_under_cursor(count)),
            NormalCmd::AddCursor => self.add_cursor(),
            NormalCmd::PlaceCursor => {
                self.record_placement_undo();
                self.place_cursor_here();
            }
            NormalCmd::DeleteBefore => self.edit_each_cursor(|ed| ed.delete_before_cursor(count)),
            NormalCmd::DeleteToEol => self.edit_each_cursor(|ed| ed.delete_to_eol()),
            NormalCmd::ChangeToEol => self.edit_each_cursor(|ed| {
                ed.delete_to_eol();
                ed.mode = Mode::Insert;
                ed.snapshot_taken = true;
            }),
            NormalCmd::SubstituteChar => self.edit_each_cursor(|ed| {
                ed.delete_under_cursor(count);
                ed.mode = Mode::Insert;
                ed.snapshot_taken = true;
            }),
            // With a multi-cursor set, paste gives each cursor its own per-cursor
            // yank (or broadcasts the active register when they don't match) — see
            // `paste_multi`; the single-cursor path is plain `paste`.
            NormalCmd::PasteAfter if self.cursors_active() => self.paste_multi(true, count),
            NormalCmd::PasteBefore if self.cursors_active() => self.paste_multi(false, count),
            NormalCmd::PasteAfter => self.paste(true, count),
            NormalCmd::PasteBefore => self.paste(false, count),
            // In placement mode `u`/`<C-r>` step the cursor *placement* history
            // (drop/undrop), not the text undo tree — placing cursors never edits
            // the document, so there is nothing textual to undo while still placing.
            NormalCmd::Undo if self.mode == Mode::MultiCursor => self.undo_placement(),
            NormalCmd::Redo if self.mode == Mode::MultiCursor => self.redo_placement(),
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
            NormalCmd::Join => self.edit_each_cursor(|ed| ed.join_lines(count.max(2))),
            NormalCmd::ToggleCase => self.edit_each_cursor(|ed| ed.toggle_case(count)),
            // `R` enters Replace mode: snapshot for undo, then overtype until
            // `<Esc>` (the insert handler honors `Mode::Replace`).
            NormalCmd::EnterReplace => {
                if !self.modifiable() {
                    self.refuse_edit();
                    return;
                }
                self.push_undo();
                self.snapshot_taken = true;
                self.mode = Mode::Replace;
            }
            // From normal mode entering visual anchors the selection; from visual
            // mode `v`/`V` only switch the selection's shape, leaving the anchor.
            NormalCmd::EnterVisual => {
                if !self.mode.is_visual() {
                    self.visual_anchor = self.cursor;
                    // With a multi-cursor set placed, each secondary anchors its own
                    // selection where it sits — visual then extends/operates on all.
                    self.begin_visual_anchors();
                }
                self.mode = Mode::Visual;
            }
            // `gh` / `gH`: start Select mode (the keyboard entry to the mode). From
            // Normal (or on top of an existing selection) anchor at the cursor for a
            // 1-wide selection, like `v`/`V` but Select; `<Esc>` defaults to Normal
            // (vim's `v_CTRL-G`). `linewise` picks charwise (`gh`) vs linewise (`gH`).
            NormalCmd::EnterSelect(linewise) => {
                if !self.mode.is_visual() && self.mode != Mode::Select {
                    self.visual_anchor = self.cursor;
                    self.begin_visual_anchors();
                }
                self.select_linewise = linewise;
                self.select_escape_insert = false;
                self.mode = Mode::Select;
            }
            // `<C-g>`: toggle Visual → Select, keeping the current selection and its
            // charwise/linewise shape (the Select → Visual half lives in
            // `handle_select`). `<Esc>` from the toggled-in Select defaults to Normal.
            NormalCmd::ToggleVisualSelect => {
                self.select_linewise = self.mode == Mode::VisualLine;
                self.select_escape_insert = false;
                self.mode = Mode::Select;
            }
            NormalCmd::VisualSwapEnds => self.visual_swap_ends(),
            NormalCmd::ReselectVisual => self.reselect_visual(),
            NormalCmd::EnterVisualLine => {
                if !self.mode.is_visual() {
                    self.visual_anchor = self.cursor;
                    self.begin_visual_anchors();
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
            NormalCmd::ViewScroll {
                place,
                first_nonblank,
                count,
            } => self.view_reposition(place, first_nonblank, count),
            NormalCmd::Fold(fc) => self.execute_fold(fc, count),
            NormalCmd::JumpBack => self.jump_back(count),
            NormalCmd::JumpForward => self.jump_forward(count),
            NormalCmd::AltBuffer => self.goto_alternate(),
            NormalCmd::TabNext(n) => self.goto_tab_next(n),
            NormalCmd::TabPrev(n) => self.goto_tab_prev(n),
            NormalCmd::ChangeOlder => self.change_older(count),
            NormalCmd::ChangeNewer => self.change_newer(count),
            NormalCmd::DotRepeat => self.repeat_change(),
        }
        self.reset_pending();
    }

    /// Replay the last buffer-changing command (`.`). Re-feeds the recorded raw
    /// key stream through [`Editor::input`] under the `replaying_change` guard, so
    /// the entire grammar (counts, operators, registers, text objects, inserted
    /// text) re-parses and re-executes exactly as typed. No-op when nothing has
    /// been recorded yet (vim beeps; bemtvi has no bell).
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
        // MULTICURSOR `{count}cc`: drop a cursor on each of `count` lines from the
        // primary down (the linewise place; `cc` places one at the current line).
        if self.mode == Mode::MultiCursor && op == 'c' {
            let n = self.effective_count().max(1);
            self.reset_pending();
            // One undo step covers the whole `{count}cc` linewise drop.
            self.record_placement_undo();
            for i in 0..n {
                self.ensure_cursor_here();
                if i + 1 < n {
                    self.move_vertical(1, false);
                }
            }
            return;
        }
        // Multi-cursor (Normal mode only): the doubled operator runs at every
        // cursor, as one undo group.
        if self.has_secondary_cursors() && self.mode == Mode::Normal {
            self.edit_each_cursor(|ed| ed.apply_doubled_operator_once(op));
            self.reset_pending();
            return;
        }
        let count = self.effective_count();
        let last = self.cursor.line + count - 1;
        let target = self.buffer().line_start(last.min(self.last_line()));
        // axis is unused for the operator path, but the field is required.
        let m = MotionResult::linewise(target, MoveAxis::LineAnchor);
        self.pending.operator = None;
        self.apply_operator(op, m);
        self.reset_pending();
    }

    /// Resolve motion `m` at the *current* cursor and apply it — as the pending
    /// operator's range when one is set, else as plain movement. Per-cursor: reads
    /// pending (operator/count) but does **not** reset it, so [`for_each_cursor`]
    /// can replay it at every cursor before the single reset. A motion that
    /// doesn't resolve (a find/`;`/`,` miss at this cursor) is a no-op here.
    ///
    /// [`for_each_cursor`]: Editor::for_each_cursor
    fn apply_motion_once(&mut self, m: Motion) {
        let Some(mr) = self.resolve_motion(m) else {
            return;
        };
        match self.pending.operator {
            Some(op) => self.apply_operator(op, mr),
            None => self.apply_movement(mr),
        }
    }

    /// Apply a doubled operator (`dd`/`yy`/`cc`) at the current cursor over its
    /// `count` lines. Per-cursor (reads pending, never resets it), so
    /// [`edit_each_cursor`] can replay it at every cursor.
    ///
    /// [`edit_each_cursor`]: Editor::edit_each_cursor
    fn apply_doubled_operator_once(&mut self, op: char) {
        let count = self.effective_count();
        let last = self.cursor.line + count - 1;
        let target = self.buffer().line_start(last.min(self.last_line()));
        let m = MotionResult::linewise(target, MoveAxis::LineAnchor);
        self.apply_operator(op, m);
    }

    /// Apply the pending operator over the text object at the current cursor
    /// (`diw`/`ci"`/…). Per-cursor (reads pending, never resets it); a no-op when
    /// no object is found at this cursor.
    fn apply_text_object_once(&mut self, ia: char, key: char, count: usize) {
        let Some((lo, hi, linewise)) = self.resolve_text_object(ia, key, count) else {
            return;
        };
        let Some(op) = self.pending.operator else {
            return;
        };
        if linewise {
            let first_line = self.buffer().byte_to_line(lo);
            self.apply_operator_to_range(op, lo, hi, true, first_line);
        } else {
            self.apply_operator_to_range(op, lo, hi, false, 0);
        }
    }

    /// Apply a resolved motion: as an operator's range if one is pending,
    /// otherwise as plain cursor movement. An off-screen jump animates its scroll
    /// via the viewport snapshot [`input`](Self::input) takes around every command.
    fn apply_resolved_motion(&mut self, m: MotionResult) {
        if let Some(op) = self.pending.operator.take() {
            self.apply_operator(op, m);
        } else {
            self.apply_movement(m);
        }
        // A completed motion clears the whole pending command. (The old movement
        // path only cleared `count`, but operator/g-prefix/find were already
        // cleared before it ran, so a full reset is equivalent — and now correct,
        // since `parse_step` leaves the stage set until the command finishes.)
        self.reset_pending();
    }
}
