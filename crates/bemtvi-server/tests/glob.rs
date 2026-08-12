//! Behavior tests for the `btv.glob.*` surface — the canonical glob engine
//! (`bemtvi_core::glob`: globset parses the pattern, the translated regex is compiled
//! and cached) exposed to Lua. Black-box per the project conventions: a real server
//! over RPC, asserting via `nvim_exec_lua`.
//!
//! The engine has no standalone test seam of its own (no unit tests), so this suite
//! is what covers the core module too: the syntax, the two deliberate defaults
//! (`*` stops at `/`; path style is explicit rather than the host's), and the cache
//! key.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{exec_lua, start_attached};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Run `expr` (a Lua expression) and return its `tostring()`'d value, so the
/// per-case assertions stay one-liners and booleans/nil are printable.
async fn eval(rpc: &Rpc, expr: &str) -> String {
    exec_lua(rpc, &format!("return tostring({expr})"))
        .await
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// `btv.glob.match(pat, path)` as a string, for the many one-line syntax cases.
async fn matches(rpc: &Rpc, pattern: &str, path: &str) -> String {
    eval(rpc, &format!("btv.glob.match({pattern:?}, {path:?})")).await
}

// ===== The `*` / `**` / `?` defaults =======================================

/// The headline default: `*` does NOT cross a path separator (shell / gitignore /
/// LSP semantics), and `**` is how you say "any depth". `?` is a single character
/// and likewise never a separator.
#[tokio::test]
async fn star_stops_at_a_separator_and_doublestar_crosses_it() {
    let (rpc, _incoming) = start().await;
    assert_eq!(matches(&rpc, "*.lua", "init.lua").await, "true");
    assert_eq!(
        matches(&rpc, "*.lua", "conf/init.lua").await,
        "false",
        "`*` must not cross `/` — that is what `**` is for"
    );
    assert_eq!(matches(&rpc, "**/*.lua", "conf/init.lua").await, "true");
    assert_eq!(matches(&rpc, "**/*.lua", "a/b/c/init.lua").await, "true");
    assert_eq!(matches(&rpc, "src/**", "src/a/b.rs").await, "true");
    assert_eq!(matches(&rpc, "src/**/b.rs", "src/a/x/b.rs").await, "true");
    // `?` is exactly one character, and never the separator itself.
    assert_eq!(matches(&rpc, "a?c.txt", "abc.txt").await, "true");
    assert_eq!(matches(&rpc, "a?c.txt", "ac.txt").await, "false");
    assert_eq!(matches(&rpc, "a?c", "a/c").await, "false");
}

/// `literal_separator = false` is the opt-out that gives vim's autocmd rule, where
/// `*` *does* cross `/`. Phase 2 converges `autocmd.lua` onto exactly this.
#[tokio::test]
async fn literal_separator_false_lets_star_cross_a_separator() {
    let (rpc, _incoming) = start().await;
    let got = eval(
        &rpc,
        "btv.glob.match('*.lua', 'a/b/c.lua', { literal_separator = false })",
    )
    .await;
    assert_eq!(got, "true", "with literal_separator=false, `*` crosses `/`");
}

/// `basename = true` matches a separator-less pattern against the path's last
/// component (vim's file-pattern rule, gitignore's). A pattern that *does* contain a
/// separator is unaffected and still matches the whole path.
#[tokio::test]
async fn basename_matches_the_last_component_only_for_separatorless_patterns() {
    let (rpc, _incoming) = start().await;
    let base = "{ basename = true }";
    assert_eq!(
        eval(
            &rpc,
            &format!("btv.glob.match('*.lua', 'a/b/c.lua', {base})")
        )
        .await,
        "true"
    );
    // A bare name still matches a bare candidate (the tail of a separator-less path
    // is the path).
    assert_eq!(
        eval(&rpc, &format!("btv.glob.match('*.lua', 'c.lua', {base})")).await,
        "true"
    );
    assert_eq!(
        eval(
            &rpc,
            &format!("btv.glob.match('*.rs', 'a/b/c.lua', {base})")
        )
        .await,
        "false"
    );
    // The pattern has a separator, so `basename` does not apply to it: it is still
    // matched against the whole path, and `*` still stops at `/`.
    assert_eq!(
        eval(
            &rpc,
            &format!("btv.glob.match('b/*.lua', 'a/b/c.lua', {base})")
        )
        .await,
        "false",
        "a pattern containing `/` is matched whole-path even under basename"
    );
    assert_eq!(
        eval(
            &rpc,
            &format!("btv.glob.match('b/*.lua', 'b/c.lua', {base})")
        )
        .await,
        "true"
    );
}

// ===== Classes and braces ==================================================

/// Bracket classes: a set, a range, and both negation spellings — shell's `[!a]` and
/// vim's `[^a]` mean the same thing.
#[tokio::test]
async fn bracket_classes_cover_sets_ranges_and_both_negations() {
    let (rpc, _incoming) = start().await;
    assert_eq!(matches(&rpc, "[abc].txt", "b.txt").await, "true");
    assert_eq!(matches(&rpc, "[abc].txt", "d.txt").await, "false");
    assert_eq!(matches(&rpc, "[a-z].txt", "q.txt").await, "true");
    assert_eq!(matches(&rpc, "[a-z].txt", "Q.txt").await, "false");
    assert_eq!(matches(&rpc, "[!a]*.txt", "b.txt").await, "true");
    assert_eq!(matches(&rpc, "[!a]*.txt", "a.txt").await, "false");
    assert_eq!(matches(&rpc, "[^a]*.txt", "b.txt").await, "true");
    assert_eq!(
        matches(&rpc, "[^a]*.txt", "a.txt").await,
        "false",
        "`[^a]` must negate like `[!a]`, not match `^` and `a` literally"
    );
}

/// Brace alternation, including a nested branch — neither of which the old
/// Lua-pattern autocmd translator could express at all.
#[tokio::test]
async fn brace_alternation_including_nested_branches() {
    let (rpc, _incoming) = start().await;
    assert_eq!(matches(&rpc, "*.{rs,toml}", "Cargo.toml").await, "true");
    assert_eq!(matches(&rpc, "*.{rs,toml}", "main.rs").await, "true");
    assert_eq!(matches(&rpc, "*.{rs,toml}", "main.lua").await, "false");
    assert_eq!(
        matches(&rpc, "src/**/*.{rs,toml}", "src/a/b/Cargo.toml").await,
        "true"
    );
    // A nested alternate: one branch is itself a multi-component glob.
    assert_eq!(matches(&rpc, "{a,b/**}/x.txt", "a/x.txt").await, "true");
    assert_eq!(matches(&rpc, "{a,b/**}/x.txt", "b/c/d/x.txt").await, "true");
    assert_eq!(matches(&rpc, "{a,b/**}/x.txt", "c/x.txt").await, "false");
}

/// `empty_alternates` governs whether a brace list may carry an EMPTY branch, so
/// `x.{lua,}` can mean "`x.lua` or bare `x.`". Off by default (an empty alternate is
/// nearly always a typo), and it is part of the compile cache key, so both spellings
/// must be able to coexist.
#[tokio::test]
async fn empty_alternates_opts_into_the_empty_brace_branch() {
    let (rpc, _incoming) = start().await;
    // Default: the non-empty branch still matches, the empty one does not exist.
    assert_eq!(matches(&rpc, "x.{lua,}", "x.lua").await, "true");
    assert_eq!(matches(&rpc, "x.{lua,}", "x.").await, "false");
    // Opted in: the empty branch matches too — same pattern, same call, other answer.
    let got = exec_lua(
        &rpc,
        "local on = { empty_alternates = true }\n\
         return tostring(btv.glob.match('x.{lua,}', 'x.', on))\n\
         \x20 .. '|' .. tostring(btv.glob.match('x.{lua,}', 'x.lua', on))\n\
         \x20 .. '|' .. tostring(btv.glob.match('x.{lua,}', 'x.rs', on))\n\
         \x20 .. '|' .. tostring(btv.glob.match('x.{lua,}', 'x.'))",
    )
    .await;
    // The last field re-runs the DEFAULT spelling after the opted-in one: it must still
    // be `false`, which is only true if the option is part of the cache key.
    assert_eq!(got.as_str(), Some("true|true|false|false"));
    // The translation itself shows the difference — the empty branch in the group.
    let regexes = exec_lua(
        &rpc,
        "return btv.glob.to_regex('x.{lua,}')\n\
         \x20 .. ' || ' .. btv.glob.to_regex('x.{lua,}', { empty_alternates = true })",
    )
    .await;
    let regexes = regexes.as_str().unwrap_or_default();
    let (off, on) = regexes.split_once(" || ").expect("two regexes");
    assert!(off.contains("(?:lua)"), "off: {off}");
    assert!(on.contains("(?:lua|)"), "on: {on}");
}

/// An unclosed class (`foo[bar`) is taken as literal text rather than raising — a
/// filename may genuinely contain a bracket, and this is the one glob malformation
/// common enough to tolerate. (It is also what keeps the autocmd suite's
/// `malformed_glob_class_does_not_abort_the_event_fire` case working after Phase 2.)
#[tokio::test]
async fn unclosed_bracket_class_is_literal_text_not_an_error() {
    let (rpc, _incoming) = start().await;
    assert_eq!(matches(&rpc, "foo[bar", "foo[bar").await, "true");
    assert_eq!(matches(&rpc, "foo[bar", "foobar").await, "false");
}

/// A genuinely invalid pattern fails LOUD (no silent "matches nothing"), naming the
/// glob. A reversed range is the canonical case.
#[tokio::test]
async fn an_invalid_pattern_raises_naming_the_glob() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        "local ok, err = pcall(btv.glob.match, '[z-a].txt', 'b.txt')\n\
         return tostring(ok) .. '|' .. tostring(err)",
    )
    .await;
    let got = got.as_str().unwrap_or_default();
    assert!(
        got.starts_with("false|"),
        "an invalid glob must raise, not silently match nothing: {got}"
    );
    assert!(
        got.contains("[z-a]"),
        "the error must name the offending glob: {got}"
    );
}

