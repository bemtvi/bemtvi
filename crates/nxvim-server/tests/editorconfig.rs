//! Black-box tests for built-in `.editorconfig` support (prelude/editorconfig.lua).
//!
//! Each test lays out a temp project directory containing one or more
//! `.editorconfig` files plus a source file, opens the source file over RPC, and
//! polls the resulting buffer options (`vim.bo.*`). EditorConfig is applied through
//! the async `nx.fs` seam, so a positive assertion polls until it settles; a
//! negative one waits a fixed budget (long enough for the async chain to have run)
//! and then asserts the default still holds.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    command, exec_lua, lua_bool, lua_u64, poll_true, serial_lock, start_attached, temp_dir,
};
use rmpv::Value;
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

/// Restore the process cwd on drop — the relative-path test `:cd`s away from it.
struct CwdGuard(std::path::PathBuf);
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

/// Write `content` to `dir/name`, creating parent directories as needed.
fn write(dir: &Path, name: &str, content: &str) {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(path, content).expect("write file");
}

async fn server() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// `:edit dir/name` over RPC.
async fn edit(rpc: &Rpc, dir: &Path, name: &str) {
    let path = dir.join(name);
    command(rpc, &format!("edit {}", path.to_string_lossy())).await;
}

/// `:edit! dir/name` — force a reload of the current file in place (re-fires
/// BufReadPost). nxvim requires the explicit path even for a reload.
async fn reload(rpc: &Rpc, dir: &Path, name: &str) {
    let path = dir.join(name);
    command(rpc, &format!("edit! {}", path.to_string_lossy())).await;
}

/// Let the async EditorConfig chain run to completion (for negative assertions).
async fn settle() {
    tokio::time::sleep(Duration::from_millis(500)).await;
}

async fn num(rpc: &Rpc, expr: &str) -> Option<u64> {
    lua_u64(rpc, &format!("return {expr}")).await
}

async fn boolean(rpc: &Rpc, expr: &str) -> Option<bool> {
    lua_bool(rpc, &format!("return {expr}")).await
}

async fn string(rpc: &Rpc, expr: &str) -> String {
    match exec_lua(rpc, &format!("return {expr}")).await {
        Value::String(s) => s.into_str().unwrap_or_default(),
        other => panic!("expected string, got {other:?}"),
    }
}

#[tokio::test]
async fn applies_space_indentation() {
    let dir = temp_dir("editorconfig_space");
    write(
        &dir,
        ".editorconfig",
        "root = true\n[*]\nindent_style = space\nindent_size = 2\n",
    );
    write(&dir, "main.txt", "hello\n");
    let (rpc, _inc) = server().await;
    edit(&rpc, &dir, "main.txt").await;

    assert!(
        poll_true(&rpc, "return vim.bo.shiftwidth == 2").await,
        "indent_size=2 should set shiftwidth"
    );
    assert_eq!(
        boolean(&rpc, "vim.bo.expandtab").await,
        Some(true),
        "space => expandtab"
    );
    assert_eq!(num(&rpc, "vim.bo.softtabstop").await, Some(2));
    // tab_width unset => tabstop follows indent_size.
    assert_eq!(num(&rpc, "vim.bo.tabstop").await, Some(2));
}

#[tokio::test]
async fn tab_style_with_distinct_tab_width() {
    let dir = temp_dir("editorconfig_tab");
    write(
        &dir,
        ".editorconfig",
        "root = true\n[*]\nindent_style = tab\nindent_size = 4\ntab_width = 8\n",
    );
    write(&dir, "main.txt", "hello\n");
    let (rpc, _inc) = server().await;
    edit(&rpc, &dir, "main.txt").await;

    assert!(
        poll_true(&rpc, "return vim.bo.tabstop == 8").await,
        "tab_width should set tabstop"
    );
    assert_eq!(
        boolean(&rpc, "vim.bo.expandtab").await,
        Some(false),
        "tab => noexpandtab"
    );
    assert_eq!(
        num(&rpc, "vim.bo.shiftwidth").await,
        Some(4),
        "indent_size sets shiftwidth"
    );
}

#[tokio::test]
async fn section_globs_select_by_extension() {
    let dir = temp_dir("editorconfig_glob");
    write(
        &dir,
        ".editorconfig",
        "root = true\n\
         [*]\nindent_size = 3\n\
         [*.py]\nindent_style = space\nindent_size = 4\n\
         [*.{js,ts}]\nindent_style = space\nindent_size = 2\n",
    );
    write(&dir, "a.py", "x\n");
    write(&dir, "a.ts", "x\n");
    let (rpc, _inc) = server().await;

    edit(&rpc, &dir, "a.py").await;
    assert!(
        poll_true(&rpc, "return vim.bo.shiftwidth == 4").await,
        "[*.py] should win over [*] for a .py file"
    );

    edit(&rpc, &dir, "a.ts").await;
    assert!(
        poll_true(&rpc, "return vim.bo.shiftwidth == 2").await,
        "[*.{{js,ts}}] brace group should match a .ts file"
    );
}

