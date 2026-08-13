//! The bounds that keep a *pattern* from taking the editor down with it.
//!
//! Every pattern here is one a user, a config, or a plugin can type. The engine
//! is the vendored vim `regexp.c`, and its native failure mode for a pathological
//! pattern is not "slow" but "gone": an allocation the allocator refuses (which
//! aborts the process), a parse recursion deep enough to overflow the thread's
//! stack (likewise), or a match that simply never returns. None of those are
//! recoverable, and `bemtvi_regex::interrupt()` — the engine's `got_int` escape —
//! has no caller in the editor, so a runaway match cannot be cancelled from
//! outside either. The compile-time caps and the per-call deadline are what turn
//! each of those into an ordinary `Err`.
//!
//! Each test names the crash it replaces; reverting the corresponding bound turns
//! the test into an abort of this binary rather than a failed assertion.

use std::time::{Duration, Instant};

use bemtvi_regex::VimRegex;

/// `a\{2147483647}` — an exact quantifier at the parser's `MAX_LIMIT`. The NFA
/// engine expands `\{m,n}` into `maxval` copies of the atom, so this asks it for
/// ~2.1 billion states; `nstate` is a plain `int`, it overflows negative, and the
/// `sizeof(nfa_state_T) * (size_t)nstate` for the program wraps into an enormous
/// allocation — `out of memory allocating 18446743987810205872 bytes`, i.e. the
/// editor dies on `:s/a\{2147483647}/x/`.
///
/// With the expansion capped, the NFA engine declines the pattern and automatic
/// engine selection falls back to the backtracking engine, whose program is
/// proportional to the pattern text — so the pattern still *works*, it just stops
/// being a way to kill the process.
#[test]
fn an_exact_quantifier_at_the_parser_limit_does_not_allocate_the_expansion() {
    let re = VimRegex::compile("a\\{2147483647}").expect("compile");
    // It compiled; it must also still behave. Four `a`s are not 2147483647 of
    // them, so this is a non-match — the point is that we get an answer at all.
    assert_eq!(re.exec_line("aaaa", 0, false).unwrap(), None);
}

/// The same bound must hold for the very-magic spelling, which reaches the same
/// expansion by a different parse path.
#[test]
fn the_very_magic_spelling_of_a_huge_quantifier_is_bounded_too() {
    let re = VimRegex::compile("\\va{2147483647}").expect("compile");
    assert_eq!(re.exec_line("aaaa", 0, false).unwrap(), None);
}

/// A quantifier under the cap still expands, i.e. the bound didn't cost the
/// feature.
#[test]
fn an_ordinary_exact_quantifier_still_matches() {
    let re = VimRegex::compile("a\\{5}").expect("compile");
    assert_eq!(re.exec_line("aaaaaaa", 0, false).unwrap().unwrap().start, 0);
    assert_eq!(re.exec_line("aaaaaaa", 0, false).unwrap().unwrap().end, 5);
    assert_eq!(
        VimRegex::compile("a\\{5}")
            .unwrap()
            .exec_line("aaa", 0, false)
            .unwrap(),
        None
    );
}

/// `\%(` is the one group that nests without bound: `\(` and `\z(` are counted
/// against `NSUBEXP` and stop at E51/E50, but `\%(` is deliberately uncounted, so
/// a pattern nesting thousands of them recurses the parser
/// (`reg → regbranch → regconcat → regpiece → regatom → reg`) until the thread's
/// stack is gone — `fatal runtime error: stack overflow`, which is an abort, not
/// an error the editor can report.
///
/// The depth cap turns it into a compile error. 5000 is deep enough to have
/// reliably overflowed an 8 MiB stack.
#[test]
fn deeply_nested_non_capturing_groups_are_rejected_not_a_stack_overflow() {
    let depth = 5000;
    let pat = format!("{}x{}", "\\%(".repeat(depth), "\\)".repeat(depth));
    let err = VimRegex::compile(&pat).expect_err("a 5000-deep pattern must not compile");
    assert!(
        err.to_string().contains("nested too deeply"),
        "the rejection should name the nesting, got {err:?}"
    );
}

/// The cap is a *depth* limit, not a ban: ordinary nesting still compiles and
/// matches. (400 is comfortably under the 500 cap and far past anything a human
/// writes.)
#[test]
fn ordinary_non_capturing_nesting_still_compiles() {
    let depth = 400;
    let pat = format!("{}x{}", "\\%(".repeat(depth), "\\)".repeat(depth));
    let re = VimRegex::compile(&pat).expect("400 levels is within the cap");
    assert!(re.exec_line("axb", 0, false).unwrap().is_some());
}

/// Catastrophic backtracking. `\(a\|aa\)\+b` against a run of `a`s with no `b` is
/// the classic exponential case, and `\%#=1` pins the backtracking engine the way
/// `'regexpengine'` or the NFA engine's own "too slow" fallback does. Unbounded,
/// this does not return: 30 `a`s took ~2.5 s and each further `a` roughly doubles
/// it, on the editor's synchronous thread, with no interrupt wired — the editor
/// is simply gone.
///
/// The per-call deadline turns it into an error after
/// [`bemtvi_regex::DEFAULT_TIMEOUT_MS`]. The generous assertion window is
/// deliberate: the claim under test is "bounded", not "bounded to the millisecond"
/// (this suite runs alongside a loaded `cargo test --workspace`).
#[test]
fn catastrophic_backtracking_ends_in_a_timeout_not_a_hang() {
    let re = VimRegex::compile("\\%#=1\\(a\\|aa\\)\\+b").expect("compile");
    let line = "a".repeat(30);
    let started = Instant::now();
    let err = re
        .exec_line(&line, 0, false)
        .expect_err("an exponential match must be cut off, not run to completion");
    let elapsed = started.elapsed();
    assert!(
        err.to_string().contains("timed out"),
        "the error should name the timeout, got {err:?}"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "the deadline is what bounds the freeze; this took {elapsed:?}"
    );
}

/// The budget is a real parameter, not a constant baked into the timeout check: a
/// short explicit budget must cut the same match off correspondingly sooner. This
/// is what proves the deadline is being *honored* rather than the match happening
/// to end near the default.
#[test]
fn an_explicit_budget_bounds_the_same_match_sooner() {
    let re = VimRegex::compile("\\%#=1\\(a\\|aa\\)\\+b").expect("compile");
    let line = "a".repeat(30);
    let started = Instant::now();
    let err = re
        .exec_line_timed(&line, 0, false, Some(150))
        .expect_err("the 150 ms budget must cut the match off");
    let elapsed = started.elapsed();
    assert!(
        err.to_string().contains("timed out"),
        "the error should name the timeout, got {err:?}"
    );
    assert!(
        elapsed < Duration::from_millis(1_500),
        "a 150 ms budget should end well before the {}ms default; took {elapsed:?}",
        bemtvi_regex::DEFAULT_TIMEOUT_MS
    );
}

/// The deadline must not fire on ordinary patterns — a timeout is reported as "no
/// match" upstream, so a spurious one would silently break search. A normal match
/// over a long line stays a match.
#[test]
fn an_ordinary_pattern_on_a_long_line_does_not_time_out() {
    let re = VimRegex::compile("needle").expect("compile");
    let line = format!("{}needle{}", "x".repeat(200_000), "y".repeat(200_000));
    let m = re
        .exec_line(&line, 0, false)
        .expect("a linear pattern must not hit the deadline")
        .expect("it is there");
    assert_eq!(m.start, 200_000);
}
