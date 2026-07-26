//! The test harness must not litter the shared system temp dir.
//!
//! Every harness-created temp path lands inside one per-process **run root**
//! (`$TMPDIR/nxvim-testrun-<pid>`), which is removed when the test binary exits
//! and — for the runs that died without unwinding — swept by the next run that
//! sees a root whose pid is gone. Without that, a `cargo test --workspace` left
//! ~2000 entries (~130 MB) sitting directly in `/tmp` on every invocation.

use std::path::{Path, PathBuf};

use nxvim_test_harness::{
    sweep_stale_temp_roots, temp_dir, temp_path, temp_root, write_n_lines, write_temp,
};

/// The four temp helpers must all produce paths under the single run root, not
/// scattered across the system temp dir. `temp_root()` itself must be a *child*
/// of the system temp dir — being the system temp dir would mean no grouping at
/// all, and nothing to remove at exit.
#[test]
fn every_harness_temp_path_lives_under_the_run_root() {
    let root = temp_root();
    let system = std::env::temp_dir();

    assert_ne!(
        root,
        system,
        "the run root must be its own directory under {}, not the system temp dir itself",
        system.display()
    );
    assert_eq!(
        root.parent(),
        Some(system.as_path()),
        "run root {} should sit directly under the system temp dir",
        root.display()
    );
    assert!(root.is_dir(), "run root {} must exist", root.display());

    let paths: Vec<PathBuf> = vec![
        temp_path("hygiene"),
        temp_dir("hygiene"),
        PathBuf::from(write_temp("hygiene", "lua", "return 1\n")),
        PathBuf::from(write_n_lines("hygiene", 3)),
    ];
    for path in &paths {
        assert_eq!(
            path.parent(),
            Some(root.as_path()),
            "{} escaped the run root {}",
            path.display(),
            root.display()
        );
    }
}

/// A run that dies without unwinding (SIGKILL, `--test-threads` abort, a hard
/// panic under `panic=abort`) never runs its exit hook, so its root survives.
/// The next run must reclaim it — that is what keeps `/tmp` bounded across a
/// history of crashed runs rather than only across clean ones.
#[test]
fn a_run_root_whose_process_is_gone_is_swept() {
    // A genuinely-dead pid: spawn a trivial child and reap it. Reaping is what
    // frees the pid, so nothing else in this test can still be holding it.
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn /bin/true");
    let dead_pid = child.id();
    child.wait().expect("reap /bin/true");

    let stale = std::env::temp_dir().join(format!("nxvim-testrun-{dead_pid}"));
    // The pid could in principle have been recycled between the reap and here;
    // `create_dir_all` keeps the test from failing on that rather than on the
    // behavior under test.
    std::fs::create_dir_all(&stale).expect("plant stale root");
    std::fs::write(stale.join("leftover.txt"), b"stale\n").expect("plant leftover");

    sweep_stale_temp_roots();

    assert!(
        !stale.exists(),
        "stale root {} should have been swept",
        stale.display()
    );
    // The sweep must never touch a *live* run's root — ours.
    assert!(
        temp_root().is_dir(),
        "the sweep removed this run's own root {}",
        temp_root().display()
    );
}

/// A test that leaves an unwritable directory behind (one proving a save into
/// one fails safely, say, that then panicked before restoring the mode) must not
/// pin its root in the temp dir forever: a plain `remove_dir_all` cannot enter
/// that subtree, so the sweep would hit the same wall on every later run.
#[cfg(unix)]
#[test]
fn a_stale_root_holding_an_unwritable_directory_is_still_swept() {
    use std::os::unix::fs::PermissionsExt;

    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn /bin/true");
    let dead_pid = child.id();
    child.wait().expect("reap /bin/true");

    let stale = std::env::temp_dir().join(format!("nxvim-testrun-{dead_pid}"));
    let locked = stale.join("locked");
    std::fs::create_dir_all(&locked).expect("plant stale root");
    std::fs::write(locked.join("inside.txt"), b"x").expect("plant file");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500))
        .expect("make dir unwritable");

    sweep_stale_temp_roots();

    assert!(
        !stale.exists(),
        "an unwritable subtree kept {} from being swept",
        stale.display()
    );
}

/// The run root is registered for removal at process exit. We cannot observe our
/// own exit from inside the run, so assert the contract the exit hook depends on:
/// the root is a single directory that owns every temp path (checked above) and
/// is safe to remove wholesale — i.e. nothing outside it is handed out.
#[test]
fn the_run_root_is_self_contained() {
    let root = temp_root();
    let nested = temp_dir("nested");
    let inner = nested.join("deep");
    std::fs::create_dir(&inner).expect("create nested dir");
    std::fs::write(inner.join("f.txt"), b"x").expect("write nested file");

    assert!(
        inner.starts_with(&root),
        "{} is not under {}",
        inner.display(),
        root.display()
    );
    assert!(
        Path::new(&root).read_dir().expect("read run root").count() > 0,
        "run root should hold this run's temp entries"
    );
}
