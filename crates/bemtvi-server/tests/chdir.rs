//! Black-box tests for `:cd` / `:chdir` / `:pwd` — the working-directory commands.
//!
//! These mutate the **process** working directory (vim's `:cd` is a real
//! `chdir`), which `vim.fn.getcwd` and every relative-path resolution read back.
//! That global is shared by every test in a binary, so each test holds the
//! process-wide [`serial_lock`] for its whole body and restores the original cwd
//! on the way out (via [`CwdGuard`]) — and this lives in its **own** test binary
//! (its own process) so it can't perturb the cwd-reading tests in other suites.

use std::path::{Path, PathBuf};

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    barrier, command, exec_lua, feed, message_after, serial_lock, start_attached, temp_dir,
};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Restore the process cwd to what it was when constructed — so a test that
/// changes the cwd (or panics mid-way) doesn't leak it to the next one.
struct CwdGuard(PathBuf);
impl CwdGuard {
    fn capture() -> Self {
        CwdGuard(std::env::current_dir().expect("cwd"))
    }
}
impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

/// The cwd as the server reports it (`vim.fn.getcwd`).
async fn getcwd(rpc: &Rpc) -> String {
    exec_lua(rpc, "return vim.fn.getcwd()")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Cycle focus to the next window (`<C-w>w`) and settle.
async fn cycle_window(rpc: &Rpc) {
    feed(rpc, "<C-w>w");
    barrier(rpc).await;
}

/// The kernel-canonical form of `p` (symlinks resolved) as a string — what
/// `getcwd()` returns after a `set_current_dir`, regardless of how the path was
/// typed (e.g. `/tmp` is a symlink on some systems).
fn canon(p: &Path) -> String {
    std::fs::canonicalize(p)
        .expect("canonicalize")
        .to_string_lossy()
        .into_owned()
}

#[tokio::test]
async fn cd_changes_working_directory() {
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard::capture();
    let (rpc, _incoming) = start().await;

    let dir = temp_dir("cd");
    command(&rpc, &format!("cd {}", dir.display())).await;
    assert_eq!(getcwd(&rpc).await, canon(&dir));
}

#[tokio::test]
async fn cd_dash_toggles_previous_directory() {
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard::capture();
    let (rpc, _incoming) = start().await;

    let origin = getcwd(&rpc).await;
    let dir = temp_dir("cd_dash");

    command(&rpc, &format!("cd {}", dir.display())).await;
    assert_eq!(getcwd(&rpc).await, canon(&dir));

    // `:cd -` returns to where we came from …
    command(&rpc, "cd -").await;
    assert_eq!(getcwd(&rpc).await, origin);

    // … and again toggles back (the previous dir is updated each `:cd`).
    command(&rpc, "cd -").await;
    assert_eq!(getcwd(&rpc).await, canon(&dir));
}

#[tokio::test]
async fn cd_dash_without_history_errors() {
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard::capture();
    let (rpc, mut incoming) = start().await;

    // No `:cd` has run yet, so there is no previous directory.
    let before = getcwd(&rpc).await;
    let msg = message_after(&rpc, &mut incoming, ":cd -<CR>").await;
    assert!(msg.contains("E186"), "expected E186, got {msg:?}");
    assert_eq!(getcwd(&rpc).await, before, "cwd must be unchanged");
}

#[tokio::test]
async fn cd_no_arg_goes_home() {
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard::capture();
    let (rpc, _incoming) = start().await;

    // Move away from home first so the jump is observable.
    let dir = temp_dir("cd_home");
    command(&rpc, &format!("cd {}", dir.display())).await;

    let home = std::env::var("HOME").expect("HOME set in test env");
    command(&rpc, "cd").await;
    assert_eq!(getcwd(&rpc).await, canon(Path::new(&home)));
}

#[tokio::test]
async fn cd_tilde_expands_to_home() {
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard::capture();
    let (rpc, _incoming) = start().await;

    let dir = temp_dir("cd_tilde");
    command(&rpc, &format!("cd {}", dir.display())).await;

    let home = std::env::var("HOME").expect("HOME set in test env");
    command(&rpc, "cd ~").await;
    assert_eq!(getcwd(&rpc).await, canon(Path::new(&home)));
}

#[tokio::test]
async fn cd_nonexistent_directory_errors_and_keeps_cwd() {
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard::capture();
    let (rpc, mut incoming) = start().await;

    let dir = temp_dir("cd_bad");
    command(&rpc, &format!("cd {}", dir.display())).await;
    let before = getcwd(&rpc).await;

    let bad = dir.join("does/not/exist");
    let msg = message_after(&rpc, &mut incoming, &format!(":cd {}<CR>", bad.display())).await;
    assert!(msg.contains("E344"), "expected E344, got {msg:?}");
    assert_eq!(getcwd(&rpc).await, before, "a failed :cd must not move");
}

#[tokio::test]
async fn pwd_prints_working_directory() {
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard::capture();
    let (rpc, mut incoming) = start().await;

    let dir = temp_dir("pwd");
    command(&rpc, &format!("cd {}", dir.display())).await;

    let msg = message_after(&rpc, &mut incoming, ":pwd<CR>").await;
    assert_eq!(msg, canon(&dir));
}

#[tokio::test]
async fn cd_fires_dirchanged_autocmd() {
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard::capture();
    let (rpc, _incoming) = start().await;

    // A DirChanged handler that records the new cwd (from v:event) and the match
    // (the scope pattern) it was fired with.
    exec_lua(
        &rpc,
        r#"_G.dir_event = nil
           vim.api.nvim_create_autocmd("DirChanged", {
             callback = function(args)
               _G.dir_event = { cwd = vim.v.event.cwd, scope = args.match, file = args.file }
             end,
           })"#,
    )
    .await;

    let dir = temp_dir("cd_au");
    command(&rpc, &format!("cd {}", dir.display())).await;

    let want = canon(&dir);
    let cwd = exec_lua(&rpc, "return _G.dir_event and _G.dir_event.cwd")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string();
    let scope = exec_lua(&rpc, "return _G.dir_event and _G.dir_event.scope")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string();
    let file = exec_lua(&rpc, "return _G.dir_event and _G.dir_event.file")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_eq!(cwd, want, "v:event.cwd");
    assert_eq!(scope, "global", "DirChanged pattern is the scope");
    assert_eq!(file, want, "<afile> is the new directory");
}

#[tokio::test]
async fn lcd_is_window_local() {
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard::capture();
    let (rpc, _incoming) = start().await;

    let origin = getcwd(&rpc).await;
    let dir = temp_dir("lcd");

    // Two windows; both start on the global dir.
    command(&rpc, "split").await;
    assert_eq!(getcwd(&rpc).await, origin);

    // `:lcd` binds the dir to the *current* window only.
    command(&rpc, &format!("lcd {}", dir.display())).await;
    assert_eq!(getcwd(&rpc).await, canon(&dir));

    // The other window is unaffected — it falls back to the global dir …
    cycle_window(&rpc).await;
    assert_eq!(getcwd(&rpc).await, origin);

    // … and returning to the first window restores its local dir.
    cycle_window(&rpc).await;
    assert_eq!(getcwd(&rpc).await, canon(&dir));
}

#[tokio::test]
async fn tcd_is_tab_local() {
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard::capture();
    let (rpc, _incoming) = start().await;

    let origin = getcwd(&rpc).await;
    let dir = temp_dir("tcd");

    // A second tab; it starts on the global dir.
    command(&rpc, "tabnew").await;
    assert_eq!(getcwd(&rpc).await, origin);

    // `:tcd` binds the dir to the current tab page.
    command(&rpc, &format!("tcd {}", dir.display())).await;
    assert_eq!(getcwd(&rpc).await, canon(&dir));

    // The first tab is unaffected (global dir) …
    command(&rpc, "tabp").await;
    assert_eq!(getcwd(&rpc).await, origin);

    // … and the second still has its tab-local dir.
    command(&rpc, "tabnext").await;
    assert_eq!(getcwd(&rpc).await, canon(&dir));
}

#[tokio::test]
async fn scope_override_order_window_over_tab_over_global() {
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard::capture();
    let (rpc, _incoming) = start().await;

    let global = getcwd(&rpc).await;
    let tab_dir = temp_dir("ovr_tab");
    let win_dir = temp_dir("ovr_win");

    command(&rpc, "split").await;
    // Tab-local applies to every window in the tab without its own local dir.
    command(&rpc, &format!("tcd {}", tab_dir.display())).await;
    assert_eq!(getcwd(&rpc).await, canon(&tab_dir));

    // A window-local dir overrides the tab-local one for *this* window.
    command(&rpc, &format!("lcd {}", win_dir.display())).await;
    assert_eq!(getcwd(&rpc).await, canon(&win_dir));

    // The other window has no window-local dir, so it still sees the tab-local one
    // (proving window > tab > global resolution).
    cycle_window(&rpc).await;
    assert_eq!(getcwd(&rpc).await, canon(&tab_dir));
    cycle_window(&rpc).await;
    assert_eq!(getcwd(&rpc).await, canon(&win_dir));

    let _ = global;
}

#[tokio::test]
async fn cd_clears_window_and_tab_local_dirs() {
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard::capture();
    let (rpc, _incoming) = start().await;

    let tab_dir = temp_dir("clr_tab");
    let win_dir = temp_dir("clr_win");
    let global = temp_dir("clr_glob");

    command(&rpc, "split").await;
    command(&rpc, &format!("tcd {}", tab_dir.display())).await;
    command(&rpc, &format!("lcd {}", win_dir.display())).await;
    assert_eq!(getcwd(&rpc).await, canon(&win_dir));

    // `:cd` drops the current window-local *and* tab-local dirs, then sets global.
    command(&rpc, &format!("cd {}", global.display())).await;
    assert_eq!(getcwd(&rpc).await, canon(&global));

    // The other window now also resolves to the new global dir — the tab-local dir
    // is gone, not merely shadowed.
    cycle_window(&rpc).await;
    assert_eq!(getcwd(&rpc).await, canon(&global));
}

#[tokio::test]
async fn lcd_dash_toggles_window_previous_dir() {
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard::capture();
    let (rpc, _incoming) = start().await;

    let a = temp_dir("lcd_a");
    let b = temp_dir("lcd_b");

    command(&rpc, &format!("lcd {}", a.display())).await;
    command(&rpc, &format!("lcd {}", b.display())).await;
    assert_eq!(getcwd(&rpc).await, canon(&b));

    command(&rpc, "lcd -").await;
    assert_eq!(getcwd(&rpc).await, canon(&a));
}

#[tokio::test]
async fn lcd_and_tcd_fire_dirchanged_with_their_scope() {
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard::capture();
    let (rpc, _incoming) = start().await;

    exec_lua(
        &rpc,
        r#"_G.last_scope = nil
           vim.api.nvim_create_autocmd("DirChanged", {
             callback = function(args) _G.last_scope = args.match end,
           })"#,
    )
    .await;

    let scope_after = |cmd: String| {
        let rpc = &rpc;
        async move {
            command(rpc, &cmd).await;
            exec_lua(rpc, "return _G.last_scope")
                .await
                .as_str()
                .unwrap_or_default()
                .to_string()
        }
    };

    let win_dir = temp_dir("scope_win");
    let tab_dir = temp_dir("scope_tab");
    assert_eq!(
        scope_after(format!("lcd {}", win_dir.display())).await,
        "window"
    );
    assert_eq!(
        scope_after(format!("tcd {}", tab_dir.display())).await,
        "tabpage"
    );
}

/// A `--workspace <dir>` launch (`workspace_dir` + `workspace_cwd`) cds into that directory
/// at boot, so `getcwd()` reports the workspace root with no `:cd` typed — the canonical
/// `bemtvi --workspace DIR`.
#[tokio::test]
async fn workspace_launch_cds_into_the_workspace_dir() {
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard::capture();

    let dir = temp_dir("ws-cwd");
    let init = ServerInit {
        workspace_dir: Some(dir.to_string_lossy().into_owned()),
        workspace_cwd: true,
        ..ServerInit::default()
    };
    let (rpc, _incoming) = start_attached(init, 80, 24).await;
    assert_eq!(getcwd(&rpc).await, canon(&dir));
}

/// `--workspace-no-cwd` (`workspace_cwd = false`): the workspace launch keeps the cwd it was
/// started from, even though `workspace_dir` is set — the cd is purely a CLI decision now,
/// not a Lua option, so there is no init.lua override to honor.
#[tokio::test]
async fn workspace_no_cwd_keeps_the_launch_cwd() {
    let _g = serial_lock().lock().await;
    let cwd = CwdGuard::capture();
    let before = getcwd_raw();

    let dir = temp_dir("ws-cwd-off");
    let init = ServerInit {
        workspace_dir: Some(dir.to_string_lossy().into_owned()),
        workspace_cwd: false,
        ..ServerInit::default()
    };
    let (rpc, _incoming) = start_attached(init, 80, 24).await;
    assert_eq!(
        getcwd(&rpc).await,
        before,
        "--workspace-no-cwd must not move the cwd"
    );
    drop(cwd);
}

/// The launch process cwd as a string, for comparing against `getcwd()` after a no-op
/// startup (no auto-cd). `current_dir()` already returns the kernel-canonical path.
fn getcwd_raw() -> String {
    std::env::current_dir()
        .expect("cwd")
        .to_string_lossy()
        .into_owned()
}
