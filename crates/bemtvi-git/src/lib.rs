//! In-process **git** for bemtvi, via [`gix`] — the native executor behind the async
//! `btv.git.*` Lua API. Replaces every shell-out to the `git` binary (there is no
//! `git` process anywhere once this is wired): the plugin manager's clone/checkout/
//! pull/submodule, and the read verbs the bundled statusline / diff plugins used.
//!
//! Shape mirrors the fs seam ([`bemtvi_lua::run_fs_job`]): one typed [`GitJob`] in,
//! one `Result<`[`GitValue`]`, `[`GitError`]`>` out, pure and synchronous — safe to
//! run on the event-loop actor's blocking pool (native) or daemon-side (a daemon /
//! wasm session; git runs where the files are). The typed job/value/error live in the
//! wasm-safe `bemtvi-lua` crate so the Lua bridge and the daemon wire codec share them;
//! this crate is the native-only engine that turns a job into gix calls. It never
//! runs in a serverless-web session — that rejects loud upstream (no in-browser git).
//!
//! The read verbs are `discover`/`head`/`show`/`diff_file`/`status`; the mutation /
//! network verbs are `clone`/`checkout`/`fetch`/`pull`/`submodule_update` — the
//! plugin-manager backing. gix has no one-call `clone --filter` (partial clone — shallow
//! `depth` substitutes), `submodule update`, or `reset --hard`, so those are hand-rolled
//! here over gix's fetch / worktree-state / reference primitives; see each `fn`.
//!
//! `checkout` has two modes and both matter: DETACHING pins an exact commit, ATTACHING
//! puts HEAD back on a branch so `pull` (which fast-forwards the current *branch*) can run
//! again. `fetch` is `pull`'s half that touches no working state, and its `unshallow` drops
//! a shallow clone's boundary — the two together are what let a lockfile check out an
//! arbitrary recorded revision in place and later move off it.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use bemtvi_lua::{GitError, GitHunk, GitJob, GitStatusEntry, GitValue};

/// Run one [`GitJob`] against the on-disk repository discovered from its path.
/// Pure and synchronous; every error is shaped into a `{ code, message }`
/// [`GitError`] — never a panic, never a silent empty value.
pub fn run_git_job(job: &GitJob) -> Result<GitValue, GitError> {
    match job {
        GitJob::Discover { path } => discover(path),
        GitJob::Head { path } => head(path),
        GitJob::Show { file, rev } => show(file, rev),
        GitJob::DiffFile { path, file } => diff_file(path, file),
        GitJob::Status { path, ignored } => status(path, *ignored),
        GitJob::Clone {
            url,
            dir,
            depth,
            branch,
        } => clone(url, dir, *depth, branch.as_deref()),
        GitJob::Checkout { dir, rev, detach } => checkout(dir, rev, *detach),
        GitJob::Fetch { dir, unshallow } => fetch(dir, *unshallow),
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
/// parent. This keeps `btv.git.show`/`diff_file` reporting `ENOENT` ("no HEAD version") for
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

/// Point `repo`'s HEAD *symbolically* at `branch_ref` — the state `git checkout <branch>`
/// leaves behind, and the inverse of [`detach_head`]. Attaching is what makes a detached
/// checkout movable again: `pull` fast-forwards the current *branch*, so it rejects
/// outright while HEAD names a bare commit.
fn attach_head(repo: &gix::Repository, branch_ref: &gix::refs::FullName) -> Result<(), GitError> {
    use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
    use gix::refs::Target;
    repo.edit_reference(RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: format!("checkout: moving to {}", branch_ref.as_bstr()).into(),
            },
            expected: PreviousValue::Any,
            new: Target::Symbolic(branch_ref.clone()),
        },
        name: "HEAD".try_into().expect("HEAD is a valid ref name"),
        deref: false,
    })
    .map_err(|e| egit("attach HEAD", e))?;
    Ok(())
}

