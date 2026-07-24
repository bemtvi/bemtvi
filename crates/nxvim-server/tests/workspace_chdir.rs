//! `--workspace DIR` startup: the server cds into the workspace root at **boot** — before
//! the editor opens the startup file, seeds its `DirState`, or restores the session. The cd
//! is a CLI decision (`ServerInit::workspace_cwd`, from the `--workspace` / `--workspace-no-cwd`
//! flags), not the old `'workspacecwd'` Lua option, so the cwd is correct from the first
//! instruction: a relative startup file and the session's relative buffer paths just resolve
//! against the workspace root, with no late-cd path reconciliation.
//!
//! This binary mutates the **process** cwd (the workspace cd is a real `chdir`), so it holds
//! the process-wide [`serial_lock`] for each test body and restores the cwd on the way out —
//! and lives on its own so it can't perturb cwd-reading tests in other suites (the same
//! isolation `chdir.rs` uses).

use std::path::PathBuf;

use nxvim_server::{RedbFileStore, ServerInit};
use nxvim_test_harness::{
    await_lines, await_server_exit, buf_name, exec_lua, feed, serial_lock, start_attached, temp_dir,
};

/// Restore the process cwd on drop, even if the test panics mid-way.
struct CwdGuard(PathBuf);
impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

async fn getcwd(rpc: &nxvim_rpc::Rpc) -> Option<String> {
    exec_lua(rpc, "return vim.fn.getcwd()")
        .await
        .as_str()
        .map(str::to_owned)
}

/// `nxvim --workspace proj aaa` from a parent cwd: the server cds into `proj` at boot, then
/// opens the *separate* positional file `aaa` — which resolves against the workspace root and
/// keeps its relative name (`:ls`-friendly), no absolutize hack.
#[tokio::test]
async fn workspace_cd_at_boot_resolves_a_relative_file_against_the_root() {
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard(std::env::current_dir().expect("cwd"));

    // A launch parent holding `proj/aaa`. Canonicalise the parent so the cwd the server reads
    // back matches the absolute root exactly, regardless of symlinks in the temp root (e.g.
    // `/tmp` → `/private/tmp` on macOS).
    let parent = std::fs::canonicalize(temp_dir("ws-cd")).expect("canonicalize parent");
    let proj = parent.join("proj");
    std::fs::create_dir(&proj).expect("create proj dir");
    std::fs::write(proj.join("aaa"), "hello\n").expect("write aaa");

    // Launch from the parent, as `nxvim --workspace proj aaa` does: `workspace_dir` is the
    // absolute root the server cds into (`workspace_cwd` = on, the default `--workspace`), and
    // `file` is the separate positional, spelled relative to the workspace.
    std::env::set_current_dir(&parent).expect("cd into launch parent");
    let init = ServerInit {
        file: Some("aaa".to_string()),
        workspace_dir: Some(proj.to_string_lossy().into_owned()),
        workspace_cwd: true,
        ..Default::default()
    };
    let (rpc, _incoming) = start_attached(init, 80, 24).await;

    // The boot cd landed on the absolute root…
    assert_eq!(
        getcwd(&rpc).await.as_deref(),
        Some(proj.to_string_lossy().as_ref()),
        "the workspace cd landed on the absolute root at boot"
    );
    // …and the relative positional opened its real bytes there, named relative to the root.
    assert_eq!(
        await_lines(&rpc, &["hello"]).await,
        vec!["hello"],
        "the relative positional resolved against the workspace root"
    );
    assert_eq!(
        buf_name(&rpc).await,
        "aaa",
        "the opened file keeps its relative name (relative `:ls` in a workspace)"
    );
}

/// `--workspace-no-cwd` (`workspace_cwd = false`): the workspace identity is still seeded, but
/// the process cwd stays at the launch dir — no boot cd.
#[tokio::test]
async fn workspace_no_cwd_keeps_the_launch_cwd() {
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard(std::env::current_dir().expect("cwd"));

    let parent = std::fs::canonicalize(temp_dir("ws-nocd")).expect("canonicalize parent");
    let proj = parent.join("proj");
    std::fs::create_dir(&proj).expect("create proj dir");

    std::env::set_current_dir(&parent).expect("cd into launch parent");
    let init = ServerInit {
        file: None,
        workspace_dir: Some(proj.to_string_lossy().into_owned()),
        workspace_cwd: false,
        ..Default::default()
    };
    let (rpc, _incoming) = start_attached(init, 80, 24).await;

    assert_eq!(
        getcwd(&rpc).await.as_deref(),
        Some(parent.to_string_lossy().as_ref()),
        "--workspace-no-cwd leaves the launch cwd untouched"
    );
}

/// A workspace session stores buffer paths RELATIVE to the workspace root (a portable shada).
/// On the next launch the boot cd runs BEFORE the restore, so the relative `aaa` resolves
/// against the root and comes back with its real bytes and its relative name — not a blank
/// buffer read against the launch cwd (the reported bug).
#[tokio::test]
async fn workspace_session_restores_a_relative_file_against_the_root() {
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard(std::env::current_dir().expect("cwd"));

    let parent = std::fs::canonicalize(temp_dir("ws-sess")).expect("canonicalize parent");
    let proj = parent.join("proj");
    std::fs::create_dir(&proj).expect("create proj dir");
    std::fs::write(proj.join("aaa"), "hello\n").expect("write aaa");
    let store = temp_dir("ws-sess-store");

    // The init a `--workspace proj` launch builds: boot cd + capture + restore + the native
    // layout opt-in, with the absolute root the server cds into.
    let ws_init = |file: Option<String>| ServerInit {
        file,
        workspace_dir: Some(proj.to_string_lossy().into_owned()),
        workspace_cwd: true,
        shada: Some(Box::new(RedbFileStore::new(store.clone()))),
        workspace_session: true,
        restore_session: true,
        session_save_layout: true,
        ..Default::default()
    };

    // Session 1: launch from the parent with the positional `aaa`. The server cds into `proj`
    // at boot, opens `aaa` (named relative — `aaa`), then quit so the exit flush captures the
    // session with that relative path.
    std::env::set_current_dir(&parent).expect("cd parent");
    {
        let (rpc, incoming) = start_attached(ws_init(Some("aaa".to_string())), 80, 24).await;
        assert_eq!(await_lines(&rpc, &["hello"]).await, vec!["hello"]);
        assert_eq!(buf_name(&rpc).await, "aaa");
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: a FRESH launch from the parent (session 1's boot cd moved the process cwd
    // into `proj`; reset it to mimic relaunching from the original directory). The boot cd cds
    // back into `proj` before the restore, so the relative `aaa` resolves there.
    std::env::set_current_dir(&parent).expect("cd parent again");
    {
        let (rpc, _incoming) = start_attached(ws_init(None), 80, 24).await;
        assert_eq!(
            await_lines(&rpc, &["hello"]).await,
            vec!["hello"],
            "the restored file came back with its real bytes, not a blank buffer"
        );
        assert_eq!(
            buf_name(&rpc).await,
            "aaa",
            "the restored buffer keeps its relative name (portable shada + relative `:ls`)"
        );
    }
}