// ===== Path style: unix vs windows =========================================

/// Windows style makes `\` a separator: `src\*.rs` is a path, both spellings of the
/// candidate match, and `**` crosses a backslash. This must hold on a Unix host —
/// the whole point of taking style as an option rather than a `#[cfg(windows)]` is
/// that a daemon/remote session hands a Unix-hosted editor Windows paths.
#[tokio::test]
async fn windows_style_treats_backslash_as_a_separator() {
    let (rpc, _incoming) = start().await;
    let win = "{ style = 'windows' }";
    assert_eq!(
        eval(
            &rpc,
            &format!(r#"btv.glob.match('src\\*.rs', 'src\\main.rs', {win})"#)
        )
        .await,
        "true"
    );
    // Mixed spellings normalize to the same path, so either side may use either
    // separator.
    assert_eq!(
        eval(
            &rpc,
            &format!(r#"btv.glob.match('src/*.rs', 'src\\main.rs', {win})"#)
        )
        .await,
        "true"
    );
    assert_eq!(
        eval(
            &rpc,
            &format!(r#"btv.glob.match('src\\*.rs', 'src/main.rs', {win})"#)
        )
        .await,
        "true"
    );
    // `*` stops at a backslash too, since it is a separator here.
    assert_eq!(
        eval(
            &rpc,
            &format!(r#"btv.glob.match('src\\*.rs', 'src\\a\\main.rs', {win})"#)
        )
        .await,
        "false",
        "`*` must stop at `\\` in windows style"
    );
    assert_eq!(
        eval(
            &rpc,
            &format!(r#"btv.glob.match('src\\**\\*.rs', 'src\\a\\b\\main.rs', {win})"#)
        )
        .await,
        "true"
    );
    // A drive letter is just an ordinary leading component.
    assert_eq!(
        eval(
            &rpc,
            &format!(r#"btv.glob.match('C:/**/*.txt', 'C:\\Users\\me\\a.txt', {win})"#)
        )
        .await,
        "true"
    );
}

/// Unix style keeps `\` as the pattern ESCAPE character (that is how a literal `*`
/// in a filename is spelled) and as an ordinary byte in a candidate — the exact
/// opposite of windows style, on the same host. The two styles must not bleed.
#[tokio::test]
async fn unix_style_treats_backslash_as_an_escape_not_a_separator() {
    let (rpc, _incoming) = start().await;
    let unix = "{ style = 'unix' }";
    // `\*` is a literal asterisk, so it matches the file actually named `a*b`...
    assert_eq!(
        eval(&rpc, &format!(r#"btv.glob.match('a\\*b', 'a*b', {unix})"#)).await,
        "true"
    );
    // ...and NOT an arbitrary run of characters, which an unescaped `*` would.
    assert_eq!(
        eval(
            &rpc,
            &format!(r#"btv.glob.match('a\\*b', 'axyzb', {unix})"#)
        )
        .await,
        "false",
        "an escaped `*` must be literal in unix style"
    );
    // A backslash in a unix candidate is an ordinary filename byte, not a
    // separator, so `*` may cross it.
    assert_eq!(
        eval(&rpc, &format!(r#"btv.glob.match('a*b', 'a\\qb', {unix})"#)).await,
        "true",
        "`\\` is a plain byte in a unix path, so `*` crosses it"
    );
}

/// The same pattern and path, matched under both styles, must disagree — proof that
/// the style option actually reaches the engine rather than the host convention
/// deciding for it.
#[tokio::test]
async fn the_two_styles_disagree_on_the_same_input() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        "local p, path = 'a*b', 'a\\\\q\\\\b'\n\
         return tostring(btv.glob.match(p, path, { style = 'unix' }))\n\
         \x20 .. '|' .. tostring(btv.glob.match(p, path, { style = 'windows' }))",
    )
    .await;
    assert_eq!(
        got.as_str(),
        Some("true|false"),
        "unix treats `\\` as a byte (`*` crosses it); windows treats it as a separator (`*` stops)"
    );
}

/// An unknown `style` fails loud rather than silently falling back to the host
/// convention — a typo'd style that quietly matched the wrong way would be
/// near-impossible to spot.
#[tokio::test]
async fn an_unknown_path_style_raises() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        "local ok, err = pcall(btv.glob.match, '*.rs', 'a.rs', { style = 'win32' })\n\
         return tostring(ok) .. '|' .. tostring(err)",
    )
    .await;
    let got = got.as_str().unwrap_or_default();
    assert!(got.starts_with("false|"), "unknown style must raise: {got}");
    assert!(
        got.contains("win32") && got.contains("unix"),
        "the error must name the bad style and the valid ones: {got}"
    );
}

/// `ignorecase` is ASCII-case-insensitive, and is NOT implied by windows style —
/// matching case-sensitively against Windows-spelled paths is a legitimate ask.
#[tokio::test]
async fn ignorecase_is_opt_in_and_independent_of_style() {
    let (rpc, _incoming) = start().await;
    assert_eq!(matches(&rpc, "*.TXT", "a.txt").await, "false");
    assert_eq!(
        eval(
            &rpc,
            "btv.glob.match('*.TXT', 'a.txt', { ignorecase = true })"
        )
        .await,
        "true"
    );
    assert_eq!(
        eval(
            &rpc,
            r#"btv.glob.match('SRC\\*.RS', 'src\\a.rs', { style = 'windows' })"#
        )
        .await,
        "false",
        "windows style must not silently imply ignorecase"
    );
}

// ===== The compiled object, sets, and the list helpers =====================

/// `btv.glob.compile` hands back a reusable object: `:test`, `:pattern` (as written),
/// and `:regex` (the translation, for debugging).
#[tokio::test]
async fn compile_returns_a_reusable_object_exposing_its_translation() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        "local g = btv.glob.compile('**/*.lua')\n\
         return tostring(g:test('a/b.lua')) .. '|' .. tostring(g:test('a/b.rs'))\n\
         \x20 .. '|' .. g:pattern() .. '|' .. tostring(#g:regex() > 0)",
    )
    .await;
    assert_eq!(got.as_str(), Some("true|false|**/*.lua|true"));
}

/// `btv.glob.to_regex` exposes the real translation: anchored, carrying the literal
/// part, and — the load-bearing bit — reflecting the options, so the `*`-stops-at-`/`
/// default is visibly a separator exclusion that `literal_separator = false` removes.
///
/// It is asserted on shape rather than round-tripped through `btv.regex`: globset
/// emits a BYTE-oriented source (`(?-u)`, and `[^/]` can match invalid UTF-8), which
/// the str-based `btv.regex` rejects by design. That is why the engine compiles it
/// with `regex::bytes` internally.
#[tokio::test]
async fn to_regex_exposes_the_translation_including_the_options() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        "local strict = btv.glob.to_regex('*.lua')\n\
         local loose = btv.glob.to_regex('*.lua', { literal_separator = false })\n\
         return strict .. '\\n' .. loose",
    )
    .await;
    let got = got.as_str().unwrap_or_default();
    let (strict, loose) = got.split_once('\n').expect("two regex sources");
    for src in [strict, loose] {
        assert!(
            src.contains(r"\.lua") && src.contains('^') && src.contains('$'),
            "the translation must be anchored and carry the literal part: {src}"
        );
    }
    assert!(
        strict.contains("[^/]"),
        "the `*`-stops-at-`/` default must show up as a separator exclusion: {strict}"
    );
    assert!(
        !loose.contains("[^/]"),
        "literal_separator=false must drop the separator exclusion: {loose}"
    );
    // And the matcher's verdicts follow that same translation.
    assert_eq!(matches(&rpc, "*.lua", "init.lua").await, "true");
    assert_eq!(matches(&rpc, "*.lua", "a/init.lua").await, "false");
}

/// `basename` is the one option `to_regex` cannot honor: it reduces the CANDIDATE to
/// its last component before the regex runs, which is a step on the path rather than
/// anything the pattern can express (with `literal_separator = false` a `*` crosses
/// `/`, so no `(?:.*/)?` prefix would be faithful either). Since the whole point of
/// `to_regex` is to hand the translation to an engine that will not perform that
/// reduction, it raises rather than returning a regex that quietly means something
/// else — while `compile(...):regex()`, which introspects a glob that DOES apply the
/// reduction, still answers.
#[tokio::test]
async fn to_regex_refuses_a_basename_glob_instead_of_answering_wrong() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        // `\\n` joins nothing here: a bridge error carries a multi-line traceback, so
        // the parts are separated by a byte that cannot occur inside one.
        "local ok, err = pcall(btv.glob.to_regex, '*.lua', { basename = true })\n\
         -- a basename pattern only reduces when it has no separator: one that does\n\
         -- carry a `/` matches whole-path anyway, so it translates fine.\n\
         local with_sep = btv.glob.to_regex('a/*.lua', { basename = true })\n\
         -- and the introspection form still answers for the reducing pattern.\n\
         local introspect = btv.glob.compile('*.lua', { basename = true }):regex()\n\
         return table.concat({ tostring(ok), (tostring(err):gsub('\\n.*', '')),\n\
         \x20 with_sep, introspect }, '\\1')",
    )
    .await;
    let got = got.as_str().unwrap_or_default();
    let mut parts = got.splitn(4, '\u{1}');
    let (ok, err) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let (with_sep, introspect) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    assert_eq!(ok, "false", "a basename glob must not translate: {got:?}");
    assert!(
        err.contains("basename") && err.contains("*.lua"),
        "the refusal must name the option and the pattern, got {err:?}"
    );
    assert!(
        with_sep.contains(r"\.lua"),
        "a separator-carrying pattern does not reduce, so it must still translate: \
         {with_sep:?}"
    );
    assert!(
        introspect.contains(r"\.lua"),
        "`compile(..):regex()` introspects the glob's own regex and must still answer: \
         {introspect:?}"
    );
    // The matcher itself is unaffected — `basename` still works, it just isn't a regex.
    assert_eq!(
        eval(
            &rpc,
            "btv.glob.match('*.lua', 'a/b/c.lua', { basename = true })"
        )
        .await,
        "true"
    );
}

/// A wrong-typed path argument must name the function the CALLER wrote. `any` and
/// `filter` both work through a compiled set, so left to the bridge they would blame
/// `globset:test` — and `any` would blame `btv.glob.match` instead whenever it was
/// handed a single pattern string, i.e. two different names for one mistake. `filter`
/// additionally reports WHICH entry of the list is bad, which the bridge cannot know.
#[tokio::test]
async fn any_and_filter_name_themselves_on_a_bad_path_argument() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        "local _, list_err = pcall(btv.glob.any, { '*.rs' }, 7)\n\
         local _, str_err = pcall(btv.glob.any, '*.rs', 7)\n\
         local _, filt_err = pcall(btv.glob.filter, '*.rs', { 'a.rs', 'b.rs', 7 })\n\
         return table.concat({ (tostring(list_err):gsub('\\n.*', '')),\n\
         \x20 (tostring(str_err):gsub('\\n.*', '')),\n\
         \x20 (tostring(filt_err):gsub('\\n.*', '')) }, '\\1')",
    )
    .await;
    let got = got.as_str().unwrap_or_default();
    let mut parts = got.splitn(3, '\u{1}');
    let (list_err, str_err) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let filt_err = parts.next().unwrap_or("");
    for err in [list_err, str_err] {
        assert!(
            err.contains("btv.glob.any: path must be a string, got number"),
            "both branches of `any` must name `any` itself, got {err:?}"
        );
    }
    assert!(
        filt_err.contains("btv.glob.filter: paths[3] must be a string, got number"),
        "`filter` must name itself AND the offending index, got {filt_err:?}"
    );
    for err in [list_err, str_err, filt_err] {
        assert!(
            !err.contains("prelude/glob"),
            "the raise must be positioned at the caller, not at the prelude source, \
             got {err:?}"
        );
    }
}

