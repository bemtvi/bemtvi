//! Black-box tests for `nx.regex` — the compiled pattern object for matching Lua
//! strings (`:find` / `:match` / `:gmatch` / `:gsub` / `:test`). Driven over RPC;
//! everything runs inside the VM, so each case is a small Lua program whose result
//! is returned as a string for one assertion. Default engine is the Rust `regex`
//! crate (pcre); `engine = "vim"` and `plain = true` are exercised too.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, feed, start_attached, write_temp};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn server() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Run a Lua chunk and return its string result (panics on any other type).
async fn run(rpc: &Rpc, code: &str) -> String {
    match exec_lua(rpc, code).await {
        Value::String(s) => s.into_str().unwrap_or_default(),
        other => panic!("expected a string result, got {other:?}"),
    }
}

#[tokio::test]
async fn find_returns_one_based_inclusive_offsets() {
    let (rpc, _inc) = server().await;
    // `s:sub(re:find(s))` must reproduce the match — the 1-based, inclusive-end
    // contract that mirrors string.find.
    let out = run(
        &rpc,
        r#"local re = nx.regex([[\d+]])
           local s = "abc 123 xyz"
           local a, b = re:find(s)
           return a..":"..b..":"..s:sub(a, b)"#,
    )
    .await;
    assert_eq!(out, "5:7:123");
}

#[tokio::test]
async fn find_no_match_returns_nil() {
    let (rpc, _inc) = server().await;
    let out = run(
        &rpc,
        r#"local re = nx.regex([[\d+]])
           return tostring(re:find("no digits here"))"#,
    )
    .await;
    assert_eq!(out, "nil");
}

#[tokio::test]
async fn find_reports_captures_after_offsets() {
    let (rpc, _inc) = server().await;
    let out = run(
        &rpc,
        r#"local re = nx.regex([[(\w+)@(\w+)]])
           local a, b, user, host = re:find("mail jo@acme end")
           return a..":"..b..":"..user..":"..host"#,
    )
    .await;
    assert_eq!(out, "6:12:jo:acme");
}

#[tokio::test]
async fn find_honours_init() {
    let (rpc, _inc) = server().await;
    // init past the first match jumps to the second.
    let out = run(
        &rpc,
        r#"local re = nx.regex([[\d+]])
           local s = "1 22 333"
           local a, b = re:find(s, 3)
           return a..":"..b..":"..s:sub(a,b)"#,
    )
    .await;
    assert_eq!(out, "3:4:22");
}

#[tokio::test]
async fn find_with_init_sees_matches_overlapping_an_earlier_one() {
    let (rpc, _inc) = server().await;
    // string.find("aaaa", "aa", 2) is 2:3 — the scan restarts at init, it does not
    // reuse the non-overlapping match set computed from the string start (which
    // would skip 2:3 and land on 3:4).
    let out = run(
        &rpc,
        r#"local re = nx.regex([[aa]])
           local a, b = re:find("aaaa", 2)
           return a..":"..b"#,
    )
    .await;
    assert_eq!(out, "2:3");
}

#[tokio::test]
async fn find_plain_with_mid_char_init_matches_like_string_find() {
    let (rpc, _inc) = server().await;
    // "€" is 3 bytes; init = 3 points inside it. string.find("€b", "b", 3) is 4:4 —
    // a byte-offset init inside a multi-byte char must not error, it just can't
    // start a match there.
    let out = run(
        &rpc,
        r#"local re = nx.regex("b", { plain = true })
           local a, b = re:find("\u{20AC}b", 3)
           return a..":"..b"#,
    )
    .await;
    assert_eq!(out, "4:4");
}

#[tokio::test]
async fn match_returns_captures_or_whole() {
    let (rpc, _inc) = server().await;
    let out = run(
        &rpc,
        r#"local whole = nx.regex([[\d+]]):match("x 42 y")
           local g = nx.regex([[(\w+)=(\w+)]]):match("k=v")
           local k, v = nx.regex([[(\w+)=(\w+)]]):match("k=v")
           return whole..":"..k..":"..v"#,
    )
    .await;
    assert_eq!(out, "42:k:v");
}

