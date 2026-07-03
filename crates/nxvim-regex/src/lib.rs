//! Vim regular expressions for nxvim — the real thing.
//!
//! This crate vendors neovim's `regexp.c` (both the backtracking engine and
//! the NFA engine, with vim's automatic engine selection) plus the minimal
//! set of supporting code (`mbyte`, `charset`, `garray`, utf8proc), compiled
//! as C and wrapped in a safe API. Matching behavior — magic levels, `\zs`,
//! lookaround, backreferences, multis, character classes, case folding,
//! multi-line patterns — is vim's, byte for byte.
//!
//! # Model
//!
//! * [`VimRegex`] is a compiled pattern (`vim_regcomp`).
//! * [`VimRegex::exec_line`] matches against a single line of text (no
//!   line breaks; `\n` in the pattern will not match anything).
//! * [`VimBuffer`] holds buffer lines plus the editor state the context
//!   assertions read (cursor for `\%#`, Visual range for `\%V`, marks for
//!   `\%'m`, `'iskeyword'` for `\k`), and [`VimBuffer::exec`] runs a
//!   multi-line match (`vim_regexec_multi`), where `\n` in the pattern
//!   crosses line boundaries.
//!
//! # Concurrency
//!
//! The engine keeps its matching state in C globals (upstream design), so
//! every call is serialized behind one process-wide mutex. Compiled programs
//! are `Send` but matching is effectively single-threaded.
//!
//! # Known divergences from vim
//!
//! * `\=` expression substitution and `submatch()` are not provided at this
//!   layer (the engine fails loud if asked); the host implements them.
//! * Virtual-column assertions (`\%v`) use tabstop + Unicode width rather
//!   than vim's full charsize machinery ('vartabstop', `<xx>` display of
//!   unprintable bytes, 'list' mode etc. are not modeled).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

// ---------------------------------------------------------------------------
// FFI surface (see csrc/nvim/regexp.h and csrc/shim/nxre_shim.h)

type LinenrT = i32;
type ColnrT = i32;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LposT {
    lnum: LinenrT,
    col: ColnrT,
}

const NSUBEXP: usize = 10;

#[repr(C)]
struct RegmatchT {
    regprog: *mut c_void,
    startp: [*mut c_char; NSUBEXP],
    endp: [*mut c_char; NSUBEXP],
    rm_matchcol: ColnrT,
    rm_ic: bool,
}

#[repr(C)]
struct RegmmatchT {
    regprog: *mut c_void,
    startpos: [LposT; NSUBEXP],
    endpos: [LposT; NSUBEXP],
    rmm_matchcol: ColnrT,
    rmm_ic: c_int,
    rmm_maxcol: ColnrT,
}

const RE_MAGIC: c_int = 1;
const RE_STRING: c_int = 2;

unsafe extern "C" {
    fn vim_regcomp(expr: *const c_char, re_flags: c_int) -> *mut c_void;
    fn vim_regfree(prog: *mut c_void);
    fn vim_regexec(rmp: *mut RegmatchT, line: *const c_char, col: ColnrT) -> bool;
    fn re_multiline(prog: *const c_void) -> c_int;
    #[allow(clippy::too_many_arguments)]
    fn vim_regexec_multi(
        rmp: *mut RegmmatchT,
        win: *mut c_void,
        buf: *mut c_void,
        lnum: LinenrT,
        col: ColnrT,
        tm: *const u64,
        timed_out: *mut c_int,
    ) -> c_int;

    fn nxre_buf_new(
        get_line: extern "C" fn(*mut c_void, LinenrT, *mut ColnrT) -> *const c_char,
        userdata: *mut c_void,
        line_count: LinenrT,
    ) -> *mut c_void;
    fn nxre_buf_free(buf: *mut c_void);
    fn nxre_buf_set_iskeyword(buf: *mut c_void, iskeyword: *const c_char) -> bool;
    fn nxre_buf_set_tabstop(buf: *mut c_void, tabstop: i64);
    fn nxre_win_new(buf: *mut c_void) -> *mut c_void;
    fn nxre_win_free(win: *mut c_void);
    fn nxre_set_current(buf: *mut c_void, win: *mut c_void);
    fn nxre_set_mark_provider(
        lookup: Option<extern "C" fn(*mut c_void, c_int, *mut LinenrT, *mut ColnrT) -> bool>,
        userdata: *mut c_void,
    );
    fn nxre_set_interrupt(value: bool);
    fn nxre_take_last_error() -> *const c_char;
    fn nxre_set_regexpengine(engine: i64);
    fn nxre_profile_setlimit(ms: i64) -> u64;
    fn nxre_win_set_cursor(win: *mut c_void, lnum: LinenrT, col: ColnrT);
    fn nxre_buf_set_visual(
        buf: *mut c_void,
        start_lnum: LinenrT,
        start_col: ColnrT,
        end_lnum: LinenrT,
        end_col: ColnrT,
        mode: c_int,
    );
}

