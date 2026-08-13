//! `bemtvi` entry point — the single binary, in one of three roles:
//!
//! - **default** (`bemtvi [file]`): an *embedded* server (a headless editor on its
//!   own thread) plus the terminal UI client on the main thread, over an
//!   in-process duplex stream.
//! - **`--daemon`** (`bemtvi --daemon`): the *edit-host split*'s remote half — just
//!   the fs + process + watch + LSP host, no editor and no UI, multiplexing every
//!   leg of the daemon wire over **stdin/stdout**. The editor (core + Lua) stays
//!   *local* for a zero-round-trip keystroke path; only I/O runs here. This is what
//!   the local edit-host spawns over SSH (`ssh host bemtvi --daemon`).
//! - **`--connect-daemon`** (`bemtvi --connect-daemon [file]`): the *edit-host split*'s
//!   **local** half — the full editor + terminal UI (exactly the default role), but
//!   with its fs/process/watch/LSP host seams pointed at a `--daemon` child instead of
//!   the local disk. It spawns that daemon (this same binary in `--daemon` mode by
//!   default, or whatever `BEMTVI_DAEMON_CMD` names — e.g. `ssh host bemtvi --daemon`),
//!   wraps the child's stdio in [`bemtvi_server::connect_daemon`], and injects the
//!   resulting seams into [`ServerInit`]. By default the session runs the **local**
//!   config (only I/O crosses the wire); `--remote-config` instead **fetches** the
//!   daemon's config + plugins (`config_bundle`, materialized onto a local cache), so the
//!   session runs the *remote's* config. (A later phase ties shada to the same choice;
//!   for now shada stays local in both.) Either way the keystroke path stays local.
//!
//! The default and `--connect-daemon` roles both run [`bemtvi_server::run`] — a full
//! local editor over an in-process duplex; `--connect-daemon` only differs by pointing
//! the host seams at a daemon. `--daemon` is the inverse: it runs
//! [`bemtvi_server::run_daemon_io`] (no editor), the remote half of the boundary moved
//! *below* the editor.
//!
//! There is deliberately **no** "whole editor runs remote, thin client local" role:
//! to edit on another machine, open an SSH session and run `bemtvi` there, or use the
//! edit-host split (`--connect-daemon`) to keep the keystroke path local.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use clap::Parser;

mod test_runner;
use bemtvi_server::{
    bind_quic_listener, connect_daemon_respawning, connect_quic_reconnecting, daemon_log_stderr,
    env_daemon_command, mint_token, parse_connect_uri, run as run_server,
    run_daemon_io as run_server_daemon_io, serve_quic, ConfigSource, DaemonClient, ReconnectHandle,
    ReconnectPolicy, ReconnectSpec, ReconnectTransport, ServerInit, CONNECT_URI_SCHEME,
    DAEMON_TOKEN_ENV,
};

/// The shada-namespace + workspace options derived from the command line. The namespace
/// comes from `--shada-namespace` or is derived from a `--workspace` directory; the
/// identity (namespace + root) is surfaced to Lua (`btv.shada.namespace()` / `btv.workspace`)
/// by seeding the runtime, *not* an env var — a daemon session derives both from the
/// daemon's cwd after it connects (which the binary can't know up front). Plain `Send` data
/// moved onto the server thread.
#[derive(Clone, Default)]
struct ShadaOpts {
    namespace: Option<String>,
    restore: bool,
    /// True when launched as a `--workspace` directory session. It forces session capture
    /// (no plugin opt-in needed) and restore, and exposes the root via `btv.workspace`. The
    /// shada namespace itself is *not* special: it is the directory-derived value passed
    /// straight through the `--shada-namespace` machinery (an explicit one overrides it).
    workspace: bool,
    /// The absolute workspace root for a local `--workspace` launch. `None` for a non-
    /// workspace launch *and* for a `--workspace` daemon launch, whose root is the daemon's
    /// cwd — resolved post-connect by [`ShadaOpts::resolve_remote_workspace`].
    workspace_dir: Option<String>,
}

impl ShadaOpts {
    /// Build from the parsed CLI, validating the explicit namespace token. `workspace_dir`
    /// is `Some` only for a *local* `--workspace` launch (the resolved absolute directory);
    /// a daemon `--workspace` passes `None` here and resolves its root + namespace later
    /// from the daemon's cwd ([`resolve_remote_workspace`]). An explicit `--shada-namespace`
    /// always wins over the derived one. An invalid explicit namespace aborts.
    ///
    /// [`resolve_remote_workspace`]: Self::resolve_remote_workspace
    fn from_cli(cli: &Cli, workspace_dir: Option<&std::path::Path>) -> Result<Self> {
        // An explicit `--shada-namespace` always wins; otherwise a *local* `--workspace`
        // derives one from the directory (the same `ns/<token>` store, no special-casing).
        let namespace =
            match cli.shada_namespace.as_deref() {
                Some(raw) => Some(bemtvi_server::valid_namespace(raw).ok_or_else(|| {
                    anyhow!("invalid --shada-namespace {raw:?} (use [A-Za-z0-9_-])")
                })?),
                None => workspace_dir.map(bemtvi_server::workspace_namespace),
            };
        Ok(ShadaOpts {
            namespace,
            // `--workspace` implies restore; otherwise honor the explicit flag.
            restore: cli.restore_session || cli.workspace.is_some(),
            workspace: cli.workspace.is_some(),
            workspace_dir: workspace_dir.map(|d| d.to_string_lossy().into_owned()),
        })
    }

    /// Resolve a `--workspace` daemon session's identity from the daemon's `remote_cwd`
    /// (the workspace lives on the remote machine, so its namespace must derive from the
    /// remote path). A no-op unless this is a workspace launch; an explicit
    /// `--shada-namespace` is preserved, only the root is filled. Called inside the edit-
    /// host session once the daemon's cwd is known.
    fn resolve_remote_workspace(&mut self, remote_cwd: Option<&std::path::Path>) {
        if !self.workspace {
            return;
        }
        let Some(cwd) = remote_cwd else { return };
        if self.namespace.is_none() {
            self.namespace = Some(bemtvi_server::workspace_namespace(cwd));
        }
        if self.workspace_dir.is_none() {
            self.workspace_dir = Some(cwd.to_string_lossy().into_owned());
        }
    }

    fn store(&self) -> Box<dyn bemtvi_server::ShadaStore + Send> {
        bemtvi_server::workspace_shada(self.namespace.as_deref())
    }

    /// The **global** history store for a workspace launch (`'persisthistory'` may route
    /// history here in addition to the namespaced store). `Some` only when a namespace is
    /// set — a plain launch's primary store IS the global one, so it needs no second
    /// handle. The server gates actual use on `'persisthistory'` including `global`.
    fn global_history_store(&self) -> Option<Box<dyn bemtvi_server::ShadaStore + Send>> {
        self.namespace
            .as_ref()
            .map(|_| bemtvi_server::default_shada())
    }

