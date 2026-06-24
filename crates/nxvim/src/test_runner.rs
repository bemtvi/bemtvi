//! `nxvim --test-plugin [dir]` — the headless runner for a plugin's Lua test suite.
//!
//! It boots an embedded server on its own thread (the same `run_server` the editor
//! uses) and drives it as an RPC client over an in-process duplex — exactly the
//! Rust black-box harness pattern, but pointed at a plugin repo and orchestrating a
//! *Lua* framework (`nx.test`, see `crates/nxvim-lua/src/prelude/test.lua`).
//!
//! Flow: discover `<dir>/test/**/*_spec.lua`, source each over `nvim_exec_lua` so it
//! registers `nx.test.describe/it` cases, kick `nx.test._run()` (asynchronous — the
//! cases await ticks), poll `nx.test` for the results, print a report, and exit
//! `0`/`1` for CI. The plugin under test is the sole runtimepath entry, so its
//! `require("<name>")` resolves; config (no user `init.lua`), clipboard, and shada
//! are all hermetic.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ClipboardProvider, ServerInit};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// How long to wait for the whole suite to finish before declaring it hung. Each
/// test settles in a handful of ticks; a multi-second cap is generous and only trips
/// on a genuinely stuck `wait_for` / infinite await.
const SUITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Entry point for the `--test-plugin` role. `dir` is the plugin repo root (defaults
/// to the cwd at the call site). Returns `Ok(true)` when every test passed.
pub fn run_test_plugin(dir: PathBuf) -> Result<bool> {
    let dir = dir.canonicalize().unwrap_or(dir);
    let test_dir = dir.join("test");
    let mut files = Vec::new();
    discover(&test_dir, &mut files)
        .with_context(|| format!("scanning {} for *_spec.lua", test_dir.display()))?;
    files.sort();
    if files.is_empty() {
        eprintln!(
            "nxvim --test-plugin: no *_spec.lua files under {}",
            test_dir.display()
        );
        return Ok(false);
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("building the test runner runtime")?;
    runtime.block_on(run(dir, files))
}

/// Recursively collect `*_spec.lua` files under `dir` (missing dir = no files).
fn discover(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            discover(&path, out)?;
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with("_spec.lua"))
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Spawn the embedded server (its own thread + runtime) wired to a client over an
/// in-process duplex — the harness `spawn`, inlined so the binary needs no test dep.
fn spawn_server(plugin_dir: PathBuf) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (server_end, client_end) = tokio::io::duplex(1 << 16);
    std::thread::spawn(move || {
        let init = ServerInit {
            file: None,
            // Hermetic: no user init.lua, the plugin is the only runtimepath entry
            // (so `require("<plugin>")` resolves), no persistence, no host clipboard.
            config_dir: None,
            shada: None,
            workspace_session: false,
            restore_session: false,
            runtimepath: vec![plugin_dir],
            clipboard: ClipboardProvider::Disabled,
            mouse_clock: None,
            host_fs: None,
            host_proc: None,
            host_fs_async: None,
            lsp_transport: None,
            fs_jobs: None,
            // Hermetic: never offer the built-in recommended set / first-run welcome,
            // and leave command-line completion off (a plugin's own setup{} can opt in).
            offer_default_recommended: false,
            cmdline_complete_default: false,
            // Hermetic: no remote, so no tree-sitter parsers to mirror.
            ts_autoinstall: Vec::new(),
            // No daemon: seed the cwd from the local process.
            remote_cwd: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("test server runtime");
        let _ = runtime.block_on(run_server(server_end, init));
    });
    let (reader, writer) = tokio::io::split(client_end);
    connect(reader, writer)
}

/// Drive the suite to completion and print the report. `_incoming` is held (not
/// read) so the connection's reader task stays alive — dropping it closes the wire.
async fn run(dir: PathBuf, files: Vec<PathBuf>) -> Result<bool> {
    let (rpc, _incoming) = spawn_server(dir.clone());

    // Attach a UI so the server projects redraws (the `nx._ui` mirror a test's
    // `t:float()` / `t:message()` read from is populated on redraw).
    rpc.request(
        "nx_ui_attach",
        vec![Value::from(80u64), Value::from(24u64), Value::Map(vec![])],
    )
    .await
    .map_err(|e| anyhow!("attaching the test UI: {e}"))?;

    // Turn on plugin-test mode: installs the `nx.test` framework into Lua and starts
    // the `nx._ui` redraw mirror. Without this the API is absent — the gate that keeps
    // it out of normal editor sessions.
    rpc.request("nx_enable_test_mode", vec![])
        .await
        .map_err(|e| anyhow!("enabling test mode: {e}"))?;

    exec_lua(&rpc, "nx.test.reset()").await?;

    // Source each spec; a load error (syntax / top-level throw) is fatal for that
    // file but we report it and keep the others, so one bad spec doesn't blank the run.
    let mut load_errors = Vec::new();
    for file in &files {
        let code =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        if let Err(e) = exec_lua_checked(&rpc, &code).await {
            load_errors.push((file.clone(), e.to_string()));
        }
    }

    exec_lua(&rpc, "nx.test._run()").await?;

    // Poll for completion. Each request advances a server tick; the suite's awaited
    // ticks also fire autonomously on the server's timer loop between polls.
    let started = Instant::now();
    let results = loop {
        let done = exec_lua(&rpc, "return nx.test._done == true").await?;
        if matches!(done, Value::Boolean(true)) {
            break exec_lua(&rpc, "return nx.test._results").await?;
        }
        if started.elapsed() > SUITE_TIMEOUT {
            return Err(anyhow!(
                "test suite did not finish within {:?} (a stuck wait_for / await?)",
                SUITE_TIMEOUT
            ));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };

    Ok(report(&results, &load_errors))
}

/// `nvim_exec_lua(code)` returning its value; panics-to-error on a transport failure.
async fn exec_lua(rpc: &Rpc, code: &str) -> Result<Value> {
    rpc.request(
        "nvim_exec_lua",
        vec![Value::from(code), Value::Array(vec![])],
    )
    .await
    .map_err(|e| anyhow!("nvim_exec_lua failed: {e}"))
}

/// Like [`exec_lua`] but surfaces a *Lua* error (the server replies with an error
/// payload) as an `Err`, for sourcing spec files where a throw must be reported.
async fn exec_lua_checked(rpc: &Rpc, code: &str) -> Result<Value> {
    rpc.request(
        "nvim_exec_lua",
        vec![Value::from(code), Value::Array(vec![])],
    )
    .await
    .map_err(|e| anyhow!("{e}"))
}

// ----- reporting -------------------------------------------------------------

struct Case {
    path: Vec<String>,
    name: String,
    status: String,
    message: Option<String>,
    ms: i64,
}

/// Parse the msgpack results array into [`Case`]s.
fn parse(results: &Value) -> Vec<Case> {
    let Value::Array(items) = results else {
        return Vec::new();
    };
    items.iter().filter_map(parse_case).collect()
}

fn parse_case(item: &Value) -> Option<Case> {
    let map = item.as_map()?;
    let get = |key: &str| {
        map.iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v)
    };
    let path = match get("path") {
        Some(Value::Array(p)) => p
            .iter()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    Some(Case {
        path,
        name: get("name")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string(),
        status: get("status")
            .and_then(Value::as_str)
            .unwrap_or("error")
            .to_string(),
        message: get("message").and_then(Value::as_str).map(str::to_string),
        ms: get("ms").and_then(Value::as_i64).unwrap_or(0),
    })
}

/// Print the grouped report and return whether everything passed.
fn report(results: &Value, load_errors: &[(PathBuf, String)]) -> bool {
    let cases = parse(results);
    let mut passed = 0;
    let mut failed = 0;

    println!();
    let mut last_group = String::new();
    for case in &cases {
        let group = case.path.join(" › ");
        if group != last_group {
            println!("{group}");
            last_group = group;
        }
        match case.status.as_str() {
            "pass" => {
                passed += 1;
                println!("  \x1b[32m✓\x1b[0m {} ({}ms)", case.name, case.ms);
            }
            _ => {
                failed += 1;
                println!("  \x1b[31m✗\x1b[0m {} ({}ms)", case.name, case.ms);
                if let Some(msg) = &case.message {
                    for line in msg.lines() {
                        println!("      \x1b[31m{line}\x1b[0m");
                    }
                }
            }
        }
    }

    for (file, err) in load_errors {
        failed += 1;
        println!("\x1b[31m✗ failed to load {}\x1b[0m", file.display());
        for line in err.lines() {
            println!("      \x1b[31m{line}\x1b[0m");
        }
    }

    println!();
    let summary = format!("{passed} passed, {failed} failed");
    if failed == 0 {
        println!("\x1b[32m{summary}\x1b[0m");
    } else {
        println!("\x1b[31m{summary}\x1b[0m");
    }
    failed == 0 && load_errors.is_empty()
}
