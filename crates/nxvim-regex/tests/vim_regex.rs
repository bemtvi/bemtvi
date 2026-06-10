//! Black-box tests of the vendored vim regexp engines through the public API.
//!
//! Expected results are vim's documented behavior (checked against `:help
//! pattern` and real vim/nvim where in doubt).

use nxvim_regex::{BufPos, Engine, PatternKind, VimBuffer, VimRegex};

fn m(re: &str, line: &str) -> Option<(usize, usize)> {
    VimRegex::compile(re)
        .unwrap_or_else(|e| panic!("compile {re:?}: {e}"))
        .exec_line(line, 0, false)
        .unwrap()
        .map(|m| (m.start, m.end))
}

fn matched(re: &str, line: &str) -> Option<String> {
    m(re, line).map(|(s, e)| line[s..e].to_string())
}

// ---------------------------------------------------------------------------
// basics, magic levels

#[test]
fn literal_and_magic_default() {
    assert_eq!(m("foo", "say foo!"), Some((4, 7)));
    // magic: . is any char, * repeats
    assert_eq!(matched("f.o*", "xfaooo"), Some("faooo".into()));
    // + needs a backslash under 'magic' (it's \+)
    assert_eq!(matched("ab\\+", "abbb"), Some("abbb".into()));
    assert_eq!(matched("ab+", "abbb"), None); // literal "ab+"
    assert_eq!(matched("ab+", "xab+y"), Some("ab+".into()));
}

#[test]
fn very_magic_and_very_nomagic() {
    // \v: ERE-like
    assert_eq!(matched("\\v(ab)+", "xabab!"), Some("abab".into()));
    assert_eq!(matched("\\v<\\w+>", "  hello "), Some("hello".into()));
    // \V: everything literal except backslash
    assert_eq!(matched("\\Va.b", "xa.by"), Some("a.b".into()));
    assert_eq!(matched("\\Va.b", "xaXby"), None);
}

#[test]
fn alternation_and_grouping() {
    assert_eq!(
        matched("\\(foo\\|bar\\)baz", "xbarbaz"),
        Some("barbaz".into())
    );
    let re = VimRegex::compile("\\(a*\\)\\(b*\\)c").unwrap();
    let mm = re.exec_line("xaabbc", 0, false).unwrap().unwrap();
    assert_eq!(mm.submatches[1], Some((1, 3))); // "aa"
    assert_eq!(mm.submatches[2], Some((3, 5))); // "bb"
}

// ---------------------------------------------------------------------------
// multis: greedy, lazy, counted

#[test]
fn counted_and_lazy_multis() {
    assert_eq!(matched("a\\{2,3}", "aaaaa"), Some("aaa".into()));
    assert_eq!(matched("a\\{-1,}", "aaa"), Some("a".into())); // lazy
    assert_eq!(matched("x\\{-}y", "xxxy"), Some("xxxy".into())); // lazy but must reach y
    assert_eq!(matched("\\vx{2}", "xxx"), Some("xx".into()));
}

// ---------------------------------------------------------------------------
// \zs / \ze

#[test]
fn zs_ze_set_match_bounds() {
    assert_eq!(matched("foo\\zsbar", "foobarbaz"), Some("bar".into()));
    assert_eq!(matched("foo\\zebar", "foobarbaz"), Some("foo".into()));
}

// ---------------------------------------------------------------------------
// case folding

#[test]
fn ignore_case_flag_and_atoms() {
    let re = VimRegex::compile("hello").unwrap();
    assert!(re.exec_line("say HELLO", 0, true).unwrap().is_some());
    assert!(re.exec_line("say HELLO", 0, false).unwrap().is_none());
    // \c forces ignore-case from inside the pattern
    assert_eq!(matched("\\cHELLO", "say hello"), Some("hello".into()));
    // \C forces match-case even with ignore_case=true
    let re = VimRegex::compile("\\CHELLO").unwrap();
    assert!(re.exec_line("say hello", 0, true).unwrap().is_none());
    // multibyte case folding (NFA + utf8proc)
    assert_eq!(matched("\\cÄ", "xäy"), Some("ä".into()));
}

// ---------------------------------------------------------------------------
// character classes, iskeyword

#[test]
fn char_classes() {
    assert_eq!(matched("\\d\\+", "ab123cd"), Some("123".into()));
    assert_eq!(matched("\\x\\+", "zz1aF!"), Some("1aF".into()));
    assert_eq!(matched("[[:upper:]]\\+", "abcDEFg"), Some("DEF".into()));
    assert_eq!(matched("[^a-c]\\+", "abxyc"), Some("xy".into()));
}

#[test]
fn keyword_class_follows_iskeyword() {
    let mut buf = VimBuffer::from_lines(&["foo-bar"]).unwrap();
    let re = VimRegex::compile("\\k\\+").unwrap();
    // default iskeyword: '-' is not a keyword char
    let m1 = buf.exec(&re, 1, 0, false, None).unwrap().unwrap();
    assert_eq!((m1.start.col, m1.end.col), (0, 3)); // "foo"
                                                    // add '-' to iskeyword
    buf.set_iskeyword("@,48-57,_,192-255,-").unwrap();
    let m2 = buf.exec(&re, 1, 0, false, None).unwrap().unwrap();
    assert_eq!((m2.start.col, m2.end.col), (0, 7)); // "foo-bar"
}