/// A glob set tests a path against every pattern in one pass: `:test` for "any",
/// `:matches` for which (1-based, so it indexes the caller's own Lua list),
/// `:patterns` for the list as written.
#[tokio::test]
async fn a_glob_set_reports_which_patterns_matched() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        "local s = btv.glob.set({ '**/*.rs', '**/Cargo.toml', '*.tmp' })\n\
         local hits = s:matches('src/a/Cargo.toml')\n\
         return tostring(s:test('src/a/Cargo.toml'))\n\
         \x20 .. '|' .. table.concat(hits, ',')\n\
         \x20 .. '|' .. tostring(s:test('README.md'))\n\
         \x20 .. '|' .. table.concat(s:matches('x.rs'), ',')\n\
         \x20 .. '|' .. #s:patterns()",
    )
    .await;
    assert_eq!(
        got.as_str(),
        Some("true|2|false|1|3"),
        "Cargo.toml hits pattern 2 only; README.md nothing; `x.rs` hits pattern 1 \
         because a leading `**/` matches ZERO directories too"
    );
}

/// A leading `**/` matches **zero** or more directories, so `**/*.lua` covers a file
/// at the root as well as one nested arbitrarily deep. This is gitignore / ripgrep
/// behavior and the reason `**/x` is the right way to say "an `x` anywhere" without
/// also needing a bare `x` alternative.
#[tokio::test]
async fn a_leading_doublestar_matches_zero_directories_too() {
    let (rpc, _incoming) = start().await;
    assert_eq!(matches(&rpc, "**/*.lua", "init.lua").await, "true");
    assert_eq!(matches(&rpc, "**/*.lua", "a/init.lua").await, "true");
    assert_eq!(matches(&rpc, "**/*.lua", "a/b/c/init.lua").await, "true");
    assert_eq!(matches(&rpc, "**/init.lua", "init.lua").await, "true");
    // A `**` in the MIDDLE likewise spans zero components.
    assert_eq!(matches(&rpc, "src/**/x.rs", "src/x.rs").await, "true");
    assert_eq!(matches(&rpc, "src/**/x.rs", "src/a/b/x.rs").await, "true");
}