    /// The shada namespace, so a `Remote`-config session isolates its on-daemon shada under
    /// `ns/<NS>/` the same way the local store does.
    fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    /// A namespaced launch captures the session; the global store never does.
    fn capture(&self) -> bool {
        self.namespace.is_some()
    }

    /// Restore the layout only when explicitly asked AND a namespace is present.
    fn do_restore(&self) -> bool {
        self.namespace.is_some() && self.restore
    }

    /// Seed the layout-capture opt-in (`btv.shada.save_layout`) on at boot. Only a
    /// `--workspace` launch does this — a plain `--shada-namespace` leaves capture to a
    /// plugin / the config, preserving the existing behavior.
    fn session_save_layout(&self) -> bool {
        self.workspace
    }

    /// The (owned) shada namespace + workspace root surfaced to Lua via the runtime seed.
    fn workspace_identity(&self) -> (Option<String>, Option<String>) {
        (self.namespace.clone(), self.workspace_dir.clone())
    }
}

/// Resolve the `--workspace DIR` value (the cwd for bare `--workspace`, via clap's
/// `default_missing_value`), canonicalized to an absolute path. It MUST be an existing
/// directory — a `--workspace` session is a directory session (`bemtvi --workspace .`);
/// pointing it at a file or a missing path is a loud error, not a silent fall-through. Used
/// for a *local* session; a daemon session resolves its root from the daemon's cwd instead.
fn resolve_workspace_dir(dir: Option<&str>) -> Result<PathBuf> {
    let raw = dir.unwrap_or(".");
    let abs = std::fs::canonicalize(raw).map_err(|e| anyhow!("--workspace {raw:?}: {e}"))?;
    if !abs.is_dir() {
        bail!("--workspace requires a directory, but {raw:?} is not one");
    }
    Ok(abs)
}

/// Env var carrying this process's positional file arguments (newline-joined), read back
/// by `btv.argv()`. Set once in `main` so it is available in every role.
const ARGV_ENV: &str = "BEMTVI_ARGV";

/// Quote `s` as a Lua string literal (double-quoted, with the escapes Lua needs), so
/// arbitrary user code/paths can be embedded in a generated chunk and re-compiled
/// Lua-side via `load` — the only way to *observe* a load error, since the server's
/// `nvim_exec_lua` reports a failed chunk as an `:echo` + `Nil` reply, not an error.
pub(crate) fn lua_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The bootstrap chunk `--lua` runs CODE through. It compiles CODE Lua-side — as an
/// expression first (`return (CODE)`, the documented shape), falling back to a chunk
/// *body* so statement-form CODE (`return x`, multi-statement chunks) works — and
/// **returns the compile error as a string** when both fail (`nvim_exec_lua` cannot
/// carry it as an RPC error; see [`lua_quote`]). On success it starts the evaluation
/// inside `btv.async` (so a top-level `btv.await` works), awaits a returned promise, and
/// records the outcome in the two globals the completion poll reads —
/// `__bemtvi_oneshot_done` (settled) and `__bemtvi_oneshot_err` (the stringified
/// rejection, `nil` on success) — then returns `true`.
fn oneshot_bootstrap(code: &str) -> String {
    let src = lua_quote(code);
    format!(
        "_G.__bemtvi_oneshot_done = false\n\
         _G.__bemtvi_oneshot_err = nil\n\
         local __src = {src}\n\
         local __f = load('return (' .. __src .. ')', '=--lua')\n\
         if not __f then\n\
         local __e\n\
         __f, __e = load(__src, '=--lua')\n\
         if not __f then return tostring(__e) end\n\
         end\n\
         btv.async(function()\n\
         local __r = __f()\n\
         if type(__r) == 'table' and type(__r.next) == 'function' then __r = btv.await(__r) end\n\
         return __r\n\
         end)():next(\n\
         function() _G.__bemtvi_oneshot_done = true end,\n\
         function(e)\n\
         _G.__bemtvi_oneshot_err = tostring(e)\n\
         btv.notify('bemtvi --lua: ' .. tostring(e), 4)\n\
         _G.__bemtvi_oneshot_done = true\n\
         end)\n\
         return true\n"
    )
}

/// `nvim_exec_lua(code)`, surfacing a dead transport as an `Err`. (A *Lua* failure
/// never arrives here as an error — the server echoes it and replies `Nil` — which is
/// why the generated chunks report their own outcome as a value.)
async fn oneshot_exec(rpc: &bemtvi_rpc::Rpc, code: &str) -> Result<rmpv::Value> {
    rpc.request(
        "nvim_exec_lua",
        vec![rmpv::Value::from(code), rmpv::Value::Array(vec![])],
    )
    .await
    .map_err(|e| anyhow!("{e}"))
}

