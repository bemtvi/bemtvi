//! Behavior tests for LSP **work-done progress** (`$/progress`): the chain from a
//! server's begin/report/end stream, through the editor's per-token store, to
//! `btv.lsp.progress()` and the `LspProgress` autocmd.
//!
//! Wired like `lsp_features.rs`: the scripted mock server (`bemtvi --__lsp-mock`)
//! stands in for a real language server, `$BEMTVI_LSP_CMD` overrides the spawn argv,
//! and a `rust`-filetype buffer drives the dispatch. The mock replays its `progress`
//! script on the first `didOpen`, so a script that stops at a `report` leaves the
//! task *running* and observable, while one that includes its `end` leaves the store
//! empty. The process-global env means these tests serialize on `serial_lock`.

use std::path::Path;
use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{attach, exec_lua, feed, serial_lock, spawn, temp_dir};
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

/// Open a `.rs` buffer (filetype `rust`), attach, run `setup` (Lua evaluated
/// *before* the server is enabled — where an `LspProgress` autocmd has to be
/// registered to see the first `begin`), then enable the mock server.
async fn open_with_server(dir: &Path, setup: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, "fn main() {}\n").expect("write test file");
    let init = ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    feed(&rpc, "gg0");
    if !setup.is_empty() {
        exec_lua(&rpc, setup).await;
    }
    exec_lua(
        &rpc,
        r#"
        btv.lsp.config("mock", { cmd = { "mock" }, filetypes = { "rust" } })
        btv.lsp.enable({ "mock" })
        "#,
    )
    .await;
    (rpc, incoming)
}

/// Poll `expr` (a `return`-ed Lua expression) until its string form equals `want`.
/// Returns the last value seen, so a failure message can show what it settled on.
async fn await_lua_eq(rpc: &Rpc, expr: &str, want: &str) -> String {
    let code = format!("return tostring({expr})");
    let mut last = String::new();
    for _ in 0..200 {
        last = exec_lua(rpc, &code)
            .await
            .as_str()
            .unwrap_or_default()
            .to_string();
        if last == want {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    last
}

/// A one-line rendering of `btv.lsp.progress()` — `client|title|message|percentage`
/// per task, `-` for an absent field. One string keeps the polling helper simple and
/// makes a failure show the whole state rather than one field of it.
const RENDER: &str = r#"
(function()
  local out = {}
  for _, p in ipairs(btv.lsp.progress()) do
    out[#out + 1] = table.concat({
      p.client_name, p.title, p.message or "-", p.percentage and tostring(p.percentage) or "-",
    }, "|")
  end
  table.sort(out)
  return table.concat(out, " ; ")
end)()
"#;

/// The client must **advertise** `window.workDoneProgress` at `initialize`: a
/// conforming server sends no `$/progress` at all without it. Nothing downstream can
/// detect that — every layer works and simply never receives anything — so the
/// declaration is asserted on the wire the editor actually sent.
#[tokio::test]
async fn initialize_advertises_window_work_done_progress() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_progress");
    let rec = dir.join("rec.jsonl");
    arm_mock(&dir, &format!(r#"{{ "record": "{}" }}"#, rec.display()));
    let (rpc, _incoming) = open_with_server(&dir, "").await;

    // Wait for the LSP client to attach before reading the recorded wire. The
    // handshake (initialize → initialized) completes before `LspAttach` fires, so
    // once the client is visible the `initialize` message is guaranteed to be on
    // disk. Without this barrier the test races the mock process start and flakes
    // under load (the polling loop below can exhaust its tries before the process
    // has even spawned).
    assert_eq!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "1",
        "the mock client should be attached before we read its recorded initialize"
    );

    let recorded = std::fs::read_to_string(&rec).unwrap_or_default();
    let init = recorded
        .lines()
        .find(|l| l.contains("\"initialize\""))
        .unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(init).unwrap_or(serde_json::Value::Null);
    assert_eq!(
        v.pointer("/params/capabilities/window/workDoneProgress"),
        Some(&serde_json::Value::Bool(true)),
        "initialize must declare window.workDoneProgress; got: {init}"
    );
}

/// A `begin` followed by a `report` that never ends leaves the task listed, with the
/// report's newer message and percentage folded onto the begin's title. This is the
/// state a statusline spends a long index in.
#[tokio::test]
async fn a_running_task_is_listed_with_its_latest_report() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_progress");
    arm_mock(
        &dir,
        r#"{
            "progress": [
                { "token": "t1", "value": {
                    "kind": "begin", "title": "Indexing", "message": "0/10", "percentage": 0 } },
                { "token": "t1", "value": {
                    "kind": "report", "message": "3/10", "percentage": 30 } }
            ]
        }"#,
    );
    let (rpc, _incoming) = open_with_server(&dir, "").await;

    assert_eq!(
        await_lua_eq(&rpc, RENDER, "mock|Indexing|3/10|30").await,
        "mock|Indexing|3/10|30",
        "the running task should be listed with the report's message and percentage"
    );
}