/// A set mixing basename and whole-path patterns must attribute each hit to the
/// pattern that actually wanted it — the two cannot share a single pass, so this is
/// where the set's per-pattern reduction earns its keep.
#[tokio::test]
async fn a_mixed_basename_set_attributes_each_hit_correctly() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        "local s = btv.glob.set({ '*.lua', 'src/*.rs' }, { basename = true })\n\
         return table.concat(s:matches('conf/init.lua'), ',')\n\
         \x20 .. '|' .. table.concat(s:matches('src/main.rs'), ',')\n\
         \x20 .. '|' .. table.concat(s:matches('deep/src/main.rs'), ',')",
    )
    .await;
    assert_eq!(
        got.as_str(),
        Some("1|2|"),
        "`*.lua` is separator-less so it matches the tail; `src/*.rs` has a \
         separator so it stays whole-path and must NOT match a nested `src/`"
    );
}

/// An invalid pattern anywhere in a set fails the whole set loudly, rather than
/// being silently dropped (a quietly-ignored entry in an ignore list is a bug that
/// surfaces much later).
#[tokio::test]
async fn an_invalid_pattern_fails_the_whole_set() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        "local ok, err = pcall(btv.glob.set, { '*.rs', '[z-a]' })\n\
         return tostring(ok) .. '|' .. tostring(err)",
    )
    .await;
    let got = got.as_str().unwrap_or_default();
    assert!(got.starts_with("false|"), "must raise: {got}");
    assert!(
        got.contains("[z-a]"),
        "the error must name the offending pattern: {got}"
    );
}

