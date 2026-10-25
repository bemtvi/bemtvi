//! The daemon wire protocol, `btv.git` half (the `git_op` leg) —
//! docs/plans/2026-07-24-native-git-gix.md.
//!
//! Proves async `btv.git.*` runs **on the daemon over a real wire** in a native-daemon
//! session: the editor is given a [`RemoteGitJobs`](bemtvi_server::RemoteGitJobs) as its
//! git seam (so the event-loop actor is `GitBackend::Remote` — it has NO local git and
//! can ONLY send `git_op` requests), and a `serve_git_daemon` answers them over an
//! in-process `tokio::io::duplex`, running `bemtvi_git::run_git_job` against the real repo.
//!
//! Faithful, not a no-op: the actor holds no local git, so a branch name / HEAD blob it
//! reports can only have crossed the wire to the daemon's gix engine. The whole
//! [`GitJob`] is encoded ([`git_job_to_value`](bemtvi_lua)) into one `git_op` request, run
//! daemon-side, and the typed reply decoded back — the same leg (and codec) a web
//! edit-host uses, exercised natively here.
//!
//! `git` must be on PATH to build the fixture (dev/CI have it); absent, the test skips.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::{RemoteGitJobs, ServerInit};
use bemtvi_test_harness::{attach, exec_lua, spawn, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "bemtvi test")
        .env("GIT_AUTHOR_EMAIL", "test@bemtvi")
        .env("GIT_COMMITTER_NAME", "bemtvi test")
        .env("GIT_COMMITTER_EMAIL", "test@bemtvi")
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed in {cwd:?}");
}

fn make_repo(tag: &str) -> PathBuf {
    let repo = temp_dir(tag).join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("file.txt"), "a\nb\nc\n").unwrap();
    // Pin the checkout to the committed bytes. The daemon's git runs with the host's
    // config, and Git for Windows ships `core.autocrlf=true` in its *system* gitconfig,
    // so without this a clone's worktree would hold `a\r\nb\r\n...` — the host's
    // line-ending policy, not anything the wire did.
    std::fs::write(repo.join(".gitattributes"), "* -text\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    repo
}

/// Start a server whose `btv.git` seam is a [`RemoteGitJobs`] talking to a
/// `serve_git_daemon` over an in-process duplex. The actor is `GitBackend::Remote` — no
/// local git — so every `btv.git` op must cross the wire.
async fn spawn_with_daemon_git() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (edit_host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (daemon_reader, daemon_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = bemtvi_server::serve_git_daemon(daemon_reader, daemon_writer).await;
    });

    let (host_reader, host_writer) = tokio::io::split(edit_host_end);
    let remote = RemoteGitJobs::connect(host_reader, host_writer);
    let init = ServerInit {
        git_jobs: Some(remote),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

async fn await_lua_eq(rpc: &Rpc, expr: &str, want: &str) -> bool {
    let code = format!("return tostring({expr})");
    for _ in 0..150 {
        if exec_lua(rpc, &code).await.as_str() == Some(want) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// `btv.git.head` reports the daemon repo's branch over the wire. The actor has no local
/// git, so `main` can only have come from the daemon's gix engine.
#[tokio::test]
async fn btv_git_head_reads_the_daemon_repo_over_the_wire() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let repo = make_repo("daemon_git_head");
    let file = repo.join("file.txt");
    let file_str = file.to_string_lossy().replace('\\', "\\\\");
    let (rpc, _incoming) = spawn_with_daemon_git().await;

    exec_lua(
        &rpc,
        &format!(
            r#"_G.__b = nil
               btv.git.head("{file_str}"):next(
                 function(h) _G.__b = h.branch end,
                 function(e) _G.__b = "err:" .. tostring(e.code) end)
               return 1"#,
        ),
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "_G.__b", "main").await,
        "btv.git.head should resolve with the daemon repo's branch; got {:?}",
        exec_lua(&rpc, "return tostring(_G.__b)").await.as_str(),
    );
}

/// `btv.git.show` fetches the HEAD blob from the daemon over the wire (proving it reads
/// the daemon's object store, not any local file).
#[tokio::test]
async fn btv_git_show_fetches_head_blob_over_the_wire() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let repo = make_repo("daemon_git_show");
    let file = repo.join("file.txt");
    let file_str = file.to_string_lossy().replace('\\', "\\\\");
    let (rpc, _incoming) = spawn_with_daemon_git().await;

    exec_lua(
        &rpc,
        &format!(
            r#"_G.__s = nil
               btv.git.show("{file_str}", "HEAD"):next(
                 function(bytes) _G.__s = bytes end,
                 function(e) _G.__s = "err:" .. tostring(e.code) end)
               return 1"#,
        ),
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "_G.__s", "a\nb\nc\n").await,
        "btv.git.show should resolve with the daemon HEAD blob; got {:?}",
        exec_lua(&rpc, "return tostring(_G.__s)").await.as_str(),
    );
}

