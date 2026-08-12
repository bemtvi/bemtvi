//! Multi-server LSP over the **daemon wire** — Phase 6 of
//! `docs/plans/2026-07-25-multi-server-lsp-attach.md`.
//!
//! The remote session is a tier-1 target: a buffer served by two language servers
//! has to behave identically whether their stdio is a local pipe or a tunnel to a
//! daemon. That is *plausible* by construction — the whole multi-server layer
//! (`LspDocState.servers`, the per-server pending map, the merged mirrors) lives in
//! `EditHost`, and both transports are already keyed by `ServerKey` — but "the design
//! says it should" is not a verification, so these drive it.
//!
//! Wiring: [`spawn_with_daemon_lsp`] injects a `RemoteLspTransport` talking to a
//! `serve_lsp_daemon` over an in-process duplex, so each mock server is a real child
//! held by the daemon side with its stdio streamed over the wire. `$BEMTVI_LSP_CMD_
//! <NAME>` points the two servers at different scripts (the blanket `$BEMTVI_LSP_CMD`
//! would aim both at one, and no assertion could tell which answered). The env is
//! process-global, so these serialize on `serial_lock`.

use std::path::Path;
use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{exec_lua, feed, lines, serial_lock, spawn_with_daemon_lsp, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

const BEMTVI_BIN: &str = env!("CARGO_BIN_EXE_bemtvi");

/// Point `$BEMTVI_LSP_CMD_<NAME>` at the mock with its own script.
fn arm_mock_named(dir: &Path, name: &str, script: &str) {
    let file = dir.join(format!("mock-{name}.json"));
    std::fs::write(&file, script).expect("write mock script");
    // SAFETY: serialized on `serial_lock`, so no other test races this env mutation.
    std::env::set_var(
        format!("BEMTVI_LSP_CMD_{}", name.to_uppercase()),
        format!("{BEMTVI_BIN} --__lsp-mock {}", file.display()),
    );
}

fn disarm_mocks() {
    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");
}

/// Poll `expr` until it equals `want`; returns whether it matched.
async fn await_lua_eq(rpc: &Rpc, expr: &str, want: &str) -> bool {
    let code = format!("return tostring({expr})");
    for _ in 0..200 {
        if exec_lua(rpc, &code).await.as_str() == Some(want) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// Poll `expr` until it contains `want`; returns the last value seen.
async fn await_lua_contains(rpc: &Rpc, expr: &str, want: &str) -> String {
    let code = format!("return tostring({expr})");
    let mut last = String::new();
    for _ in 0..200 {
        last = exec_lua(rpc, &code)
            .await
            .as_str()
            .unwrap_or_default()
            .to_string();
        if last.contains(want) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    last
}

/// Open a `.rs` buffer in a daemon-LSP session and enable both mock servers.
async fn start_two_over_daemon(dir: &Path, body: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, body).expect("write test file");
    let (rpc, incoming) = spawn_with_daemon_lsp(ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    })
    .await;
    // Cursor on `foo` so a hover has a symbol under it.
    feed(&rpc, "0fw");
    exec_lua(
        &rpc,
        "btv.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.enable({ 'alpha', 'beta' })",
    )
    .await;
    (rpc, incoming)
}

#[tokio::test]
async fn two_servers_attach_and_publish_over_the_daemon_wire() {
    // Both servers spawn on the daemon, both receive `didOpen` over the tunnel, and
    // both pushes land merged in the editor's diagnostic state. `publishDiagnostics`
    // is the sharpest probe available: it is a SERVER→client push that only a real
    // `didOpen` reaching that server can trigger, so seeing both messages proves each
    // server holds the document — not merely that two children were spawned.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("daemon-lsp-two");
    arm_mock_named(
        dir.as_path(),
        "alpha",
        r#"{ "diagnostics": [ { "range": { "start": { "line": 0, "character": 4 },
                                           "end": { "line": 0, "character": 7 } },
                               "severity": 1, "message": "diag-from-alpha" } ] }"#,
    );
    arm_mock_named(
        dir.as_path(),
        "beta",
        r#"{ "diagnostics": [ { "range": { "start": { "line": 0, "character": 10 },
                                           "end": { "line": 0, "character": 13 } },
                               "severity": 2, "message": "diag-from-beta" } ] }"#,
    );
    let (rpc, _incoming) = start_two_over_daemon(dir.as_path(), "let foo = bar()\n").await;

    let attached = await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await;
    let msgs = await_lua_contains(
        &rpc,
        "(function()\n\
         \x20 local out = {}\n\
         \x20 for _, d in ipairs(btv.diagnostic.get(0) or {}) do out[#out+1] = d.message end\n\
         \x20 table.sort(out)\n\
         \x20 return table.concat(out, ',')\n\
         end)()",
        "diag-from-beta",
    )
    .await;

    disarm_mocks();
    assert!(attached, "both servers attached over the daemon wire");
    assert_eq!(
        msgs, "diag-from-alpha,diag-from-beta",
        "both servers' pushed diagnostics merge in the editor's state over the wire"
    );
}

