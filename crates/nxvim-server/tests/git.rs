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

/// `show` of a file that is *inside* a repo but has no HEAD version — including a path
/// that doesn't exist on disk yet (a buffer `:edit`ed for a not-yet-created file) —
/// rejects with `ENOENT` ("no HEAD version"), NOT `ENOREPO`. This is the canonical
/// behavior nxvim-diff relies on: `open()` discovers from the parent dir when the path
/// isn't a directory, so a non-existent file still resolves its repo.
#[tokio::test]
async fn show_uncommitted_file_rejects_enoent_not_enorepo() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let repo = make_repo("git_uncommitted");
    // A path INSIDE the repo that was never created on disk or committed.
    let ghost = repo.join("never-existed.txt");
    exec_lua(
        &rpc,
        &format!(
            "_G.code = nil\n\
             nx.git.show({ghost:?}, 'HEAD'):next(function() _G.code = 'RESOLVED' end, function(e) _G.code = e.code end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.code").await.as_str(),
        Some("ENOENT")
    );
}

// ===== Phase 2: mutation / network verbs (clone / checkout / pull / submodule) =====

/// Capture `git`'s stdout in `cwd` (test plumbing — e.g. resolve a commit sha).
fn git_out(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    assert!(out.status.success(), "git {args:?} failed in {cwd:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A content snapshot of a worktree: every file's repo-relative path → its bytes, with
/// all `.git` metadata (dirs *and* the `.git` file a submodule uses) excluded. Two
/// worktrees with the same snapshot hold byte-identical checked-out content — the
/// property the oracle tests assert (my hand-rolled op == real `git`'s result).
fn snapshot_worktree(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if name == ".git" {
                continue; // skip repo metadata (dir in a normal repo, file in a submodule)
            }
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// Clone `src` into a fresh `<tmp>/<tag>/wt` with real `git` — the oracle side of a
/// differential test. Returns the clone dir.
fn git_clone(src: &Path, tag: &str) -> PathBuf {
    let dest = temp_dir(tag).join("wt");
    git(
        dest.parent().unwrap(),
        &[
            "clone",
            "-q",
            &src.to_string_lossy(),
            &dest.to_string_lossy(),
        ],
    );
    dest
}

/// Commit `contents` to `name` in `repo` with message `msg`; returns the new HEAD sha.
fn commit_file(repo: &Path, name: &str, contents: &str, msg: &str) -> String {
    std::fs::write(repo.join(name), contents).unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", msg]);
    git_out(repo, &["rev-parse", "HEAD"])
}

/// `nx.git_local.clone` clones a (local) source repo into a fresh dir and checks out its
/// worktree — the committed file lands with its content, and `head` resolves.
#[tokio::test]
async fn clone_creates_worktree() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let src = make_repo("git_clone_src");
    let dest = temp_dir("git_clone_dest").join("cloned");
    exec_lua(
        &rpc,
        &format!(
            "_G.dir = nil\n\
             nx.git_local.clone({src:?}, {dest:?}, {{ depth = 1 }})\
               :next(function(d) _G.dir = d end, function(e) _G.err = e end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    // Resolved the dir, no error, and the committed file is really on disk.
    assert_eq!(
        exec_lua(&rpc, "return _G.err and _G.err.message")
            .await
            .as_str(),
        None,
        "clone rejected"
    );
    assert!(
        dest.join("file.txt").exists(),
        "cloned worktree missing file"
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("file.txt")).unwrap(),
        "a\nb\nc\n"
    );
}

/// `nx.git_local.checkout(dir, sha, {detach=true})` detaches HEAD onto an older commit
/// AND resets the worktree to that commit's tree — a file added in a later commit is
/// removed (proving the hand-rolled `reset --hard` deletes dropped paths, not just
/// overwrites).
#[tokio::test]
async fn checkout_detach_resets_worktree() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let src = make_repo("git_co_src");
    let first = git_out(&src, &["rev-parse", "HEAD"]); // has only file.txt
    commit_file(&src, "added.txt", "second\n", "add another file"); // now has added.txt too

    let dest = temp_dir("git_co_dest").join("cloned");
    let dest_s = dest.to_string_lossy().to_string();
    // Full clone (need history to reach `first`), then detach onto the first commit.
    exec_lua(
        &rpc,
        &format!(
            "_G.done = nil\n\
             nx.async(function()\n\
               nx.await(nx.git_local.clone({src:?}, {dest:?}))\n\
               nx.await(nx.git_local.checkout({dest:?}, {first:?}, {{ detach = true }}))\n\
               _G.done = true\n\
             end)():catch(function(e) _G.err = e end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.err and _G.err.message")
            .await
            .as_str(),
        None,
        "checkout flow rejected"
    );
    // added.txt (from the second commit) is gone after resetting to `first`.
    assert!(
        !Path::new(&dest_s).join("added.txt").exists(),
        "reset --hard did not delete the file the target tree lacks"
    );
    assert!(Path::new(&dest_s).join("file.txt").exists());
    // HEAD is detached at `first`.
    exec_lua(
        &rpc,
        &format!(
            "_G.h = nil\n\
             nx.git.head({dest:?}):next(function(r) _G.h = r end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.h and _G.h.detached")
            .await
            .as_bool(),
        Some(true)
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.h and _G.h.sha").await.as_str(),
        Some(first.as_str())
    );
}

/// `nx.git_local.pull` fast-forwards the branch to a new upstream commit (updating the
/// worktree), and reports `updated=false` when already current.
#[tokio::test]
async fn pull_fast_forwards_then_reports_noop() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let src = make_repo("git_pull_src");
    let dest = temp_dir("git_pull_dest").join("cloned");
    let dest_s = dest.to_string_lossy().to_string();
    // Clone, then advance the source with a new commit.
    exec_lua(
        &rpc,
        &format!("nx.git_local.clone({src:?}, {dest:?}):next(function() _G.cloned = true end, function(e) _G.err = e end)"),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.cloned").await.as_bool(),
        Some(true),
        "clone failed"
    );
    let new_sha = commit_file(&src, "file.txt", "a\nb\nc\nd\n", "advance");

    // First pull: a real fast-forward.
    exec_lua(
        &rpc,
        &format!("_G.p = nil\nnx.git_local.pull({dest:?}):next(function(r) _G.p = r end, function(e) _G.perr = e end)"),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.perr and _G.perr.message")
            .await
            .as_str(),
        None,
        "pull rejected"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.p and _G.p.updated")
            .await
            .as_bool(),
        Some(true)
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.p and _G.p.sha").await.as_str(),
        Some(new_sha.as_str())
    );
    // The worktree picked up the upstream change.
    assert_eq!(
        std::fs::read_to_string(Path::new(&dest_s).join("file.txt")).unwrap(),
        "a\nb\nc\nd\n"
    );

    // Second pull: nothing new → updated=false.
    exec_lua(
        &rpc,
        &format!("_G.p2 = nil\nnx.git_local.pull({dest:?}):next(function(r) _G.p2 = r end)"),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.p2 and _G.p2.updated")
            .await
            .as_bool(),
        Some(false)
    );
}

/// `nx.git_local.pull` refuses a non-fast-forward (the branches diverged) with `ENOTFF`,
/// never silently merging or resetting.
#[tokio::test]
async fn pull_rejects_non_fast_forward() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let src = make_repo("git_nff_src");
    let dest = temp_dir("git_nff_dest").join("cloned");
    let dest_s = dest.to_string_lossy().to_string();
    exec_lua(
        &rpc,
        &format!("nx.git_local.clone({src:?}, {dest:?}):next(function() _G.cloned = true end)"),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.cloned").await.as_bool(),
        Some(true),
        "clone failed"
    );
    // Diverge: the clone commits locally, and the source commits differently.
    commit_file(
        Path::new(&dest_s),
        "local.txt",
        "local\n",
        "local-only commit",
    );
    commit_file(&src, "remote.txt", "remote\n", "remote-only commit");

    exec_lua(
        &rpc,
        &format!(
            "_G.code = nil\n\
             nx.git_local.pull({dest:?}):next(function() _G.code = 'RESOLVED' end, function(e) _G.code = e.code end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.code").await.as_str(),
        Some("ENOTFF")
    );
}

