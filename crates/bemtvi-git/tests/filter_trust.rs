//! A repository's own `.git/config` can name **filter drivers** —
//! `filter.<name>.clean` / `.smudge` / `.process` — which are shell commands.
//! `btv.git.status` compares worktree content against the index, and when a path's
//! `.gitattributes` selects a filter, that comparison runs the driver's `clean`
//! command. The statusline asks for status on every buffer it shows, so opening a
//! file in a repository is enough to run whatever that repository configured.
//!
//! git runs these drivers for repositories it trusts and refuses to read the
//! repository's config at all for ones it doesn't (`safe.directory`): a repository
//! owned by another user is not trusted unless `safe.directory` names it. These tests
//! pin both halves on the gix engine: a repository you own still runs its filter
//! (git parity), one owned by someone else does not — and status still reports the
//! change, compared without the filter.
//!
//! Building a repository owned by another user needs no root: `unshare --map-auto`
//! maps the caller's subordinate uid range, inside which a `chown` lands the files on
//! a uid that, outside, is not ours. When that is unavailable the untrusted tests skip
//! loudly, the project's convention for an unavailable external dependency. The
//! fixture repository is built with the `git` CLI, which skips the same way.

use std::path::{Path, PathBuf};
use std::process::Command;

use bemtvi_lua::{GitJob, GitValue};

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

fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("bemtvi-filtertrust-{}-{name}", std::process::id()));
    remove_tree(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Run `args` as root of a fresh user namespace that maps the caller's subordinate
/// uids — so a `chown` inside reaches a uid outside that is not the caller's.
fn in_userns(args: &[&str]) -> bool {
    Command::new("unshare")
        .args(["--map-auto", "--map-root-user"])
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Remove a scratch tree, including one handed to another uid by [`disown`].
fn remove_tree(dir: &Path) {
    if std::fs::remove_dir_all(dir).is_err() && dir.exists() {
        in_userns(&["rm", "-rf", &dir.to_string_lossy()]);
    }
}

/// Give `dir` (recursively) to a uid other than ours. `false` when the platform
/// can't (no `unshare`, no subordinate uid range).
#[cfg(unix)]
fn disown(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let ours = std::fs::metadata(dir).map(|m| m.uid()).ok();
    in_userns(&["chown", "-R", "1:1", &dir.to_string_lossy()])
        && std::fs::metadata(dir).map(|m| m.uid()).ok() != ours
}

/// `<scratch>/repo`: one committed `a.txt` whose `.gitattributes` selects the
/// `evil` filter, and a `.git/config` whose `filter.evil.clean` touches `marker`
/// (outside the repository) before passing content through. The worktree copy of
/// `a.txt` is then edited (same size, newer mtime), so status has to hash it — through
/// the clean filter, if it runs. The filter is configured only after the commit, so
/// the fixture's own `git` runs never fire it.
fn fixture(name: &str) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let base = scratch(name);
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    if !git(&repo, &["init", "-q", "-b", "main"]) {
        eprintln!("skip: the `git` CLI is not available");
        remove_tree(&base);
        return None;
    }
    std::fs::write(repo.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
    std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
    assert!(git(&repo, &["add", "-A"]) && git(&repo, &["commit", "-qm", "one"]));

    let marker = base.join("FILTER-RAN");
    let mut config = std::fs::read_to_string(repo.join(".git/config")).unwrap();
    config.push_str(&format!(
        "[filter \"evil\"]\n\tclean = \"touch '{}'; cat\"\n",
        marker.display()
    ));
    std::fs::write(repo.join(".git/config"), config).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(repo.join("a.txt"), "HELLO\n").unwrap();
    Some((base, repo, marker))
}

/// Status of `repo` as `(path, XY)` pairs, plus the notice it carries (if any).
fn status_with_notice(repo: &Path) -> (Vec<(String, String)>, Option<String>) {
    match bemtvi_git::run_git_job(&GitJob::Status {
        path: repo.to_string_lossy().into_owned(),
        ignored: false,
    }) {
        Ok(GitValue::Status {
            entries, notice, ..
        }) => (
            entries
                .into_iter()
                .map(|e| (e.path, format!("{}{}", e.index, e.worktree)))
                .collect(),
            notice,
        ),
        other => panic!("status should succeed, got {other:?}"),
    }
}

fn status(repo: &Path) -> Vec<(String, String)> {
    status_with_notice(repo).0
}

/// git parity: a repository you own is trusted, and its configured clean filter runs
/// when status compares the edited file.
#[test]
fn a_trusted_repository_runs_its_clean_filter_on_status() {
    let Some((base, repo, marker)) = fixture("trusted") else {
        return;
    };
    let entries = status(&repo);
    assert!(
        entries.contains(&("a.txt".into(), " M".into())),
        "the edited file must read as modified: {entries:?}"
    );
    assert!(
        marker.exists(),
        "a trusted repository's clean filter must run, as it does under git"
    );
    remove_tree(&base);
}

/// A repository owned by another user is untrusted: its `.git/config` filter must
/// not run, and status still reports the edit, compared without the filter.
#[cfg(unix)]
#[test]
fn an_untrusted_repository_does_not_run_its_clean_filter() {
    let Some((base, repo, marker)) = fixture("untrusted") else {
        return;
    };
    if !disown(&repo) {
        eprintln!("skip: cannot hand the fixture to another uid (no `unshare --map-auto`)");
        remove_tree(&base);
        return;
    }
    let entries = status(&repo);
    assert!(
        !marker.exists(),
        "an untrusted repository's filter driver ran — a repository owned by another \
         user must not execute commands from its own config"
    );
    assert!(
        entries.contains(&("a.txt".into(), " M".into())),
        "status must still compare the file (without the filter): {entries:?}"
    );
    remove_tree(&base);
}

/// Skipping the filter changes what status computes, so it must not be silent: the
/// result carries a notice naming the repository and the driver that did not run.
#[cfg(unix)]
#[test]
fn an_untrusted_repository_reports_the_filters_it_skipped() {
    let Some((base, repo, _marker)) = fixture("untrusted-notice") else {
        return;
    };
    if !disown(&repo) {
        eprintln!("skip: cannot hand the fixture to another uid (no `unshare --map-auto`)");
        remove_tree(&base);
        return;
    }
    let (_, notice) = status_with_notice(&repo);
    let notice = notice.expect("an untrusted repo's skipped filter must be reported");
    assert!(
        notice.contains("evil") && notice.contains(&*repo.to_string_lossy()),
        "the notice should name the skipped driver and the repository: {notice}"
    );
    remove_tree(&base);
}

/// A trusted repository's filter ran, so there is nothing to report.
#[test]
fn a_trusted_repository_reports_no_skipped_filters() {
    let Some((base, repo, _marker)) = fixture("trusted-notice") else {
        return;
    };
    let (_, notice) = status_with_notice(&repo);
    assert_eq!(notice, None);
    remove_tree(&base);
}