/// `--lua CODE` headless mode: boot an embedded server with the user's config +
/// runtimepath (so plugins load), evaluate CODE once, then exit — no UI. CODE is a Lua
/// EXPRESSION (statement-form CODE like `return x` is also accepted, compiled as a
/// chunk body); if it yields a promise the wrapper waits for it to settle. The exit
/// status reports the outcome for shells/CI: `0` when CODE settles cleanly, non-zero
/// (with the error on stderr) when it fails to load, throws/rejects, or never settles
/// within the deadline — never a silent success. A workspace wrapper uses this to read
/// its config and relaunch with `--shada-namespace` via `btv.reexec`, which replaces
/// this process (so the wrapper's `:qa!` never runs). `workspace_cwd` mirrors the
/// interactive launch: a `--workspace DIR` one-shot runs CODE with DIR as the cwd
/// (unless `--workspace-no-cwd`).
fn run_lua_oneshot(code: String, shada: &ShadaOpts, workspace_cwd: bool) -> Result<()> {
    use std::time::Duration;

    // Mark this as a oneshot bootstrap so a workspace plugin skips its interactive
    // auto-evaluation (it should only relaunch, not prompt). `os.getenv("BEMTVI_LUA_ONESHOT")`
    // is the Lua signal. (`BEMTVI_ARGV` for `btv.argv()` is already set by `main`.)
    std::env::set_var("BEMTVI_LUA_ONESHOT", "1");

    // Surface any explicit `--shada-namespace` to `btv.shada.namespace()` so a wrapper that
    // inspects it (before relaunching) reads the value it was launched with.
    let (shada_namespace, workspace_dir) = shada.workspace_identity();

    let (config_dir, runtimepath) = bemtvi_server::default_runtime();
    let (server_end, client_end) = tokio::io::duplex(1 << 16);
    std::thread::spawn(move || {
        let init = ServerInit {
            file: None,
            config_dir,
            // A oneshot never persists and offers no first-run UI.
            shada: None,
            shada_namespace,
            workspace_dir,
            // A `--workspace DIR` one-shot cds into DIR like the interactive launch, so
            // CODE's relative paths resolve against the workspace root.
            workspace_cwd,
            runtimepath,
            clipboard: bemtvi_server::ClipboardProvider::Disabled,
            offer_default_recommended: false,
            cmdline_complete_default: false,
            // Headless: no UI is attached, so route `print` / echo / error output to the
            // real stdout/stderr instead of an invisible message line.
            lua_stdio: true,
            ..Default::default()
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("lua-oneshot server runtime");
        let _ = runtime.block_on(run_server(server_end, init));
    });

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(async move {
        let (reader, writer) = tokio::io::split(client_end);
        let (rpc, _incoming) = bemtvi_rpc::connect(reader, writer);

        // Run CODE via the bootstrap chunk (see [`oneshot_bootstrap`]): it compiles
        // CODE Lua-side, reports a load failure back as a string value, and otherwise
        // starts the async evaluation that sets the completion globals when it
        // settles. If CODE calls `btv._reexec`, the process is replaced before the
        // globals are ever read. A load failure is fatal — surfaced with the Lua error
        // and a non-zero exit, never swallowed into a 30-second poll of a flag the
        // chunk never got to set.
        match oneshot_exec(&rpc, &oneshot_bootstrap(&code)).await? {
            rmpv::Value::Boolean(true) => {}
            rmpv::Value::String(err) => bail!(
                "--lua CODE failed to load: {}",
                err.as_str().unwrap_or("Lua error")
            ),
            other => bail!("--lua: the bootstrap chunk failed unexpectedly (got {other})"),
        }

        // Poll until the chunk settles (each request pumps a server tick, so the async
        // work — fs reads, the reexec — makes progress), bounded so a hung chunk can't
        // wedge a shell. One request reads both globals: `nil` while pending, `true` on
        // success, the rejection string on failure. Returning exits the process
        // (dropping the server thread); an error exits non-zero via `main`'s `Result`.
        const SETTLE_TIMEOUT: Duration = Duration::from_secs(30);
        let deadline = std::time::Instant::now() + SETTLE_TIMEOUT;
        loop {
            let done = oneshot_exec(
                &rpc,
                "if _G.__bemtvi_oneshot_done then return _G.__bemtvi_oneshot_err or true end",
            )
            .await
            .map_err(|e| anyhow!("--lua: the embedded server went away: {e}"))?;
            match done {
                rmpv::Value::Boolean(true) => return Ok(()),
                rmpv::Value::String(err) => {
                    bail!("--lua: {}", err.as_str().unwrap_or("Lua error"))
                }
                _ => {}
            }
            if std::time::Instant::now() > deadline {
                bail!("--lua CODE did not settle within {SETTLE_TIMEOUT:?} (a stuck await?)");
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
}

/// The argument that runs this binary as the **daemon** (no UI, no editor): just the
/// fs + process + watch + LSP host the *edit-host split* drives. Defined as a constant
/// because [`daemon_command`] also passes it to the child it spawns. With `--listen` it
/// instead binds a WebTransport/QUIC listener (the native transport, Open Decision #2).
const DAEMON_FLAG: &str = "--daemon";

/// Default daemon bind address when `--listen` is given no explicit address: loopback
/// on a fixed port. Loopback-only is defense-in-depth (the bearer token is the actual
/// auth gate); pass an explicit `0.0.0.0:PORT` to accept off-host connections.
const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:8765";

// A positional argument with the daemon URI scheme (`bemtvi_server::CONNECT_URI_SCHEME`,
// `bemtvi://HOST:PORT?cert=HASH`, token via `$BEMTVI_DAEMON_TOKEN`; the legacy
// `bemtvi://HOST:PORT/TOKEN?cert=HASH` form still dials for the browser web client)
// selects the QUIC connect path over the default stdio-child split; `parse_connect_uri`
// (also bemtvi-server's, shared with the GUI) splits it into the dial pieces.

/// Internal, debug-only flag that runs this binary as a scripted mock language
/// server (see `bemtvi_lsp::mock`), used by the LSP test suite as a hermetic
/// stand-in for a real server. Never present in release builds.
#[cfg(debug_assertions)]
const LSP_MOCK_FLAG: &str = "--__lsp-mock";

/// bemtvi's command line. clap derives the parser, validates flags, errors on unknown
/// options, and generates `--help`/`--version` — there is no hand-rolled scanning.
///
/// The roles are mutually exclusive **flags** (grouped so clap rejects two at once),
/// and the positional [`Cli::targets`] is shared across them: the FILE to open in the
/// default / `--connect-daemon` editor, the DIR for `--test-plugin`, and/or a
/// `bemtvi://…` connect URI. (The QUIC connect role legitimately takes both a URI and a
/// file, so more than one positional is allowed; the URI is recognised by its scheme.)
#[derive(Parser)]
#[command(
    name = "bemtvi",
    version,
    about = "A modal, vim-style editor: a headless editor server plus a terminal UI client.",
    long_about = "A modal, vim-style editor: a headless editor server plus a terminal UI \
        client, both run from this one binary. With no role flag, bemtvi opens the given \
        file (or an empty buffer) in the terminal.",
    // The three roles below are mutually exclusive; clap enforces it and documents it.
    group = clap::ArgGroup::new("role").args(["test_plugin", "connect_daemon", "daemon"]),
    after_help = "Environment:\n  BEMTVI_DAEMON_CMD  Command the --connect-daemon role \
        spawns as its daemon, run through `sh -c`\n                    (e.g. \"ssh host \
        bemtvi --daemon\"). Unset = this binary in --daemon mode."
)]
struct Cli {
    /// File to open (DIR for --test-plugin), and/or a bemtvi://… daemon connect URI
    #[arg(value_name = "TARGET")]
    targets: Vec<String>,

    /// Run the Lua `btv.test` suite under <TARGET>/test/**/*_spec.lua and exit 0/1 (no UI; TARGET defaults to the cwd)
    #[arg(long)]
    test_plugin: bool,

    /// Run the editor locally but route fs/process/watch/LSP to a --daemon child
    #[arg(long)]
    connect_daemon: bool,

    /// Run the remote half: fs/process/watch/LSP host over stdin/stdout, no editor or UI
    #[arg(long)]
    daemon: bool,

    /// When connecting to a daemon, run the daemon's config + plugins instead of the
    /// local config (default: local config)
    #[arg(long)]
    remote_config: bool,

    /// With --daemon, bind a QUIC listener at [ADDR] (default 127.0.0.1:8765) instead of using stdio
    #[arg(long, value_name = "ADDR", num_args = 0..=1, requires = "daemon")]
    listen: Option<Option<SocketAddr>>,

    /// Scope shada (marks, registers, session) to a private ns/<NS> subfolder for this launch
    #[arg(long, value_name = "NS")]
    shada_namespace: Option<String>,

    /// Restore the saved window/tab layout at startup (requires --shada-namespace or
    /// --workspace, which derives a namespace from the directory; validated in `main`,
    /// since clap's `requires` cannot express the either/or)
    #[arg(long)]
    restore_session: bool,

    /// Run a Lua chunk after sourcing config, then exit (no UI) — for workspace wrapper scripts
    #[arg(long, value_name = "CODE")]
    lua: Option<String>,

    /// Open DIR as a workspace: cd into it, derive a per-directory shada namespace, and
    /// save/restore the window/split/dock/buffer layout across launches (DIR must be an
    /// existing directory; bare `--workspace` uses the cwd). The TARGET positional is a
    /// separate optional file to open. An explicit --shada-namespace overrides the derived one.
    #[arg(long, value_name = "DIR", num_args = 0..=1, default_missing_value = ".")]
    workspace: Option<String>,

    /// With --workspace, do NOT cd into the workspace directory (keep the launch cwd)
    #[arg(long, requires = "workspace")]
    workspace_no_cwd: bool,

    /// Extract the embedded Lua runtime (the `btv.*`/`vim.*` prelude source) to a
    /// directory and exit, so the Lua language server can index the API. Writes to
    /// [DIR], else $BEMTVI_RUNTIME, else <data-dir>/bemtvi/runtime; point your
    /// `.luarc.json` `workspace.library` at that path.
    #[arg(long, value_name = "DIR", num_args = 0..=1, default_missing_value = "")]
    extract_lua_runtime: Option<String>,
}

/// Env var naming the extracted Lua runtime directory. A `.luarc.json` can point its
/// `workspace.library` at `$BEMTVI_RUNTIME` and the Lua language server expands it.
const RUNTIME_ENV: &str = "BEMTVI_RUNTIME";

/// The directory the extracted Lua runtime lives in, with no `DIR` override: an explicit
/// `$BEMTVI_RUNTIME` if the user (or a prior launch) exported one, else the standard
/// `<data-dir>/bemtvi/runtime` (matching `stdpath("data")`). Shared by `--extract-lua-runtime`
/// and the startup export in `main`, so both agree on the default location.
fn default_runtime_dir() -> PathBuf {
    std::env::var_os(RUNTIME_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(bemtvi_lua::stdpath("data")).join("runtime"))
}

/// Handle `--extract-lua-runtime [DIR]`: write the embedded `btv.*`/`vim.*` prelude to a
/// directory the Lua language server can index (`workspace.library`), then return.
///
/// The destination resolves in priority order: the `DIR` argument if given, else an
/// already-set `$BEMTVI_RUNTIME`, else `<data-dir>/bemtvi/runtime` (the standard data path,
/// matching `stdpath("data")`). We set `$BEMTVI_RUNTIME` for this process and — since a
/// child cannot mutate the parent shell — print the export line the user should add so
/// their `.luarc.json`'s `$BEMTVI_RUNTIME` resolves in future sessions.
fn extract_lua_runtime(dir_arg: String) -> Result<()> {
    let dir = if !dir_arg.is_empty() {
        PathBuf::from(dir_arg)
    } else {
        default_runtime_dir()
    };

    let written = bemtvi_lua::extract_prelude(&dir)
        .map_err(|e| anyhow!("--extract-lua-runtime: writing to {}: {e}", dir.display()))?;

    // Absolute path so the printed export line works from any cwd.
    let abs = std::fs::canonicalize(&dir).unwrap_or(dir);
    let already_set = std::env::var_os(RUNTIME_ENV).is_some();
    std::env::set_var(RUNTIME_ENV, &abs);

    println!(
        "Extracted {} Lua runtime file(s) to {}",
        written.len(),
        abs.display()
    );
    if !already_set {
        println!(
            "\nAdd this to your shell so `.luarc.json` can resolve $BEMTVI_RUNTIME:\n  \
             export {RUNTIME_ENV}={}",
            abs.display()
        );
    }
    println!("\n.luarc.json:\n  {{ \"workspace.library\": [\"${RUNTIME_ENV}\"] }}");
    Ok(())
}

fn main() -> Result<()> {
    // Mock language server mode (debug builds only): a hermetic, scripted LSP
    // server the test suite spawns instead of a real one. It never starts an
    // editor; the script path follows the flag. Handled before clap so the internal
    // flag stays off the public surface (and clap would reject the `--__`-prefixed arg).
    #[cfg(debug_assertions)]
    if std::env::args().nth(1).as_deref() == Some(LSP_MOCK_FLAG) {
        let script = std::env::args().nth(2).unwrap_or_default();
        bemtvi_lsp::mock::run(&script);
        return Ok(());
    }

    // clap parses, validates, and exits with usage/version itself for `--help`/`-h`,
    // `--version`/`-V`, unknown options, and conflicting roles.
    let cli = Cli::parse();

    // `--extract-lua-runtime [DIR]`: dump the embedded prelude to disk for the Lua
    // language server, then exit. Standalone — it starts no editor and needs no other
    // flag, so it runs before any role dispatch or validation below.
    if let Some(dir) = cli.extract_lua_runtime {
        return extract_lua_runtime(dir);
    }

    // `--restore-session` needs a shada namespace to restore *from*: an explicit
    // `--shada-namespace`, or the one a `--workspace` launch derives from its directory.
    // clap's `requires` can't express the either/or, so validate it here — loudly, in
    // every role, rather than silently never restoring.
    if cli.restore_session && cli.shada_namespace.is_none() && cli.workspace.is_none() {
        bail!("--restore-session requires --shada-namespace or --workspace");
    }

    // The first `bemtvi://…` positional is the QUIC connect target; the first other
    // positional is the file (or, for --test-plugin, the plugin dir).
    let connect_uri = cli
        .targets
        .iter()
        .find(|a| a.starts_with(CONNECT_URI_SCHEME))
        .cloned();
    let file = cli
        .targets
        .iter()
        .find(|a| !a.starts_with(CONNECT_URI_SCHEME))
        .cloned();

    // Expose every positional file argument to `btv.argv()` (newline-joined), in all
    // roles, before any server thread reads the environment.
    let argv: Vec<&str> = cli
        .targets
        .iter()
        .filter(|a| !a.starts_with(CONNECT_URI_SCHEME))
        .map(String::as_str)
        .collect();
    std::env::set_var(ARGV_ENV, argv.join("\n"));

    // Expose the Lua runtime dir in this process's environment (mirroring BEMTVI_ARGV), so the
    // editor's own Lua and every `btv.run` child — which inherit this env — can resolve
    // `$BEMTVI_RUNTIME` with no shell config. Set-if-absent: an explicit shell export, or a
    // custom `--extract-lua-runtime DIR` location the user already exported, wins over the
    // `<data-dir>/bemtvi/runtime` default. Runs in every role that reaches here (the
    // `--extract-lua-runtime` branch returned above and resolves its own destination).
    if std::env::var_os(RUNTIME_ENV).is_none() {
        std::env::set_var(RUNTIME_ENV, default_runtime_dir());
    }

    // Plugin test runner role: no editor UI — boot an embedded server, drive the Lua
    // `btv.test` suite, and exit with the pass/fail code. The positional is the plugin
    // dir (default: the cwd).
    if cli.test_plugin {
        let dir = file
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let passed = test_runner::run_test_plugin(dir)?;
        std::process::exit(if passed { 0 } else { 1 });
    }

    // Daemon role: the edit-host split's remote half — fs/process/watch/LSP, no editor
    // and no UI. With `--listen` it binds a QUIC listener (the real native transport);
    // otherwise it serves over this process's stdin/stdout (the local stand-in), wound
    // down by EOF on stdin.
    if cli.daemon {
        if let Some(addr) = cli.listen {
            let addr = addr.unwrap_or_else(|| {
                DEFAULT_LISTEN_ADDR
                    .parse()
                    .expect("DEFAULT_LISTEN_ADDR is a valid socket address")
            });
            return run_daemon_listen(addr);
        }
        return run_daemon();
    }

    // `--workspace DIR`: for a *local* session, resolve the directory value (the cwd for
    // bare `--workspace`) to an absolute path now, so its derived shada namespace +
    // `btv.workspace` root are known. A non-directory / missing dir aborts here. A *daemon*
    // session's workspace lives on the remote machine — its root + namespace derive from the
    // daemon's cwd post-connect ([`ShadaOpts::resolve_remote_workspace`]), so skip the local
    // resolve. The TARGET positional is a separate optional file, no longer the workspace dir.
    let is_daemon_session = cli.connect_daemon || connect_uri.is_some();
    let workspace_dir = if cli.workspace.is_some() && !is_daemon_session {
        Some(resolve_workspace_dir(cli.workspace.as_deref())?)
    } else {
        None
    };

    // Derive the shada namespace (from `--shada-namespace`, or a local `--workspace`
    // directory) and the workspace identity surfaced to Lua. No env stamping — the runtime
    // is seeded from `ServerInit`, so a daemon session can fill in the remote-derived values.
    let shada = ShadaOpts::from_cli(&cli, workspace_dir.as_deref())?;

    // `--lua` headless mode: source config (so plugins load), run the one-liner, exit —
    // no UI. A workspace wrapper uses it to read its config and relaunch with the right
    // `--shada-namespace`; see `btv.reexec`.
    if let Some(code) = cli.lua.clone() {
        return run_lua_oneshot(
            code,
            &shada,
            cli.workspace.is_some() && !cli.workspace_no_cwd,
        );
    }

    // Which config (+ shada) a daemon session runs: the local machine's by default, or
    // the daemon's with `--remote-config`. `--remote-config` is meaningless without a
    // connect target — reject it loudly rather than silently ignore it (the TUI has no
    // live `:connect`, so a connect target must be on the command line).
    let config_source = if cli.remote_config {
        ConfigSource::Remote
    } else {
        ConfigSource::Local
    };
    if cli.remote_config && connect_uri.is_none() && !cli.connect_daemon {
        bail!("--remote-config only applies when connecting to a daemon (--connect-daemon or a bemtvi://… target)");
    }

    // Edit-host split, local half, over QUIC: a `bemtvi://…` target (with or without the
    // `--connect-daemon` flag) connects to a `--daemon --listen` listener and routes
    // fs/process/watch/LSP to it. Checked before the stdio-child split below.
    if let Some(uri) = connect_uri {
        return run_with_daemon_quic(file, &uri, shada, config_source);
    }

    // Edit-host split, local half, over stdio: the default editor + UI, but spawning a
    // `--daemon` child and routing fs/process/watch/LSP to it over stdio pipes.
    if cli.connect_daemon {
        return run_with_daemon(file, shada, config_source);
    }

    // In-process, bidirectional transport between client and server.
    let (server_end, client_end) = tokio::io::duplex(1 << 16);

    // The server runs on its own thread with its own single-threaded runtime,
    // so the (non-Send) editor + Lua state live entirely on that thread.
    let (config_dir, runtimepath) = bemtvi_server::default_runtime();
    let init = ServerInit {
        file,
        config_dir,
        // Persist cross-session state (registers, marks, history, …) under
        // `stdpath("state")/shada`, or a private `ns/<NS>` subfolder when
        // `--shada-namespace` is given. Editor state lives where the editor runs, so the
        // edit-host split persists locally (only fs/proc cross to the daemon).
        shada: Some(shada.store()),
        // The global history store (used iff `'persisthistory'` includes `global` on a
        // workspace launch); `None` for a plain launch (its primary is already global).
        global_shada: shada.global_history_store(),
        // A local (embedded) session never syncs shada to a daemon.
        remote_shada: None,
        // A namespaced launch also captures the editor session (open files + exact
        // layout); `--restore-session` reapplies it at boot.
        workspace_session: shada.capture(),
        restore_session: shada.do_restore(),
        // `--workspace` auto-opts into capturing the layout (no plugin needed).
        session_save_layout: shada.session_save_layout(),
        // Seed the namespace + root that `btv.shada.namespace()` / `btv.workspace` report.
        shada_namespace: shada.namespace().map(str::to_owned),
        workspace_dir: shada.workspace_dir.clone(),
        // cd into the workspace root at boot (the canonical `bemtvi --workspace DIR`),
        // unless `--workspace-no-cwd` keeps the launch cwd. Known here, before the server
        // opens any file or restores the session — so no later reconciliation is needed.
        workspace_cwd: cli.workspace.is_some() && !cli.workspace_no_cwd,
        runtimepath,
        // The real editor wires the host clipboard for the `"+` / `"*` registers.
        clipboard: bemtvi_server::ClipboardProvider::System,
        // The real binary uses the monotonic wall clock for mouse multi-click
        // timing; only tests inject a fake clock here.
        mouse_clock: None,
        // The local binary reads/writes through the real disk (the default); a
        // daemon-backed fs is injected here by the edit-host split.
        host_fs: None,
        // The local binary spawns real local processes (the default); a
        // daemon-backed process host is injected here by the edit-host split.
        host_proc: None,
        // The local binary opens the startup file synchronously through the disk;
        // the async (daemon) fs is injected here by the edit-host split.
        host_fs_async: None,
        // The local binary spawns language servers as real local children (the
        // default); a daemon-backed LSP transport is injected here by the edit-host
        // split.
        lsp_transport: None,
        // The local binary opens `:terminal` PTYs locally (the default); a daemon-backed
        // terminal seam is injected here by the edit-host split.
        host_term: None,
        // The local binary runs `btv.fs` against the local disk (the actor's `StdLuaFs`);
        // a daemon-backed `luafs_op` seam is injected here by the edit-host split.
        fs_jobs: None,
        // Likewise, `btv.fs.watch` watches the local disk; a daemon session arms its
        // watches on the daemon (`luafs_watch`) instead — see the branch below.
        fs_watch: None,
        // Likewise `btv.http` runs locally (the actor's `ureq`); a daemon-backed `http_op`
        // seam is injected by the edit-host split (see the daemon-session branch below).
        http_jobs: None,
        // `btv.git` runs locally (the actor's gix engine); a daemon session injects a
        // `git_op` seam below.
        git_jobs: None,
        // The interactive binary offers bemtvi's built-in default recommended set on a
        // fresh setup (the first-run welcome); a config's own recommend{} overrides it.
        offer_default_recommended: true,
        // Seed the client-owned system-plugin tier (`stdpath("data")/system/*`) into every
        // session — loaded before init.lua, guaranteed present before any connect. Empty on
        // a fresh install (no system dir), so nothing changes until a connector is installed.
        system_plugins: bemtvi_server::discover_system_plugins(),
        // Command-line completion (`:`+<Tab>) is on by default in the interactive
        // binary; a config's own `btv.cmdline_complete.setup{ ... }` still wins.
        cmdline_complete_default: true,
        // Interactive: `print` shows on the message line, not stdout (which the TUI owns).
        lua_stdio: false,
        // A local session has no remote parser set to mirror; tree-sitter installs
        // happen on demand via `:TSInstall`.
        ts_autoinstall: Vec::new(),
        // Local session: seed the working directory from the local process cwd
        // (`EditHost::new`'s default), not a daemon.
        remote_cwd: None,
        // Local session: a leading `~` expands against this process's own `$HOME`.
        remote_home: None,
        // The TUI handles `:connect`/`:workspace` only at startup (flags), not as live
        // client-intercepted commands, so it registers no virtual commands.
        client_init_lua: None,
        // The reconnecting daemon link is not wired into the TUI yet (a later phase migrates
        // `--connect-daemon` / QUIC to the reconnecting dialer); a one-shot link has no handle.
        daemon_link: None,
    };
    let server_thread = std::thread::spawn(move || {
        // Test-only fault injection (debug builds only): force a server-thread
        // panic so the parent's crash handling below can be exercised end to end.
        // Compiled out of release builds entirely.
        #[cfg(debug_assertions)]
        if std::env::var_os("BEMTVI_PANIC_TEST").is_some() {
            panic!("BEMTVI_PANIC_TEST: injected server-thread panic");
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("failed to build server runtime");
        runtime
            .block_on(run_server(server_end, init))
            .map_err(|err| anyhow!("server error: {err}"))
    });

    // The client (terminal UI) runs on the main thread; when it exits the dropped
    // stream signals the server to wind down, and a server-thread panic surfaces as
    // exit 101 (see `drive_tui_with_swaps`).
    drive_tui_with_swaps(client_end, server_thread)
}

/// Run the terminal UI client on the main thread until it exits, keeping the window across
/// `btv.session.reconnect` swaps (§B), then join every server thread (the initial one plus
/// each swap's). When the client exits, dropping its transport signals the server to wind
/// down. A server-thread failure is surfaced as a non-zero exit, taking precedence over a
/// clean client `result`, since a crashed server is the more important failure: a *panic*
/// exits 101 (Rust's conventional panic code), an `Err` return (a failed daemon connect, an
/// edit-host setup error) exits 1 with the message on stderr. Both checks run after the TUI
/// has restored the terminal. Shared by the default and edit-host roles.
fn drive_tui_with_swaps(
    client_end: tokio::io::DuplexStream,
    server_thread: std::thread::JoinHandle<Result<()>>,
) -> Result<()> {
    use std::sync::{Arc, Mutex};

    // Every server thread this window drives: the initial one, plus one per swap. Shared with
    // the builder (which pushes each swap's) so we can join them all at teardown.
    let threads: Arc<Mutex<Vec<std::thread::JoinHandle<Result<()>>>>> =
        Arc::new(Mutex::new(vec![server_thread]));
    let threads_for_build = threads.clone();
    // The session builder the TUI swap loop calls on a `btv_session_reconnect`: parse the wire
    // spec, bring up the new backend (blocking on its handshake, so a connect failure is an
    // Err that leaves the current session intact), record its server thread, and hand back the
    // new client transport.
    let builder: bemtvi_tui::SessionBuilder<tokio::io::DuplexStream> =
        Arc::new(move |params: Vec<rmpv::Value>| {
            // The builder serves both swap notifications (§B/§C). `btv_session_reconnect`
            // sends a normalized spec MAP; `btv_connect_fallback` (a `:connect <url>` with no
            // provider) sends the raw URL STRING — its default direct dial. Distinguish by
            // the param's type, then bring up the backend the same way.
            let param = params
                .first()
                .ok_or_else(|| anyhow!("session swap: missing spec/url"))?;
            let spec = match param.as_str() {
                Some(url) => ReconnectSpec::fallback_from_url(url)?,
                None => ReconnectSpec::from_value(param)?,
            };
            let (stream, handle) = build_session_from_spec(spec)?;
            threads_for_build.lock().unwrap().push(handle);
            Ok(stream)
        });

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    let result = runtime.block_on(bemtvi_tui::run(client_end, builder));

    // Join every server thread. A panic / error takes precedence over the client result.
    let handles: Vec<_> = std::mem::take(&mut *threads.lock().unwrap());
    for handle in handles {
        match handle.join() {
            Err(payload) => {
                eprintln!(
                    "bemtvi: server thread panicked: {}",
                    panic_message(payload.as_ref())
                );
                std::process::exit(101);
            }
            Ok(Err(err)) => {
                eprintln!("bemtvi: {err:#}");
                std::process::exit(1);
            }
            Ok(Ok(())) => {}
        }
    }
    // If we are only here because the process was killed, the shutdown is now
    // complete — the editor ran its exit sequence, the server flushed its shada, the
    // terminal is restored. Die of that signal so the shell (or whatever sent it)
    // still sees a `kill`, not a clean exit 0. Returns normally otherwise.
    bemtvi_tui::exit_as_signal_if_killed();
    result
}

/// Build the daemon session a `btv.session.reconnect(spec)` requested (§B): the wire spec's
/// transport chooses the dialer and `spec.config_source` (§D) drives config resolution. The
/// system-plugin tier (§A) is re-seeded regardless (in [`spawn_edit_host_session`]). A
/// swapped session is a plain (non-workspace) session — it does not inherit the launching
/// session's workspace identity. **Blocking** on the handshake, so a failed provision/spawn
/// is an `Err` the swap loop reports while leaving the current session intact.
fn build_session_from_spec(
    spec: ReconnectSpec,
) -> Result<(tokio::io::DuplexStream, std::thread::JoinHandle<Result<()>>)> {
    spec.reject_keep_buffers()?;
    let config_source = spec.config_source;
    let shada = ShadaOpts {
        namespace: None,
        restore: false,
        workspace: false,
        workspace_dir: None,
    };
    match spec.transport {
        ReconnectTransport::Spawn { command } => {
            spawn_edit_host_session(None, shada, config_source, move || {
                // Reconnecting stdio child running the spec's command (structured argv, or a
                // `sh -c` line like `run_with_daemon`'s `$BEMTVI_DAEMON_CMD`); each (re)dial
                // re-spawns it on the link thread. The daemon's stderr goes to the private
                // temp log (it can't share the TUI's terminal).
                let (client, handle) = connect_daemon_respawning(
                    "spawning the daemon",
                    move || {
                        let mut c = command.to_command();
                        c.stderr(daemon_log_stderr());
                        Ok(c)
                    },
                    ReconnectPolicy::default(),
                )?;
                Ok((client, Some(handle), Box::new(())))
            })
        }
        ReconnectTransport::Quic { addr } => {
            let (url, cert_hash, token) = parse_connect_uri(&addr)?;
            spawn_edit_host_session(None, shada, config_source, move || {
                let (client, handle) = connect_quic_reconnecting(
                    &url,
                    &cert_hash,
                    &token,
                    ReconnectPolicy::default(),
                )?;
                Ok((client, Some(handle), Box::new(())))
            })
        }
    }
}

/// Run the daemon (`--daemon`) over this process's stdin/stdout until the edit-host
/// closes the pipe. No editor, no Lua, no UI, and **no config sourcing**: the daemon
/// is pure I/O. `run_daemon_io` connects once and
/// multiplexes every leg of the wire (fs/process/watch/`sys_run`/LSP/`luafs`) onto
/// this one stdio stream, serving each against the real local disk and processes;
/// LSP/process discovery and the project tree arrive on the wire from the remote
/// edit-host. The server owns the main thread's runtime directly.
fn run_daemon() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(run_server_daemon_io(
        tokio::io::stdin(),
        tokio::io::stdout(),
    ))
}

/// Run the daemon as a **QUIC listener** (`--daemon --listen [addr]`): the real native
/// transport (Open Decision #2). Mints a bearer token + self-signed cert, binds `addr`,
/// prints the connect strings the edit-host needs, then accepts connections — each
/// running the full six-leg multiplexer ([`serve_quic`] → `run_daemon_io`) over one
/// QUIC bidi stream. Like `--daemon`, no editor / Lua / config: pure I/O.
fn run_daemon_listen(addr: SocketAddr) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(async move {
        let (endpoint, info) = bind_quic_listener(addr, mint_token())?;
        // The connect credentials (cert hash + token are the auth) go to stdout so a
        // human can copy the command; if the bind address is non-loopback, the user
        // substitutes the reachable host for the connecting machine.
        println!("bemtvi daemon listening on {}", info.addr);
        // Browser form first: a webpage has no shell env, so its paste string keeps
        // the legacy `/TOKEN` path (the WebTransport CONNECT path *is* the auth
        // channel). It also carries the full URI first in stdout, which is the form
        // the browser verifiers grep for.
        println!(
            "  browser: paste '{CONNECT_URI_SCHEME}{}/{}?cert={}' into the web client",
            info.addr, info.token, info.cert_hash
        );
        // The native command runs through a shell, so the token rides the env var,
        // NOT the URI: a `bemtvi://…` URI that lands in shell history, a log, or a
        // reconnect config no longer carries the daemon's sole auth credential.
        println!(
            "  connect with: {DAEMON_TOKEN_ENV}={} bemtvi --connect-daemon '{CONNECT_URI_SCHEME}{}?cert={}'",
            info.token, info.addr, info.cert_hash
        );
        serve_quic(endpoint, info.token).await
    })
}

/// Build the [`tokio::process::Command`] for the daemon child. Defaults to *this*
/// binary in `--daemon` mode (a local two-process split over stdio pipes); if
/// `$BEMTVI_DAEMON_CMD` is set ([`env_daemon_command`]), run that command line through
/// `sh -c` instead — so a remote daemon (`ssh host bemtvi --daemon`) is just an env
/// var, no code change.
fn daemon_command() -> Result<tokio::process::Command> {
    if let Some(c) = env_daemon_command() {
        return Ok(c);
    }
    let exe = std::env::current_exe()?;
    let mut c = tokio::process::Command::new(exe);
    c.arg(DAEMON_FLAG);
    Ok(c)
}

/// Run the **local** edit-host over a stdio-piped `--daemon` child (`--connect-daemon`,
/// no `bemtvi://` target): the local two-process split. The server thread spawns the
/// daemon (its stdio *is* the wire), wraps it in [`connect_daemon`], and runs the editor
/// against those seams. The daemon's stderr is redirected to a private temp log (it can't
/// corrupt the TUI); `tail` it to debug the daemon. `kill_on_drop` reaps the child on quit.
fn run_with_daemon(
    file: Option<String>,
    shada: ShadaOpts,
    config_source: ConfigSource,
) -> Result<()> {
    run_edit_host_session(file, shada, config_source, || {
        // Reconnecting: re-spawn the daemon command on each (re)dial so a dropped link
        // (the daemon died, or — for an `BEMTVI_DAEMON_CMD="ssh …"` split — the hop dropped
        // on sleep) re-establishes the seams in place, keeping the editor's local state.
        // The daemon's stderr can't share the terminal with the TUI; send it to the
        // private, symlink-safe log the user can tail to diagnose the daemon side.
        let (client, handle) = connect_daemon_respawning(
            "spawning the daemon",
            || {
                let mut c = daemon_command()?;
                c.stderr(daemon_log_stderr());
                Ok(c)
            },
            ReconnectPolicy::default(),
        )?;
        Ok((client, Some(handle), Box::new(())))
    })
}

/// Run the **local** edit-host over a QUIC connection to a `--daemon --listen` listener
/// (a tokenless `bemtvi://HOST:PORT?cert=HASH` target; the bearer token comes from
/// `$BEMTVI_DAEMON_TOKEN` so the copy-paste-able URI never carries the daemon's auth
/// credential). Same editor + TUI as the stdio split;
/// only the transport differs — [`connect_quic_reconnecting`] pins the daemon's cert TOFU and
/// presents the bearer token, then returns the same seams plus a reconnect handle. The QUIC
/// endpoint + connection are owned by the link thread (no child to hold), and re-dialed under
/// the seams on a drop (sleep/wake, a network blip), like the ssh path.
fn run_with_daemon_quic(
    file: Option<String>,
    uri: &str,
    shada: ShadaOpts,
    config_source: ConfigSource,
) -> Result<()> {
    let (url, cert_hash, token) = parse_connect_uri(uri)?;
    run_edit_host_session(file, shada, config_source, move || {
        let (client, handle) =
            connect_quic_reconnecting(&url, &cert_hash, &token, ReconnectPolicy::default())?;
        Ok((client, Some(handle), Box::new(())))
    })
}

/// The shared edit-host runtime: spawn the server + drive the TUI. Delegates the session
/// build to [`spawn_edit_host_session`] (blocking on the handshake) and then the swap-capable
/// TUI driver. The editor keystroke path stays local; only fs/process/watch/LSP cross the
/// wire, and config comes from `config_source` (the daemon's, or this machine's).
fn run_edit_host_session<F>(
    file: Option<String>,
    shada: ShadaOpts,
    config_source: ConfigSource,
    connect: F,
) -> Result<()>
where
    F: FnOnce() -> Result<(
            DaemonClient,
            Option<ReconnectHandle>,
            Box<dyn std::any::Any + Send>,
        )> + Send
        + 'static,
{
    let (client_end, server_thread) = spawn_edit_host_session(file, shada, config_source, connect)?;
    // The client (terminal UI) runs on the main thread, exactly as the default role, and
    // keeps the window across any `btv.session.reconnect` swap.
    drive_tui_with_swaps(client_end, server_thread)
}

/// Build the in-process editor↔TUI duplex, run the embedded server (host seams from
/// `connect`) on its own thread, and **block until its daemon handshake + config resolution
/// succeed** — so a connect failure is an `Err` HERE (before the UI starts, or before a swap
/// commits), leaving any current session intact. Returns the client transport + the server
/// thread handle (joined by the caller). `connect` runs inside the server runtime and yields
/// the [`DaemonClient`] plus a guard kept alive for the whole session (the stdio child, so
/// `kill_on_drop` reaps it on quit; `()` for QUIC). Shared by the startup daemon roles and
/// the `btv.session.reconnect` swap builder ([`build_session_from_spec`]).
fn spawn_edit_host_session<F>(
    file: Option<String>,
    shada: ShadaOpts,
    config_source: ConfigSource,
    connect: F,
) -> Result<(tokio::io::DuplexStream, std::thread::JoinHandle<Result<()>>)>
where
    F: FnOnce() -> Result<(
            DaemonClient,
            Option<ReconnectHandle>,
            Box<dyn std::any::Any + Send>,
        )> + Send
        + 'static,
{
    bemtvi_server::spawn_session_thread(move || async move {
        assemble_edit_host_init(file, shada, config_source, connect).await
    })
}

/// Connect to the daemon, resolve the session config + shada from `config_source`, and
/// assemble the [`ServerInit`] for an edit-host TUI session, returning it alongside the
/// session guard (kept alive by the caller). Config + plugins are fetched from the daemon
/// and materialized locally for `Remote`; the client-owned system-plugin tier (§A) is
/// re-seeded either way. A broken link / unstageable cache is loud (the session can't run a
/// config it couldn't resolve), not a silent fall back.
async fn assemble_edit_host_init<F>(
    file: Option<String>,
    shada: ShadaOpts,
    config_source: ConfigSource,
    connect: F,
) -> Result<(ServerInit, Box<dyn std::any::Any + Send>)>
where
    F: FnOnce() -> Result<(
        DaemonClient,
        Option<ReconnectHandle>,
        Box<dyn std::any::Any + Send>,
    )>,
{
    let (client, daemon_link, guard) = connect()?;
    // Resolve the session's config from the chosen source. `Remote` (`--remote-config`)
    // fetches the daemon's config surface (one `config_bundle` round trip) and materializes
    // it onto a local per-process cache, then points `config_dir`/`runtimepath` at that copy
    // — the editor loads the remote's config + plugins locally (Lua's synchronous
    // `require`/runtimepath can't await the daemon, so the files must be local). `Local` (the
    // default) runs this machine's own config and fetches only the daemon's cwd / parser set.
    // Either way the cwd seeds `DirState` so `:pwd`/`:cd`/`getcwd` operate on the daemon's dir
    // (`docs/plans/2026-06-23-remote-cwd.md`), and tree-sitter parsers are compiled locally on
    // demand.
    let resolved = client
        .config
        .resolve(config_source)
        .await
        .map_err(|e| anyhow!("could not resolve the session config from the daemon: {e}"))?;
    // A `--workspace` daemon session derives its identity (shada namespace + root) from the
    // *daemon's* cwd, now that we know it — the workspace lives on the remote machine. A
    // non-workspace launch / explicit `--shada-namespace` is left as-is.
    let mut shada = shada;
    shada.resolve_remote_workspace(resolved.remote_cwd.as_deref());
    // Shada follows the config source. `Remote` keeps it on the daemon (Approach A):
    // download its store into a local staging dir now (before `client.host_fs` is moved
    // below) and sync it back after each flush. A download error disables remote shada —
    // fall back to the local store rather than clobber the daemon's copy. `Local` uses the
    // local store (the `--shada-namespace` workspace store + session).
    let (shada_store, remote_shada) = bemtvi_server::resolve_session_shada(
        &client.host_fs,
        config_source,
        resolved.state_dir.as_deref(),
        shada.namespace(),
        shada.store(),
    )
    .await;
    let init = ServerInit {
        file,
        config_dir: resolved.config_dir,
        shada: Some(shada_store),
        // A remote/daemon session keeps the existing single-store behavior — the
        // global history dual-write is native + local only.
        global_shada: None,
        remote_shada,
        workspace_session: shada.capture(),
        restore_session: shada.do_restore(),
        session_save_layout: shada.session_save_layout(),
        // The namespace + root reported by `btv.shada.namespace()` / `btv.workspace` —
        // for a workspace daemon session these are the remote-cwd-derived values.
        shada_namespace: shada.namespace().map(str::to_owned),
        workspace_dir: shada.workspace_dir.clone(),
        // A daemon session's workspace cd happens on the daemon (remote cwd); the
        // local half never cds.
        workspace_cwd: false,
        runtimepath: resolved.runtimepath,
        clipboard: bemtvi_server::ClipboardProvider::System,
        mouse_clock: None,
        // The local disk is unused for buffers in a daemon session — every
        // fs/process/LSP/Lua-fs path is routed to the daemon below.
        host_fs: None,
        host_proc: Some(Box::new(client.host_proc)),
        host_fs_async: Some(Box::new(client.host_fs)),
        lsp_transport: Some(Box::new(client.lsp_transport)),
        // `:terminal` opens its PTY on the daemon (where the files are), not locally.
        host_term: Some(client.host_term),
        fs_jobs: Some(client.fs_jobs),
        // Arm `btv.fs.watch` on the daemon (where the files are) over `luafs_watch`, so a
        // watch — and everything built on one, the LSP file-watch client included — sees
        // the remote project rather than this machine's disk.
        fs_watch: Some(client.fs_watch),
        // Route `btv.http.fetch` to the daemon (which owns the network) over `http_op`.
        http_jobs: Some(client.http),
        // Route session-scoped `btv.git.*` to the daemon (where the repo is) over `git_op`.
        git_jobs: Some(client.git_jobs),
        // A daemon-backed session is still the interactive editor — offer the
        // built-in default recommended set on first run, and enable
        // command-line completion by default (a config's setup{} still wins).
        offer_default_recommended: true,
        // Seed the client-owned system-plugin tier into the daemon session too:
        // system plugins load into the LOCAL VM via the local runtimepath (like the
        // remote-aware plugin manager), so a connector persists across the reload.
        system_plugins: bemtvi_server::discover_system_plugins(),
        cmdline_complete_default: true,
        // Interactive: `print` shows on the message line, not stdout.
        lua_stdio: false,
        // Mirror the daemon's installed tree-sitter parsers locally.
        ts_autoinstall: resolved.ts_autoinstall,
        // Seed the working directory from the daemon (remote-cwd).
        remote_cwd: resolved.remote_cwd,
        // A leading `~` in a file argument expands against the daemon's home.
        remote_home: resolved.remote_home,
        // The TUI registers no client-intercepted virtual commands.
        client_init_lua: None,
        // The reconnecting daemon link (ssh/stdio) — its status flows into
        // `btv.daemon.status()` and `:reconnect`/`:disconnect` drive it. `None` for a
        // one-shot QUIC connect.
        daemon_link,
    };
    Ok((init, guard))
}

/// Best-effort human-readable text from a thread panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "<non-string panic payload>"
    }
}