/// `nx.git_local.submodule_update{init, recursive}` clones a missing submodule and checks
/// it out to the recorded commit — the submodule's file lands in the parent worktree.
#[tokio::test]
async fn submodule_update_inits_and_checks_out() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    // A standalone sub-repo with a file.
    let sub = temp_dir("git_sm_sub").join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    git(&sub, &["init", "-q", "-b", "main"]);
    commit_file(&sub, "subfile.txt", "hello from submodule\n", "sub initial");
    // A super-repo that embeds it as a submodule at `vendored/`.
    let super_repo = make_repo("git_sm_super");
    let sub_s = sub.to_string_lossy().to_string();
    git(
        &super_repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            &sub_s,
            "vendored",
        ],
    );
    git(&super_repo, &["commit", "-q", "-m", "add submodule"]);

    // Clone the super-repo WITHOUT recursing, then update submodules.
    let dest = temp_dir("git_sm_dest").join("cloned");
    let dest_s = dest.to_string_lossy().to_string();
    exec_lua(
        &rpc,
        &format!(
            "_G.done = nil\n\
             nx.async(function()\n\
               nx.await(nx.git_local.clone({super_repo:?}, {dest:?}))\n\
               nx.await(nx.git_local.submodule_update({dest:?}, {{ init = true, recursive = true }}))\n\
               _G.done = true\n\
             end)():catch(function(e) _G.err = e end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.err and _G.err.message")
            .await
            .as_str(),
        None,
        "submodule_update flow rejected"
    );
    let subfile = Path::new(&dest_s).join("vendored").join("subfile.txt");
    assert!(subfile.exists(), "submodule file not checked out");
    assert_eq!(
        std::fs::read_to_string(&subfile).unwrap(),
        "hello from submodule\n"
    );
}