/// Reset `repo`'s worktree to `commit_id` and leave HEAD detached onto it — the state
/// `git checkout --detach <sha>` produces. Shared by [`checkout`]'s detach mode and
/// [`update_submodules_of`], which already holds the opened submodule repository and
/// must not re-discover it from its path just to check it out.
fn checkout_commit(repo: &gix::Repository, commit_id: gix::ObjectId) -> Result<(), GitError> {
    let commit = repo
        .find_object(commit_id)
        .map_err(|e| egit("read object", e))?
        .peel_to_kind(gix::object::Kind::Commit)
        .map_err(|e| err("EGIT", format!("'{commit_id}' is not a commit: {e}")))?;
    // Detach at the *peeled* commit: gix's `rev_parse_single` returns an annotated
    // tag's object id verbatim (like `git rev-parse v1.0`), while `git checkout
    // --detach v1.0` writes the peeled commit into HEAD — the id `head().sha` must
    // name, since a consumer feeding it to a commit-only op (merge-base, log) needs
    // an actual commit.
    let commit_id = commit.id;
    let tree_id = commit
        .peel_to_tree()
        .map_err(|e| egit("peel to tree", e))?
        .id;
    reset_worktree_to_tree(repo, tree_id)?;
    detach_head(repo, commit_id)?;
    Ok(())
}

/// Record that the local branch `short` tracks `remote`, by writing the `branch.<short>.
/// remote` / `.merge` pair `git checkout <branch>` writes when it creates a branch from a
/// remote-tracking ref. This is not cosmetic: [`pull`] resolves the upstream tip through
/// exactly that config (gix's `branch_remote_tracking_ref_name` reads `branch.<n>.merge`),
/// so a branch materialized without it attaches fine and then rejects every pull with "no
/// upstream to pull from" — leaving a commit-pinned checkout as stuck as it was before
/// attaching existed.
///
/// The local sections are written straight back to the repository's own `config` file, the
/// way gix's clone does it for the branch it checks out.
fn set_branch_tracking(
    repo: &mut gix::Repository,
    short: &str,
    full: &gix::refs::FullName,
    remote: &str,
) -> Result<(), GitError> {
    let path = repo.common_dir().join("config");
    let mut snapshot = repo.config_snapshot_mut();
    {
        let mut section = snapshot
            .new_section("branch", Some(gix::bstr::BString::from(short)))
            .map_err(|e| egit("branch config section", e))?;
        section
            .push("remote", Some(gix::bstr::BStr::new(remote)))
            .map_err(|e| egit("write branch.remote", e))?;
        section
            .push("merge", Some(full.as_bstr()))
            .map_err(|e| egit("write branch.merge", e))?;
    }
    let mut out: Vec<u8> = Vec::new();
    snapshot
        .write_to_filter(&mut out, |s| s.meta().source == gix::config::Source::Local)
        .map_err(|e| egit("serialize config", e))?;
    std::fs::write(&path, out).map_err(|e| egit("write config", e))?;
    snapshot.commit().map_err(|e| egit("apply config", e))?;
    Ok(())
}