/// **The sticky-field rule.** A `report` carrying only a `percentage` means "the
/// title and message I sent before still stand" — not "clear them". A store that
/// overwrote per update would blank the title on the very first report, which is the
/// frame that renders for most of a long task.
#[tokio::test]
async fn a_report_keeps_the_fields_it_did_not_carry() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_progress");
    arm_mock(
        &dir,
        r#"{
            "progress": [
                { "token": "t1", "value": {
                    "kind": "begin", "title": "Loading workspace",
                    "message": "reading manifests", "percentage": 5 } },
                { "token": "t1", "value": { "kind": "report", "percentage": 40 } }
            ]
        }"#,
    );
    let (rpc, _incoming) = open_with_server(&dir, "").await;

    let want = "mock|Loading workspace|reading manifests|40";
    assert_eq!(
        await_lua_eq(&rpc, RENDER, want).await,
        want,
        "a report with only a percentage must keep the begin's title and message"
    );
}

/// `end` retires the task: `btv.lsp.progress()` means "busy right now", so a finished
/// task is gone rather than parked at 100%.
#[tokio::test]
async fn an_ended_task_leaves_no_progress() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_progress");
    arm_mock(
        &dir,
        r#"{
            "progress": [
                { "token": "t1", "value": { "kind": "begin", "title": "Indexing" } },
                { "token": "t1", "value": { "kind": "report", "percentage": 90 } },
                { "token": "t1", "value": { "kind": "end", "message": "done" } }
            ]
        }"#,
    );
    // Count the updates so the empty-store assertion below can't pass trivially by
    // running before anything arrived at all.
    let (rpc, _incoming) = open_with_server(
        &dir,
        r#"
        _G.seen_progress = 0
        btv.autocmd.create("LspProgress", { callback = function()
          _G.seen_progress = _G.seen_progress + 1
        end })
        "#,
    )
    .await;

    // The `end` is the last thing the server says, so wait for the whole sequence to
    // have landed before asserting the store is empty.
    assert_eq!(
        await_lua_eq(&rpc, "_G.seen_progress or 0", "3").await,
        "3",
        "all three progress updates should have been delivered"
    );
    assert_eq!(
        await_lua_eq(&rpc, "#btv.lsp.progress()", "0").await,
        "0",
        "an ended task should be dropped, not left at its last percentage"
    );
}

/// Several tokens run at once (rust-analyzer routinely does), and ending one leaves
/// the other running — the store is per-(client, token), not a single slot.
#[tokio::test]
async fn concurrent_tokens_are_tracked_independently() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_progress");
    arm_mock(
        &dir,
        r#"{
            "progress": [
                { "token": "t1", "value": { "kind": "begin", "title": "Indexing" } },
                { "token": "t2", "value": { "kind": "begin", "title": "Building" } },
                { "token": "t1", "value": { "kind": "report", "percentage": 50 } },
                { "token": "t1", "value": { "kind": "end" } }
            ]
        }"#,
    );
    let (rpc, _incoming) = open_with_server(&dir, "").await;

    let want = "mock|Building|-|-";
    assert_eq!(
        await_lua_eq(&rpc, RENDER, want).await,
        want,
        "ending one token should leave the other's task untouched"
    );
}