// ----- differential (oracle) tests: hand-rolled op vs real `git` -----
//
// For each manually-implemented verb, run the SAME operation two ways — real `git` in
// one clone, `nx.git_local.*` in a parallel clone of the same source — and assert the
// resulting worktrees are byte-identical. This pins the hand-rolled reset --hard /
// ff-pull / submodule-update to git's actual behavior, not just "it did something".

/// Oracle: `nx.git_local.checkout(dir, sha, {detach})` produces the same worktree as
/// `git checkout --detach <sha>` — including deleting files the target commit lacks.
#[tokio::test]
async fn checkout_matches_git_oracle() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let src = make_repo("git_co_oracle_src");
    let first = git_out(&src, &["rev-parse", "HEAD"]);
    // Later commits add, then delete, then re-add files so the reset has real work.
    commit_file(&src, "b.txt", "beta\n", "add b");
    commit_file(&src, "file.txt", "a\nb\nc\nCHANGED\n", "change file");

    // Oracle side: real git checkout --detach.
    let git_wt = git_clone(&src, "git_co_oracle_git");
    git(&git_wt, &["checkout", "-q", "--detach", &first]);

    // My side: clone + nx.git_local.checkout.
    let mine = temp_dir("git_co_oracle_mine").join("wt");
    let mine_s = mine.to_string_lossy().to_string();
    exec_lua(
        &rpc,
        &format!(
            "nx.async(function()\n\
               nx.await(nx.git_local.clone({src:?}, {mine:?}))\n\
               nx.await(nx.git_local.checkout({mine:?}, {first:?}, {{ detach = true }}))\n\
               _G.done = true\n\
             end)():catch(function(e) _G.err = e end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.err and _G.err.message")
            .await
            .as_str(),
        None
    );
    assert_eq!(
        snapshot_worktree(&git_wt),
        snapshot_worktree(Path::new(&mine_s)),
        "checkout worktree differs from `git checkout --detach`"
    );
}

