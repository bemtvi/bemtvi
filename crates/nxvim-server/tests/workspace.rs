//! The `--shada-namespace` plumbing — validating the namespace token and redirecting
//! the shada store into its private `ns/<NS>/` subfolder, isolated from the global
//! store. nxvim itself reads no project file; the namespace always arrives on the CLI.
//!
//! The `--workspace` convenience (a directory session) layers on top: it derives the
//! namespace from the directory via [`workspace_namespace`] and exposes the workspace
//! root to Lua through `nx.workspace`. Those two surfaces are covered here too.

use nxvim_server::{valid_namespace, workspace_namespace, workspace_shada, ServerInit};
use nxvim_test_harness::{exec_lua, serial_lock, start_attached, temp_dir};
use std::path::Path;

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

#[test]
fn workspace_namespace_folds_separators_and_other_chars() {
    // Path separators become `-`; alphanumerics and `_` survive.
    assert_eq!(
        workspace_namespace(Path::new("/home/ada/proj")),
        "-home-ada-proj"
    );
    assert_eq!(workspace_namespace(Path::new("/srv/my_app")), "-srv-my_app");
    // Dots, spaces, and anything else outside the token set also fold to `-`.
    assert_eq!(workspace_namespace(Path::new("/a b/c.d")), "-a-b-c-d");
    // The result is always a valid `--shada-namespace` token — the whole point is that
    // `--workspace` is just a derived `--shada-namespace`, no special-casing downstream.
    assert!(valid_namespace(&workspace_namespace(Path::new("/x/y"))).is_some());
}

#[test]
fn workspace_namespace_keeps_the_complete_path_untruncated() {
    // The full path is preserved (never bounded/hashed) so the `ns/<…>` directory reads as
    // the path it came from — a user who moves a project can rename the dir to match.
    let long = format!("/{}", "segment/".repeat(40)); // well over 128 chars
    let ns = workspace_namespace(Path::new(&long));
    assert!(
        ns.len() > 128,
        "the complete long path is not truncated: {}",
        ns.len()
    );
    // Exactly the folded full path: every `/` is a `-`, every other char survives.
    assert_eq!(ns, long.replace('/', "-"));
    // Deterministic: the same directory always maps to the same store.
    assert_eq!(ns, workspace_namespace(Path::new(&long)));
    // Distinct paths map to distinct namespaces (no collisions from truncation).
    let other = format!("/{}", "segment/".repeat(41));
    assert_ne!(ns, workspace_namespace(Path::new(&other)));
}

#[tokio::test]
async fn nx_workspace_reflects_the_seeded_identity() {
    // `nx.workspace.dir()` / `.active()` and `nx.shada.namespace()` read the identity the
    // server seeds from `ServerInit` (NOT an env var) — this is what lets a *daemon* session
    // surface a root derived from the daemon's cwd, resolved only after it connects.
    let init = ServerInit {
        shada_namespace: Some("-srv-ada-proj".to_string()),
        workspace_dir: Some("/srv/ada/proj".to_string()),
        ..Default::default()
    };
    let (rpc, _incoming) = start_attached(init, 80, 24).await;
    assert_eq!(
        exec_lua(&rpc, "return nx.workspace.dir()").await.as_str(),
        Some("/srv/ada/proj"),
    );
    assert_eq!(
        exec_lua(&rpc, "return nx.workspace.active()")
            .await
            .as_bool(),
        Some(true),
    );
    assert_eq!(
        exec_lua(&rpc, "return nx.shada.namespace()").await.as_str(),
        Some("-srv-ada-proj"),
    );

    // A default (non-workspace) launch reports no workspace and a nil namespace.
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    assert_eq!(
        exec_lua(&rpc, "return nx.workspace.active()")
            .await
            .as_bool(),
        Some(false),
    );
    assert!(exec_lua(&rpc, "return nx.shada.namespace()").await.is_nil());
}
