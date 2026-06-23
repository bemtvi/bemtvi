//! Hermetic coverage for the Lua-pattern matcher backing `#lua-match?` predicates.
//!
//! The expected results mirror Lua 5.4's `string.find(s, p) ~= nil`. No grammar or
//! network is needed (unlike the treesitter e2e tests), so these run by default.

use nxvim_ts::lua_pattern::lua_match;

/// `lua_match` over `&str` inputs for terser cases.
fn m(s: &str, p: &str) -> bool {
    lua_match(s.as_bytes(), p.as_bytes())
}

#[test]
fn literals_and_anchors() {
    assert!(m("hello", "ell"));
    assert!(!m("hello", "xyz"));
    assert!(m("hello", "^he"));
    assert!(!m("hello", "^ell")); // `^` anchors to the start
    assert!(m("hello", "llo$"));
    assert!(!m("hello", "ell$")); // `$` anchors to the end
    assert!(m("hello", "^hello$"));
    assert!(m("", "^$")); // empty string, empty-anchored pattern
}

#[test]
fn the_shebang_predicate() {
    // The exact pattern from bash's highlights.scm shebang rule.
    let pat = r"^#![ \t]*/";
    assert!(lua_match(b"#!/bin/bash", pat.as_bytes()));
    assert!(lua_match(b"#! /usr/bin/env bash", pat.as_bytes()));
    assert!(!lua_match(b"# a normal comment", pat.as_bytes()));
    assert!(!lua_match(b"#!no-slash", pat.as_bytes()));
}

#[test]
fn character_classes() {
    assert!(m("abc123", "%d")); // a digit exists
    assert!(m("abc", "^%a+$")); // all letters
    assert!(!m("abc1", "^%a+$")); // a digit breaks all-letters
    assert!(m("   x", "^%s+")); // leading whitespace
    assert!(m("FOO", "^%u+$")); // all uppercase
    assert!(!m("Foo", "^%u+$"));
    assert!(m("a_b", "%w")); // %w is alphanumeric (not '_')
    assert!(!m("___", "^%w+$")); // '_' is not %w
    assert!(m("deadBEEF", "^%x+$")); // all hex digits
    assert!(!m("xyz", "^%x+$"));
}

#[test]
fn negated_classes_and_sets() {
    assert!(m("abc", "[abc]"));
    assert!(!m("xyz", "[abc]"));
    assert!(m("a-z range", "^[a-z]")); // range
    assert!(m("X", "^[^a-z]$")); // negated set
    assert!(!m("m", "^[^a-z]$"));
    assert!(m("]", "^[]]$")); // a leading ']' is a literal set member
    assert!(m("9", "^[%d]$")); // a class inside a set
    assert!(!m("a", "^[%d]$"));
}

#[test]
fn quantifiers() {
    assert!(m("aaa", "^a*$")); // greedy 0+
    assert!(m("", "^a*$")); // 0 is allowed
    assert!(m("aaa", "^a+$")); // greedy 1+
    assert!(!m("", "^a+$")); // needs at least one
    assert!(m("color", "^colou?r$")); // optional 'u' absent
    assert!(m("colour", "^colou?r$")); // optional 'u' present
    assert!(m("<a><b>", "^<.->")); // lazy: matches the shortest "<...>"
    assert!(m("abcabc", "^a.-c")); // lazy still finds a match
}

#[test]
fn escaped_magic_characters() {
    assert!(m("a.b", "a%.b")); // escaped '.' is literal
    assert!(!m("axb", "a%.b")); // so 'x' does not match '%.'
    assert!(m("100%", "%d+%%")); // escaped '%'
    assert!(m("(x)", "%(x%)")); // escaped parens
}

#[test]
fn balanced_and_frontier() {
    assert!(m("(a(b)c)", "%b()")); // %b balanced match
    assert!(!m("(((", "%b()")); // never closes → no balanced run
    assert!(m("THE (quick) fox", "%f[%a]%a+")); // frontier onto a letter run
                                                // %f boundary: 'word' at a letter frontier (prev char not a letter).
    assert!(m("word", "%f[%a]word")); // start-of-string counts as a non-letter
    assert!(m("_word", "%f[%a]word")); // '_' is not %a, so the frontier still holds
    assert!(!m("aword", "%f[%a]word")); // 'a' IS %a, so there is no frontier before 'w'
}

#[test]
fn anywhere_vs_anchored() {
    assert!(m("xxhello", "hello")); // unanchored: found mid-string
    assert!(!m("xxhello", "^hello")); // anchored: must be at start
    assert!(m("a1b2c3", "%d")); // first digit anywhere
}

#[test]
fn malformed_patterns_do_not_panic() {
    // A dangling '%' / unterminated set must not panic — they just fail to match.
    assert!(!m("abc", "%"));
    assert!(!m("abc", "[abc"));
}