/// Oracle: `nx.git_local.pull` fast-forwards to exactly the same worktree as `git pull
/// --ff-only`.
#[tokio::test]
async fn pull_matches_git_oracle() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let src = make_repo("git_pull_oracle_src");

    let git_wt = git_clone(&src, "git_pull_oracle_git");
    let mine = temp_dir("git_pull_oracle_mine").join("wt");
    let mine_s = mine.to_string_lossy().to_string();
    exec_lua(
        &rpc,
        &format!("nx.git_local.clone({src:?}, {mine:?}):next(function() _G.cloned = true end)"),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.cloned").await.as_bool(),
        Some(true)
    );

    // Advance the source (a modify + a new file), then pull both clones.
    commit_file(&src, "file.txt", "a\nb\nc\nd\n", "advance");
    commit_file(&src, "extra.txt", "extra\n", "add extra");

    git(&git_wt, &["pull", "-q", "--ff-only"]);
    exec_lua(
        &rpc,
        &format!("nx.git_local.pull({mine:?}):next(function() _G.pulled = true end, function(e) _G.err = e end)"),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.err and _G.err.message")
            .await
            .as_str(),
        None
    );
    assert_eq!(
        snapshot_worktree(&git_wt),
        snapshot_worktree(Path::new(&mine_s)),
        "pulled worktree differs from `git pull --ff-only`"
    );
}

/// Oracle: `nx.git_local.submodule_update{init,recursive}` checks out the submodule to
/// the same content as `git submodule update --init --recursive`.
#[tokio::test]
async fn submodule_update_matches_git_oracle() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let sub = temp_dir("git_sm_oracle_sub").join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    git(&sub, &["init", "-q", "-b", "main"]);
    commit_file(&sub, "subfile.txt", "hello from submodule\n", "sub initial");
    commit_file(&sub, "more.txt", "more\n", "sub second");

    let super_repo = make_repo("git_sm_oracle_super");
    let sub_s = sub.to_string_lossy().to_string();
    git(
        &super_repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            &sub_s,
            "vendored",
        ],
    );
    git(&super_repo, &["commit", "-q", "-m", "add submodule"]);

    // Oracle: git clone (no recurse) + submodule update --init --recursive.
    let git_wt = git_clone(&super_repo, "git_sm_oracle_git");
    git(
        &git_wt,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "--recursive",
        ],
    );

    // Mine: nx.git_local.clone + submodule_update.
    let mine = temp_dir("git_sm_oracle_mine").join("wt");
    let mine_s = mine.to_string_lossy().to_string();
    exec_lua(
        &rpc,
        &format!(
            "nx.async(function()\n\
               nx.await(nx.git_local.clone({super_repo:?}, {mine:?}))\n\
               nx.await(nx.git_local.submodule_update({mine:?}, {{ init = true, recursive = true }}))\n\
               _G.done = true\n\
             end)():catch(function(e) _G.err = e end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.err and _G.err.message")
            .await
            .as_str(),
        None
    );
    // Compare the checked-out submodule content on both sides.
    assert_eq!(
        snapshot_worktree(&git_wt.join("vendored")),
        snapshot_worktree(&Path::new(&mine_s).join("vendored")),
        "submodule worktree differs from `git submodule update`"
    );
}