#[tokio::test]
async fn a_request_routes_by_capability_over_the_daemon_wire() {
    // The capability routing (Phase 3a) has to survive the tunnel: `alpha` sorts first
    // but withholds `hoverProvider`, so the hover must reach `beta`. A reply decoded
    // from the wrong server's tunnel would answer nothing (alpha's hover is scripted
    // but never asked for), so the assertion distinguishes the two.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("daemon-lsp-route");
    arm_mock_named(
        dir.as_path(),
        "alpha",
        r#"{ "capabilities": { "hoverProvider": false },
             "hover": { "contents": "FROM-ALPHA" } }"#,
    );
    arm_mock_named(
        dir.as_path(),
        "beta",
        r#"{ "hover": { "contents": "FROM-BETA" } }"#,
    );
    let (rpc, _incoming) = start_two_over_daemon(dir.as_path(), "let foo = bar()\n").await;

    assert!(
        await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await,
        "both servers attached over the daemon wire"
    );

    // `btv.lsp.hover()` resolves with the reply's markup; read it off the promise
    // rather than the float so this stays a pure wire assertion.
    exec_lua(
        &rpc,
        "btv._daemon_hover = nil\n\
         btv.lsp.hover():next(function(r) btv._daemon_hover = tostring(r) end)",
    )
    .await;
    let hover = await_lua_contains(&rpc, "btv._daemon_hover", "FROM-BETA").await;

    disarm_mocks();
    assert!(
        hover.contains("FROM-BETA"),
        "the hover reached the server that advertises it, over the wire: {hover:?}"
    );
    assert!(
        !hover.contains("FROM-ALPHA"),
        "and not the first server, which withholds hoverProvider: {hover:?}"
    );
}

