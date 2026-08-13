//! Hostile (or merely unlucky) *filesystem shapes* the editor's own read and
//! write paths have to survive. Black-box per the project conventions: a real
//! server over RPC, driven with ex-commands and `nvim_exec_lua`, asserting on the
//! real filesystem.
//!
//! The shapes here are all things a directory can contain, or a caller can ask
//! for, without anyone doing anything unusual to the editor:
//!
//! - a **FIFO** (or any non-regular file) named on the command line or in `:e`,
//!   which the synchronous open blocks on forever;
//! - a **pre-created temp path** in a directory the editor is about to save into,
//!   which an atomic save would write *through* if it opened a predictable name;
//! - a `btv.fs.remove` whose path walked up to the filesystem **root**;
//! - a **non-finite timestamp** handed to the fs seam, which `Duration`'s
//!   constructor turns into a panic rather than an error.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    command, exec_lua, lines, message_after, poll_true, start_attached, temp_dir,
};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// `mkfifo(3)` — a named pipe with no writer. Opening one for reading *blocks*
/// until a writer arrives, which is the whole point of the test.
fn mkfifo(path: &Path) {
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path has no NUL");
    // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
    let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o644) };
    assert_eq!(rc, 0, "mkfifo({}) failed", path.display());
}

/// Opening a FIFO is an *uncancellable* block on the editor's synchronous thread:
/// `File::open` on a reader-side FIFO does not return until some other process
/// opens the write end, and nothing in the editor is left running to notice. A
/// `mkfifo` in a repo (or a `:e` on one) would hang the whole editor with no way
/// out but SIGKILL. The same open path reads `/dev/zero` — a device that never
/// EOFs — straight into a `Vec` until memory runs out.
///
/// So the read path refuses anything that isn't a regular file, loudly. Without
/// the check this test does not fail — it hangs, which is exactly the bug.
#[tokio::test]
async fn opening_a_fifo_fails_loudly_instead_of_blocking_the_editor() {
    let dir = temp_dir("fs_hardening_fifo");
    let fifo = dir.join("pipe");
    mkfifo(&fifo);

    let (rpc, mut incoming) = start().await;
    // The whole point is that this returns at all; bound it so a regression is a
    // failed test rather than a test run that never ends.
    let msg = tokio::time::timeout(
        Duration::from_secs(30),
        message_after(&rpc, &mut incoming, &format!(":edit {}\r", fifo.display())),
    )
    .await
    .expect("`:edit` on a FIFO must return — a blocked open hangs the editor thread");
    assert!(
        msg.contains("not a regular file"),
        "the refusal should name what it refused, got {msg:?}"
    );

    // And the editor is still an editor afterwards.
    assert_eq!(
        exec_lua(&rpc, "return 6 * 7").await,
        rmpv::Value::from(42),
        "the server must still be serving after refusing the FIFO"
    );
}

/// A regular file reached *through* a symlink still opens — the check is on the
/// resolved metadata, not on the link. (`metadata` follows; `symlink_metadata`
/// would not, and would have broken every symlinked file in a repo.)
#[tokio::test]
async fn a_symlink_to_a_regular_file_still_opens() {
    let dir = temp_dir("fs_hardening_symlink_read");
    let real = dir.join("real.txt");
    let link = dir.join("link.txt");
    std::fs::write(&real, "through the link\n").unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let (rpc, _incoming) = start().await;
    command(&rpc, &format!("edit {}", link.display())).await;
    assert_eq!(lines(&rpc).await, vec!["through the link"]);
}

/// The atomic save's temp file is *created* in the target's directory, which on a
/// shared directory (`/tmp`, a group-writable project dir) is a file an attacker
/// can get to first. With a name derived only from the pid — `.notes.txt.bemtvi-tmp.1234`
/// — the attacker can compute it, plant a symlink there pointing at any file the
/// victim can write, and the save opens *through* the link: the buffer's contents
/// land in the attacker's chosen file, and the rename then moves the link over the
/// real target.
///
/// The temp name now carries CSPRNG bytes and is created with `O_EXCL`, so the
/// planted path is never the one opened. This test plants exactly the old
/// predictable name and asserts the victim is untouched.
#[tokio::test]
async fn a_save_does_not_write_through_a_planted_temp_symlink() {
    let dir = temp_dir("fs_hardening_temp_symlink");
    let target = dir.join("notes.txt");
    let victim = dir.join("victim.txt");
    std::fs::write(&target, "original\n").unwrap();
    std::fs::write(&victim, "PRECIOUS\n").unwrap();

    // The pre-hardening temp name, verbatim: `.{file}.bemtvi-tmp.{pid}`. The
    // server runs in-process in this harness, so our pid *is* the writer's.
    let planted = dir.join(format!(".notes.txt.bemtvi-tmp.{}", std::process::id()));
    std::os::unix::fs::symlink(&victim, &planted).unwrap();

    let (rpc, _incoming) = start().await;
    command(&rpc, &format!("edit {}", target.display())).await;
    command(&rpc, "normal! ggdG").await;
    exec_lua(&rpc, r#"btv.cmd("normal! ihostile")"#).await;
    command(&rpc, "write").await;

    assert_eq!(
        std::fs::read_to_string(&victim).unwrap(),
        "PRECIOUS\n",
        "the save wrote through the planted symlink — the temp name must not be \
         predictable, and must be created with O_EXCL"
    );
    assert!(
        std::fs::symlink_metadata(&planted)
            .expect("the planted link should still be there, simply unused")
            .file_type()
            .is_symlink(),
        "the planted path should have been ignored, not opened or replaced"
    );
    // The real save still happened, to the real target, as a regular file.
    assert!(std::fs::read_to_string(&target)
        .unwrap()
        .contains("hostile"));
    assert!(std::fs::symlink_metadata(&target)
        .unwrap()
        .file_type()
        .is_file());
}

/// A successful save leaves no temp behind: every `.bemtvi-tmp.` entry is either
/// renamed into place or removed. (A random name makes leaks *invisible* to a
/// fixed-name check, so this asserts on the prefix instead.)
#[tokio::test]
async fn a_save_leaves_no_temp_file_behind() {
    let dir = temp_dir("fs_hardening_temp_leak");
    let target = dir.join("notes.txt");
    std::fs::write(&target, "original\n").unwrap();

    let (rpc, _incoming) = start().await;
    command(&rpc, &format!("edit {}", target.display())).await;
    exec_lua(&rpc, r#"btv.cmd("normal! ihello")"#).await;
    command(&rpc, "write").await;
    assert!(std::fs::read_to_string(&target).unwrap().contains("hello"));

    let strays: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".bemtvi-tmp."))
        .collect();
    assert!(
        strays.is_empty(),
        "a completed save should leave no temp files, found {strays:?}"
    );
}