/// Oracle: a submodule declared with a RELATIVE url (`../dep` — common on GitHub, where
/// a repo's submodules live under the same owner) resolves against the superproject's
/// remote. `nx.git_local.submodule_update` must check it out to the same content as `git
/// submodule update --init --recursive` (which resolves the relative url too). Guards the
/// relative-URL resolution gix's `sm.url()` does NOT do for us.
#[tokio::test]
async fn submodule_relative_url_matches_git_oracle() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let base = temp_dir("git_sm_rel");
    // A sibling `dep` repo the super references *relatively*.
    let dep = base.join("dep");
    std::fs::create_dir_all(&dep).unwrap();
    git(&dep, &["init", "-q", "-b", "main"]);
    commit_file(
        &dep,
        "reldep.txt",
        "relative submodule content\n",
        "dep initial",
    );

    // The super embeds dep at vendored/, then rewrites the committed `.gitmodules` url to
    // a RELATIVE `../dep`, so a cloner must resolve it against the super's origin.
    let super_repo = base.join("super");
    std::fs::create_dir_all(&super_repo).unwrap();
    git(&super_repo, &["init", "-q", "-b", "main"]);
    std::fs::write(super_repo.join("file.txt"), "super\n").unwrap();
    git(&super_repo, &["add", "-A"]);
    git(&super_repo, &["commit", "-q", "-m", "super initial"]);
    let dep_s = dep.to_string_lossy().to_string();
    git(
        &super_repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            &dep_s,
            "vendored",
        ],
    );
    git(
        &super_repo,
        &[
            "config",
            "-f",
            ".gitmodules",
            "submodule.vendored.url",
            "../dep",
        ],
    );
    git(&super_repo, &["add", ".gitmodules"]);
    git(
        &super_repo,
        &["commit", "-q", "-m", "relative submodule url"],
    );

    // Oracle: git clone (no recurse) + recursive submodule update (git resolves `../dep`).
    let git_wt = git_clone(&super_repo, "git_sm_rel_git");
    git(
        &git_wt,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "--recursive",
        ],
    );

    // Mine: nx.git_local.clone + submodule_update (my resolver resolves `../dep`).
    let mine = temp_dir("git_sm_rel_mine").join("wt");
    let mine_s = mine.to_string_lossy().to_string();
    exec_lua(
        &rpc,
        &format!(
            "nx.async(function()\n\
               nx.await(nx.git_local.clone({super_repo:?}, {mine:?}))\n\
               nx.await(nx.git_local.submodule_update({mine:?}, {{ init = true, recursive = true }}))\n\
               _G.done = true\n\
             end)():catch(function(e) _G.err = e end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.err and _G.err.message")
            .await
            .as_str(),
        None,
        "relative-url submodule_update rejected"
    );
    // The relative-url submodule was really checked out (non-empty) AND matches git.
    let mine_dep = Path::new(&mine_s).join("vendored").join("reldep.txt");
    assert!(
        mine_dep.exists(),
        "relative-url submodule not checked out at {mine_dep:?}"
    );
    assert_eq!(
        snapshot_worktree(&git_wt.join("vendored")),
        snapshot_worktree(&Path::new(&mine_s).join("vendored")),
        "relative-url submodule worktree differs from `git submodule update`"
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

/// A file that is BOTH staged and then modified again is ONE entry carrying both
/// porcelain columns (`M` / `M`), not two half-filled ones. gix reports the staged
/// half (`TreeIndex`) and the unstaged half (`IndexWorktree`) as separate items; the
/// engine folds them per path, because otherwise every consumer must fold them
/// identically and a consumer that doesn't silently reports "staged, clean worktree"
/// for a file with unstaged edits.
#[tokio::test]
async fn status_folds_a_staged_and_modified_file_into_one_entry() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let repo = make_repo("git_status_fold");
    // Stage a change, then dirty the worktree again on top of it.
    std::fs::write(repo.join("file.txt"), "a\nb\nc\nstaged\n").unwrap();
    git(&repo, &["add", "-A"]);
    std::fs::write(repo.join("file.txt"), "a\nb\nc\nstaged\nunstaged\n").unwrap();

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
        exec_lua(&rpc, "return _G.st and #_G.st.entries").await,
        rmpv::Value::from(1),
        "one path, one entry"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.st.entries[1].index")
            .await
            .as_str(),
        Some("M"),
        "the staged column survives the fold"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.st.entries[1].worktree")
            .await
            .as_str(),
        Some("M"),
        "the unstaged column survives the fold"
    );
}

/// An untracked file is porcelain's `??` — BOTH columns, not a bare worktree `?`.
/// Read literally, a lone `?` is neither `??` nor a known status letter, so every
/// consumer has to special-case it back into porcelain's spelling.
#[tokio::test]
async fn status_spells_untracked_as_both_columns() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let repo = make_repo("git_status_untracked");
    std::fs::write(repo.join("new.txt"), "fresh\n").unwrap();

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
        exec_lua(&rpc, "return _G.st and _G.st.entries[1].path")
            .await
            .as_str(),
        Some("new.txt")
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.st.entries[1].index")
            .await
            .as_str(),
        Some("?")
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.st.entries[1].worktree")
            .await
            .as_str(),
        Some("?")
    );
}

