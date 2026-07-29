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
    poll_true, q, spawn, start_attached, temp_dir,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
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

/// Declare the manager's install root AND config dir (both temp dirs) so the test
/// never touches the host's data or config dir, and return that root path.
///
/// The config dir matters as much as the install root: the LOCKFILE lives beside the
/// user's `init.lua` (`<config>/nxvim-lock.json`), so a `sync()` under the default
/// config dir writes the developer's real lockfile — a hermeticity leak that also
/// makes two tests race over one host file.
async fn setup_root(rpc: &Rpc, tag: &str) -> PathBuf {
    let base = temp_dir(tag);
    let root = base.join("install");
    exec_lua(
        rpc,
        &format!(
            "nx.plugins.setup_manager({{ root = \"{}\", config = \"{}\" }})",
            q(&root),
            q(&base.join("config"))
        ),
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
             nx.plugins.install():next(function() _G.done = true end)
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
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

// ----- a failed clone fails loud (never hangs) -------------------------------

// The manager runs git in-process via `nx.git_local` (gix — no `git` binary, so no
// child process that could open /dev/tty and hang the editor on a credential prompt).
// The invariant that guards is: an unreachable / invalid repo REJECTS loud with a
// captured error, fast — never a silent success, never a hang. Here an install of a
// bogus `file://` path must reject and surface its message.
#[tokio::test]
async fn install_of_unreachable_repo_rejects_loud() {
    let (rpc, _i) = start().await;
    let dir = temp_dir("plug_bad_clone");
    let root = dir.join("install");
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.setup_manager({{ root = \"{root}\", config = \"{cfg}\" }})\n\
             nx.plugins {{ {{ \"file:///no/such/repo\", name = \"zeta\" }} }}\n\
             nx.plugins.install():next(\n\
               function() _G.err = false end,\n\
               function(e) _G.err = tostring(e and e.message or e) end)",
            root = q(&root),
            cfg = q(&dir.join("config")),
        ),
    )
    .await;

    assert!(
        poll_true(&rpc, "return type(_G.err) == 'string'").await,
        "install of an unreachable repo must reject loud; _G.err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    // The rejection carries a git-failure message (not a silent empty resolve).
    let msg = exec_lua(&rpc, "return _G.err").await;
    assert!(
        msg.as_str()
            .map(|s| s.contains("clone failed"))
            .unwrap_or(false),
        "the reject should name the failed clone; got {msg:?}"
    );
}

// ----- submodules are initialised on install --------------------------------

// Build a plugin repo at `<base>/<name>.git-src` that vendors `sub` as a git submodule
// at `vendored/`. Returns its path. (`-c protocol.file.allow=always` lets `git
// submodule add` use the local sub path; the manager's own gix clone is unaffected.)
fn make_repo_with_submodule(base: &Path, name: &str, sub: &Path) -> PathBuf {
    let repo = make_repo(base, name);
    git(
        &repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            &sub.to_string_lossy(),
            "vendored",
        ],
    );
    git(&repo, &["commit", "-q", "-m", "add submodule"]);
    repo
}

// A plugin may vendor its dependencies as git submodules. On install the manager runs
// the equivalent of `git submodule update --init --recursive`, so a submodule-bearing
// plugin lands COMPLETE (its vendored files really on disk). It is DEFAULT ON;
// `submodules = false` opts a plugin out — its submodule dir then stays empty. This is
// a real behavior test (gix in-process, no argv to inspect): assert the vendored file's
// presence, and its absence for the opted-out plugin.
#[tokio::test]
async fn submodules_are_initialised_on_install() {
    let (rpc, _i) = start().await;
    let base = temp_dir("plug_submodules");
    let src = base.join("src");
    std::fs::create_dir_all(&src).unwrap();

    // A standalone sub-repo with a vendored file, embedded in two plugin repos.
    let sub = make_repo(&src, "vendored_dep");
    let withsub = make_repo_with_submodule(&src, "withsub", &sub);
    let nosub = make_repo_with_submodule(&src, "nosub", &sub);

    let root = base.join("install");
    let root_s = root.to_string_lossy().to_string();
    // Default (submodules on) vs opted-out.
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.setup_manager({{ root = \"{root}\", config = \"{cfg}\" }})\n\
             nx.plugins {{\n\
               {{ \"file://{withsub}\", name = \"withsub\" }},\n\
               {{ \"file://{nosub}\", name = \"nosub\", submodules = false }} }}\n\
             nx.plugins.sync():next(function() _G.synced = true end)\n\
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
            root = q(&root),
            cfg = q(&base.join("config")),
            withsub = q(&withsub),
            nosub = q(&nosub),
        ),
    )
    .await;

    assert!(
        poll_true(&rpc, "return _G.synced == true").await,
        "sync should complete; err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );

    // The default plugin's submodule was initialised — its vendored file is present.
    let withsub_dep = Path::new(&root_s)
        .join("withsub")
        .join("vendored")
        .join("lua")
        .join("vendored_dep")
        .join("init.lua");
    assert!(
        withsub_dep.exists(),
        "a default plugin must have its submodule checked out at {withsub_dep:?}"
    );
    // The opted-out plugin's submodule stayed empty (no submodule_update ran).
    let nosub_dep = Path::new(&root_s)
        .join("nosub")
        .join("vendored")
        .join("lua")
        .join("vendored_dep")
        .join("init.lua");
    assert!(
        !nosub_dep.exists(),
        "submodules = false must leave the submodule un-initialised (found {nosub_dep:?})"
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

// On a fresh setup the first-run flow opens the WELCOME offer; accepting it (the whole
// recommended set) writes it to the user's config (a separate plugins.lua that init.lua
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
            "nx.plugins.recommend({{\n\
               {{ \"file://{repo}\", name = \"zeta\", desc = \"Zeta the plugin\" }} }})\n\
             nx.plugins.bootstrap()",
            repo = q(&repo)
        ),
    )
    .await;

    // The welcome offer appears (after the async marker check) and grabs focus — the
    // current buffer becomes the welcome view.
    assert!(
        poll_true(&rpc, "return nx.plugins._prompting == true").await,
        "the recommended-set welcome should appear on a fresh setup"
    );
    assert!(
        poll_true(&rpc, "return vim.bo.filetype == 'nxpluginswelcome'").await,
        "the welcome offer view should be focused"
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

    // <CR> accepts the whole offered set → install + load + persist.
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
// welcome offer OPENS THE MANAGER DASHBOARD, and the chosen set installs THERE
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

// ----- the offer screen: one decision, with the list behind `c` --------------

/// Wait for the first-run OFFER, then `c` into the customize checklist — the path to
/// every per-plugin assertion now that the offer itself lists nothing.
async fn open_customize(rpc: &Rpc) -> bool {
    poll_true(rpc, "return vim.bo.filetype == 'nxpluginswelcome'").await
        && feed_until(rpc, "c", "return vim.bo.filetype == 'nxpluginscustomize'").await
}

// The offer is ONE decision: it must NOT enumerate the set (that's what made it too
// long to read), but it must still answer "whose code is this?" — so it names the
// distinct ORIGINS of the sources, and the count of what's being offered.
#[tokio::test]
async fn welcome_offer_summarizes_instead_of_listing_the_set() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_offer_src");
    let repo1 = make_repo(&src, "alpha");
    let repo2 = make_repo(&src, "beta");
    let _cfg = setup_root_and_config(&rpc, "plug_offer").await;

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.recommend({{ {{ \"file://{r1}\", name = \"alpha\" }},\n\
               {{ \"file://{r2}\", name = \"beta\" }} }})\n\
             nx.plugins.bootstrap()",
            r1 = q(&repo1),
            r2 = q(&repo2)
        ),
    )
    .await;

    assert!(poll_true(&rpc, "return vim.bo.filetype == 'nxpluginswelcome'").await);

    // The offer names the count and the shared origin (the temp dir both repos sit in).
    let origin = format!("file://{}", q(&src));
    let mut summarized = false;
    for _ in 0..200 {
        let ls = lines(&rpc).await;
        if ls
            .iter()
            .any(|l| l.contains("2 plugins from") && l.contains(&origin))
        {
            summarized = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        summarized,
        "the offer should summarize the set (count + origins); got {:?}",
        lines(&rpc).await
    );

    // …and it must not enumerate it: no per-plugin checklist rows on this screen.
    let ls = lines(&rpc).await;
    assert!(
        !ls.iter().any(|l| l.contains('☑') || l.contains('☐')),
        "the offer must not list the individual plugins; got {ls:?}"
    );

    // `c` is the way to the full list — and the per-plugin sources are there.
    assert!(
        feed_until(&rpc, "c", "return vim.bo.filetype == 'nxpluginscustomize'").await,
        "`c` should open the customize checklist"
    );
    let needle = format!("file://{}", repo1.display());
    let mut listed = false;
    for _ in 0..200 {
        if lines(&rpc)
            .await
            .iter()
            .any(|l| l.contains('☑') && l.contains(&needle))
        {
            listed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(listed, "the customize checklist should list every source");
}

// ----- partial selection: unticking excludes a plugin ------------------------

// The headline of the customize checklist: the user can untick the plugins they don't
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

    // Offer up, `c` into the checklist, rendered with both items pre-ticked. Rendered
    // content ⇒ the component's setup ran and its buffer-local maps are bound.
    assert!(open_customize(&rpc).await);
    let mut rendered = false;
    for _ in 0..200 {
        let ls = lines(&rpc).await;
        if ls.iter().filter(|l| l.contains('☑')).count() == 2 {
            rendered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        rendered,
        "the checklist should render both items pre-ticked"
    );

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

// On mount the checklist must land the cursor ON the first item, not above the list:
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

    assert!(open_customize(&rpc).await);

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
        "the checklist cursor should start on the first item (line 4); got {:?}",
        cursor(&rpc).await
    );

    // And it must STAY there (not get reset back above the list by a late grab/render).
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        cursor(&rpc).await.0,
        4,
        "the checklist cursor should rest on the first item, not drift off the list"
    );
}

// ----- the customize checklist is a trust gate: it must show the full source -

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

    assert!(open_customize(&rpc).await);

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
        "the customize checklist must render the full source ({needle}) on the ticked item, \
         not hide it behind the name/desc"
    );
}

// A long plugin description must be REAL buffer text, not an end-of-line virt_text
// decoration: only real text wraps with the window (the checklist sets wrap=true), so a
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

    assert!(open_customize(&rpc).await);

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
        "the checklist description must be real buffer text (so wrap can reflow it), \
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
             nx.plugins.install():next(function() _G.done = true end)
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
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

// `:PluginsWelcome` opens the welcome offer ON DEMAND, ignoring the first-run
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
        ":PluginsWelcome should open the offer even after the marker exists"
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

    // The offer summarizes the REAL built-in set: its size, and the origins its code
    // comes from — including `rafamadriz` (friendly-snippets), which is only reachable
    // as a dependency, so this also proves dependencies count toward the trust summary.
    let n = exec_lua(&rpc, "return #nx.plugins._recommended").await;
    let n = n.as_i64().expect("recommended count");
    let want = format!("{n} plugins from github.com/nxvim, github.com/rafamadriz.");
    let mut summarized = false;
    for _ in 0..200 {
        if lines(&rpc).await.iter().any(|l| l.contains(&want)) {
            summarized = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        summarized,
        "the offer should summarize the built-in set as {want:?}; got {:?}",
        lines(&rpc).await
    );

    // `c` lists every one of them, each with its exact clone source.
    assert!(feed_until(&rpc, "c", "return vim.bo.filetype == 'nxpluginscustomize'").await);
    let mut listed = false;
    for _ in 0..200 {
        let ls = lines(&rpc).await;
        if ls.iter().filter(|l| l.contains('☑')).count() == n as usize
            && ls.iter().any(|l| l.contains("nxvim/nxvim-tree"))
        {
            listed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        listed,
        "the checklist should list all {n} built-in recommendations with their sources; \
         got {:?}",
        lines(&rpc).await
    );

    // Skip it — no clone, hermetic.
    feed_until(
        &rpc,
        "<Esc>",
        "return vim.bo.filetype ~= 'nxpluginscustomize'",
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

// ----- system-plugin tier: runtime promotion (§A) ----------------------------

/// `nx.plugins.system{...}` clones a plugin into the system dir (via the local-always
/// seam) and loads it into the current session, so it takes effect now AND is re-seeded
/// by the client into every future session. It stays OUT of the managed spec set.
#[tokio::test]
async fn nx_plugins_system_clones_into_the_system_dir_and_loads() {
    let base = temp_dir("plugins_system_api");
    let src = base.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let repo = make_repo(&src, "conn");
    let sysdir = base.join("data").join("system");

    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.setup_manager({{ root = \"{root}\", system = \"{sys}\", config = \"{cfg}\" }})\n\
             nx.plugins.system({{ \"file://{repo}\", name = \"conn\" }})",
            root = q(&base.join("install")),
            sys = q(&sysdir),
            cfg = q(&base.join("config")),
            repo = q(&repo),
        ),
    )
    .await;

    // The plugin's own plugin/ script ran → it loaded into this session.
    assert!(
        poll_true(&rpc, "return _G.conn_plugin == true").await,
        "nx.plugins.system must load the plugin into the current session",
    );
    // It landed physically under the system dir (so the client re-seeds it next launch).
    assert!(
        sysdir.join("conn").join("plugin").join("conn.lua").exists(),
        "the plugin must be cloned into the system dir",
    );
    // Registered in the tier, and NOT leaked into the managed spec set.
    assert_eq!(
        lua_bool(&rpc, "return nx.plugins._system['conn'] ~= nil").await,
        Some(true),
        "the plugin must be registered in the system tier",
    );
    assert_eq!(
        lua_bool(&rpc, "return nx.plugins._specs['conn'] == nil").await,
        Some(true),
        "a system plugin must not become a managed spec (sync/clean must ignore it)",
    );
}

/// `nx.plugins.promote(name)` moves an already-declared managed plugin into the system
/// tier: it clones the on-disk checkout into the system dir and registers it, so it
/// persists into every future session. Rejects loudly for an unknown plugin.
#[tokio::test]
async fn nx_plugins_promote_moves_a_managed_plugin_into_the_tier() {
    let base = temp_dir("plugins_promote");
    let src = base.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let repo = make_repo(&src, "promo");
    let sysdir = base.join("data").join("system");

    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.setup_manager({{ root = \"{root}\", system = \"{sys}\", config = \"{cfg}\" }})\n\
             nx.plugins({{ {{ \"file://{repo}\", name = \"promo\" }} }})\n\
             nx.plugins.sync()",
            root = q(&base.join("install")),
            sys = q(&sysdir),
            cfg = q(&base.join("config")),
            repo = q(&repo),
        ),
    )
    .await;
    // Wait for the managed plugin to install + load first.
    assert!(
        poll_true(&rpc, "return nx.plugins._loaded['promo'] == true").await,
        "the managed plugin should install and load",
    );

    // Promote it into the system tier.
    exec_lua(
        &rpc,
        "_G.PROMOTED = nil\nnx.plugins.promote('promo'):next(function() _G.PROMOTED = true end)",
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.PROMOTED == true").await,
        "promote should resolve",
    );
    assert!(
        sysdir
            .join("promo")
            .join("plugin")
            .join("promo.lua")
            .exists(),
        "promote must clone the plugin into the system dir",
    );
    assert_eq!(
        lua_bool(&rpc, "return nx.plugins._system['promo'] ~= nil").await,
        Some(true),
        "the promoted plugin must be registered in the system tier",
    );

    // Unknown plugin rejects loudly.
    exec_lua(&rpc, "_G.REJ = nil\nnx.plugins.promote('nope'):next(function() _G.REJ = 'ok' end, function() _G.REJ = 'rej' end)").await;
    assert!(poll_true(&rpc, "return _G.REJ ~= nil").await, "settled");
    assert_eq!(
        exec_lua(&rpc, "return _G.REJ").await.as_str(),
        Some("rej"),
        "promote of an unknown plugin must reject",
    );
}

/// An EAGER, local-`dir` plugin declared in `init.lua` must have its `plugin/` script
/// sourced EXACTLY ONCE — not twice. Its runtimepath entry is added synchronously in
/// `init.lua` (before the boot `source_plugins` pass), so without the manager-owned skip
/// both the manager's `source_runtime` AND that native pass would source it (a real double
/// for any `plugin/` side effect). Regression guard for that footgun.
#[tokio::test]
async fn eager_local_dir_plugin_sources_plugin_scripts_once() {
    let base = temp_dir("plugin_double_source");

    // A local plugin dir with a COUNTING plugin/ script (a bool couldn't detect a double).
    let plugin_dir = base.join("delta");
    std::fs::create_dir_all(plugin_dir.join("lua").join("delta")).unwrap();
    std::fs::create_dir_all(plugin_dir.join("plugin")).unwrap();
    std::fs::write(
        plugin_dir.join("lua").join("delta").join("init.lua"),
        "return {}\n",
    )
    .unwrap();
    std::fs::write(
        plugin_dir.join("plugin").join("delta.lua"),
        "_G.delta_plugin_count = (_G.delta_plugin_count or 0) + 1\n",
    )
    .unwrap();

    // A config dir whose init.lua eagerly declares the plugin, so it is on the runtimepath
    // BEFORE the boot `source_plugins` pass runs.
    let config = base.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        format!(
            "nx.plugins {{ {{ name = \"delta\", dir = \"{}\" }} }}\n",
            q(&plugin_dir)
        ),
    )
    .unwrap();

    let init = ServerInit {
        config_dir: Some(config.clone()),
        runtimepath: vec![config.clone()],
        ..Default::default()
    };
    let (rpc, _incoming) = start_attached(init, 80, 24).await;

    // Wait for the eager load to settle (its module require-loads + plugin/ sources).
    assert!(
        poll_true(&rpc, "return nx.plugins._loaded.delta == true").await,
        "the eager local-dir plugin should load at startup",
    );

    // The plugin/ script ran exactly once — not twice.
    assert_eq!(
        exec_lua(&rpc, "return _G.delta_plugin_count")
            .await
            .as_i64(),
        Some(1),
        "an eager local-dir plugin's plugin/ script must be sourced exactly once",
    );
}

// ----- PluginsLoaded: fires once after every eager plugin has settled --------

// `PluginsLoaded` is the "all my non-lazy plugins are ready" hook. It fires once,
// after every eager plugin's config — including an ASYNC config that `nx.await`s — has
// run to completion. The realistic flow: a config's `init.lua` registers the handler
// AND declares its plugins (so both happen before the startup `VimEnter`), then the
// handler fires once the eager loads settle. It snapshots whether the async config had
// finished when it fired — proving "settled", not merely "load started".
#[tokio::test]
async fn plugins_loaded_event_fires_after_all_eager_settle() {
    let base = temp_dir("plug_allloaded");

    // A local plugin dir whose module init.lua has real content (the async config reads
    // it, so the config only settles a few ticks after the load starts).
    let plugin_dir = base.join("sigma");
    std::fs::create_dir_all(plugin_dir.join("lua").join("sigma")).unwrap();
    std::fs::write(
        plugin_dir.join("lua").join("sigma").join("init.lua"),
        "return { marker = true }\n",
    )
    .unwrap();
    let sigma_init = plugin_dir.join("lua").join("sigma").join("init.lua");

    let config = base.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        format!(
            "nx.on(\"PluginsLoaded\", {{}}, function()\n\
               _G.all_loaded = true\n\
               _G.cfg_done_at_fire = _G.sigma_cfg == true\n\
             end)\n\
             nx.plugins {{ {{ name = \"sigma\", dir = \"{dir}\", config = function()\n\
               local txt = nx.await(nx.fs.read_text(\"{f}\"))\n\
               _G.sigma_cfg = #txt > 0\n\
             end }} }}\n",
            dir = q(&plugin_dir),
            f = q(&sigma_init)
        ),
    )
    .unwrap();

    let init = ServerInit {
        config_dir: Some(config.clone()),
        runtimepath: vec![config.clone()],
        ..Default::default()
    };
    let (rpc, _incoming) = start_attached(init, 80, 24).await;

    assert!(
        poll_true(&rpc, "return _G.all_loaded == true").await,
        "PluginsLoaded should fire after the eager plugin loads"
    );
    assert_eq!(
        lua_bool(&rpc, "return _G.cfg_done_at_fire == true").await,
        Some(true),
        "PluginsLoaded must fire only after the async config had settled"
    );
}

// ----- the startup announce window: a plugin still sees the startup file ------

// The gap this closes. `nx.plugins` activates an eager spec asynchronously (it awaits the
// spec's dir before sourcing it), so the plugin's `config` — and every autocmd that config
// registers — lands several ticks into startup, after the startup file's `BufReadPost` has
// come and gone. A plugin whose behavior hangs off the read event would therefore do
// nothing at all for `nxvim file.txt`, while `:e file.txt` in the same session works.
//
// The fix is a replay, not a delay: the read fires on time (built-in `FileType` consumers
// still colour the file immediately), and it is re-announced to the handlers registered
// while the plugins were loading. This test registers BOTH handlers from an ASYNC config
// and asserts each one saw the startup file, with the buffer and match it was read with.
#[tokio::test]
async fn startup_read_is_replayed_to_a_plugin_that_loaded_after_it() {
    let base = temp_dir("plug_late_read");

    // The file the editor opens on the command line — read before any plugin loads.
    let startup = base.join("startup.rs");
    std::fs::write(&startup, "fn main() {}\n").unwrap();

    // A local plugin dir whose module has real content, so the async config below
    // genuinely settles a few ticks after the load starts.
    let plugin_dir = base.join("tardy");
    std::fs::create_dir_all(plugin_dir.join("lua").join("tardy")).unwrap();
    let module = plugin_dir.join("lua").join("tardy").join("init.lua");
    std::fs::write(&module, "return { marker = true }\n").unwrap();

    let config = base.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        format!(
            "nx.plugins {{ {{ name = \"tardy\", dir = \"{dir}\", config = function()\n\
               local txt = nx.await(nx.fs.read_text(\"{f}\"))\n\
               _G.tardy_cfg = #txt > 0\n\
               nx.on(\"BufReadPost\", {{}}, function(a)\n\
                 _G.read = a.file\n\
                 _G.read_buf = a.buf\n\
                 _G.read_after_cfg = _G.tardy_cfg == true\n\
               end)\n\
               nx.on(\"FileType\", {{}}, function(a) _G.ft = a.match end)\n\
             end }} }}\n",
            dir = q(&plugin_dir),
            f = q(&module)
        ),
    )
    .unwrap();

    let init = ServerInit {
        file: Some(startup.to_string_lossy().into_owned()),
        config_dir: Some(config.clone()),
        runtimepath: vec![config.clone()],
        ..Default::default()
    };
    let (rpc, _incoming) = start_attached(init, 80, 24).await;

    assert!(
        poll_true(&rpc, "return _G.read ~= nil").await,
        "a plugin's BufReadPost must reach the startup file even though the plugin \
         loaded after the read"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.read").await.as_str(),
        Some(startup.to_string_lossy().as_ref()),
        "the replay names the file that was actually read"
    );
    assert_eq!(
        lua_bool(&rpc, "return _G.read_buf == vim.api.nvim_get_current_buf()").await,
        Some(true),
        "and carries that buffer's own id"
    );
    assert_eq!(
        lua_bool(&rpc, "return _G.read_after_cfg == true").await,
        Some(true),
        "the replay lands only after the plugin's async config had settled"
    );
    // `FileType` is replayed too — this is what makes an ftplugin-shaped plugin work
    // on the startup file.
    assert_eq!(
        exec_lua(&rpc, "return _G.ft").await.as_str(),
        Some("rust"),
        "FileType is replayed with the buffer's filetype as the match"
    );
}

// The replay must not double-deliver. The config's OWN handler is registered before the
// startup read, so it receives the event once, on the read — and the replay, which is
// filtered by the same registration watermark the async settle uses, must skip it.
#[tokio::test]
async fn the_startup_replay_does_not_redeliver_to_handlers_that_already_saw_it() {
    let base = temp_dir("plug_replay_once");
    let startup = base.join("startup.rs");
    std::fs::write(&startup, "fn main() {}\n").unwrap();

    let plugin_dir = base.join("tardy");
    std::fs::create_dir_all(plugin_dir.join("lua").join("tardy")).unwrap();
    let module = plugin_dir.join("lua").join("tardy").join("init.lua");
    std::fs::write(&module, "return {}\n").unwrap();

    let config = base.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        format!(
            "_G.cfg_reads, _G.cfg_fts = 0, 0\n\
             nx.on(\"BufReadPost\", {{}}, function() _G.cfg_reads = _G.cfg_reads + 1 end)\n\
             nx.on(\"FileType\", {{}}, function() _G.cfg_fts = _G.cfg_fts + 1 end)\n\
             nx.plugins {{ {{ name = \"tardy\", dir = \"{dir}\", config = function()\n\
               nx.await(nx.fs.read_text(\"{f}\"))\n\
               _G.plugin_reads = 0\n\
               nx.on(\"BufReadPost\", {{}}, function() _G.plugin_reads = _G.plugin_reads + 1 end)\n\
             end }} }}\n",
            dir = q(&plugin_dir),
            f = q(&module)
        ),
    )
    .unwrap();

    let init = ServerInit {
        file: Some(startup.to_string_lossy().into_owned()),
        config_dir: Some(config.clone()),
        runtimepath: vec![config.clone()],
        ..Default::default()
    };
    let (rpc, _incoming) = start_attached(init, 80, 24).await;

    assert!(
        poll_true(&rpc, "return _G.plugin_reads == 1").await,
        "the plugin's handler gets the startup read exactly once"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.cfg_reads").await.as_i64(),
        Some(1),
        "the config's handler saw the read on the read itself, and must NOT be replayed to"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.cfg_fts").await.as_i64(),
        Some(1),
        "same for FileType"
    );
}

// The window is a STARTUP one: it closes at `PluginsLoaded` and never reopens. A handler
// registered afterwards (a lazy plugin's, a `:lua` at the prompt) gets the reads that
// follow it and nothing from before — otherwise every later registration would replay the
// startup file forever.
#[tokio::test]
async fn the_startup_replay_window_closes_at_plugins_loaded() {
    let base = temp_dir("plug_replay_closed");
    let startup = base.join("startup.rs");
    std::fs::write(&startup, "fn main() {}\n").unwrap();
    let later = base.join("later.rs");
    std::fs::write(&later, "fn later() {}\n").unwrap();

    let config = base.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("init.lua"), "_G.booted = true\n").unwrap();

    let init = ServerInit {
        file: Some(startup.to_string_lossy().into_owned()),
        config_dir: Some(config.clone()),
        runtimepath: vec![config.clone()],
        ..Default::default()
    };
    let (rpc, _incoming) = start_attached(init, 80, 24).await;
    // With no plugins declared, `PluginsLoaded` fires at `VimEnter` — so by the time we
    // can run anything over RPC the window is already shut.
    assert!(poll_true(&rpc, "return nx.plugins._plugins_loaded_fired == true").await);

    let _ = exec_lua(
        &rpc,
        "_G.seen = {}\n\
         nx.on('BufReadPost', {}, function(a) _G.seen[#_G.seen+1] = a.file end)\n\
         return true",
    )
    .await;
    // Registering did not replay the startup file at it.
    assert_eq!(
        exec_lua(&rpc, "return #_G.seen").await.as_i64(),
        Some(0),
        "a handler registered after PluginsLoaded is not replayed the startup read"
    );
    // But it does get everything from here on.
    feed(&rpc, &format!(":edit {}<CR>", later.display()));
    let _ = lines(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.seen[1]").await.as_str(),
        Some(later.to_string_lossy().as_ref()),
        "and still receives ordinary reads"
    );
}

// ----- PluginLoaded: per-plugin, fires when a specific plugin loads -----------

// `PluginLoaded` lets a config hook a SPECIFIC plugin's load — including a lazy one,
// which loads only when its trigger fires. The event's `pattern` is the plugin name
// (so an `nx.on("PluginLoaded", { pattern = name }, …)` handler targets just that one)
// and `args.data.name` carries it too.
#[tokio::test]
async fn plugin_loaded_event_fires_per_plugin_including_lazy() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_perload");
    let repo = make_repo(&src, "omega");
    setup_root(&rpc, "plug_perload").await;

    // A local `dir` plugin (no clone) made lazy by a `cmd` trigger.
    exec_lua(
        &rpc,
        &format!(
            "nx.on(\"PluginLoaded\", {{ pattern = \"omega\" }}, function(ev)\n\
               _G.omega_evt = ev.data and ev.data.name or ev.match\n\
             end)\n\
             nx.plugins {{ {{ name = \"omega\", dir = \"{dir}\", cmd = \"OmegaGo\",\n\
               config = function() require(\"omega\").setup() end }} }}",
            dir = q(&repo)
        ),
    )
    .await;

    // Lazy: not loaded, so the per-plugin event has not fired yet.
    assert_eq!(
        lua_bool(&rpc, "return _G.omega_evt == nil").await,
        Some(true),
        "a lazy plugin must not fire PluginLoaded before its trigger"
    );

    // Invoke the command that lazy-loads it.
    exec_lua(&rpc, "vim.cmd('OmegaGo')").await;

    assert!(
        poll_true(&rpc, "return _G.omega_evt == \"omega\"").await,
        "PluginLoaded should fire carrying the plugin name when the lazy plugin loads"
    );
}

// ----- the lockfile (Phase 1: record + read) ---------------------------------
//
// The manager supports pinning but never RECORDED what an unpinned plugin resolved to,
// so an install was whatever `HEAD` happened to be and an update could not be undone.
// Phase 1 writes `<config>/nxvim-lock.json` after every install/update/sync. See
// docs/plans/2026-07-25-plugin-lockfile.md.

/// `git rev-parse HEAD` in `dir`, trimmed — the SHA the lockfile must agree with.
fn head_sha(dir: &Path) -> String {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git rev-parse");
    assert!(out.status.success(), "git rev-parse failed in {dir:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[tokio::test]
async fn sync_writes_a_lockfile_recording_the_cloned_commit() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_lock_src");
    let repo = make_repo(&src, "alpha");
    let (root, cfg) = setup_root_and_config(&rpc, "plug_lock").await;

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\" }} }}\n\
             nx.plugins.sync():catch(function(e) _G.err = tostring(e and e.message or e) end)",
            repo = q(&repo)
        ),
    )
    .await;

    let lock = cfg.join("nxvim-lock.json");
    assert!(
        poll_true(&rpc, "return nx.plugins.locked().alpha ~= nil").await,
        "sync should record the plugin in the lock; err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    assert!(lock.exists(), "sync should write {lock:?}");

    // The recorded commit is the one actually checked out — in the file AND in memory.
    let want = head_sha(&root.join("alpha"));
    let text = std::fs::read_to_string(&lock).unwrap();
    assert!(
        text.contains(&want),
        "lockfile must record the checked-out SHA {want}; file={text}"
    );
    assert_eq!(
        exec_lua(&rpc, "return nx.plugins.locked().alpha.commit")
            .await
            .as_str(),
        Some(want.as_str())
    );
}

#[tokio::test]
async fn the_lockfile_is_pretty_printed_with_sorted_keys() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_locksort_src");
    let a = make_repo(&src, "zeta");
    let b = make_repo(&src, "alpha");
    let (_root, cfg) = setup_root_and_config(&rpc, "plug_locksort").await;

    // Declared zeta-first, so a sorted file proves the ORDER comes from the encoder and
    // not from declaration order — that is what keeps the committed file's diffs clean.
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ \"file://{a}\", name = \"zeta\" }}, {{ \"file://{b}\", name = \"alpha\" }} }}\n\
             nx.plugins.sync():catch(function(e) _G.err = tostring(e and e.message or e) end)",
            a = q(&a),
            b = q(&b)
        ),
    )
    .await;

    assert!(
        poll_true(
            &rpc,
            "return nx.plugins.locked().zeta ~= nil and nx.plugins.locked().alpha ~= nil"
        )
        .await,
        "err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );

    let text = std::fs::read_to_string(cfg.join("nxvim-lock.json")).unwrap();
    assert!(
        text.find("\"alpha\"") < text.find("\"zeta\""),
        "keys must be sorted, not in declaration order: {text}"
    );
    assert!(
        text.contains("\n  \""),
        "must be pretty-printed (2-space indent), not a one-liner: {text}"
    );
    assert!(text.ends_with('\n'), "must end with a newline: {text:?}");
}

#[tokio::test]
async fn a_dev_dir_plugin_is_never_locked() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_lockdev_src");
    let repo = make_repo(&src, "alpha");
    let devdir = make_repo(&src, "devplug");
    let (_root, cfg) = setup_root_and_config(&rpc, "plug_lockdev").await;

    // A `dir` plugin is a working checkout the manager never clones — locking it would
    // record a SHA nothing can reproduce, so it must be absent from the lockfile.
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\" }}, {{ dir = \"{dev}\", name = \"devplug\" }} }}\n\
             nx.plugins.sync():catch(function(e) _G.err = tostring(e and e.message or e) end)",
            repo = q(&repo),
            dev = q(&devdir)
        ),
    )
    .await;

    assert!(
        poll_true(&rpc, "return nx.plugins.locked().alpha ~= nil").await,
        "err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    assert!(cfg.join("nxvim-lock.json").exists());
    assert_eq!(
        lua_bool(
            &rpc,
            "return nx.plugins.locked().alpha ~= nil and nx.plugins.locked().devplug == nil"
        )
        .await,
        Some(true),
        "the managed plugin is locked, the dev `dir` plugin is not"
    );
}

#[tokio::test]
async fn a_malformed_lockfile_fails_loud() {
    let (rpc, _i) = start().await;
    let (_root, cfg) = setup_root_and_config(&rpc, "plug_lockbad").await;
    // A corrupt lock must never be silently treated as "nothing pinned" — that would
    // quietly drop every pin the user committed.
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("nxvim-lock.json"), "{ this is not json").unwrap();

    exec_lua(
        &rpc,
        "nx.plugins._read_lock()\n  :next(function() _G.ok = true end)\n  \
         :catch(function(e) _G.lock_err = tostring(e and e.message or e) end)",
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.lock_err ~= nil").await,
        "a malformed lockfile must reject, not resolve empty; ok={:?}",
        exec_lua(&rpc, "return _G.ok").await
    );
    let msg = exec_lua(&rpc, "return _G.lock_err").await;
    let msg = msg.as_str().unwrap_or("");
    assert!(
        msg.contains("nxvim-lock.json"),
        "the error must name the file: {msg}"
    );
}

#[tokio::test]
async fn plugin_lock_command_is_wired() {
    let (rpc, _i) = start().await;
    assert_eq!(
        lua_bool(&rpc, "return nx.user_command.get().PluginLock ~= nil").await,
        Some(true)
    );
}

// ----- the lockfile (Phase 2: install reproduces it) -------------------------

/// Add a second commit to a source repo and return its SHA — so a test can prove an
/// install landed on the LOCKED commit rather than on the remote's current tip.
fn add_commit(repo: &Path, marker: &str) -> String {
    std::fs::write(repo.join("SECOND.md"), format!("{marker}\n")).unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", marker]);
    head_sha(repo)
}

#[tokio::test]
async fn install_reproduces_the_locked_commit_not_the_remote_tip() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_repro_src");
    let repo = make_repo(&src, "alpha");
    let first = head_sha(&repo);

    // Machine 1: install at the repo's current tip and let it lock.
    let base = temp_dir("plug_repro");
    let cfg = base.join("config");
    let root_a = base.join("install-a");
    let root_b = base.join("install-b");
    std::fs::create_dir_all(&cfg).unwrap();
    let declare = format!(
        "nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\" }} }}\n\
         nx.plugins.sync():next(function() _G.done = true end)\n\
           :catch(function(e) _G.err = tostring(e and e.message or e) end)",
        repo = q(&repo)
    );
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.setup_manager({{ root = \"{}\", config = \"{}\" }})",
            q(&root_a),
            q(&cfg)
        ),
    )
    .await;
    exec_lua(&rpc, &declare).await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "machine 1 should install; err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    assert_eq!(head_sha(&root_a.join("alpha")), first);

    // The remote moves on. The lockfile still names `first`.
    let second = add_commit(&repo, "moved-on");
    assert_ne!(second, first);

    // Machine 2: same config + same lockfile, a fresh install root. It must land on the
    // LOCKED commit, not the remote's new tip — that is the whole point of the lockfile.
    exec_lua(
        &rpc,
        &format!(
            "_G.err, _G.done = nil, nil\nnx.plugins.setup_manager({{ root = \"{}\", config = \"{}\" }})",
            q(&root_b),
            q(&cfg)
        ),
    )
    .await;
    exec_lua(&rpc, &declare).await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "machine 2 should install; err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    assert_eq!(
        head_sha(&root_b.join("alpha")),
        first,
        "a locked install must reproduce the locked commit, not the remote tip {second}"
    );
}

