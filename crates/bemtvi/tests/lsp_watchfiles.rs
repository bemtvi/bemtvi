//! Behavior tests for the **dynamic capability registration** half of the LSP
//! handshake: the `workspaceFolders` a server needs to find its workspace at all,
//! and `client/registerCapability` → `workspace/didChangeWatchedFiles`, which is how
//! a server learns about a file that changed outside the editor.
//!
//! All three are the kind of gap nothing downstream can detect: every layer works and
//! the feature simply never happens, while the server logs a warning nobody reads
//! (`LSP client does not support dynamic capability registration`, `Your LSP client
//! doesn't support file watching`, `File or directory "/<default workspace root>"
//! does not exist`). So the assertions are on the **wire** — what the editor actually
//! sent — read back from the mock's recording.
//!
//! Wired like `lsp_progress.rs`: the scripted mock server (`bemtvi --__lsp-mock`)
//! stands in for a real language server, `$BEMTVI_LSP_CMD` overrides the spawn argv,
//! and a `rust`-filetype buffer drives the dispatch. The process-global env means
//! these tests serialize on `serial_lock`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{attach, exec_lua, feed, serial_lock, spawn, temp_dir};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const BEMTVI_BIN: &str = env!("CARGO_BIN_EXE_bemtvi");

/// Write a mock LSP script and point `$BEMTVI_LSP_CMD` at the binary's `--__lsp-mock`
/// mode. The caller holds `serial_lock`.
fn arm_mock(dir: &Path, script: &str) {
    std::fs::write(dir.join("mock.json"), script).expect("write mock script");
    // SAFETY: serialized on `serial_lock`, so no other test races this env mutation.
    std::env::set_var(
        "BEMTVI_LSP_CMD",
        format!("{BEMTVI_BIN} --__lsp-mock {}/mock.json", dir.display()),
    );
}

/// Open a `.rs` buffer in `dir` and enable the mock server rooted **at `dir`** — an
/// explicit `root_dir` rather than a marker search, so the workspace under test is
/// the temp directory and nothing else (the watch tests arm a recursive watch on it).
async fn open_with_server(dir: &Path) -> (Rpc, UnboundedReceiver<Incoming>) {
    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, "fn main() {}\n").expect("write test file");
    let init = ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    feed(&rpc, "gg0");
    exec_lua(
        &rpc,
        &format!(
            r#"
            btv.lsp.config("mock", {{
              cmd = {{ "mock" }},
              filetypes = {{ "rust" }},
              root_dir = "{}",
            }})
            btv.lsp.enable({{ "mock" }})
            "#,
            dir.display()
        ),
    )
    .await;
    (rpc, incoming)
}

