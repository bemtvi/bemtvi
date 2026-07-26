//! The daemon wire protocol, `nx.git` half (the `git_op` leg) —
//! docs/plans/2026-07-24-native-git-gix.md.
//!
//! Proves async `nx.git.*` runs **on the daemon over a real wire** in a native-daemon
//! session: the editor is given a [`RemoteGitJobs`](nxvim_server::RemoteGitJobs) as its
//! git seam (so the event-loop actor is `GitBackend::Remote` — it has NO local git and
//! can ONLY send `git_op` requests), and a `serve_git_daemon` answers them over an
//! in-process `tokio::io::duplex`, running `nxvim_git::run_git_job` against the real repo.
//!
//! Faithful, not a no-op: the actor holds no local git, so a branch name / HEAD blob it
//! reports can only have crossed the wire to the daemon's gix engine. The whole
//! [`GitJob`] is encoded ([`git_job_to_value`](nxvim_lua)) into one `git_op` request, run
//! daemon-side, and the typed reply decoded back — the same leg (and codec) a web
//! edit-host uses, exercised natively here.
//!
//! `git` must be on PATH to build the fixture (dev/CI have it); absent, the test skips.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::{RemoteGitJobs, ServerInit};
use nxvim_test_harness::{attach, exec_lua, spawn, temp_dir};
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
        .env("GIT_AUTHOR_NAME", "nxvim test")
        .env("GIT_AUTHOR_EMAIL", "test@nxvim")
        .env("GIT_COMMITTER_NAME", "nxvim test")
        .env("GIT_COMMITTER_EMAIL", "test@nxvim")
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed in {cwd:?}");
}

fn make_repo(tag: &str) -> PathBuf {
    let repo = temp_dir(tag).join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("file.txt"), "a\nb\nc\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    repo
}

/// Start a server whose `nx.git` seam is a [`RemoteGitJobs`] talking to a
/// `serve_git_daemon` over an in-process duplex. The actor is `GitBackend::Remote` — no
/// local git — so every `nx.git` op must cross the wire.
async fn spawn_with_daemon_git() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (edit_host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (daemon_reader, daemon_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = nxvim_server::serve_git_daemon(daemon_reader, daemon_writer).await;
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

/// `nx.git.head` reports the daemon repo's branch over the wire. The actor has no local
/// git, so `main` can only have come from the daemon's gix engine.
#[tokio::test]
async fn nx_git_head_reads_the_daemon_repo_over_the_wire() {
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
               nx.git.head("{file_str}"):next(
                 function(h) _G.__b = h.branch end,
                 function(e) _G.__b = "err:" .. tostring(e.code) end)
               return 1"#,
        ),
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "_G.__b", "main").await,
        "nx.git.head should resolve with the daemon repo's branch; got {:?}",
        exec_lua(&rpc, "return tostring(_G.__b)").await.as_str(),
    );
}

/// `nx.git.show` fetches the HEAD blob from the daemon over the wire (proving it reads
/// the daemon's object store, not any local file).
#[tokio::test]
async fn nx_git_show_fetches_head_blob_over_the_wire() {
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
               nx.git.show("{file_str}", "HEAD"):next(
                 function(bytes) _G.__s = bytes end,
                 function(e) _G.__s = "err:" .. tostring(e.code) end)
               return 1"#,
        ),
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "_G.__s", "a\nb\nc\n").await,
        "nx.git.show should resolve with the daemon HEAD blob; got {:?}",
        exec_lua(&rpc, "return tostring(_G.__s)").await.as_str(),
    );
}

/// A Phase-2 mutation verb crosses the wire: `nx.git_local.clone` runs on the daemon
/// (the actor has no local git), and the cloned worktree really lands on disk with the
/// source's committed content. Proves the mutation `GitJob`s ride the same `git_op` leg
/// as the reads.
#[tokio::test]
async fn nx_git_clone_runs_on_the_daemon_over_the_wire() {
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
               nx.git_local.clone("{src_str}", "{dest_str}"):next(
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
async fn nx_git_discover_rejects_outside_a_repo_over_the_wire() {
    let dir = temp_dir("daemon_git_norepo");
    let dir_str = dir.to_string_lossy().replace('\\', "\\\\");
    let (rpc, _incoming) = spawn_with_daemon_git().await;

    exec_lua(
        &rpc,
        &format!(
            r#"_G.__e = nil
               nx.git.discover("{dir_str}"):next(
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
async fn nx_git_fetch_and_attach_checkout_run_on_the_daemon_over_the_wire() {
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
               nx.async(function()
                 nx.await(nx.git_local.clone("{src_str}", "{dest_str}", {{ depth = 1 }}))
                 -- unshallow, then reach the commit the shallow clone could not contain
                 nx.await(nx.git_local.fetch("{dest_str}", {{ unshallow = true }}))
                 nx.await(nx.git_local.checkout("{dest_str}", "{first}", {{ detach = true }}))
                 -- ...and re-attach to the branch (the mode that was unimplemented)
                 nx.await(nx.git_local.checkout("{dest_str}", "main"))
                 local h = nx.await(nx.git.head("{dest_str}"))
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
async fn nx_git_status_reports_ignored_over_the_wire() {
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
               nx.git.status("{repo_str}"{opts}):next(
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