#[tokio::test]
async fn an_explicit_spec_commit_outranks_the_lockfile() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_specpin_src");
    let repo = make_repo(&src, "alpha");
    let first = head_sha(&repo);
    let second = add_commit(&repo, "the-explicitly-pinned-one");

    let base = temp_dir("plug_specpin");
    let cfg = base.join("config");
    let root = base.join("install");
    std::fs::create_dir_all(&cfg).unwrap();
    // A lockfile naming `first` on disk...
    std::fs::write(
        cfg.join("nxvim-lock.json"),
        format!("{{\n  \"alpha\": {{ \"commit\": \"{first}\" }}\n}}\n"),
    )
    .unwrap();

    // ...but the spec explicitly pins `second`. A hand-written pin is an instruction;
    // the lock is only a record, so the spec must win.
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.setup_manager({{ root = \"{root}\", config = \"{cfg}\" }})\n\
             nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\", commit = \"{second}\" }} }}\n\
             nx.plugins.install():next(function() _G.done = true end)
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
            root = q(&root),
            cfg = q(&cfg),
            repo = q(&repo)
        ),
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    assert_eq!(
        head_sha(&root.join("alpha")),
        second,
        "an explicit spec commit must outrank the lockfile entry"
    );
}

#[tokio::test]
async fn an_unlocked_install_stays_shallow() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_shallow_src");
    let repo = make_repo(&src, "alpha");
    add_commit(&repo, "second");
    let (root, _cfg) = setup_root_and_config(&rpc, "plug_shallow").await;

    // With nothing locked, the clone must stay `depth = 1` — reaching a locked commit is
    // the ONLY reason to pay for a full clone, so an unlocked install keeps the speed-up.
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\" }} }}\n\
             nx.plugins.install():next(function() _G.done = true end)
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
            repo = q(&repo)
        ),
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    assert!(
        root.join("alpha").join(".git").join("shallow").exists(),
        "an unlocked clone should be shallow (.git/shallow present)"
    );
}