/// Every recorded `{method, params}` line so far (the mock appends one JSON object
/// per message it received, plus the synthetic `_*_response` records).
fn records(rec: &Path) -> Vec<Value> {
    std::fs::read_to_string(rec)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

/// Poll the recording until a line with `method` appears, and return it (or `None`
/// after ~5s — the caller asserts with the whole recording in the message).
async fn await_record(rec: &Path, method: &str) -> Option<Value> {
    for _ in 0..200 {
        if let Some(v) = records(rec)
            .into_iter()
            .find(|v| v.get("method").and_then(Value::as_str) == Some(method))
        {
            return Some(v);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    None
}

/// Every `{uri, type}` the editor has reported through `workspace/didChangeWatchedFiles`
/// so far, flattened across notifications.
fn watched_changes(rec: &Path) -> Vec<(String, i64)> {
    records(rec)
        .into_iter()
        .filter(|v| {
            v.get("method").and_then(Value::as_str) == Some("workspace/didChangeWatchedFiles")
        })
        .filter_map(|v| v.get("params")?.get("changes")?.as_array().cloned())
        .flatten()
        .filter_map(|c| {
            Some((
                c.get("uri")?.as_str()?.to_string(),
                c.get("type")?.as_i64()?,
            ))
        })
        .collect()
}

/// Create files in the watched root until one of them comes back as a `Created`
/// change, and return the one that did. The retry loop closes the arm race: the
/// registration is acked before `btv.fs.watch` has actually armed the inotify watch, so
/// a single write can land in the gap. Each attempt uses a **fresh name**, so the
/// change we assert on is genuinely a creation rather than a re-modification.
async fn create_until_reported(dir: &Path, rec: &Path, stem: &str, ext: &str) -> Option<PathBuf> {
    for attempt in 0..40 {
        let path = dir.join(format!("{stem}{attempt}.{ext}"));
        std::fs::write(&path, "x = 1\n").expect("write watched file");
        let uri = format!("file://{}", path.display());
        for _ in 0..8 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            if watched_changes(rec)
                .iter()
                .any(|(u, t)| *u == uri && *t == 1)
            {
                return Some(path);
            }
        }
    }
    None
}

/// The client must advertise the two capabilities a server reads before it will
/// bother with dynamic registration at all, and send its root as `workspaceFolders`.
///
/// `rootUri` alone is what produced `File or directory "/<default workspace root>"
/// does not exist` from basedpyright: it ignores the deprecated field, invents a
/// synthetic workspace, and analyses nothing — while every layer of bemtvi looks fine.
#[tokio::test]
async fn initialize_advertises_workspace_folders_and_dynamic_registration() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_watchfiles");
    let rec = dir.join("rec.jsonl");
    arm_mock(&dir, &format!(r#"{{ "record": "{}" }}"#, rec.display()));
    let (_rpc, _incoming) = open_with_server(&dir).await;

    let init = await_record(&rec, "initialize")
        .await
        .expect("the client must send `initialize`");
    let want_uri = Value::from(format!("file://{}", dir.display()));
    assert_eq!(
        init.pointer("/params/workspaceFolders/0/uri"),
        Some(&want_uri),
        "initialize must carry the root as a workspaceFolder; got: {init}"
    );
    assert_eq!(
        init.pointer("/params/rootUri"),
        Some(&want_uri),
        "the deprecated rootUri stays, for servers that read only it; got: {init}"
    );
    let caps = init
        .pointer("/params/capabilities/workspace")
        .cloned()
        .unwrap_or(Value::Null);
    for path in [
        "/workspaceFolders",
        "/didChangeConfiguration/dynamicRegistration",
        "/didChangeWatchedFiles/dynamicRegistration",
        "/didChangeWatchedFiles/relativePatternSupport",
    ] {
        assert_eq!(
            caps.pointer(path),
            Some(&Value::Bool(true)),
            "workspace capability {path} must be advertised; got: {caps}"
        );
    }
}

/// A server that trusts neither `rootUri` nor the pushed folders pulls the set with
/// `workspace/workspaceFolders`. Declaring the capability without answering the pull
/// is worse than not declaring it — the server gets method-not-found and ends up with
/// no workspace at all — so the answer is asserted on the wire.
#[tokio::test]
async fn the_workspace_folders_pull_is_answered_with_the_root() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_watchfiles");
    let rec = dir.join("rec.jsonl");
    arm_mock(
        &dir,
        &format!(
            r#"{{ "record": "{}", "workspace_folders_pull": true }}"#,
            rec.display()
        ),
    );
    let (_rpc, _incoming) = open_with_server(&dir).await;

    let reply = await_record(&rec, "_workspace_folders_response")
        .await
        .expect("the client must answer workspace/workspaceFolders");
    assert_eq!(
        reply.pointer("/params/0/uri"),
        Some(&Value::from(format!("file://{}", dir.display()))),
        "the pull must answer with the workspace root; got: {reply}"
    );
    assert_eq!(
        reply.pointer("/params/0/name"),
        dir.file_name()
            .map(|n| Value::from(n.to_string_lossy().into_owned()))
            .as_ref(),
        "the folder is named by its directory; got: {reply}"
    );
}

/// `client/registerCapability` must be **acked**. Before it was answered it fell to
/// async-lsp's method-not-found, which is exactly what a server reads as "this client
/// cannot do dynamic registration" — ruff then logs that automatic configuration
/// reloading is unavailable and never registers its watches.
#[tokio::test]
async fn a_dynamic_registration_is_acked_not_errored() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_watchfiles");
    let rec = dir.join("rec.jsonl");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "record": "{}",
                "register_capability": [
                    {{ "id": "r1", "method": "workspace/didChangeConfiguration" }}
                ]
            }}"#,
            rec.display()
        ),
    );
    let (_rpc, _incoming) = open_with_server(&dir).await;

    let reply = await_record(&rec, "_register_response")
        .await
        .expect("the client must answer client/registerCapability");
    assert!(
        reply.pointer("/params/error").is_none(),
        "a registration must be acked, not errored; got: {reply}"
    );
    assert!(
        reply.pointer("/params/result").is_some(),
        "a registration must be acked with a result; got: {reply}"
    );
}