/// `btv.glob.any` takes either a single pattern string or a list, so a caller reading
/// "a glob or a list of globs" from user config need not branch.
#[tokio::test]
async fn any_accepts_a_single_pattern_or_a_list() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        "return tostring(btv.glob.any('*.rs', 'a.rs'))\n\
         \x20 .. '|' .. tostring(btv.glob.any('*.rs', 'a.lua'))\n\
         \x20 .. '|' .. tostring(btv.glob.any({ '*.rs', '*.toml' }, 'Cargo.toml'))\n\
         \x20 .. '|' .. tostring(btv.glob.any({ '*.rs', '*.toml' }, 'a.lua'))",
    )
    .await;
    assert_eq!(got.as_str(), Some("true|false|true|false"));
}

/// `btv.glob.filter` keeps the matching paths in their original order.
#[tokio::test]
async fn filter_keeps_matching_paths_in_order() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        "local paths = { 'z.rs', 'a.lua', 'm.rs', 'src/b.rs', 'k.toml' }\n\
         return table.concat(btv.glob.filter({ '*.rs', '*.toml' }, paths), ',')",
    )
    .await;
    assert_eq!(
        got.as_str(),
        Some("z.rs,m.rs,k.toml"),
        "original order preserved; `src/b.rs` excluded since `*` stops at `/`"
    );
}