#[tokio::test]
async fn a_server_initiated_apply_edit_lands_over_the_daemon_wire() {
    // `workspace/applyEdit` is the one inbound *request* the editor answers, and it
    // answers it a tick or more later — after the edit has reached the buffers. That
    // round trip runs through a different client on each transport (the async
    // `async-lsp` router natively, the `SyncLspClient` on the wasm/daemon leg), so the
    // wire leg has to be driven, not assumed: here the request arrives over the
    // tunnel, the edit applies locally, and the response has to travel back down the
    // same tunnel or the server would block forever.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("daemon-lsp-apply-edit");
    let record = dir.join("rec-alpha.jsonl");
    let uri = format!("file://{}", dir.join("a.rs").display());
    arm_mock_named(
        dir.as_path(),
        "alpha",
        &format!(
            r#"{{ "record": "{rec}",
                  "code_action": [ {{ "title": "Rewrite", "kind": "refactor",
                    "command": {{ "title": "run", "command": "alpha.rewrite" }} }} ],
                  "apply_edit": {{ "changes": {{ "{uri}": [ {{
                      "range": {{ "start": {{ "line": 0, "character": 4 }},
                                  "end": {{ "line": 0, "character": 7 }} }},
                      "newText": "bar" }} ] }} }} }}"#,
            rec = record.display(),
        ),
    );
    arm_mock_named(dir.as_path(), "beta", r#"{ "code_action": [] }"#);
    let (rpc, _incoming) = start_two_over_daemon(dir.as_path(), "let foo = 1\n").await;
    assert!(
        await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await,
        "both servers attached over the daemon wire"
    );

    // One action survives, so `apply` dispatches its command straight away; the mock
    // answers with the applyEdit push.
    exec_lua(&rpc, "btv.lsp.code_action({ apply = true })").await;

    let mut applied_in_buffer = false;
    for _ in 0..200 {
        if lines(&rpc).await == vec!["let bar = 1".to_string()] {
            applied_in_buffer = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // …and the server was answered down the tunnel it asked on.
    let mut answered = String::new();
    for _ in 0..200 {
        let content = std::fs::read_to_string(&record).unwrap_or_default();
        if let Some(line) = content
            .lines()
            .find(|l| l.contains("_apply_edit_response"))
            .map(str::to_string)
        {
            answered = line;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    disarm_mocks();
    assert!(
        applied_in_buffer,
        "the server-initiated edit must reach the buffer over the wire"
    );
    assert!(
        answered.contains(r#""applied":true"#),
        "the response must travel back down the tunnel: {answered:?}"
    );
}

/// The **spawn directory** crosses the wire. `cmd_cwd` (and, without it, the
/// editor's own cwd) is resolved editor-side and shipped to the daemon, which is the
/// only way a remote session lands the server in the same directory a local one
/// would: the daemon is stateless — it has no per-session cwd of its own — so a
/// value it doesn't receive silently becomes "wherever the daemon was launched".
#[cfg(unix)]
#[tokio::test]
async fn the_spawn_directory_reaches_the_daemon_side_child() {
    let _serial = serial_lock().lock().await;
    struct CwdGuard(std::path::PathBuf);
    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }
    let _cwd = CwdGuard(std::env::current_dir().expect("cwd"));

    let dir = temp_dir("daemon-lsp-cmd-cwd");
    let log = dir.as_path().join("rec.log");
    let pinned = dir.as_path().join("run-here");
    std::fs::create_dir(&pinned).expect("create the pinned dir");
    // A "server" that records where it was launched, then blocks on stdin so it stays
    // alive (a dead child would be respawned and double the record).
    let script = dir.as_path().join("recorder.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"CWD=$(pwd)\" >> '{}'\ncat >/dev/null\n",
            log.display()
        ),
    )
    .expect("write recorder");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // The daemon side spawns from THIS process (in-process duplex), so put its cwd
    // somewhere else: a recorded `run-here` can then only have come over the wire.
    let elsewhere = temp_dir("daemon-lsp-cmd-cwd-elsewhere");
    std::env::set_current_dir(elsewhere.as_path()).expect("cd elsewhere");

    let file_path = dir.as_path().join("a.rs");
    std::fs::write(&file_path, "let foo = 1\n").expect("write test file");
    let (rpc, _incoming) = spawn_with_daemon_lsp(ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    })
    .await;
    exec_lua(
        &rpc,
        &format!(
            "btv.lsp.config('rec', {{ cmd = {{ '{}' }}, filetypes = {{ 'rust' }},\n\
               cmd_cwd = '{}' }})\n\
             btv.lsp.enable('rec')",
            script.display(),
            pinned.display()
        ),
    )
    .await;

    let mut recorded = String::new();
    for _ in 0..200 {
        recorded = std::fs::read_to_string(&log).unwrap_or_default();
        if !recorded.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let want = std::fs::canonicalize(&pinned).expect("canonicalize the pinned dir");
    assert_eq!(
        recorded.trim(),
        format!("CWD={}", want.display()),
        "the remote child must run in the editor-resolved directory"
    );
}

// ---------------------------------------------------------------------------
// Reconnect: the LSP leg over a link that drops and comes back.
// ---------------------------------------------------------------------------

/// The future one re-dial produces: the edit-host end of a fresh duplex.
type DialFut =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<DialEnds>> + Send>>;
type DialEnds = (
    tokio::io::ReadHalf<tokio::io::DuplexStream>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
);

/// Stands up a **fully multiplexed** daemon (`run_daemon_io` — fs *and* `lsp_*` over one
/// ordered stream, what `--connect-daemon` really talks to) per dial, and remembers the
/// task so the test can sever it. The reconnect suite's [`Dialer`] serves only the fs leg;
/// this one has to carry LSP, since the resync being tested is the LSP one.
#[derive(Clone)]
struct LspDialer {
    daemons: std::sync::Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl LspDialer {
    fn new() -> Self {
        LspDialer {
            daemons: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Abort the live daemon — the edit-host sees EOF and the supervisor reacts as if the
    /// network dropped, taking the remote language-server children with it.
    fn sever(&self) {
        if let Some(h) = self.daemons.lock().unwrap().last() {
            h.abort();
        }
    }

    fn make(&self) -> impl FnMut() -> DialFut + Send + 'static {
        let this = self.clone();
        move || {
            let this = this.clone();
            Box::pin(async move {
                let (eh_end, daemon_end) = tokio::io::duplex(1 << 16);
                let (dr, dw) = tokio::io::split(daemon_end);
                let h = tokio::spawn(async move {
                    let _ = bemtvi_server::run_daemon_io(dr, dw).await;
                });
                this.daemons.lock().unwrap().push(h);
                let (er, ew) = tokio::io::split(eh_end);
                Ok((er, ew))
            })
        }
    }
}

/// A snappy retry policy so the re-dial lands in milliseconds under test.
fn fast_policy() -> bemtvi_server::ReconnectPolicy {
    bemtvi_server::ReconnectPolicy {
        max_attempts: 5,
        base: Duration::from_millis(20),
        cap: Duration::from_millis(60),
    }
}

/// The ids of every client `btv.lsp.clients()` lists, sorted — the shape that shows a
/// stale handle, which a bare count would too if it were the *only* symptom, but the ids
/// also say *which* client survived.
const CLIENT_IDS: &str = r#"
(function()
  local ids = {}
  for _, c in ipairs(btv.lsp.clients()) do ids[#ids + 1] = c.id end
  table.sort(ids)
  return table.concat(ids, ",")
end)()
"#;

/// A dropped link must leave exactly **one live client**: the respawn, with the pre-drop
/// one retired.
///
/// The retirement is driven by an exit no process reported — the demux clears its inflight
/// map on the drop, dropping the `exit_tx` so `RemoteLspProcess::wait` resolves
/// `(None, None)` and the manager raises `ServerExited`. That synthetic exit is what keeps
/// `resync_lsp_after_reconnect` — the one teardown that drops a server record *without*
/// `retire_lsp_server` — from ever meeting a live record. Lose it and the resync forgets
/// the server instead of retiring it: the client id goes in Rust, but `btv.lsp._clients` is
/// a mirror the server has to *tell*, so the dead handle would stay and the re-`ensure`'s
/// fresh `Initialized` would add a second one — a buffer reporting two servers with one
/// process running, the failure `retire_lsp_server`'s own doc comment describes for
/// `:LspStop`. (The browser twin, where the exit is synthesized furthest from any process
/// and the same mutation reproduces that leak, is `web/verify-lsp-reconnect.mjs`.)
#[tokio::test]
async fn a_reconnect_leaves_exactly_one_live_client() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("daemon_lsp_reconnect");
    arm_mock_named(&dir, "alpha", "{}");
    let file = dir.join("a.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write test file");

    let dialer = LspDialer::new();
    let (client, handle) =
        bemtvi_server::connect_daemon_reconnecting_on(dialer.make(), fast_policy())
            .await
            .expect("the initial daemon dial succeeds");
    let bemtvi_server::DaemonClient {
        host_fs,
        lsp_transport,
        ..
    } = client;
    let (rpc, _incoming) = bemtvi_test_harness::spawn(ServerInit {
        file: Some(file.to_string_lossy().into_owned()),
        host_fs_async: Some(Box::new(host_fs)),
        lsp_transport: Some(Box::new(lsp_transport)),
        daemon_link: Some(handle),
        ..Default::default()
    });
    bemtvi_test_harness::attach(&rpc, 80, 24).await;
    exec_lua(
        &rpc,
        "btv.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.enable({ 'alpha' })",
    )
    .await;
    assert!(
        await_lua_eq(&rpc, CLIENT_IDS, "1").await,
        "one client should be attached before the link drops"
    );

    // Sever: the remote server child dies with the daemon, and the supervisor re-dials
    // and resyncs the LSP leg against the fresh link.
    dialer.sever();

    // The respawn mints client id 2. A leaked handle shows up as `1,2` — the dead
    // pre-drop client listed alongside the live one.
    let ids = await_lua_eq(&rpc, CLIENT_IDS, "2").await;
    let last = exec_lua(&rpc, &format!("return tostring({CLIENT_IDS})"))
        .await
        .as_str()
        .unwrap_or_default()
        .to_string();
    disarm_mocks();
    assert!(
        ids,
        "after the reconnect only the respawned client should be listed, got {last:?}"
    );
}