/// Resolve `name` to a LOCAL branch ref, creating it from its remote-tracking counterpart
/// when only that exists. A fresh clone materializes just the default branch locally —
/// every other branch arrives as `refs/remotes/<remote>/<name>` — so without this,
/// attaching to any non-default branch would fail on a normally-cloned repo, exactly where
/// `git checkout <branch>` succeeds by creating the tracking branch on demand. A branch
/// created here is also set up to TRACK the remote it came from ([`set_branch_tracking`]),
/// without which the pull that attaching exists to enable would still reject.
fn resolve_local_branch(
    repo: &mut gix::Repository,
    name: &str,
) -> Result<(gix::refs::FullName, gix::ObjectId), GitError> {
    let local: gix::refs::FullName = format!("refs/heads/{name}")
        .try_into()
        .map_err(|e| err("EGIT", format!("invalid branch name '{name}': {e}")))?;
    if let Ok(mut r) = repo.find_reference(local.as_ref()) {
        let id = r
            .peel_to_id()
            .map_err(|e| egit("resolve branch", e))?
            .detach();
        return Ok((local, id));
    }
    // No local branch: look for exactly one remote-tracking ref with this name and branch
    // from it. Ambiguity across remotes is an error rather than a guess.
    let mut found: Option<(gix::ObjectId, String)> = None;
    let platform = repo.references().map_err(|e| egit("iterate refs", e))?;
    let iter = platform
        .prefixed("refs/remotes/")
        .map_err(|e| egit("iterate remote refs", e))?;
    for r in iter {
        let mut r = r.map_err(|e| egit("read ref", e))?;
        let full = r.name().as_bstr().to_string();
        // `refs/remotes/<remote>/<name>` — match on the trailing component(s).
        let Some(remote) = full.strip_prefix("refs/remotes/").and_then(|rest| {
            rest.split_once('/')
                .filter(|(_remote, br)| *br == name)
                .map(|(remote, _br)| remote.to_string())
        }) else {
            continue;
        };
        let id = r
            .peel_to_id()
            .map_err(|e| egit("resolve remote branch", e))?
            .detach();
        if found.as_ref().is_some_and(|(prev, _)| *prev != id) {
            return Err(err(
                "EGIT",
                format!("branch '{name}' is ambiguous across remotes"),
            ));
        }
        found = Some((id, remote));
    }
    let (id, remote) = found.ok_or_else(|| {
        err(
            "ENOENT",
            format!("no such branch '{name}' (no local ref and no remote-tracking ref)"),
        )
    })?;
    drop(platform);
    repo.reference(
        local.as_ref(),
        id,
        gix::refs::transaction::PreviousValue::MustNotExist,
        format!("branch: created from remote-tracking ref at {id}"),
    )
    .map_err(|e| egit("create local branch", e))?;
    set_branch_tracking(repo, name, &local, &remote)?;
    Ok((local, id))
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
    // The on-disk index is attacker-influenced (a malicious repo can carry a
    // crafted index): validate every path before deleting, so nothing outside the
    // worktree can be removed. The checks close both escape routes — a `..` /
    // absolute component, and a symlinked intermediate directory redirecting the
    // path out of the worktree (canonicalize follows the links, so the resolved
    // parent must still live under the resolved workdir). A path that fails is
    // dropped rather than deleting through it; the new tree is still checked out
    // below, leaving a stale file behind is strictly safer than removing the
    // wrong one.
    let canon_workdir = std::fs::canonicalize(&workdir).unwrap_or_else(|_| workdir.clone());
    for gone in old_paths.difference(&new_paths) {
        if !is_safe_worktree_path(gone) {
            continue;
        }
        let target = workdir.join(gone);
        // Best-effort: a file already absent (or a dir) is fine — the tree is the truth.
        if let Some(parent) = target.parent() {
            if !std::fs::canonicalize(parent)
                .map(|p| p.starts_with(&canon_workdir))
                .unwrap_or(false)
            {
                continue;
            }
        }
        let _ = std::fs::remove_file(target);
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

/// `btv.git.status` — the working-tree status as ONE porcelain-`XY` entry PER PATH.
///
/// gix reports a path's staged half (`TreeIndex`) and unstaged half (`IndexWorktree`)
/// as separate items, so a file that is staged and then edited again arrives twice,
/// each carrying one filled column. Those are folded here rather than left to the
/// caller: the fold is lossless (the two item kinds can never set the same column),
/// every consumer would otherwise have to repeat it, and a consumer that forgets
/// silently reports "staged, clean worktree" for a file with unstaged edits.
///
/// Worktree rename detection is off by default in gix (git has no config for it
/// either), which makes a renamed-but-unstaged file look like an untracked new path.
/// It is enabled here so a rename reads as `R` on its destination.
///
/// `ignored` additionally emits git-ignored paths (porcelain `!!`), which the dirwalk
/// prunes by default. It is opt-in because that pruning is the whole reason a status over
/// a repo with a big `target/` is fast: asked for, the walk must descend into every
/// ignored directory. `CollapseDirectory` keeps the *result* small — a wholly-ignored
/// directory is reported as itself rather than as each file beneath it — which is what
/// makes this affordable for a file tree that refreshes on every write.
fn status(path: &str, ignored: bool) -> Result<GitValue, GitError> {
    let repo = open(path)?;
    let mut platform = repo
        .status(gix::progress::Discard)
        .map_err(|e| egit("status", e))?
        .index_worktree_rewrites(Some(gix::diff::Rewrites::default()));
    if ignored {
        platform = platform.dirwalk_options(|opts| {
            opts.emit_ignored(Some(gix::dir::walk::EmissionMode::CollapseDirectory))
        });
    }
    let iter = platform.into_iter(None).map_err(|e| egit("status", e))?;

    // Fold by path, preserving first-seen order (`at[path]` indexes into `entries`).
    let mut entries: Vec<GitStatusEntry> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for item in iter {
        let item = item.map_err(|e| egit("status", e))?;
        let Some(entry) = status_entry(&item) else {
            continue;
        };
        match at.get(&entry.path) {
            Some(&i) => {
                // Merge: a field is only meaningful when set, so an unmodified `" "`
                // column (or an empty `orig_path`) from this item never overwrites
                // what the other half recorded. Order-independent by construction —
                // a staged rename that is then edited in the worktree keeps its
                // source whichever item gix yields first.
                let slot: &mut GitStatusEntry = &mut entries[i];
                if entry.index != " " {
                    slot.index = entry.index;
                }
                if entry.worktree != " " {
                    slot.worktree = entry.worktree;
                }
                if !entry.orig_path.is_empty() {
                    slot.orig_path = entry.orig_path;
                }
            }
            None => {
                at.insert(entry.path.clone(), entries.len());
                entries.push(entry);
            }
        }
    }
    Ok(GitValue::Status {
        dirty: !entries.is_empty(),
        entries,
    })
}

// ----- mutation / network verbs ----------------------------------------------

/// `btv.git_local.clone` — clone `url` into `dir` and check out its worktree. `depth`
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

/// `btv.git_local.checkout` — point the repo at `rev` and make the worktree match it, in
/// one of two modes. DETACHING (`detach`) writes HEAD straight at the resolved commit —
/// how an exact pin is applied. ATTACHING (`!detach`) takes `rev` as a BRANCH and leaves
/// HEAD symbolic on it, creating (and setting up to track) the local branch when only a
/// remote-tracking ref exists, as `git checkout <branch>` does. Attaching is what makes a
/// commit-pinned checkout movable again, since [`pull`] fast-forwards the current *branch*
/// and refuses a detached HEAD.
fn checkout(dir: &str, rev: &str, detach: bool) -> Result<GitValue, GitError> {
    if !detach {
        // ATTACH mode: `rev` names a branch, and HEAD ends up symbolic on it with the
        // worktree at its tip — `git checkout <branch>`. This is the mode that makes a
        // detached (commit-pinned) checkout movable again, since `pull` fast-forwards the
        // current *branch* and refuses to run on a bare commit.
        let mut repo = open(dir)?;
        let (branch_ref, id) = resolve_local_branch(&mut repo, rev)?;
        let tree_id = repo
            .find_object(id)
            .map_err(|e| egit("read commit", e))?
            .peel_to_tree()
            .map_err(|e| egit("peel to tree", e))?
            .id;
        reset_worktree_to_tree(&repo, tree_id)?;
        attach_head(&repo, &branch_ref)?;
        return Ok(GitValue::Nil);
    }
    let repo = open(dir)?;
    let id = repo
        .rev_parse_single(rev)
        .map_err(|e| err("ENOENT", format!("no such revision '{rev}': {e}")))?;
    checkout_commit(&repo, id.detach())?;
    Ok(GitValue::Nil)
}

/// `btv.git_local.fetch` — fetch the repo's remote, updating remote-tracking refs but
/// touching neither HEAD nor the worktree (that is `pull`'s job). `unshallow` drops a
/// shallow clone's boundary (`git fetch --unshallow`), which is what makes an older commit
/// a `depth = 1` clone could not contain reachable — the prerequisite for checking out an
/// arbitrary pinned/locked revision in place instead of re-cloning.
fn fetch(dir: &str, unshallow: bool) -> Result<GitValue, GitError> {
    use gix::remote::Direction;

    let repo = open(dir)?;
    let remote = repo
        .find_fetch_remote(None)
        .map_err(|e| egit("find remote", e))?;
    let interrupt = AtomicBool::new(false);
    let connection = remote
        .connect(Direction::Fetch)
        .map_err(|e| egit("connect", e))?;
    let mut prepared = connection
        .prepare_fetch(gix::progress::Discard, Default::default())
        .map_err(|e| egit("prepare fetch", e))?;
    if unshallow {
        // `Shallow::undo()` extends the boundary past every limit — gix's spelling of
        // `--unshallow`. It is a no-op on a repo that was never shallow.
        prepared = prepared.with_shallow(gix::remote::fetch::Shallow::undo());
    }
    prepared
        .receive(gix::progress::Discard, &interrupt)
        .map_err(|e| egit("fetch", e))?;
    Ok(GitValue::Nil)
}

/// `btv.git_local.pull` — fetch the repo's remote and **fast-forward only** the current
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
    if base == new_id {
        // The upstream tip is at or behind our tip (local-only commits): nothing to
        // move the branch to. `git pull --ff-only` prints "Already up to date." and
        // exits 0 in this state — resolving beats rejecting a non-divergence.
        return Ok(GitValue::Pull {
            updated: false,
            sha: old_id.to_string(),
        });
    }
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

/// `btv.git_local.submodule_update` — clone-if-missing (`init`) and check out every
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
    // Base for resolving *relative* submodule URLs (`../dep`, common on GitHub): git
    // resolves them against the superproject's remote — see `resolve_submodule_url`. A
    // nested submodule's base is its OWN remote, which is the absolute URL we clone it
    // from below, so the `recursive` descent stays correct.
    let base_url = superproject_base_url(repo);
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
        let raw_url = sm
            .url()
            .map_err(|e| egit("submodule url", e))?
            .to_bstring()
            .to_string();
        let url = resolve_submodule_url(base_url.as_deref(), &raw_url);

        // Open the submodule if already cloned; otherwise clone it (only under `init`,
        // matching `--init`). A missing, un-init'd submodule is left alone.
        let sub_repo = match sm.open().map_err(|e| egit("open submodule", e))? {
            Some(r) => r,
            None => {
                if !init {
                    continue;
                }
                let _ = std::fs::create_dir_all(&abs);
                clone_at(&url, &abs, None, None)
                    .map_err(|e| err(&e.code, format!("submodule '{name}': {e}", e = e.message)))?
            }
        };
        // Check out the gitlink-pinned commit on the repository we already opened —
        // `checkout` would only re-discover the same repo from `abs`.
        checkout_commit(&sub_repo, pinned)?;

        if recursive {
            update_submodules_of(&sub_repo, init, recursive)?;
        }
    }
    Ok(())
}