#[tokio::test]
async fn a_locked_install_is_deep_enough_to_reach_the_commit() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_deep_src");
    let repo = make_repo(&src, "alpha");
    let first = head_sha(&repo);
    add_commit(&repo, "second");

    let base = temp_dir("plug_deep");
    let cfg = base.join("config");
    let root = base.join("install");
    std::fs::create_dir_all(&cfg).unwrap();
    // `first` is the PARENT of the tip, so a depth-1 clone could not contain it: this
    // proves the locked path clones deep enough rather than just asking for a checkout.
    std::fs::write(
        cfg.join("nxvim-lock.json"),
        format!("{{\n  \"alpha\": {{ \"commit\": \"{first}\" }}\n}}\n"),
    )
    .unwrap();

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.setup_manager({{ root = \"{root}\", config = \"{cfg}\" }})\n\
             nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\" }} }}\n\
             nx.plugins.install():next(function() _G.done = true end)
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
            root = q(&root),
            cfg = q(&cfg),
            repo = q(&repo)
        ),
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    assert_eq!(head_sha(&root.join("alpha")), first);
}

#[tokio::test]
async fn a_lockfile_naming_an_unreachable_commit_fails_loud() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_badpin_src");
    let repo = make_repo(&src, "alpha");

    let base = temp_dir("plug_badpin");
    let cfg = base.join("config");
    let root = base.join("install");
    std::fs::create_dir_all(&cfg).unwrap();
    // A commit that does not exist (a force-pushed-away revision). Installing must FAIL
    // and say the pin came from the lockfile — silently falling back to the remote tip
    // would hand back a different plugin tree than the lockfile promises.
    std::fs::write(
        cfg.join("nxvim-lock.json"),
        "{\n  \"alpha\": { \"commit\": \"0123456789abcdef0123456789abcdef01234567\" }\n}\n",
    )
    .unwrap();

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.setup_manager({{ root = \"{root}\", config = \"{cfg}\" }})\n\
             nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\" }} }}\n\
             nx.plugins.install():next(function() _G.done = true end)
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
            root = q(&root),
            cfg = q(&cfg),
            repo = q(&repo)
        ),
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.err ~= nil").await,
        "an unreachable locked commit must reject, not silently install the tip"
    );
    let msg = exec_lua(&rpc, "return _G.err").await;
    let msg = msg.as_str().unwrap_or("");
    // The message must be actionable: which plugin, that the pin came from the lock, and
    // the file to edit. Without the path the user has no way to find the stale entry.
    assert!(msg.contains("alpha"), "must name the plugin: {msg}");
    assert!(msg.contains("is locked at"), "must say it is locked: {msg}");
    assert!(
        msg.contains("nxvim-lock.json"),
        "must name the lockfile to fix: {msg}"
    );
}

#[tokio::test]
async fn sync_reproduces_the_lock_while_update_advances_past_it() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_advance_src");
    let repo = make_repo(&src, "alpha");
    let first = head_sha(&repo);
    let tip = add_commit(&repo, "second");

    let base = temp_dir("plug_advance");
    let cfg = base.join("config");
    let root = base.join("install");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("nxvim-lock.json"),
        format!("{{\n  \"alpha\": {{ \"branch\": \"main\", \"commit\": \"{first}\" }}\n}}\n"),
    )
    .unwrap();

    // `sync` = install + update. The install lands on the locked commit; the update pass
    // must NOT then advance past it, or the lockfile would be honoured only for the
    // instant between the two halves. (It also must not fail: the locked checkout is
    // detached, and `pull` rejects a detached HEAD.)
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.setup_manager({{ root = \"{root}\", config = \"{cfg}\" }})\n\
             nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\" }} }}\n\
             nx.plugins.sync():next(function() _G.done = true end)\n\
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
            root = q(&root),
            cfg = q(&cfg),
            repo = q(&repo)
        ),
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "sync must succeed over a lock-installed (detached) plugin; err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    assert_eq!(
        head_sha(&root.join("alpha")),
        first,
        "sync must reproduce the locked commit, not advance past it"
    );

    // `_update` reports it as `locked` rather than pretending it is up to date.
    exec_lua(
        &rpc,
        "_G.status = nil\nnx.plugins._update('alpha'):next(function(s) _G.status = s end)",
    )
    .await;
    assert!(poll_true(&rpc, "return _G.status ~= nil").await);
    assert_eq!(
        exec_lua(&rpc, "return _G.status").await.as_str(),
        Some("locked")
    );

    // The tracked branch survived the detaching install, so update knows where to reattach.
    assert_eq!(
        exec_lua(&rpc, "return nx.plugins.locked().alpha.branch")
            .await
            .as_str(),
        Some("main")
    );

    // The explicit update verb DOES advance: it re-attaches to the branch, fast-forwards,
    // and re-records the lock. Without this a lock pin would be a permanent freeze.
    exec_lua(
        &rpc,
        "_G.upd, _G.uerr = nil, nil\n\
         nx.plugins.update():next(function(n) _G.upd = n end)\n\
           :catch(function(e) _G.uerr = tostring(e and e.message or e) end)",
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.upd ~= nil").await,
        "update should resolve; uerr={:?}",
        exec_lua(&rpc, "return _G.uerr").await
    );
    assert_eq!(
        head_sha(&root.join("alpha")),
        tip,
        "an explicit update must advance past the lock to the branch tip"
    );
    // HEAD is attached again, and the lockfile now records the new commit.
    assert!(
        poll_true(
            &rpc,
            &format!("return nx.plugins.locked().alpha.commit == \"{tip}\"")
        )
        .await,
        "update must re-record the lock at the advanced commit"
    );
    let text = std::fs::read_to_string(cfg.join("nxvim-lock.json")).unwrap();
    assert!(
        text.contains(&tip),
        "lockfile on disk should hold {tip}: {text}"
    );
}