#[test]
fn word_boundaries() {
    assert_eq!(matched("\\<bar\\>", "foo bar baz"), Some("bar".into()));
    assert_eq!(m("\\<bar\\>", "foobar baz"), None);
}

// ---------------------------------------------------------------------------
// backreferences (backtracking engine territory)

#[test]
fn backreferences() {
    assert_eq!(
        matched("\\(\\w\\+\\) \\1", "say foo foo!"),
        Some("foo foo".into())
    );
    assert_eq!(m("\\(\\w\\+\\) \\1", "say foo bar!"), None);
}

#[test]
fn engine_selection_explicit() {
    // the same pattern through both engines must agree
    for eng in [Engine::Backtracking, Engine::Nfa] {
        let re = VimRegex::compile_with("\\v(ab|a)+c", PatternKind::Buffer, eng).unwrap();
        let mm = re.exec_line("xaababc!", 0, false).unwrap().unwrap();
        assert_eq!((mm.start, mm.end), (1, 7), "engine {eng:?}");
    }
    // backrefs are rejected by the pure NFA engine but work via Auto
    assert!(VimRegex::compile("\\(a\\)\\1").is_ok());
}

// ---------------------------------------------------------------------------
// lookaround

#[test]
fn lookahead_lookbehind() {
    assert_eq!(matched("foo\\(bar\\)\\@=", "xfoobar"), Some("foo".into()));
    assert_eq!(m("foo\\(bar\\)\\@=", "xfoobaz"), None);
    assert_eq!(matched("foo\\(bar\\)\\@!", "xfoobaz"), Some("foo".into()));
    assert_eq!(matched("\\(foo\\)\\@<=bar", "foobar"), Some("bar".into()));
    assert_eq!(m("\\(foo\\)\\@<=bar", "bazbar"), None);
    assert_eq!(matched("\\(foo\\)\\@<!bar", "bazbar"), Some("bar".into()));
}

#[test]
fn optional_sequence() {
    // \%[...] — optionally matched sequence
    assert_eq!(matched("r\\%[ead]", "x re y"), Some("re".into()));
    assert_eq!(matched("r\\%[ead]", "x read y"), Some("read".into()));
}

// ---------------------------------------------------------------------------
// errors

#[test]
fn compile_errors_carry_vim_messages() {
    let err = VimRegex::compile("\\(unclosed").unwrap_err();
    assert!(err.0.contains("E54"), "got: {err}");
    // unmatched \) is E55 (note: a\{2,1} is NOT an error in vim — it matches)
    let err = VimRegex::compile("ab\\)").unwrap_err();
    assert!(err.0.contains("E55"), "got: {err}");
}

#[test]
fn nul_bytes_rejected() {
    assert!(VimRegex::compile("a\0b").is_err());
    let re = VimRegex::compile("a").unwrap();
    assert!(re.exec_line("a\0b", 0, false).is_err());
}

// ---------------------------------------------------------------------------
// multi-line buffer matching

fn buf3() -> VimBuffer {
    VimBuffer::from_lines(&["hello world", "foo bar baz", "x  end"]).unwrap()
}

#[test]
fn multiline_newline_pattern() {
    let buf = buf3();
    let re = VimRegex::compile("world\\nfoo").unwrap();
    assert!(re.is_multiline());
    let m = buf.exec(&re, 1, 0, false, None).unwrap().unwrap();
    assert_eq!(m.start, BufPos { lnum: 1, col: 6 });
    assert_eq!(m.end, BufPos { lnum: 2, col: 3 });
    // no match starting on line 2
    assert!(buf.exec(&re, 2, 0, false, None).unwrap().is_none());
}

#[test]
fn multiline_any_class() {
    let buf = buf3();
    let re = VimRegex::compile("l\\_.\\{-}bar").unwrap();
    let m = buf.exec(&re, 1, 0, false, None).unwrap().unwrap();
    assert_eq!(m.start, BufPos { lnum: 1, col: 2 });
    assert_eq!(m.end, BufPos { lnum: 2, col: 7 });
}

#[test]
fn match_must_start_at_lnum() {
    let buf = buf3();
    let re = VimRegex::compile("foo").unwrap();
    assert!(buf.exec(&re, 1, 0, false, None).unwrap().is_none());
    let m = buf.exec(&re, 2, 0, false, None).unwrap().unwrap();
    assert_eq!(m.start, BufPos { lnum: 2, col: 0 });
}

#[test]
fn col_offset_respected() {
    let buf = VimBuffer::from_lines(&["abc abc"]).unwrap();
    let re = VimRegex::compile("abc").unwrap();
    let m = buf.exec(&re, 1, 1, false, None).unwrap().unwrap();
    assert_eq!(m.start.col, 4);
}