#[tokio::test]
async fn end_of_line_and_charset() {
    let dir = temp_dir("editorconfig_eol");
    write(
        &dir,
        ".editorconfig",
        "root = true\n[*]\nend_of_line = crlf\ncharset = latin1\n",
    );
    write(&dir, "main.txt", "hello\n");
    let (rpc, _inc) = server().await;
    edit(&rpc, &dir, "main.txt").await;

    assert!(
        poll_true(&rpc, "return vim.bo.fileformat == 'dos'").await,
        "crlf => fileformat dos"
    );
    assert_eq!(
        string(&rpc, "vim.bo.fileencoding").await,
        "latin1",
        "charset => fileencoding"
    );
}

#[tokio::test]
async fn property_values_are_case_insensitive() {
    // EditorConfig values are case-insensitive; an uppercased value still applies.
    let dir = temp_dir("editorconfig_case");
    write(
        &dir,
        ".editorconfig",
        "root = true\n[*]\nindent_style = Space\nindent_size = 2\nend_of_line = CRLF\n",
    );
    write(&dir, "main.txt", "hi\n");
    let (rpc, _inc) = server().await;
    edit(&rpc, &dir, "main.txt").await;

    assert!(
        poll_true(&rpc, "return vim.bo.fileformat == 'dos'").await,
        "CRLF (uppercase) => fileformat dos"
    );
    assert_eq!(
        boolean(&rpc, "vim.bo.expandtab").await,
        Some(true),
        "Space (uppercase) => expandtab"
    );
}

#[tokio::test]
async fn root_stops_upward_search() {
    // Parent sets sw=2; the nested dir is `root = true` and sets nothing for the
    // file, so the parent's `[*]` must NOT leak in.
    let dir = temp_dir("editorconfig_root");
    write(
        &dir,
        ".editorconfig",
        "root = true\n[*]\nindent_style = space\nindent_size = 2\n",
    );
    write(
        &dir,
        "sub/.editorconfig",
        "root = true\n[*.md]\nindent_size = 9\n",
    );
    write(&dir, "sub/main.txt", "hi\n");
    let (rpc, _inc) = server().await;
    edit(&rpc, &dir, "sub/main.txt").await;

    settle().await;
    // main.txt matches neither the nested [*.md] nor (blocked by root) the parent [*].
    assert_ne!(
        num(&rpc, "vim.bo.shiftwidth").await,
        Some(2),
        "a root=true nested config must block the parent's rules"
    );
}

#[tokio::test]
async fn nearest_config_overrides_parent() {
    // Parent (root) sets sw=2; child (non-root) sets sw=8. Nearest wins.
    let dir = temp_dir("editorconfig_nearest");
    write(
        &dir,
        ".editorconfig",
        "root = true\n[*]\nindent_style = space\nindent_size = 2\n",
    );
    write(&dir, "sub/.editorconfig", "[*]\nindent_size = 8\n");
    write(&dir, "sub/main.txt", "hi\n");
    let (rpc, _inc) = server().await;
    edit(&rpc, &dir, "sub/main.txt").await;

    assert!(
        poll_true(&rpc, "return vim.bo.shiftwidth == 8").await,
        "the nearer .editorconfig overrides the parent"
    );
    // But expandtab is inherited from the parent (child didn't set indent_style).
    assert_eq!(
        boolean(&rpc, "vim.bo.expandtab").await,
        Some(true),
        "parent indent_style inherited"
    );
}

#[tokio::test]
async fn global_toggle_off_disables() {
    let dir = temp_dir("editorconfig_gtoggle");
    write(
        &dir,
        ".editorconfig",
        "root = true\n[*]\nindent_style = space\nindent_size = 2\n",
    );
    write(&dir, "main.txt", "hi\n");
    let (rpc, _inc) = server().await;

    exec_lua(&rpc, "vim.g.editorconfig = false").await;
    edit(&rpc, &dir, "main.txt").await;
    settle().await;
    assert_ne!(
        num(&rpc, "vim.bo.shiftwidth").await,
        Some(2),
        "vim.g.editorconfig=false should suppress application"
    );
}