#[tokio::test]
async fn update_of_a_detached_plugin_with_no_recorded_branch_fails_loud() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_nobranch_src");
    let repo = make_repo(&src, "alpha");
    let first = head_sha(&repo);
    add_commit(&repo, "second");

    let base = temp_dir("plug_nobranch");
    let cfg = base.join("config");
    let root = base.join("install");
    std::fs::create_dir_all(&cfg).unwrap();
    // A lock entry with NO branch (hand-written, or from an older nxvim): there is then no
    // way to know where to re-attach. Guessing a branch could move the plugin somewhere the
    // user never asked for, so it must fail loud and name the fix.
    std::fs::write(
        cfg.join("nxvim-lock.json"),
        format!("{{\n  \"alpha\": {{ \"commit\": \"{first}\" }}\n}}\n"),
    )
    .unwrap();

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.setup_manager({{ root = \"{root}\", config = \"{cfg}\" }})\n\
             nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\" }} }}\n\
             nx.plugins.install():next(function()\n\
               nx.plugins._update('alpha', {{ advance = true }}):next(function(s) _G.status = s end)\n\
                 :catch(function(e) _G.uerr = tostring(e and e.message or e) end)\n\
             end):catch(function(e) _G.err = tostring(e and e.message or e) end)",
            root = q(&root),
            cfg = q(&cfg),
            repo = q(&repo)
        ),
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.uerr ~= nil or _G.status ~= nil").await,
        "update should settle; err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    let msg = exec_lua(&rpc, "return _G.uerr").await;
    let msg = msg.as_str().unwrap_or("");
    assert!(
        msg.contains("detached") && msg.contains("branch"),
        "must explain that there is no branch to reattach to: {msg} (status={:?})",
        exec_lua(&rpc, "return _G.status").await
    );
}

// ----- the lockfile (Phase 3: restore + drift) --------------------------------

#[tokio::test]
async fn restore_moves_a_checkout_back_to_the_locked_commit() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_restore_src");
    let repo = make_repo(&src, "alpha");
    let first = head_sha(&repo);
    let tip = add_commit(&repo, "second");

    let (root, cfg) = setup_root_and_config(&rpc, "plug_restore").await;
    std::fs::create_dir_all(&cfg).unwrap();

    // Install unpinned: a SHALLOW clone at the tip, and the lock records the tip.
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\" }} }}\n\
             nx.plugins.install():next(function() _G.done = true end)\n\
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
            repo = q(&repo)
        ),
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    assert_eq!(head_sha(&root.join("alpha")), tip);
    assert!(
        root.join("alpha").join(".git").join("shallow").exists(),
        "precondition: the unpinned clone is shallow"
    );

    // The user checks out an OLDER lockfile from their config repo (naming `first`, a
    // commit the shallow clone does not contain) and restores.
    std::fs::write(
        cfg.join("nxvim-lock.json"),
        format!("{{\n  \"alpha\": {{ \"branch\": \"main\", \"commit\": \"{first}\" }}\n}}\n"),
    )
    .unwrap();
    exec_lua(
        &rpc,
        "_G.res, _G.rerr = nil, nil\n\
         nx.plugins.restore():next(function(r) _G.res = r end)\n\
           :catch(function(e) _G.rerr = tostring(e and e.message or e) end)",
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.res ~= nil").await,
        "restore should resolve; rerr={:?}",
        exec_lua(&rpc, "return _G.rerr").await
    );
    assert_eq!(
        head_sha(&root.join("alpha")),
        first,
        "restore must move the checkout to the locked commit, deepening the shallow clone \
         to reach it"
    );
    assert_eq!(
        exec_lua(&rpc, "return #_G.res.restored").await.as_u64(),
        Some(1)
    );
    assert_eq!(
        exec_lua(&rpc, "return #_G.res.failed").await.as_u64(),
        Some(0)
    );

    // Restoring again is a no-op: already at the locked commit, nothing re-checked-out.
    exec_lua(
        &rpc,
        "_G.res2 = nil\nnx.plugins.restore():next(function(r) _G.res2 = r end)",
    )
    .await;
    assert!(poll_true(&rpc, "return _G.res2 ~= nil").await);
    assert_eq!(
        exec_lua(&rpc, "return #_G.res2.restored").await.as_u64(),
        Some(0),
        "an already-restored plugin must not be re-checked-out"
    );
    assert_eq!(
        exec_lua(&rpc, "return #_G.res2.current").await.as_u64(),
        Some(1)
    );
}

