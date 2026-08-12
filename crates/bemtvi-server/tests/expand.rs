//! Behavior tests for `vim.fn.expand` (`btv.expand`) — specifically the `<sfile>` /
//! `<script>` script-path keywords plugins use to locate their own install root
//! (the `expand("<sfile>:p:h:h")` idiom), plus the fail-loud contract for `<...>`
//! tokens bemtvi doesn't model. Black-box over RPC per the project conventions: a
//! real server sources an `init.lua` from disk (so the chunk carries an `@<path>`
//! name, exactly like a real user config), then we read back what it computed.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{attach, exec_lua, spawn, start_attached, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Source `init_lua` from a throwaway config dir; return a connected client. The
/// init file is sourced with an `@<path>` chunk name (`source_init`), which is what
/// makes `<sfile>` resolvable — the same path a real user config takes.
async fn start_with_init(
    dir: &std::path::Path,
    init_lua: &str,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(dir.join("init.lua"), init_lua).expect("write init.lua");
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

// `<sfile>` resolves to the path of the file being sourced, and a `:mods` suffix
// routes through fnamemodify — so `<sfile>:p:h:h` is the script's grandparent dir,
// the canonical "find my own install root" idiom.
#[tokio::test]
async fn sfile_resolves_to_the_sourced_script_and_mods_apply() {
    let cfg = temp_dir("expand_sfile");
    let (rpc, _incoming) = start_with_init(
        &cfg,
        "_G.self = vim.fn.expand('<sfile>')\n\
         _G.root = vim.fn.expand('<sfile>:p:h:h')\n\
         _G.scriptalias = vim.fn.expand('<script>')",
    )
    .await;

    let got_self = exec_lua(&rpc, "return _G.self").await;
    let got_root = exec_lua(&rpc, "return _G.root").await;
    let got_alias = exec_lua(&rpc, "return _G.scriptalias").await;

    let expected_self = cfg.join("init.lua").to_string_lossy().into_owned();
    // <cfg>/init.lua  -> :h <cfg>  -> :h parent(<cfg>)
    let expected_root = cfg.parent().unwrap().to_string_lossy().into_owned();

    assert_eq!(
        got_self.as_str(),
        Some(expected_self.as_str()),
        "expand('<sfile>') should be the sourced file's path"
    );
    assert_eq!(
        got_root.as_str(),
        Some(expected_root.as_str()),
        "expand('<sfile>:p:h:h') should be the script root (grandparent dir)"
    );
    assert_eq!(
        got_alias.as_str(),
        Some(expected_self.as_str()),
        "expand('<script>') should alias <sfile> for the sourced file"
    );
}

// Outside any sourced file (a bare `:lua` / RPC call has no `@<path>` chunk on the
// stack), `<sfile>` is the empty string — matching neovim, not a bogus path.
#[tokio::test]
async fn sfile_outside_a_script_is_empty() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(&rpc, "return vim.fn.expand('<sfile>')").await;
    assert_eq!(
        got.as_str(),
        Some(""),
        "expand('<sfile>') with no sourced file on the stack is empty"
    );
}

// An angle-bracket keyword bemtvi doesn't model fails loud, rather than silently
// returning the literal text as a bogus "path" (the old passthrough behavior).
#[tokio::test]
async fn unknown_angle_token_fails_loud() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        "local ok, e = pcall(vim.fn.expand, '<afile>:p')\n\
         return tostring(ok) .. '|' .. tostring(e)",
    )
    .await;
    let s = got.as_str().unwrap_or("");
    assert!(
        s.starts_with("false|"),
        "expand('<afile>') should raise, got: {s}"
    );
    assert!(
        s.contains("unsupported"),
        "the error should name the unsupported keyword, got: {s}"
    );
}