// ---------------------------------------------------------------------------
// the engine lock

/// All engine calls go through this lock: the vendored C keeps match state in
/// globals (`rex` in regexp.c), and the shim's curbuf/curwin/mark-provider
/// registers are globals too.
fn engine() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The `buf` pointer most recently made current via `nxre_set_current`, or 0
/// when unknown (nothing set yet, the current buffer was freed, or its
/// options changed). Only read/written while holding the engine lock, which
/// provides the ordering — `Relaxed` suffices.
static CURRENT_BUF: AtomicUsize = AtomicUsize::new(0);

/// Makes `buf`/`win` the engine's current context, skipping the C call when
/// `buf` is already current. `nxre_set_current` reparses the option strings
/// and rebuilds the 256-entry character tables every call, which is pure
/// waste on the per-line `exec_line`/`exec` hot paths where the context
/// rarely changes. The cache is cleared whenever it could go stale: a
/// `VimBuffer` drop (the C side NULLs `curbuf`, and a later allocation could
/// reuse the address) and `set_iskeyword` (chartabs must be rebuilt).
fn make_current(_guard: &MutexGuard<'_, ()>, buf: *mut c_void, win: *mut c_void) {
    if CURRENT_BUF.load(Ordering::Relaxed) != buf as usize {
        unsafe { nxre_set_current(buf, win) };
        CURRENT_BUF.store(buf as usize, Ordering::Relaxed);
    }
}

/// Default match context: `curbuf`/`curwin` must never be NULL (the engine
/// reads them for `\k`-style classes even in single-line matching), and a
/// dropped `VimBuffer` clears them. Every entry point that isn't bound to a
/// specific `VimBuffer` pins this default context (vim option defaults,
/// empty text) before calling into the engine. Created once, never freed.
fn set_default_context(guard: &MutexGuard<'_, ()>) {
    static CTX: OnceLock<(usize, usize)> = OnceLock::new();
    let &(buf, win) = CTX.get_or_init(|| {
        extern "C" fn empty_line(
            _ud: *mut c_void,
            _lnum: LinenrT,
            len: *mut ColnrT,
        ) -> *const c_char {
            if !len.is_null() {
                unsafe { *len = 0 };
            }
            c"".as_ptr()
        }
        unsafe {
            let buf = nxre_buf_new(empty_line, std::ptr::null_mut(), 1);
            let win = nxre_win_new(buf);
            (buf as usize, win as usize)
        }
    });
    make_current(guard, buf as *mut c_void, win as *mut c_void);
}

/// Takes the engine's queued error message, if any, clearing it. Returns
/// `None` when no error is pending (e.g. a plain non-match).
fn pending_error() -> Option<VimRegexError> {
    unsafe {
        let p = nxre_take_last_error();
        if p.is_null() {
            None
        } else {
            Some(VimRegexError(
                CStr::from_ptr(p).to_string_lossy().into_owned(),
            ))
        }
    }
}

fn take_error(fallback: &str) -> VimRegexError {
    pending_error().unwrap_or_else(|| VimRegexError(fallback.to_string()))
}

// ---------------------------------------------------------------------------
// public types