/// A Phase-2 mutation verb crosses the wire: `btv.git_local.clone` runs on the daemon
/// (the actor has no local git), and the cloned worktree really lands on disk with the
/// source's committed content. Proves the mutation `GitJob`s ride the same `git_op` leg
/// as the reads.
#[tokio::test]
async fn btv_git_clone_runs_on_the_daemon_over_the_wire() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let src = make_repo("daemon_git_clone_src");
    let src_str = src.to_string_lossy().replace('\\', "\\\\");
    let dest = temp_dir("daemon_git_clone_dest").join("cloned");
    let dest_str = dest.to_string_lossy().replace('\\', "\\\\");
    let (rpc, _incoming) = spawn_with_daemon_git().await;

    exec_lua(
        &rpc,
        &format!(
            r#"_G.__c = nil
               btv.git_local.clone("{src_str}", "{dest_str}"):next(
                 function(dir) _G.__c = "ok" end,
                 function(e) _G.__c = "err:" .. tostring(e.code) end)
               return 1"#,
        ),
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "_G.__c", "ok").await,
        "clone should resolve over the wire; got {:?}",
        exec_lua(&rpc, "return tostring(_G.__c)").await.as_str(),
    );
    // The daemon ran the clone against the real fs — the worktree is on disk.
    assert_eq!(
        std::fs::read_to_string(dest.join("file.txt")).unwrap(),
        "a\nb\nc\n"
    );
}

/// A non-repo path rejects loud (ENOREPO) over the wire — never a silent empty result.
#[tokio::test]
async fn btv_git_discover_rejects_outside_a_repo_over_the_wire() {
    let dir = temp_dir("daemon_git_norepo");
    let dir_str = dir.to_string_lossy().replace('\\', "\\\\");
    let (rpc, _incoming) = spawn_with_daemon_git().await;

    exec_lua(
        &rpc,
        &format!(
            r#"_G.__e = nil
               btv.git.discover("{dir_str}"):next(
                 function() _G.__e = "unexpected-ok" end,
                 function(err) _G.__e = err.code end)
               return 1"#,
        ),
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "_G.__e", "ENOREPO").await,
        "discover outside a repo should reject ENOREPO over the wire; got {:?}",
        exec_lua(&rpc, "return tostring(_G.__e)").await.as_str(),
    );
}

/// The new verbs ride the same `git_op` leg: `fetch` (with `unshallow`) and the ATTACHING
/// `checkout` both cross the wire and take effect on the daemon's disk. Without this the
/// lockfile's restore path would work locally and silently not over a remote session — the
/// tier-1 rule ("the remote session is not a degraded mode") applied to a new git verb.
#[tokio::test]
async fn btv_git_fetch_and_attach_checkout_run_on_the_daemon_over_the_wire() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let src = make_repo("daemon_git_fetch_src");
    let first = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&src)
            .output()
            .expect("git rev-parse")
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    // A second commit, so `first` is unreachable from a depth-1 clone.
    std::fs::write(src.join("second.txt"), "second\n").unwrap();
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-q", "-m", "second"]);

    let src_str = src.to_string_lossy().replace('\\', "\\\\");
    let dest = temp_dir("daemon_git_fetch_dest").join("cloned");
    let dest_str = dest.to_string_lossy().replace('\\', "\\\\");
    let (rpc, _incoming) = spawn_with_daemon_git().await;

    exec_lua(
        &rpc,
        &format!(
            r#"_G.__f = nil
               btv.async(function()
                 btv.await(btv.git_local.clone("{src_str}", "{dest_str}", {{ depth = 1 }}))
                 -- unshallow, then reach the commit the shallow clone could not contain
                 btv.await(btv.git_local.fetch("{dest_str}", {{ unshallow = true }}))
                 btv.await(btv.git_local.checkout("{dest_str}", "{first}", {{ detach = true }}))
                 -- ...and re-attach to the branch (the mode that was unimplemented)
                 btv.await(btv.git_local.checkout("{dest_str}", "main"))
                 local h = btv.await(btv.git.head("{dest_str}"))
                 _G.__f = (h.detached == false) and ("ok:" .. tostring(h.branch)) or "detached"
               end)():catch(function(e) _G.__f = "err:" .. tostring(e and e.message or e) end)
               return 1"#,
        ),
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "_G.__f", "ok:main").await,
        "fetch+attach should resolve over the wire; got {:?}",
        exec_lua(&rpc, "return tostring(_G.__f)").await.as_str(),
    );
    // The daemon really unshallowed the clone on disk.
    assert!(
        !dest.join(".git").join("shallow").exists(),
        "unshallow should have removed .git/shallow on the daemon's disk"
    );
}