#[tokio::test]
async fn buffer_local_override_reenables() {
    // Global off, but the buffer opts back in via vim.b.editorconfig=true; a forced
    // reload then re-fires BufReadPost and applies the config.
    let dir = temp_dir("editorconfig_btoggle");
    write(
        &dir,
        ".editorconfig",
        "root = true\n[*]\nindent_style = space\nindent_size = 6\n",
    );
    write(&dir, "main.txt", "hi\n");
    let (rpc, _inc) = server().await;

    exec_lua(&rpc, "vim.g.editorconfig = false").await;
    edit(&rpc, &dir, "main.txt").await;
    settle().await;
    assert_ne!(
        num(&rpc, "vim.bo.shiftwidth").await,
        Some(6),
        "global off => not applied yet"
    );

    exec_lua(&rpc, "vim.b.editorconfig = true").await;
    reload(&rpc, &dir, "main.txt").await;
    assert!(
        poll_true(&rpc, "return vim.bo.shiftwidth == 6").await,
        "a buffer-local true overrides the global off on reload"
    );
}

#[tokio::test]
async fn buffer_local_off_disables_for_one_buffer() {
    // Global on (default); the buffer opts out via vim.b.editorconfig=false. After a
    // reload, the sentinel shiftwidth is left untouched (EditorConfig skipped).
    let dir = temp_dir("editorconfig_bdisable");
    write(
        &dir,
        ".editorconfig",
        "root = true\n[*]\nindent_style = space\nindent_size = 4\n",
    );
    write(&dir, "main.txt", "hi\n");
    let (rpc, _inc) = server().await;

    edit(&rpc, &dir, "main.txt").await;
    assert!(
        poll_true(&rpc, "return vim.bo.shiftwidth == 4").await,
        "applies while enabled"
    );

    exec_lua(&rpc, "vim.b.editorconfig = false").await;
    reload(&rpc, &dir, "main.txt").await; // reload resets options to defaults
    settle().await;
    assert_ne!(
        num(&rpc, "vim.bo.shiftwidth").await,
        Some(4),
        "vim.b.editorconfig=false should stop EditorConfig from re-applying on reload"
    );
}

#[tokio::test]
async fn exposes_resolved_properties() {
    // Properties with no backing option (trim_trailing_whitespace) are still
    // resolved and queryable via nx.editorconfig.properties().
    let dir = temp_dir("editorconfig_props");
    write(
        &dir,
        ".editorconfig",
        "root = true\n[*]\nindent_size = 2\ntrim_trailing_whitespace = true\n",
    );
    write(&dir, "main.txt", "hi\n");
    let (rpc, _inc) = server().await;
    edit(&rpc, &dir, "main.txt").await;

    assert!(
        poll_true(
            &rpc,
            "local p = nx.editorconfig.properties(0); \
             return p ~= nil and p.trim_trailing_whitespace == 'true' and p.indent_size == '2'"
        )
        .await,
        "resolved properties should be queryable, including unsupported ones"
    );
}

#[tokio::test]
async fn applies_when_opened_by_relative_path() {
    // The everyday case: `:cd project` then `:edit sub/main.txt` (or `nxvim
    // sub/main.txt` from the project root). The buffer's name is the *relative* path
    // as typed, so the upward `.editorconfig` walk must still resolve it against the
    // cwd. `:cd` moves the process cwd, so this holds the serial lock and restores it.
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard::capture();
    let dir = temp_dir("editorconfig_relative");
    write(
        &dir,
        ".editorconfig",
        "root = true\n[*]\nindent_style = space\nindent_size = 2\n",
    );
    write(&dir, "sub/main.txt", "hello\n");
    let (rpc, _inc) = server().await;
    command(&rpc, &format!("cd {}", dir.to_string_lossy())).await;
    command(&rpc, "edit sub/main.txt").await;

    assert!(
        poll_true(&rpc, "return vim.bo.shiftwidth == 2").await,
        "a relatively-opened file should still find the project .editorconfig"
    );
}

#[tokio::test]
async fn applies_to_the_startup_file_argument() {
    // `nxvim main.txt` from the project root: the startup buffer's name is the bare
    // relative argument, and its BufReadPost must resolve the project .editorconfig
    // the same way an explicit `:edit` does.
    let _g = serial_lock().lock().await;
    let _cwd = CwdGuard::capture();
    let dir = temp_dir("editorconfig_argv");
    write(
        &dir,
        ".editorconfig",
        "root = true\n[*]\nindent_style = space\nindent_size = 3\n",
    );
    write(&dir, "main.txt", "hello\n");
    std::env::set_current_dir(&dir).expect("chdir");

    let (rpc, _inc) = start_attached(
        ServerInit {
            file: Some("main.txt".into()),
            ..ServerInit::default()
        },
        80,
        24,
    )
    .await;

    assert!(
        poll_true(&rpc, "return vim.bo.shiftwidth == 3").await,
        "the startup file argument should pick up the project .editorconfig"
    );
}