/// A file renamed in the WORKTREE (not staged) is reported, as `R` on the worktree
/// column against its new path. gix surfaces this as an `index_worktree::Item::Rewrite`,
/// which the engine used to drop on the floor — so a rename showed no status at all.
#[tokio::test]
async fn status_reports_an_unstaged_rename() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let repo = make_repo("git_status_rename");
    // Rename on disk only — nothing staged, so gix must detect it as a worktree rewrite.
    std::fs::rename(repo.join("file.txt"), repo.join("renamed.txt")).unwrap();

    exec_lua(
        &rpc,
        &format!(
            "_G.st = nil\n\
             nx.git.status({repo:?}):next(function(r) _G.st = r end, function(e) _G.sterr = e end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // One entry: the destination, marked `R` on the worktree column, carrying the path
    // it came from. (git's own porcelain does not detect unstaged renames — it prints
    // ` D file.txt` + `?? renamed.txt` — so this is deliberately more informative.)
    assert_eq!(
        exec_lua(&rpc, "return _G.st and #_G.st.entries").await,
        rmpv::Value::from(1)
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.st.entries[1].path")
            .await
            .as_str(),
        Some("renamed.txt")
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return _G.st.entries[1].index .. _G.st.entries[1].worktree"
        )
        .await
        .as_str(),
        Some(" R")
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.st.entries[1].orig_path")
            .await
            .as_str(),
        Some("file.txt"),
        "a rename carries the path it came from — porcelain's `R old -> new`"
    );
}

/// A non-rename entry's `orig_path` is empty, not nil — so a consumer can read the
/// field unconditionally.
#[tokio::test]
async fn status_leaves_orig_path_empty_for_a_plain_change() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let repo = make_repo("git_status_origpath");
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
        exec_lua(&rpc, "return _G.st and _G.st.entries[1].orig_path")
            .await
            .as_str(),
        Some("")
    );
}

/// A file renamed AND staged, then edited again in the worktree, folds into one `RM`
/// entry that still knows where it came from. The rename source arrives on the
/// `TreeIndex` half and the modification on the `IndexWorktree` half, so a fold that
/// only merged the two columns would drop `orig_path` whenever gix happened to yield
/// the worktree half first.
#[tokio::test]
async fn status_keeps_the_rename_source_when_folding_a_later_edit() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let repo = make_repo("git_status_rename_edit");
    git(&repo, &["mv", "file.txt", "moved.txt"]);
    std::fs::write(repo.join("moved.txt"), "a\nb\nc\nedited\n").unwrap();

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
        exec_lua(&rpc, "return _G.st and #_G.st.entries").await,
        rmpv::Value::from(1),
        "the staged rename and the later edit are one path, one entry"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.st.entries[1].path")
            .await
            .as_str(),
        Some("moved.txt")
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return _G.st.entries[1].index .. _G.st.entries[1].worktree"
        )
        .await
        .as_str(),
        Some("RM")
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.st.entries[1].orig_path")
            .await
            .as_str(),
        Some("file.txt"),
        "the rename source survives the fold with the worktree edit"
    );
}

// ----- checkout (attach) + fetch ---------------------------------------------
//
// Two gaps the plugin lockfile ran into: `checkout` implemented only the DETACHING
// mode, so nothing could re-attach a detached checkout to its branch; and there was no
// `fetch`, so a shallow clone could never be deepened to reach an older commit. See
// docs/plans/2026-07-25-plugin-lockfile.md.

