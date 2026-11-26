//! Standalone behavior tests for the general-purpose `btv.utils.*` helpers promoted
//! out of per-module copies (the btv.utils rule: broadly-useful utilities are public,
//! documented, and tested standalone, then consumed by features). Black-box per the
//! project conventions — a real server over RPC, asserting via `nvim_exec_lua`.
//! (`btv.utils.debounce` is covered in async_runtime.rs, with the timer machinery;
//! `btv.utils.caller_source`'s `@`-named sourced-file path is locked by expand.rs's
//! `<sfile>` tests, which run through it.)

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{exec_lua, start_attached};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Run `expr` (a Lua expression) and return its tostring()'d value — keeps the
/// per-case assertions one-liners and makes nil/booleans printable.
async fn eval_str(rpc: &Rpc, expr: &str) -> String {
    let got = exec_lua(rpc, &format!("return tostring({expr})")).await;
    got.as_str().unwrap_or_default().to_string()
}

// dirname/basename are pure string math: last-component stripping / extraction,
// with the documented edges (root, trailing separators, no-separator input).
#[tokio::test]
async fn dirname_and_basename_cover_the_documented_edges() {
    let (rpc, _incoming) = start().await;

    let cases = [
        ("btv.utils.dirname('/a/b/c.txt')", "/a/b"),
        ("btv.utils.dirname('/a')", ""),
        ("btv.utils.dirname('rel')", "rel"), // no `/`: nothing to strip
        ("btv.utils.basename('/a/b/c.txt')", "c.txt"),
        ("btv.utils.basename('/a/b/')", "b"),
        ("btv.utils.basename('/')", "nil"),
        ("btv.utils.basename('owner/repo')", "repo"),
    ];
    for (expr, want) in cases {
        assert_eq!(eval_str(&rpc, expr).await, want, "{expr}");
    }
}

// expanduser touches only a LEADING `~` / `~/`; `~user` and mid-path tildes stay
// literal (the strictest of the three per-module copies this replaced).
#[tokio::test]
async fn expanduser_expands_only_the_leading_tilde() {
    let (rpc, _incoming) = start().await;
    let home = std::env::var("HOME").expect("HOME set in dev/CI env");

    assert_eq!(eval_str(&rpc, "btv.utils.expanduser('~')").await, home);
    assert_eq!(
        eval_str(&rpc, "btv.utils.expanduser('~/x/y')").await,
        format!("{home}/x/y")
    );
    // `~user` is not resolved and a mid-path `~` is a literal component.
    assert_eq!(
        eval_str(&rpc, "btv.utils.expanduser('~nobody/x')").await,
        "~nobody/x"
    );
    assert_eq!(
        eval_str(&rpc, "btv.utils.expanduser('/a/~/b')").await,
        "/a/~/b"
    );
}

// ancestors iterates dirname(path) upward, nearest first, and never produces the
// root / empty string; a *relative* path is first resolved against the editor's
// cwd, so the walk reaches the real project tree rather than stopping at the first
// typed component (the `.editorconfig` / LSP-root discovery case: a buffer's name
// is the path as typed, so `bemtvi src/main.rs` hands in a relative one).
#[tokio::test]
async fn ancestors_walks_upward_nearest_first() {
    let (rpc, _incoming) = start().await;
    let walk = |p: &str| {
        format!(
            "local out = {{}}\n\
             for dir in btv.utils.ancestors('{p}') do out[#out + 1] = dir end\n\
             return table.concat(out, '|')"
        )
    };
    // A native absolute path per host: on Windows that is drive-rooted, and the walk
    // reports native `\`-separated directories (a `/`-rooted path there is
    // drive-relative, not absolute). The `\\\\` is a Lua-escaped `\\` inside `walk`'s
    // single-quoted literal.
    let (abs_path, abs_walk) = if cfg!(windows) {
        ("C:\\\\a\\\\b\\\\c.txt", "C:\\a\\b|C:\\a")
    } else {
        ("/a/b/c.txt", "/a/b|/a")
    };
    let abs = exec_lua(&rpc, &walk(abs_path)).await;
    assert_eq!(abs.as_str(), Some(abs_walk), "absolute path walk");

    // The relative walk starts at `<cwd>/a/b` and keeps climbing *through* the cwd
    // into its own ancestors — an absolute chain, not the two bare components.
    let rel = exec_lua(&rpc, &walk("a/b/c.txt")).await;
    let cwd = eval_str(&rpc, "vim.fn.getcwd()").await;
    let dirs: Vec<String> = rel
        .as_str()
        .unwrap_or_default()
        .split('|')
        .map(str::to_string)
        .collect();
    let sep = std::path::MAIN_SEPARATOR;
    assert_eq!(
        dirs.iter().take(3).collect::<Vec<_>>(),
        vec![&format!("{cwd}{sep}a{sep}b"), &format!("{cwd}{sep}a"), &cwd],
        "a relative path resolves against the cwd before walking"
    );

    let top_path = if cfg!(windows) {
        "C:\\\\top.txt"
    } else {
        "/top.txt"
    };
    let top = exec_lua(&rpc, &walk(top_path)).await;
    assert_eq!(
        top.as_str(),
        Some(""),
        "a file directly under the root has no ancestors"
    );
}