// ---------------------------------------------------------------------------
// context assertions: cursor, marks, Visual, line/col

#[test]
fn line_and_column_assertions() {
    let buf = buf3();
    // \%2l: only on line 2
    let re = VimRegex::compile("\\%2lba.").unwrap();
    let m = buf.exec(&re, 2, 0, false, None).unwrap().unwrap();
    assert_eq!(m.start, BufPos { lnum: 2, col: 4 });
    let re_l3 = VimRegex::compile("\\%3lba.").unwrap();
    assert!(buf.exec(&re_l3, 2, 0, false, None).unwrap().is_none());
    // \%>2c: column > 2
    let re_c = VimRegex::compile("\\%>4cb..").unwrap();
    let m = buf.exec(&re_c, 2, 0, false, None).unwrap().unwrap();
    assert_eq!(m.start.col, 4); // "bar" at col 4 (1-based col 5 > 4)
}

#[test]
fn cursor_assertion() {
    let mut buf = buf3();
    buf.set_cursor(BufPos { lnum: 2, col: 4 });
    let re = VimRegex::compile("\\%#\\w\\+").unwrap();
    let m = buf.exec(&re, 2, 0, false, None).unwrap().unwrap();
    assert_eq!((m.start.col, m.end.col), (4, 7)); // "bar" under the cursor
    assert!(buf.exec(&re, 1, 0, false, None).unwrap().is_none());
}

#[test]
fn mark_assertion() {
    let mut buf = buf3();
    buf.set_mark('m', BufPos { lnum: 2, col: 4 });
    let re = VimRegex::compile("\\%'m\\w\\+").unwrap();
    let m = buf.exec(&re, 2, 0, false, None).unwrap().unwrap();
    assert_eq!((m.start.col, m.end.col), (4, 7));
    // unset mark never matches (vim behavior), reported as no-match not error
    let re_q = VimRegex::compile("\\%'q\\w\\+").unwrap();
    assert!(buf.exec(&re_q, 2, 0, false, None).unwrap().is_none());
}

#[test]
fn visual_assertion() {
    let mut buf = buf3();
    // charwise Visual covering "bar" on line 2 (anchor col 4, end col 6)
    buf.set_visual(BufPos { lnum: 2, col: 4 }, BufPos { lnum: 2, col: 6 }, 'v');
    // vim semantics (verified against real nvim): the trailing \%V is
    // asserted at the next-char position, so the greedy \w\+ backs off to
    // "ba" — the position after 'a' (the 'r', col 6) is still inside Visual.
    let re = VimRegex::compile("\\%V\\w\\+\\%V").unwrap();
    let m = buf.exec(&re, 2, 0, false, None).unwrap().unwrap();
    assert_eq!((m.start.col, m.end.col), (4, 6));
    // without the trailing assertion the whole word matches from the anchor
    let re2 = VimRegex::compile("\\%V\\w\\+").unwrap();
    let m2 = buf.exec(&re2, 2, 0, false, None).unwrap().unwrap();
    assert_eq!((m2.start.col, m2.end.col), (4, 7));
}

// ---------------------------------------------------------------------------
// \z external submatches are syntax-engine plumbing; \= substitution is
// host-side. Both out of scope here.

// ---------------------------------------------------------------------------
// unicode

#[test]
fn utf8_matching() {
    assert_eq!(matched("é\\+", "xééy"), Some("éé".into()));
    // composing-char-aware dot: precomposed vs decomposed must not be conflated
    assert_eq!(m("x.y", "xéy"), Some((0, "xéy".len())));
    // multibyte word chars: é is iskeyword (192-255 default)
    assert_eq!(matched("\\k\\+", " café "), Some("café".into()));
}

#[test]
fn utf8_offsets_are_bytes() {
    let line = "αβγ delta";
    let (s, e) = m("delta", line).unwrap();
    assert_eq!(&line[s..e], "delta");
    assert_eq!(s, 7); // 3 two-byte greek letters + space
}

// ---------------------------------------------------------------------------
// pattern introspection

#[test]
fn is_multiline_reflects_pattern() {
    assert!(!VimRegex::compile("foo").unwrap().is_multiline());
    assert!(VimRegex::compile("foo\\nbar").unwrap().is_multiline());
    assert!(VimRegex::compile("a\\_sb").unwrap().is_multiline());
}

// ---------------------------------------------------------------------------
// regression: many compiled programs alive at once, drop order

#[test]
fn many_programs_and_drop() {
    let res: Vec<VimRegex> = (0..100)
        .map(|i| VimRegex::compile(&format!("pat{i}\\d\\+")).unwrap())
        .collect();
    for (i, re) in res.iter().enumerate() {
        let line = format!("xx pat{i}42 yy");
        let mm = re.exec_line(&line, 0, false).unwrap().unwrap();
        assert_eq!(&line[mm.start..mm.end], &format!("pat{i}42"));
    }
}