/// The base URL a `repo`'s *relative* submodule URLs resolve against — git uses the
/// superproject's fetch remote, falling back to its worktree path when there is no
/// remote (a purely local superproject still resolves `../dep` against its own dir).
fn superproject_base_url(repo: &gix::Repository) -> Option<String> {
    if let Ok(remote) = repo.find_fetch_remote(None) {
        if let Some(url) = remote.url(gix::remote::Direction::Fetch) {
            return Some(url.to_bstring().to_string());
        }
    }
    repo.workdir().map(|p| p.to_string_lossy().into_owned())
}

/// Resolve a submodule's `.gitmodules` URL the way git does. A `./` / `../`-prefixed URL
/// is **relative to the superproject's remote** (`base`); anything else — a real URL,
/// scp-like `git@host:owner/repo`, or an absolute path — is used verbatim. Each `../`
/// drops the last path segment of `base`; `./` is a no-op level. Without this, gix would
/// try to clone `../dep` relative to the process CWD and fail — the exact break a GitHub
/// plugin with relative submodule URLs hit.
fn resolve_submodule_url(base: Option<&str>, raw: &str) -> String {
    if !(raw.starts_with("./") || raw.starts_with("../")) {
        return raw.to_string();
    }
    let base = match base {
        Some(b) => b.trim_end_matches('/'),
        None => return raw.to_string(), // nothing to resolve against — let the clone fail loud
    };
    let mut base = base.to_string();
    let mut rel = raw;
    loop {
        if let Some(r) = rel.strip_prefix("../") {
            base = drop_last_path_segment(&base);
            rel = r;
        } else if let Some(r) = rel.strip_prefix("./") {
            rel = r;
        } else {
            break;
        }
    }
    let base = base.trim_end_matches('/');
    if base.is_empty() {
        rel.to_string()
    } else {
        format!("{base}/{rel}")
    }
}