#[tokio::test]
async fn restore_fails_loud_for_a_commit_that_no_longer_exists() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_restorebad_src");
    let repo = make_repo(&src, "alpha");
    let (root, cfg) = setup_root_and_config(&rpc, "plug_restorebad").await;
    std::fs::create_dir_all(&cfg).unwrap();

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\" }} }}\n\
             nx.plugins.install():next(function() _G.done = true end)\n\
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
            repo = q(&repo)
        ),
    )
    .await;
    assert!(poll_true(&rpc, "return _G.done == true").await);
    let before = head_sha(&root.join("alpha"));

    // A commit the remote does not have — even unshallowing cannot produce it.
    std::fs::write(
        cfg.join("nxvim-lock.json"),
        "{\n  \"alpha\": { \"commit\": \"0123456789abcdef0123456789abcdef01234567\" }\n}\n",
    )
    .unwrap();
    exec_lua(
        &rpc,
        "_G.res, _G.rerr = nil, nil\n\
         nx.plugins.restore():next(function(r) _G.res = r end)\n\
           :catch(function(e) _G.rerr = tostring(e and e.message or e) end)",
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.res ~= nil or _G.rerr ~= nil").await,
        "restore should settle"
    );
    // It must report the failure, not claim a successful rollback.
    assert_eq!(
        exec_lua(&rpc, "return _G.res and #_G.res.failed")
            .await
            .as_u64(),
        Some(1),
        "an unreachable commit must be reported as failed; rerr={:?}",
        exec_lua(&rpc, "return _G.rerr").await
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.res and #_G.res.restored")
            .await
            .as_u64(),
        Some(0),
        "nothing was actually restored, so nothing may be counted as restored"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.res.failed[1].name")
            .await
            .as_str(),
        Some("alpha")
    );
    // The checkout is untouched — a failed restore leaves the tree as it was.
    assert_eq!(head_sha(&root.join("alpha")), before);
}