// The path-style primitives every path helper reads its platform rules from: under the
// windows style a drive / UNC share / bare separator is a root and both separators
// split; under the unix style only a leading `/` roots and `\` is an ordinary byte.
// `btv.fname.modify` and the `file://` conversion are built on them.
#[tokio::test]
async fn path_style_primitives_follow_the_host() {
    let (rpc, _incoming) = start().await;
    let cases: &[(&str, &str)] = if cfg!(windows) {
        &[
            ("btv.utils.path_style()", "windows"),
            ("btv.utils.path_sep()", r"\"),
            (r"btv.utils.path_root([[C:\a]])", r"C:\"),
            ("btv.utils.path_root('C:/a')", "C:/"),
            (r"btv.utils.path_root([[\\srv\share\x]])", r"\\srv\share\"),
            (r"btv.utils.is_absolute([[\a]])", "true"),
            (r"btv.utils.is_absolute([[a\b]])", "false"),
            (r"btv.utils.dirname([[C:\a\b]])", r"C:\a"),
            (r"btv.utils.dirname([[C:\a]])", ""),
            (
                r"btv.fname.modify([[C:/x/./y\z.txt]], ':p')",
                r"C:\x\y\z.txt",
            ),
            (r"btv.fname.modify([[C:\x\y.txt]], ':t')", "y.txt"),
            (r"btv.fname.modify([[C:\x]], ':h')", r"C:\"),
            (
                r"btv.utils.path_join([[C:\a]], 'b/c', 'd.txt')",
                r"C:\a\b\c\d.txt",
            ),
            (r"btv.utils.path_join([[C:\a\]], [[\b]])", r"C:\a\b"),
            (r"btv.utils.path_join('C:/a', nil, '', 'b')", r"C:\a\b"),
            (r"btv.utils.path_join([[C:\]], 'a')", r"C:\a"),
            (r"btv.utils.local_path_join([[C:\a]], 'b/c')", r"C:\a\b\c"),
            (
                r"btv.utils.uri_from_path([[\\srv\share\a b]])",
                "file://srv/share/a%20b",
            ),
            (
                "btv.utils.uri_to_path('file://srv/share/a%20b')",
                r"\\srv\share\a b",
            ),
        ]
    } else {
        &[
            ("btv.utils.path_style()", "unix"),
            ("btv.utils.path_sep()", "/"),
            ("btv.utils.path_root('/a')", "/"),
            (r"btv.utils.path_root([[C:\a]])", ""),
            (r"btv.utils.is_absolute([[\a]])", "false"),
            ("btv.utils.dirname('/a/b')", "/a"),
            (r"btv.fname.modify([[/x/a\b.txt]], ':t')", r"a\b.txt"),
            ("btv.utils.path_join('/a', 'b/c', 'd.txt')", "/a/b/c/d.txt"),
            ("btv.utils.path_join('/a/', '/b')", "/a/b"),
            ("btv.utils.path_join('/a', nil, '', 'b')", "/a/b"),
            ("btv.utils.path_join('/', 'a')", "/a"),
            (r"btv.utils.path_join('/a', [[b\c]])", r"/a/b\c"),
            ("btv.utils.local_path_join('/a', 'b')", "/a/b"),
        ]
    };
    for (expr, want) in cases {
        assert_eq!(eval_str(&rpc, expr).await, *want, "{expr}");
    }
}

// argv flattens `{ cmd = string|list, args = list }` — the two spellings the run
// family documents as equivalent produce the same argv.
#[tokio::test]
async fn argv_flattens_cmd_and_args_equivalently() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        "local a = table.concat(btv.utils.argv({ cmd = 'git', args = { 'log', '-1' } }), ' ')\n\
         local b = table.concat(btv.utils.argv({ cmd = { 'git', 'log', '-1' } }), ' ')\n\
         return a .. '=' .. b",
    )
    .await;
    assert_eq!(got.as_str(), Some("git log -1=git log -1"));
}

// str_list normalizes string|string[]|nil and fails loud on anything else, naming
// the option (`what`) in the message.
#[tokio::test]
async fn str_list_normalizes_and_fails_loud() {
    let (rpc, _incoming) = start().await;

    let ok = exec_lua(
        &rpc,
        "local one = btv.utils.str_list('<Tab>', 'w')\n\
         local many = btv.utils.str_list({ '<Tab>', '<CR>' }, 'w')\n\
         local none = btv.utils.str_list(nil, 'w')\n\
         return #one .. '|' .. #many .. '|' .. #none",
    )
    .await;
    assert_eq!(ok.as_str(), Some("1|2|0"));

    let err = exec_lua(
        &rpc,
        "local ok, e = pcall(btv.utils.str_list, 42, 'btv.foo.setup: keys')\n\
         return tostring(ok) .. '|' .. tostring(e)",
    )
    .await;
    let s = err.as_str().unwrap_or("");
    assert!(
        s.starts_with("false|"),
        "a non-string spec must raise, got: {s}"
    );
    assert!(
        s.contains("btv.foo.setup: keys"),
        "the error must name the option, got: {s}"
    );

    let err_elem = exec_lua(
        &rpc,
        "local ok, e = pcall(btv.utils.str_list, { '<Tab>', 7 }, 'btv.foo.setup: keys')\n\
         return tostring(ok) .. '|' .. tostring(e)",
    )
    .await;
    let s = err_elem.as_str().unwrap_or("");
    assert!(
        s.starts_with("false|") && s.contains("string(s)"),
        "a non-string element must raise, got: {s}"
    );
}

// (No direct caller_source test here: what it reports for a bare RPC chunk is
// incidental — an exec_lua chunk is `@`-named after its Rust load site, but a
// tail call can elide that frame entirely — and no caller depends on it: the
// namespace attribution treats an off-runtimepath path and nil identically. The
// meaningful contract, resolving a sourced user file, is locked by expand.rs's
// `<sfile>` tests, which run through btv.utils.caller_source.)

// The pure-Lua UTF-8 walkers behind `btv.str.chars` / `to_list` / `displaywidth` /
// `charpart` document that a malformed sequence counts as ONE 1-byte character, so a
// Latin-1 byte (`\xe9`, "é") followed by ASCII must not swallow the ASCII bytes after
// it as fake continuation bytes.
#[tokio::test]
async fn utf8_helpers_treat_a_malformed_lead_byte_as_one_char() {
    let (rpc, _incoming) = start().await;
    assert_eq!(eval_str(&rpc, r#"btv.str.chars("\xe9t\xe9")"#).await, "3");
    assert_eq!(
        eval_str(&rpc, r#"table.concat(btv.str.to_list("\xe9ab"), ",")"#).await,
        "233,97,98"
    );
    assert_eq!(
        eval_str(&rpc, r#"btv.str.displaywidth("\xf0abc")"#).await,
        "4"
    );
    assert_eq!(
        eval_str(&rpc, r#"btv.str.charpart("\xe9ab", 1, 1)"#).await,
        "a"
    );
    // Well-formed multibyte text still decodes as whole characters.
    assert_eq!(eval_str(&rpc, r#"btv.str.chars("héllo日本")"#).await, "7");
    assert_eq!(
        eval_str(&rpc, r#"table.concat(btv.str.to_list("é日😀"), ",")"#).await,
        "233,26085,128512"
    );
}

// `btv.notify_once` shows a given message only the first time, like neovim's — a plugin
// that warns from a per-keystroke path through it must not flood the message line.
#[tokio::test]
async fn notify_once_shows_each_message_only_once() {
    let (rpc, _incoming) = start().await;
    let got = eval_str(
        &rpc,
        "(function() \
           local n = 0 \
           btv.notify = function() n = n + 1 end \
           local a = btv.notify_once('deprecated: foo') \
           local b = btv.notify_once('deprecated: foo') \
           btv.notify_once('deprecated: bar') \
           return n .. ',' .. tostring(a) .. ',' .. tostring(b) \
         end)()",
    )
    .await;
    assert_eq!(got, "2,true,false");
}

// A `file://` URI always names an ABSOLUTE path: a buffer's name is the path as typed
// (`bemtvi src/main.rs`), and `file://src/main.rs` reads as host `src`, path `/main.rs`
// to a language server, so a relative path resolves against the cwd first.
#[tokio::test]
async fn uri_from_path_resolves_a_relative_path_against_the_cwd() {
    let (rpc, _incoming) = start().await;
    let cwd = eval_str(&rpc, "vim.fn.getcwd()").await;
    // The resolved path in the platform's own spelling (`D:\…\src\main.rs` on
    // Windows), and its URI as a server expects it (`file:///D:/…/src/main.rs`).
    let abs = std::path::Path::new(&cwd).join("src").join("main.rs");
    assert_eq!(
        eval_str(&rpc, "btv.utils.uri_from_path('src/main.rs')").await,
        bemtvi_test_harness::file_uri(&abs)
    );
    assert_eq!(
        eval_str(
            &rpc,
            "btv.utils.uri_to_path(btv.utils.uri_from_path('src/main.rs'))"
        )
        .await,
        abs.to_string_lossy()
    );
    assert_eq!(
        eval_str(&rpc, "btv.utils.uri_from_path('/abs/x.rs')").await,
        "file:///abs/x.rs"
    );
}

// `btv.str.displaywidth` measures as the screen paints: tabs on the current buffer's
// `'tabstop'` (vim's `strdisplaywidth`), and a control character at the width of the
// `^X` it is drawn as. It used to assume a tabstop of 8 and count a control character
// as one cell, so a plugin lining text up around either drew it misaligned.
#[tokio::test]
async fn displaywidth_measures_as_the_screen_paints() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, "btv.cmd('set tabstop=4')").await;
    assert_eq!(eval_str(&rpc, r#"btv.str.displaywidth("a\tb")"#).await, "5");
    // A tab from column 2 runs to the next stop, 4.
    assert_eq!(
        eval_str(&rpc, r#"btv.str.displaywidth("\t", 2)"#).await,
        "2"
    );
    // NUL paints as `^@`, DEL as `^?`, a C1 control as `<xx>`.
    assert_eq!(eval_str(&rpc, r#"btv.str.displaywidth("a\0b")"#).await, "4");
    assert_eq!(eval_str(&rpc, r#"btv.str.displaywidth("\127")"#).await, "2");
    assert_eq!(
        eval_str(&rpc, r#"btv.str.displaywidth("\u{85}")"#).await,
        "4"
    );
    // Wide and combining characters are unchanged.
    assert_eq!(
        eval_str(&rpc, r#"btv.str.displaywidth("日e\u{301}")"#).await,
        "3"
    );
    // A negative or fractional starting column is clamped and floored, as vim does,
    // rather than raising.
    assert_eq!(
        eval_str(&rpc, r#"btv.str.displaywidth("\t", -3)"#).await,
        "4"
    );
    assert_eq!(
        eval_str(&rpc, r#"btv.str.displaywidth("\t", 2.5)"#).await,
        "2"
    );
    // Another buffer's tab stops, named.
    assert_eq!(
        eval_str(&rpc, r#"btv.str.displaywidth("a\tb", 0, 2)"#).await,
        "3"
    );
    exec_lua(&rpc, "btv.cmd('set tabstop=8')").await;
    assert_eq!(eval_str(&rpc, r#"btv.str.displaywidth("a\tb")"#).await, "9");
}

// `btv.json.encode` writes an object's keys sorted, so a file a plugin keeps (the
// `btv.plugins` lockfile) diffs cleanly. A dependency turning on serde_json's
// `preserve_order` made objects keep insertion order, which for a Lua table is its hash
// order, different from one VM to the next: the lockfile came out shuffled.
#[tokio::test]
async fn json_encode_sorts_object_keys() {
    let (rpc, _incoming) = start().await;
    let got = eval_str(
        &rpc,
        r#"(function()
          local t = {}
          for i = 50, 1, -1 do t[("k%02d"):format(i)] = i end
          local keys = {}
          for k in btv.json.encode(t):gmatch('"(k%d%d)"') do keys[#keys + 1] = k end
          for i = 2, #keys do
            if keys[i - 1] > keys[i] then return "unsorted at " .. keys[i] end
          end
          local marked = btv.json.encode(btv.json.empty_object())
          return #keys .. " " .. marked
        end)()"#,
    )
    .await;
    assert_eq!(got, "50 {}");
    // Pretty output too, and nested objects.
    let nested = eval_str(
        &rpc,
        r#"btv.json.encode({ z = { b = 1, a = 2 }, a = 1 }, { pretty = true })"#,
    )
    .await;
    assert!(
        nested.find("\"a\": 1") < nested.find("\"z\"")
            && nested.find("\"a\": 2") < nested.find("\"b\""),
        "{nested}"
    );
}

// `btv.str.grapheme_floor` / `grapheme_next` expose the core's grapheme-cluster
// boundaries (the ones the cursor and selections move by), so a plugin that cuts a
// string, such as one measuring a huge cell a piece at a time, never splits a ZWJ emoji,
// a flag or a letter from its combining accent. Offsets are 0-based bytes.
#[tokio::test]
async fn grapheme_boundaries_follow_the_core() {
    let (rpc, _incoming) = start().await;
    // A family emoji: three 4-byte emoji joined by two 3-byte ZWJs, 18 bytes, one
    // grapheme. Any offset inside it floors to 1 and steps on to 19.
    let fam = r#""a\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}b""#;
    for at in [1, 5, 8, 12, 18] {
        assert_eq!(
            eval_str(&rpc, &format!("btv.str.grapheme_floor({fam}, {at})")).await,
            "1",
            "floor of {at}"
        );
    }
    assert_eq!(
        eval_str(&rpc, &format!("btv.str.grapheme_next({fam}, 5)")).await,
        "19"
    );
    assert_eq!(
        eval_str(&rpc, &format!("btv.str.grapheme_next({fam}, 0)")).await,
        "1"
    );
    assert_eq!(
        eval_str(&rpc, &format!("btv.str.grapheme_floor({fam}, 19)")).await,
        "19"
    );
    // A flag is two regional indicators (4 bytes each); `e` + U+0301 is one letter.
    assert_eq!(
        eval_str(&rpc, r#"btv.str.grapheme_floor("\u{1F1E7}\u{1F1F7}", 4)"#).await,
        "0"
    );
    assert_eq!(
        eval_str(&rpc, r#"btv.str.grapheme_next("e\u{301}x", 0)"#).await,
        "3"
    );
    // Offsets clamp into the string; the end is a boundary with nothing after it.
    assert_eq!(
        eval_str(&rpc, r#"btv.str.grapheme_floor("abc", -2)"#).await,
        "0"
    );
    assert_eq!(
        eval_str(&rpc, r#"btv.str.grapheme_floor("abc", 9)"#).await,
        "3"
    );
    assert_eq!(
        eval_str(&rpc, r#"btv.str.grapheme_next("abc", 3)"#).await,
        "3"
    );
    assert_eq!(eval_str(&rpc, r#"btv.str.grapheme_next("", 0)"#).await, "0");
    // A malformed byte is one character of its own, as in the other `btv.str`
    // helpers, and the offsets around it stay the string's own bytes.
    assert_eq!(
        eval_str(&rpc, r#"btv.str.grapheme_next("\xe9e\u{301}", 0)"#).await,
        "1"
    );
    assert_eq!(
        eval_str(&rpc, r#"btv.str.grapheme_floor("\xe9e\u{301}", 3)"#).await,
        "1"
    );
    assert_eq!(
        eval_str(&rpc, r#"btv.str.grapheme_next("\xe9e\u{301}", 1)"#).await,
        "4"
    );
    // A truncated 4-byte character (3 bytes, one U+FFFD on screen) is one character.
    assert_eq!(
        eval_str(&rpc, r#"btv.str.grapheme_floor("\xf0\x9f\x98x", 2)"#).await,
        "0"
    );
    assert_eq!(
        eval_str(&rpc, r#"btv.str.grapheme_next("\xf0\x9f\x98x", 1)"#).await,
        "3"
    );
}

// `btv.str.chars(s, skipcc)` (vim's `strchars`) leaves composing characters out of the
// count. It used to ignore `skipcc` and count every codepoint. Here a character is a
// grapheme cluster, the unit the cursor moves by, so a ZWJ emoji or a flag counts once
// too. A `skipcc` of `0` or `false` is vim's "count them", as is leaving it out.
#[tokio::test]
async fn chars_skipcc_counts_graphemes() {
    let (rpc, _incoming) = start().await;
    let s = r#""e\u{301}\u{1F468}\u{200D}\u{1F469}\u{1F1E7}\u{1F1F7}x""#;
    assert_eq!(eval_str(&rpc, &format!("btv.str.chars({s})")).await, "8");
    assert_eq!(eval_str(&rpc, &format!("btv.str.chars({s}, 0)")).await, "8");
    assert_eq!(
        eval_str(&rpc, &format!("btv.str.chars({s}, false)")).await,
        "8"
    );
    assert_eq!(
        eval_str(&rpc, &format!("btv.str.chars({s}, true)")).await,
        "4"
    );
    assert_eq!(
        eval_str(&rpc, &format!("vim.fn.strchars({s}, 1)")).await,
        "4"
    );
    // A malformed byte still counts as one.
    assert_eq!(
        eval_str(&rpc, r#"btv.str.chars("\xe9e\u{301}", true)"#).await,
        "2"
    );
    assert_eq!(
        eval_str(&rpc, r#"btv.str.chars("\xf0\x9f\x98x", true)"#).await,
        "2"
    );
}
