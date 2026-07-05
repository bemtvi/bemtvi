//! Behavior tests for `nx.plugins` — nxvim's native package / plugin manager.
//!
//! Black-box per the project conventions: a real server over RPC, driven with
//! `nvim_exec_lua`, asserting on observable Lua state and the real filesystem.
//! Hermetic — no network: the "remote" each test clones is a throwaway local git
//! repo on disk, cloned over `file://`. (`git` must be on PATH, which the dev/CI
//! environment provides; the editor itself never shells out — the manager does,
//! via `nx.run`.)
//!
//! `nx.plugins` is async end to end: a sync clones over `nx.run`, a load sources
//! `plugin/` scripts off the tick over `nx.fs`. So each test kicks the operation,
//! then POLLS an observable (a `_G` flag the plugin's own scripts set, or the real
//! filesystem) until it settles — exactly like the `nx.fs` / `nx.run` tests.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, barrier, cursor, drain_to_latest_redraw, exec_lua, feed, lines, lua_bool, map_get,
    spawn, start_attached, temp_dir,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Lua-escape a path for embedding in a double-quoted string literal.
fn q(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// Run `git` with `args` in `cwd`, with a fixed identity and no host config
/// bleeding in, asserting success. Test plumbing only.
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

/// Build a throwaway plugin repo at `<base>/<name>.git-src` whose layout exercises
/// the loader: a `lua/<name>/init.lua` module (`require`-able; its `setup` sets a
/// flag), a `plugin/<name>.lua` script (auto-sourced; sets a flag), and a
/// `colors/<name>.lua` (resolved via the runtimepath). Returns its absolute path.
fn make_repo(base: &Path, name: &str) -> PathBuf {
    let repo = base.join(format!("{name}.git-src"));
    std::fs::create_dir_all(repo.join("lua").join(name)).unwrap();
    std::fs::create_dir_all(repo.join("plugin")).unwrap();
    std::fs::create_dir_all(repo.join("colors")).unwrap();
    std::fs::write(
        repo.join("lua").join(name).join("init.lua"),
        format!(
            "local M = {{}}\n\
             _G.{name}_required = true\n\
             function M.setup(opts) _G.{name}_setup = (opts and opts.tag) or true end\n\
             return M\n"
        ),
    )
    .unwrap();
    std::fs::write(
        repo.join("plugin").join(format!("{name}.lua")),
        format!("_G.{name}_plugin = true\n"),
    )
    .unwrap();
    std::fs::write(
        repo.join("colors").join(format!("{name}.lua")),
        "-- a colorscheme stub\n",
    )
    .unwrap();

    git(&repo, &["init", "-q", "-b", "main"]);
    // file:// clones go through upload-pack, so allow the blobless filter the
    // manager requests.
    git(&repo, &["config", "uploadpack.allowfilter", "true"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    repo
}

/// Poll a `return`-style chunk until it is `true` (~3s). Async manager steps settle
/// over later ticks, so the flag a plugin's scripts set is nil until then.
async fn poll_true(rpc: &Rpc, code: &str) -> bool {
    for _ in 0..200 {
        if lua_bool(rpc, code).await == Some(true) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// Poll (via the redraw `float` surface) for the content float being present
/// (`want = true`, a map) or gone (`want = false`, `Nil`). The `float` redraw key
/// is the `nx.ui.float` content-float slot — the restart notice — distinct from the
/// manager's own `nx.view` window. Take-latest, so the reader task settles.
async fn poll_float_present(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    want: bool,
) -> bool {
    for _ in 0..200 {
        barrier(rpc).await;
        if drain_to_latest_redraw(incoming, |m| {
            matches!(map_get(m, "float"), Some(Value::Map(_))) == want
        })
        .is_some()
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// Feed `key` repeatedly (~3s) until `code` returns true. The welcome / manager
/// views mount + bind their buffer-local maps over a couple of ticks, so an early
/// keypress lands on the no-op default map; re-feeding until the observable settles
/// is race-free (an extra keypress after the view closes is harmless).
async fn feed_until(rpc: &Rpc, key: &str, code: &str) -> bool {
    for _ in 0..150 {
        feed(rpc, key);
        if lua_bool(rpc, code).await == Some(true) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    lua_bool(rpc, code).await == Some(true)
}

/// Declare the manager's install root (a temp dir) so the test never touches the
/// host data dir, and return that root path.
async fn setup_root(rpc: &Rpc, tag: &str) -> PathBuf {
    let root = temp_dir(tag).join("install");
    exec_lua(
        rpc,
        &format!("nx.plugins.setup_manager({{ root = \"{}\" }})", q(&root)),
    )
    .await;
    root
}

// ----- the prelude module loads ----------------------------------------------

#[tokio::test]
async fn plugins_namespace_present() {
    let (rpc, _i) = start().await;
    assert_eq!(
        lua_bool(
            &rpc,
            "return type(nx.plugins) == 'table' and type(nx.plugins.sync) == 'function'"
        )
        .await,
        Some(true)
    );
    // The callable form and the commands are wired.
    assert_eq!(
        lua_bool(&rpc, "return getmetatable(nx.plugins).__call ~= nil").await,
        Some(true)
    );
    assert_eq!(
        lua_bool(
            &rpc,
            "return nx.user_command.get().PluginSync ~= nil and nx.user_command.get().PluginList ~= nil"
        )
        .await,
        Some(true)
    );
}

// ----- declare + sync installs, then loads eagerly ---------------------------

#[tokio::test]
async fn sync_clones_and_loads_eager_plugin() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_sync");
    let repo = make_repo(&src, "alpha");
    let root = setup_root(&rpc, "plug_sync").await;

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\",\n\
               config = function() require(\"alpha\").setup() end }} }}\n\
             nx.plugins.sync():catch(function(e) _G.sync_err = tostring(e and e.message or e) end)",
            repo = q(&repo)
        ),
    )
    .await;

    // The clone landed on disk, the module is require-able, its plugin/ script ran,
    // and its config() ran setup().
    assert!(
        poll_true(&rpc, "return _G.alpha_setup == true").await,
        "config().setup() should have run; sync_err={:?}",
        exec_lua(&rpc, "return _G.sync_err").await
    );
    assert_eq!(
        lua_bool(&rpc, "return _G.alpha_required == true").await,
        Some(true)
    );
    assert_eq!(
        lua_bool(&rpc, "return _G.alpha_plugin == true").await,
        Some(true)
    );
    assert!(root
        .join("alpha")
        .join("lua")
        .join("alpha")
        .join("init.lua")
        .exists());

    // It reports as installed + loaded, and the runtimepath now resolves its colors/.
    assert_eq!(
        lua_bool(&rpc, "return nx.plugins._loaded.alpha == true").await,
        Some(true)
    );
    assert_eq!(
        lua_bool(
            &rpc,
            "return #nx.runtime_file('colors/alpha.lua', false) == 1"
        )
        .await,
        Some(true)
    );
}

// ----- a lazy (cmd-triggered) plugin loads only on first use -----------------

#[tokio::test]
async fn lazy_cmd_defers_load_until_invoked() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_lazy");
    let repo = make_repo(&src, "beta");
    let root = setup_root(&rpc, "plug_lazy").await;

    // Declare it lazy behind :Beta, then install (clone only — no load).
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ \"file://{repo}\", name = \"beta\", cmd = \"Beta\",\n\
               config = function() require(\"beta\").setup() end }} }}\n\
             nx.plugins.install():catch(function(e) _G.err = tostring(e and e.message or e) end)",
            repo = q(&repo)
        ),
    )
    .await;

    // Wait for the clone to land on disk (poll the real filesystem).
    let installed = root.join("beta").join("lua").join("beta").join("init.lua");
    for _ in 0..200 {
        if installed.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        installed.exists(),
        "clone should have landed; err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );

    // The :Beta stub is armed, but the body has NOT loaded — config() never ran.
    assert_eq!(
        lua_bool(&rpc, "return nx.user_command.get().Beta ~= nil").await,
        Some(true)
    );
    assert_ne!(
        lua_bool(&rpc, "return nx.plugins._loaded.beta == true").await,
        Some(true)
    );
    assert_ne!(
        lua_bool(&rpc, "return _G.beta_setup == true").await,
        Some(true)
    );

    // Invoke the lazy command — that loads the plugin, then re-dispatches.
    exec_lua(&rpc, "vim.cmd('Beta')").await;
    assert!(
        poll_true(&rpc, "return _G.beta_setup == true").await,
        "invoking :Beta should load beta and run its config()"
    );
    assert_eq!(
        lua_bool(&rpc, "return nx.plugins._loaded.beta == true").await,
        Some(true)
    );
}

// ----- a local `dir` (dev) plugin loads without cloning ----------------------

#[tokio::test]
async fn local_dir_plugin_loads_without_clone() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_dir");
    // Use the repo's working tree directly as a `dir` plugin (no clone).
    let repo = make_repo(&src, "gamma");
    setup_root(&rpc, "plug_dir").await;

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ name = \"gamma\", dir = \"{dir}\",\n\
               config = function() require(\"gamma\").setup() end }} }}",
            dir = q(&repo)
        ),
    )
    .await;

    assert!(
        poll_true(&rpc, "return _G.gamma_setup == true").await,
        "a dir plugin should load eagerly with no clone"
    );
    assert_eq!(
        lua_bool(&rpc, "return _G.gamma_plugin == true").await,
        Some(true)
    );
}