/// An error reported by the engine (vim `E…` numbers preserved) or by this
/// wrapper's input validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VimRegexError(pub String);

impl fmt::Display for VimRegexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for VimRegexError {}

/// Which regexp engine compiles the pattern — vim's `'regexpengine'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Engine {
    /// Automatic selection with fallback (vim's default).
    #[default]
    Auto,
    /// The old backtracking engine.
    Backtracking,
    /// The NFA engine.
    Nfa,
}

/// How the pattern treats `\n`: buffer patterns match line breaks with `\n`,
/// string patterns treat `\n` as a literal newline character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PatternKind {
    /// Buffer semantics (`:s`, search): `\n` matches a line break.
    #[default]
    Buffer,
    /// String semantics (`matchstr()` etc. on strings).
    String,
}

/// A single-line match: byte offsets into the searched line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineMatch {
    /// Whole-match byte range (submatch 0).
    pub start: usize,
    pub end: usize,
    /// Submatches `\1`..`\9` as byte ranges (index 1..=9; index 0 mirrors
    /// start/end).
    pub submatches: [Option<(usize, usize)>; NSUBEXP],
}

/// A position in a [`VimBuffer`]: 1-based line, 0-based byte column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufPos {
    pub lnum: u32,
    pub col: u32,
}

/// A multi-line match in a [`VimBuffer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufMatch {
    /// Whole-match range: start inclusive, end exclusive (end.col is the byte
    /// just past the match on line end.lnum).
    pub start: BufPos,
    pub end: BufPos,
    /// Submatches `\1`..`\9` (index 1..=9; index 0 mirrors start/end).
    pub submatches: [Option<(BufPos, BufPos)>; NSUBEXP],
}

// ---------------------------------------------------------------------------
// VimRegex

/// A compiled vim pattern (`vim_regcomp`).
pub struct VimRegex {
    /// The compiled program. Held in a `Cell` because the engine may *replace*
    /// it mid-exec: with automatic engine selection, an NFA program that hits
    /// `NFA_TOO_EXPENSIVE` is freed and recompiled with the backtracking engine
    /// inside `vim_regexec`/`vim_regexec_multi`, which write the new pointer
    /// back into the `regmatch_T`/`regmmatch_T`. We must mirror that write here
    /// or the old (now-freed) pointer becomes a use-after-free / double-free.
    /// All access happens under the engine lock, which serializes it.
    prog: Cell<*mut c_void>,
    pattern: String,
}

// The compiled program is owned exclusively and only used under the engine
// lock; the raw pointer does not alias any thread-local state. The `Cell` makes
// `VimRegex` `!Sync` (so `&VimRegex` cannot cross threads), which is correct:
// the engine lock only serializes owned access, it does not make concurrent
// shared mutation of the same program safe.
unsafe impl Send for VimRegex {}

impl VimRegex {
    /// Compiles `pattern` with `'magic'` buffer semantics and automatic
    /// engine selection — the common case.
    pub fn compile(pattern: &str) -> Result<Self, VimRegexError> {
        Self::compile_with(pattern, PatternKind::Buffer, Engine::Auto)
    }

    /// Compiles with explicit `\n` semantics and engine choice.
    pub fn compile_with(
        pattern: &str,
        kind: PatternKind,
        engine_sel: Engine,
    ) -> Result<Self, VimRegexError> {
        let cpat = CString::new(pattern)
            .map_err(|_| VimRegexError("pattern contains a NUL byte".into()))?;
        let flags = RE_MAGIC
            + match kind {
                PatternKind::Buffer => 0,
                PatternKind::String => RE_STRING,
            };
        let guard = engine();
        set_default_context(&guard);
        let prog = unsafe {
            nxre_set_regexpengine(match engine_sel {
                Engine::Auto => 0,
                Engine::Backtracking => 1,
                Engine::Nfa => 2,
            });
            let prog = vim_regcomp(cpat.as_ptr(), flags);
            nxre_set_regexpengine(0);
            prog
        };
        if prog.is_null() {
            return Err(take_error("invalid vim pattern"));
        }
        // a successful compile can still have queued a message (e.g. engine
        // fallback notices route through emsg in some paths) — clear it so it
        // isn't misattributed to the next failing call
        unsafe { nxre_take_last_error() };
        Ok(VimRegex {
            prog: Cell::new(prog),
            pattern: pattern.to_string(),
        })
    }