/// The whole point of the feature: a file created on disk — by a `git checkout`, a
/// code generator, anything outside the editor — reaches the server as a `Created`
/// change, and a file the registration's glob does NOT name never does.
#[tokio::test]
async fn a_registered_glob_reports_a_file_created_on_disk() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_watchfiles");
    let rec = dir.join("rec.jsonl");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "record": "{}",
                "register_capability": [
                    {{ "id": "watch-py", "method": "workspace/didChangeWatchedFiles",
                       "registerOptions": {{ "watchers": [ {{ "globPattern": "**/*.py" }} ] }} }}
                ]
            }}"#,
            rec.display()
        ),
    );
    let (_rpc, _incoming) = open_with_server(&dir).await;
    await_record(&rec, "_register_response")
        .await
        .expect("the registration must be acked before the watch can arm");

    // A file the glob does NOT name, written alongside the ones it does.
    std::fs::write(dir.join("untouched.txt"), "not python\n").expect("write ignored file");
    let created = create_until_reported(&dir, &rec, "gen", "py")
        .await
        .unwrap_or_else(|| {
            panic!(
                "a created .py file must be reported as a watched change; recorded: {:?}",
                watched_changes(&rec)
            )
        });
    assert!(
        watched_changes(&rec)
            .iter()
            .any(|(u, t)| *u == format!("file://{}", created.display()) && *t == 1),
        "the creation must be reported with FileChangeType.Created"
    );
    assert!(
        !watched_changes(&rec)
            .iter()
            .any(|(u, _)| u.ends_with("untouched.txt")),
        "a file outside the registered glob must never be reported; recorded: {:?}",
        watched_changes(&rec)
    );
}

/// A deletion is reported as `Deleted`, not as a change. The change type is derived
/// from what is on disk rather than from the raw watcher event class — a coalesced
/// burst reports itself as a generic modify, and a server told "changed" about a file
/// that is gone re-reads it and errors.
#[tokio::test]
async fn a_deleted_file_is_reported_as_deleted() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_watchfiles");
    let rec = dir.join("rec.jsonl");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "record": "{}",
                "register_capability": [
                    {{ "id": "watch-py", "method": "workspace/didChangeWatchedFiles",
                       "registerOptions": {{ "watchers": [ {{ "globPattern": "**/*.py" }} ] }} }}
                ]
            }}"#,
            rec.display()
        ),
    );
    let (_rpc, _incoming) = open_with_server(&dir).await;
    await_record(&rec, "_register_response")
        .await
        .expect("the registration must be acked before the watch can arm");

    // Creating it first is also how we know the watch is armed — only then can the
    // deletion race nothing.
    let created = create_until_reported(&dir, &rec, "doomed", "py")
        .await
        .expect("the watch must report the creation first");
    let uri = format!("file://{}", created.display());
    std::fs::remove_file(&created).expect("remove watched file");
    for _ in 0..200 {
        if watched_changes(&rec)
            .iter()
            .any(|(u, t)| *u == uri && *t == 3)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "the deletion must be reported with FileChangeType.Deleted; recorded: {:?}",
        watched_changes(&rec)
    );
}

