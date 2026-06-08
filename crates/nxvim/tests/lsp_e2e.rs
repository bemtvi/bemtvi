//! End-to-end LSP suite against the **real** language-server binaries, configured
//! entirely through the **vendored nvim-lspconfig** (`vendor/nvim-lspconfig`).
//!
//! This is the test that answers "does nxvim's nvim-lspconfig support actually
//! work?" with real servers rather than a scripted mock. For each of the ten most
//! popular servers it:
//!   1. lays down a real mini-project (the committed `tests/fixtures/lsp-e2e/<server>`)
//!      containing a *deliberate* error,
//!   2. points the server runtimepath at `vendor/nvim-lspconfig` and sources an
//!      `init.lua` that does nothing but `vim.lsp.enable('<server>')` (+ optional
//!      `settings`), so the **vendored** `lsp/<server>.lua` resolves the `cmd`,
//!      `filetypes`, and root — exactly as a user's config would,
//!   3. opens the file with the real server (found on `$PATH`) and waits for the
//!      real `textDocument/publishDiagnostics` to surface in a `redraw`.
//!
//! A diagnostic arriving is end-to-end proof of the whole round trip:
//! `initialize` → `textDocument/didOpen` → the server analysing real code →
//! `publishDiagnostics` → nxvim projecting it into the view. If the vendored config
//! could not resolve a `cmd`/root, or nxvim could not speak the protocol, no
//! diagnostic would ever appear and the case **fails loudly** (the project's
//! no-silent-stubs rule applies to the test too).
//!
//! ## This suite does not run by default
//!
//! It is gated behind `NXVIM_LSP_E2E=1` *and* needs the real servers installed:
//!
//! ```sh
//! scripts/lsp-e2e/lsp-e2e.sh install      # download + hash-verify all 10 servers
//! export PATH="$PWD/.lsp-e2e/bin:$PATH"   # (the script prints this line)
//! NXVIM_LSP_E2E=1 cargo test -p nxvim --test lsp_e2e -- --nocapture --test-threads=1
//! ```
//!
//! Without `NXVIM_LSP_E2E=1` every case is a passing no-op, so a plain
//! `cargo test --workspace` never spawns a real server. The cases share global
//! process env (PATH / NXVIM_LSP_* ) and each drives a heavyweight server, so they
//! serialize on a single lock; pass `--test-threads=1` to keep the output readable.
//!
//! ## Treesitter grammars are intentionally NOT required
//!
//! The syntax worker loads grammars at runtime and degrades gracefully when one is
//! missing (it emits a `ts_error` and moves on — see `nxvim_ts::loader`), and the
//! LSP path is independent of it. So a CI host with zero grammars installed runs
//! this suite fine; the assertions are all on LSP diagnostics, never highlights.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
use nxvim_test_harness::{drain_latest_redraw, message_of, serial_lock as lock};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const COLS: u16 = 80;
const ROWS: u16 = 24;

// ----- gating ---------------------------------------------------------------

/// The suite only runs when explicitly opted in. Returns false (→ the case is a
/// passing no-op) otherwise.
fn e2e_enabled() -> bool {
    matches!(std::env::var("NXVIM_LSP_E2E").as_deref(), Ok("1"))
}

/// The directory the installed servers live in (`scripts/lsp-e2e/lsp-e2e.sh`
/// installs here; `NXVIM_LSP_E2E_DIR` overrides the prefix to match the script).
fn server_bin_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("NXVIM_LSP_E2E_DIR") {
        return PathBuf::from(dir).join("bin");
    }
    repo_root().join(".lsp-e2e/bin")
}

/// The repo root, derived from this crate's manifest dir (`crates/nxvim`).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("canonicalize repo root")
}

/// The checked-out nvim-lspconfig submodule; panics (when enabled) if it isn't
/// populated, since the whole point is to drive the *vendored* configs.
fn lspconfig_rtp() -> PathBuf {
    let dir = repo_root().join("vendor/nvim-lspconfig");
    assert!(
        dir.join("lsp/rust_analyzer.lua").is_file(),
        "vendor/nvim-lspconfig is not checked out — run\n  \
         git submodule update --init vendor/nvim-lspconfig"
    );
    dir
}

/// Committed fixtures root: `crates/nxvim/tests/fixtures/lsp-e2e`.
fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lsp-e2e")
}