    /// The source pattern.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// The compiled program pointer, or a loud error if a prior exec's NFA→BT
    /// fallback recompile failed and left it NULL (the engine would deref it).
    /// Call under the engine lock.
    fn checked_prog(&self) -> Result<*mut c_void, VimRegexError> {
        let prog = self.prog.get();
        if prog.is_null() {
            return Err(VimRegexError(
                "vim regex program is invalid (engine fallback recompile previously failed)".into(),
            ));
        }
        Ok(prog)
    }

    /// True if the compiled pattern can match across line boundaries
    /// (`\n`, `\_x` classes) — useful for choosing a search strategy.
    pub fn is_multiline(&self) -> bool {
        let _guard = engine();
        // The program can be NULL if a prior exec's NFA→BT fallback recompile
        // failed (`vim_regexec` writes NULL back; see `checked_prog`).
        // `re_multiline` dereferences it unconditionally, so answer "not
        // multiline" instead of crashing — the next exec fails loud.
        let prog = self.prog.get();
        !prog.is_null() && unsafe { re_multiline(prog) != 0 }
    }

    /// Matches against a single line (no line breaks), starting at byte
    /// column `col`. Returns the first match at-or-after `col`, vim
    /// semantics (leftmost, with vim's greedy/lazy multis).
    pub fn exec_line(
        &self,
        line: &str,
        col: usize,
        ignore_case: bool,
    ) -> Result<Option<LineMatch>, VimRegexError> {
        // The engine needs a NUL-terminated subject. `exec_line` is the
        // hottest path — search/substitute call it once per line (and repeatedly
        // within a line as the substitute loop advances over each match) — so a
        // fresh `CString` per call would be one malloc/free per call over a
        // whole buffer. Reuse a thread-local scratch buffer instead: all
        // matching is serialized behind the engine lock (held here) and
        // `vim_regexec` never re-enters this path, so the buffer stays untouched
        // for the lifetime of the subject pointer we hand the engine.
        if line.as_bytes().contains(&0) {
            return Err(VimRegexError("line contains a NUL byte".into()));
        }
        // The engine starts scanning at `subject + col` with no bounds check of
        // its own; a `col` past the end of the line would read past the NUL
        // terminator (OOB). `col == line.len()` is valid (it points at the NUL).
        if col > line.len() {
            return Err(VimRegexError(format!(
                "column {col} past end of line (length {})",
                line.len()
            )));
        }
        let col = ColnrT::try_from(col).map_err(|_| VimRegexError("column out of range".into()))?;

        let guard = engine();
        set_default_context(&guard);
        let prog = self.checked_prog()?;

        thread_local! {
            static SUBJECT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
        }
        SUBJECT.with(|cell| {
            let mut subject = cell.borrow_mut();
            subject.clear();
            subject.reserve(line.len() + 1);
            subject.extend_from_slice(line.as_bytes());
            subject.push(0);
            let base = subject.as_ptr() as *const c_char;

            let mut rm = RegmatchT {
                regprog: prog,
                startp: [std::ptr::null_mut(); NSUBEXP],
                endp: [std::ptr::null_mut(); NSUBEXP],
                rm_matchcol: 0,
                rm_ic: ignore_case,
            };
            let matched = unsafe { vim_regexec(&mut rm, base, col) };
            // The engine may have freed our program and recompiled it (NFA→BT
            // fallback); adopt the pointer it wrote back so we don't keep a
            // dangling one. It can also be left NULL if recompilation failed.
            self.prog.set(rm.regprog);
            if !matched {
                if let Some(err) = pending_error() {
                    return Err(err);
                }
                return Ok(None);
            }
            let off = |p: *mut c_char| -> Option<usize> {
                if p.is_null() {
                    None
                } else {
                    Some(unsafe { p.offset_from(base) } as usize)
                }
            };
            let mut submatches = [None; NSUBEXP];
            for (sub, (sp, ep)) in submatches
                .iter_mut()
                .zip(rm.startp.iter().zip(rm.endp.iter()))
            {
                if let (Some(s), Some(e)) = (off(*sp), off(*ep)) {
                    *sub = Some((s, e));
                }
            }
            let (start, end) = submatches[0]
                .ok_or_else(|| VimRegexError("engine reported a match without bounds".into()))?;
            Ok(Some(LineMatch {
                start,
                end,
                submatches,
            }))
        })
    }
}