/// The whole chain over a **daemon session**, which is a tier-1 target, not a degraded
/// mode: the language server runs on the daemon (its stdio tunneled over the `lsp_*` leg)
/// and the watch is armed on the daemon too (the `luafs_watch` leg), so a file created on
/// the remote disk still comes back as a `Created` change.
///
/// Both seams are remote-*only* here — the editor has no local LSP child and, with
/// `fs_watch` injected, the event-loop actor never arms a local watcher — so a reported
/// change proves the registration crossed one leg and the change crossed the other. Before
/// the `luafs_watch` client leg existed this could not work at all: a daemon session armed
/// its watches on the local disk and never saw a remote change.
#[tokio::test]
async fn the_watch_chain_works_over_a_daemon_session() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_watchfiles_daemon");
    let rec = dir.join("rec.jsonl");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "record": "{}",
                "register_capability": [
                    {{ "id": "watch-py", "method": "workspace/didChangeWatchedFiles",
                       "registerOptions": {{ "watchers": [ {{ "globPattern": "**/*.py" }} ] }} }}
                ]
            }}"#,
            rec.display()
        ),
    );

    // The `luafs_watch` leg: watches arm on the daemon, changes push back.
    let (eh_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (daemon_reader, daemon_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = bemtvi_server::serve_luafs_watch_daemon(daemon_reader, daemon_writer).await;
    });
    let (host_reader, host_writer) = tokio::io::split(eh_end);

    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, "fn main() {}\n").expect("write test file");
    let (rpc, _incoming) = bemtvi_test_harness::spawn_with_daemon_lsp(ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        fs_watch: Some(bemtvi_server::RemoteFsWatch::connect(
            host_reader,
            host_writer,
        )),
        ..Default::default()
    })
    .await;
    feed(&rpc, "gg0");
    exec_lua(
        &rpc,
        &format!(
            r#"
            btv.lsp.config("mock", {{
              cmd = {{ "mock" }},
              filetypes = {{ "rust" }},
              root_dir = "{}",
            }})
            btv.lsp.enable({{ "mock" }})
            "#,
            dir.display()
        ),
    )
    .await;
    await_record(&rec, "_register_response")
        .await
        .expect("the registration must cross the daemon LSP leg and be acked");

    let created = create_until_reported(&dir, &rec, "remote", "py")
        .await
        .unwrap_or_else(|| {
            panic!(
                "a file created on the daemon's disk must be reported; recorded: {:?}",
                watched_changes(&rec)
            )
        });
    assert!(
        watched_changes(&rec)
            .iter()
            .any(|(u, t)| *u == format!("file://{}", created.display()) && *t == 1),
        "the remote creation must be reported with FileChangeType.Created"
    );
}

/// `client/unregisterCapability` tears the watch down. A registration that outlives
/// its unregister keeps a recursive filesystem watch on the whole workspace for the
/// rest of the session and keeps feeding a server that asked it to stop.
#[tokio::test]
async fn an_unregistered_capability_drops_its_watches() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_watchfiles");
    let rec = dir.join("rec.jsonl");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "record": "{}",
                "unregister_after_watch_events": 1,
                "register_capability": [
                    {{ "id": "watch-py", "method": "workspace/didChangeWatchedFiles",
                       "registerOptions": {{ "watchers": [ {{ "globPattern": "**/*.py" }} ] }} }}
                ]
            }}"#,
            rec.display()
        ),
    );
    let (rpc, _incoming) = open_with_server(&dir).await;
    await_record(&rec, "_register_response")
        .await
        .expect("the registration must be acked before the watch can arm");

    // The registration is live while the watch reports — that first report is what
    // makes the mock unregister.
    create_until_reported(&dir, &rec, "trigger", "py")
        .await
        .expect("the watch must report a creation to trigger the unregister");
    const COUNT: &str = r#"
        local n = 0
        for _, regs in pairs(btv.lsp._registrations) do
          for _ in pairs(regs) do n = n + 1 end
        end
        return n
    "#;
    for _ in 0..200 {
        if exec_lua(&rpc, COUNT).await.as_i64() == Some(0) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the unregistered capability must be dropped, watches and all");
}