/// Prepend the installed-server bin dir to `$PATH` (idempotently) so the vendored
/// configs' bare commands (`rust-analyzer`, `gopls`, …) resolve to the pinned,
/// hash-verified binaries the install script laid down. Panics with install
/// instructions if the bin dir is missing.
fn ensure_servers_on_path() {
    let bin = server_bin_dir();
    assert!(
        bin.is_dir(),
        "servers not installed at {} — run\n  scripts/lsp-e2e/lsp-e2e.sh install",
        bin.display()
    );
    static ORIGINAL: OnceLock<std::ffi::OsString> = OnceLock::new();
    let original = ORIGINAL.get_or_init(|| std::env::var_os("PATH").unwrap_or_default());
    let mut paths = vec![bin.clone()];
    paths.extend(std::env::split_paths(original));
    let joined = std::env::join_paths(paths).expect("join PATH");
    std::env::set_var("PATH", joined);
}

/// Assert a specific server binary is on `$PATH`, with a pointer to the installer.
fn require_server(bin: &str) {
    let found = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|p| p.join(bin).exists());
    assert!(
        found,
        "language server '{bin}' not found on PATH — run\n  scripts/lsp-e2e/lsp-e2e.sh install"
    );
}

// ----- per-server case description ------------------------------------------

/// One server's end-to-end case.
struct Case {
    /// The nvim-lspconfig config name (and fixture sub-directory), e.g. `pyright`.
    config: &'static str,
    /// The launcher binary that must be present on PATH (e.g. `pyright-langserver`).
    bin: &'static str,
    /// The source file (relative to the fixture dir) to open and diagnose.
    open: &'static str,
    /// Empty directories to create in the prepared project as root markers (e.g.
    /// `.git` for servers whose only `root_markers` entry is `.git`).
    marker_dirs: &'static [&'static str],
    /// Optional Lua merged into the config via `vim.lsp.config(config, { … })`
    /// before `enable` — used to trim slow/irrelevant features.
    settings_lua: &'static str,
    /// How long to wait for the first real diagnostic. Heavy servers
    /// (rust-analyzer/gopls index or build on first open) get a larger budget.
    budget: Duration,
}

// ----- project preparation --------------------------------------------------

/// Copy the committed fixture for `case.config` into a fresh temp dir (so the
/// server can write caches, and so root markers resolve to the temp dir rather
/// than walking up into the nxvim repo), create any extra marker dirs, and return
/// `(project_root, opened_file_abs)`.
fn prepare_project(case: &Case) -> (PathBuf, PathBuf) {
    let src = fixtures_root().join(case.config);
    assert!(
        src.is_dir(),
        "missing fixture for {}: {}",
        case.config,
        src.display()
    );
    let root = std::env::temp_dir().join(format!(
        "nxvim-lsp-e2e-{}-{}",
        std::process::id(),
        case.config
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create project root");
    // Canonicalize so the buffer's `file://` URI matches what path-canonicalizing
    // servers (clangd, gopls, rust-analyzer) report in `publishDiagnostics`. On
    // macOS the temp dir is `/var/folders/…`, a symlink to `/private/var/…`, and
    // diagnostics are routed by exact URI; without this the canonical-URI
    // diagnostics would never map back to the buffer. (A real user opens a real
    // path, so this only papers over the temp-dir symlink, not a server bug.)
    let root = root.canonicalize().expect("canonicalize project root");
    // `cp -R <src>/. <root>` copies the fixture *contents* (dotfiles included).
    let status = std::process::Command::new("cp")
        .arg("-R")
        .arg(format!("{}/.", src.display()))
        .arg(&root)
        .status()
        .expect("spawn cp");
    assert!(status.success(), "cp fixture failed for {}", case.config);
    for dir in case.marker_dirs {
        std::fs::create_dir_all(root.join(dir)).expect("create marker dir");
    }
    let opened = root.join(case.open);
    assert!(
        opened.is_file(),
        "opened file missing: {}",
        opened.display()
    );
    (root, opened)
}

/// Write a config dir whose `init.lua` enables `case.config` through the vendored
/// nvim-lspconfig (optionally merging `settings_lua` first), and return it.
fn write_config_dir(case: &Case) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nxvim-lsp-e2e-cfg-{}-{}",
        std::process::id(),
        case.config
    ));
    std::fs::create_dir_all(&dir).expect("create config dir");
    let mut init = String::new();
    if !case.settings_lua.trim().is_empty() {
        init.push_str(&format!(
            "vim.lsp.config('{}', {})\n",
            case.config, case.settings_lua
        ));
    }
    init.push_str(&format!("vim.lsp.enable('{}')\n", case.config));
    std::fs::write(dir.join("init.lua"), init).expect("write init.lua");
    dir
}