impl Drop for VimRegex {
    fn drop(&mut self) {
        let _guard = engine();
        unsafe { vim_regfree(self.prog.get()) };
    }
}

impl fmt::Debug for VimRegex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VimRegex")
            .field("pattern", &self.pattern)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// VimBuffer

struct BufferData {
    /// Materialized lines, 1-based (index 0 unused). CString heap storage is
    /// stable, satisfying the engine's requirement that line pointers stay
    /// valid for a whole exec call.
    lines: Vec<CString>,
    lens: Vec<ColnrT>,
    marks: HashMap<u8, (LinenrT, ColnrT)>,
}

extern "C" fn buffer_get_line(ud: *mut c_void, lnum: LinenrT, len: *mut ColnrT) -> *const c_char {
    let data = unsafe { &*(ud as *const BufferData) };
    match data.lines.get(lnum as usize) {
        Some(line) => {
            if !len.is_null() {
                unsafe { *len = data.lens[lnum as usize] };
            }
            line.as_ptr()
        }
        // Out-of-range request: the shim aborts on NULL, which is the
        // fail-loud path for an engine/host disagreement on line_count.
        None => std::ptr::null(),
    }
}

extern "C" fn buffer_mark_lookup(
    ud: *mut c_void,
    name: c_int,
    lnum: *mut LinenrT,
    col: *mut ColnrT,
) -> bool {
    let data = unsafe { &*(ud as *const BufferData) };
    match u8::try_from(name).ok().and_then(|n| data.marks.get(&n)) {
        Some(&(l, c)) => {
            unsafe {
                *lnum = l;
                *col = c;
            }
            true
        }
        None => false,
    }
}

/// Buffer text plus the editor state vim's context assertions read.
///
/// Positions are 1-based lines and 0-based byte columns, matching vim.
pub struct VimBuffer {
    data: Box<BufferData>,
    buf: *mut c_void,
    win: *mut c_void,
}

impl VimBuffer {
    /// Builds a buffer from lines (without trailing newlines). Lines must not
    /// contain NUL bytes.
    pub fn from_lines<S: AsRef<str>>(lines: &[S]) -> Result<Self, VimRegexError> {
        let mut stored = Vec::with_capacity(lines.len() + 1);
        let mut lens = Vec::with_capacity(lines.len() + 1);
        stored.push(CString::default()); // index 0 unused (lnum is 1-based)
        lens.push(0);
        for (i, line) in lines.iter().enumerate() {
            let line = line.as_ref();
            lens.push(
                ColnrT::try_from(line.len())
                    .map_err(|_| VimRegexError(format!("line {} too long", i + 1)))?,
            );
            stored.push(
                CString::new(line)
                    .map_err(|_| VimRegexError(format!("line {} contains a NUL byte", i + 1)))?,
            );
        }
        let line_count = LinenrT::try_from(lines.len().max(1))
            .map_err(|_| VimRegexError("too many lines".into()))?;
        if lines.is_empty() {
            // vim buffers always have at least one (empty) line
            stored.push(CString::default());
            lens.push(0);
        }

        let mut data = Box::new(BufferData {
            lines: stored,
            lens,
            marks: HashMap::new(),
        });
        let guard = engine();
        set_default_context(&guard);
        let ud = &mut *data as *mut BufferData as *mut c_void;
        let (buf, win) = unsafe {
            let buf = nxre_buf_new(buffer_get_line, ud, line_count);
            let win = nxre_win_new(buf);
            (buf, win)
        };
        Ok(VimBuffer { data, buf, win })
    }