/// `btv.glob.is_glob` is the canonical "is this a pattern or a plain path" predicate.
#[tokio::test]
async fn is_glob_detects_every_metacharacter() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        "local out = {}\n\
         for _, s in ipairs({ 'src/*.rs', 'a?.txt', '[ab].txt', '{a,b}.txt',\n\
         \x20 'src/lib.rs', '', 'plain-name' }) do\n\
         \x20 out[#out + 1] = tostring(btv.glob.is_glob(s))\n\
         end\n\
         return table.concat(out, ',')",
    )
    .await;
    assert_eq!(got.as_str(), Some("true,true,true,true,false,false,false"));
}

// ===== Non-UTF-8 candidates ================================================

/// A path is matched as **bytes**, not decoded text. A filename that is not valid
/// UTF-8 — latin-1 on disk, or arriving over the encoding seam — must match by its real
/// bytes rather than raise or be lossily rewritten; that byte-orientation is the whole
/// reason the engine compiles to `regex::bytes` instead of `regex`. Every entry point
/// has to honor it, so all four are exercised here.
#[tokio::test]
async fn a_non_utf8_path_matches_by_its_bytes() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        // `caf\xe9` is 'café' in latin-1: a lone 0xe9 is not a valid UTF-8 sequence.
        "local path = 'caf\\xe9/init.lua'\n\
         local g = btv.glob.compile('**/*.lua')\n\
         local s = btv.glob.set({ '**/*.lua', '**/*.rs' })\n\
         return table.concat({\n\
         \x20 tostring(btv.glob.match('**/*.lua', path)),\n\
         \x20 tostring(btv.glob.match('**/*.rs', path)),\n\
         \x20 tostring(g:test(path)),\n\
         \x20 tostring(s:test(path)),\n\
         \x20 table.concat(s:matches(path), '+'),\n\
         \x20 tostring(btv.glob.is_glob(path)),\n\
         \x20 -- The decisive pair: `?` is ONE byte, so it matches the lone 0xe9. Had\n\
         \x20 -- the path been lossily decoded first, `caf\\xe9` would have become\n\
         \x20 -- `caf` + U+FFFD — three bytes where one is expected — and `caf?` would\n\
         \x20 -- fail while `caf???` matched. The bytes reached the regex untouched.\n\
         \x20 tostring(btv.glob.match('caf?/*.lua', path)),\n\
         \x20 tostring(btv.glob.match('caf???/*.lua', path)),\n\
         \x20 tostring(btv.glob.match('cafe/*.lua', path)),\n\
         }, ',')",
    )
    .await;
    assert_eq!(
        got.as_str(),
        Some("true,false,true,true,1,false,true,false,false")
    );
}

/// The pattern, unlike the candidate, must be valid UTF-8 — globset's parser is
/// `&str`-based. It fails loud, naming the public function rather than the private
/// `btv._glob*` bridge behind it, as a wrong argument type does too.
#[tokio::test]
async fn a_non_utf8_pattern_and_a_wrong_type_both_fail_loud_by_public_name() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        "local out = {}\n\
         local function err(...)\n\
         \x20 local ok, e = pcall(...)\n\
         \x20 out[#out + 1] = tostring(ok) .. ':' .. tostring(e):gsub('\\n.*', '')\n\
         end\n\
         err(btv.glob.match, 'caf\\xe9*', 'x')\n\
         err(btv.glob.match, '*.lua', nil)\n\
         err(btv.glob.is_glob, nil)\n\
         err(btv.glob.compile, 42)\n\
         return table.concat(out, '\\n')",
    )
    .await;
    let got = got.as_str().unwrap_or_default();
    for expected in [
        "btv.glob.match: pattern must be valid UTF-8",
        "btv.glob.match: path must be a string, got nil",
        "btv.glob.is_glob: value must be a string, got nil",
        "btv.glob.compile: pattern must be a string, got integer",
    ] {
        assert!(got.contains(expected), "missing {expected:?} in:\n{got}");
    }
    // …and none of them leaks the private bridge it happens to sit behind. (Match the
    // bridge spellings exactly: `is_glob` is a PUBLIC name that contains `_glob`.)
    for bridge in [
        "_glob_match",
        "_glob_is_glob",
        "_glob_set",
        "_glob_to_regex",
    ] {
        assert!(!got.contains(bridge), "leaked {bridge:?} in:\n{got}");
    }
    assert!(!got.contains("bad argument"), "raw mlua message in:\n{got}");
}

// ===== The cache ===========================================================

