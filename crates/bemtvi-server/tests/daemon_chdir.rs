//! The daemon wire protocol, working-directory half — **remote-aware `:cd` / `:pwd` /
//! `getcwd`** (`docs/plans/2026-06-23-remote-cwd.md`).
//!
//! In a daemon session the edit-host runs locally while files live on the remote daemon,
//! so the *daemon's* cwd is the one true working directory. These tests drive a real
//! editor whose async fs is a [`RemoteHostFs`](bemtvi_server::RemoteHostFs) talking to a
//! [`serve_fs_daemon`](bemtvi_server::serve_fs_daemon) over an in-process duplex, with the
//! daemon's cwd seeded through [`ServerInit::remote_cwd`] (the edit-host fetches it over
//! the `config_bundle` handshake in production). The `/virtual/...` paths can't exist on
//! the edit-host's local disk, so a `:pwd` / `getcwd` that reports them can only be the
//! remote cwd, not the local process's — the faithfulness argument the sibling daemon
//! suites make.

use std::path::PathBuf;
use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    await_lines, barrier, exec_lua, feed, message, spawn_with_daemon_fs_init, wait_redraw, DaemonFs,
};
use tokio::sync::mpsc::UnboundedReceiver;

/// A fake daemon fs with `/virtual/proj` (holding `src/` and a README) and
/// `/virtual/proj/src` (holding `main.rs`).
fn fixture() -> DaemonFs {
    let fs = DaemonFs::default();
    fs.dir("/virtual/proj", &[(true, "src"), (false, "README.md")])
        .dir("/virtual/proj/src", &[(false, "main.rs")])
        .set("/virtual/proj/README.md", "# Readme\n")
        .set("/virtual/proj/src/main.rs", "fn main() {}\n");
    fs
}

/// Start a server whose async fs is a [`RemoteHostFs`] talking to a `serve_fs_daemon`
/// (backed by `fake`) over an in-process duplex, opening `file` with the daemon's cwd
/// seeded to `cwd`. UI-attached.
async fn spawn_remote(fake: DaemonFs, cwd: &str, file: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    spawn_with_daemon_fs_init(
        fake,
        ServerInit {
            file: Some(file.to_string()),
            remote_cwd: Some(PathBuf::from(cwd)),
            ..Default::default()
        },
    )
    .await
}