/// `checkout(dir, branch)` (no `detach`) attaches HEAD to a branch: HEAD becomes
/// symbolic again and the worktree matches that branch's tip. The inverse of the
/// detaching mode, and what makes a lock-pinned checkout movable again.
#[tokio::test]
async fn checkout_attaches_head_to_a_branch() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let src = make_repo("git_attach_src");
    let first = git_out(&src, &["rev-parse", "HEAD"]);
    commit_file(&src, "added.txt", "second\n", "add another file");
    let tip = git_out(&src, &["rev-parse", "HEAD"]);

    let dest = temp_dir("git_attach_dest").join("cloned");
    let dest_s = dest.to_string_lossy().to_string();
    // Full clone, detach onto the older commit, then re-attach to `main`.
    exec_lua(
        &rpc,
        &format!(
            "_G.done, _G.err = nil, nil\n\
             nx.async(function()\n\
               nx.await(nx.git_local.clone({src:?}, {dest:?}))\n\
               nx.await(nx.git_local.checkout({dest:?}, {first:?}, {{ detach = true }}))\n\
               nx.await(nx.git_local.checkout({dest:?}, \"main\"))\n\
               _G.done = true\n\
             end)():catch(function(e) _G.err = e end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.err and _G.err.message")
            .await
            .as_str(),
        None,
        "attach flow rejected"
    );

    // HEAD is on the branch again (not detached) at its tip, and the worktree came along.
    exec_lua(
        &rpc,
        &format!("_G.h = nil\nnx.git.head({dest:?}):next(function(r) _G.h = r end)"),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.h and _G.h.detached")
            .await
            .as_bool(),
        Some(false),
        "HEAD should be attached to a branch"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.h and _G.h.branch").await.as_str(),
        Some("main")
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.h and _G.h.sha").await.as_str(),
        Some(tip.as_str())
    );
    assert!(
        Path::new(&dest_s).join("added.txt").exists(),
        "attaching to the branch tip must bring the worktree along"
    );
    // The git oracle agrees HEAD is symbolic.
    assert_eq!(
        git_out(Path::new(&dest_s), &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
}

/// Attaching to a branch that exists only on the remote (a fresh clone tracks just the
/// default branch) creates the local branch from its remote-tracking ref, like
/// `git checkout <branch>` does.
#[tokio::test]
async fn checkout_attaches_to_a_remote_only_branch() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let src = make_repo("git_remotebr_src");
    git(&src, &["checkout", "-q", "-b", "side"]);
    commit_file(&src, "side.txt", "on the side\n", "side commit");
    git(&src, &["checkout", "-q", "main"]);

    let dest = temp_dir("git_remotebr_dest").join("cloned");
    let dest_s = dest.to_string_lossy().to_string();
    exec_lua(
        &rpc,
        &format!(
            "_G.done, _G.err = nil, nil\n\
             nx.async(function()\n\
               nx.await(nx.git_local.clone({src:?}, {dest:?}))\n\
               nx.await(nx.git_local.checkout({dest:?}, \"side\"))\n\
               _G.done = true\n\
             end)():catch(function(e) _G.err = e end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.err and _G.err.message")
            .await
            .as_str(),
        None,
        "attach-to-remote-branch rejected"
    );
    assert_eq!(
        git_out(Path::new(&dest_s), &["symbolic-ref", "HEAD"]),
        "refs/heads/side"
    );
    assert!(Path::new(&dest_s).join("side.txt").exists());
}

