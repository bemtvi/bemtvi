//! Behavior tests for the `btv.utils` path helpers (`joinpath` / `normalize` /
//! `relpath`) and `btv.fs.which` — the pure path math and the async executable lookup
//! that together replace vim's `vim.fs.*` / blocking `executable()` for configs and
//! plugins (docs/plans/2026-07-29-nvim-lspconfig-native-port.md, Phase 1).
//!
//! Black-box per the project conventions: a real server over RPC, driven with
//! `nvim_exec_lua`. The path helpers are synchronous, so they assert directly;
//! `btv.fs.which` settles OFF the editor tick (like every `btv.fs` op), so those tests
//! queue the chain and poll the global it sets.

use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{exec_lua, start_attached, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// A `return`-style chunk's string result.
async fn lua_str(rpc: &Rpc, code: &str) -> Option<String> {
    exec_lua(rpc, code).await.as_str().map(str::to_owned)
}

/// Poll a `return`-style chunk until it yields a non-nil value (~3s). An off-tick
/// `btv.fs` op settles on a later tick, so the global its chain sets is nil until the
/// loop processes the actor's result.
async fn poll_settled(rpc: &Rpc, code: &str) -> rmpv::Value {
    for _ in 0..150 {
        let v = exec_lua(rpc, code).await;
        if !matches!(v, rmpv::Value::Nil) {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    exec_lua(rpc, code).await
}

// ----- btv.utils.joinpath ------------------------------------------------------

#[tokio::test]
async fn joinpath_collapses_separators_at_every_seam() {
    let (rpc, _incoming) = start().await;
    // The shape every LSP config writes: root + a relative subpath + a program name.
    let joined = lua_str(
        &rpc,
        r#"return btv.utils.joinpath("/a/b", "node_modules/.bin", "eslint")"#,
    )
    .await;
    assert_eq!(joined.as_deref(), Some("/a/b/node_modules/.bin/eslint"));

    // Redundant separators on either side of a seam collapse to exactly one — the
    // whole reason this isn't string concatenation.
    let messy = lua_str(&rpc, r#"return btv.utils.joinpath("/a/", "/b/", "c")"#).await;
    assert_eq!(messy.as_deref(), Some("/a/b/c"));

    // A later absolute-looking component APPENDS; it does not restart at the root.
    // (Only the first component decides absolute-vs-relative.)
    let rooted = lua_str(&rpc, r#"return btv.utils.joinpath("/a", "/b")"#).await;
    assert_eq!(rooted.as_deref(), Some("/a/b"));

    // A relative first component stays relative.
    let rel = lua_str(&rpc, r#"return btv.utils.joinpath("a", "b")"#).await;
    assert_eq!(rel.as_deref(), Some("a/b"));
}

#[tokio::test]
async fn joinpath_skips_empty_components_and_handles_the_root() {
    let (rpc, _incoming) = start().await;
    // A conditionally-absent middle segment must not leave a doubled slash behind —
    // configs build these from optional config values.
    let gap = lua_str(&rpc, r#"return btv.utils.joinpath("/a", "", "c")"#).await;
    assert_eq!(gap.as_deref(), Some("/a/c"));

    let nil_gap = lua_str(&rpc, r#"return btv.utils.joinpath("/a", nil, "c")"#).await;
    assert_eq!(nil_gap.as_deref(), Some("/a/c"));

    // The root is the one first component whose trailing separator IS the path.
    let from_root = lua_str(&rpc, r#"return btv.utils.joinpath("/", "usr", "bin")"#).await;
    assert_eq!(from_root.as_deref(), Some("/usr/bin"));

    let nothing = lua_str(&rpc, r#"return btv.utils.joinpath()"#).await;
    assert_eq!(nothing.as_deref(), Some(""));
}

#[tokio::test]
async fn joinpath_rejects_a_non_string_component() {
    let (rpc, _incoming) = start().await;
    // Fail loud: a number silently stringified would produce a plausible-looking
    // wrong path rather than an error at the point of the mistake.
    let err = lua_str(
        &rpc,
        r#"local ok, e = pcall(btv.utils.joinpath, "/a", 7) return tostring(e)"#,
    )
    .await
    .unwrap_or_default();
    assert!(
        err.contains("must be a string"),
        "a non-string component must raise, got {err:?}"
    );
}

// ----- btv.utils.normalize -----------------------------------------------------

#[tokio::test]
async fn normalize_resolves_dot_and_dotdot_lexically() {
    let (rpc, _incoming) = start().await;
    let cases = [
        (r#""/a/b/../c""#, "/a/c"),
        (r#""/a/./b""#, "/a/b"),
        (r#""/a//b///c""#, "/a/b/c"),
        (r#""/a/b/""#, "/a/b"),
        // `..` past the root is dropped, as on every OS.
        (r#""/../a""#, "/a"),
        // Backslashes fold to `/`.
        (r#""/a\\b""#, "/a/b"),
        // A relative `..` with nothing to cancel is KEPT — there is no cwd here to
        // resolve it against, so dropping it would silently change the meaning.
        (r#""a/../../b""#, "../b"),
        (r#""a/b/../.""#, "a"),
    ];
    for (input, want) in cases {
        let got = lua_str(&rpc, &format!("return btv.utils.normalize({input})")).await;
        assert_eq!(got.as_deref(), Some(want), "normalize({input})");
    }
}

#[tokio::test]
async fn normalize_expands_a_leading_tilde() {
    let (rpc, _incoming) = start().await;
    let home = std::env::var("HOME").expect("HOME is set in the test environment");
    let got = lua_str(&rpc, r#"return btv.utils.normalize("~/work/x")"#).await;
    assert_eq!(got, Some(format!("{home}/work/x")));
}

// ----- btv.utils.relpath -------------------------------------------------------

#[tokio::test]
async fn relpath_compares_whole_components_not_string_prefixes() {
    let (rpc, _incoming) = start().await;
    let inside = lua_str(&rpc, r#"return btv.utils.relpath("/a/b", "/a/b/c/d")"#).await;
    assert_eq!(inside.as_deref(), Some("c/d"));

    let same = lua_str(&rpc, r#"return btv.utils.relpath("/a/b", "/a/b")"#).await;
    assert_eq!(same.as_deref(), Some("."));

    // The bug a plain prefix-match has: `/a/bc` is NOT inside `/a/b`.
    let sibling = lua_str(&rpc, r#"return btv.utils.relpath("/a/b", "/a/bc")"#).await;
    assert_eq!(
        sibling, None,
        "a sibling sharing a name prefix is not inside"
    );

    let outside = lua_str(&rpc, r#"return btv.utils.relpath("/a/b", "/x/y")"#).await;
    assert_eq!(outside, None);

    // Everything is inside the root.
    let from_root = lua_str(&rpc, r#"return btv.utils.relpath("/", "/a/b")"#).await;
    assert_eq!(from_root.as_deref(), Some("a/b"));
}

// ----- btv.fs.which ------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn which_finds_an_executable_on_path_and_answers_nil_for_a_missing_one() {
    let (rpc, _incoming) = start().await;
    // `sh` is on `$PATH` on every POSIX system this suite runs on.
    exec_lua(
        &rpc,
        r#"btv.async(function() _G.sh = btv.await(btv.fs.which("sh")) or false end)()"#,
    )
    .await;
    let found = poll_settled(&rpc, "return _G.sh").await;
    let found = found.as_str().unwrap_or_default().to_string();
    assert!(
        found.ends_with("/sh"),
        "which('sh') should resolve an absolute path, got {found:?}"
    );

    // A name that cannot exist resolves nil — "not installed" is a true answer, so
    // the promise must FULFIL with nil rather than reject (which is what lets a
    // config write `btv.await(btv.fs.which(x)) or fallback`).
    exec_lua(
        &rpc,
        r#"btv.async(function()
             local hit = btv.await(btv.fs.which("bemtvi-no-such-program-xyzzy"))
             _G.missing = (hit == nil) and "nil" or tostring(hit)
           end)()"#,
    )
    .await;
    let missing = poll_settled(&rpc, "return _G.missing").await;
    assert_eq!(
        missing.as_str(),
        Some("nil"),
        "an absent program resolves nil, and must not reject"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn which_accepts_an_explicit_path_only_when_it_is_executable() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("fs-which");
    let (rpc, _incoming) = start().await;

    // An explicit path with the executable bit — the `node_modules/.bin` case every
    // ported LSP config uses to prefer a project-local binary.
    let exe = dir.as_path().join("tool");
    std::fs::write(&exe, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

    // The same file without the bit: present, but not runnable — so `which` must say
    // no. Answering yes here would have the config spawn something that can't run.
    let plain = dir.as_path().join("notes.txt");
    std::fs::write(&plain, "not a program\n").unwrap();

    exec_lua(
        &rpc,
        &format!(
            r#"btv.async(function()
                 _G.exe = btv.await(btv.fs.which({exe})) or false
                 _G.plain = btv.await(btv.fs.which({plain})) == nil and "nil" or "found"
               end)()"#,
            exe = lua_quote(&exe.to_string_lossy()),
            plain = lua_quote(&plain.to_string_lossy()),
        ),
    )
    .await;

    let got = poll_settled(&rpc, "return _G.exe").await;
    assert_eq!(got.as_str(), Some(exe.to_string_lossy().as_ref()));

    let got = poll_settled(&rpc, "return _G.plain").await;
    assert_eq!(
        got.as_str(),
        Some("nil"),
        "a non-executable file must not resolve as a program"
    );
}

/// A Lua single-quoted string literal for `path` (test paths have no quotes).
fn lua_quote(s: &str) -> String {
    format!("'{s}'")
}
