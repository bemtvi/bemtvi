//! In-process **git** for nxvim, via [`gix`] — the native executor behind the async
//! `nx.git.*` Lua API. Replaces every shell-out to the `git` binary (there is no
//! `git` process anywhere once this is wired): the plugin manager's clone/checkout/
//! pull/submodule, and the read verbs the bundled statusline / diff plugins used.
//!
//! Shape mirrors the fs seam ([`nxvim_lua::run_fs_job`]): one typed [`GitJob`] in,
//! one `Result<`[`GitValue`]`, `[`GitError`]`>` out, pure and synchronous — safe to
//! run on the event-loop actor's blocking pool (native) or daemon-side (a daemon /
//! wasm session; git runs where the files are). The typed job/value/error live in the
//! wasm-safe `nxvim-lua` crate so the Lua bridge and the daemon wire codec share them;
//! this crate is the native-only engine that turns a job into gix calls. It never
//! runs in a serverless-web session — that rejects loud upstream (no in-browser git).
//!
//! Phase 1 is the read verbs (`discover`/`head`/`show`/`diff_file`/`status`); the
//! mutation / network verbs (clone/checkout/pull/submodule) land in Phase 2.

use std::path::{Path, PathBuf};

use nxvim_lua::{GitError, GitHunk, GitJob, GitStatusEntry, GitValue};

/// Run one [`GitJob`] against the on-disk repository discovered from its path.
/// Pure and synchronous; every error is shaped into a `{ code, message }`
/// [`GitError`] — never a panic, never a silent empty value.
pub fn run_git_job(job: &GitJob) -> Result<GitValue, GitError> {
    match job {
        GitJob::Discover { path } => discover(path),
        GitJob::Head { path } => head(path),
        GitJob::Show { file, rev } => show(file, rev),
        GitJob::DiffFile { path, file } => diff_file(path, file),
        GitJob::Status { path } => status(path),
    }
}

// ----- error shaping ---------------------------------------------------------

/// Build a [`GitError`] with a stable `code` hint and a human `message`.
fn err(code: &str, message: impl Into<String>) -> GitError {
    GitError {
        code: code.into(),
        message: message.into(),
    }
}

/// Map a gix error into a `{ code, message }` under a generic `EGIT` code (callers
/// that need a finer code — `ENOREPO`, `ENOENT` — build it directly).
fn egit(context: &str, e: impl std::fmt::Display) -> GitError {
    err("EGIT", format!("{context}: {e}"))
}

// ----- repo discovery --------------------------------------------------------

/// Open the repository that contains `path` (a file or directory inside a worktree),
/// rejecting with `ENOREPO` when there is none — the discovery `git -C <dir> rev-parse`
/// did before. `gix::discover` walks up from an existing *directory*, so we discover from
/// `path` only when it is a directory; for a file — or a path that doesn't exist yet (a
/// buffer `:edit`ed for a file not on disk, still inside the repo) — we discover from its
/// parent. This keeps `nx.git.show`/`diff_file` reporting `ENOENT` ("no HEAD version") for
/// an uncommitted new file rather than `ENOREPO`.
fn open(path: &str) -> Result<gix::Repository, GitError> {
    let p = Path::new(path);
    let from = if p.is_dir() {
        p
    } else {
        p.parent()
            .filter(|par| !par.as_os_str().is_empty())
            .unwrap_or(p)
    };
    gix::discover(from).map_err(|e| err("ENOREPO", format!("not a git repository ({path}): {e}")))
}

/// The worktree root (toplevel) of `repo`, or `EGIT` for a bare repo (no worktree).
fn workdir(repo: &gix::Repository) -> Result<PathBuf, GitError> {
    repo.workdir()
        .map(Path::to_path_buf)
        .ok_or_else(|| err("EGIT", "bare repository has no worktree"))
}

// ----- verbs -----------------------------------------------------------------