// ----- a `dir` with a leading `~` expands to $HOME ---------------------------

#[tokio::test]
async fn local_dir_expands_leading_tilde() {
    let (rpc, _i) = start().await;

    // A `dir` may name a dev checkout under the home directory with `~`. It is
    // expanded once at declaration, so the stored install dir is absolute (no
    // stray `~` that a later `require` / rtp insert would fail to resolve).
    // `enabled = false` registers the spec without trying to load the (absent)
    // checkout. The comparison runs server-side so it reads the server's $HOME.
    let ok = lua_bool(
        &rpc,
        r#"
        local home = os.getenv("HOME")
        nx.plugins.add({ name = "tildeplug", dir = "~/dev/tildeplug", enabled = false })
        local spec = nx.plugins._specs["tildeplug"]
        return spec.dir == home .. "/dev/tildeplug"
          and spec._dir == home .. "/dev/tildeplug"
        "#,
    )
    .await;
    assert_eq!(
        ok,
        Some(true),
        "a `dir` of \"~/dev/tildeplug\" should expand its leading ~ to $HOME"
    );
}

// ----- clean removes undeclared clones ---------------------------------------

#[tokio::test]
async fn clean_removes_undeclared_dirs() {
    let (rpc, _i) = start().await;
    let root = setup_root(&rpc, "plug_clean").await;
    // A stray directory under the install root that no spec declares.
    std::fs::create_dir_all(root.join("orphan").join("lua")).unwrap();
    std::fs::write(root.join("orphan").join("README"), b"stale").unwrap();
    assert!(root.join("orphan").exists());

    exec_lua(
        &rpc,
        "nx.plugins.clean():next(function(r) _G.cleaned = #r end)",
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.cleaned == 1").await,
        "clean should remove the one orphan dir"
    );
    assert!(!root.join("orphan").exists());
}