#[tokio::test]
async fn status_reports_drift_against_the_lockfile() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_drift_src");
    let repo = make_repo(&src, "alpha");
    let first = head_sha(&repo);
    let tip = add_commit(&repo, "second");
    let (_root, cfg) = setup_root_and_config(&rpc, "plug_drift").await;
    std::fs::create_dir_all(&cfg).unwrap();

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\" }} }}\n\
             nx.plugins.install():next(function() _G.done = true end)\n\
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
            repo = q(&repo)
        ),
    )
    .await;
    assert!(poll_true(&rpc, "return _G.done == true").await);

    // Freshly installed and locked: the checkout matches the lock, so no drift.
    exec_lua(
        &rpc,
        "_G.rows = nil\nnx.plugins.status():next(function(r) _G.rows = r end)",
    )
    .await;
    assert!(poll_true(&rpc, "return _G.rows ~= nil").await);
    assert_eq!(
        exec_lua(&rpc, "return _G.rows[1].drifted").await.as_bool(),
        Some(false)
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.rows[1].locked").await.as_str(),
        Some(tip.as_str())
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.rows[1].head").await.as_str(),
        Some(tip.as_str())
    );

    // Point the lockfile at a different commit: the checkout now DRIFTS from it.
    std::fs::write(
        cfg.join("nxvim-lock.json"),
        format!("{{\n  \"alpha\": {{ \"commit\": \"{first}\" }}\n}}\n"),
    )
    .unwrap();
    exec_lua(
        &rpc,
        "_G.rows2 = nil\nnx.plugins.status():next(function(r) _G.rows2 = r end)",
    )
    .await;
    assert!(poll_true(&rpc, "return _G.rows2 ~= nil").await);
    assert_eq!(
        exec_lua(&rpc, "return _G.rows2[1].drifted").await.as_bool(),
        Some(true),
        "a checkout that differs from the lockfile must report drift"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.rows2[1].locked").await.as_str(),
        Some(first.as_str())
    );
}

#[tokio::test]
async fn restore_leaves_a_dev_dir_plugin_alone() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_restoredev_src");
    let devdir = make_repo(&src, "devplug");
    let (_root, cfg) = setup_root_and_config(&rpc, "plug_restoredev").await;
    std::fs::create_dir_all(&cfg).unwrap();
    // Even a hand-written entry for a dev plugin must not move the user's own checkout —
    // restore is for the manager's clones, never for a working tree it does not own.
    std::fs::write(
        cfg.join("nxvim-lock.json"),
        "{\n  \"devplug\": { \"commit\": \"0123456789abcdef0123456789abcdef01234567\" }\n}\n",
    )
    .unwrap();
    let before = head_sha(&devdir);

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ dir = \"{dev}\", name = \"devplug\" }} }}\n\
             _G.res = nil\n\
             nx.plugins.restore():next(function(r) _G.res = r end)\n\
               :catch(function(e) _G.rerr = tostring(e and e.message or e) end)",
            dev = q(&devdir)
        ),
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.res ~= nil").await,
        "rerr={:?}",
        exec_lua(&rpc, "return _G.rerr").await
    );
    assert_eq!(
        exec_lua(&rpc, "return #_G.res.restored").await.as_u64(),
        Some(0)
    );
    assert_eq!(
        exec_lua(&rpc, "return #_G.res.failed").await.as_u64(),
        Some(0),
        "a dev plugin is skipped, not reported as a failure"
    );
    assert_eq!(head_sha(&devdir), before, "the dev checkout must not move");
}

#[tokio::test]
async fn plugin_restore_command_is_wired() {
    let (rpc, _i) = start().await;
    assert_eq!(
        lua_bool(&rpc, "return nx.user_command.get().PluginRestore ~= nil").await,
        Some(true)
    );
}

#[tokio::test]
async fn the_dashboard_offers_restore_and_reaches_it() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_uirestore_src");
    let repo = make_repo(&src, "alpha");
    let first = head_sha(&repo);
    let tip = add_commit(&repo, "second");
    let (root, cfg) = setup_root_and_config(&rpc, "plug_uirestore").await;
    std::fs::create_dir_all(&cfg).unwrap();

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\" }} }}\n\
             nx.plugins.install():next(function() _G.done = true end)\n\
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
            repo = q(&repo)
        ),
    )
    .await;
    assert!(poll_true(&rpc, "return _G.done == true").await);
    assert_eq!(head_sha(&root.join("alpha")), tip);

    // Point the lock back at the older commit, then open the dashboard: it must advertise
    // the restore verb and show the plugin as drifted.
    std::fs::write(
        cfg.join("nxvim-lock.json"),
        format!("{{\n  \"alpha\": {{ \"branch\": \"main\", \"commit\": \"{first}\" }}\n}}\n"),
    )
    .unwrap();
    exec_lua(&rpc, "vim.cmd('Plugins')").await;
    assert!(
        poll_true(&rpc, "return vim.bo.filetype == 'nxplugins'").await,
        ":Plugins should open the dashboard"
    );
    // The hint line is real buffer text, so it is assertable here. The per-row `drifted`
    // flag is an end-of-line virt_text decoration (not buffer text) and there is no test
    // seam for the dashboard's decor list; the DATA behind it is covered directly by
    // `status_reports_drift_against_the_lockfile`.
    let mut saw_verb = false;
    for _ in 0..200 {
        if lines(&rpc).await.join("\n").contains("R restore") {
            saw_verb = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(saw_verb, "the dashboard should advertise the restore verb");
    // Drift really is what the dashboard's status pass sees for this plugin. (`status()` is
    // a promise, so the flag has to be parked on a global and polled — reading it in the
    // same chunk would always see the pre-resolution value.)
    exec_lua(
        &rpc,
        "_G.__drift = nil\n\
         nx.plugins.status():next(function(r)\n\
           for _, row in ipairs(r) do\n\
             if row.name == 'alpha' then _G.__drift = row.drifted end\n\
           end\n\
         end)",
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.__drift == true").await,
        "the plugin should read as drifted while the checkout differs from the lock"
    );

    // Pressing R restores — the same path `:PluginRestore` takes.
    feed(&rpc, "R");
    let mut back = false;
    for _ in 0..250 {
        if head_sha(&root.join("alpha")) == first {
            back = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(back, "R should restore the checkout to the locked commit");
}

/// A lockfile entry for a plugin no longer declared must not linger. `resolve_lock`
/// rebuilds the table from the DECLARED set, so the next lock write drops it — otherwise
/// a config that dropped a plugin would keep pinning it forever and `:PluginClean` would
/// leave a stale pin behind that a future re-add would silently resurrect.
#[tokio::test]
async fn a_lockfile_entry_for_an_undeclared_plugin_is_pruned() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_prune_src");
    let repo = make_repo(&src, "alpha");
    let (_root, cfg) = setup_root_and_config(&rpc, "plug_prune").await;
    std::fs::create_dir_all(&cfg).unwrap();
    // A lockfile carrying an entry for a plugin the config does not declare.
    std::fs::write(
        cfg.join("nxvim-lock.json"),
        "{\n  \"ghost\": { \"commit\": \"0123456789abcdef0123456789abcdef01234567\" }\n}\n",
    )
    .unwrap();

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\" }} }}\n\
             nx.plugins.install():next(function() _G.done = true end)\n\
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
            repo = q(&repo)
        ),
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );

    assert_eq!(
        lua_bool(
            &rpc,
            "return nx.plugins.locked().alpha ~= nil and nx.plugins.locked().ghost == nil"
        )
        .await,
        Some(true),
        "the undeclared plugin's entry should be gone from the lock"
    );
    let text = std::fs::read_to_string(cfg.join("nxvim-lock.json")).unwrap();
    assert!(
        !text.contains("ghost"),
        "the stale entry should be gone from the FILE too: {text}"
    );
}

// ----- the lockfile: invalidation, carry-over, and honest reporting -----------
//
// A lockfile is a RECORD of a resolution, so (like `Cargo.lock` against its manifest) it
// stops applying the moment the declaration that produced it changes, it never quietly
// loses an entry it was not asked to drop, and the verbs never claim more or less than
// they did. Each test below pins one of those.

/// Tag a repo's current commit. Test plumbing only.
fn tag_head(repo: &Path, name: &str) {
    git(repo, &["tag", name]);
}

/// A `tag = "v2"` bump in the spec must beat a lock entry recorded under `v1`: the
/// lockfile records what the OLD declaration resolved to, and a stale record must never
/// outrank the config the user just edited. Without invalidation the install silently
/// lands on v1 — the config says one thing and the editor runs another, with no error and
/// no drift flag (head and lock agree), and no verb able to move it.
#[tokio::test]
async fn a_tag_bump_invalidates_the_lock_entry() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_tagbump_src");
    let repo = make_repo(&src, "alpha");
    let v1 = head_sha(&repo);
    tag_head(&repo, "v1");
    let v2 = add_commit(&repo, "second");
    tag_head(&repo, "v2");

    let base = temp_dir("plug_tagbump");
    let cfg = base.join("config");
    let root = base.join("install");
    std::fs::create_dir_all(&cfg).unwrap();
    // A lockfile recorded when the spec still said `tag = "v1"`.
    std::fs::write(
        cfg.join("nxvim-lock.json"),
        format!("{{\n  \"alpha\": {{ \"commit\": \"{v1}\", \"tag\": \"v1\" }}\n}}\n"),
    )
    .unwrap();

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.setup_manager({{ root = \"{root}\", config = \"{cfg}\" }})\n\
             nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\", tag = \"v2\" }} }}\n\
             nx.plugins.sync():next(function() _G.done = true end)\n\
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
            root = q(&root),
            cfg = q(&cfg),
            repo = q(&repo)
        ),
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "sync should succeed; err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    assert_eq!(
        head_sha(&root.join("alpha")),
        v2,
        "the spec's tag must win over a lock entry recorded under the old tag ({v1})"
    );
    // And the lock is re-recorded under the tag that actually produced this checkout, so
    // the next sync is a no-op instead of fighting the spec again.
    assert!(
        poll_true(
            &rpc,
            &format!(
                "local e = nx.plugins.locked().alpha\n\
                 return e ~= nil and e.commit == \"{v2}\" and e.tag == \"v2\""
            )
        )
        .await,
        "the lock should now record v2: {:?}",
        exec_lua(&rpc, "return nx.json.encode(nx.plugins.locked())").await
    );
}