fn discover(path: &str) -> Result<GitValue, GitError> {
    let repo = open(path)?;
    let root = workdir(&repo)?;
    let git_dir = repo.git_dir().to_path_buf();
    // `prefix` = the repo-root→(dir of `path`) relative path, matching `--show-prefix`
    // (empty at the repo root). `path`'s directory is what git's cwd-relative prefix
    // reports; if `path` is itself a directory we use it directly.
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
    let dir = if abs.is_dir() {
        abs.as_path()
    } else {
        abs.parent().unwrap_or(abs.as_path())
    };
    let canon_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
    let prefix = dir
        .strip_prefix(&canon_root)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    Ok(GitValue::Discover {
        root: root.to_string_lossy().into_owned(),
        git_dir: git_dir.to_string_lossy().into_owned(),
        prefix,
    })
}

fn head(path: &str) -> Result<GitValue, GitError> {
    let repo = open(path)?;
    let head = repo.head().map_err(|e| egit("read HEAD", e))?;
    // The short branch name (`main`), or None on a detached HEAD / unborn branch.
    let branch = head.referent_name().map(|full| full.shorten().to_string());
    let detached = branch.is_none();
    // The resolved commit oid. An unborn HEAD (no commits yet) has no id — report an
    // empty sha rather than reject, so a fresh repo still yields a branch name.
    let sha = head.id().map(|id| id.to_string()).unwrap_or_default();
    Ok(GitValue::Head {
        branch,
        detached,
        sha,
    })
}

fn show(file: &str, rev: &str) -> Result<GitValue, GitError> {
    let repo = open(file)?;
    let root = workdir(&repo)?;
    let rel = repo_relative(&root, file)?;
    // Resolve `<rev>:<rel>` to a single object and read its bytes. rev_parse handles
    // the `HEAD:path` navigation syntax; a missing path/rev is the common "no version
    // at HEAD" case (new / untracked file) — a distinct ENOENT so the caller can tell
    // it apart from a real git error.
    let spec = format!("{rev}:{rel}");
    let id = repo
        .rev_parse_single(spec.as_str())
        .map_err(|e| err("ENOENT", format!("no {rev} version of {rel}: {e}")))?;
    let obj = id.object().map_err(|e| egit("read object", e))?;
    let blob = obj
        .peel_to_kind(gix::object::Kind::Blob)
        .map_err(|e| err("ENOENT", format!("{rel} at {rev} is not a file: {e}")))?;
    Ok(GitValue::Bytes(blob.data.clone()))
}

fn diff_file(path: &str, file: &str) -> Result<GitValue, GitError> {
    let repo = open(path)?;
    let root = workdir(&repo)?;
    let rel = repo_relative(&root, file)?;

    // The HEAD blob for this path (empty when the file is new / untracked at HEAD — an
    // all-additions diff), and the current worktree bytes (empty when the file was
    // deleted — an all-removals diff).
    let old = head_blob(&repo, &rel);
    let new = std::fs::read(join(&root, &rel)).unwrap_or_default();

    Ok(diff_counts(&old, &new))
}

fn status(path: &str) -> Result<GitValue, GitError> {
    let repo = open(path)?;
    let platform = repo
        .status(gix::progress::Discard)
        .map_err(|e| egit("status", e))?;
    let iter = platform.into_iter(None).map_err(|e| egit("status", e))?;
    let mut entries = Vec::new();
    for item in iter {
        let item = item.map_err(|e| egit("status", e))?;
        if let Some(entry) = status_entry(&item) {
            entries.push(entry);
        }
    }
    Ok(GitValue::Status {
        dirty: !entries.is_empty(),
        entries,
    })
}

// ----- helpers ---------------------------------------------------------------

