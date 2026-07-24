//! Standalone behavior tests for the general-purpose `nx.utils.*` helpers promoted
//! out of per-module copies (the nx.utils rule: broadly-useful utilities are public,
//! documented, and tested standalone, then consumed by features). Black-box per the
//! project conventions — a real server over RPC, asserting via `nvim_exec_lua`.
//! (`nx.utils.debounce` is covered in async_runtime.rs, with the timer machinery;
//! `nx.utils.caller_source`'s `@`-named sourced-file path is locked by expand.rs's
//! `<sfile>` tests, which run through it.)

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, start_attached};
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
        ("nx.utils.dirname('/a/b/c.txt')", "/a/b"),
        ("nx.utils.dirname('/a')", ""),
        ("nx.utils.dirname('rel')", "rel"), // no `/`: nothing to strip
        ("nx.utils.basename('/a/b/c.txt')", "c.txt"),
        ("nx.utils.basename('/a/b/')", "b"),
        ("nx.utils.basename('/')", "nil"),
        ("nx.utils.basename('owner/repo')", "repo"),
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

    assert_eq!(eval_str(&rpc, "nx.utils.expanduser('~')").await, home);
    assert_eq!(
        eval_str(&rpc, "nx.utils.expanduser('~/x/y')").await,
        format!("{home}/x/y")
    );
    // `~user` is not resolved and a mid-path `~` is a literal component.
    assert_eq!(
        eval_str(&rpc, "nx.utils.expanduser('~nobody/x')").await,
        "~nobody/x"
    );
    assert_eq!(
        eval_str(&rpc, "nx.utils.expanduser('/a/~/b')").await,
        "/a/~/b"
    );
}

// ancestors iterates dirname(path) upward, nearest first, and never produces the
// root / empty string; a relative path ends at its first component.
#[tokio::test]
async fn ancestors_walks_upward_nearest_first() {
    let (rpc, _incoming) = start().await;
    let walk = |p: &str| {
        format!(
            "local out = {{}}\n\
             for dir in nx.utils.ancestors('{p}') do out[#out + 1] = dir end\n\
             return table.concat(out, '|')"
        )
    };
    let abs = exec_lua(&rpc, &walk("/a/b/c.txt")).await;
    assert_eq!(abs.as_str(), Some("/a/b|/a"), "absolute path walk");

    let rel = exec_lua(&rpc, &walk("a/b/c.txt")).await;
    assert_eq!(rel.as_str(), Some("a/b|a"), "relative path walk");

    let top = exec_lua(&rpc, &walk("/top.txt")).await;
    assert_eq!(
        top.as_str(),
        Some(""),
        "a file directly under the root has no ancestors"
    );
}

// argv flattens `{ cmd = string|list, args = list }` — the two spellings the run
// family documents as equivalent produce the same argv.
#[tokio::test]
async fn argv_flattens_cmd_and_args_equivalently() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        "local a = table.concat(nx.utils.argv({ cmd = 'git', args = { 'log', '-1' } }), ' ')\n\
         local b = table.concat(nx.utils.argv({ cmd = { 'git', 'log', '-1' } }), ' ')\n\
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
        "local one = nx.utils.str_list('<Tab>', 'w')\n\
         local many = nx.utils.str_list({ '<Tab>', '<CR>' }, 'w')\n\
         local none = nx.utils.str_list(nil, 'w')\n\
         return #one .. '|' .. #many .. '|' .. #none",
    )
    .await;
    assert_eq!(ok.as_str(), Some("1|2|0"));

    let err = exec_lua(
        &rpc,
        "local ok, e = pcall(nx.utils.str_list, 42, 'nx.foo.setup: keys')\n\
         return tostring(ok) .. '|' .. tostring(e)",
    )
    .await;
    let s = err.as_str().unwrap_or("");
    assert!(
        s.starts_with("false|"),
        "a non-string spec must raise, got: {s}"
    );
    assert!(
        s.contains("nx.foo.setup: keys"),
        "the error must name the option, got: {s}"
    );

    let err_elem = exec_lua(
        &rpc,
        "local ok, e = pcall(nx.utils.str_list, { '<Tab>', 7 }, 'nx.foo.setup: keys')\n\
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
// `<sfile>` tests, which run through nx.utils.caller_source.)