// Plain paths are untouched by the fail-loud rule: `~` and `$VAR` still expand and
// the remainder passes through verbatim.
#[tokio::test]
async fn plain_path_still_expands_env() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(&rpc, "return vim.fn.expand('$HOME/x/y')").await;
    let home = std::env::var("HOME").expect("HOME set in dev/CI env");
    let want = format!("{home}/x/y");
    assert_eq!(
        got.as_str(),
        Some(want.as_str()),
        "a plain $VAR path must still expand, not fail loud"
    );

    // Only a LEADING `~` / `~/` expands (the shared btv.utils.expanduser edge):
    // a `~user` form is not resolved — vim leaves an unknown user's `~user`
    // literal too — and a mid-path `~` is an ordinary path component.
    let tilde = exec_lua(&rpc, "return vim.fn.expand('~/x')").await;
    assert_eq!(tilde.as_str(), Some(format!("{home}/x").as_str()));
    let user = exec_lua(&rpc, "return vim.fn.expand('~nobody/x')").await;
    assert_eq!(
        user.as_str(),
        Some("~nobody/x"),
        "an unresolvable ~user path stays literal, not $HOME-mangled"
    );
}

// Both env-var spellings expand — bare `$VAR` and the `${VAR}` brace form vim also
// accepts — including mid-string; an unset var is left verbatim in either form.
#[tokio::test]
async fn env_var_brace_and_bare_forms_expand() {
    // A custom var proves it's a generic env lookup, not a HOME special-case.
    // SAFETY: set on the test thread before the server starts; no concurrent readers.
    unsafe { std::env::set_var("BEMTVI_EXPAND_PROBE", "/zzz") };
    let (rpc, _incoming) = start().await;

    let cases = [
        ("$BEMTVI_EXPAND_PROBE/bar", "/zzz/bar"),
        ("${BEMTVI_EXPAND_PROBE}/bar", "/zzz/bar"),
        ("pre/${BEMTVI_EXPAND_PROBE}/post", "pre//zzz/post"),
        (
            "${BEMTVI_DEFINITELY_UNSET}/bar",
            "${BEMTVI_DEFINITELY_UNSET}/bar",
        ),
    ];
    for (expr, want) in cases {
        let got = exec_lua(&rpc, &format!("return vim.fn.expand('{expr}')")).await;
        assert_eq!(got.as_str(), Some(want), "expand('{expr}')");
    }
}

// vim.fn.fnameescape (alias of btv.fname.escape) backslash-escapes the cmdline-magic
// characters so a path can be fed straight to `:edit`, and guards a leading `>`/`+`
// and a lone `-`. Cases match real neovim. Inputs are passed through a level-2 long
// bracket (`[==[ ]==]`) so backslashes/quotes/`]]` in the path stay literal; `\n`/`\t`
// are written as real bytes via string.char in a separate case.
#[tokio::test]
async fn fnameescape_escapes_cmdline_magic() {
    let (rpc, _incoming) = start().await;

    let cases = [
        // input              -> escaped
        ("foo bar", r"foo\ bar"),
        ("a*b?c[d]", r"a\*b\?c\[d]"),
        ("a{b`c$d", r"a\{b\`c\$d"),
        (r"a\b%c#d", r"a\\b\%c\#d"),
        ("a'b\"c|d", "a\\'b\\\"c\\|d"),
        ("a!b<c", r"a\!b\<c"),
        (">foo", r"\>foo"),
        ("+foo", r"\+foo"),
        ("-", r"\-"),
        // a leading `-` that isn't the whole name is left alone
        ("-foo", "-foo"),
        ("plain/path.txt", "plain/path.txt"),
    ];
    for (input, want) in cases {
        let lua = format!("return vim.fn.fnameescape([==[{input}]==])");
        let got = exec_lua(&rpc, &lua).await;
        assert_eq!(got.as_str(), Some(want), "fnameescape({input:?})");
    }

    // Whitespace bytes (tab, newline) get escaped too — built via string.char so the
    // Lua source carries no literal control characters.
    let got = exec_lua(
        &rpc,
        "return vim.fn.fnameescape('a' .. string.char(9) .. 'b' .. string.char(10) .. 'c')",
    )
    .await;
    assert_eq!(
        got.as_str(),
        Some("a\\\tb\\\nc"),
        "fnameescape(tab/newline)"
    );
}