/// The repo-relative, forward-slashed path of `file` under `root`. Canonicalizes both
/// so a symlinked worktree dir (e.g. macOS `/var` → `/private/var`) still strips
/// cleanly — the exact breakage the plugins' string-prefix `--show-prefix` math hit.
fn repo_relative(root: &Path, file: &str) -> Result<String, GitError> {
    let abs = std::fs::canonicalize(file).unwrap_or_else(|_| PathBuf::from(file));
    let canon_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    abs.strip_prefix(&canon_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .map_err(|_| err("ENOENT", format!("{file} is not inside the repository")))
}

/// Join a repo-relative path back onto the worktree root.
fn join(root: &Path, rel: &str) -> PathBuf {
    root.join(rel)
}

/// The bytes of `rel` at `HEAD`, or empty when it has no HEAD version (new file /
/// empty repo) — the diff then reads as all-additions, matching `git diff` for an
/// untracked-then-added file.
fn head_blob(repo: &gix::Repository, rel: &str) -> Vec<u8> {
    (|| {
        let id = repo.rev_parse_single(format!("HEAD:{rel}").as_str()).ok()?;
        let obj = id.object().ok()?;
        let blob = obj.peel_to_kind(gix::object::Kind::Blob).ok()?;
        Some(blob.data.clone())
    })()
    .unwrap_or_default()
}

/// Line-diff `old` → `new` and fold into `{ added, changed, removed, hunks }`, the
/// same accounting the plugins did over `git diff -U0` `@@` headers: a hunk with no
/// old lines is additions, none new is removals, both is a change (counted as the
/// larger side).
fn diff_counts(old: &[u8], new: &[u8]) -> GitValue {
    use gix::diff::blob::{sources, Algorithm, Diff, InternedInput};

    let input = InternedInput::new(sources::byte_lines(old), sources::byte_lines(new));
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    // Merge/shift hunks to natural line boundaries so counts and ranges read the way
    // `git diff -U0` presents them (imara's raw hunks can over-segment).
    diff.postprocess_lines(&input);

    let mut hunks = Vec::new();
    let (mut added, mut changed, mut removed) = (0u32, 0u32, 0u32);
    for h in diff.hunks() {
        let old_count = h.before.end - h.before.start;
        let new_count = h.after.end - h.after.start;
        if old_count == 0 {
            added += new_count;
        } else if new_count == 0 {
            removed += old_count;
        } else {
            changed += old_count.max(new_count);
        }
        hunks.push(GitHunk {
            // Unified-diff headers are 1-based; imara's ranges are 0-based line indices.
            // Match `@@ -old_start,old_count +new_start,new_count @@`.
            old_start: h.before.start + 1,
            old_count,
            new_start: h.after.start + 1,
            new_count,
        });
    }
    GitValue::Diff {
        added,
        changed,
        removed,
        hunks,
    }
}

/// Turn one gix status item into a porcelain-`XY` [`GitStatusEntry`], or `None` for an
/// item that carries no path change we model.
fn status_entry(item: &gix::status::Item) -> Option<GitStatusEntry> {
    use gix::status::Item;
    match item {
        // A worktree change (unstaged / untracked): the second (`worktree`) column.
        Item::IndexWorktree(change) => {
            let (path, worktree) = index_worktree_change(change)?;
            Some(GitStatusEntry {
                path,
                index: " ".into(),
                worktree,
            })
        }
        // A staged change (index vs HEAD): the first (`index`) column.
        Item::TreeIndex(change) => {
            let (path, index) = tree_index_change(change)?;
            Some(GitStatusEntry {
                path,
                index,
                worktree: " ".into(),
            })
        }
    }
}

/// The path + worktree-column letter for an unstaged / untracked change.
fn index_worktree_change(change: &gix::status::index_worktree::Item) -> Option<(String, String)> {
    use gix::status::index_worktree::Item;
    use gix::status::plumbing::index_as_worktree::{Change, EntryStatus};
    match change {
        Item::Modification {
            rela_path, status, ..
        } => {
            let letter = match status {
                EntryStatus::Change(Change::Removed) => "D",
                EntryStatus::Change(Change::Modification { .. }) => "M",
                EntryStatus::Change(Change::Type { .. }) => "T",
                EntryStatus::Change(Change::SubmoduleModification(_)) => "M",
                EntryStatus::Conflict { .. } => "U",
                EntryStatus::IntentToAdd => "A",
                EntryStatus::NeedsUpdate(_) => return None,
            };
            Some((rela_path.to_string(), letter.into()))
        }
        Item::DirectoryContents { entry, .. } => Some((entry.rela_path.to_string(), "?".into())),
        Item::Rewrite { .. } => None,
    }
}

/// The path + index-column letter for a staged change (index vs HEAD tree).
fn tree_index_change(change: &gix::diff::index::Change) -> Option<(String, String)> {
    use gix::diff::index::Change;
    let (location, letter) = match change {
        Change::Addition { location, .. } => (location, "A"),
        Change::Deletion { location, .. } => (location, "D"),
        Change::Modification { location, .. } => (location, "M"),
        Change::Rewrite { location, .. } => (location, "R"),
    };
    Some((location.to_string(), letter.into()))
}