/// `LspProgress` fires once per update, with the update's **kind as the autocmd
/// pattern** (neovim's contract) and the whole payload as `args.data` — so
/// `pattern = "end"` narrows to completions and `data.client_id` resolves through
/// `btv.lsp.clients()`.
#[tokio::test]
async fn lsp_progress_fires_with_the_kind_as_its_pattern() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_progress");
    arm_mock(
        &dir,
        r#"{
            "progress": [
                { "token": "t1", "value": {
                    "kind": "begin", "title": "Indexing", "percentage": 0 } },
                { "token": "t1", "value": { "kind": "report", "percentage": 60 } },
                { "token": "t1", "value": { "kind": "end", "message": "done" } }
            ]
        }"#,
    );
    // Registered BEFORE the server is enabled, so the first `begin` is seen. The
    // pattern-scoped handler proves the kind really is the pattern: a bare-event
    // handler would fire for all three either way.
    let (rpc, _incoming) = open_with_server(
        &dir,
        r#"
        _G.log = {}
        _G.ends = 0
        _G.end_client = nil
        btv.autocmd.create("LspProgress", { callback = function(a)
          _G.log[#_G.log + 1] = a.match .. ":" .. a.data.kind .. ":" .. (a.data.token or "?")
        end })
        btv.autocmd.create("LspProgress", { pattern = "end", callback = function(a)
          _G.ends = _G.ends + 1
          local c = btv.lsp.client_by_id(a.data.client_id)
          _G.end_client = c and c.name or "<unresolved>"
        end })
        "#,
    )
    .await;

    let want = "begin:begin:t1 report:report:t1 end:end:t1";
    assert_eq!(
        await_lua_eq(&rpc, "table.concat(_G.log, \" \")", want).await,
        want,
        "every update should fire LspProgress with args.match == the kind"
    );
    assert_eq!(
        await_lua_eq(&rpc, "_G.ends", "1").await,
        "1",
        "a pattern-scoped handler should see only the `end`"
    );
    assert_eq!(
        await_lua_eq(&rpc, "_G.end_client", "mock").await,
        "mock",
        "data.client_id should resolve to the reporting client"
    );
}

/// `filter.bufnr` narrows to the clients attached to that buffer — what a per-window
/// statusline wants, since a server busy on some other project is not this buffer's
/// status. With the mock attached, the current buffer sees its task; a fresh scratch
/// buffer with no client attached sees none.
#[tokio::test]
async fn the_bufnr_filter_narrows_to_that_buffers_clients() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_progress");
    arm_mock(
        &dir,
        r#"{
            "progress": [
                { "token": "t1", "value": { "kind": "begin", "title": "Indexing" } }
            ]
        }"#,
    );
    let (rpc, _incoming) = open_with_server(&dir, "").await;

    assert_eq!(
        await_lua_eq(&rpc, "#btv.lsp.progress({ bufnr = 0 })", "1").await,
        "1",
        "the attached buffer should see its client's task"
    );
    // A scratch buffer has no client attached, so nothing is ITS status — even though
    // the unfiltered list is still non-empty.
    exec_lua(&rpc, "btv.cmd('enew')").await;
    assert_eq!(
        await_lua_eq(&rpc, "#btv.lsp.progress({ bufnr = 0 })", "0").await,
        "0",
        "a buffer with no attached client should see no progress"
    );
    assert_eq!(
        await_lua_eq(&rpc, "#btv.lsp.progress()", "1").await,
        "1",
        "the unfiltered list should still hold the running task"
    );
}

/// A server that goes away mid-task takes its progress with it: the `end` is never
/// coming, so a half-finished "Indexing 40%" must not sit on the bar forever.
#[tokio::test]
async fn stopping_a_server_clears_its_progress() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_progress");
    arm_mock(
        &dir,
        r#"{
            "progress": [
                { "token": "t1", "value": {
                    "kind": "begin", "title": "Indexing", "percentage": 40 } }
            ]
        }"#,
    );
    let (rpc, _incoming) = open_with_server(&dir, "").await;

    assert_eq!(
        await_lua_eq(&rpc, "#btv.lsp.progress()", "1").await,
        "1",
        "the task should be running before the server stops"
    );
    exec_lua(&rpc, "btv.lsp.stop('mock')").await;
    assert_eq!(
        await_lua_eq(&rpc, "#btv.lsp.progress()", "0").await,
        "0",
        "a stopped server's unfinished task should be dropped"
    );
}

