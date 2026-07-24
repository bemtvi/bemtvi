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
