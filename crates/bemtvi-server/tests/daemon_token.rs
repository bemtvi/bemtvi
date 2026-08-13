//! Where the daemon's bearer token is allowed to travel.
//!
//! The token is the daemon's *only* credential, and a daemon spawns processes and
//! reads and writes files on its host — so a leaked token is remote code execution
//! on that machine. It used to ride in the connect URI:
//!
//! ```text
//! bemtvi --connect-daemon 'bemtvi://127.0.0.1:8765/<TOKEN>?cert=<HASH>'
//! ```
//!
//! which puts it in **argv**. On Linux `/proc/<pid>/cmdline` is world-readable, so
//! every other local user can read the credential straight off a running editor —
//! no exploit, just `ps`. `/proc/<pid>/environ`, by contrast, is readable only by
//! the process's own owner, so the token moves to `$BEMTVI_DAEMON_TOKEN`.
//!
//! The URI keeps parsing a `/TOKEN` path, because the browser client genuinely
//! cannot use the env var — a web page has no shell environment to inherit — and
//! its paste string is the one place the token still has to be in the URI.
//!
//! `parse_connect_uri` is the single point every native dialer (the TUI binary and
//! the GUI) goes through, so it is where the policy is observable. It reads a
//! process-global env var, hence `serial_lock`.

use bemtvi_server::{parse_connect_uri, DAEMON_TOKEN_ENV};
use bemtvi_test_harness::serial_lock;

const CERT: &str = "aa:bb:cc";

/// Run `body` with `$BEMTVI_DAEMON_TOKEN` set to `value` (or unset for `None`),
/// restoring whatever was there before.
fn with_token_env<T>(value: Option<&str>, body: impl FnOnce() -> T) -> T {
    let previous = std::env::var(DAEMON_TOKEN_ENV).ok();
    // SAFETY: every caller holds `serial_lock`, so no other test races this.
    match value {
        Some(v) => std::env::set_var(DAEMON_TOKEN_ENV, v),
        None => std::env::remove_var(DAEMON_TOKEN_ENV),
    }
    let out = body();
    match previous {
        Some(v) => std::env::set_var(DAEMON_TOKEN_ENV, v),
        None => std::env::remove_var(DAEMON_TOKEN_ENV),
    }
    out
}

/// The form the daemon now prints for a native connect: no token in the URI, the
/// token on the environment. This is the whole point — the string that lands in
/// argv, shell history, a log, or a reconnect config no longer carries the
/// credential.
#[tokio::test]
async fn a_tokenless_uri_takes_its_token_from_the_environment() {
    let _guard = serial_lock().lock().await;
    let (url, cert, token) = with_token_env(Some("s3cret-from-env"), || {
        parse_connect_uri(&format!("bemtvi://127.0.0.1:8765?cert={CERT}")).expect("should parse")
    });
    assert_eq!(url, "https://127.0.0.1:8765");
    assert_eq!(cert, CERT);
    assert_eq!(token, "s3cret-from-env");
}

/// A tokenless URI with nothing on the environment fails **loud**. The one
/// outcome that must never happen is dialing anyway: a dial with an empty or
/// absent credential is an unauthenticated connection attempt, and "it didn't
/// work" is a much worse diagnosis than "you didn't set the token".
#[tokio::test]
async fn a_tokenless_uri_with_no_env_token_refuses_to_dial() {
    let _guard = serial_lock().lock().await;
    let err = with_token_env(None, || {
        parse_connect_uri(&format!("bemtvi://127.0.0.1:8765?cert={CERT}"))
            .expect_err("a credential-less dial must not be attempted")
    });
    let msg = err.to_string();
    assert!(
        msg.contains(DAEMON_TOKEN_ENV),
        "the error should name the env var the user has to set, got {msg:?}"
    );
}

/// The browser's paste string — `bemtvi://HOST:PORT/TOKEN?cert=HASH` — still
/// parses. A web page has no environment to read the token from, so this form
/// cannot simply be retired, and the native path must keep understanding it.
#[tokio::test]
async fn the_legacy_token_in_path_form_still_parses() {
    let _guard = serial_lock().lock().await;
    let (url, cert, token) = with_token_env(None, || {
        parse_connect_uri(&format!("bemtvi://127.0.0.1:8765/path-token?cert={CERT}"))
            .expect("the legacy form must still dial")
    });
    assert_eq!(url, "https://127.0.0.1:8765");
    assert_eq!(cert, CERT);
    assert_eq!(token, "path-token");
}

/// An explicit token in the path wins over the ambient environment: what the user
/// typed is more specific than what their shell happened to be carrying, and a
/// stale exported token silently overriding a pasted URI would be baffling.
#[tokio::test]
async fn an_explicit_path_token_beats_the_environment() {
    let _guard = serial_lock().lock().await;
    let (_url, _cert, token) = with_token_env(Some("from-env"), || {
        parse_connect_uri(&format!("bemtvi://127.0.0.1:8765/from-path?cert={CERT}"))
            .expect("should parse")
    });
    assert_eq!(token, "from-path");
}

/// Every malformed shape stays a loud failure rather than a half-specified dial.
/// (An empty `/` path is *not* an empty token — it falls through to the env var,
/// which is the tokenless form with a trailing slash.)
#[tokio::test]
async fn malformed_uris_fail_loud() {
    let _guard = serial_lock().lock().await;
    with_token_env(Some("env-token"), || {
        for uri in [
            "http://127.0.0.1:8765/tok?cert=aa",   // wrong scheme
            "bemtvi://127.0.0.1:8765/tok",         // no cert query
            "bemtvi://127.0.0.1:8765/tok?cert=",   // empty cert
            "bemtvi://127.0.0.1:8765/tok?other=1", // no cert key
            "bemtvi://?cert=aa",                   // no host:port
        ] {
            assert!(
                parse_connect_uri(uri).is_err(),
                "{uri:?} is not a complete dial target and must be refused"
            );
        }
        // A trailing slash with no token is the tokenless form: the env var supplies it.
        let (_u, _c, token) =
            parse_connect_uri(&format!("bemtvi://127.0.0.1:8765/?cert={CERT}")).expect("parses");
        assert_eq!(token, "env-token");
    });
}