/// A `report` for a token that never `begin`-ned is **accepted with an empty title**,
/// not dropped. Some servers in the wild skip the begin, and showing *something* for a
/// visibly busy server beats showing nothing — the case `btv.lsp.progress`'s docstring
/// promises when it says `title` is `""` if the server never began.
#[tokio::test]
async fn a_report_without_a_begin_is_still_listed() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_progress");
    arm_mock(
        &dir,
        r#"{
            "progress": [
                { "token": "t1", "value": {
                    "kind": "report", "message": "scanning", "percentage": 20 } }
            ]
        }"#,
    );
    let (rpc, _incoming) = open_with_server(&dir, "").await;

    let want = "mock||scanning|20";
    assert_eq!(
        await_lua_eq(&rpc, RENDER, want).await,
        want,
        "a begin-less report should be listed with an empty title, not dropped"
    );
}

/// The wire token is a `NumberOrString`, and servers disagree on which they mint. It is
/// normalized to its **decimal spelling** at the client edge so exactly one key type
/// crosses into the store, the mirror, and the `LspProgress` payload — the guarantee
/// that lets a plugin key a table on `p.token` without an untagged union. Numeric
/// tokens stay distinct from each other, and a number is never confused with the
/// same-looking string.
#[tokio::test]
async fn a_numeric_token_is_normalized_to_its_decimal_spelling() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_progress");
    arm_mock(
        &dir,
        r#"{
            "progress": [
                { "token": 7, "value": { "kind": "begin", "title": "Indexing" } },
                { "token": 12, "value": { "kind": "begin", "title": "Building" } },
                { "token": 7, "value": { "kind": "end" } }
            ]
        }"#,
    );
    let (rpc, _incoming) = open_with_server(&dir, "").await;

    // Token 7 ended, 12 did not: the two numeric tokens have to be tracked apart, which
    // a normalization collapsing both to one key (or to `""`) would fail.
    let tokens = r#"
    (function()
      local out = {}
      for _, p in ipairs(btv.lsp.progress()) do
        out[#out + 1] = type(p.token) .. ":" .. tostring(p.token) .. ":" .. p.title
      end
      table.sort(out)
      return table.concat(out, " ; ")
    end)()
    "#;
    let want = "string:12:Building";
    assert_eq!(
        await_lua_eq(&rpc, tokens, want).await,
        want,
        "a numeric token should reach Lua as its decimal spelling, distinct per token"
    );
}

/// `btv.lsp.progress()` lists clients in a **stable, ascending-client-id** order — the
/// "newest client last" its docstring promises. The mirror is a table keyed by client
/// id, and Lua's `pairs` over one is unordered: with the ids in the hash part (any
/// session where client 1 has since stopped) a plain `pairs` walk yields whichever
/// client reported *first*, so the list silently reorders as servers come and go.
/// bemtvi-line renders `tasks[1]` plus `(+N)` for the rest, so an unstable order means
/// the bar picks a different server's task from one update to the next.
///
/// Staged at the mirror seam the server pushes through rather than with two live
/// servers: which of two real servers reports first is a race, so only the seam can
/// pin down the adverse order (the higher id arriving first) deterministically.
#[tokio::test]
async fn progress_is_listed_in_ascending_client_id_order() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_progress");
    arm_mock(&dir, r#"{}"#);
    let (rpc, _incoming) = open_with_server(&dir, "").await;

    // Ids 3 and 2 — outside the table's array part, and pushed highest-first, which is
    // exactly the shape `pairs` walks in reverse.
    exec_lua(
        &rpc,
        r#"
        btv.lsp._set_client(3, "gamma", {}, "utf-8")
        btv.lsp._set_client(2, "beta", {}, "utf-8")
        btv.lsp._set_progress(3, { { token = "g1", title = "Gamma work" } })
        btv.lsp._set_progress(2, { { token = "b1", title = "Beta work" } })
        "#,
    )
    .await;

    let names = r#"
    (function()
      local out = {}
      for _, p in ipairs(btv.lsp.progress()) do out[#out + 1] = p.client_name end
      return table.concat(out, ",")
    end)()
    "#;
    let want = "beta,gamma";
    assert_eq!(
        await_lua_eq(&rpc, names, want).await,
        want,
        "progress should be listed by ascending client id regardless of report order"
    );
}
