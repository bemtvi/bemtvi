//! `btv.qf.parse` — an `errorformat` written in Lua.
//!
//! `'errorformat'` is a mini-language inherited from a 1990s C compiler's output;
//! a build tool's line is a string and an entry is a record, so parsing one into
//! the other is a pure function. This is the sandbox's table *return* pointed at
//! that. Black-box throughout: the assertions are on the entries the list ends up
//! holding and on the jumps they have to support.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    buf_name, cursor, drain_to_latest_redraw, exec_lua, lines, message, message_after,
    start_attached, write_temp,
};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Output in a shape `'errorformat'`'s default patterns do not read, so a test
/// that parses it proves the block did the work.
const OUTPUT: &str = "start.c(10,5): error: boom\\nother.c(3,1): warning: later\\njust some prose";

/// The block that reads it.
const PARSER: &str = r#"btv.qf.parse([[
  local file, ln, col, kind, msg = line:match("^(%S+)%((%d+),(%d+)%): (%a+): (.*)$")
  if not file then return nil end
  return {
    filename = file, lnum = tonumber(ln), col = tonumber(col), text = msg,
    type = kind == "error" and "E" or "W",
  }
]])"#;

/// Set the list from `OUTPUT` and fold every entry into one inspectable string.
async fn parse_and_read(rpc: &Rpc, code: &str) -> String {
    exec_lua(rpc, code).await;
    exec_lua(
        rpc,
        &format!(r#"vim.fn.setqflist({{}}, " ", {{ lines = vim.split("{OUTPUT}", "\n") }})"#),
    )
    .await;
    exec_lua(
        rpc,
        r#"local out = {}
           for _, e in ipairs(vim.fn.getqflist()) do
             out[#out + 1] = table.concat(
               { e.filename, e.lnum, e.col, e.type, tostring(e.valid), e.text }, "/")
           end
           return table.concat(out, " | ")"#,
    )
    .await
    .as_str()
    .unwrap_or("<not a string>")
    .to_string()
}

/// The latest message any frame carries after `code` ran.
async fn msg_after_lua(rpc: &Rpc, inc: &mut UnboundedReceiver<Incoming>, code: &str) -> String {
    exec_lua(rpc, code).await;
    for _ in 0..50 {
        if let Some(m) = drain_to_latest_redraw(inc, |m| !message(m).is_empty()) {
            return message(&m);
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    String::new()
}

// ===== parsing ===============================================================

#[tokio::test]
async fn a_parser_block_builds_the_entries() {
    let (rpc, _inc) = start().await;
    let got = parse_and_read(&rpc, PARSER).await;
    assert_eq!(
        got,
        "start.c/10/5/E/true/boom | other.c/3/1/W/true/later | /0/0//false/just some prose"
    );
}

#[tokio::test]
async fn without_a_parser_the_same_output_is_left_to_errorformat() {
    // The control: `'errorformat'`'s defaults do not read `file(line,col)`, so
    // every line lands as plain, unjumpable output. The parser above is the whole
    // difference.
    let (rpc, _inc) = start().await;
    let got = parse_and_read(&rpc, "return 1").await;
    assert!(
        !got.contains("start.c/10/5"),
        "errorformat should not have understood this output: {got:?}"
    );
}

#[tokio::test]
async fn a_declined_line_is_kept_as_an_invalid_entry() {
    // vim keeps a non-matching output line as an invalid entry carrying its text,
    // which is what makes `:copen` show a build's prose alongside its errors.
    let (rpc, _inc) = start().await;
    let got = parse_and_read(&rpc, PARSER).await;
    assert!(got.ends_with("/0/0//false/just some prose"), "got {got:?}");
}

#[tokio::test]
async fn lnum_is_the_position_in_the_output() {
    let (rpc, _inc) = start().await;
    exec_lua(
        &rpc,
        r#"btv.qf.parse([[ return { text = "line " .. lnum } ]])"#,
    )
    .await;
    exec_lua(
        &rpc,
        r#"vim.fn.setqflist({}, " ", { lines = { "a", "b", "c" } })"#,
    )
    .await;
    let got = exec_lua(
        &rpc,
        r#"local out = {}
           for _, e in ipairs(vim.fn.getqflist()) do out[#out + 1] = e.text end
           return table.concat(out, ",")"#,
    )
    .await;
    assert_eq!(got.as_str(), Some("line 1,line 2,line 3"));
}

#[tokio::test]
async fn valid_defaults_to_having_a_line_number() {
    let (rpc, _inc) = start().await;
    exec_lua(
        &rpc,
        r#"btv.qf.parse([[
             if line == "yes" then return { filename = "f.c", lnum = 2, text = line } end
             return { text = line }
           ]])"#,
    )
    .await;
    exec_lua(
        &rpc,
        r#"vim.fn.setqflist({}, " ", { lines = { "yes", "no" } })"#,
    )
    .await;
    let got = exec_lua(
        &rpc,
        r#"local q = vim.fn.getqflist()
           return tostring(q[1].valid) .. "," .. tostring(q[2].valid)"#,
    )
    .await;
    assert_eq!(got.as_str(), Some("true,false"));
}

#[tokio::test]
async fn a_parsed_entry_is_jumpable() {
    let (rpc, mut inc) = start().await;
    let path = write_temp("qf_parse_jump", "txt", "one\ntwo\nthree\nfour\n");
    exec_lua(
        &rpc,
        r#"btv.qf.parse([[
             local file, ln = line:match("^(.*)@(%d+)$")
             if not file then return nil end
             return { filename = file, lnum = tonumber(ln), text = "here" }
           ]])"#,
    )
    .await;
    exec_lua(
        &rpc,
        &format!(
            r#"vim.fn.setqflist({{}}, " ", {{ lines = {{ "{p}@3" }} }})"#,
            p = path.replace('\\', "\\\\")
        ),
    )
    .await;
    message_after(&rpc, &mut inc, ":cc<CR>").await;
    assert_eq!(buf_name(&rpc).await, path);
    assert_eq!(cursor(&rpc).await.0, 3);
}

