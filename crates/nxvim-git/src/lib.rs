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
//! The read verbs are `discover`/`head`/`show`/`diff_file`/`status`; the mutation /
//! network verbs are `clone`/`checkout`/`pull`/`submodule_update` — the plugin-manager
//! backing. gix has no one-call `clone --filter` (partial clone — shallow `depth`
//! substitutes), `submodule update`, or `reset --hard`, so those are hand-rolled here
//! over gix's fetch / worktree-state / reference primitives; see each `fn`.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

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
        GitJob::Clone {
            url,
            dir,
            depth,
            branch,
        } => clone(url, dir, *depth, branch.as_deref()),
        GitJob::Checkout { dir, rev, detach } => checkout(dir, rev, *detach),
        GitJob::Pull { dir } => pull(dir),
        GitJob::SubmoduleUpdate {
            dir,
            init,
            recursive,
        } => submodule_update(dir, *init, *recursive),
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

/// Detach `repo`'s HEAD onto `id`: replace the symbolic `HEAD` with a direct object
/// target (`deref: false`), the state `git checkout --detach <sha>` leaves behind.
fn detach_head(repo: &gix::Repository, id: gix::ObjectId) -> Result<(), GitError> {
    use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
    use gix::refs::Target;
    repo.edit_reference(RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: format!("checkout: detach HEAD at {id}").into(),
            },
            expected: PreviousValue::Any,
            new: Target::Object(id),
        },
        name: "HEAD".try_into().expect("HEAD is a valid ref name"),
        deref: false,
    })
    .map_err(|e| egit("detach HEAD", e))?;
    Ok(())
}

/// Hand-rolled `reset --hard`: make `repo`'s worktree and index match `tree_id`. gix has
/// no high-level reset, so: build a fresh index from the target tree, delete worktree
/// files the old index tracked but the new tree drops, then check every new entry out
/// over the worktree (overwriting). Adequate for the managed-plugin repos this serves —
/// they carry no local modifications, so there is nothing to preserve or conflict.
fn reset_worktree_to_tree(repo: &gix::Repository, tree_id: gix::ObjectId) -> Result<(), GitError> {
    use std::collections::BTreeSet;

    let workdir = workdir(repo)?;

    // Paths the current index tracks — so we can delete the ones the new tree removes
    // (a plain overwrite-checkout would leave them behind as stale files).
    let old_paths: BTreeSet<String> = {
        let idx = repo.index_or_empty().map_err(|e| egit("open index", e))?;
        idx.entries()
            .iter()
            .map(|e| e.path(&idx).to_string())
            .collect()
    };

    // The index the target tree implies (in-memory, pathed at the repo's index file).
    let mut new_index = repo
        .index_from_tree(&tree_id)
        .map_err(|e| egit("index from tree", e))?;
    let new_paths: BTreeSet<String> = new_index
        .entries()
        .iter()
        .map(|e| e.path(&new_index).to_string())
        .collect();
    for gone in old_paths.difference(&new_paths) {
        // Best-effort: a file already absent (or a dir) is fine — the tree is the truth.
        let _ = std::fs::remove_file(workdir.join(gone));
    }

    // Write every entry of the new index into the worktree, overwriting what's there.
    let mut opts = repo
        .checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping)
        .map_err(|e| egit("checkout options", e))?;
    opts.overwrite_existing = true;
    opts.destination_is_initially_empty = false;
    let objects = repo
        .objects
        .clone()
        .into_arc()
        .map_err(|e| egit("object db", e))?;
    let interrupt = AtomicBool::new(false);
    gix_worktree_state::checkout(
        &mut new_index,
        &workdir,
        objects,
        &gix::progress::Discard,
        &gix::progress::Discard,
        &interrupt,
        opts,
    )
    .map_err(|e| egit("checkout worktree", e))?;

    // Persist the new index so `status`/`diff` see a clean tree afterwards.
    new_index
        .write(Default::default())
        .map_err(|e| egit("write index", e))?;
    Ok(())
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

// ----- mutation / network verbs ----------------------------------------------

/// `nx.git_local.clone` — clone `url` into `dir` and check out its worktree. `depth`
/// makes it shallow (`Some(1)` = tip commit only); `branch` checks out a named branch
/// or tag instead of the remote's default HEAD. Resolves the canonicalized `dir`.
///
/// git's `--filter=blob:none` (partial clone) has no gix analog; shallow `depth`
/// gives the equivalent bandwidth win for the plugin-manager clone. The full flow is
/// gix's own: `prepare_clone` → `fetch_then_checkout` → `main_worktree`. After
/// `main_worktree` takes ownership of the repo, the `PrepareCheckout` no longer removes
/// the worktree on drop, so a successful clone persists.
fn clone(
    url: &str,
    dir: &str,
    depth: Option<u32>,
    branch: Option<&str>,
) -> Result<GitValue, GitError> {
    let repo = clone_at(url, Path::new(dir), depth, branch)?;
    let root = workdir(&repo)?;
    Ok(GitValue::Cloned(root.to_string_lossy().into_owned()))
}