#[tokio::test]
async fn gmatch_iterates_all_matches() {
    let (rpc, _inc) = server().await;
    let out = run(
        &rpc,
        r#"local re = nx.regex([[\d+]])
           local t = {}
           for n in re:gmatch("1 22 333 x") do t[#t+1] = n end
           return table.concat(t, ",")"#,
    )
    .await;
    assert_eq!(out, "1,22,333");
}

#[tokio::test]
async fn gmatch_yields_multiple_captures() {
    let (rpc, _inc) = server().await;
    let out = run(
        &rpc,
        r#"local re = nx.regex([[(\w+):(\d+)]])
           local t = {}
           for name, num in re:gmatch("a:1 b:2") do t[#t+1] = name.."="..num end
           return table.concat(t, ",")"#,
    )
    .await;
    assert_eq!(out, "a=1,b=2");
}

#[tokio::test]
async fn gsub_string_replacement_with_capture_refs() {
    let (rpc, _inc) = server().await;
    let out = run(
        &rpc,
        r#"local re = nx.regex([[(\w+)@(\w+)]])
           local s, n = re:gsub("a@b and c@d", "%2.%1")
           return s..":"..n"#,
    )
    .await;
    assert_eq!(out, "b.a and d.c:2");
}

#[tokio::test]
async fn gsub_function_replacement_and_limit() {
    let (rpc, _inc) = server().await;
    // The function upper-cases; n = 1 stops after the first replacement.
    let out = run(
        &rpc,
        r#"local re = nx.regex([[\w+]])
           local s, n = re:gsub("foo bar baz", function(w) return w:upper() end, 1)
           return s..":"..n"#,
    )
    .await;
    assert_eq!(out, "FOO bar baz:1");
}

#[tokio::test]
async fn gsub_function_nil_keeps_original() {
    let (rpc, _inc) = server().await;
    // Returning nil from the function keeps that match unchanged (Lua semantics).
    let out = run(
        &rpc,
        r#"local re = nx.regex([[\w+]])
           local s = re:gsub("keep drop keep", function(w)
             if w == "drop" then return "X" end
           end)
           return s"#,
    )
    .await;
    assert_eq!(out, "keep X keep");
}

#[tokio::test]
async fn test_is_boolean_match() {
    let (rpc, _inc) = server().await;
    let out = run(
        &rpc,
        r#"local re = nx.regex([[^\d+$]])
           return tostring(re:test("123"))..":"..tostring(re:test("12a"))"#,
    )
    .await;
    assert_eq!(out, "true:false");
}

#[tokio::test]
async fn vim_engine_supports_zs() {
    let (rpc, _inc) = server().await;
    // The vim engine brings `\zs` (set match start) — proof the engine selector works.
    let out = run(
        &rpc,
        r#"local re = nx.regex([[foo\zsbar]], { engine = "vim" })
           local s = "foobar"
           local a, b = re:find(s)
           return s:sub(a, b)"#,
    )
    .await;
    assert_eq!(out, "bar");
}

#[tokio::test]
async fn plain_treats_pattern_literally() {
    let (rpc, _inc) = server().await;
    let out = run(
        &rpc,
        r#"local re = nx.regex("a.c", { plain = true })
           return tostring(re:test("a.c"))..":"..tostring(re:test("abc"))"#,
    )
    .await;
    assert_eq!(out, "true:false");
}

#[tokio::test]
async fn ignorecase_option() {
    let (rpc, _inc) = server().await;
    let out = run(
        &rpc,
        r#"local re = nx.regex([[foo]], { ignorecase = true })
           return tostring(re:test("FOO"))"#,
    )
    .await;
    assert_eq!(out, "true");
}

/// The shipped `examples/regex/` config must load and every command must run its
/// full body over the shipped sample buffer without error — not just parse. Mirrors
/// the project's "verified end-to-end" example convention.
#[tokio::test]
async fn shipped_example_loads_and_runs_on_the_sample() {
    let sample = include_str!("../../../examples/regex/sample.txt");
    let path = write_temp("regex_example", "txt", sample);
    let init = ServerInit {
        file: Some(path),
        ..Default::default()
    };
    let (rpc, _inc) = start_attached(init, 80, 24).await;

    // Load the example config verbatim — defines :Emails / :Numbers / :Phones / :Redact.
    // Run it as an explicit chunk: `nvim_exec_lua` evaluates in expression-mode-first
    // (mlua prepends `return `), which misparses a chunk that opens with a comment, so
    // we `load()` it as a statement block instead.
    let init_lua = include_str!("../../../examples/regex/init.lua");
    let loaded = run(
        &rpc,
        &format!(
            "assert(load([==[\n{init_lua}\n]==], '@examples/regex/init.lua'))()\nreturn 'loaded'"
        ),
    )
    .await;
    assert_eq!(loaded, "loaded");

    // The extraction is correct over the real buffer: both addresses, captures
    // intact (the substance of :Emails, asserted on values notify can't return).
    let emails = run(
        &rpc,
        r#"local re = nx.regex([[([\w.+-]+)@([\w-]+\.[\w.-]+)]])
           local out = {}
           for _, line in ipairs(nx.buf.lines(0, 0, -1)) do
             for user, host in re:gmatch(line) do out[#out + 1] = user .. "@" .. host end
           end
           return table.concat(out, ",")"#,
    )
    .await;
    assert_eq!(emails, "jane.doe@acme.io,j.smith+work@mail.example.com");

    // Every command runs its full body (gmatch / gsub / test / match + notify) over
    // the real sample without raising. :Redact reads the cursor, so park it on a line.
    feed(&rpc, "9G");
    let ran = run(
        &rpc,
        r#"local out = "ok"
           for _, c in ipairs({ "Emails", "Numbers", "Phones", "Redact" }) do
             local ok, err = pcall(vim.cmd, c)
             if not ok then out = c .. ": " .. tostring(err) break end
           end
           return out"#,
    )
    .await;
    assert_eq!(ran, "ok");
}

#[tokio::test]
async fn invalid_pattern_raises() {
    let (rpc, _inc) = server().await;
    let out = run(
        &rpc,
        r#"local ok, err = pcall(nx.regex, "(unterminated")
           err = tostring(err)
           return tostring(ok)..":"..(err:match("invalid pcre") and "named" or err)"#,
    )
    .await;
    assert_eq!(out, "false:named");
}