#[tokio::test]
async fn a_parsed_list_renders_in_the_quickfix_window() {
    let (rpc, mut inc) = start().await;
    exec_lua(&rpc, PARSER).await;
    exec_lua(
        &rpc,
        &format!(r#"vim.fn.setqflist({{}}, " ", {{ lines = vim.split("{OUTPUT}", "\n") }})"#),
    )
    .await;
    message_after(&rpc, &mut inc, ":copen<CR>").await;
    let rendered = lines(&rpc).await;
    assert_eq!(rendered[0], "start.c|10 col 5| boom");
    assert_eq!(rendered[2], "|| just some prose");
}

#[tokio::test]
async fn it_applies_to_cbuffer_too() {
    // Every populating path funnels through the same parse, so `:cbuffer` — which
    // reads a buffer rather than a Lua list — gets it as well.
    let (rpc, mut inc) = start().await;
    exec_lua(&rpc, PARSER).await;
    exec_lua(
        &rpc,
        r#"vim.cmd("enew")
           vim.api.nvim_buf_set_lines(0, 0, -1, false,
             { "start.c(10,5): error: boom", "prose" })"#,
    )
    .await;
    message_after(&rpc, &mut inc, ":cbuffer<CR>").await;
    let got = exec_lua(
        &rpc,
        r#"local q = vim.fn.getqflist()
           return q[1].filename .. ":" .. q[1].lnum .. " (" .. #q .. ")""#,
    )
    .await;
    assert_eq!(got.as_str(), Some("start.c:10 (2)"));
}

#[tokio::test]
async fn clearing_it_hands_the_output_back_to_errorformat() {
    let (rpc, _inc) = start().await;
    let with = parse_and_read(&rpc, PARSER).await;
    assert!(with.contains("start.c/10/5"));
    let without = parse_and_read(&rpc, "btv.qf.parse(nil)").await;
    assert!(
        !without.contains("start.c/10/5"),
        "errorformat should be back in charge: {without:?}"
    );
}

#[tokio::test]
async fn a_parser_wins_over_an_explicit_errorformat() {
    // Two parsers disagreeing about one line has no sensible answer, so the block
    // replaces the errorformat pass rather than layering on it — even when the
    // caller passes an `efm` that *would* have matched.
    let (rpc, _inc) = start().await;
    exec_lua(&rpc, r#"btv.qf.parse([[ return { text = "from lua" } ]])"#).await;
    exec_lua(
        &rpc,
        r#"vim.fn.setqflist({}, " ",
             { lines = { "e.c:7: nope" }, efm = "%f:%l: %m" })"#,
    )
    .await;
    let got = exec_lua(
        &rpc,
        r#"local e = vim.fn.getqflist()[1] return e.text .. "/" .. e.lnum"#,
    )
    .await;
    assert_eq!(got.as_str(), Some("from lua/0"));
}

#[tokio::test]
async fn one_chunk_can_install_the_parser_and_populate_the_list() {
    // The natural way to write it in a config — install, then populate — and the
    // ordering that makes it work: both quickfix sandbox setters drain *before*
    // the queued list writes, since they configure how those writes are parsed.
    let (rpc, _inc) = start().await;
    exec_lua(
        &rpc,
        &format!(
            r#"{PARSER}
               vim.fn.setqflist({{}}, " ", {{ lines = {{ "one.c(4,2): error: nope" }} }})"#
        ),
    )
    .await;
    let got = exec_lua(
        &rpc,
        r#"local e = vim.fn.getqflist()[1]
           return e.filename .. ":" .. e.lnum .. ":" .. e.col .. " " .. e.type"#,
    )
    .await;
    assert_eq!(got.as_str(), Some("one.c:4:2 E"));
}

#[tokio::test]
async fn it_can_be_reinstalled_after_being_cleared() {
    let (rpc, _inc) = start().await;
    assert!(parse_and_read(&rpc, PARSER).await.contains("start.c/10/5"));
    assert!(!parse_and_read(&rpc, "btv.qf.parse(nil)")
        .await
        .contains("start.c/10/5"));
    assert!(
        parse_and_read(&rpc, PARSER).await.contains("start.c/10/5"),
        "installing again after a clear must parse again"
    );
}

// ===== failure, loudly =======================================================

#[tokio::test]
async fn a_compile_error_is_reported_where_it_is_configured() {
    let (rpc, mut inc) = start().await;
    let msg = msg_after_lua(&rpc, &mut inc, "btv.qf.parse([[ return { ]])").await;
    assert!(
        msg.contains("btv.qf.parse") && msg.contains("invalid expression"),
        "got {msg:?}"
    );
}

#[tokio::test]
async fn an_unknown_key_fails_loud() {
    // A parser that writes `line` where it meant `lnum` would otherwise quietly
    // produce entries that cannot be jumped to.
    let (rpc, mut inc) = start().await;
    exec_lua(
        &rpc,
        r#"btv.qf.parse([[ return { filename = "a.c", line = 3, text = "x" } ]])"#,
    )
    .await;
    let msg = msg_after_lua(
        &rpc,
        &mut inc,
        r#"vim.fn.setqflist({}, " ", { lines = { "whatever" } })"#,
    )
    .await;
    assert!(
        msg.contains("unknown key `line`"),
        "a misspelled key should name itself, got {msg:?}"
    );
}

#[tokio::test]
async fn an_erroring_block_reports_and_hands_back_to_errorformat() {
    let (rpc, mut inc) = start().await;
    exec_lua(&rpc, r#"btv.qf.parse([[ error("boom") ]])"#).await;
    let msg = msg_after_lua(
        &rpc,
        &mut inc,
        r#"vim.fn.setqflist({}, " ", { lines = { "a.c:1: real" }, efm = "%f:%l: %m" })"#,
    )
    .await;
    assert!(
        msg.contains("'errorformat' parsing restored") && msg.contains("boom"),
        "got {msg:?}"
    );
    // The whole input is re-parsed by errorformat, not half-parsed by the block.
    let got = exec_lua(
        &rpc,
        r#"local e = vim.fn.getqflist()[1] return e.filename .. ":" .. e.lnum"#,
    )
    .await;
    assert_eq!(got.as_str(), Some("a.c:1"));
    // Uninstalled, so a later parse does not fail again.
    exec_lua(
        &rpc,
        r#"vim.fn.setqflist({}, " ", { lines = { "b.c:2: again" }, efm = "%f:%l: %m" })"#,
    )
    .await;
    let got = exec_lua(
        &rpc,
        r#"local e = vim.fn.getqflist()[1] return e.filename .. ":" .. e.lnum"#,
    )
    .await;
    assert_eq!(got.as_str(), Some("b.c:2"));
}

#[tokio::test]
async fn a_non_table_return_fails_loud() {
    let (rpc, mut inc) = start().await;
    exec_lua(&rpc, r#"btv.qf.parse([[ return "just a string" ]])"#).await;
    let msg = msg_after_lua(
        &rpc,
        &mut inc,
        r#"vim.fn.setqflist({}, " ", { lines = { "x" } })"#,
    )
    .await;
    assert!(
        msg.contains("expected an entry table or nil"),
        "got {msg:?}"
    );
}

#[tokio::test]
async fn a_deadline_overrun_reports_and_hands_back() {
    let (rpc, mut inc) = start().await;
    exec_lua(&rpc, "btv.qf.parse([[ while true do end ]])").await;
    let msg = msg_after_lua(
        &rpc,
        &mut inc,
        r#"vim.fn.setqflist({}, " ", { lines = { "x" } })"#,
    )
    .await;
    assert!(
        msg.contains("budget") && msg.contains("'errorformat' parsing restored"),
        "got {msg:?}"
    );
}

#[tokio::test]
async fn a_non_string_argument_raises_at_the_call_site() {
    let (rpc, _inc) = start().await;
    let err = exec_lua(
        &rpc,
        "local ok, e = pcall(btv.qf.parse, 42) return tostring(e)",
    )
    .await;
    assert!(
        err.as_str().unwrap_or("").contains("expected a string"),
        "got {err:?}"
    );
}