/// The clone primitive shared by [`clone`] and [`submodule_update`] (a submodule is
/// cloned the same way, just into the gitlink's directory). Returns the opened,
/// persisted [`gix::Repository`].
fn clone_at(
    url: &str,
    dir: &Path,
    depth: Option<u32>,
    branch: Option<&str>,
) -> Result<gix::Repository, GitError> {
    use gix::remote::fetch::Shallow;

    let mut prep = gix::prepare_clone(url, dir).map_err(|e| egit("prepare clone", e))?;
    if let Some(nz) = depth.and_then(NonZeroU32::new) {
        prep = prep.with_shallow(Shallow::DepthAtRemote(nz));
    }
    if let Some(b) = branch {
        prep = prep
            .with_ref_name(Some(b))
            .map_err(|e| err("EGIT", format!("invalid branch/tag '{b}': {e}")))?;
    }
    let interrupt = AtomicBool::new(false);
    let (mut checkout, _) = prep
        .fetch_then_checkout(gix::progress::Discard, &interrupt)
        .map_err(|e| egit("fetch", e))?;
    let (repo, _) = checkout
        .main_worktree(gix::progress::Discard, &interrupt)
        .map_err(|e| egit("checkout", e))?;
    Ok(repo)
}

/// `nx.git_local.checkout` — point the repo at `rev` and make the worktree match it.
/// `detach` (the only mode the plugin manager uses — it pins arbitrary commits) writes
/// HEAD straight at the resolved commit; the non-detach path would move the current
/// branch there, which we don't need yet, so it rejects loud rather than silently
/// behaving like `--detach`.
fn checkout(dir: &str, rev: &str, detach: bool) -> Result<GitValue, GitError> {
    if !detach {
        return Err(err(
            "EGIT",
            "checkout without detach is not implemented (the plugin manager only pins commits)",
        ));
    }
    let repo = open(dir)?;
    let id = repo
        .rev_parse_single(rev)
        .map_err(|e| err("ENOENT", format!("no such revision '{rev}': {e}")))?;
    let commit = id
        .object()
        .map_err(|e| egit("read object", e))?
        .peel_to_kind(gix::object::Kind::Commit)
        .map_err(|e| err("EGIT", format!("'{rev}' is not a commit: {e}")))?;
    let commit_id = commit.id;
    let tree_id = commit
        .peel_to_tree()
        .map_err(|e| egit("peel to tree", e))?
        .id;
    reset_worktree_to_tree(&repo, tree_id)?;
    // Detach HEAD onto the commit: replace the symbolic HEAD with a direct object
    // target (`deref: false`), matching `git checkout --detach <sha>`.
    detach_head(&repo, commit_id)?;
    Ok(GitValue::Nil)
}