/// `opts.ignored` must survive the daemon wire: the flag is a NEW field on the status
/// job, and a codec that forgets to carry it silently degrades a remote session to
/// "no ignored paths" — a feature that works locally and not remotely, which the
/// tier-1-remote rule forbids. The actor has no local git, so a `!!` entry can only
/// have come from the daemon's gix engine having received `ignored = true`.
///
/// Asserted as `<path>=<XY>` pairs so the same expression also proves the flag does not
/// leak into the default call (checked first, with the ignored file already on disk).
#[tokio::test]
async fn btv_git_status_reports_ignored_over_the_wire() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let repo = make_repo("daemon_git_status_ignored");
    std::fs::write(repo.join(".gitignore"), "*.log\n").unwrap();
    git(&repo, &["add", ".gitignore"]);
    git(&repo, &["commit", "-q", "-m", "ignore"]);
    std::fs::write(repo.join("noise.log"), "noise\n").unwrap();
    std::fs::write(repo.join("fresh.txt"), "new\n").unwrap();
    let repo_str = repo.to_string_lossy().replace('\\', "\\\\");
    let (rpc, _incoming) = spawn_with_daemon_git().await;

    // Collect "<path>=<XY>", sorted, into a single string per call.
    let call = |opts: &str| {
        format!(
            r#"_G.__st = nil
               btv.git.status("{repo_str}"{opts}):next(
                 function(r)
                   local out = {{}}
                   for _, e in ipairs(r.entries) do
                     out[#out + 1] = e.path .. "=" .. e.index .. e.worktree
                   end
                   table.sort(out)
                   _G.__st = table.concat(out, ",")
                 end,
                 function(e) _G.__st = "err:" .. tostring(e.code) end)
               return 1"#,
        )
    };

    // Default over the wire: the ignored file stays invisible.
    exec_lua(&rpc, &call("")).await;
    assert!(
        await_lua_eq(&rpc, "_G.__st", "fresh.txt=??").await,
        "default remote status must not report ignored paths; got {:?}",
        exec_lua(&rpc, "return tostring(_G.__st)").await.as_str(),
    );

    // With the flag: the `!!` entry crosses the wire alongside the untracked one.
    exec_lua(&rpc, &call(", { ignored = true }")).await;
    assert!(
        await_lua_eq(&rpc, "_G.__st", "fresh.txt=??,noise.log=!!").await,
        "opts.ignored must cross the daemon wire; got {:?}",
        exec_lua(&rpc, "return tostring(_G.__st)").await.as_str(),
    );
}