/// An existing file's permissions survive the save — the temp is created fresh
/// (with `create_new`'s default mode), so the mode has to be carried over
/// explicitly or every save silently re-permissions the file.
#[tokio::test]
async fn a_save_preserves_the_files_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("fs_hardening_perms");
    let target = dir.join("script.sh");
    std::fs::write(&target, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o750)).unwrap();

    let (rpc, _incoming) = start().await;
    command(&rpc, &format!("edit {}", target.display())).await;
    exec_lua(&rpc, r#"btv.cmd("normal! oecho hi")"#).await;
    command(&rpc, "write").await;

    let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o750,
        "the save must carry the prior mode onto the replacement"
    );
}

/// `btv.fs.remove("/", { recursive = true })` — reachable from any plugin, and
/// from an LSP workspace edit that deletes a `file:///` URI — must refuse. Nothing
/// the editor does has a legitimate reason to walk the filesystem root, and the
/// walk is the one mistake with no undo.
#[tokio::test]
async fn removing_the_filesystem_root_is_refused() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        r#"
        _G.root_err = nil
        btv.fs.remove("/", { recursive = true })
          :next(function() _G.root_err = "RESOLVED" end)
          :catch(function(e) _G.root_err = tostring(e.message or e) end)
        "#,
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.root_err ~= nil").await,
        "the remove promise never settled"
    );
    let err = exec_lua(&rpc, "return _G.root_err").await;
    let err = err.as_str().unwrap_or_default();
    assert!(
        err.contains("refusing to remove the filesystem root"),
        "removing `/` must reject loudly, got {err:?}"
    );
}

/// `LuaFs::utime` is public API, and `Duration::from_secs_f64` **panics** on
/// NaN/±inf rather than erroring. Nothing calls it today — there is no
/// `btv.fs.utime` op — so this is not a live hole; it is the trait staying total,
/// which is what keeps a future op (or an out-of-crate implementation) from
/// turning a bad float into a panic on whichever thread called it.
#[test]
fn a_non_finite_utime_timestamp_is_an_error_not_a_panic() {
    use bemtvi_lua::LuaFs;

    let dir = temp_dir("fs_hardening_utime");
    let file = dir.join("stamped.txt");
    std::fs::write(&file, "x").unwrap();
    let path = file.to_string_lossy().into_owned();
    let fs = bemtvi_lua::StdLuaFs::new();

    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let err = fs
            .utime(&path, bad, 0.0)
            .expect_err("a non-finite timestamp must be a recoverable error");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "for {bad}");
        // …in either position.
        assert!(fs.utime(&path, 0.0, bad).is_err(), "for mtime {bad}");
    }

    // An ordinary timestamp still lands, so the guard didn't cost the operation.
    fs.utime(&path, 1_000_000.0, 1_000_000.0)
        .expect("a finite timestamp must still be applied");
    let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
    let secs = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
        .as_secs();
    assert_eq!(secs, 1_000_000);
}

/// The refusal is for the root *itself*, not for everything under it: an ordinary
/// recursive remove still works, so the guard didn't cost the feature.
#[tokio::test]
async fn an_ordinary_recursive_remove_still_works() {
    let dir = temp_dir("fs_hardening_remove_tree");
    let tree = dir.join("tree");
    std::fs::create_dir_all(tree.join("nested")).unwrap();
    std::fs::write(tree.join("nested/file.txt"), "x").unwrap();

    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        &format!(
            r#"
            _G.rm_done = nil
            btv.fs.remove("{}", {{ recursive = true }})
              :next(function() _G.rm_done = "ok" end)
              :catch(function(e) _G.rm_done = "ERR: " .. tostring(e.message or e) end)
            "#,
            tree.display()
        ),
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.rm_done ~= nil").await,
        "the remove promise never settled"
    );
    let outcome = exec_lua(&rpc, "return _G.rm_done").await;
    assert_eq!(
        outcome.as_str(),
        Some("ok"),
        "an ordinary recursive remove must succeed"
    );
    assert!(!tree.exists(), "the tree should be gone");
}