/// The same bump on an ALREADY-INSTALLED plugin: a pin is an instruction, so realizing the
/// declared state moves the checkout onto it. (`:PluginUpdate` still never advances a pin
/// past what it names — that is what pinning means.)
#[tokio::test]
async fn a_tag_bump_moves_an_existing_install() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_tagmove_src");
    let repo = make_repo(&src, "alpha");
    let v1 = head_sha(&repo);
    tag_head(&repo, "v1");
    let v2 = add_commit(&repo, "second");
    tag_head(&repo, "v2");

    let (root, cfg) = setup_root_and_config(&rpc, "plug_tagmove").await;
    let declare = |tag: &str| {
        format!(
            "_G.done, _G.err = nil, nil\n\
             nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\", tag = \"{tag}\" }} }}\n\
             nx.plugins.sync():next(function() _G.done = true end)\n\
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
            repo = q(&repo)
        )
    };
    exec_lua(&rpc, &declare("v1")).await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    assert_eq!(head_sha(&root.join("alpha")), v1);

    // The user edits the spec: tag = "v2". A sync must realize it.
    exec_lua(&rpc, &declare("v2")).await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    assert_eq!(
        head_sha(&root.join("alpha")),
        v2,
        "a sync must move an existing install onto the tag the spec now names"
    );
    let text = std::fs::read_to_string(cfg.join("nxvim-lock.json")).unwrap();
    assert!(text.contains(&v2), "the lock should follow: {text}");
}

/// A `branch` change invalidates the entry for the same reason a tag bump does.
#[tokio::test]
async fn a_branch_change_invalidates_the_lock_entry() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_brchange_src");
    let repo = make_repo(&src, "alpha");
    let on_main = head_sha(&repo);
    git(&repo, &["checkout", "-q", "-b", "side"]);
    let on_side = add_commit(&repo, "side-only");
    git(&repo, &["checkout", "-q", "main"]);

    let base = temp_dir("plug_brchange");
    let cfg = base.join("config");
    let root = base.join("install");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("nxvim-lock.json"),
        format!("{{\n  \"alpha\": {{ \"branch\": \"main\", \"commit\": \"{on_main}\" }}\n}}\n"),
    )
    .unwrap();

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.setup_manager({{ root = \"{root}\", config = \"{cfg}\" }})\n\
             nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\", branch = \"side\" }} }}\n\
             nx.plugins.sync():next(function() _G.done = true end)\n\
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
            root = q(&root),
            cfg = q(&cfg),
            repo = q(&repo)
        ),
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "sync should succeed; err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    assert_eq!(
        head_sha(&root.join("alpha")),
        on_side,
        "the spec's branch must win over an entry recorded on the old branch"
    );
}

/// A plugin that is DECLARED but not installed on this machine — `enabled = false`, a
/// platform-conditional `enabled` predicate, a clone not yet synced — must keep its
/// recorded pin. Rebuilding the lock from only what is installed silently strips those
/// entries, and since the lockfile is the file the user COMMITS, the stripped copy is what
/// travels back to the machine that did need the pin.
#[tokio::test]
async fn a_disabled_plugins_lock_entry_survives_a_sync() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_keepdisabled_src");
    let alpha = make_repo(&src, "alpha");
    let beta = make_repo(&src, "beta");
    let beta_sha = head_sha(&beta);

    let base = temp_dir("plug_keepdisabled");
    let cfg = base.join("config");
    let root = base.join("install");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("nxvim-lock.json"),
        format!("{{\n  \"beta\": {{ \"branch\": \"main\", \"commit\": \"{beta_sha}\" }}\n}}\n"),
    )
    .unwrap();

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.setup_manager({{ root = \"{root}\", config = \"{cfg}\" }})\n\
             nx.plugins {{\n\
               {{ \"file://{alpha}\", name = \"alpha\" }},\n\
               {{ \"file://{beta}\", name = \"beta\", enabled = false }},\n\
             }}\n\
             nx.plugins.sync():next(function() _G.done = true end)\n\
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
            root = q(&root),
            cfg = q(&cfg),
            alpha = q(&alpha),
            beta = q(&beta)
        ),
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "sync should succeed; err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    let text = std::fs::read_to_string(cfg.join("nxvim-lock.json")).unwrap();
    assert!(
        text.contains(&beta_sha),
        "the disabled plugin's pin must survive: {text}"
    );
    assert!(
        text.contains("alpha"),
        "and the installed one must be recorded: {text}"
    );
}

/// A local dev `dir` override is a property of THIS machine; it must not delete the entry
/// the shared lockfile carries for that plugin. (Its own SHA is still never recorded — a
/// working checkout is not a reproducible artifact.)
#[tokio::test]
async fn a_dev_dir_override_keeps_the_recorded_entry() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_devkeep_src");
    let repo = make_repo(&src, "alpha");
    let recorded = "0123456789abcdef0123456789abcdef01234567";

    let (_root, cfg) = setup_root_and_config(&rpc, "plug_devkeep").await;
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("nxvim-lock.json"),
        format!("{{\n  \"alpha\": {{ \"branch\": \"main\", \"commit\": \"{recorded}\" }}\n}}\n"),
    )
    .unwrap();

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ \"file://x/alpha\", name = \"alpha\", dir = \"{dir}\" }} }}\n\
             nx.plugins.lock():next(function() _G.done = true end)\n\
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
            dir = q(&repo)
        ),
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "lock should succeed; err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    let text = std::fs::read_to_string(cfg.join("nxvim-lock.json")).unwrap();
    assert!(
        text.contains(recorded),
        "a dev override must not erase the shared entry: {text}"
    );
}

/// A session that declares no plugins at all must never overwrite a populated lockfile
/// with an empty one — the declared set is the only thing that says which entries are
/// still wanted, and an empty set says nothing.
#[tokio::test]
async fn a_session_with_no_declared_plugins_leaves_the_lockfile_alone() {
    let (rpc, _i) = start().await;
    let (_root, cfg) = setup_root_and_config(&rpc, "plug_nodecl").await;
    std::fs::create_dir_all(&cfg).unwrap();
    let before =
        "{\n  \"ghost\": { \"commit\": \"0123456789abcdef0123456789abcdef01234567\" }\n}\n";
    std::fs::write(cfg.join("nxvim-lock.json"), before).unwrap();

    exec_lua(
        &rpc,
        "nx.plugins.lock():next(function() _G.done = true end)\n\
           :catch(function(e) _G.err = tostring(e and e.message or e) end)",
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "lock should resolve; err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    assert_eq!(
        std::fs::read_to_string(cfg.join("nxvim-lock.json")).unwrap(),
        before,
        "an empty declared set must leave the file untouched"
    );
}

/// `:PluginUpdate` must count the plugin it moved. Advancing past the lock happens in the
/// RE-ATTACH (which puts the worktree on the branch tip), so the pull that follows is
/// often a no-op — trusting it reports "updated 0 plugin(s)" for a run that moved the
/// plugin and rewrote the lockfile.
#[tokio::test]
async fn update_counts_the_plugin_it_advanced_past_the_lock() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_count_src");
    let repo = make_repo(&src, "alpha");
    let first = head_sha(&repo);
    let tip = add_commit(&repo, "second");

    let base = temp_dir("plug_count");
    let cfg = base.join("config");
    let root = base.join("install");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("nxvim-lock.json"),
        format!("{{\n  \"alpha\": {{ \"branch\": \"main\", \"commit\": \"{first}\" }}\n}}\n"),
    )
    .unwrap();

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins.setup_manager({{ root = \"{root}\", config = \"{cfg}\" }})\n\
             nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\" }} }}\n\
             nx.plugins.install():next(function()\n\
               nx.plugins.update():next(function(n) _G.upd = n end)\n\
                 :catch(function(e) _G.uerr = tostring(e and e.message or e) end)\n\
             end):catch(function(e) _G.err = tostring(e and e.message or e) end)",
            root = q(&root),
            cfg = q(&cfg),
            repo = q(&repo)
        ),
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.upd ~= nil or _G.uerr ~= nil").await,
        "update should settle; err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    assert_eq!(
        head_sha(&root.join("alpha")),
        tip,
        "update should have advanced past the lock"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.upd").await.as_u64(),
        Some(1),
        "the count must include the plugin it actually moved (uerr={:?})",
        exec_lua(&rpc, "return _G.uerr").await
    );
}

/// `:PluginSync` REPRODUCES the lockfile, which has to mean an existing checkout too —
/// pulling a teammate's updated `nxvim-lock.json` and syncing must land you on the commits
/// it names, not leave you drifted with the file only honoured for clones that happen to
/// be missing.
#[tokio::test]
async fn sync_moves_a_drifted_checkout_back_onto_the_lock() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_converge_src");
    let repo = make_repo(&src, "alpha");
    let first = head_sha(&repo);
    let tip = add_commit(&repo, "second");

    let (root, cfg) = setup_root_and_config(&rpc, "plug_converge").await;
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ \"file://{repo}\", name = \"alpha\" }} }}\n\
             nx.plugins.sync():next(function() _G.done = true end)\n\
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
            repo = q(&repo)
        ),
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    assert_eq!(head_sha(&root.join("alpha")), tip);

    // A colleague's lockfile arrives naming the earlier commit.
    std::fs::write(
        cfg.join("nxvim-lock.json"),
        format!("{{\n  \"alpha\": {{ \"branch\": \"main\", \"commit\": \"{first}\" }}\n}}\n"),
    )
    .unwrap();
    exec_lua(
        &rpc,
        "_G.done, _G.err = nil, nil\n\
         nx.plugins.sync():next(function() _G.done = true end)\n\
           :catch(function(e) _G.err = tostring(e and e.message or e) end)",
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    assert_eq!(
        head_sha(&root.join("alpha")),
        first,
        "sync must realize the lockfile it was handed, not leave the checkout drifted"
    );
}