// ----- config / init accept async functions ----------------------------------

// A plugin's `config` may be a plain function OR an async one (it nx.awaits, e.g.
// reads a file / shells out). It must run to completion either way, and the plugin
// is marked loaded only after it finishes.
#[tokio::test]
async fn config_accepts_an_async_function() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_asynccfg");
    let repo = make_repo(&src, "delta");
    setup_root(&rpc, "plug_asynccfg").await;
    let initlua = repo.join("lua").join("delta").join("init.lua");

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ name = \"delta\", dir = \"{dir}\", config = function()\n\
               local txt = nx.await(nx.fs.read_text(\"{f}\"))\n\
               _G.delta_cfg = #txt > 0\n\
             end }} }}",
            dir = q(&repo),
            f = q(&initlua)
        ),
    )
    .await;

    assert!(
        poll_true(&rpc, "return _G.delta_cfg == true").await,
        "an async config (one that nx.awaits) must run to completion"
    );
    assert_eq!(
        lua_bool(&rpc, "return nx.plugins._loaded.delta == true").await,
        Some(true)
    );
}

// `init` (the always-run hook) is armed synchronously at declaration, so an async
// init is the case that breaks without an explicit coroutine wrapper.
#[tokio::test]
async fn init_accepts_an_async_function() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_asyncinit");
    let repo = make_repo(&src, "epsilon");
    setup_root(&rpc, "plug_asyncinit").await;
    let initlua = repo.join("lua").join("epsilon").join("init.lua");

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ name = \"epsilon\", dir = \"{dir}\", init = function()\n\
               local txt = nx.await(nx.fs.read_text(\"{f}\"))\n\
               _G.epsilon_init = #txt > 0\n\
             end }} }}",
            dir = q(&repo),
            f = q(&initlua)
        ),
    )
    .await;

    assert!(
        poll_true(&rpc, "return _G.epsilon_init == true").await,
        "an async init (one that nx.awaits) must run to completion"
    );
}

// ----- git is spawned non-interactively --------------------------------------

