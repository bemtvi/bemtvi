//! A repository's **index** decides which worktree files a checkout deletes — and
//! the index is a file on disk, not something the editor derived.
//!
//! `reset_worktree_to_tree` (the hand-rolled `reset --hard` behind `checkout` /
//! `pull` / `submodule update`) diffs the old index against the target tree and
//! `remove_file`s every path the tree drops. Joined onto the worktree root
//! without validation, an index entry of `../escaped14.txt` names a file *outside*
//! the repository — and the checkout deletes it.
//!
//! Reaching it needs a `.git` directory the attacker wrote, which is not exotic:
//! a repository delivered as an archive (with its `.git/` inside), a shared
//! checkout, or anything else that hands over a whole repo directory rather than
//! having git build it locally. gix verifies the index's trailing checksum, so the
//! entry has to be stamped — which costs an attacker one SHA-1, and costs this
//! test the same.
//!
//! Note which side is guarded: the *write* side of the checkout goes through
//! `gix_worktree_state::checkout`, which validates every path component itself.
//! The delete side was ours, and had no such check.
//!
//! Uses the `git` CLI to build the fixture repository (creating commits is not
//! something this crate's own verbs do). Skips — loudly — when git is absent,
//! the project's convention for an unavailable external dependency.

use std::path::{Path, PathBuf};
use std::process::Command;

use bemtvi_lua::GitJob;

/// Run `git` in `dir` with a fixed identity (so a developer's `user.name` and any
/// global hooks/templates play no part). `false` if git is missing or the command
/// failed.
fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "bemtvi test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "bemtvi test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git runs");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A private temp directory for one test, removed on the way out.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bemtvi-gittest-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Overwrite the 16-byte path of the index entry named `doomed` with `replacement`
/// (also 16 bytes, so the entry's length and its `flags` path-length stay valid),
/// then re-stamp the index's trailing SHA-1 — gix rejects a bad checksum, and so
/// would git, which is exactly what an attacker fixes up too.
fn rewrite_index_path(repo: &Path, doomed: &[u8; 16], replacement: &[u8; 16]) {
    let idx_path = repo.join(".git/index");
    let mut idx = std::fs::read(&idx_path).expect("read .git/index");
    let at = idx
        .windows(doomed.len())
        .position(|w| w == doomed)
        .expect("the doomed path should be in the index verbatim");
    idx[at..at + doomed.len()].copy_from_slice(replacement);

    let body = idx.len() - 20;
    let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha1);
    hasher.update(&idx[..body]);
    let sum = hasher.try_finalize().expect("sha1");
    idx[body..].copy_from_slice(sum.as_slice());
    std::fs::write(&idx_path, &idx).expect("write .git/index");
}

/// Build `<scratch>/repo` with two commits: the first carries `keep.txt` plus a
/// 16-character file, the second drops the 16-character one. Leaves the worktree
/// on the first commit, so its path is what the index lists. Returns the repo path
/// and the second commit's id (the checkout target), or `None` if git is missing.
fn fixture(name: &str) -> Option<(PathBuf, PathBuf, String)> {
    let base = scratch(name);
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    if !git(&repo, &["init", "-q", "-b", "main"]) {
        eprintln!("skip: the `git` CLI is not available");
        return None;
    }
    std::fs::write(repo.join("keep.txt"), "keep\n").unwrap();
    std::fs::write(
        repo.join("aaaaaaaaaaaa.txt"),
        "dropped by the next commit\n",
    )
    .unwrap();
    assert!(git(&repo, &["add", "-A"]) && git(&repo, &["commit", "-qm", "one"]));
    let first = git_stdout(&repo, &["rev-parse", "HEAD"]);

    std::fs::remove_file(repo.join("aaaaaaaaaaaa.txt")).unwrap();
    assert!(git(&repo, &["add", "-A"]) && git(&repo, &["commit", "-qm", "two"]));
    let second = git_stdout(&repo, &["rev-parse", "HEAD"]);

    assert!(git(&repo, &["checkout", "-q", &first]));
    Some((base, repo, second))
}