/// Attaching to a ref that is nowhere (no local branch, no remote-tracking ref) fails
/// loud rather than silently leaving HEAD where it was.
#[tokio::test]
async fn checkout_attach_rejects_an_unknown_branch() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let src = make_repo("git_nobr_src");
    let dest = temp_dir("git_nobr_dest").join("cloned");
    exec_lua(
        &rpc,
        &format!(
            "_G.err = nil\n\
             nx.async(function()\n\
               nx.await(nx.git_local.clone({src:?}, {dest:?}))\n\
               nx.await(nx.git_local.checkout({dest:?}, \"no-such-branch\"))\n\
             end)():catch(function(e) _G.err = e end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let msg = exec_lua(&rpc, "return _G.err and _G.err.message").await;
    let msg = msg.as_str().unwrap_or("");
    assert!(
        msg.contains("no-such-branch"),
        "must name the missing branch: {msg}"
    );
}

/// `fetch(dir, { unshallow = true })` removes a shallow clone's boundary, so an older
/// commit the `depth = 1` clone could not contain becomes reachable. This is what lets a
/// lockfile restore an earlier revision in place instead of re-cloning.
#[tokio::test]
async fn fetch_unshallow_makes_older_history_reachable() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let src = make_repo("git_unshallow_src");
    let first = git_out(&src, &["rev-parse", "HEAD"]);
    commit_file(&src, "added.txt", "second\n", "second commit");

    let dest = temp_dir("git_unshallow_dest").join("cloned");
    let dest_s = dest.to_string_lossy().to_string();
    // A depth-1 clone cannot contain `first` — checking it out must fail...
    exec_lua(
        &rpc,
        &format!(
            "_G.err1, _G.ok2 = nil, nil\n\
             nx.async(function()\n\
               nx.await(nx.git_local.clone({src:?}, {dest:?}, {{ depth = 1 }}))\n\
               local ok, e = pcall(nx.await, nx.git_local.checkout({dest:?}, {first:?}, {{ detach = true }}))\n\
               if not ok then _G.err1 = tostring(e and e.message or e) end\n\
               -- ...until the clone is unshallowed, after which it succeeds.\n\
               nx.await(nx.git_local.fetch({dest:?}, {{ unshallow = true }}))\n\
               nx.await(nx.git_local.checkout({dest:?}, {first:?}, {{ detach = true }}))\n\
               _G.ok2 = true\n\
             end)():catch(function(e) _G.err2 = tostring(e and e.message or e) end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(3000)).await;
    assert!(
        exec_lua(&rpc, "return _G.err1").await.as_str().is_some(),
        "a depth-1 clone should not be able to reach the parent commit"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.ok2").await.as_bool(),
        Some(true),
        "after unshallow the older commit must be reachable; err2={:?}",
        exec_lua(&rpc, "return _G.err2").await
    );
    assert_eq!(
        git_out(Path::new(&dest_s), &["rev-parse", "HEAD"]),
        first,
        "HEAD should sit on the previously-unreachable commit"
    );
    // The shallow marker is gone — the oracle agrees it is a full repo now.
    assert!(
        !Path::new(&dest_s).join(".git").join("shallow").exists(),
        "unshallow should remove .git/shallow"
    );
}

/// A plain `fetch(dir)` updates remote-tracking refs without touching the worktree or
/// the shallow boundary.
#[tokio::test]
async fn fetch_updates_tracking_refs_without_touching_the_worktree() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (rpc, _incoming) = start().await;
    let src = make_repo("git_fetch_src");
    let dest = temp_dir("git_fetch_dest").join("cloned");
    let dest_s = dest.to_string_lossy().to_string();
    exec_lua(
        &rpc,
        &format!("nx.async(function() nx.await(nx.git_local.clone({src:?}, {dest:?})) end)()"),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let before = git_out(Path::new(&dest_s), &["rev-parse", "HEAD"]);

    // The remote moves on; a fetch brings the tracking ref forward but leaves HEAD and
    // the worktree exactly where they were (that is `pull`'s job, not `fetch`'s).
    commit_file(&src, "added.txt", "second\n", "second commit");
    let tip = git_out(&src, &["rev-parse", "HEAD"]);
    exec_lua(
        &rpc,
        &format!(
            "_G.done, _G.err = nil, nil\n\
             nx.git_local.fetch({dest:?}):next(function() _G.done = true end)\n\
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.done").await.as_bool(),
        Some(true),
        "fetch rejected: {:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    assert_eq!(
        git_out(Path::new(&dest_s), &["rev-parse", "HEAD"]),
        before,
        "fetch must not move HEAD"
    );
    assert!(
        !Path::new(&dest_s).join("added.txt").exists(),
        "fetch must not touch the worktree"
    );
    assert_eq!(
        git_out(
            Path::new(&dest_s),
            &["rev-parse", "refs/remotes/origin/main"]
        ),
        tip,
        "fetch must advance the remote-tracking ref"
    );
}