/// A lockfile that ends up with no entries is still a JSON OBJECT. Lua cannot tell an
/// empty map from an empty list, and `nx.json` picks the list form — writing `[]` into a
/// file documented (and re-read) as a name-keyed object.
#[tokio::test]
async fn an_emptied_lockfile_is_written_as_a_json_object() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_emptylock_src");
    let repo = make_repo(&src, "alpha");
    let (_root, cfg) = setup_root_and_config(&rpc, "plug_emptylock").await;
    std::fs::create_dir_all(&cfg).unwrap();
    // One entry, for a plugin the config does not declare — so the next write resolves to
    // nothing at all.
    std::fs::write(
        cfg.join("nxvim-lock.json"),
        "{\n  \"ghost\": { \"commit\": \"0123456789abcdef0123456789abcdef01234567\" }\n}\n",
    )
    .unwrap();

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ \"file://x/alpha\", name = \"alpha\", dir = \"{dir}\" }} }}\n\
             nx.plugins.lock():next(function() _G.done = true end)\n\
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
            dir = q(&repo)
        ),
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );
    let text = std::fs::read_to_string(cfg.join("nxvim-lock.json")).unwrap();
    assert_eq!(
        text.trim(),
        "{}",
        "an empty lockfile must be `{{}}`: {text}"
    );
}

/// One plugin failing must not lose the record of the ones that already moved: the verb
/// aborts loud, but a lockfile that no longer describes the tree is exactly the silent
/// disagreement the file exists to prevent.
#[tokio::test]
async fn a_failed_update_still_records_the_plugins_that_moved() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_partial_src");
    let alpha = make_repo(&src, "alpha");
    let beta = make_repo(&src, "beta");

    let (root, cfg) = setup_root_and_config(&rpc, "plug_partial").await;
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{\n\
               {{ \"file://{alpha}\", name = \"alpha\" }},\n\
               {{ \"file://{beta}\", name = \"beta\" }},\n\
             }}\n\
             nx.plugins.install():next(function() _G.done = true end)\n\
               :catch(function(e) _G.err = tostring(e and e.message or e) end)",
            alpha = q(&alpha),
            beta = q(&beta)
        ),
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "err={:?}",
        exec_lua(&rpc, "return _G.err").await
    );

    // alpha gains a commit to fast-forward onto; beta's remote disappears, so its pull
    // fails and aborts the run after alpha has already moved.
    let moved = add_commit(&alpha, "second");
    std::fs::remove_dir_all(&beta).unwrap();

    exec_lua(
        &rpc,
        "_G.upd, _G.uerr = nil, nil\n\
         nx.plugins.update():next(function(n) _G.upd = n end)\n\
           :catch(function(e) _G.uerr = tostring(e and e.message or e) end)",
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.uerr ~= nil or _G.upd ~= nil").await,
        "update should settle"
    );
    assert!(
        exec_lua(&rpc, "return _G.uerr").await.as_str().is_some(),
        "beta's failure must be reported loud, not swallowed"
    );
    assert_eq!(head_sha(&root.join("alpha")), moved);
    let text = std::fs::read_to_string(cfg.join("nxvim-lock.json")).unwrap();
    assert!(
        text.contains(&moved),
        "alpha moved, so the lockfile must say so even though beta failed: {text}"
    );
}

/// An entry that is present but carries no usable commit is a corrupt lockfile, not
/// "nothing pinned for that plugin": dropping it silently reinstalls at the remote tip,
/// which is precisely the un-reproducible install the file exists to prevent.
#[tokio::test]
async fn a_lock_entry_with_no_commit_fails_loud() {
    let (rpc, _i) = start().await;
    let (_root, cfg) = setup_root_and_config(&rpc, "plug_badentry").await;
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("nxvim-lock.json"),
        "{\n  \"alpha\": { \"branch\": \"main\" }\n}\n",
    )
    .unwrap();

    exec_lua(
        &rpc,
        "_G.err = nil\n\
         nx.plugins._read_lock():next(function() _G.ok = true end)\n\
           :catch(function(e) _G.err = tostring(e and e.message or e) end)",
    )
    .await;
    assert!(poll_true(&rpc, "return _G.err ~= nil or _G.ok ~= nil").await);
    let msg = exec_lua(&rpc, "return _G.err").await;
    let msg = msg.as_str().unwrap_or("");
    assert!(
        msg.contains("alpha") && msg.contains("nxvim-lock.json"),
        "must name the bad entry and the file: {msg}"
    );
}

// Expanding a plugin row (`<CR>`) shows the spec's own `desc` — the human line the
// config author wrote — alongside the url / dir / trigger details.
#[tokio::test]
async fn the_dashboard_shows_a_plugins_desc_when_expanded() {
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_uidesc_src");
    let repo = make_repo(&src, "vista");
    setup_root(&rpc, "plug_uidesc").await;

    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ name = \"vista\", dir = \"{dir}\", \
               desc = \"Panoramic tag viewer\" }} }}",
            dir = q(&repo)
        ),
    )
    .await;
    exec_lua(&rpc, "vim.cmd('Plugins')").await;
    assert!(
        poll_true(&rpc, "return vim.bo.filetype == 'nxplugins'").await,
        ":Plugins should open the manager dashboard"
    );
    // Wait for the row, then put the cursor on it and expand.
    let mut listed = false;
    for _ in 0..200 {
        if lines(&rpc).await.join("\n").contains("vista") {
            listed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(listed, "the dashboard should list the plugin");
    feed(&rpc, "/vista<CR>");
    feed(&rpc, "<CR>");

    let mut saw_desc = false;
    for _ in 0..200 {
        if lines(&rpc)
            .await
            .join("\n")
            .contains("desc: Panoramic tag viewer")
        {
            saw_desc = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        saw_desc,
        "the expanded row should show the spec's own description"
    );
}

// The dashboard is a screen-centered dialog: opening `:Plugins` from a focused dock
// still frames it in the middle of the whole editor (it rides the `screen` region),
// rather than being boxed into the region that happened to have focus.
#[tokio::test]
async fn the_dashboard_is_centered_on_the_whole_screen() {
    let (rpc, mut incoming) = start().await; // 80 x 24
    setup_root(&rpc, "plug_uicenter").await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    exec_lua(&rpc, "vim.cmd('Plugins')").await;
    assert!(
        poll_true(&rpc, "return vim.bo.filetype == 'nxplugins'").await,
        ":Plugins should open the manager dashboard"
    );

    // The dashboard float is 80% x 80% of the editor, centered: 64 inner columns plus
    // one border cell per side = 66, leaving 7 columns on each side of the 80.
    let mut geom = None;
    for _ in 0..100 {
        barrier(&rpc).await;
        if let Some(map) = drain_to_latest_redraw(&mut incoming, |m| float_rect(m).is_some()) {
            geom = float_rect(&map);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (region, x, width) = geom.expect("the dashboard float in a redraw");
    assert_eq!(
        region, "screen",
        "the dashboard is placed against the screen"
    );
    assert_eq!(width, 66, "80% of 80 columns, plus the border");
    assert_eq!(x, 7, "centered on the whole screen, not on the main region");
}

/// The floating window in a redraw map as `(region, x, width)`.
fn float_rect(map: &[(Value, Value)]) -> Option<(String, u64, u64)> {
    let Some(Value::Array(wins)) = map_get(map, "windows") else {
        return None;
    };
    wins.iter().find_map(|w| {
        let Value::Map(m) = w else { return None };
        if map_get(m, "floating").and_then(Value::as_bool) != Some(true) {
            return None;
        }
        let region = map_get(m, "region")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let Some(Value::Map(r)) = map_get(m, "rect") else {
            return None;
        };
        let n = |k: &str| map_get(r, k).and_then(Value::as_u64).unwrap_or(0);
        Some((region, n("x"), n("width")))
    })
}

// ----- ft-lazy plugin with an ASYNC config -----------------------------------

#[tokio::test]
async fn ft_lazy_plugin_with_an_async_config_still_gets_the_filetype_event() {
    // The end-to-end defect this whole async-event-model work exists to fix
    // (docs/plans/2026-07-26-async-event-model.md, D2 + D3).
    //
    // An `ft = "python"`-lazy plugin is woken by the FileType event, and its `config`
    // is ASYNC — so by the time it registers its own `FileType python` handler, that
    // event has long since finished dispatching. Before the settle protocol the
    // plugin's handler simply never ran for the buffer that woke it: the plugin
    // "loaded" and did nothing, on every single open.
    //
    // lazy.nvim gets away with a blanket re-fire because it is synchronous; we cannot
    // re-fire on `load()` returning, because `load()` returns a promise. The watermark
    // replay is what delivers the event once the async config settles.
    let (rpc, _i) = start().await;
    let src = temp_dir("plug_ftlazy_async");
    let repo = make_repo(&src, "zeta");
    setup_root(&rpc, "plug_ftlazy_async").await;
    let marker = repo.join("lua").join("zeta").join("init.lua");

    exec_lua(
        &rpc,
        &format!(
            "_G.zeta_ft = nil\n\
             nx.plugins {{ {{ name = \"zeta\", dir = \"{dir}\", ft = \"python\", config = function()\n\
               -- async: the handler below is registered a tick or more after the\n\
               -- FileType fire that triggered this load.\n\
               local txt = nx.await(nx.fs.read_text(\"{f}\"))\n\
               _G.zeta_loaded = #txt > 0\n\
               nx.autocmd.create(\"FileType\", {{ pattern = \"python\", callback = function(a)\n\
                 _G.zeta_ft = a.file\n\
               end }})\n\
             end }} }}",
            dir = q(&repo),
            f = q(&marker)
        ),
    )
    .await;

    // Opening a .py file fires FileType, which wakes the lazy plugin.
    let py = src.join("sample.py");
    std::fs::write(&py, "x = 1\n").unwrap();
    exec_lua(&rpc, &format!("nx.cmd('edit {}')", q(&py))).await;

    assert!(
        poll_true(&rpc, "return _G.zeta_loaded == true").await,
        "the ft trigger loaded the plugin and its async config ran to completion"
    );
    assert!(
        poll_true(&rpc, "return _G.zeta_ft ~= nil").await,
        "the plugin's OWN FileType handler ran for the buffer that woke it — \
         without the replay it would never fire, which is the whole defect"
    );
    let seen = exec_lua(&rpc, "return _G.zeta_ft").await;
    assert!(
        seen.as_str().unwrap_or_default().ends_with("sample.py"),
        "and it saw the triggering buffer, got {seen:?}"
    );
}
