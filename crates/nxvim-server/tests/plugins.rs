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
use nxvim_test_harness::{attach, exec_lua, feed, lua_bool, spawn, start_attached, temp_dir};
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

/// Declare the manager's install root (a temp dir) so the test never touches the
/// host data dir, and return that root path.
async fn setup_root(rpc: &Rpc, tag: &str) -> PathBuf {
    let root = temp_dir(tag).join("install");
    exec_lua(
        rpc,
        &format!("nx.plugins.setup({{ root = \"{}\" }})", q(&root)),
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
            "nx.plugins.setup({{ root = \"{root}\", git = \"{git}\" }})\n\
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
            "nx.plugins.setup({{ root = \"{}\", config = \"{}\" }})",
            q(&root),
            q(&cfg)
        ),
    )
    .await;
    (root, cfg)
}

// On a fresh setup the first-run flow offers the recommended set; accepting it
// writes the set to the user's config (a separate plugins.lua that init.lua
// requires) and installs+loads it now.
#[tokio::test]
async fn first_run_offers_recommended_and_persists_on_yes() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_reco_src");
    let repo = make_repo(&src, "zeta");
    let (root, cfg) = setup_root_and_config(&rpc, "plug_reco").await;

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.recommend({{ {{ \"file://{repo}\", name = \"zeta\" }} }})\n\
             nx.plugins.bootstrap()",
            repo = q(&repo)
        ),
    )
    .await;

    // The confirm appears (after the async marker check); accept it.
    assert!(
        poll_true(&rpc, "return nx.plugins._prompting == true").await,
        "the recommended-set confirm should appear on a fresh setup"
    );
    feed(&rpc, "y");

    // The set installs + loads, and is persisted to the user's config.
    assert!(
        poll_true(&rpc, "return nx.plugins._loaded.zeta == true").await,
        "accepting installs and loads the recommended set"
    );
    let pluginslua = std::fs::read_to_string(cfg.join("lua").join("plugins.lua")).unwrap();
    assert!(
        pluginslua.contains("zeta"),
        "the set is written to lua/plugins.lua (got: {pluginslua:?})"
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

// Declining the offer records the marker (so it never asks again) and writes
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
    feed(&rpc, "n");

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
            "nx.plugins.setup({{ root = \"{root}\", config = \"{cfg}\" }})\n\
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

    // No exec_lua trigger: VimEnter alone must surface the prompt.
    assert!(
        poll_true(&rpc, "return nx.plugins._prompting == true").await,
        "VimEnter should drive the first-run recommended-set prompt"
    );
    feed(&rpc, "y");
    assert!(
        poll_true(&rpc, "return nx.plugins._loaded.theta == true").await,
        "accepting the VimEnter prompt installs the set"
    );
}