/// Per-case env: a debug LSP log to a temp file (surfaced on failure), the syntax
/// worker pointed at the real binary (so opening the file doesn't try to re-spawn
/// the test executable as a ts worker), and no command/root overrides so the
/// vendored config alone decides them.
fn configure_env(case: &Case) -> PathBuf {
    let log = std::env::temp_dir().join(format!(
        "nxvim-lsp-e2e-{}-{}.log",
        std::process::id(),
        case.config
    ));
    let _ = std::fs::remove_file(&log);
    std::env::set_var("NXVIM_LSP_LOG_FILE", &log);
    std::env::set_var("NXVIM_LSP_LOG_LEVEL", "debug");
    std::env::set_var("NXVIM_TS_WORKER", env!("CARGO_BIN_EXE_nxvim"));
    std::env::remove_var("NXVIM_LSP_CMD");
    std::env::remove_var("NXVIM_LSP_ROOT");
    log
}

// ----- server harness (mirrors tests/lspconfig.rs) --------------------------

async fn start(file: PathBuf, config_dir: PathBuf) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (server_end, client_end) = tokio::io::duplex(1 << 16);
    let file = file.to_string_lossy().into_owned();
    let rtp = lspconfig_rtp();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("server runtime");
        let _ = runtime.block_on(run_server(
            server_end,
            ServerInit {
                file: Some(file),
                config_dir: Some(config_dir),
                runtimepath: vec![rtp],
                ..Default::default()
            },
        ));
    });
    let (reader, writer) = tokio::io::split(client_end);
    let (rpc, incoming) = connect(reader, writer);
    rpc.request(
        "nvim_ui_attach",
        vec![
            Value::from(COLS as u64),
            Value::from((ROWS - 2) as u64),
            Value::Map(vec![]),
        ],
    )
    .await
    .expect("ui attach");
    (rpc, incoming)
}

/// Force the server to process pending input and emit a redraw (drives LSP doc
/// sync), giving the server a chance to send/receive the next message.
async fn barrier(rpc: &Rpc) {
    rpc.request(
        "nvim_buf_get_lines",
        vec![
            Value::from(0u64),
            Value::from(0i64),
            Value::from(-1i64),
            Value::Boolean(false),
        ],
    )
    .await
    .expect("barrier");
}

/// Whether a `redraw` carries at least one non-empty `diagnostics` row. The
/// per-row `diagnostics` now live under the first window (`windows[0]`).
fn has_diagnostics(params: &[Value]) -> bool {
    let Some(Value::Map(map)) = params.first() else {
        return false;
    };
    let Some(Value::Map(win)) = map
        .iter()
        .find(|(k, _)| k.as_str() == Some("windows"))
        .and_then(|(_, v)| v.as_array())
        .and_then(|w| w.first())
    else {
        return false;
    };
    let Some((_, Value::Array(rows))) = win.iter().find(|(k, _)| k.as_str() == Some("diagnostics"))
    else {
        return false;
    };
    rows.iter()
        .any(|row| row.as_array().is_some_and(|spans| !spans.is_empty()))
}

// ----- the shared case runner -----------------------------------------------

