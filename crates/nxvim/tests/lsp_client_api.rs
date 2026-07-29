//! Behavior tests for the **client-side LSP helper surface** — the last gap the
//! native nvim-lspconfig port needed
//! (docs/plans/2026-07-29-nvim-lspconfig-native-port.md, Phase 4 pass 3).
//!
//! A per-server config drives its server's *own* vocabulary: it hand-builds a
//! position, asks whether a method is answered at all, and runs a `Command`. Every
//! one of those needs a fact the client handle did not carry — above all the
//! **negotiated position encoding**, which is silent when wrong (the request
//! succeeds and answers about a different character).
//!
//! Wired like `lsp_config.rs`: the scripted mock language server stands in for a
//! real one, `$NXVIM_LSP_CMD` overrides the spawn argv, and the mock's `record`
//! file is what actually went over the wire. The multi-byte buffer is the point —
//! utf-8 and utf-16 only disagree on a line holding one.

use std::path::Path;
use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{attach, exec_lua, feed, serial_lock, spawn, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

const NXVIM_BIN: &str = env!("CARGO_BIN_EXE_nxvim");

/// Write a mock LSP script and point `$NXVIM_LSP_CMD` at the binary's
/// `--__lsp-mock` mode. The caller holds `serial_lock`.
fn arm_mock(dir: &Path, script: &str) {
    std::fs::write(dir.join("mock.json"), script).expect("write mock script");
    // SAFETY: serialized on `serial_lock`, so no other test races this env mutation.
    std::env::set_var(
        "NXVIM_LSP_CMD",
        format!("{NXVIM_BIN} --__lsp-mock {}/mock.json", dir.display()),
    );
}

/// Open a `.rs` buffer holding `body` (filetype `rust`) and attach, without
/// starting a server — the test drives `nx.lsp.config` / `enable` itself.
async fn open_rust(dir: &Path, body: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, body).expect("write test file");
    let init = ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Enable one server named `demo` on `rust` buffers and wait for it to attach.
async fn enable_demo(rpc: &Rpc) {
    exec_lua(
        rpc,
        "nx.lsp.config('demo', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         nx.lsp.enable('demo')",
    )
    .await;
    assert!(
        await_lua_eq(rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the demo server attached"
    );
}

/// Poll `tostring(expr)` until it equals `want` (or ~5s elapse).
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

/// Every recorded message whose `method` is `method`, newest last.
fn recorded(record: &Path, method: &str) -> Vec<serde_json::Value> {
    std::fs::read_to_string(record)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("method").and_then(|m| m.as_str()) == Some(method))
        .collect()
}

/// Wait until at least `n` messages for `method` have been recorded, then return
/// them. Panics with the whole record on timeout, so a failure shows what DID land.
async fn await_recorded(record: &Path, method: &str, n: usize) -> Vec<serde_json::Value> {
    for _ in 0..200 {
        let got = recorded(record, method);
        if got.len() >= n {
            return got;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "only {} {method} message(s) recorded; the whole record was:\n{}",
        recorded(record, method).len(),
        std::fs::read_to_string(record).unwrap_or_default()
    );
}

// ----- the negotiated encoding, on the handle --------------------------------

#[tokio::test]
async fn offset_encoding_reports_what_the_server_negotiated() {
    // `client.offset_encoding` is the fact every hand-built position depends on. A
    // handle that always said "utf-16" (the protocol default) would be wrong for
    // exactly the servers that pick something else — clangd, and every server that
    // answers nxvim's advertised `utf-8` preference — and wrong only on lines with a
    // multi-byte character.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-client-enc");
    arm_mock(dir.as_path(), r#"{ "position_encoding": "utf-8" }"#);
    let (rpc, _incoming) = open_rust(dir.as_path(), "let x = 1\n").await;
    enable_demo(&rpc).await;

    let encoding = exec_lua(
        &rpc,
        "return nx.lsp.clients({ bufnr = 0 })[1].offset_encoding",
    )
    .await;
    std::env::remove_var("NXVIM_LSP_CMD");
    assert_eq!(
        encoding.as_str(),
        Some("utf-8"),
        "the handle reports the encoding the two sides settled on"
    );
}

#[tokio::test]
async fn offset_encoding_reports_utf16_when_the_server_asks_for_it() {
    // The mutation half of the test above: flip the server's answer and the handle
    // must follow it, or it is reporting a constant rather than the negotiation.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-client-enc16");
    arm_mock(dir.as_path(), r#"{ "position_encoding": "utf-16" }"#);
    let (rpc, _incoming) = open_rust(dir.as_path(), "let x = 1\n").await;
    enable_demo(&rpc).await;

    let encoding = exec_lua(
        &rpc,
        "return nx.lsp.clients({ bufnr = 0 })[1].offset_encoding",
    )
    .await;
    std::env::remove_var("NXVIM_LSP_CMD");
    assert_eq!(encoding.as_str(), Some("utf-16"));
}

#[tokio::test]
async fn assigning_offset_encoding_renegotiates_the_live_client() {
    // clangd reports its chosen encoding as a **top-level `offsetEncoding`** on the
    // `initialize` result — outside `capabilities.positionEncoding`, so no protocol
    // reader can see it. Its config's `on_init` therefore assigns
    // `client.offset_encoding`, and that write has to reach the engine: if it only
    // relabelled the Lua handle, every column nxvim sent would stay in the old
    // encoding while the config believed otherwise.
    //
    // The observable is the wire. The buffer's line starts with a 2-byte `é`, so a
    // `didChange` range past it counts 2 in utf-8 and 1 in utf-16 — one number that
    // can only come from the encoding the ENGINE is using.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-client-enc-write");
    let rec = dir.join("rec.jsonl");
    arm_mock(
        dir.as_path(),
        &format!(
            r#"{{ "record": "{}", "position_encoding": "utf-16" }}"#,
            rec.display()
        ),
    );
    let (rpc, _incoming) = open_rust(dir.as_path(), "é\n").await;
    enable_demo(&rpc).await;

    // Append a character at the end of the line: the change's range starts right
    // after `é`, which is byte 2 / utf-16 unit 1.
    feed(&rpc, "A!<Esc>");
    let utf16 = await_recorded(&rec, "textDocument/didChange", 1).await;
    let start_char = |v: &serde_json::Value| {
        v["params"]["contentChanges"][0]["range"]["start"]["character"]
            .as_u64()
            .expect("a ranged didChange")
    };
    assert_eq!(
        start_char(&utf16[0]),
        1,
        "utf-16: one code unit before the insertion point"
    );

    // The clangd shape, applied to the live client.
    exec_lua(
        &rpc,
        "nx.lsp.clients({ bufnr = 0 })[1].offset_encoding = 'utf-8'",
    )
    .await;
    feed(&rpc, "A?<Esc>");
    let after = await_recorded(&rec, "textDocument/didChange", 2).await;
    std::env::remove_var("NXVIM_LSP_CMD");

    assert_eq!(
        start_char(&after[1]),
        3,
        "utf-8: the same point is 3 BYTES in (é + !), so the engine — not just the \
         handle — switched encodings"
    );
}

// ----- supports_method -------------------------------------------------------

#[tokio::test]
async fn supports_method_reads_the_advertised_capabilities() {
    // A config guards its own `:Lsp…` command with this, so a wrong answer either
    // hides a working feature or fires a request that comes back as an error the
    // user has to decode. The mock advertises hover only when `hover` is scripted.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-client-supports");
    arm_mock(
        dir.as_path(),
        r#"{ "hover": { "contents": { "kind": "plaintext", "value": "hi" } } }"#,
    );
    let (rpc, _incoming) = open_rust(dir.as_path(), "let x = 1\n").await;
    enable_demo(&rpc).await;

    let answers = exec_lua(
        &rpc,
        "local c = nx.lsp.clients({ bufnr = 0 })[1]\n\
         return table.concat({\n\
         \x20 tostring(c:supports_method('textDocument/hover')),\n\
         \x20 tostring(c:supports_method('textDocument/inlayHint')),\n\
         \x20 tostring(c:supports_method('textDocument/switchSourceHeader')),\n\
         }, ',')",
    )
    .await;
    std::env::remove_var("NXVIM_LSP_CMD");
    assert_eq!(
        answers.as_str(),
        Some("true,false,true"),
        "advertised ⇒ true, unadvertised ⇒ false, and a server's OWN extension \
         (which no capability describes) ⇒ true"
    );
}

// ----- exec_cmd --------------------------------------------------------------

#[tokio::test]
async fn exec_cmd_sends_the_command_to_the_server() {
    // The fallback leg: nothing handles `demo.doIt` client-side, so it goes out as
    // `workspace/executeCommand` with the arguments the config passed.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-client-exec");
    let rec = dir.join("rec.jsonl");
    arm_mock(
        dir.as_path(),
        &format!(r#"{{ "record": "{}" }}"#, rec.display()),
    );
    let (rpc, _incoming) = open_rust(dir.as_path(), "let x = 1\n").await;
    enable_demo(&rpc).await;

    exec_lua(
        &rpc,
        "nx.lsp.clients({ bufnr = 0 })[1]:exec_cmd({\n\
         \x20 title = 'do it', command = 'demo.doIt', arguments = { 'ARG-ONE' },\n\
         }, { bufnr = 0 })",
    )
    .await;
    let sent = await_recorded(&rec, "workspace/executeCommand", 1).await;
    std::env::remove_var("NXVIM_LSP_CMD");

    assert_eq!(sent[0]["params"]["command"].as_str(), Some("demo.doIt"));
    assert_eq!(
        sent[0]["params"]["arguments"][0].as_str(),
        Some("ARG-ONE"),
        "the command's arguments cross verbatim"
    );
}

#[tokio::test]
async fn exec_cmd_prefers_the_configs_own_command_handler() {
    // A command name is one server's private vocabulary, and some of them are
    // defined to run *client*-side (open a file, start a rename) — the server cannot
    // do them at all. The config's own `commands` table therefore wins over the
    // round trip, and is handed `ctx` naming the client and the buffer.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-client-exec-handler");
    let rec = dir.join("rec.jsonl");
    arm_mock(
        dir.as_path(),
        &format!(r#"{{ "record": "{}" }}"#, rec.display()),
    );
    let (rpc, _incoming) = open_rust(dir.as_path(), "let x = 1\n").await;
    exec_lua(
        &rpc,
        "_G.ran = nil\n\
         nx.lsp.config('demo', {\n\
         \x20 cmd = { 'unused' }, filetypes = { 'rust' },\n\
         \x20 commands = {\n\
         \x20   ['demo.doIt'] = function(command, ctx)\n\
         \x20     _G.ran = command.arguments[1] .. '/' .. tostring(ctx.client_id ~= nil)\n\
         \x20   end,\n\
         \x20 },\n\
         })\n\
         nx.lsp.enable('demo')",
    )
    .await;
    assert!(await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await);

    exec_lua(
        &rpc,
        "nx.lsp.clients({ bufnr = 0 })[1]:exec_cmd({\n\
         \x20 command = 'demo.doIt', arguments = { 'ARG-ONE' } }, { bufnr = 0 })",
    )
    .await;
    let handled = await_lua_eq(&rpc, "_G.ran", "ARG-ONE/true").await;
    // Give a stray round trip time to show up before asserting it never happened.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let round_tripped = !recorded(&rec, "workspace/executeCommand").is_empty();
    std::env::remove_var("NXVIM_LSP_CMD");

    assert!(
        handled,
        "the config's own handler ran, with the command's args"
    );
    assert!(
        !round_tripped,
        "and the server was never asked — a client-side command is the editor's to run"
    );
}

// ----- position params -------------------------------------------------------

#[tokio::test]
async fn position_params_counts_the_column_in_the_requested_encoding() {
    // The builder behind every hand-issued request. `é` is 2 bytes / 1 utf-16 unit /
    // 1 codepoint, and `𝄞` is 4 bytes / 2 utf-16 units / 1 codepoint — so a cursor
    // past both is a different number in all three encodings, and a builder that
    // returned nxvim's own byte column would be right only for utf-8.
    let dir = temp_dir("lsp-client-pos");
    let (rpc, _incoming) = open_rust(dir.as_path(), "é𝄞xy\n").await;
    // `$` lands on the last character of the line (`y`), 7 bytes in.
    feed(&rpc, "$");

    let cols = exec_lua(
        &rpc,
        "local out = {}\n\
         for _, enc in ipairs({ 'utf-8', 'utf-16', 'utf-32' }) do\n\
         \x20 out[#out + 1] = nx.lsp.position_params({ encoding = enc }).position.character\n\
         end\n\
         return table.concat(out, ',')",
    )
    .await;
    assert_eq!(
        cols.as_str(),
        Some("7,4,3"),
        "utf-8 counts bytes (2+4+1), utf-16 counts code units (1+2+1), utf-32 \
         counts codepoints (1+1+1)"
    );

    let rest = exec_lua(
        &rpc,
        "local p = nx.lsp.position_params({ encoding = 'utf-8' })\n\
         return p.position.line .. '|' .. p.textDocument.uri",
    )
    .await;
    let rest = rest.as_str().expect("position params");
    let (line, uri) = rest.split_once('|').expect("line|uri");
    assert_eq!(line, "0", "the cursor's 0-based line");
    assert!(
        uri.starts_with("file://") && uri.ends_with("/a.rs"),
        "the document is named by its file:// URI, got {uri}"
    );
}

#[tokio::test]
async fn text_document_params_names_the_buffers_file() {
    let dir = temp_dir("lsp-client-td");
    let (rpc, _incoming) = open_rust(dir.as_path(), "let x = 1\n").await;
    let uri = exec_lua(&rpc, "return nx.lsp.text_document_params(0).uri").await;
    let uri = uri.as_str().expect("uri");
    assert!(
        uri.starts_with("file://") && uri.ends_with("/a.rs"),
        "got {uri}"
    );
}

#[tokio::test]
async fn a_uri_percent_encodes_the_characters_that_would_break_it() {
    // A path with a space or a `#` produces a URI the server misreads (it truncates
    // at the fragment) unless it is escaped, and the round trip has to come back
    // byte-identical or the server's document map misses the buffer it already holds.
    let dir = temp_dir("lsp-client-uri");
    let (rpc, _incoming) = open_rust(dir.as_path(), "x\n").await;
    let out = exec_lua(
        &rpc,
        "local u = nx.utils.uri_from_path('/tmp/a b/c#d/é.rs')\n\
         return u .. '|' .. tostring(nx.utils.uri_to_path(u))\n",
    )
    .await;
    assert_eq!(
        out.as_str(),
        Some("file:///tmp/a%20b/c%23d/%C3%A9.rs|/tmp/a b/c#d/é.rs"),
        "escaped on the way out, decoded byte-for-byte on the way back"
    );

    let other = exec_lua(
        &rpc,
        "return tostring(nx.utils.uri_to_path('deno:/https/example.com/mod.ts'))",
    )
    .await;
    assert_eq!(
        other.as_str(),
        Some("nil"),
        "a non-file:// document has no path, and must not be mistaken for one"
    );
}

// ----- locations -> quickfix items -------------------------------------------

#[tokio::test]
async fn locations_to_items_converts_columns_and_reads_the_line() {
    // The bridge from a server's `Location[]` to a quickfix list. Two things have to
    // be right: the column comes back as nxvim's own BYTE offset (the server counted
    // utf-16 units), and each item carries the source line — which for a file no
    // buffer holds means reading it, asynchronously, because nxvim does no blocking
    // I/O.
    let dir = temp_dir("lsp-client-items");
    let other = dir.join("other.rs");
    std::fs::write(&other, "fn é_here() {}\nsecond line\n").expect("write");
    let (rpc, _incoming) = open_rust(dir.as_path(), "let x = 1\n").await;

    let out = exec_lua(
        &rpc,
        &format!(
            "_G.items = nil\n\
             local locs = {{\n\
             \x20 {{ uri = 'file://{p}', range = {{\n\
             \x20     start = {{ line = 0, character = 5 }},\n\
             \x20     ['end'] = {{ line = 0, character = 7 }} }} }},\n\
             \x20 {{ uri = 'file://{p}', range = {{\n\
             \x20     start = {{ line = 1, character = 0 }},\n\
             \x20     ['end'] = {{ line = 1, character = 6 }} }} }},\n\
             }}\n\
             nx.lsp.locations_to_items(locs, {{ encoding = 'utf-16' }}):next(function(items)\n\
             \x20 _G.items = items\n\
             end)\n\
             return 'queued'",
            p = other.display()
        ),
    )
    .await;
    assert_eq!(out.as_str(), Some("queued"));

    assert!(
        await_lua_eq(&rpc, "_G.items and #_G.items or 0", "2").await,
        "the promise resolved with one item per location"
    );
    let first = exec_lua(
        &rpc,
        "local i = _G.items[1]\n\
         return i.lnum .. '|' .. i.col .. '|' .. i.end_col .. '|' .. i.text",
    )
    .await;
    assert_eq!(
        first.as_str(),
        // `fn é_here` — utf-16 character 5 is byte 6 (é costs one extra byte), and
        // the columns are 1-based the way a quickfix entry is.
        Some("1|7|9|fn é_here() {}"),
        "the utf-16 columns became 1-based BYTE columns, and the line came off disk"
    );
    let second = exec_lua(&rpc, "return _G.items[2].lnum .. '|' .. _G.items[2].text").await;
    assert_eq!(second.as_str(), Some("2|second line"));
}

#[tokio::test]
async fn locations_to_items_prefers_an_open_buffers_unsaved_text() {
    // A location into the buffer being edited must quote what is on SCREEN, not what
    // is on disk — otherwise every entry in a reference list goes stale the moment
    // you type, and the columns are counted against the wrong line.
    let dir = temp_dir("lsp-client-items-buf");
    let (rpc, _incoming) = open_rust(dir.as_path(), "original\n").await;
    feed(&rpc, "ccedited now<Esc>");

    let path = dir.join("a.rs");
    exec_lua(
        &rpc,
        &format!(
            "_G.items = nil\n\
             nx.lsp.locations_to_items({{ {{ uri = 'file://{p}', range = {{\n\
             \x20 start = {{ line = 0, character = 0 }},\n\
             \x20 ['end'] = {{ line = 0, character = 6 }} }} }} }})\n\
             \x20 :next(function(items) _G.items = items end)",
            p = path.display()
        ),
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "_G.items and _G.items[1].text", "edited now").await,
        "the item quotes the buffer's unsaved text, not the file's"
    );
}