/// Run `args` as root of a fresh user namespace mapping the caller's subordinate uids,
/// so a `chown` inside lands on a uid outside that is not ours. `false` if unavailable.
#[cfg(unix)]
fn in_userns(args: &[&str]) -> bool {
    Command::new("unshare")
        .args(["--map-auto", "--map-root-user"])
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Hand `dir` (recursively) to another uid, making the repository in it untrusted.
/// `false` when the platform can't (no `unshare --map-auto` / subordinate uid range).
#[cfg(unix)]
fn disown(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let ours = std::fs::metadata(dir).map(|m| m.uid()).ok();
    in_userns(&["chown", "-R", "1:1", &dir.to_string_lossy()])
        && std::fs::metadata(dir).map(|m| m.uid()).ok() != ours
}

/// Remove a tree [`disown`] may have handed to another uid.
#[cfg(unix)]
fn remove_tree(dir: &Path) {
    if std::fs::remove_dir_all(dir).is_err() && dir.exists() {
        in_userns(&["rm", "-rf", &dir.to_string_lossy()]);
    }
}

/// A repo whose `.gitattributes` puts `file.txt` through the `evil` filter, and whose
/// `.git/config` (written after the commit, so the fixture's own git never runs it)
/// makes that filter's `clean` command touch the returned marker, outside the repo.
/// `file.txt` is then edited at the same size, so status must hash it — through the
/// filter, when the filter runs.
fn make_filter_repo(tag: &str) -> (PathBuf, PathBuf) {
    let repo = temp_dir(tag).join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
    std::fs::write(repo.join("file.txt"), "a\nb\nc\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let marker = repo.parent().unwrap().join("FILTER-RAN");
    let _ = std::fs::remove_file(&marker);
    let mut config = std::fs::read_to_string(repo.join(".git/config")).unwrap();
    config.push_str(&format!(
        "[filter \"evil\"]\n\tclean = \"touch '{}'; cat\"\n",
        // Forward slashes: a `\` inside a quoted git-config value starts an escape
        // (`C:\Users` is an invalid `\U`, failing the whole config), while `sh` —
        // which runs the filter, Git for Windows' too — takes `C:/Users/…` as-is.
        marker.to_string_lossy().replace('\\', "/")
    ));
    std::fs::write(repo.join(".git/config"), config).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(repo.join("file.txt"), "a\nB\nc\n").unwrap();
    (repo, marker)
}

/// Lua that runs `btv.git.status` on `repo` twice, in sequence, recording every
/// `btv.notify` message in `_G.__notes` and the second result as `<path>=<XY>` pairs
/// in `_G.__st` (or `err:<code>`).
fn status_twice_lua(repo: &Path) -> String {
    let repo = repo.to_string_lossy().replace('\\', "\\\\");
    format!(
        r#"_G.__st = nil
           _G.__notes = {{}}
           local orig = btv.notify
           btv.notify = function(m, ...)
             table.insert(_G.__notes, m)
             return orig(m, ...)
           end
           local function fmt(r)
             local out = {{}}
             for _, e in ipairs(r.entries) do
               out[#out + 1] = e.path .. "=" .. e.index .. e.worktree
             end
             table.sort(out)
             return table.concat(out, ",")
           end
           btv.git.status("{repo}"):next(function()
             return btv.git.status("{repo}")
           end):next(
             function(r) _G.__st = fmt(r) end,
             function(e) _G.__st = "err:" .. tostring(e.code) end)
           return 1"#,
    )
}

/// git parity: a repository you own is trusted, so the filter its own config defines
/// runs when status compares an edited file — as `git status` would run it.
#[tokio::test]
async fn btv_git_status_runs_a_trusted_repos_clean_filter_over_the_wire() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (repo, marker) = make_filter_repo("daemon_git_filter_trusted");
    let (rpc, _incoming) = spawn_with_daemon_git().await;
    exec_lua(&rpc, &status_twice_lua(&repo)).await;
    assert!(
        await_lua_eq(&rpc, "_G.__st ~= nil", "true").await,
        "status never settled"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.__st").await.as_str(),
        Some("file.txt= M")
    );
    assert!(
        marker.exists(),
        "a trusted repository's clean filter must run"
    );
    assert_eq!(
        exec_lua(&rpc, "return #_G.__notes").await.as_i64(),
        Some(0),
        "nothing was skipped, so nothing is reported"
    );
}

/// A repository owned by another user is untrusted (git's `safe.directory` rule):
/// statusing it must not run the filter its `.git/config` defines — opening a file in
/// someone else's checkout must not execute their commands — yet status still reports
/// the edit, compared unfiltered, and the user is told ONCE that the filter was skipped.
#[cfg(unix)]
#[tokio::test]
async fn btv_git_status_of_an_untrusted_repo_skips_its_filter_and_says_so_once_over_the_wire() {
    if !have_git() {
        eprintln!("skip: git not on PATH");
        return;
    }
    let (repo, marker) = make_filter_repo("daemon_git_filter_untrusted");
    if !disown(&repo) {
        eprintln!("skip: cannot hand the fixture to another uid (no `unshare --map-auto`)");
        remove_tree(&repo);
        return;
    }
    let (rpc, _incoming) = spawn_with_daemon_git().await;
    exec_lua(&rpc, &status_twice_lua(&repo)).await;
    assert!(
        await_lua_eq(&rpc, "_G.__st ~= nil", "true").await,
        "status never settled"
    );
    let st = exec_lua(&rpc, "return _G.__st").await;
    let notes = exec_lua(&rpc, "return table.concat(_G.__notes, '\\n')").await;
    let count = exec_lua(&rpc, "return #_G.__notes").await.as_i64();
    remove_tree(&repo);
    assert!(
        !marker.exists(),
        "an untrusted repository's filter driver ran"
    );
    assert_eq!(st.as_str(), Some("file.txt= M"));
    assert_eq!(count, Some(1), "told once across two statuses: {notes:?}");
    let notes = notes.as_str().unwrap_or_default().to_string();
    assert!(
        notes.contains("evil") && notes.contains("not trusted"),
        "the notice names the skipped filter: {notes}"
    );
}
