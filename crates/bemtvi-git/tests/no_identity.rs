//! A checkout works on a machine with no git identity configured.
//!
//! Moving HEAD writes a reflog entry, and gix signs it with the committer
//! identity; with no `user.name`/`user.email` anywhere it refused ("Could not
//! update reflog"), so `btv.git.checkout` failed for a user who never set one up,
//! and on CI. Plain git falls back to a made-up identity for the reflog, and
//! bemtvi does the same.
//!
//! Its own binary because it empties the identity for the whole process (HOME,
//! the system config, the identity variables): one test, so nothing else runs
//! beside it. Uses the `git` CLI, with its own fixed identity, to build the
//! fixture, and skips loudly when git is absent.

use std::path::{Path, PathBuf};
use std::process::Command;

use bemtvi_lua::GitJob;

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "bemtvi test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "bemtvi test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn rev(dir: &Path, what: &str) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", what])
        .output()
        .expect("git runs");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn a_checkout_works_with_no_git_identity_configured() {
    let base: PathBuf =
        std::env::temp_dir().join(format!("bemtvi-gittest-{}-noident", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let home = base.join("home");
    let repo = base.join("repo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&repo).unwrap();

    // No identity anywhere: an empty home, no system config, none in the environment.
    // The only test in this binary, so no other thread reads the environment.
    for var in ["HOME", "XDG_CONFIG_HOME", "USERPROFILE"] {
        std::env::set_var(var, &home);
    }
    std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
    for var in [
        "GIT_CONFIG_GLOBAL",
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "EMAIL",
    ] {
        std::env::remove_var(var);
    }

    if !git(&repo, &["init", "-q", "-b", "main"]) {
        eprintln!("skip: the git CLI is not available to build the fixture");
        return;
    }
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    assert!(git(&repo, &["add", "."]) && git(&repo, &["commit", "-qm", "one"]));
    let first = rev(&repo, "HEAD");
    std::fs::write(repo.join("a.txt"), "two\n").unwrap();
    assert!(git(&repo, &["commit", "-qam", "two"]));

    let dir = repo.to_string_lossy().into_owned();
    // Detaching HEAD onto the first commit, then attaching it back to the branch:
    // both move HEAD, so both write the reflog.
    let detached = bemtvi_git::run_git_job(&GitJob::Checkout {
        dir: dir.clone(),
        rev: first.clone(),
        detach: true,
    });
    assert!(detached.is_ok(), "detached checkout: {detached:?}");
    assert_eq!(rev(&repo, "HEAD"), first);
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "one\n"
    );

    let attached = bemtvi_git::run_git_job(&GitJob::Checkout {
        dir,
        rev: "main".into(),
        detach: false,
    });
    assert!(attached.is_ok(), "branch checkout: {attached:?}");
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "two\n"
    );

    let _ = std::fs::remove_dir_all(&base);
}