/// The cache is keyed by the pattern AND every option that changes the emitted
/// regex — not by the pattern alone. Matching one pattern under several option sets
/// interleaved must give each set's own answer; a pattern-only key would serve the
/// first compile to all of them.
#[tokio::test]
async fn the_compiled_cache_is_keyed_by_the_options_not_just_the_pattern() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        "local p, path = '*.LUA', 'a/b/c.lua'\n\
         local out = {}\n\
         -- Interleaved deliberately: each call must consult its OWN cache entry,
         -- and repeats must return the same answer as their first occurrence.
         out[#out+1] = tostring(btv.glob.match(p, path))\n\
         out[#out+1] = tostring(btv.glob.match(p, path, { literal_separator = false }))\n\
         out[#out+1] = tostring(btv.glob.match(p, path, { ignorecase = true }))\n\
         out[#out+1] = tostring(btv.glob.match(p, path))\n\
         out[#out+1] = tostring(btv.glob.match(p, path, { basename = true, ignorecase = true }))\n\
         out[#out+1] = tostring(btv.glob.match(p, path, { literal_separator = false, ignorecase = true }))\n\
         return table.concat(out, ',')",
    )
    .await;
    assert_eq!(
        got.as_str(),
        // default: `*` stops at `/` and case matters      -> false
        // literal_separator=false, case still matters      -> false
        // ignorecase, but `*` still stops at `/`           -> false
        // default again (must not have been overwritten)   -> false
        // basename + ignorecase: tail `c.lua` vs `*.LUA`   -> true
        // crosses `/` + ignorecase                         -> true
        Some("false,false,false,false,true,true"),
        "each (pattern, opts) pair must get its own compiled regex"
    );
}

/// The cache is what makes repeated matching cheap: 20k one-shot matches of the same
/// pattern must complete fast, because only the first compiles. Without caching each
/// call re-parses the glob and rebuilds a regex, which is orders of magnitude
/// slower — the guard is generous enough not to flake under a loaded
/// `cargo test --workspace` but far below the uncached cost.
#[tokio::test]
async fn repeated_matching_reuses_the_compiled_regex() {
    let (rpc, _incoming) = start().await;
    let started = std::time::Instant::now();
    let got = exec_lua(
        &rpc,
        "local n = 0\n\
         for i = 1, 20000 do\n\
         \x20 if btv.glob.match('src/**/*.{rs,toml}', 'src/a/b/mod.rs') then n = n + 1 end\n\
         end\n\
         return n",
    )
    .await;
    let elapsed = started.elapsed();
    assert_eq!(got.as_u64(), Some(20000), "every iteration must match");
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "20k cached glob matches took {elapsed:?} — the compiled regex is not being reused"
    );
}

/// A compiled `btv.glob.compile` object and the one-shot `btv.glob.match` share the
/// same cache entry, so neither path is a second compile of the same glob. Observable
/// as: they always agree, including on the option-sensitive cases.
#[tokio::test]
async fn compile_and_the_one_shot_form_agree_on_every_option_set() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        "local cases = {\n\
         \x20 { '*.lua', 'a/b.lua', nil },\n\
         \x20 { '*.lua', 'a/b.lua', { literal_separator = false } },\n\
         \x20 { '*.lua', 'a/b.lua', { basename = true } },\n\
         \x20 { 'a\\\\*b', 'a*b', { style = 'unix' } },\n\
         \x20 { 'a\\\\*b', 'a*b', { style = 'windows' } },\n\
         \x20 { '*.LUA', 'a.lua', { ignorecase = true } },\n\
         \x20 { '*.LUA', 'a.lua', { ignorecase = false } },\n\
         \x20 { 'x.{lua,}', 'x.', { empty_alternates = true } },\n\
         \x20 { 'x.{lua,}', 'x.', { empty_alternates = false } },\n\
         }\n\
         for _, c in ipairs(cases) do\n\
         \x20 local one = btv.glob.match(c[1], c[2], c[3])\n\
         \x20 local obj = btv.glob.compile(c[1], c[3]):test(c[2])\n\
         \x20 if one ~= obj then\n\
         \x20   return 'disagree on ' .. c[1] .. ' vs ' .. c[2]\n\
         \x20 end\n\
         end\n\
         return 'ok'",
    )
    .await;
    assert_eq!(got.as_str(), Some("ok"));
}

// ===== The runtimepath lookup that rides the same engine ===================

/// `btv.runtime_file` (a.k.a. `nvim_get_runtime_file`) matches its final component
/// through this engine, so it speaks the full dialect — not the single `*` the
/// hand-rolled matcher it replaced supported. It also SORTS each directory listing, so
/// the `all = false` form returns a deterministic first hit rather than whatever order
/// the filesystem yielded; and a non-UTF-8 filename is matched by its bytes, not by a
/// lossy rendering that no `?` could line up with.
#[tokio::test]
async fn runtime_file_lookup_speaks_the_full_dialect_and_sorts() {
    let dir = bemtvi_test_harness::temp_dir("glob_rtp");
    let lsp = dir.join("lsp");
    std::fs::create_dir(&lsp).expect("create lsp dir");
    // `b1.lua` and `b[9-0].lua` are the pair that separates "a valid class is a class"
    // from "an uncompilable one is a literal name": `b[1].lua` must find the former,
    // and the reversed-range `b[9-0].lua` must find the latter, by its exact spelling.
    for name in [
        "zeta.lua",
        "alpha.lua",
        "mid.lua",
        "notes.txt",
        "b1.lua",
        "b[9-0].lua",
    ] {
        std::fs::write(lsp.join(name), "").expect("write");
    }
    // A latin-1 filename: one 0xe9 byte, which is not valid UTF-8. Skipped where the
    // platform has no byte filenames (there is nothing to test there).
    #[cfg(unix)]
    let non_utf8 = {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::OsStr::from_bytes(b"caf\xe9.lua");
        std::fs::write(lsp.join(name), "").expect("write latin-1 name");
        true
    };
    #[cfg(not(unix))]
    let non_utf8 = false;

    let (rpc, _incoming) = start_attached(
        ServerInit {
            runtimepath: vec![dir.clone()],
            ..Default::default()
        },
        80,
        24,
    )
    .await;

    // Full dialect: a brace list, a class, and two wildcards in one component — none
    // of which the one-`*` matcher this replaced could express.
    let got = exec_lua(
        &rpc,
        "local function tails(name, all)\n\
         \x20 local out = {}\n\
         \x20 for _, p in ipairs(btv.runtime_file(name, all)) do\n\
         \x20   out[#out + 1] = p:match('[^/]*$')\n\
         \x20 end\n\
         \x20 table.sort(out)\n\
         \x20 return table.concat(out, ',')\n\
         end\n\
         return table.concat({\n\
         \x20 'brace=' .. tails('lsp/{alpha,zeta}.lua', true),\n\
         \x20 'class=' .. tails('lsp/[am]*.lua', true),\n\
         \x20 'twostar=' .. tails('lsp/*i*.lua', true),\n\
         \x20 'ext=' .. tails('lsp/*.txt', true),\n\
         }, '\\n')",
    )
    .await;
    let got = got.as_str().unwrap_or_default();
    assert!(got.contains("brace=alpha.lua,zeta.lua"), "{got}");
    assert!(got.contains("class=alpha.lua,mid.lua"), "{got}");
    assert!(got.contains("twostar=mid.lua"), "{got}");
    assert!(got.contains("ext=notes.txt"), "{got}");

    // `all = false` picks the sorted-first entry, not a filesystem-order accident.
    let first = exec_lua(
        &rpc,
        "local hits = btv.runtime_file('lsp/*.lua', false)\n\
         return #hits .. ':' .. hits[1]:match('[^/]*$')",
    )
    .await;
    assert_eq!(
        first.as_str(),
        Some("1:alpha.lua"),
        "one hit, and it must be the alphabetically first"
    );

    // A metacharacter-carrying name that does NOT compile falls through to a LITERAL
    // lookup rather than a swallowed error returning nothing — a real file may contain
    // `[`, `?` or `{`. A valid class, by contrast, stays a class.
    let literal = exec_lua(
        &rpc,
        "local function tail(name)\n\
         \x20 local hits = btv.runtime_file(name, true)\n\
         \x20 return #hits .. '/' .. (hits[1] and hits[1]:match('[^/]*$') or '-')\n\
         end\n\
         return 'invalid=' .. tail('lsp/b[9-0].lua')\n\
         \x20 .. ' valid=' .. tail('lsp/b[1].lua')",
    )
    .await;
    assert_eq!(
        literal.as_str(),
        Some("invalid=1/b[9-0].lua valid=1/b1.lua"),
        "an uncompilable pattern must resolve as the literal filename it spells, while \
         a valid class must still be matched as a class"
    );

    if non_utf8 {
        // `?` is one BYTE: it lines up with the lone 0xe9 only because the name was
        // matched raw. Lossily decoded it would be `caf` + U+FFFD (three bytes), so
        // `caf?.lua` would miss and `caf???.lua` would hit instead.
        let bytes = exec_lua(
            &rpc,
            "return #btv.runtime_file('lsp/caf?.lua', true)\n\
             \x20 .. ':' .. #btv.runtime_file('lsp/caf???.lua', true)",
        )
        .await;
        assert_eq!(
            bytes.as_str(),
            Some("1:0"),
            "a non-UTF-8 filename must match by its real bytes, not a lossy rendering"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

// ── btv.glob.split ───────────────────────────────────────────────────────────

#[tokio::test]
async fn split_takes_a_comma_separated_line_without_breaking_brace_alternation() {
    // The splitter behind a "files to include" text box. Commas separate patterns —
    // except inside `{…}`, where they belong to a single glob's alternation. One
    // implementation, shared by the picker's Rust-side badge count and its Lua-side
    // filter, so the two can never disagree on what a pattern is.
    let (rpc, _incoming) = start().await;

    assert_eq!(
        eval(
            &rpc,
            r#"table.concat(btv.glob.split("src/**, docs/**"), "|")"#
        )
        .await,
        "src/**|docs/**"
    );
    assert_eq!(
        eval(
            &rpc,
            r#"table.concat(btv.glob.split("**/{node_modules,target}/**"), "|")"#
        )
        .await,
        "**/{node_modules,target}/**",
        "a brace alternation's commas are part of the pattern, not separators"
    );
    assert_eq!(
        eval(
            &rpc,
            r#"table.concat(btv.glob.split("  , *.lock ,, a/b "), "|")"#
        )
        .await,
        "*.lock|a/b",
        "entries are trimmed and blanks dropped"
    );
    assert_eq!(
        eval(&rpc, r#"#btv.glob.split("")"#).await,
        "0",
        "an empty line is no patterns at all"
    );
}
