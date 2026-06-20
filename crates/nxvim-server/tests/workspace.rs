//! The `--shada-namespace` plumbing — validating the namespace token and redirecting
//! the shada store into its private `ns/<NS>/` subfolder, isolated from the global
//! store. nxvim itself reads no project file; the namespace always arrives on the CLI.

use nxvim_server::{valid_namespace, workspace_shada};
use nxvim_test_harness::{serial_lock, temp_dir};

#[test]
fn valid_namespace_accepts_safe_tokens_and_rejects_the_rest() {
    assert_eq!(
        valid_namespace("abc-123_DEF"),
        Some("abc-123_DEF".to_string())
    );
    assert_eq!(valid_namespace("5f2c0e1a"), Some("5f2c0e1a".to_string()));
    // path-traversal / separators must never become a directory component.
    assert_eq!(valid_namespace("../../etc"), None);
    assert_eq!(valid_namespace("a/b"), None);
    assert_eq!(valid_namespace("a.b"), None);
    assert_eq!(valid_namespace(""), None);
    assert_eq!(valid_namespace(&"x".repeat(200)), None);
}

#[tokio::test]
async fn store_redirects_into_the_namespace_subfolder() {
    let _g = serial_lock().lock().await;
    let state = temp_dir("ws-state");
    let prev = std::env::var_os("XDG_STATE_HOME");
    std::env::set_var("XDG_STATE_HOME", &state);

    // With a namespace, the store lives under shada/ns/<ns>/ …
    let ns = "session-ns-1";
    let mut store = workspace_shada(Some(ns));
    store.load().expect("namespaced store loads");
    let ns_dir = state.join("nxvim").join("shada").join("ns").join(ns);
    let has_redb = std::fs::read_dir(&ns_dir)
        .map(|rd| {
            rd.flatten()
                .any(|e| e.path().extension().map(|x| x == "redb").unwrap_or(false))
        })
        .unwrap_or(false);
    assert!(has_redb, "expected a .redb under {}", ns_dir.display());

    // … and the global store (no namespace) writes a top-level .redb instead.
    let mut global = workspace_shada(None);
    global.load().expect("global store loads");
    let global_dir = state.join("nxvim").join("shada");
    let global_files: Vec<_> = std::fs::read_dir(&global_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().map(|x| x == "redb").unwrap_or(false))
        .collect();
    assert!(
        !global_files.is_empty(),
        "global store wrote a top-level .redb"
    );

    match prev {
        Some(v) => std::env::set_var("XDG_STATE_HOME", v),
        None => std::env::remove_var("XDG_STATE_HOME"),
    }
}