// A git that prompts for a password writes to /dev/tty and reads from it, which
// corrupts the TUI and hangs the editor. The manager must run git non-interactively
// so it never touches the terminal: it fails fast with a captured error instead.
// Here a fake `git` records whether GIT_TERMINAL_PROMPT was passed; the manager must
// set it to "0".
#[cfg(unix)]
#[tokio::test]
async fn git_runs_noninteractive_with_terminal_prompt_disabled() {
    use std::os::unix::fs::PermissionsExt;

    let (rpc, _i) = start().await;
    let dir = temp_dir("plug_noninteractive");
    let marker = dir.join("git_env.log");
    let fake = dir.join("fakegit.sh");
    // Record GIT_TERMINAL_PROMPT for every invocation; on `clone`, create the
    // destination dir (the last argv) so the manager's later steps don't error.
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"${{GIT_TERMINAL_PROMPT-UNSET}}\" >> \"{marker}\"\n\
             if [ \"$1\" = clone ]; then for a in \"$@\"; do dest=\"$a\"; done; mkdir -p \"$dest\"; fi\n\
             exit 0\n",
            marker = q(&marker)
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let root = dir.join("install");
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.setup_manager({{ root = \"{root}\", git = \"{git}\" }})\n\
             nx.plugins {{ {{ \"file:///no/such/repo\", name = \"zeta\" }} }}\n\
             nx.plugins.install():catch(function(e) _G.err = tostring(e and e.message or e) end)",
            root = q(&root),
            git = q(&fake)
        ),
    )
    .await;

    for _ in 0..200 {
        if marker.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let logged = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(
        logged.lines().any(|l| l == "0"),
        "git must be spawned with GIT_TERMINAL_PROMPT=0 so it never prompts on the \
         terminal (recorded values: {logged:?})"
    );
}

// ----- first-run recommended-set bootstrap -----------------------------------

// Point the manager's install root + config dir at temp dirs (hermetic).
async fn setup_root_and_config(rpc: &Rpc, tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let base = temp_dir(tag);
    let root = base.join("data");
    let cfg = base.join("config");
    exec_lua(
        rpc,
        &format!(
            "nx.plugins.setup_manager({{ root = \"{}\", config = \"{}\" }})",
            q(&root),
            q(&cfg)
        ),
    )
    .await;
    (root, cfg)
}

// On a fresh setup the first-run flow opens the WELCOME checklist; confirming it
// (items pre-ticked) writes the chosen set to the user's config (a separate
// plugins.lua that init.lua requires) and installs+loads it now.
#[tokio::test]
async fn first_run_offers_recommended_and_persists_on_yes() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_reco_src");
    let repo = make_repo(&src, "zeta");
    let (root, cfg) = setup_root_and_config(&rpc, "plug_reco").await;

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.recommend({{\n\
               {{ \"file://{repo}\", name = \"zeta\", desc = \"Zeta the plugin\" }} }})\n\
             nx.plugins.bootstrap()",
            repo = q(&repo)
        ),
    )
    .await;

    // The welcome checklist view appears (after the async marker check) and grabs
    // focus — the current buffer becomes the welcome view.
    assert!(
        poll_true(&rpc, "return nx.plugins._prompting == true").await,
        "the recommended-set welcome should appear on a fresh setup"
    );
    assert!(
        poll_true(&rpc, "return vim.bo.filetype == 'nxpluginswelcome'").await,
        "the welcome checklist view should be focused"
    );
    // The welcome view wraps long lines so its intro / hint stay fully readable, and
    // insets its content from the border via the 'padding' window option.
    assert!(
        poll_true(&rpc, "return vim.wo.wrap == true").await,
        "the welcome view should enable line wrapping"
    );
    assert!(
        poll_true(
            &rpc,
            "return vim.wo.padding ~= '' and vim.wo.padding ~= nil"
        )
        .await,
        "the welcome view should set a 'padding' margin"
    );

    // <CR> confirms the pre-ticked set → install + load + persist.
    assert!(
        feed_until(&rpc, "<CR>", "return nx.plugins._loaded.zeta == true").await,
        "confirming the welcome installs and loads the recommended set"
    );
    let pluginslua = std::fs::read_to_string(cfg.join("lua").join("plugins.lua")).unwrap();
    assert!(
        pluginslua.contains("zeta"),
        "the set is written to lua/plugins.lua (got: {pluginslua:?})"
    );
    assert!(
        pluginslua.contains("Zeta the plugin"),
        "the spec's desc is serialized into plugins.lua (got: {pluginslua:?})"
    );
    let initlua = std::fs::read_to_string(cfg.join("init.lua")).unwrap();
    assert!(
        initlua.contains("require(\"plugins\")"),
        "init.lua is pointed at the managed plugins.lua (got: {initlua:?})"
    );
    // The "asked already" marker now exists, so a second run never re-prompts.
    assert!(root.join(".recommended-prompted").exists());
    exec_lua(&rpc, "nx.plugins._order = {}; nx.plugins.bootstrap()").await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_ne!(
        lua_bool(&rpc, "return nx.plugins._prompting == true").await,
        Some(true),
        "an already-answered setup must not prompt again"
    );
}

// The first-run flow must not install silently in the background: confirming the
// welcome checklist OPENS THE MANAGER DASHBOARD, and the chosen set installs THERE
// with live per-plugin status. (Regression: bootstrap used to `await M.sync()` with
// no UI, so the welcome vanished and installs ran invisibly.)
#[tokio::test]
async fn welcome_confirm_opens_the_manager_and_installs_there() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_wman_src");
    let repo = make_repo(&src, "iota");
    let (_root, _cfg) = setup_root_and_config(&rpc, "plug_wman").await;

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.recommend({{ {{ \"file://{repo}\", name = \"iota\" }} }})\n\
             nx.plugins.bootstrap()",
            repo = q(&repo)
        ),
    )
    .await;
    assert!(poll_true(&rpc, "return vim.bo.filetype == 'nxpluginswelcome'").await);

    // Confirming the welcome hands off to the manager dashboard (not a silent sync)…
    assert!(
        feed_until(&rpc, "<CR>", "return vim.bo.filetype == 'nxplugins'").await,
        "confirming the welcome should open the manager dashboard"
    );
    // …and the install runs THERE — the chosen plugin ends up cloned + loaded.
    assert!(
        poll_true(&rpc, "return nx.plugins._loaded.iota == true").await,
        "the manager should install the chosen set with live status"
    );
}

// Skipping the welcome (Esc) records the marker (so it never asks again) and writes
// nothing to the user's config.
#[tokio::test]
async fn first_run_decline_writes_nothing_but_marks_asked() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_decline_src");
    let repo = make_repo(&src, "eta");
    let (root, cfg) = setup_root_and_config(&rpc, "plug_decline").await;

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.recommend({{ {{ \"file://{repo}\", name = \"eta\" }} }})\n\
             nx.plugins.bootstrap()",
            repo = q(&repo)
        ),
    )
    .await;
    assert!(poll_true(&rpc, "return nx.plugins._prompting == true").await);
    // Wait for the welcome to be up, then <Esc> to skip — the view closes.
    assert!(poll_true(&rpc, "return vim.bo.filetype == 'nxpluginswelcome'").await);
    assert!(
        feed_until(
            &rpc,
            "<Esc>",
            "return vim.bo.filetype ~= 'nxpluginswelcome'"
        )
        .await,
        "Esc should skip and close the welcome view"
    );

    // Marker written (asked once), but no config and nothing installed.
    for _ in 0..200 {
        if root.join(".recommended-prompted").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        root.join(".recommended-prompted").exists(),
        "asked-once marker is recorded"
    );
    assert!(
        !cfg.join("lua").join("plugins.lua").exists(),
        "declining writes no config"
    );
    assert_ne!(
        lua_bool(&rpc, "return nx.plugins._loaded.eta == true").await,
        Some(true),
        "declining installs nothing"
    );
}

// VimEnter (fired by the server at the end of startup) drives the first-run prompt
// for a brand-new user whose init.lua only registers a recommended set.
#[tokio::test]
async fn vim_enter_triggers_the_first_run_prompt() {
    let src = temp_dir("plug_vimenter_src");
    let repo = make_repo(&src, "theta");
    let base = temp_dir("plug_vimenter");
    let root = base.join("data");
    let cfg = base.join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    // A config that only sets paths and registers a recommended set — no plugins of
    // the user's own, so the bootstrap should offer the set at VimEnter.
    std::fs::write(
        cfg.join("init.lua"),
        format!(
            "nx.plugins.setup_manager({{ root = \"{root}\", config = \"{cfg}\" }})\n\
             nx.plugins.recommend({{ {{ \"file://{repo}\", name = \"theta\" }} }})\n",
            root = q(&root),
            cfg = q(&cfg),
            repo = q(&repo)
        ),
    )
    .unwrap();
    let init = ServerInit {
        config_dir: Some(cfg.clone()),
        runtimepath: vec![cfg.clone()],
        ..Default::default()
    };
    let (rpc, _incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    // No exec_lua trigger: VimEnter alone must surface the welcome view.
    assert!(
        poll_true(&rpc, "return nx.plugins._prompting == true").await,
        "VimEnter should drive the first-run recommended-set welcome"
    );
    assert!(poll_true(&rpc, "return vim.bo.filetype == 'nxpluginswelcome'").await);
    assert!(
        feed_until(&rpc, "<CR>", "return nx.plugins._loaded.theta == true").await,
        "confirming the VimEnter welcome installs the set"
    );
}

// ----- partial selection: unticking excludes a plugin ------------------------

// The headline of the welcome checklist: the user can untick the plugins they don't
// want. With two recommended, unticking the second installs + persists only the first.
#[tokio::test]
async fn welcome_untick_excludes_a_plugin() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_untick_src");
    let repo1 = make_repo(&src, "uno");
    let repo2 = make_repo(&src, "dos");
    let (_root, cfg) = setup_root_and_config(&rpc, "plug_untick").await;

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.recommend({{ {{ \"file://{r1}\", name = \"uno\" }},\n\
               {{ \"file://{r2}\", name = \"dos\" }} }})\n\
             nx.plugins.bootstrap()",
            r1 = q(&repo1),
            r2 = q(&repo2)
        ),
    )
    .await;

    // Welcome up and rendered with both items pre-ticked. Rendered content ⇒ the
    // component's setup ran and its buffer-local maps are bound.
    assert!(poll_true(&rpc, "return vim.bo.filetype == 'nxpluginswelcome'").await);
    let mut rendered = false;
    for _ in 0..200 {
        let ls = lines(&rpc).await;
        if ls.iter().filter(|l| l.contains('☑')).count() == 2 {
            rendered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(rendered, "welcome should render both items pre-ticked");

    // Jump to the second item (line 5 = 2 intro lines + a blank + item #2) with the
    // builtin `5G` motion (not remapped), then untick it with <Space>.
    feed(&rpc, "5G");
    feed(&rpc, "<Space>");
    let mut unticked = false;
    for _ in 0..100 {
        let ls = lines(&rpc).await;
        if ls.get(4).map(|l| l.contains('☐')).unwrap_or(false) {
            unticked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(unticked, "<Space> should untick the second item");

    // Confirm: only the still-ticked `uno` installs + persists; `dos` is excluded.
    assert!(
        feed_until(&rpc, "<CR>", "return nx.plugins._loaded.uno == true").await,
        "confirming installs the ticked plugin"
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_ne!(
        lua_bool(&rpc, "return nx.plugins._loaded.dos == true").await,
        Some(true),
        "the unticked plugin must not be installed"
    );
    let pluginslua = std::fs::read_to_string(cfg.join("lua").join("plugins.lua")).unwrap();
    assert!(
        pluginslua.contains("uno") && !pluginslua.contains("dos"),
        "only the ticked plugin is written to plugins.lua (got: {pluginslua:?})"
    );
}

// On mount the welcome must land the cursor ON the first item, not above the list:
// otherwise `move()` clamps the off-list cursor to row 1 and the very first `j`
// jumps straight to the SECOND item, skipping the first. The placement has to
// survive the float grab, which lands a tick after mount.
#[tokio::test]
async fn welcome_starts_cursor_on_first_item() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_cursor_src");
    let repo1 = make_repo(&src, "ichi");
    let repo2 = make_repo(&src, "nidan");
    let _cfg = setup_root_and_config(&rpc, "plug_cursor").await;

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.recommend({{ {{ \"file://{r1}\", name = \"ichi\" }},\n\
               {{ \"file://{r2}\", name = \"nidan\" }} }})\n\
             nx.plugins.bootstrap()",
            r1 = q(&repo1),
            r2 = q(&repo2)
        ),
    )
    .await;

    assert!(poll_true(&rpc, "return vim.bo.filetype == 'nxpluginswelcome'").await);

    // The first item is at 1-based line WELCOME_HEADER + 1 = 4 (2 intro + 1 blank).
    let mut landed = false;
    for _ in 0..200 {
        if cursor(&rpc).await.0 == 4 {
            landed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        landed,
        "the welcome cursor should start on the first item (line 4); got {:?}",
        cursor(&rpc).await
    );

    // And it must STAY there (not get reset back above the list by a late grab/render).
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        cursor(&rpc).await.0,
        4,
        "the welcome cursor should rest on the first item, not drift off the list"
    );
}

// ----- the welcome checklist is a trust gate: it must show the full source ----

// Ticking a recommendation fetches and runs that code, so the checklist has to show
// the EXACT clone target (owner/repo / url / dir), never just a friendly name + a
// human description — otherwise a benign-looking `desc` could disguise a hostile
// `src`. The rendered item line must contain the full source even when a desc is set.
#[tokio::test]
async fn welcome_shows_the_full_source_not_just_name_and_desc() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_src_visible_src");
    let repo = make_repo(&src, "realrepo");
    let _cfg = setup_root_and_config(&rpc, "plug_src_visible").await;

    // A spec whose friendly name + description say nothing about where the code comes
    // from — the source is a distinct `file://` path the user needs to see.
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.recommend({{ {{ \"file://{repo}\",\n\
               name = \"friendly\", desc = \"Looks harmless\" }} }})\n\
             nx.plugins.bootstrap()",
            repo = q(&repo)
        ),
    )
    .await;

    assert!(poll_true(&rpc, "return vim.bo.filetype == 'nxpluginswelcome'").await);

    // The rendered checklist line carries the FULL source path, not just "friendly".
    let needle = format!("file://{repo}", repo = repo.display());
    let mut shown = false;
    for _ in 0..200 {
        let ls = lines(&rpc).await;
        if ls.iter().any(|l| l.contains('☑') && l.contains(&needle)) {
            shown = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        shown,
        "the welcome checklist must render the full source ({needle}) on the ticked item, \
         not hide it behind the name/desc"
    );
}

// A long plugin description must be REAL buffer text, not an end-of-line virt_text
// decoration: only real text wraps with the window (the welcome sets wrap=true), so a
// description longer than the float width would otherwise be truncated at the right
// edge instead of flowing onto a continuation row. Asserting the desc lands in
// nvim_buf_get_lines proves it is wrappable buffer content rather than virtual text.
#[tokio::test]
async fn welcome_long_description_is_wrappable_real_text() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_desc_wrap_src");
    let repo = make_repo(&src, "wraprepo");
    let _cfg = setup_root_and_config(&rpc, "plug_desc_wrap").await;

    let long = "A very long recommended-plugin description that comfortably exceeds the welcome \
         float width so the welcome view must wrap it onto another row";
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.recommend({{ {{ \"file://{repo}\",\n\
               name = \"wrappy\", desc = \"{long}\" }} }})\n\
             nx.plugins.bootstrap()",
            repo = q(&repo)
        ),
    )
    .await;

    assert!(poll_true(&rpc, "return vim.bo.filetype == 'nxpluginswelcome'").await);

    // The description text must show up in the BUFFER LINES, not only as a virt_text
    // decoration — that is what lets wrap=true reflow it instead of clipping it.
    let mut shown = false;
    for _ in 0..200 {
        if lines(&rpc).await.iter().any(|l| l.contains(long)) {
            shown = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        shown,
        "the welcome description must be real buffer text (so wrap can reflow it), \
         not an end-of-line virt_text decoration that gets clipped at the float edge"
    );
}

// ----- the manager UI: live task state + the dashboard view ------------------

// The manager UI renders LIVE per-plugin progress from `M._tasks` and re-renders via
// `M.on_change`. Install records a `done` task with the result word the UI shows, and
// the change watchers fire — the two hooks the dashboard is built on.
#[tokio::test]
async fn install_records_task_state_and_fires_on_change() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_tasks_src");
    let repo = make_repo(&src, "tau");
    let _root = setup_root(&rpc, "plug_tasks").await;

    exec_lua(
        &rpc,
        "_G.changes = 0\n\
         nx.plugins.on_change(function() _G.changes = _G.changes + 1 end)",
    )
    .await;
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ \"file://{repo}\", name = \"tau\" }} }}\n\
             nx.plugins.install():catch(function(e) _G.err = tostring(e and e.message or e) end)",
            repo = q(&repo)
        ),
    )
    .await;

    assert!(
        poll_true(
            &rpc,
            "local t = nx.plugins._tasks.tau\n\
             return t ~= nil and t.state == 'done' and t.msg == 'installed'"
        )
        .await,
        "install should record a done task for the plugin; err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    assert!(
        poll_true(&rpc, "return _G.changes > 0").await,
        "on_change watchers should fire during install"
    );
}

// `:Plugins` opens the lazy-style dashboard — a focused view listing the declared
// plugins and the action-key hints.
#[tokio::test]
async fn plugins_command_opens_the_manager_dashboard() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_ui_src");
    let repo = make_repo(&src, "vista");
    setup_root(&rpc, "plug_ui").await;

    // A local `dir` plugin loads eagerly with no clone, so it shows up at once.
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ name = \"vista\", dir = \"{dir}\" }} }}",
            dir = q(&repo)
        ),
    )
    .await;
    assert_eq!(
        lua_bool(&rpc, "return nx.user_command.get().Plugins ~= nil").await,
        Some(true),
        ":Plugins command should be registered"
    );
    exec_lua(&rpc, "vim.cmd('Plugins')").await;

    assert!(
        poll_true(&rpc, "return vim.bo.filetype == 'nxplugins'").await,
        ":Plugins should open the manager dashboard"
    );
    let mut ok = false;
    for _ in 0..200 {
        let body = lines(&rpc).await.join("\n");
        if body.contains("vista") && body.contains("I install") {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        ok,
        "the dashboard should list the plugin and the action-key hints"
    );

    // The component blanks the end-of-buffer `~` fillers in its window (the dashboard
    // is plugin-owned content, not an editable file). `eob:<space>` → no tildes.
    assert!(
        poll_true(&rpc, "return vim.wo.fillchars == 'eob: '").await,
        "the component window should hide the end-of-buffer fill characters"
    );
    // The dashboard wraps long rows (the key hint) instead of clipping them, and
    // insets its content via the 'padding' window option.
    assert!(
        poll_true(&rpc, "return vim.wo.wrap == true").await,
        "the dashboard should enable line wrapping"
    );
    assert!(
        poll_true(
            &rpc,
            "return vim.wo.padding ~= '' and vim.wo.padding ~= nil"
        )
        .await,
        "the dashboard should set a 'padding' margin"
    );
}

// Installing a missing plugin from the dashboard (pressing `I`) pops the
// restart-required notice once the clone lands — a fresh clone only loads cleanly
// from a clean startup.
#[tokio::test]
async fn installing_via_the_manager_prompts_to_restart() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_restart_src");
    let repo = make_repo(&src, "rho");
    setup_root(&rpc, "plug_restart").await;

    // An eager plugin declared but not yet on disk → it shows under "Missing".
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ \"file://{repo}\", name = \"rho\" }} }}",
            repo = q(&repo)
        ),
    )
    .await;
    exec_lua(&rpc, "vim.cmd('Plugins')").await;

    // Wait for the dashboard to render (its key hint present ⇒ setup ran, so the `I`
    // map is bound), then press I once to install.
    let mut ready = false;
    for _ in 0..200 {
        if lines(&rpc).await.join("\n").contains("I install") {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ready, "the dashboard should render before we press I");
    feed(&rpc, "I");

    // The clone lands and the restart notice fires.
    assert!(
        poll_true(
            &rpc,
            "local t = nx.plugins._tasks.rho\n\
             return t ~= nil and t.state == 'done'"
        )
        .await,
        "pressing I should install the missing plugin"
    );
    assert!(
        poll_true(&rpc, "return nx.plugins.ui._restart_shown == true").await,
        "installing via the manager should prompt to restart"
    );
}

// The restart notice is a plain, NON-grabbing content float (`nx.ui.float`) — a
// transient popup the next key wipes. A single <Esc> dismisses it even though that
// same <Esc> also fires the manager's own <Esc> map underneath, because a transient
// content float is dismissed at the per-key DISPATCH level (before the key routes
// into a mapping), not only when a key reaches `Editor::input`. (Regression: the
// dismissal used to run only inside `Editor::input`, which a mapped <Esc> bypasses,
// so the notice lingered on that first <Esc> and a second was needed to clear it.)
#[tokio::test]
async fn one_esc_dismisses_the_restart_notice() {
    let (rpc, mut incoming) = start().await;
    let src = temp_dir("plug_restart_focus_src");
    let repo = make_repo(&src, "phi");
    setup_root(&rpc, "plug_restart_focus").await;

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ \"file://{repo}\", name = \"phi\" }} }}",
            repo = q(&repo)
        ),
    )
    .await;
    exec_lua(&rpc, "vim.cmd('Plugins')").await;

    let mut ready = false;
    for _ in 0..200 {
        if lines(&rpc).await.join("\n").contains("I install") {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ready, "the dashboard should render before we press I");
    feed(&rpc, "I");

    // The notice fires and shows as the content float over the open manager.
    assert!(
        poll_true(&rpc, "return nx.plugins.ui._restart_shown == true").await,
        "installing via the manager should prompt to restart"
    );
    assert!(
        poll_float_present(&rpc, &mut incoming, true).await,
        "the restart notice should show as a content float"
    );

    // A single <Esc> dismisses it — even though the manager's own <Esc> map (a Lua
    // handler that fires outside `Editor::input`) runs on the same key.
    feed(&rpc, "<Esc>");
    assert!(
        poll_float_present(&rpc, &mut incoming, false).await,
        "one <Esc> should dismiss the restart notice content float"
    );
}

// `:PluginsWelcome` opens the welcome checklist ON DEMAND, ignoring the first-run
// ask-once marker (and the "no plugins declared" gate), then installs + persists the
// chosen set — the way to re-pick the recommended set after first-run is over.
#[tokio::test]
async fn plugins_welcome_command_reopens_after_marker() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_welcmd_src");
    let repo = make_repo(&src, "kappa");
    let (root, cfg) = setup_root_and_config(&rpc, "plug_welcmd").await;

    // Simulate "first-run already happened": the marker exists and a set is declared.
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join(".recommended-prompted"), b"1\n").unwrap();
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.recommend({{ {{ \"file://{repo}\", name = \"kappa\" }} }})",
            repo = q(&repo)
        ),
    )
    .await;

    // The command opens the welcome despite the marker; confirm installs + persists.
    exec_lua(&rpc, "vim.cmd('PluginsWelcome')").await;
    assert!(
        poll_true(&rpc, "return vim.bo.filetype == 'nxpluginswelcome'").await,
        ":PluginsWelcome should open the checklist even after the marker exists"
    );
    assert!(
        feed_until(&rpc, "<CR>", "return nx.plugins._loaded.kappa == true").await,
        "confirming :PluginsWelcome installs the chosen set"
    );
    let pluginslua = std::fs::read_to_string(cfg.join("lua").join("plugins.lua")).unwrap();
    assert!(
        pluginslua.contains("kappa"),
        "the chosen set is written to plugins.lua (got: {pluginslua:?})"
    );
}

// ----- built-in default recommended set --------------------------------------

// With ServerInit.offer_default_recommended set (the interactive binary), nxvim's
// built-in default set is active on a fresh setup even when the user's config
// registers none — so the welcome appears. The test stays hermetic: it routes the
// install root + config at temp dirs and SKIPS (no network clone).
#[tokio::test]
async fn built_in_default_recommended_offers_when_config_registers_none() {
    let base = temp_dir("plug_default");
    let root = base.join("data");
    let cfg = base.join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    // The config registers NO recommended set — only hermetic paths.
    std::fs::write(
        cfg.join("init.lua"),
        format!(
            "nx.plugins.setup_manager({{ root = \"{root}\", config = \"{cfg}\" }})\n",
            root = q(&root),
            cfg = q(&cfg)
        ),
    )
    .unwrap();
    let init = ServerInit {
        config_dir: Some(cfg.clone()),
        runtimepath: vec![cfg.clone()],
        offer_default_recommended: true,
        ..Default::default()
    };
    let (rpc, _incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    // The built-in default became the active recommended set (the config set none)...
    assert!(
        poll_true(&rpc, "return #nx.plugins._recommended > 0").await,
        "the built-in default set should activate when the config registers none"
    );
    // ...so the first-run welcome appears.
    assert!(
        poll_true(&rpc, "return vim.bo.filetype == 'nxpluginswelcome'").await,
        "a fresh setup with the default offered shows the welcome"
    );
    // Skip it — no clone, hermetic.
    feed_until(
        &rpc,
        "<Esc>",
        "return vim.bo.filetype ~= 'nxpluginswelcome'",
    )
    .await;
}

// The default is OFF unless opted in: the headless harness (ServerInit::default,
// offer_default_recommended=false) keeps an empty recommended set, so no test ever
// trips the first-run welcome.
#[tokio::test]
async fn default_recommended_is_off_unless_opted_in() {
    let (rpc, _i) = start().await;
    assert_eq!(
        lua_bool(&rpc, "return #nx.plugins._recommended == 0").await,
        Some(true),
        "without offer_default_recommended the recommended set stays empty (tests stay hermetic)"
    );
}

#[tokio::test]
async fn plugin_clean_forgets_shada_namespace() {
    // :PluginClean removes an undeclared plugin's directory AND forgets its shada
    // namespace, so an uninstalled plugin doesn't leave cross-session data orphaned.
    let (rpc, _i) = start().await;
    let root = setup_root(&rpc, "plug_clean_shada").await;

    // An installed-but-now-undeclared plugin: a dir under the manager root, plus the
    // shada it stowed under its namespace (== its directory name).
    std::fs::create_dir_all(root.join("orphan")).unwrap();
    exec_lua(&rpc, r#"nx.shada.plugin("orphan"):set("seen", true)"#).await;
    assert_eq!(
        lua_bool(
            &rpc,
            r#"return vim.tbl_contains(nx.shada.namespaces(), "orphan")"#
        )
        .await,
        Some(true),
        "the orphan namespace is stored before cleaning"
    );

    // Nothing is declared, so clean removes the dir and forgets the namespace.
    exec_lua(&rpc, "nx.plugins.clean()").await;
    assert!(
        poll_true(
            &rpc,
            r#"return not vim.tbl_contains(nx.shada.namespaces(), "orphan")"#
        )
        .await,
        "clean() forgot the removed plugin's shada namespace"
    );
    assert!(
        exec_lua(&rpc, r#"return nx.shada.plugin("orphan"):get("seen")"#)
            .await
            .is_nil(),
        "the orphaned data is gone"
    );
    assert!(
        !root.join("orphan").exists(),
        "the plugin dir was pruned too"
    );
}

// ----- load-failure recovery ---------------------------------------------------

/// A load that fails mid-flight (here: a dependency that isn't installed) must
/// drop the `_loading` in-flight guard. Leaving it set wedges the plugin forever:
/// every later load attempt sees `_loading` truthy and silently resolves `false`,
/// so the plugin can never be retried (e.g. after a :PluginSync installs the dep).
#[tokio::test]
async fn a_failed_dependency_load_does_not_wedge_the_dependent() {
    let (rpc, _incoming) = start().await;
    setup_root(&rpc, "wedge").await;

    // A real local dir for the dependent (loadable), depending on a declared but
    // never-installed plugin, so the dependency's load rejects.
    let dir = temp_dir("wedge_leafy");
    std::fs::create_dir_all(dir.join("plugin")).unwrap();
    std::fs::write(
        dir.join("plugin").join("leafy.lua"),
        "_G.leafy_plugin = true\n",
    )
    .unwrap();

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.add({{ {{ name = \"leafy\", dir = \"{}\", lazy = true,\n\
             \x20 dependencies = {{ \"ghost/ghost\" }} }} }})\n\
             _G.first = nil\n\
             nx.plugins.load(\"leafy\"):next(\n\
             \x20 function(v) _G.first = \"resolved:\" .. tostring(v) end,\n\
             \x20 function(e) _G.first = \"rejected\" end)",
            q(&dir)
        ),
    )
    .await;

    assert!(
        poll_true(&rpc, "return _G.first ~= nil").await,
        "the load settled"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.first").await.as_str(),
        Some("rejected"),
        "the missing dependency rejects the dependent's load"
    );
    // The substance: the in-flight guard is released again.
    assert_eq!(
        lua_bool(&rpc, "return nx.plugins._loading.leafy ~= true").await,
        Some(true),
        "a failed load must clear the _loading guard so the plugin can be retried"
    );
    // And a retry is a real retry — it rejects loudly again instead of silently
    // resolving false off the stale guard.
    exec_lua(
        &rpc,
        "_G.second = nil\n\
         nx.plugins.load(\"leafy\"):next(\n\
         \x20 function(v) _G.second = \"resolved:\" .. tostring(v) end,\n\
         \x20 function(e) _G.second = \"rejected\" end)",
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.second ~= nil").await,
        "the retry settled"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.second").await.as_str(),
        Some("rejected"),
        "a retry after a failed load re-attempts (and re-reports) instead of no-opping"
    );
}
