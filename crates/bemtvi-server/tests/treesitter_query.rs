//! `btv.treesitter.query` end-to-end over a *real* markdown parse: the markdown block
//! grammar is compiled into a temp data dir, a `.md` file is opened, and a plugin-side
//! query must come back with the nodes the grammar actually built — the pipe table
//! outside a code fence and not the one inside it, a node crossing the row range with
//! its full extent, results that follow edits — or reject with a message naming why
//! there is no answer.
//!
//! Hermetic: the grammar's C sources compile out of the cargo registry (a
//! dev-dependency, so they are present) and `BEMTVI_DATA_DIR` pins the engine's search
//! path to the temp dir. It needs a C compiler; absent one, each test skips (the
//! external-dependency convention).

use std::path::{Path, PathBuf};

use bemtvi_rpc::Rpc;
use bemtvi_server::ServerInit;
use bemtvi_test_harness::*;

/// Locate an unpacked crate source dir in the cargo registry by its
/// `<name>-<version>` folder (a dev-dependency, so cargo guarantees presence).
fn registry_crate_dir(crate_dir: &str) -> PathBuf {
    let cargo_home = std::env::var("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(
                std::env::var("HOME")
                    .or_else(|_| std::env::var("USERPROFILE"))
                    .expect("HOME, USERPROFILE or CARGO_HOME must be set"),
            )
            .join(".cargo")
        });
    let registry = cargo_home.join("registry").join("src");
    for index in std::fs::read_dir(&registry).expect("read cargo registry src") {
        let candidate = index.unwrap().path().join(crate_dir);
        if candidate.is_dir() {
            return candidate;
        }
    }
    panic!("{crate_dir} source not found under {registry:?}");
}

/// Compile the markdown block grammar into `<root>/parser/markdown.so` and write its
/// bundled highlights query. Returns `false` when no C compiler is available.
fn install_markdown_grammar(root: &Path) -> bool {
    let md = registry_crate_dir("tree-sitter-md-0.5.3").join("tree-sitter-markdown");
    let src = md.join("src");
    let parser_dir = root.join("parser");
    std::fs::create_dir_all(&parser_dir).unwrap();
    let cc = c_compiler();
    let status = std::process::Command::new(&cc[0])
        .args(&cc[1..])
        .args(["-shared", "-fPIC", "-O1"])
        .arg("-I")
        .arg(&src)
        .arg(src.join("parser.c"))
        .arg(src.join("scanner.c"))
        .arg("-o")
        .arg(parser_dir.join("markdown.so"))
        .status();
    match status {
        Ok(s) if s.success() => {}
        _ => return false,
    }
    let qdir = root.join("queries").join("markdown");
    std::fs::create_dir_all(&qdir).unwrap();
    let highlights =
        std::fs::read_to_string(md.join("queries").join("highlights.scm")).expect("read query");
    std::fs::write(qdir.join("highlights.scm"), highlights).unwrap();
    true
}

/// A table inside a code fence (rows 2-4) and a real one (rows 7-10).
const SAMPLE: &str = "\
# Tables

```
| not | a table |
|-----|---------|
```

| Queue | Upstream |
|---|---|
| a | b |
| c | d |

after
";

/// The data dir holding the compiled markdown grammar, built once per test binary
/// (`None` when there is no C compiler).
///
/// Shared and never deleted during the run, on purpose. A test can finish while its
/// server is still loading the grammar in the background, and on macOS deleting a
/// library while `dlopen` is mapping it hangs that load in the kernel, holding the
/// dynamic loader's lock, so every later load in the process hangs behind it.
fn grammar_dir() -> Option<&'static Path> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let data = temp_dir("tsq-grammar");
        install_markdown_grammar(&data).then_some(data)
    })
    .as_deref()
}

/// Clears `BEMTVI_DATA_DIR` when the test ends, so another test binary's server never
/// sees this one's.
struct EnvKeep;
impl Drop for EnvKeep {
    fn drop(&mut self) {
        std::env::remove_var("BEMTVI_DATA_DIR");
    }
}