    /// Number of lines.
    pub fn line_count(&self) -> u32 {
        (self.data.lines.len() - 1) as u32
    }

    /// Sets the buffer's `'iskeyword'` (affects `\k`, `\<`, `\>` …).
    pub fn set_iskeyword(&mut self, iskeyword: &str) -> Result<(), VimRegexError> {
        let c = CString::new(iskeyword)
            .map_err(|_| VimRegexError("iskeyword contains a NUL byte".into()))?;
        let _guard = engine();
        // Invalidate the current-context cache so the next exec re-runs
        // `nxre_set_current` and rebuilds the character tables against the
        // new value (`nxre_buf_set_iskeyword` rebuilds the buffer-local
        // table itself, but the cache must not assume anything stayed put).
        CURRENT_BUF.store(0, Ordering::Relaxed);
        if unsafe { nxre_buf_set_iskeyword(self.buf, c.as_ptr()) } {
            Ok(())
        } else {
            Err(take_error("invalid 'iskeyword' value"))
        }
    }

    /// Sets `'tabstop'` (affects the `\%v` virtual-column assertions).
    pub fn set_tabstop(&mut self, tabstop: u16) {
        let _guard = engine();
        unsafe { nxre_buf_set_tabstop(self.buf, i64::from(tabstop)) };
    }

    /// Places the cursor (the `\%#` assertion).
    pub fn set_cursor(&mut self, pos: BufPos) {
        let _guard = engine();
        unsafe { nxre_win_set_cursor(self.win, pos.lnum as LinenrT, pos.col as ColnrT) };
    }

    /// Sets the last Visual selection (the `\%V` assertion). `mode` is the
    /// vim mode character: `'v'`, `'V'`, or `'\x16'` (blockwise).
    pub fn set_visual(&mut self, start: BufPos, end: BufPos, mode: char) {
        let _guard = engine();
        unsafe {
            nxre_buf_set_visual(
                self.buf,
                start.lnum as LinenrT,
                start.col as ColnrT,
                end.lnum as LinenrT,
                end.col as ColnrT,
                mode as c_int,
            );
        }
    }

    /// Sets a mark (the `\%'m` assertions). `name` is the mark character.
    pub fn set_mark(&mut self, name: char, pos: BufPos) {
        self.data
            .marks
            .insert(name as u8, (pos.lnum as LinenrT, pos.col as ColnrT));
    }