/// Drop the final path segment of a URL/path string — `.../owner/repo` → `.../owner`.
/// Falls back to the scp-like `host:` boundary when there is no `/` past it.
fn drop_last_path_segment(base: &str) -> String {
    match base.rfind('/') {
        Some(i) => base[..i].to_string(),
        None => match base.rfind(':') {
            Some(i) => base[..=i].to_string(), // keep `host:`
            None => String::new(),
        },
    }
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

/// Whether an index path is safe to touch under the worktree: a *relative* path
/// (`/`-separated) whose every component is a plain name — no empty, `.`, or `..`
/// component, which could escape the worktree when joined onto it. Index paths
/// come from the on-disk index, which a crafted repo can put anything into.
/// (Component-wise, the same rule `gix_validate::path::component` enforces at
/// checkout time.)
fn is_safe_worktree_path(path: &str) -> bool {
    !path.starts_with('/')
        && !path
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
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
            // Match `@@ -old_start,old_count +new_start,new_count @@`. The zero-count
            // side's start is the line BEFORE the hunk, printed as its 0-based index —
            // `git diff -U0` shows `@@ -0,0 +1 @@` for an insertion at the top of the
            // file and `@@ -3,0 +4 @@` for one after line 3, never `-1,0` / `-4,0`.
            old_start: if old_count == 0 {
                h.before.start
            } else {
                h.before.start + 1
            },
            old_count,
            new_start: if new_count == 0 {
                h.after.start
            } else {
                h.after.start + 1
            },
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
/// item that carries no path change we model. Each item fills at most one column (the
/// other stays `" "`, unmodified) — except an untracked path, which porcelain spells
/// `??` in BOTH columns; [`status`] folds the two halves of a path back together.
fn status_entry(item: &gix::status::Item) -> Option<GitStatusEntry> {
    use gix::status::Item;
    match item {
        // A worktree change (unstaged / untracked): the second (`worktree`) column.
        Item::IndexWorktree(change) => {
            let (path, worktree, orig_path) = index_worktree_change(change)?;
            Some(GitStatusEntry {
                path,
                // Untracked is porcelain's `??` and ignored is `!!` — BOTH columns, not
                // a lone worktree letter, which is neither the porcelain code nor a
                // status letter any consumer can read.
                index: if worktree == "?" || worktree == "!" {
                    worktree.clone()
                } else {
                    " ".into()
                },
                worktree,
                orig_path,
            })
        }
        // A staged change (index vs HEAD): the first (`index`) column.
        Item::TreeIndex(change) => {
            let (path, index, orig_path) = tree_index_change(change)?;
            Some(GitStatusEntry {
                path,
                index,
                worktree: " ".into(),
                orig_path,
            })
        }
    }
}

/// The `(path, worktree-column letter, orig_path)` for an unstaged / untracked change.
/// `orig_path` is empty unless the change is a rewrite (rename / copy).
fn index_worktree_change(
    change: &gix::status::index_worktree::Item,
) -> Option<(String, String, String)> {
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
            Some((rela_path.to_string(), letter.into(), String::new()))
        }
        // A dirwalk hit: untracked by default, and — when `status` asked for it —
        // ignored. Both are spelled by their porcelain letter here and doubled into the
        // index column by the caller. A path we can't classify is dropped rather than
        // mis-spelled as untracked: `Pruned`/`Tracked` are walk bookkeeping, not a status.
        Item::DirectoryContents { entry, .. } => {
            let letter = match entry.status {
                gix::dir::entry::Status::Untracked => "?",
                gix::dir::entry::Status::Ignored(_) => "!",
                _ => return None,
            };
            Some((entry.rela_path.to_string(), letter.into(), String::new()))
        }
        // A rewrite is reported against its DESTINATION (`dirwalk_entry.rela_path` is
        // where the content now lives), carrying the source as `orig_path`. Detection
        // is enabled by `index_worktree_rewrites` in [`status`]; without it gix cannot
        // tell a rename from an untracked file, so this arm never fired and a renamed
        // file read as an untracked one.
        Item::Rewrite {
            source,
            dirwalk_entry,
            copy,
            ..
        } => Some((
            dirwalk_entry.rela_path.to_string(),
            if *copy { "C".into() } else { "R".into() },
            source.rela_path().to_string(),
        )),
    }
}

/// The `(path, index-column letter, orig_path)` for a staged change (index vs HEAD
/// tree). `orig_path` is empty unless the change is a rewrite (rename / copy).
fn tree_index_change(change: &gix::diff::index::Change) -> Option<(String, String, String)> {
    use gix::diff::index::Change;
    match change {
        Change::Addition { location, .. } => {
            Some((location.to_string(), "A".into(), String::new()))
        }
        Change::Deletion { location, .. } => {
            Some((location.to_string(), "D".into(), String::new()))
        }
        Change::Modification { location, .. } => {
            Some((location.to_string(), "M".into(), String::new()))
        }
        Change::Rewrite {
            location,
            source_location,
            copy,
            ..
        } => Some((
            location.to_string(),
            if *copy { "C".into() } else { "R".into() },
            source_location.to_string(),
        )),
    }
}