/// Take the `serial_lock`, point `BEMTVI_DATA_DIR` at the shared grammar dir, and
/// open `content` as a `.<ext>` file — binding `(rpc, _incoming, _guard, _keep)`.
/// `return`s early (skips the test) when there is no C compiler.
macro_rules! open_file {
    ($tag:expr, $ext:expr, $content:expr) => {{
        let guard = serial_lock().lock().await;
        let Some(data) = grammar_dir() else {
            eprintln!("skip: no C compiler to build the markdown grammar fixture");
            return;
        };
        std::env::set_var("BEMTVI_DATA_DIR", data);
        let file = write_temp($tag, $ext, $content);
        let (rpc, incoming) = start_attached(
            ServerInit {
                file: Some(file),
                ..Default::default()
            },
            80,
            24,
        )
        .await;
        (rpc, incoming, guard, EnvKeep)
    }};
}

/// Run `btv.treesitter.query(<args>)` and wait for it to settle. `Ok` carries every
/// captured node as `name:row,col-end_row,end_col kind`, joined by `;` in match order,
/// plus the result's `tick`; `Err` carries the rejection message.
async fn query(rpc: &Rpc, args: &str) -> Result<(String, u64), String> {
    exec_lua(
        rpc,
        &format!(
            r#"
            _Q = nil
            btv.treesitter.query({args}):next(function(res)
              local out = {{}}
              for _, m in ipairs(res.matches) do
                local names = {{}}
                for name in pairs(m.captures) do names[#names + 1] = name end
                table.sort(names)
                for _, name in ipairs(names) do
                  for _, n in ipairs(m.captures[name]) do
                    out[#out + 1] = ("%s:%d,%d-%d,%d %s"):format(name, n[1], n[2], n[3], n[4], n.kind)
                  end
                end
              end
              _Q = {{ ok = table.concat(out, ";"), tick = res.tick }}
            end, function(err)
              _Q = {{ err = tostring(err) }}
            end)
            "#
        ),
    )
    .await;
    assert!(
        poll_true(rpc, "return _Q ~= nil").await,
        "btv.treesitter.query({args}) never settled"
    );
    let err = exec_lua(rpc, "return _Q.err").await;
    if let Some(err) = err.as_str() {
        return Err(err.to_string());
    }
    let ok = exec_lua(rpc, "return _Q.ok").await;
    let tick = lua_u64(rpc, "return _Q.tick").await.expect("tick");
    Ok((ok.as_str().expect("ok string").to_string(), tick))
}

#[tokio::test]
async fn finds_the_table_outside_the_fence_and_not_the_one_inside() {
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-fence", "md", SAMPLE);
    let (nodes, _) = query(&rpc, r#"0, "(pipe_table) @table""#).await.unwrap();
    assert_eq!(nodes, "table:7,0-11,0 pipe_table");
}

#[tokio::test]
async fn captures_carry_names_kinds_and_byte_columns() {
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-cells", "md", SAMPLE);
    let (nodes, _) = query(&rpc, r#"0, "(pipe_table_header (pipe_table_cell) @cell)""#)
        .await
        .unwrap();
    // `| Queue | Upstream |`, byte columns: the grammar starts a cell at its text
    // (past the leading space) and ends it at the next pipe.
    assert_eq!(
        nodes,
        "cell:7,2-7,8 pipe_table_cell;cell:7,10-7,19 pipe_table_cell"
    );
}

#[tokio::test]
async fn a_row_range_keeps_crossing_nodes_whole_and_drops_the_rest() {
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-rows", "md", SAMPLE);
    // Only row 9, the table's middle: the table comes back with its full extent.
    let (nodes, _) = query(&rpc, r#"0, "(pipe_table) @table", { rows = { 9, 10 } }"#)
        .await
        .unwrap();
    assert_eq!(nodes, "table:7,0-11,0 pipe_table");
    // Rows 0-1 touch no table.
    let (nodes, _) = query(&rpc, r#"0, "(pipe_table) @table", { rows = { 0, 2 } }"#)
        .await
        .unwrap();
    assert_eq!(nodes, "");
}

#[tokio::test]
async fn an_empty_row_range_matches_nothing() {
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-empty", "md", SAMPLE);
    // `{0, 0}` in particular: tree-sitter reads an end point of (0, 0) as "no limit".
    for rows in ["{ 0, 0 }", "{ 9, 9 }"] {
        let (nodes, _) = query(
            &rpc,
            &format!(r#"0, "(pipe_table) @table", {{ rows = {rows} }}"#),
        )
        .await
        .unwrap();
        assert_eq!(nodes, "", "rows = {rows}");
    }
    // …but a broken query still says so, empty range or not.
    let err = query(&rpc, r#"0, "(no_such_node) @x", { rows = { 0, 0 } }"#)
        .await
        .unwrap_err();
    assert!(err.contains("invalid query"), "{err}");
}

#[tokio::test]
async fn a_query_capturing_too_many_nodes_fails_loud() {
    let mut big = String::new();
    for i in 0..110_000 {
        big.push_str(&format!("para {i}\n\n"));
    }
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-many", "md", &big);
    // 110,000 paragraphs, each its own capture: past the 100,000-capture cap.
    let err = query(&rpc, r#"0, "(paragraph) @p""#).await.unwrap_err();
    assert!(err.contains("captured more than 100000"), "{err}");
}

#[tokio::test]
async fn the_result_follows_an_edit_and_reports_its_tick() {
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-edit", "md", SAMPLE);
    let (_, before) = query(&rpc, r#"0, "(pipe_table) @table""#).await.unwrap();
    // Delete the last body row: the table ends a row earlier.
    feed_sync(&rpc, "11Gdd").await;
    assert_eq!(lines(&rpc).await[10], "");
    let (nodes, after) = query(&rpc, r#"0, "(pipe_table) @table""#).await.unwrap();
    assert_eq!(nodes, "table:7,0-10,0 pipe_table");
    assert!(after > before, "tick advanced: {before} -> {after}");
    assert_eq!(
        lua_u64(&rpc, "return btv.buf.changedtick(0)").await,
        Some(after)
    );
}

#[tokio::test]
async fn an_invalid_query_rejects_with_treesitter_s_message() {
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-bad", "md", SAMPLE);
    let err = query(&rpc, r#"0, "(no_such_node) @x""#).await.unwrap_err();
    assert!(err.contains("invalid query"), "{err}");
    assert!(
        err.contains("no_such_node") || err.contains("NodeType"),
        "{err}"
    );
}

#[tokio::test]
async fn a_language_with_no_grammar_rejects() {
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-nogrammar", "rs", "fn main() {}\n");
    let err = query(&rpc, r#"0, "(_) @x""#).await.unwrap_err();
    assert_eq!(err, "no treesitter grammar installed for 'rust'");
}

#[tokio::test]
async fn a_buffer_with_treesitter_off_rejects() {
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-off", "md", SAMPLE);
    exec_lua(&rpc, "btv.bo.ts_highlight = false").await;
    let err = query(&rpc, r#"0, "(pipe_table) @table""#)
        .await
        .unwrap_err();
    assert!(err.contains("no treesitter language"), "{err}");
}

#[tokio::test]
async fn an_unknown_buffer_rejects() {
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-nobuf", "md", SAMPLE);
    let err = query(&rpc, r#"9999, "(pipe_table) @table""#)
        .await
        .unwrap_err();
    assert_eq!(err, "invalid buffer id 9999");
}

#[tokio::test]
async fn bad_arguments_fail_at_the_call() {
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-args", "md", SAMPLE);
    for (args, want) in [
        (r#""x", "(_) @x""#, "buf must be a buffer number"),
        (r#"0, 42"#, "query must be a string"),
        (r#"0, "(_) @x", { row = { 0, 1 } }"#, "unknown option 'row'"),
        (
            r#"0, "(_) @x", { rows = { 3, 1 } }"#,
            "rows must be { first, last }",
        ),
    ] {
        let got = exec_lua(
            &rpc,
            &format!("local ok, err = pcall(btv.treesitter.query, {args}); return tostring(err)"),
        )
        .await;
        let got = got.as_str().unwrap_or_default().to_string();
        assert!(got.contains(want), "{args}: {got}");
    }
}

/// A buffer too big to parse in one budget, and not on screen: only the parked
/// query's own retries advance its parse, and the promise must still settle with
/// the right answer instead of "no tables" or a hang.
#[tokio::test]
async fn a_query_waits_for_an_unfinished_parse_in_a_hidden_buffer() {
    let mut big = String::new();
    for i in 0..60_000 {
        big.push_str(&format!(
            "paragraph line {i} with *some* `inline` markup\n\n"
        ));
    }
    big.push_str("| k | v |\n|---|---|\n| a | 1 |\n");
    // Open it and leave it in one input batch, so no frame ever highlights it: only
    // the parked query's own retries can advance its parse.
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-big-host", "md", "host\n");
    let path = write_temp("tsq-big", "md", &big);
    feed_sync(&rpc, &format!(":e {path}<CR>:enew<CR>")).await;
    let buf = lua_u64(&rpc, "for _, b in ipairs(btv.buf.list()) do if btv.buf.name(b):find('tsq%-big[^%-]') then return b end end")
        .await
        .expect("the big buffer is listed");
    let (nodes, _) = query(&rpc, &format!(r#"{buf}, "(pipe_table) @table""#))
        .await
        .unwrap();
    assert_eq!(nodes, "table:120000,0-120003,0 pipe_table");
}

// ===== `query` on a decor provider ===========================================

/// Register a decor provider named `probe` over markdown with `query`, recording each
/// `ctx.query` it is handed: `_D` is the latest as a summary string (the nodes, like
/// [`query`]'s, or `error:<msg>` / `pending:<msg>`), `_D_pending` whether any answer
/// so far was "not yet". Then invalidate, so the current viewport is dispatched.
async fn register_probe(rpc: &Rpc, filetype: &str, query: &str) {
    exec_lua(
        rpc,
        &format!(
            r#"
            _D, _D_pending = nil, false
            btv.decor.provider({{
              name = "probe",
              bufs = {{ filetype = {{ {filetype:?} }} }},
              query = {query:?},
              on_range = function(ctx, publish)
                local q = ctx.query
                if q.matches then
                  local out = {{}}
                  for _, m in ipairs(q.matches) do
                    for name, nodes in pairs(m.captures) do
                      for _, n in ipairs(nodes) do
                        out[#out + 1] = ("%s:%d,%d-%d,%d %s"):format(name, n[1], n[2], n[3], n[4], n.kind)
                      end
                    end
                  end
                  _D = table.concat(out, ";")
                else
                  if q.pending then _D_pending = true end
                  _D = (q.pending and "pending:" or "error:") .. q.error
                end
                publish({{}})
              end,
            }})
            btv.decor.invalidate()
            "#
        ),
    )
    .await;
}

/// Wait until the probe's latest answer reads `want`, returning the last one seen.
async fn probe_reads(rpc: &Rpc, want: &str) -> String {
    let check = format!("return _D == {want:?}");
    poll_true(rpc, &check).await;
    exec_lua(rpc, "return _D")
        .await
        .as_str()
        .unwrap_or("<nil>")
        .to_string()
}

#[tokio::test]
async fn a_decor_provider_gets_its_query_answered_in_ctx() {
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-decor", "md", SAMPLE);
    register_probe(&rpc, "markdown", "(pipe_table) @table").await;
    assert_eq!(
        probe_reads(&rpc, "table:7,0-11,0 pipe_table").await,
        "table:7,0-11,0 pipe_table"
    );
}

#[tokio::test]
async fn a_decor_query_follows_the_viewport_and_keeps_crossing_nodes_whole() {
    // A 40-row table at the top, filler, then a second table far below.
    let mut doc = String::from("| k | v |\n|---|---|\n");
    for i in 0..38 {
        doc.push_str(&format!("| {i} | x |\n"));
    }
    for i in 0..200 {
        doc.push_str(&format!("\nfiller {i}\n"));
    }
    doc.push_str("\n| a | b |\n|---|---|\n| 1 | 2 |\n");
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-decor-scroll", "md", &doc);
    register_probe(&rpc, "markdown", "(pipe_table) @table").await;
    assert_eq!(
        probe_reads(&rpc, "table:0,0-40,0 pipe_table").await,
        "table:0,0-40,0 pipe_table"
    );
    // Scroll so the table's top is off screen: it still comes back whole, and the
    // table far below is not in the answer.
    feed_sync(&rpc, "21Gzt").await;
    assert_eq!(
        probe_reads(&rpc, "table:0,0-40,0 pipe_table").await,
        "table:0,0-40,0 pipe_table"
    );
    // Jump to the end: only the second table.
    feed_sync(&rpc, "G").await;
    assert_eq!(
        probe_reads(&rpc, "table:441,0-444,0 pipe_table").await,
        "table:441,0-444,0 pipe_table"
    );
}

#[tokio::test]
async fn a_decor_query_follows_an_edit() {
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-decor-edit", "md", SAMPLE);
    register_probe(&rpc, "markdown", "(pipe_table) @table").await;
    probe_reads(&rpc, "table:7,0-11,0 pipe_table").await;
    feed_sync(&rpc, "11Gdd").await;
    assert_eq!(
        probe_reads(&rpc, "table:7,0-10,0 pipe_table").await,
        "table:7,0-10,0 pipe_table"
    );
}

#[tokio::test]
async fn a_decor_query_with_no_grammar_says_so() {
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-decor-nogrammar", "rs", "fn main() {}\n");
    register_probe(&rpc, "rust", "(_) @x").await;
    let want = "error:no treesitter grammar installed for 'rust'";
    assert_eq!(probe_reads(&rpc, want).await, want);
}

#[tokio::test]
async fn a_decor_query_that_does_not_compile_is_reported_once() {
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-decor-bad", "md", SAMPLE);
    exec_lua(
        &rpc,
        r#"
        _N = {}
        local notify = btv.notify
        btv.notify = function(msg, level)
          if tostring(msg):find("probe") then _N[#_N + 1] = msg end
          return notify(msg, level)
        end
        "#,
    )
    .await;
    register_probe(&rpc, "markdown", "(no_such_node) @x").await;
    assert!(
        poll_true(
            &rpc,
            r#"return (_D or ""):find("^error:invalid query") ~= nil"#
        )
        .await
    );
    // A second dispatch with the same broken query does not report it again.
    exec_lua(&rpc, "_D = nil; btv.decor.invalidate()").await;
    assert!(poll_true(&rpc, "return _D ~= nil").await);
    assert_eq!(lua_u64(&rpc, "return #_N").await, Some(1));
    let msg = exec_lua(&rpc, "return _N[1]").await;
    assert!(
        msg.as_str().unwrap_or_default().contains("invalid query"),
        "{msg:?}"
    );
}

#[tokio::test]
async fn a_decor_query_must_be_a_string() {
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-decor-arg", "md", SAMPLE);
    let got = exec_lua(
        &rpc,
        r#"local ok, err = pcall(btv.decor.provider, {
             name = "x", query = 42, on_range = function() end })
           return tostring(err)"#,
    )
    .await;
    assert!(
        got.as_str()
            .unwrap_or_default()
            .contains("query must be a non-empty string"),
        "{got:?}"
    );
}

/// A file too big to parse in one budget: the first answer is "not yet", and the
/// provider must be re-dispatched by itself with the real one, without any input.
#[tokio::test]
async fn a_decor_query_on_an_unfinished_parse_is_answered_when_it_converges() {
    let mut big = String::from("| k | v |\n|---|---|\n| a | 1 |\n");
    for i in 0..60_000 {
        big.push_str(&format!(
            "\nparagraph line {i} with *some* `inline` markup\n"
        ));
    }
    // The provider is registered first and the big file opened after, so its first
    // dispatch comes before the buffer's first highlight: deterministically "not yet".
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-decor-big-host", "md", "host\n");
    register_probe(&rpc, "markdown", "(pipe_table) @table").await;
    let path = write_temp("tsq-decor-big", "md", &big);
    feed_sync(&rpc, &format!(":e {path}<CR>")).await;
    assert_eq!(
        probe_reads(&rpc, "table:0,0-3,0 pipe_table").await,
        "table:0,0-3,0 pipe_table"
    );
    assert_eq!(
        lua_bool(&rpc, "return _D_pending").await,
        Some(true),
        "the first answer should have been \"not yet\" for a file this big"
    );
}

#[tokio::test]
async fn a_debounced_provider_queries_once_its_burst_settles() {
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-decor-debounce", "md", SAMPLE);
    // Wait for the grammar first (a query waits for it to load): this test is about the
    // debounce, and a first run inside the load would rightly answer "still loading".
    query(&rpc, r#"0, "(pipe_table) @t""#).await.unwrap();
    exec_lua(
        &rpc,
        r#"
        _RUNS, _LAST = 0, nil
        btv.decor.provider({
          name = "slow",
          bufs = { filetype = { "markdown" } },
          debounce = 150,
          query = "(pipe_table) @t",
          on_range = function(ctx, publish)
            _RUNS = _RUNS + 1
            _LAST = ctx.query.matches and #ctx.query.matches or ("error:" .. tostring(ctx.query.error))
            publish({})
          end,
        })
        "#,
    )
    .await;
    // While its debounce is coalescing, a dispatch runs no query for it.
    let wanted = exec_lua(
        &rpc,
        "return #btv._decor_queries({ win = btv.win.current(), buf = btv.buf.current(), filetype = 'markdown', buftype = '' })",
    )
    .await;
    assert_eq!(
        wanted.as_u64(),
        Some(0),
        "no query while the burst is coalescing"
    );
    // A burst of viewport changes, then quiet: the provider runs once the burst
    // settles, with its query answered.
    for _ in 0..5 {
        exec_lua(&rpc, "btv.decor.invalidate()").await;
    }
    assert!(
        poll_true(&rpc, "return _LAST ~= nil").await,
        "the provider never ran"
    );
    let last = exec_lua(&rpc, "return tostring(_LAST)").await;
    assert_eq!(last.as_str(), Some("1"), "one table matched");
    settle_ms(&rpc, 300).await;
    let runs = lua_u64(&rpc, "return _RUNS").await.unwrap();
    assert!(
        (1..=2).contains(&runs),
        "the burst ran the provider {runs} times"
    );
}

#[tokio::test]
async fn naming_a_buffer_md_runs_its_markdown_provider() {
    let (rpc, _incoming, _guard, _keep) = open_file!("tsq-decor-rename-host", "md", "host\n");
    command(&rpc, "enew").await;
    exec_lua(
        &rpc,
        "btv.buf.set_lines(0, 0, -1, false, { '| a |', '|---|' })",
    )
    .await;
    register_probe(&rpc, "markdown", "(pipe_table) @table").await;
    assert!(
        exec_lua(&rpc, "return _D").await.is_nil(),
        "not markdown yet"
    );
    // `:w path` names the buffer; its extension makes it markdown.
    let path = temp_dir("tsq-rename").join("named.md");
    feed_sync(&rpc, &format!(":w {}<CR>", path.display())).await;
    assert_eq!(
        probe_reads(&rpc, "table:0,0-2,0 pipe_table").await,
        "table:0,0-2,0 pipe_table"
    );
}

#[tokio::test]
async fn a_provider_told_no_grammar_reruns_once_one_is_installed() {
    // A data dir with no grammar yet, so the provider is told there is none.
    let guard = serial_lock().lock().await;
    let Some(src) = grammar_dir() else {
        eprintln!("skip: no C compiler to build the markdown grammar fixture");
        return;
    };
    let data = temp_dir("tsq-install");
    std::env::set_var("BEMTVI_DATA_DIR", &data);
    let _keep = EnvKeep;
    let file = write_temp("tsq-install", "md", SAMPLE);
    let (rpc, _incoming) = start_attached(
        ServerInit {
            file: Some(file),
            ..Default::default()
        },
        80,
        24,
    )
    .await;
    register_probe(&rpc, "markdown", "(pipe_table) @table").await;
    let want = "error:no treesitter grammar installed for 'markdown'";
    assert_eq!(probe_reads(&rpc, want).await, want);
    // Install it (copy the compiled grammar in) and reload, as `:TSInstall` does.
    for sub in ["parser", "queries/markdown"] {
        std::fs::create_dir_all(data.join(sub)).unwrap();
        for entry in std::fs::read_dir(src.join(sub)).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_file() {
                std::fs::copy(entry.path(), data.join(sub).join(entry.file_name())).unwrap();
            }
        }
    }
    exec_lua(&rpc, "btv.treesitter.reload('markdown')").await;
    assert_eq!(
        probe_reads(&rpc, "table:7,0-11,0 pipe_table").await,
        "table:7,0-11,0 pipe_table",
        "the provider re-ran with the new grammar"
    );
    drop(guard);
}