/// The cwd as the server reports it through `vim.fn.getcwd`.
async fn getcwd(rpc: &Rpc) -> String {
    exec_lua(rpc, "return vim.fn.getcwd()")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Poll `getcwd()` until it matches `want` or the budget runs out — a remote `:cd` lands a
/// moment after the command (the daemon `fs_chdir` round trip is off the editor tick).
async fn await_getcwd(rpc: &Rpc, want: &str) -> String {
    for _ in 0..100 {
        if getcwd(rpc).await == want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    getcwd(rpc).await
}

/// A remote session seeds its working directory from the daemon: `vim.fn.getcwd()`
/// reports the daemon's cwd, not the edit-host's local process cwd. The `/virtual/proj`
/// path can't be the local cwd, so this can only be the seeded remote dir.
#[tokio::test]
async fn getcwd_reports_the_daemon_cwd() {
    let (rpc, _incoming) =
        spawn_remote(fixture(), "/virtual/proj", "/virtual/proj/README.md").await;
    assert_eq!(
        getcwd(&rpc).await,
        "/virtual/proj",
        "getcwd() reports the daemon's seeded cwd, not the local process cwd"
    );
    assert_eq!(
        exec_lua(&rpc, "return btv._path_style").await.as_str(),
        Some("posix"),
        "Lua sees the (unix) daemon's path syntax alongside its cwd, whatever this client's OS"
    );
}

/// The Lua path helpers follow the *session's* path syntax — the daemon's — not the
/// client's OS: against a unix daemon, `/virtual/x` is absolute, `\` is an ordinary
/// character, and `:p` joins onto the remote cwd with `/`, even on a Windows client.
#[tokio::test]
async fn lua_path_helpers_follow_the_daemons_syntax() {
    let (rpc, _incoming) =
        spawn_remote(fixture(), "/virtual/proj", "/virtual/proj/README.md").await;
    await_getcwd(&rpc, "/virtual/proj").await;
    let got = exec_lua(
        &rpc,
        "return table.concat({ \
           btv.utils.path_style(), \
           tostring(btv.utils.is_absolute('/virtual/x')), \
           vim.fn.fnamemodify('/virtual/a/b.txt', ':t'), \
           vim.fn.fnamemodify('/virtual/a/b.txt', ':h'), \
           vim.fn.fnamemodify('notes.txt', ':p'), \
           vim.fn.fnamemodify([[a\\b]], ':t'), \
           btv.utils.path_join(btv._cwd, 'a', 'b.txt'), \
           btv.utils.local_path_join('x', 'y') }, '|')",
    )
    .await;
    // A path on the daemon joins by the daemon's rules; one on THIS machine by its own.
    let local = std::path::Path::new("x").join("y");
    assert_eq!(
        got.as_str(),
        Some(
            format!(
                r"unix|true|b.txt|/virtual/a|/virtual/proj/notes.txt|a\b|/virtual/proj/a/b.txt|{}",
                local.display()
            )
            .as_str()
        ),
        "path_style | is_absolute | :t | :h | :p | `\\` is not a separator | path_join | \
         local_path_join"
    );
}

/// `:pwd` prints the daemon's cwd in a remote session (it reads `DirState`, the seeded
/// remote dir, rather than `std::env::current_dir()`).
#[tokio::test]
async fn pwd_prints_the_daemon_cwd() {
    let (rpc, mut incoming) =
        spawn_remote(fixture(), "/virtual/proj", "/virtual/proj/README.md").await;
    feed(&rpc, ":pwd<CR>");
    barrier(&rpc).await;
    let map = wait_redraw(&mut incoming, |m| !message(m).is_empty()).await;
    assert_eq!(
        message(&map),
        "/virtual/proj",
        "`:pwd` reports the daemon's seeded cwd"
    );
}

/// `:cd <abs remote dir>` moves the working directory over the wire (validated +
/// canonicalized by the daemon), and a subsequent *relative* `:e` resolves against the new
/// dir — proving `:cd` actually took effect on the remote, not just in a local mirror.
#[tokio::test]
async fn cd_absolute_moves_cwd_and_relative_edit_follows() {
    let (rpc, _incoming) =
        spawn_remote(fixture(), "/virtual/proj", "/virtual/proj/README.md").await;
    await_getcwd(&rpc, "/virtual/proj").await;

    feed(&rpc, ":cd /virtual/proj/src<CR>");
    assert_eq!(
        await_getcwd(&rpc, "/virtual/proj/src").await,
        "/virtual/proj/src",
        "`:cd <remote dir>` moves the cwd over the wire"
    );

    // A relative `:e` now resolves against the new remote cwd (`/virtual/proj/src`), so it
    // opens that directory's `main.rs` over the wire — not the launch dir's.
    feed(&rpc, ":edit main.rs<CR>");
    assert_eq!(
        await_lines(&rpc, &["fn main() {}"]).await,
        vec!["fn main() {}"],
        "a relative `:edit` after `:cd` reads the file under the new remote cwd"
    );
}

/// `:cd <relative>` resolves against the current effective dir (edit-host side) before the
/// daemon validates it — `:cd src` from `/virtual/proj` lands in `/virtual/proj/src`.
#[tokio::test]
async fn cd_relative_resolves_against_the_effective_dir() {
    let (rpc, _incoming) =
        spawn_remote(fixture(), "/virtual/proj", "/virtual/proj/README.md").await;
    await_getcwd(&rpc, "/virtual/proj").await;

    feed(&rpc, ":cd src<CR>");
    assert_eq!(
        await_getcwd(&rpc, "/virtual/proj/src").await,
        "/virtual/proj/src",
        "`:cd src` resolves against the effective dir and moves into the subdirectory"
    );
}

/// `:cd <missing remote dir>` fails loud with `E344` and leaves the cwd unchanged — the
/// daemon validated the target (its `read_dir` failed), so the error is real, not local.
#[tokio::test]
async fn cd_nonexistent_remote_dir_errors_e344_and_keeps_cwd() {
    let (rpc, mut incoming) =
        spawn_remote(fixture(), "/virtual/proj", "/virtual/proj/README.md").await;
    await_getcwd(&rpc, "/virtual/proj").await;

    feed(&rpc, ":cd /virtual/nope<CR>");
    let map = wait_redraw(&mut incoming, |m| message(m).contains("E344")).await;
    assert!(
        message(&map).contains("E344"),
        "a missing remote directory reports E344 (got {:?})",
        message(&map)
    );
    assert_eq!(
        getcwd(&rpc).await,
        "/virtual/proj",
        "a failed `:cd` leaves the cwd where it was"
    );
}

/// `:cd X` immediately followed by a *relative* `:e Y` (no wait between) resolves Y against
/// X — the optimistic cwd update moves `DirState` the instant `:cd` runs, so the open in
/// the same breath sees the new dir without waiting for the daemon's validation round trip.
#[tokio::test]
async fn cd_then_immediate_relative_edit_uses_the_new_cwd() {
    let (rpc, _incoming) =
        spawn_remote(fixture(), "/virtual/proj", "/virtual/proj/README.md").await;
    await_getcwd(&rpc, "/virtual/proj").await;

    // Back-to-back in one feed — no await between the `:cd` and the `:edit`.
    feed(&rpc, ":cd /virtual/proj/src<CR>:edit main.rs<CR>");
    assert_eq!(
        await_lines(&rpc, &["fn main() {}"]).await,
        vec!["fn main() {}"],
        "a relative `:edit` issued immediately after `:cd` reads the file under the new cwd"
    );
}

/// `:cd <missing>` that moved the cwd *optimistically* rolls back when the daemon rejects
/// it: getcwd returns to the original dir once the `E344` lands.
#[tokio::test]
async fn cd_rolls_back_when_the_daemon_rejects_the_dir() {
    let (rpc, mut incoming) =
        spawn_remote(fixture(), "/virtual/proj", "/virtual/proj/README.md").await;
    await_getcwd(&rpc, "/virtual/proj").await;

    feed(&rpc, ":cd /virtual/nope<CR>");
    // The daemon's `read_dir` fails → `E344` lands and the optimistic move is reverted.
    let map = wait_redraw(&mut incoming, |m| message(m).contains("E344")).await;
    assert!(message(&map).contains("E344"), "a missing dir reports E344");
    assert_eq!(
        await_getcwd(&rpc, "/virtual/proj").await,
        "/virtual/proj",
        "the optimistic cwd is rolled back to the original after the daemon rejects it"
    );
}

/// `:lcd` is window-local over the wire: it moves only the focused window's cwd, and
/// switching back to the other window restores the global dir (the focus hook re-points
/// the `getcwd` mirror per window).
#[tokio::test]
async fn lcd_is_window_local_over_the_wire() {
    let (rpc, _incoming) =
        spawn_remote(fixture(), "/virtual/proj", "/virtual/proj/README.md").await;
    await_getcwd(&rpc, "/virtual/proj").await;

    // Split, then set a window-local cwd in the new (focused) window.
    feed(&rpc, ":split<CR>");
    barrier(&rpc).await;
    feed(&rpc, ":lcd /virtual/proj/src<CR>");
    assert_eq!(
        await_getcwd(&rpc, "/virtual/proj/src").await,
        "/virtual/proj/src",
        "`:lcd` moves the focused window's cwd"
    );

    // The other window still sees the global dir.
    feed(&rpc, "<C-w>w");
    assert_eq!(
        await_getcwd(&rpc, "/virtual/proj").await,
        "/virtual/proj",
        "the unaffected window keeps the global cwd (lcd is window-local)"
    );
}

/// Switching focus across a `:lcd` boundary fires `DirChanged` in a daemon session — the
/// remote analogue of the local `fix_current_dir` announce (Phase 1 only re-pointed the
/// `getcwd` mirror there; now it announces a real move too).
#[tokio::test]
async fn focus_switch_fires_dirchanged_over_the_wire() {
    let (rpc, _incoming) =
        spawn_remote(fixture(), "/virtual/proj", "/virtual/proj/README.md").await;
    await_getcwd(&rpc, "/virtual/proj").await;

    // Record every DirChanged's resulting cwd.
    exec_lua(
        &rpc,
        r#"_G.dir_events = {}
           vim.api.nvim_create_autocmd("DirChanged", {
             callback = function() table.insert(_G.dir_events, vim.v.event.cwd) end,
           })"#,
    )
    .await;

    // Split (the new window inherits the global dir — no boundary), then give the focused
    // window a window-local dir.
    feed(&rpc, ":split<CR>");
    barrier(&rpc).await;
    feed(&rpc, ":lcd /virtual/proj/src<CR>");
    await_getcwd(&rpc, "/virtual/proj/src").await;

    // Switch to the other window (global `/virtual/proj`): a real cwd boundary.
    feed(&rpc, "<C-w>w");
    await_getcwd(&rpc, "/virtual/proj").await;

    // The focus switch announced DirChanged for the destination cwd (the last event).
    let events = exec_lua(&rpc, "return _G.dir_events").await;
    let last = events
        .as_array()
        .and_then(|a| a.last())
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        last, "/virtual/proj",
        "switching back to the global-dir window fires DirChanged with that cwd"
    );
}

/// A **Windows** daemon's paths follow the syntax it announced, not the client's
/// `std::path`: from `C:\proj`, `:cd src` lands in `C:\proj\src` (joined with `\`), a
/// relative `:e` there fetches `C:\proj\src\main.rs`, and `:cd ..` normalizes back to
/// `C:\proj`. On a unix client `std::path` reads `C:\proj` as *relative* and joins with
/// `/`, so this is the cross-OS case a unix CI exercises — the mirror of the `/virtual`
/// suites above (a unix daemon), which a Windows client exercises.
#[tokio::test]
async fn a_windows_daemons_paths_follow_its_announced_syntax() {
    let fs = DaemonFs::default();
    fs.dir(r"C:\proj", &[(true, "src"), (false, "README.md")])
        .dir(r"C:\proj\src", &[(false, "main.rs")])
        .set(r"C:\proj\README.md", "# Readme\n")
        .set(r"C:\proj\src\main.rs", "fn main() {}\n");
    let (rpc, _incoming) = spawn_with_daemon_fs_init(
        fs,
        ServerInit {
            file: Some(r"C:\proj\README.md".to_string()),
            remote_cwd: Some(PathBuf::from(r"C:\proj")),
            remote_path_style: Some(bemtvi_core::pathstyle::PathStyle::Windows),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        await_lines(&rpc, &["# Readme"]).await,
        vec!["# Readme"],
        "the absolute Windows startup path is fetched verbatim"
    );
    await_getcwd(&rpc, r"C:\proj").await;
    assert_eq!(
        exec_lua(&rpc, "return btv._path_style").await.as_str(),
        Some("windows"),
        "Lua sees the daemon's path syntax, whatever this client's OS"
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return btv.utils.path_style() .. '|' \
               .. vim.fn.fnamemodify([[C:\\proj\\a.txt]], ':t') .. '|' \
               .. vim.fn.fnamemodify('a.txt', ':p')",
        )
        .await
        .as_str(),
        Some(r"windows|a.txt|C:\proj\a.txt"),
        "the Lua path helpers follow the daemon's Windows syntax, whatever this client's OS"
    );

    feed(&rpc, ":cd src<CR>");
    assert_eq!(
        await_getcwd(&rpc, r"C:\proj\src").await,
        r"C:\proj\src",
        "a relative `:cd` joins by the daemon's (Windows) syntax"
    );
    feed(&rpc, ":edit main.rs<CR>");
    assert_eq!(
        await_lines(&rpc, &["fn main() {}"]).await,
        vec!["fn main() {}"],
        "a relative `:edit` resolves under the new Windows cwd over the wire"
    );

    feed(&rpc, ":cd ..<CR>");
    assert_eq!(
        await_getcwd(&rpc, r"C:\proj").await,
        r"C:\proj",
        "`..` normalizes by the daemon's syntax"
    );
}