/// Run one server's case to completion or fail loudly. Skips (passing no-op) when
/// the suite is not enabled.
async fn run_case(case: Case) {
    if !e2e_enabled() {
        eprintln!(
            "[lsp_e2e] skipping {} — set NXVIM_LSP_E2E=1 (and run scripts/lsp-e2e/lsp-e2e.sh install) to enable",
            case.config
        );
        return;
    }
    let _guard = lock().lock().await;
    ensure_servers_on_path();
    require_server(case.bin);

    let log = configure_env(&case);
    let (_root, opened) = prepare_project(&case);
    let config_dir = write_config_dir(&case);

    eprintln!(
        "[lsp_e2e] {}: opening {} (budget {:?})",
        case.config,
        opened.display(),
        case.budget
    );
    let (rpc, mut incoming) = start(opened.clone(), config_dir).await;

    // Poll until the real server publishes a diagnostic for the deliberate error.
    let deadline = Instant::now() + case.budget;
    let mut last_msg = String::new();
    while Instant::now() < deadline {
        barrier(&rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(&mut incoming) {
            let msg = message_of(&params);
            if !msg.is_empty() {
                last_msg = msg;
            }
            if has_diagnostics(&params) {
                eprintln!(
                    "[lsp_e2e] {}: ✓ diagnostic surfaced{}",
                    case.config,
                    if last_msg.is_empty() {
                        String::new()
                    } else {
                        format!(" — message line: {last_msg:?}")
                    }
                );
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Fail loudly with the server log so the gap is diagnosable.
    let log_tail = std::fs::read_to_string(&log)
        .map(|s| {
            let lines: Vec<&str> = s.lines().collect();
            let start = lines.len().saturating_sub(40);
            lines[start..].join("\n")
        })
        .unwrap_or_else(|_| "<no log>".into());
    panic!(
        "{}: no diagnostic from the real server within {:?}.\n\
         This means the vendored nvim-lspconfig config did not drive a working server \
         end-to-end (cmd/root not resolved, server not speaking, or no diagnostic produced).\n\
         last message line: {:?}\n\
         ---- lsp log tail ({}) ----\n{}",
        case.config,
        case.budget,
        last_msg,
        log.display(),
        log_tail
    );
}

// ----- the ten cases --------------------------------------------------------

#[tokio::test]
async fn pyright_diagnoses_real_python() {
    run_case(Case {
        config: "pyright",
        bin: "pyright-langserver",
        open: "src/main.py",
        marker_dirs: &[],
        settings_lua: "",
        budget: Duration::from_secs(60),
    })
    .await;
}

#[tokio::test]
async fn ts_ls_diagnoses_real_typescript() {
    run_case(Case {
        config: "ts_ls",
        bin: "typescript-language-server",
        open: "src/index.ts",
        marker_dirs: &[".git"],
        settings_lua: "",
        budget: Duration::from_secs(90),
    })
    .await;
}

#[tokio::test]
async fn lua_ls_diagnoses_real_lua() {
    run_case(Case {
        config: "lua_ls",
        bin: "lua-language-server",
        open: "src/init.lua",
        marker_dirs: &[],
        settings_lua: "",
        budget: Duration::from_secs(90),
    })
    .await;
}

#[tokio::test]
async fn rust_analyzer_diagnoses_real_rust() {
    run_case(Case {
        config: "rust_analyzer",
        bin: "rust-analyzer",
        open: "src/main.rs",
        marker_dirs: &[],
        // The deliberate error is syntactic (see the fixture), so it surfaces from
        // parsing without `cargo check`; disable check/proc-macro/build-scripts to
        // keep the case fast and offline.
        settings_lua: "{ settings = { ['rust-analyzer'] = { \
            checkOnSave = false, \
            cargo = { buildScripts = { enable = false } }, \
            procMacro = { enable = false } } } }",
        budget: Duration::from_secs(90),
    })
    .await;
}

#[tokio::test]
async fn gopls_diagnoses_real_go() {
    run_case(Case {
        config: "gopls",
        bin: "gopls",
        open: "main.go",
        marker_dirs: &[],
        settings_lua: "",
        budget: Duration::from_secs(150),
    })
    .await;
}

#[tokio::test]
async fn clangd_diagnoses_real_c() {
    run_case(Case {
        config: "clangd",
        bin: "clangd",
        open: "src/main.c",
        marker_dirs: &[],
        settings_lua: "",
        budget: Duration::from_secs(90),
    })
    .await;
}

#[tokio::test]
async fn bashls_diagnoses_real_bash() {
    run_case(Case {
        config: "bashls",
        bin: "bash-language-server",
        open: "script.sh",
        // bashls' only root marker is `.git`; create one so the root resolves to
        // the temp project (and shellcheck — required for diagnostics — runs).
        marker_dirs: &[".git"],
        settings_lua: "",
        budget: Duration::from_secs(90),
    })
    .await;
}

#[tokio::test]
async fn jsonls_diagnoses_real_json() {
    run_case(Case {
        config: "jsonls",
        bin: "vscode-json-language-server",
        open: "data.json",
        marker_dirs: &[],
        settings_lua: "",
        budget: Duration::from_secs(45),
    })
    .await;
}

#[tokio::test]
async fn yamlls_diagnoses_real_yaml() {
    run_case(Case {
        config: "yamlls",
        bin: "yaml-language-server",
        open: "config.yaml",
        marker_dirs: &[],
        settings_lua: "",
        budget: Duration::from_secs(60),
    })
    .await;
}

#[tokio::test]
async fn zls_diagnoses_real_zig() {
    run_case(Case {
        config: "zls",
        bin: "zls",
        open: "src/main.zig",
        marker_dirs: &[],
        settings_lua: "",
        budget: Duration::from_secs(60),
    })
    .await;
}
