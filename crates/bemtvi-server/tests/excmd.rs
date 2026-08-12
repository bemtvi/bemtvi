//! Behavior tests for ex-command surface the core defers to the server resolver
//! (`excmd.rs`): the `:silent[!]` command modifier and `:source`. Driven black-box
//! over RPC like the other suites; the message line (read off the latest `redraw`)
//! is the observable for suppression / errors, and a `vim.g` marker proves a Lua
//! script actually ran. The local `redraw_after` mirrors the take-latest pattern
//! the harness documents (integration-test files don't share a module).

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{attach, exec_lua, feed, lines, message_after, spawn, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start(dir: &std::path::Path) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(dir.join("init.lua"), "").expect("write init.lua");
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// `:silent! {cmd}` swallows the error a bare `{cmd}` would echo — a plugin manager
/// relies on this for `silent! runtime <optional file>`. The unmodified command still
/// reports loudly (the modifier is the only thing suppressing it).
#[tokio::test]
async fn silent_bang_suppresses_command_error() {
    let dir = temp_dir("excmd_silent");
    let (rpc, mut incoming) = start(&dir).await;

    // The bare unknown command reports E492 on the message line.
    let loud = message_after(&rpc, &mut incoming, ":NotARealCommand<CR>").await;
    assert!(
        loud.contains("E492"),
        "an unknown command should report E492, got {loud:?}"
    );

    // `silent!` swallows it: the message line is left clean.
    let quiet = message_after(&rpc, &mut incoming, ":silent! NotARealCommand<CR>").await;
    assert!(
        !quiet.contains("E492") && quiet.trim().is_empty(),
        "silent! should suppress the error, got {quiet:?}"
    );
}

/// A **bare** `:silent` keeps errors: it suppresses ordinary output only, exactly as
/// in vim, where an error switches messages back on (`msg_silent` is reset in
/// `emsg`). Only the `!` form swallows the error — the two must not be the same
/// command.
#[tokio::test]
async fn bare_silent_hides_output_but_keeps_errors() {
    let dir = temp_dir("excmd_silent_bare");
    let (rpc, mut incoming) = start(&dir).await;

    // The control: `:echomsg` shows its text and records it.
    let loud = message_after(&rpc, &mut incoming, ":echom 'SHOWN'<CR>").await;
    assert_eq!(
        loud.trim(),
        "SHOWN",
        "control: expected output, got {loud:?}"
    );

    // Ordinary output goes away under the modifier…
    let quiet = message_after(&rpc, &mut incoming, ":silent echom 'HIDDEN'<CR>").await;
    assert!(
        !quiet.contains("HIDDEN"),
        ":silent should suppress ordinary output, got {quiet:?}"
    );

    // …but an error still reports — the whole difference from `silent!`.
    let err = message_after(&rpc, &mut incoming, ":silent NotARealCommand<CR>").await;
    assert!(
        err.contains("E492"),
        "a bare :silent must keep the error visible, got {err:?}"
    );

    // The same split holds in `:messages`: the suppressed line never reaches the
    // history (vim skips the entry outright under `msg_silent`), the kept error does.
    feed(&rpc, ":messages<CR>");
    let history = lines(&rpc).await;
    assert!(
        !history.iter().any(|l| l.contains("HIDDEN")),
        "a silenced message must not reach :messages: {history:?}"
    );
    assert!(
        history.iter().any(|l| l.contains("E492")),
        "the kept error should be logged: {history:?}"
    );
    assert!(
        history.iter().any(|l| l.contains("SHOWN")),
        "the control message is in the history: {history:?}"
    );
}

/// `btv.cmd(cmd, { silent = … })` — the Lua funnel's modifier table. It compiles to
/// the very same `:silent[!]` modifier, so the message-line behavior matches what
/// typing it does, including keeping an error under the bare form.
#[tokio::test]
async fn btv_cmd_silent_option() {
    let dir = temp_dir("excmd_cmd_silent");
    let (rpc, mut incoming) = start(&dir).await;

    // Without the option the command's output shows (the control).
    let loud = message_after(&rpc, &mut incoming, ":lua btv.cmd(\"echo 'SHOWN'\")<CR>").await;
    assert!(
        loud.contains("SHOWN"),
        "control: expected output, got {loud:?}"
    );

    let quiet = message_after(
        &rpc,
        &mut incoming,
        ":lua btv.cmd(\"echo 'HIDDEN'\", { silent = true })<CR>",
    )
    .await;
    assert!(
        !quiet.contains("HIDDEN"),
        "{{ silent = true }} should suppress the output, got {quiet:?}"
    );

    // `silent` alone keeps an error…
    let err = message_after(
        &rpc,
        &mut incoming,
        ":lua btv.cmd('NotARealCommand', { silent = true })<CR>",
    )
    .await;
    assert!(
        err.contains("E492"),
        "{{ silent = true }} keeps errors, got {err:?}"
    );

    // …and `emsg_silent` swallows it (`:silent!`).
    let swallowed = message_after(
        &rpc,
        &mut incoming,
        ":lua btv.cmd('NotARealCommand', { silent = true, emsg_silent = true })<CR>",
    )
    .await;
    assert!(
        !swallowed.contains("E492"),
        "emsg_silent should swallow the error, got {swallowed:?}"
    );
}

/// The `vim.*` aliases reach the same modifier: `vim.cmd(str, opts)`, the structured
/// `vim.cmd{ cmd = …, mods = … }` / `nvim_cmd`, and the indexed `vim.cmd.echo{…}`.
/// A `mods` key bemtvi cannot honor raises rather than being silently dropped, and so
/// does `emsg_silent` without `silent` (bemtvi's `:silent!` cannot express it).
#[tokio::test]
async fn vim_cmd_mods_reach_the_same_modifier() {
    let dir = temp_dir("excmd_vim_cmd_mods");
    let (rpc, mut incoming) = start(&dir).await;

    let quiet = message_after(
        &rpc,
        &mut incoming,
        ":lua vim.cmd({ cmd = 'echo', args = { \"'TABLE'\" }, mods = { silent = true } })<CR>",
    )
    .await;
    assert!(
        !quiet.contains("TABLE"),
        "vim.cmd's structured form should honor mods.silent, got {quiet:?}"
    );

    let quiet = message_after(
        &rpc,
        &mut incoming,
        ":lua vim.cmd.echo({ args = { \"'INDEXED'\" }, mods = { silent = true } })<CR>",
    )
    .await;
    assert!(
        !quiet.contains("INDEXED"),
        "vim.cmd.<name> should honor mods.silent, got {quiet:?}"
    );

    // A modifier bemtvi doesn't dispatch fails loud, naming it.
    let raised = exec_lua(
        &rpc,
        "local ok, err = pcall(btv.cmd, 'echo 1', { keepjumps = true }) return tostring(err)",
    )
    .await;
    assert!(
        raised.as_str().unwrap_or_default().contains("keepjumps"),
        "an unsupported modifier should raise by name, got {raised:?}"
    );

    // So does `emsg_silent` on its own — rounding it up to `:silent!` would also
    // eat the ordinary output the caller asked to keep.
    let raised = exec_lua(
        &rpc,
        "local ok, err = pcall(btv.cmd, 'echo 1', { emsg_silent = true }) return tostring(err)",
    )
    .await;
    assert!(
        raised.as_str().unwrap_or_default().contains("emsg_silent"),
        "emsg_silent without silent should raise, got {raised:?}"
    );
}

/// `:silent! runtime <missing>` — a plugin manager's typical bootstrap call (`silent!
/// runtime plugin/rplugin.vim`) — must not surface an error for the unimplemented
/// `:runtime`; the modifier swallows it.
#[tokio::test]
async fn silent_bang_swallows_runtime() {
    let dir = temp_dir("excmd_silent_runtime");
    let (rpc, mut incoming) = start(&dir).await;
    let msg = message_after(
        &rpc,
        &mut incoming,
        ":silent! runtime plugin/rplugin.vim<CR>",
    )
    .await;
    assert!(
        msg.trim().is_empty(),
        "silent! runtime should be quiet, got {msg:?}"
    );
}

/// `:source {file.lua}` runs the Lua file: a `vim.g` marker it sets is observable
/// afterward. (a plugin manager's loader sources plugin/ftdetect Lua files this way.)
#[tokio::test]
async fn source_runs_a_lua_file() {
    let dir = temp_dir("excmd_source");
    let script = dir.join("marker.lua");
    std::fs::write(&script, "vim.g.sourced_marker = 42\n").unwrap();

    let (rpc, mut incoming) = start(&dir).await;
    let _ = message_after(
        &rpc,
        &mut incoming,
        &format!(":source {}<CR>", script.to_string_lossy()),
    )
    .await;

    let got = exec_lua(&rpc, "return vim.g.sourced_marker").await;
    assert_eq!(
        got.as_u64(),
        Some(42),
        "sourcing a .lua file should run it: {got:?}"
    );
}

/// `:source` of a file that doesn't exist is the standard E484 (a fail-loud, not a
/// silent skip that would make a never-applied script look loaded).
#[tokio::test]
async fn source_missing_file_errors() {
    let dir = temp_dir("excmd_source_missing");
    let (rpc, mut incoming) = start(&dir).await;
    let msg = message_after(&rpc, &mut incoming, ":source /no/such/bemtvi/file.lua<CR>").await;
    assert!(
        msg.contains("E484"),
        "sourcing a missing file should report E484, got {msg:?}"
    );
}

/// The GUI's `report_connect_error` reports a failed `:connect` on the message line
/// via the current session. bemtvi is its own editor and implements `:echoerr`, but
/// **not** vimscript's `:echohl` — an `echohl`-bar command (which the GUI used to
/// emit) dies at the first bar with E492, swallowing the real connect error. Guard
/// the working form: `:echoerr '<text>'` surfaces the text and never reports E492.
#[tokio::test]
async fn echoerr_reports_message_not_e492() {
    let dir = temp_dir("excmd_echoerr");
    let (rpc, mut incoming) = start(&dir).await;

    // The form the GUI emits for a connect failure (single quotes Vim-doubled).
    let msg = message_after(
        &rpc,
        &mut incoming,
        ":echoerr ':connect failed: unknown host ''asdfasd'''<CR>",
    )
    .await;
    assert!(
        !msg.contains("E492"),
        ":echoerr must not report E492, got {msg:?}"
    );
    assert!(
        msg.contains("connect failed: unknown host 'asdfasd'"),
        ":echoerr should surface the connect error text, got {msg:?}"
    );

    // The old `echohl`-bar form is exactly what produced the reported bug: `echohl`
    // is not an bemtvi ex-command, so the bar command reports E492 (kept as the
    // documented reason the GUI must not use it).
    let broken = message_after(
        &rpc,
        &mut incoming,
        ":echohl ErrorMsg|echom 'x'|echohl NONE<CR>",
    )
    .await;
    assert!(
        broken.contains("E492") && broken.contains("echohl"),
        "the echohl-bar form should fail with E492 on echohl, got {broken:?}"
    );
}

/// `:colorscheme` with no argument reports the active scheme on the message line
/// (the documented query form) — never a silent no-op. Before any scheme loads it
/// reports `default`, like vim.
#[tokio::test]
async fn colorscheme_no_arg_reports_the_active_scheme() {
    let dir = temp_dir("excmd_colo_report");
    std::fs::create_dir_all(dir.join("colors")).expect("mkdir colors");
    std::fs::write(
        dir.join("colors").join("cat.lua"),
        "vim.api.nvim_set_hl(0, 'Normal', { fg = '#ffffff' })\n",
    )
    .expect("write colorscheme");
    let (rpc, mut incoming) = start(&dir).await;

    // No scheme loaded yet (the test attach advertises no truecolor, so the
    // default-scheme auto-apply doesn't run): the query reports `default`.
    let before = message_after(&rpc, &mut incoming, ":colorscheme<CR>").await;
    assert_eq!(
        before.trim(),
        "default",
        "no-scheme query should report 'default', got {before:?}"
    );

    let _ = message_after(&rpc, &mut incoming, ":colorscheme cat<CR>").await;
    let after = message_after(&rpc, &mut incoming, ":colorscheme<CR>").await;
    assert_eq!(
        after.trim(),
        "cat",
        "the query should report the loaded scheme, got {after:?}"
    );
}