/// An index entry that climbs out of the worktree must not be deleted through.
/// The path below resolves to a file one directory *above* the repository; before
/// the check, checking out a commit that drops the entry removed it.
#[test]
fn a_checkout_does_not_delete_through_a_dotdot_index_path() {
    let Some((base, repo, target)) = fixture("dotdot") else {
        return;
    };
    let victim = base.join("escaped14.txt");
    std::fs::write(&victim, "PRECIOUS\n").unwrap();

    // `../escaped14.txt` is exactly as long as the name it replaces.
    rewrite_index_path(&repo, b"aaaaaaaaaaaa.txt", b"../escaped14.txt");

    let outcome = bemtvi_git::run_git_job(&GitJob::Checkout {
        dir: repo.to_string_lossy().into_owned(),
        rev: target,
        detach: true,
    });
    assert!(
        outcome.is_ok(),
        "the checkout itself should still succeed: {outcome:?}"
    );

    assert!(
        victim.exists(),
        "the checkout deleted a file outside the worktree — an index path is \
         attacker-controlled input and must be validated before it is joined onto \
         the worktree root"
    );
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "PRECIOUS\n");
    // The checkout did its real job: the target tree is on disk.
    assert!(repo.join("keep.txt").exists());
    let _ = std::fs::remove_dir_all(&base);
}

/// The same guard for an **absolute** index path: `/tmp/...` joined onto the
/// worktree root is, in Rust's `Path::join`, simply the absolute path — the
/// worktree root disappears entirely.
#[test]
fn a_checkout_does_not_delete_through_an_absolute_index_path() {
    let Some((base, repo, target)) = fixture("absolute") else {
        return;
    };
    // An absolute path of exactly 16 bytes pointing into this test's scratch dir.
    let victim_dir = base.join("abs");
    std::fs::create_dir_all(&victim_dir).unwrap();
    let victim = victim_dir.join("v.txt");
    std::fs::write(&victim, "PRECIOUS\n").unwrap();
    // Build a path of *exactly* 16 bytes — the entry's `flags` field encodes the
    // path length, so the replacement has to match the name it overwrites byte for
    // byte. `/tmp/` + a 5-character name + `/v.txt` is 16 on the nose, whatever the
    // temp directory is really called.
    let short = PathBuf::from(format!("/tmp/{:05}", std::process::id() % 100_000));
    let _ = std::fs::remove_file(&short);
    if std::os::unix::fs::symlink(&victim_dir, &short).is_err() {
        eprintln!("skip: cannot create the symlink this fixture needs");
        let _ = std::fs::remove_dir_all(&base);
        return;
    }
    let absolute = format!("{}/v.txt", short.display());
    assert_eq!(
        absolute.len(),
        16,
        "the fixture path must be exactly 16 bytes"
    );
    let mut replacement = [0u8; 16];
    replacement.copy_from_slice(absolute.as_bytes());
    rewrite_index_path(&repo, b"aaaaaaaaaaaa.txt", &replacement);

    let outcome = bemtvi_git::run_git_job(&GitJob::Checkout {
        dir: repo.to_string_lossy().into_owned(),
        rev: target,
        detach: true,
    });
    assert!(
        outcome.is_ok(),
        "the checkout itself should still succeed: {outcome:?}"
    );
    assert!(
        victim.exists(),
        "an absolute index path replaced the worktree root entirely and the file it \
         named was deleted"
    );

    let _ = std::fs::remove_file(&short);
    let _ = std::fs::remove_dir_all(&base);
}

/// The guard must not stop an ordinary checkout from cleaning up: a file the
/// target tree drops is still removed from the worktree. (Otherwise "safe" would
/// just mean "does nothing".)
#[test]
fn a_checkout_still_deletes_a_file_the_target_tree_drops() {
    let Some((base, repo, target)) = fixture("ordinary") else {
        return;
    };
    let dropped = repo.join("aaaaaaaaaaaa.txt");
    assert!(dropped.exists(), "the fixture's first commit carries it");

    let outcome = bemtvi_git::run_git_job(&GitJob::Checkout {
        dir: repo.to_string_lossy().into_owned(),
        rev: target,
        detach: true,
    });
    assert!(outcome.is_ok(), "checkout: {outcome:?}");
    assert!(
        !dropped.exists(),
        "a checkout must still remove worktree files the target tree does not have"
    );
    assert!(repo.join("keep.txt").exists());
    let _ = std::fs::remove_dir_all(&base);
}
