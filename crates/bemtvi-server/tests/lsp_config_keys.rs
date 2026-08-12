//! Behavior tests for the `btv.lsp` config keys added for the native nvim-lspconfig
//! port (docs/plans/2026-07-29-nvim-lspconfig-native-port.md, Phase 1): `root_markers`
//! **priority tiers**, `workspace_required`, `cmd_env`, an **async** `cmd` builder,
//! `before_init`, and `name`.
//!
//! Black-box per the project conventions, and wired like `lsp_restart.rs`: a real
//! server over RPC and a tiny shell-script "server" that records what it was spawned
//! with (argv, cwd, environment) and then stays alive reading stdin, so a live server
//! is never auto-respawned and the log's only growth is the spawn under test. The
//! script's record — and whether it ran at all — is the observable.
//!
//! `$BEMTVI_LSP_CMD` is deliberately NOT used here: it replaces the whole argv, which
//! is exactly what the `cmd`-builder test needs to observe.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{attach, command, exec_lua, serial_lock, spawn, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start(dir: &Path) -> (Rpc, UnboundedReceiver<Incoming>) {
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Write the recording "language server": one `KEY=value` line per fact about the
/// spawn (argv, cwd, and the `cmd_env` variables under test), then append everything
/// the editor writes to its stdin to the same log — which both keeps the process
/// alive (it reads to EOF) and captures the `initialize` request, where the resolved
/// **root** shows up as `rootUri`. Root and working directory are separate facts
/// since `cmd_cwd` split them, so the log has to carry both. Returns its path.
fn write_recorder(dir: &Path, log: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("recorder.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             {{\n\
             echo \"ARGV=$*\"\n\
             echo \"CWD=$(pwd)\"\n\
             echo \"MY_VAR=${{MY_VAR:-<unset>}}\"\n\
             echo \"PATH_PRESENT=$(test -n \"$PATH\" && echo yes || echo no)\"\n\
             }} >> '{0}'\n\
             cat >> '{0}'\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// Every `KEY=value` line the recorder has written so far.
fn record(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .map(|s| s.lines().map(str::trim).map(str::to_string).collect())
        .unwrap_or_default()
}

/// The value of the first `key=` line, waiting up to ~5s for the spawn to land.
/// Panics with the whole record on timeout, so a failure shows what DID happen.
async fn wait_for(log: &Path, key: &str) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let prefix = format!("{key}=");
    loop {
        if let Some(line) = record(log).into_iter().find(|l| l.starts_with(&prefix)) {
            return line[prefix.len()..].to_string();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no {key} line appeared; the record was {:?}",
            record(log)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The filesystem path of the `rootUri` in the `initialize` request the editor sent
/// the recorder, waiting up to ~5s for it. This — not the child's cwd — is where the
/// resolved root is observable: the root is a *protocol* fact, while the cwd is the
/// editor's own directory (or `cmd_cwd`).
async fn wait_for_root(log: &Path) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        if let Some(rest) = text.split("\"rootUri\":\"file://").nth(1) {
            if let Some(end) = rest.find('"') {
                return rest[..end].to_string();
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no rootUri appeared in the initialize request; the record was {text:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The `initialize` request the editor sent the recorder, waiting up to ~5s for it.
/// Used where the interesting part is what the request does NOT carry (a rootless
/// start sends `"rootUri":null`), which `wait_for_root` cannot express — it waits for
/// a root that never comes.
async fn wait_for_initialize(log: &Path) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        // The whole frame is one line; `rootUri` is serialized last, so its presence
        // (as a path or as `null`) means the body has landed in full.
        if let Some(line) = text
            .lines()
            .find(|l| l.contains(r#""method":"initialize""#) && l.contains(r#""rootUri""#))
        {
            return line.to_string();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no initialize request appeared; the record was {text:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Give a spawn that must NOT happen a fair chance to happen, then assert silence.
/// The window matches `wait_for`'s successful path with room to spare — a root
/// search walks the fs seam off-tick, so "nothing yet" needs real wall-clock to mean
/// "nothing at all".
async fn assert_never_spawned(log: &Path, why: &str) {
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        record(log).is_empty(),
        "{why} — but the server was spawned: {:?}",
        record(log)
    );
}

/// A project tree with a lockfile at the top and a nested git repo two levels down:
///
/// ```text
/// <root>/package-lock.json
/// <root>/packages/inner/.git/          <- nearer, but a LOWER-priority marker
/// <root>/packages/inner/src/main.rs    <- the buffer
/// ```
///
/// The layout that makes marker priority observable: a flat marker list attaches at
/// `inner` (nearest wins), tiers attach at `<root>` (the lockfile tier is exhausted
/// over the whole tree before `.git` is looked at anywhere).
fn monorepo(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let dir = temp_dir(name);
    let root = dir.as_path().to_path_buf();
    std::fs::write(root.join("package-lock.json"), "{}\n").unwrap();
    let inner = root.join("packages").join("inner");
    std::fs::create_dir_all(inner.join(".git")).unwrap();
    let src = inner.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let file = src.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();
    (dir, root, inner, file)
}

/// Enable a config built from `body` (a Lua table body) for filetype `rust`, with
/// `cmd` pointing at the recorder.
async fn enable(rpc: &Rpc, script: &Path, body: &str) {
    exec_lua(
        rpc,
        &format!(
            "btv.lsp.config('rec', {{ cmd = {{ '{}' }}, filetypes = {{ 'rust' }}, {body} }})\n\
             btv.lsp.enable('rec')",
            script.display()
        ),
    )
    .await;
}

// ----- root_markers priority tiers -------------------------------------------

#[tokio::test]
async fn nested_root_markers_exhaust_each_tier_over_the_whole_tree() {
    // Each test holds a live server plus a blocked child process for its whole
    // body; serialize so this binary's footprint stays one of each rather than
    // twelve, which is what `serial_lock` is for (\"spawned subprocesses\").
    let _serial = serial_lock().lock().await;
    let (dir, root, _inner, file) = monorepo("lsp-root-tiers");
    let log = dir.as_path().join("rec.log");
    let script = write_recorder(dir.as_path(), &log);

    let (rpc, _incoming) = start(dir.as_path()).await;
    command(&rpc, &format!("e {}", file.display())).await;
    // Tier 1 is the lockfile, tier 2 is `.git`. The `.git` is NEARER — if tiers were
    // flattened (or a table marker silently never matched), this would attach at
    // `inner` instead, which is the bug this key exists to prevent.
    enable(
        &rpc,
        &script,
        "root_markers = { { 'package-lock.json' }, { '.git' } }",
    )
    .await;

    assert_eq!(
        canonical(&wait_for_root(&log).await),
        canonical(&root.to_string_lossy()),
        "the higher-priority lockfile tier must win over a nearer .git"
    );
}

#[tokio::test]
async fn a_flat_root_marker_list_takes_the_nearest_match() {
    // Each test holds a live server plus a blocked child process for its whole
    // body; serialize so this binary's footprint stays one of each rather than
    // twelve, which is what `serial_lock` is for (\"spawned subprocesses\").
    let _serial = serial_lock().lock().await;
    let (dir, _root, inner, file) = monorepo("lsp-root-flat");
    let log = dir.as_path().join("rec.log");
    let script = write_recorder(dir.as_path(), &log);

    let (rpc, _incoming) = start(dir.as_path()).await;
    command(&rpc, &format!("e {}", file.display())).await;
    // The same two markers with no tiers: one tier of equals, so the NEAREST
    // directory holding either one wins — `inner`, via its `.git`.
    enable(
        &rpc,
        &script,
        "root_markers = { 'package-lock.json', '.git' }",
    )
    .await;

    assert_eq!(
        canonical(&wait_for_root(&log).await),
        canonical(&inner.to_string_lossy()),
        "a flat list is one tier of equals — the nearest marker wins"
    );
}

// ----- workspace_required -----------------------------------------------------

#[tokio::test]
async fn workspace_required_declines_a_buffer_with_no_root() {
    // Each test holds a live server plus a blocked child process for its whole
    // body; serialize so this binary's footprint stays one of each rather than
    // twelve, which is what `serial_lock` is for (\"spawned subprocesses\").
    let _serial = serial_lock().lock().await;
    let dir = temp_dir("lsp-workspace-required");
    let log = dir.as_path().join("rec.log");
    let script = write_recorder(dir.as_path(), &log);
    // A file with none of the markers anywhere above it.
    let file = dir.as_path().join("loose.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();

    let (rpc, _incoming) = start(dir.as_path()).await;
    command(&rpc, &format!("e {}", file.display())).await;
    enable(
        &rpc,
        &script,
        "root_markers = { 'bemtvi-no-such-marker.json' }, workspace_required = true",
    )
    .await;

    // Rootless is *worse* than absent for these servers (eslint with no config lints
    // nothing, tailwindcss completes no classes), so the server must not start.
    assert_never_spawned(
        &log,
        "workspace_required with no root found must decline the buffer",
    )
    .await;
}

#[tokio::test]
async fn without_workspace_required_a_rootless_buffer_still_starts() {
    // Each test holds a live server plus a blocked child process for its whole
    // body; serialize so this binary's footprint stays one of each rather than
    // twelve, which is what `serial_lock` is for (\"spawned subprocesses\").
    let _serial = serial_lock().lock().await;
    let dir = temp_dir("lsp-rootless-ok");
    let log = dir.as_path().join("rec.log");
    let script = write_recorder(dir.as_path(), &log);
    let file = dir.as_path().join("loose.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();

    let (rpc, _incoming) = start(dir.as_path()).await;
    command(&rpc, &format!("e {}", file.display())).await;
    // The control for the test above: the same unfindable marker, no
    // `workspace_required` — the server starts, and starts ROOTLESS.
    enable(
        &rpc,
        &script,
        "root_markers = { 'bemtvi-no-such-marker.json' }",
    )
    .await;

    // No root found means no `rootUri` — single-file mode, as neovim does it. The
    // editor must not substitute the file's own directory: that told the server to
    // index whatever tree the file happened to sit in, and keyed a separate child per
    // directory, so opening two out-of-tree files started two servers.
    let init = wait_for_initialize(&log).await;
    assert!(
        init.contains(r#""rootUri":null"#),
        "a rootless start must send no rootUri, got {init:?}"
    );
}

// ----- the spawn directory (cmd_cwd, and its default) --------------------------

/// Restore the process cwd on drop, even if the test panics mid-way. The cwd tests
/// below move the *process* cwd (that is what "the editor's cwd" is locally), which
/// is safe only under the `serial_lock` every test body here already holds.
struct CwdGuard(PathBuf);
impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

#[tokio::test]
async fn the_server_is_spawned_in_the_editor_cwd_not_the_workspace_root() {
    // Each test holds a live server plus a blocked child process for its whole
    // body; serialize so this binary's footprint stays one of each rather than
    // twelve, which is what `serial_lock` is for (\"spawned subprocesses\").
    let _serial = serial_lock().lock().await;
    let _cwd = CwdGuard(std::env::current_dir().unwrap());
    let (dir, root, _inner, file) = monorepo("lsp-spawn-cwd");
    let log = dir.as_path().join("rec.log");
    let script = write_recorder(dir.as_path(), &log);
    // The editor's cwd is somewhere else entirely — as it is whenever a buffer is
    // opened from outside the tree it belongs to (a jumped-into dependency).
    let elsewhere = temp_dir("lsp-spawn-cwd-elsewhere");
    std::env::set_current_dir(elsewhere.as_path()).unwrap();

    let (rpc, _incoming) = start(dir.as_path()).await;
    command(&rpc, &format!("e {}", file.display())).await;
    enable(&rpc, &script, "root_markers = { 'package-lock.json' }").await;

    // The root reaches the server as `rootUri` — that part is unchanged…
    assert_eq!(
        canonical(&wait_for_root(&log).await),
        canonical(&root.to_string_lossy()),
        "the resolved root is still what the server is told its workspace is"
    );
    // …but the PROCESS runs where the user is. Launching it at the root instead put
    // servers in directories nobody cd'd to — `uvx` refuses to run at all with a cwd
    // inside its own cache, which is where a jumped-into dependency lives.
    assert_eq!(
        canonical(&wait_for(&log, "CWD").await),
        canonical(&elsewhere.as_path().to_string_lossy()),
        "the spawn directory is the editor's cwd, not the workspace root"
    );
}

#[tokio::test]
async fn cmd_cwd_pins_the_spawn_directory() {
    // Each test holds a live server plus a blocked child process for its whole
    // body; serialize so this binary's footprint stays one of each rather than
    // twelve, which is what `serial_lock` is for (\"spawned subprocesses\").
    let _serial = serial_lock().lock().await;
    let _cwd = CwdGuard(std::env::current_dir().unwrap());
    let dir = temp_dir("lsp-cmd-cwd");
    let log = dir.as_path().join("rec.log");
    let script = write_recorder(dir.as_path(), &log);
    let file = dir.as_path().join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();
    let pinned = dir.as_path().join("run-here");
    std::fs::create_dir(&pinned).unwrap();
    std::env::set_current_dir(dir.as_path()).unwrap();

    let (rpc, _incoming) = start(dir.as_path()).await;
    command(&rpc, &format!("e {}", file.display())).await;
    enable(
        &rpc,
        &script,
        &format!("cmd_cwd = '{}'", pinned.to_string_lossy()),
    )
    .await;

    assert_eq!(
        canonical(&wait_for(&log, "CWD").await),
        canonical(&pinned.to_string_lossy()),
        "cmd_cwd wins over the editor's cwd"
    );
}

#[tokio::test]
async fn a_relative_cmd_cwd_resolves_against_the_editor_cwd() {
    // Each test holds a live server plus a blocked child process for its whole
    // body; serialize so this binary's footprint stays one of each rather than
    // twelve, which is what `serial_lock` is for (\"spawned subprocesses\").
    let _serial = serial_lock().lock().await;
    let _cwd = CwdGuard(std::env::current_dir().unwrap());
    let dir = temp_dir("lsp-cmd-cwd-rel");
    let log = dir.as_path().join("rec.log");
    let script = write_recorder(dir.as_path(), &log);
    let file = dir.as_path().join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();
    let pinned = dir.as_path().join("run-here");
    std::fs::create_dir(&pinned).unwrap();
    std::env::set_current_dir(dir.as_path()).unwrap();

    let (rpc, _incoming) = start(dir.as_path()).await;
    command(&rpc, &format!("e {}", file.display())).await;
    // A relative `cmd_cwd` means what a relative path always means in the editor:
    // against the session's directory. Resolving it against the *spawning process*
    // would be the same thing locally and the wrong thing over a daemon.
    enable(&rpc, &script, "cmd_cwd = 'run-here'").await;

    assert_eq!(
        canonical(&wait_for(&log, "CWD").await),
        canonical(&pinned.to_string_lossy()),
        "a relative cmd_cwd resolves against the editor's cwd"
    );
}

// ----- cmd_env ----------------------------------------------------------------

#[tokio::test]
async fn cmd_env_reaches_the_spawned_process_without_replacing_its_environment() {
    // Each test holds a live server plus a blocked child process for its whole
    // body; serialize so this binary's footprint stays one of each rather than
    // twelve, which is what `serial_lock` is for (\"spawned subprocesses\").
    let _serial = serial_lock().lock().await;
    let dir = temp_dir("lsp-cmd-env");
    let log = dir.as_path().join("rec.log");
    let script = write_recorder(dir.as_path(), &log);
    let file = dir.as_path().join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();

    let (rpc, _incoming) = start(dir.as_path()).await;
    command(&rpc, &format!("e {}", file.display())).await;
    enable(&rpc, &script, "cmd_env = { MY_VAR = 'from-config' }").await;

    assert_eq!(wait_for(&log, "MY_VAR").await, "from-config");
    // Layered, not replacing: a server that lost `$PATH` couldn't find its own
    // toolchain, so `cmd_env` must ADD to the inherited environment.
    assert_eq!(
        wait_for(&log, "PATH_PRESENT").await,
        "yes",
        "cmd_env must layer over the inherited environment, not replace it"
    );
}

#[tokio::test]
async fn cmd_env_stringifies_numbers_and_booleans() {
    // Each test holds a live server plus a blocked child process for its whole
    // body; serialize so this binary's footprint stays one of each rather than
    // twelve, which is what `serial_lock` is for (\"spawned subprocesses\").
    let _serial = serial_lock().lock().await;
    let dir = temp_dir("lsp-cmd-env-coerce");
    let log = dir.as_path().join("rec.log");
    let script = write_recorder(dir.as_path(), &log);
    let file = dir.as_path().join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();

    let (rpc, _incoming) = start(dir.as_path()).await;
    command(&rpc, &format!("e {}", file.display())).await;
    // Configs write `NODE_OPTIONS = 4096` and `DEBUG = true`; an environment is
    // strings, so these are coerced rather than dropped.
    enable(&rpc, &script, "cmd_env = { MY_VAR = 4096 }").await;

    assert_eq!(wait_for(&log, "MY_VAR").await, "4096");
}

// ----- an async cmd builder ---------------------------------------------------

#[tokio::test]
async fn a_cmd_builder_may_return_a_promise_of_the_argv() {
    // Each test holds a live server plus a blocked child process for its whole
    // body; serialize so this binary's footprint stays one of each rather than
    // twelve, which is what `serial_lock` is for (\"spawned subprocesses\").
    let _serial = serial_lock().lock().await;
    let dir = temp_dir("lsp-async-cmd");
    let log = dir.as_path().join("rec.log");
    let script = write_recorder(dir.as_path(), &log);
    let file = dir.as_path().join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();

    let (rpc, _incoming) = start(dir.as_path()).await;
    command(&rpc, &format!("e {}", file.display())).await;
    // The shape every ported `node_modules/.bin` resolver takes: the lookup is I/O
    // (`btv.fs.which`), so the builder is async and returns a promise of the argv.
    // Blocking on the lookup is the thing bemtvi doesn't do, so this path is the only
    // way those configs can work at all.
    exec_lua(
        &rpc,
        &format!(
            "btv.lsp.config('rec', {{\n\
               cmd = btv.async(function(_, config)\n\
                 local found = btv.await(btv.fs.which('{}'))\n\
                 return {{ found or 'never-resolved', config.root_dir or 'no-root' }}\n\
               end),\n\
               filetypes = {{ 'rust' }},\n\
               root_markers = {{ 'recorder.sh' }},\n\
             }})\n\
             btv.lsp.enable('rec')",
            script.display()
        ),
    )
    .await;

    // The argv the builder resolved *asynchronously* is what actually spawned, and it
    // saw the resolved root (proving the builder ran after root resolution).
    let argv = wait_for(&log, "ARGV").await;
    assert_eq!(
        canonical(&argv),
        canonical(&dir.as_path().to_string_lossy()),
        "the async builder's argv reached the spawn, root included"
    );
}

#[tokio::test]
async fn a_cmd_builder_that_rejects_does_not_start_a_server() {
    // Each test holds a live server plus a blocked child process for its whole
    // body; serialize so this binary's footprint stays one of each rather than
    // twelve, which is what `serial_lock` is for (\"spawned subprocesses\").
    let _serial = serial_lock().lock().await;
    let dir = temp_dir("lsp-async-cmd-reject");
    let log = dir.as_path().join("rec.log");
    let script = write_recorder(dir.as_path(), &log);
    let file = dir.as_path().join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();

    let (rpc, _incoming) = start(dir.as_path()).await;
    command(&rpc, &format!("e {}", file.display())).await;
    // A builder whose lookup fails must NOT fall through to spawning something else;
    // it is reported and skipped.
    exec_lua(
        &rpc,
        &format!(
            "btv.lsp.config('rec', {{\n\
               cmd = btv.async(function() error('no toolchain here') end),\n\
               filetypes = {{ 'rust' }},\n\
             }})\n\
             btv.lsp.enable('rec')\n\
             _G.unused = '{}'",
            script.display()
        ),
    )
    .await;

    assert_never_spawned(&log, "a rejecting cmd builder must not spawn anything").await;
    // The editor is unharmed — one bad config never breaks the session.
    assert_eq!(exec_lua(&rpc, "return 1 + 1").await.as_i64(), Some(2));
}

// ----- before_init ------------------------------------------------------------

#[tokio::test]
async fn before_init_shapes_the_config_and_may_be_async() {
    // Each test holds a live server plus a blocked child process for its whole
    // body; serialize so this binary's footprint stays one of each rather than
    // twelve, which is what `serial_lock` is for (\"spawned subprocesses\").
    let _serial = serial_lock().lock().await;
    let dir = temp_dir("lsp-before-init");
    let log = dir.as_path().join("rec.log");
    let script = write_recorder(dir.as_path(), &log);
    let file = dir.as_path().join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();

    let (rpc, _incoming) = start(dir.as_path()).await;
    command(&rpc, &format!("e {}", file.display())).await;
    // rust-analyzer's shape — mirror `settings` into the initialization options —
    // except async, which is what makes the upstream `vim.system(…):wait()` versions
    // portable. `_G.seen_root` records that the hook ran with the resolved root, and
    // the argv proves the start waited for the hook's promise before spawning.
    exec_lua(
        &rpc,
        &format!(
            "btv.lsp.config('rec', {{\n\
               cmd = {{ '{}' }},\n\
               filetypes = {{ 'rust' }},\n\
               root_markers = {{ 'recorder.sh' }},\n\
               settings = {{ mine = {{ opt = 1 }} }},\n\
               before_init = btv.async(function(init_params, config)\n\
                 btv.await(btv.promise.delay(10))\n\
                 _G.seen_root = config.root_dir or '<none>'\n\
                 init_params.initializationOptions = config.settings.mine\n\
               end),\n\
             }})\n\
             btv.lsp.enable('rec')",
            script.display()
        ),
    )
    .await;

    wait_for(&log, "CWD").await;
    let seen = exec_lua(&rpc, "return _G.seen_root").await;
    assert_eq!(
        seen.as_str().map(canonical),
        Some(canonical(&dir.as_path().to_string_lossy())),
        "before_init ran with the resolved root, and the spawn waited for its promise"
    );
}

#[tokio::test]
async fn a_failing_before_init_does_not_start_a_half_configured_server() {
    // Each test holds a live server plus a blocked child process for its whole
    // body; serialize so this binary's footprint stays one of each rather than
    // twelve, which is what `serial_lock` is for (\"spawned subprocesses\").
    let _serial = serial_lock().lock().await;
    let dir = temp_dir("lsp-before-init-fail");
    let log = dir.as_path().join("rec.log");
    let script = write_recorder(dir.as_path(), &log);
    let file = dir.as_path().join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();

    let (rpc, _incoming) = start(dir.as_path()).await;
    command(&rpc, &format!("e {}", file.display())).await;
    // A server started with half-applied options is worse than one that visibly
    // didn't start: it answers confidently and wrongly.
    exec_lua(
        &rpc,
        &format!(
            "btv.lsp.config('rec', {{\n\
               cmd = {{ '{}' }},\n\
               filetypes = {{ 'rust' }},\n\
               before_init = function() error('cannot compute options') end,\n\
             }})\n\
             btv.lsp.enable('rec')",
            script.display()
        ),
    )
    .await;

    assert_never_spawned(&log, "a before_init that fails must not start the server").await;
}

// ----- name -------------------------------------------------------------------

#[tokio::test]
async fn a_config_name_override_still_resolves_its_lifecycle_hooks() {
    // Each test holds a live server plus a blocked child process for its whole
    // body; serialize so this binary's footprint stays one of each rather than
    // twelve, which is what `serial_lock` is for (\"spawned subprocesses\").
    let _serial = serial_lock().lock().await;
    let dir = temp_dir("lsp-name-override");
    let log = dir.as_path().join("rec.log");
    let script = write_recorder(dir.as_path(), &log);
    let file = dir.as_path().join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();

    let (rpc, _incoming) = start(dir.as_path()).await;
    command(&rpc, &format!("e {}", file.display())).await;
    // Registered under `rec`, reported as `renamed`. The lifecycle hooks resolve a
    // config from the name the SERVER reports, so without the reverse map this
    // config's `on_attach` would silently never run.
    enable(
        &rpc,
        &script,
        "name = 'renamed', on_attach = function(client) _G.attached = client.name end",
    )
    .await;

    wait_for(&log, "CWD").await;
    // The client is known under the overridden name (the recorder never answers
    // `initialize`, so this asserts the mapping the start installed, not an attach).
    let key = exec_lua(&rpc, "return btv.lsp._config_key['renamed']").await;
    assert_eq!(
        key.as_str(),
        Some("rec"),
        "the reported client name must map back to its registry key"
    );
}

// ----- unknown / unsupported keys are reported --------------------------------

#[tokio::test]
async fn an_unknown_config_key_is_reported_rather_than_silently_dropped() {
    // Each test holds a live server plus a blocked child process for its whole
    // body; serialize so this binary's footprint stays one of each rather than
    // twelve, which is what `serial_lock` is for (\"spawned subprocesses\").
    let _serial = serial_lock().lock().await;
    let dir = temp_dir("lsp-unknown-key");
    let log = dir.as_path().join("rec.log");
    let script = write_recorder(dir.as_path(), &log);
    let file = dir.as_path().join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();

    let (rpc, _incoming) = start(dir.as_path()).await;
    command(&rpc, &format!("e {}", file.display())).await;
    // Collect the notifications this dispatch emits. A typo'd key (`filetype`, not
    // `filetypes`) looks exactly like a server that won't start — the warning is the
    // difference between that and an hour of debugging.
    exec_lua(
        &rpc,
        "_G.warnings = {}\n\
         local real = btv.notify\n\
         btv.notify = function(msg, lvl) _G.warnings[#_G.warnings + 1] = msg return real(msg, lvl) end",
    )
    .await;
    enable(&rpc, &script, "filetype = 'rust', handlers = {}").await;
    wait_for(&log, "CWD").await;

    let joined = exec_lua(&rpc, "return table.concat(_G.warnings, '\\n')").await;
    let joined = joined.as_str().unwrap_or_default();
    // Named, and pointed at the likely cause. It cannot claim the key is *ignored* —
    // a config's own `cmd` / `before_init` / `on_attach` may well read a key btv.lsp
    // doesn't (powershell_es' `bundle_path`, apex_ls' `apex_jar_path`), and calling
    // those ignored would be the wrong half of the truth.
    assert!(
        joined.contains("`filetype`") && joined.contains("misspelled"),
        "a typo'd key must be named in a warning; saw {joined:?}"
    );
    // A key bemtvi knows about but doesn't act on says what will happen instead.
    assert!(
        joined.contains("`handlers`") && joined.contains("never run"),
        "an unsupported key must say what happens instead; saw {joined:?}"
    );
}

/// Resolve symlinks so a `/tmp` vs `/private/tmp` (or any symlinked temp root)
/// difference between what the test built and what the shell's `pwd` reported can't
/// make a correct path comparison fail.
fn canonical(path: &str) -> String {
    std::fs::canonicalize(path.trim())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.trim().to_string())
}