    /// Runs a multi-line match at line `lnum` (1-based), starting at byte
    /// column `col`. The match must start in that line (vim semantics —
    /// callers iterate lines to search), but may extend across lines.
    ///
    /// `timeout_ms` bounds NFA matching time (vim's `'redrawtime'`-style
    /// limit); `None` means unbounded.
    pub fn exec(
        &self,
        re: &VimRegex,
        lnum: u32,
        col: u32,
        ignore_case: bool,
        timeout_ms: Option<u64>,
    ) -> Result<Option<BufMatch>, VimRegexError> {
        if lnum == 0 || lnum > self.line_count() {
            return Err(VimRegexError(format!("lnum {lnum} out of range")));
        }
        // The engine starts scanning at `line + col` with no bounds check of its
        // own. Reject a `col` past the end of the starting line (OOB read past the
        // NUL) — and, because `col` is later narrowed `u32 as ColnrT(i32)`, a
        // value that would wrap to a negative column (OOB read before the buffer).
        // `col == line length` is valid (it points at the NUL). `lens[lnum]` is
        // the line's byte length and is always >= 0 (it came from `usize` lengths).
        let line_len = self.data.lens[lnum as usize] as u32;
        if col > line_len {
            return Err(VimRegexError(format!(
                "col {col} past end of line {lnum} (length {line_len})"
            )));
        }
        let guard = engine();
        let prog = re.checked_prog()?;

        let mut rmm = RegmmatchT {
            regprog: prog,
            startpos: [LposT::default(); NSUBEXP],
            endpos: [LposT::default(); NSUBEXP],
            rmm_matchcol: 0,
            rmm_ic: c_int::from(ignore_case),
            rmm_maxcol: 0,
        };
        let deadline = timeout_ms.map(|ms| {
            // The shim computes `now_ns + ms*1_000_000` in u64; a huge `ms`
            // would overflow that multiply/add and wrap to a *tiny* deadline,
            // causing spurious immediate timeouts. Clamp to a bound that keeps
            // the nanosecond deadline well within u64 (~years of headroom),
            // which for any real `'redrawtime'` value is effectively unbounded.
            const MAX_MS: u64 = u64::MAX / 1_000_000 / 2;
            unsafe { nxre_profile_setlimit(ms.min(MAX_MS) as i64) }
        });
        let mut timed_out: c_int = 0;
        let ud = &*self.data as *const BufferData as *mut c_void;
        make_current(&guard, self.buf, self.win);
        let nlines = unsafe {
            nxre_set_mark_provider(Some(buffer_mark_lookup), ud);
            let r = vim_regexec_multi(
                &mut rmm,
                self.win,
                self.buf,
                lnum as LinenrT,
                col as ColnrT,
                deadline
                    .as_ref()
                    .map_or(std::ptr::null(), |d| d as *const u64),
                &mut timed_out,
            );
            nxre_set_mark_provider(None, std::ptr::null_mut());
            r
        };
        // The engine may have freed `re.prog` and recompiled it (NFA→BT
        // fallback), writing the replacement into `rmm.regprog`; adopt it so
        // the `VimRegex` doesn't keep a dangling (freed) pointer.
        re.prog.set(rmm.regprog);
        if timed_out != 0 {
            return Err(VimRegexError("vim regex match timed out".into()));
        }
        if nlines == 0 {
            if let Some(err) = pending_error() {
                return Err(err);
            }
            return Ok(None);
        }

        // positions come back relative to `lnum`
        let abs = |p: LposT| BufPos {
            lnum: (p.lnum + lnum as LinenrT) as u32,
            col: p.col as u32,
        };
        let mut submatches = [None; NSUBEXP];
        for (sub, (sp, ep)) in submatches
            .iter_mut()
            .zip(rmm.startpos.iter().zip(rmm.endpos.iter()))
        {
            if sp.lnum >= 0 && ep.lnum >= 0 {
                *sub = Some((abs(*sp), abs(*ep)));
            }
        }
        let (start, end) = submatches[0]
            .ok_or_else(|| VimRegexError("engine reported a match without bounds".into()))?;
        Ok(Some(BufMatch {
            start,
            end,
            submatches,
        }))
    }
}

impl Drop for VimBuffer {
    fn drop(&mut self) {
        let _guard = engine();
        // The C side NULLs `curbuf`/`curwin` when the current ones are freed,
        // and a later allocation may reuse this address — clear the cache so
        // the next exec re-pins its context instead of matching a stale
        // pointer against a freed (or recycled) one. (Defense in depth: today
        // every path that could see a recycled address re-pins anyway,
        // because `from_lines` pins the default context first.)
        if CURRENT_BUF.load(Ordering::Relaxed) == self.buf as usize {
            CURRENT_BUF.store(0, Ordering::Relaxed);
        }
        unsafe {
            nxre_win_free(self.win);
            nxre_buf_free(self.buf);
        }
    }
}

// VimBuffer owns its C objects exclusively; they are only touched under the
// engine lock. (No Sync: exec takes &self but mutates engine globals — the
// lock serializes that, and &self across threads is fine.)
unsafe impl Send for VimBuffer {}
unsafe impl Sync for VimBuffer {}

/// Interrupts any in-progress match (vim's `got_int`), making it return "no
/// match" promptly. Callable from any thread.
pub fn interrupt() {
    // deliberately NOT taking the engine lock: the whole point is to signal a
    // match currently holding it
    unsafe { nxre_set_interrupt(true) };
}

/// Clears the interrupt flag (call before starting fresh work after an
/// [`interrupt`]).
pub fn clear_interrupt() {
    unsafe { nxre_set_interrupt(false) };
}