/// `nx.git_local.pull` — fetch the repo's remote and **fast-forward only** the current
/// branch, updating the worktree. Rejects (never merges) when the remote diverged,
/// matching `git pull --ff-only`. Resolves `{ updated, sha }` — `updated` false when the
/// branch was already at the upstream tip.
///
/// gix has no `pull`; this is the hand-rolled fetch + ref-advance the plan flagged:
/// fetch → read the branch's remote-tracking tip → require the local tip is its
/// ancestor (the ff check via `merge_base`) → move the branch ref and reset the
/// worktree to the new tree.
fn pull(dir: &str) -> Result<GitValue, GitError> {
    use gix::remote::Direction;

    let repo = open(dir)?;
    // Must be on a branch — a detached / unborn HEAD has no upstream to fast-forward.
    let head = repo.head().map_err(|e| egit("read HEAD", e))?;
    let branch_ref = head
        .referent_name()
        .ok_or_else(|| err("EGIT", "cannot pull a detached HEAD"))?
        .to_owned();
    let old_id = head
        .id()
        .ok_or_else(|| err("EGIT", "cannot pull an unborn branch (no commits yet)"))?
        .detach();

    // Fetch the branch's fetch remote (origin for a plugin clone). connect / fetch are
    // blocking here (the `blocking-network-client` feature).
    let remote = repo
        .find_fetch_remote(None)
        .map_err(|e| egit("find remote", e))?;
    let interrupt = AtomicBool::new(false);
    let connection = remote
        .connect(Direction::Fetch)
        .map_err(|e| egit("connect", e))?;
    connection
        .prepare_fetch(gix::progress::Discard, Default::default())
        .map_err(|e| egit("prepare fetch", e))?
        .receive(gix::progress::Discard, &interrupt)
        .map_err(|e| egit("fetch", e))?;

    // The branch's remote-tracking ref (refs/remotes/<remote>/<branch>) now holds the
    // upstream tip the fetch just wrote.
    let tracking = repo
        .branch_remote_tracking_ref_name(branch_ref.as_ref(), Direction::Fetch)
        .ok_or_else(|| err("EGIT", "branch has no upstream to pull from"))?
        .map_err(|e| egit("upstream ref", e))?;
    let new_id = repo
        .find_reference(tracking.as_ref())
        .map_err(|e| egit("find upstream ref", e))?
        .peel_to_id()
        .map_err(|e| egit("resolve upstream", e))?
        .detach();

    if new_id == old_id {
        // Already current — no ref move, no worktree touch.
        return Ok(GitValue::Pull {
            updated: false,
            sha: old_id.to_string(),
        });
    }
    // Fast-forward only: the local tip must be an ancestor of the upstream tip, i.e. the
    // merge base of the two is the local tip. Anything else is a divergence we refuse.
    let base = repo
        .merge_base(old_id, new_id)
        .map_err(|e| err("ENOTFF", format!("not a fast-forward: {e}")))?
        .detach();
    if base != old_id {
        return Err(err(
            "ENOTFF",
            "remote has diverged; refusing non-fast-forward pull",
        ));
    }

    // Advance the branch ref to the upstream tip and bring the worktree along.
    let new_tree = repo
        .find_object(new_id)
        .map_err(|e| egit("read commit", e))?
        .peel_to_tree()
        .map_err(|e| egit("peel to tree", e))?
        .id;
    reset_worktree_to_tree(&repo, new_tree)?;
    repo.reference(
        branch_ref.as_ref(),
        new_id,
        gix::refs::transaction::PreviousValue::MustExistAndMatch(gix::refs::Target::Object(old_id)),
        format!("pull: fast-forward to {new_id}"),
    )
    .map_err(|e| egit("advance branch", e))?;

    Ok(GitValue::Pull {
        updated: true,
        sha: new_id.to_string(),
    })
}

/// `nx.git_local.submodule_update` — clone-if-missing (`init`) and check out every
/// submodule to the commit its parent's gitlink records, `recursive`-ly into nested
/// submodules. Replaces `git submodule update --init --recursive`.
///
/// gix has no one-call submodule update; this walks `repo.submodules()`, and for each
/// one clones it into its worktree dir when absent (init) then [`checkout`]s it to the
/// recorded `index_id` — the same clone/reset primitives the top-level verbs use.
fn submodule_update(dir: &str, init: bool, recursive: bool) -> Result<GitValue, GitError> {
    let repo = open(dir)?;
    update_submodules_of(&repo, init, recursive)?;
    Ok(GitValue::Nil)
}

/// Update every submodule of `repo` (see [`submodule_update`]); factored out so the
/// `recursive` descent can call it on each freshly-checked-out submodule repo.
fn update_submodules_of(
    repo: &gix::Repository,
    init: bool,
    recursive: bool,
) -> Result<(), GitError> {
    let submodules = match repo.submodules().map_err(|e| egit("read submodules", e))? {
        Some(iter) => iter,
        None => return Ok(()), // no .gitmodules — nothing to do
    };
    for sm in submodules {
        let name = sm.name().to_string();
        // The commit the parent's index pins this submodule to (its gitlink). Without
        // one there is nothing to check out.
        let pinned = match sm.index_id().map_err(|e| egit("submodule gitlink", e))? {
            Some(id) => id,
            None => continue,
        };
        // `work_dir()` is already joined onto the superproject worktree — an absolute
        // path when the parent has a worktree (always true here).
        let abs = sm.work_dir().map_err(|e| egit("submodule path", e))?;
        let url = sm
            .url()
            .map_err(|e| egit("submodule url", e))?
            .to_bstring()
            .to_string();

        // Open the submodule if already cloned; otherwise clone it (only under `init`,
        // matching `--init`). A missing, un-init'd submodule is left alone.
        let sub_repo = match sm.open().map_err(|e| egit("open submodule", e))? {
            Some(r) => {
                checkout(&abs.to_string_lossy(), &pinned.to_string(), true)?;
                r
            }
            None => {
                if !init {
                    continue;
                }
                let _ = std::fs::create_dir_all(&abs);
                let r = clone_at(&url, &abs, None, None)
                    .map_err(|e| err(&e.code, format!("submodule '{name}': {e}", e = e.message)))?;
                checkout(&abs.to_string_lossy(), &pinned.to_string(), true)?;
                r
            }
        };

        if recursive {
            update_submodules_of(&sub_repo, init, recursive)?;
        }
    }
    Ok(())
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
