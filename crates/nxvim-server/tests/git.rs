//! End-to-end coverage of the native `nx.git.*` API (docs/plans/2026-07-24-native-git-gix.md).
//!
//! Black-box, per project conventions: a real server over RPC, driven with
//! `nvim_exec_lua`, exercising the promise-always git verbs against a throwaway
//! on-disk git repo. The EDITOR uses no `git` binary (it runs gix in-process via
//! `nxvim-git`); only the test's fixture setup shells out to `git` to build the repo,
//! the same convention as `tests/plugins.rs`. `git` must be on PATH (dev/CI have it);
//! absent, the test skips loud rather than failing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, start_attached, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Whether `git` is available; skip-if-missing is the convention for an external
/// fixture dependency (the editor itself never shells out to git).
fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run `git` in `cwd` with a fixed identity and no host config bleeding in. Test
/// plumbing only.
fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "nxvim test")
        .env("GIT_AUTHOR_EMAIL", "test@nxvim")
        .env("GIT_COMMITTER_NAME", "nxvim test")
        .env("GIT_COMMITTER_EMAIL", "test@nxvim")
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed in {cwd:?}");
}

/// Build a repo at `<tmp>/repo` on branch `main` with a committed `file.txt`
/// ("a\nb\nc\n"). Returns the repo dir.
fn make_repo(tag: &str) -> PathBuf {
    let repo = temp_dir(tag).join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("file.txt"), "a\nb\nc\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    repo
}

/// `nx.git.head` reports the branch, a full sha, and detached=false.
#[tokio::test]
async fn head_reports_branch_and_sha() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let repo = make_repo("git_head");
    let file = repo.join("file.txt");
    exec_lua(
        &rpc,
        &format!(
            "_G.h = nil\n\
             nx.git.head({file:?}):next(function(r) _G.h = r end, function(e) _G.err = e end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.h and _G.h.branch").await.as_str(),
        Some("main")
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.h and _G.h.detached")
            .await
            .as_bool(),
        Some(false)
    );
    // A full 40-hex sha (real commit id, not a placeholder).
    assert_eq!(
        exec_lua(&rpc, "return _G.h and #_G.h.sha").await.as_u64(),
        Some(40)
    );
}

/// `nx.git.diff_file` folds working-tree-vs-HEAD line changes into counts. Editing
/// one of three lines is one changed line, nothing added / removed.
#[tokio::test]
async fn diff_file_counts_a_modified_line() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let repo = make_repo("git_diff");
    let file = repo.join("file.txt");
    // Change the middle line in the working tree (HEAD still has "a\nb\nc\n").
    std::fs::write(&file, "a\nB\nc\n").unwrap();
    exec_lua(
        &rpc,
        &format!(
            "_G.d = nil\n\
             nx.git.diff_file({repo:?}, {file:?}):next(function(r) _G.d = r end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.d and _G.d.changed")
            .await
            .as_u64(),
        Some(1)
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.d and _G.d.added").await.as_u64(),
        Some(0)
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.d and _G.d.removed")
            .await
            .as_u64(),
        Some(0)
    );
}

/// `nx.git.show` returns the HEAD blob bytes (the committed content), NOT the edited
/// working-tree content — proving it reads the object store, not the file.
#[tokio::test]
async fn show_returns_head_blob_not_worktree() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let repo = make_repo("git_show");
    let file = repo.join("file.txt");
    std::fs::write(&file, "EDITED\n").unwrap(); // working tree diverges from HEAD
    exec_lua(
        &rpc,
        &format!(
            "_G.s = nil\n\
             nx.git.show({file:?}, 'HEAD'):next(function(r) _G.s = r end, function(e) _G.serr = e end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.s").await.as_str(),
        Some("a\nb\nc\n")
    );
}

/// `nx.git.status` reports a modified tracked file with a worktree-column `M`.
#[tokio::test]
async fn status_reports_a_modified_file() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let repo = make_repo("git_status");
    std::fs::write(repo.join("file.txt"), "a\nb\nc\nd\n").unwrap();
    exec_lua(
        &rpc,
        &format!(
            "_G.st = nil\n\
             nx.git.status({repo:?}):next(function(r) _G.st = r end, function(e) _G.sterr = e end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.st and _G.st.dirty")
            .await
            .as_bool(),
        Some(true)
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return _G.st and _G.st.entries[1] and _G.st.entries[1].path"
        )
        .await
        .as_str(),
        Some("file.txt")
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return _G.st and _G.st.entries[1] and _G.st.entries[1].worktree"
        )
        .await
        .as_str(),
        Some("M")
    );
}

/// A non-repo path rejects loud with `ENOREPO` (never a silent empty result).
#[tokio::test]
async fn discover_rejects_outside_a_repo() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("git_norepo");
    exec_lua(
        &rpc,
        &format!(
            "_G.code = nil\n\
             nx.git.discover({dir:?}):next(function() _G.code = 'RESOLVED' end, function(e) _G.code = e.code end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.code").await.as_str(),
        Some("ENOREPO")
    );
}
