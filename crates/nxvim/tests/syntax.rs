//! Treesitter syntax highlighting, end to end through the real stack: the server
//! owns an **in-process** treesitter engine that loads a grammar we compile into
//! a temp `NXVIM_DATA_DIR` fixture. Highlighting is now synchronous — the spans
//! are correct in the same frame as the edit — but these tests still drain to the
//! latest redraw with a bounded poll, since the client's reader task ferries
//! redraws onto the channel asynchronously (the harness race documented in
//! CLAUDE.md), not because the highlights themselves lag.
//!
//! These tests share process-global env (`NXVIM_DATA_DIR`), so they serialize on
//! a single lock.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
use nxvim_tui::paint;
use nxvim_view::View;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::Mutex;

const COLS: u16 = 80;
const ROWS: u16 = 24;

/// Serializes the subprocess-spawning tests (shared env + worker lifecycle).
fn test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

// ----- fixture grammar ------------------------------------------------------

/// Build (once) a `NXVIM_DATA_DIR` containing a compiled Rust grammar and its
/// highlights query, point the server's worker env at the real `nxvim` binary,
/// and return the data dir. Mirrors how a user installs a parser, but hermetic.
fn fixture_data_dir() -> &'static Path {
    static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
    DATA_DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join("nxvim-ts-fixture");
        let parser_dir = dir.join("parser");
        let query_dir = dir.join("queries").join("rust");
        std::fs::create_dir_all(&parser_dir).unwrap();
        std::fs::create_dir_all(&query_dir).unwrap();

        // Compile the grammar's C sources into parser/rust.so (named `.so` on
        // every OS, which our loader tries first), via the system C compiler.
        let src = grammar_src_dir().join("src");
        let out = parser_dir.join("rust.so");
        let compiler = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
        let status = std::process::Command::new(compiler)
            .args(["-shared", "-fPIC", "-O1"])
            .arg("-I")
            .arg(&src)
            .arg(src.join("parser.c"))
            .arg(src.join("scanner.c"))
            .arg("-o")
            .arg(&out)
            .status()
            .expect("run C compiler");
        assert!(status.success(), "compiling rust grammar fixture failed");

        std::fs::write(
            query_dir.join("highlights.scm"),
            tree_sitter_rust::HIGHLIGHTS_QUERY,
        )
        .unwrap();

        // The engine loads grammars + queries from here, in-process.
        std::env::set_var("NXVIM_DATA_DIR", &dir);
        dir
    })
}

/// Locate the unpacked `tree-sitter-rust` crate source in the cargo registry
/// (a dev-dependency, so cargo guarantees it is present).
fn grammar_src_dir() -> PathBuf {
    let cargo_home = std::env::var("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".cargo"));
    let registry = cargo_home.join("registry").join("src");
    for index in std::fs::read_dir(&registry).expect("read cargo registry src") {
        let candidate = index.unwrap().path().join("tree-sitter-rust-0.24.2");
        if candidate.is_dir() {
            return candidate;
        }
    }
    panic!("tree-sitter-rust-0.24.2 source not found under {registry:?}");
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .expect("HOME")
}

// ----- server harness -------------------------------------------------------

/// Start a server editing `file` (in-process treesitter engine), attach a UI.
async fn start(file: Option<String>) -> (Rpc, UnboundedReceiver<Incoming>) {
    start_with(file, Vec::new()).await
}

/// As [`start`], but seeds the server's `runtimepath` so a `colors/<name>.lua`
/// fixture is findable by `:colorscheme <name>` — the Phase 5 paint path that
/// turns resolved highlight groups into truecolor on screen.
async fn start_with(
    file: Option<String>,
    runtimepath: Vec<PathBuf>,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (server_end, client_end) = tokio::io::duplex(1 << 16);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("server runtime");
        let _ = runtime.block_on(run_server(
            server_end,
            ServerInit {
                file,
                runtimepath,
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

fn feed(rpc: &Rpc, keys: &str) {
    rpc.notify("nvim_input", vec![Value::from(keys)]);
}

async fn exec_lua(rpc: &Rpc, code: &str) -> Value {
    rpc.request(
        "nvim_exec_lua",
        vec![Value::from(code), Value::Array(vec![])],
    )
    .await
    .expect("nvim_exec_lua")
}

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

/// Drain buffered notifications, returning the most recent `redraw` params.
fn drain_latest_redraw(incoming: &mut UnboundedReceiver<Incoming>) -> Option<Vec<Value>> {
    let mut latest = None;
    while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
        if method == "redraw" {
            latest = Some(params);
        }
    }
    latest
}

/// The first window's sub-map (`windows[0]`) from a redraw — where the per-window
/// fields (highlights, scroll, …) now live.
fn window0(params: &[Value]) -> Option<&Vec<(Value, Value)>> {
    let Value::Map(map) = params.first()? else {
        return None;
    };
    let windows = map
        .iter()
        .find(|(k, _)| k.as_str() == Some("windows"))
        .and_then(|(_, v)| v.as_array())?;
    match windows.first()? {
        Value::Map(win) => Some(win),
        _ => None,
    }
}

/// The per-row highlight spans `[(start_col, end_col, group)]` from a redraw map.
fn highlights_of(params: &[Value]) -> Vec<Vec<(u64, u64, String)>> {
    let Some(rows) = window0(params)
        .and_then(|win| win.iter().find(|(k, _)| k.as_str() == Some("highlights")))
        .and_then(|(_, v)| v.as_array())
    else {
        return Vec::new();
    };
    rows.iter()
        .map(|row| {
            row.as_array()
                .map(|spans| {
                    spans
                        .iter()
                        .filter_map(|s| {
                            let a = s.as_array()?;
                            Some((a[0].as_u64()?, a[1].as_u64()?, a[2].as_str()?.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect()
}

/// The scroll-band highlights from a redraw carrying a scroll gesture, or `None`
/// if this redraw has no scroll.
fn scroll_band_highlights(params: &[Value]) -> Option<Vec<Vec<(u64, u64, String)>>> {
    let scroll = window0(params)?
        .iter()
        .find(|(k, _)| k.as_str() == Some("scroll"))
        .map(|(_, v)| v)?;
    let Value::Map(s) = scroll else { return None };
    let rows = s
        .iter()
        .find(|(k, _)| k.as_str() == Some("highlights"))
        .and_then(|(_, v)| v.as_array())?;
    Some(
        rows.iter()
            .map(|row| {
                row.as_array()
                    .map(|spans| {
                        spans
                            .iter()
                            .filter_map(|x| {
                                let a = x.as_array()?;
                                Some((a[0].as_u64()?, a[1].as_u64()?, a[2].as_str()?.to_string()))
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect(),
    )
}

/// Poll up to ~5s for a redraw whose highlights satisfy `done`, returning the
/// raw redraw params.
async fn wait_for_redraw(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    done: impl Fn(&[Vec<(u64, u64, String)>]) -> bool,
) -> Vec<Value> {
    for _ in 0..100 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            if done(&highlights_of(&params)) {
                return params;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("highlights never satisfied the condition within timeout");
}

/// As [`wait_for_redraw`] but returns just the parsed highlight spans.
async fn wait_for_highlights(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    done: impl Fn(&[Vec<(u64, u64, String)>]) -> bool,
) -> Vec<Vec<(u64, u64, String)>> {
    let params = wait_for_redraw(rpc, incoming, done).await;
    highlights_of(&params)
}

/// Write `content` to a fresh temp `.rs` file and return its path as a string.
fn temp_rs(name: &str, content: &str) -> String {
    temp_file(name, "rs", content)
}

/// Write `content` to a fresh temp file with extension `ext`; return its path.
fn temp_file(name: &str, ext: &str, content: &str) -> String {
    let path = std::env::temp_dir().join(format!("nxvim-ts-{}-{}.{ext}", std::process::id(), name));
    std::fs::write(&path, content).unwrap();
    path.display().to_string()
}

/// The `message` line from a redraw map (empty if absent).
fn message_of(params: &[Value]) -> String {
    let Some(Value::Map(map)) = params.first() else {
        return String::new();
    };
    map.iter()
        .find(|(k, _)| k.as_str() == Some("message"))
        .and_then(|(_, v)| v.as_str())
        .unwrap_or("")
        .to_string()
}

// ----- tests ----------------------------------------------------------------

#[tokio::test]
async fn an_edit_repaints_highlights_same_frame() {
    // In-process highlighting is synchronous: the redraw the server emits in
    // response to an edit already carries the spans for the edited text — no
    // second, async catch-up frame. This guards that an edit's *own* redraw
    // lights up a freshly-inserted line with no further client interaction.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = temp_rs("burst", "fn a() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    // Drain the initial highlights for the single line `fn a() {}`.
    wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| !row.is_empty())
    })
    .await;

    // Open a line above and type a fresh function, then leave insert mode. The
    // buffer is now two lines; crucially the *original* `fn a()` moves to row 1,
    // which carried no highlight before — a post-edit-only discriminator.
    feed(&rpc, "ggOfn bbbbbbbb() {}<Esc>");

    // Wait for an *unsolicited* redraw — no `barrier`/poll requests, which would
    // themselves trigger a client-path redraw and mask a missing proactive
    // repaint — that carries the `fn` keyword on the new row 1.
    let mut painted = false;
    for _ in 0..150 {
        match tokio::time::timeout(Duration::from_millis(100), incoming.recv()).await {
            Ok(Some(Incoming::Notification { method, params })) if method == "redraw" => {
                let hl = highlights_of(&params);
                if hl.get(1).is_some_and(|row| {
                    row.iter().any(|(s, e, g)| {
                        *s == 0 && *e == 2 && g.split('.').next() == Some("keyword")
                    })
                }) {
                    painted = true;
                    break;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {} // idle tick
        }
    }
    assert!(
        painted,
        "the edit's own redraw should carry row 1's `fn` keyword (same-frame highlight)"
    );
}

#[tokio::test]
async fn a_rust_buffer_gets_treesitter_highlights() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = temp_rs("basic", "fn main() {\n    let x = 42;\n}\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    // Wait until row 0 (the `fn main` line) carries any spans.
    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| !row.is_empty())
    })
    .await;

    // `fn` (screen cols 0..2) is a keyword.
    let row0 = &hl[0];
    let fn_span = row0
        .iter()
        .find(|(s, _, _)| *s == 0)
        .expect("a span starting at column 0 (the `fn` keyword)");
    assert_eq!(fn_span.1, 2, "the `fn` keyword spans two columns");
    let major = fn_span.2.split('.').next().unwrap();
    assert_eq!(
        major, "keyword",
        "`fn` is a keyword, got group {:?}",
        fn_span.2
    );

    // The number literal `42` on row 1 is highlighted too (proves it's not just
    // a one-token fluke).
    assert!(
        hl.get(1).is_some_and(|row| !row.is_empty()),
        "row 1 (`let x = 42;`) should be highlighted"
    );
}

#[tokio::test]
async fn the_keyword_is_painted_in_its_theme_color() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = temp_rs("paint", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    let params = wait_for_redraw(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| !row.is_empty())
    })
    .await;

    // Paint the real view and inspect the cell grid. The number gutter is on by
    // default at width 4, so the buffer text begins at screen column 4.
    let buf = paint(&View::from_redraw(&params), COLS, ROWS);
    const GUTTER: u16 = 4;
    assert_eq!(buf.cell((GUTTER, 0)).unwrap().symbol(), "f");
    assert_eq!(
        buf.cell((GUTTER, 0)).unwrap().style().fg,
        Some(Color::Magenta),
        "the `fn` keyword should paint in the keyword color"
    );
}

// catppuccin-mocha-ish hex values the colors fixture below sets, mirrored here
// as the RGB the painted cells must carry once the theme loads.
const NORMAL_BG: Color = Color::Rgb(0x1e, 0x1e, 0x2e);
const KEYWORD: Color = Color::Rgb(0xcb, 0xa6, 0xf7); // mauve
const STRING: Color = Color::Rgb(0xa6, 0xe3, 0xa1); // green
const VISUAL_BG: Color = Color::Rgb(0x45, 0x47, 0x5a); // surface1
const CURSOR_LINE_NR: Color = Color::Rgb(0xfa, 0xb3, 0x87); // peach

/// A `colors/cattest.lua` standing in for catppuccin: a handful of
/// `nvim_set_hl` calls for the groups the paint test asserts (the syntax groups
/// plus the editor chrome). `:colorscheme cattest` sources this off the
/// runtimepath, exactly as the real plugin's `colors/catppuccin.lua` is sourced.
const COLORS_FIXTURE: &str = "\
vim.api.nvim_set_hl(0, 'Normal', { fg = '#cdd6f4', bg = '#1e1e2e' })\n\
vim.api.nvim_set_hl(0, 'Keyword', { fg = '#cba6f7' })\n\
vim.api.nvim_set_hl(0, 'String', { fg = '#a6e3a1' })\n\
vim.api.nvim_set_hl(0, 'Visual', { bg = '#45475a' })\n\
vim.api.nvim_set_hl(0, 'CursorLineNr', { fg = '#fab387' })\n\
vim.api.nvim_set_hl(0, 'LineNr', { fg = '#6c7086' })\n\
vim.api.nvim_set_hl(0, 'StatusLine', { fg = '#cdd6f4', bg = '#313244' })\n";

/// Create a fresh runtimepath dir holding the `colors/cattest.lua` fixture.
fn theme_runtimepath(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nxvim-theme-{}-{}", std::process::id(), tag));
    std::fs::create_dir_all(dir.join("colors")).unwrap();
    std::fs::write(dir.join("colors").join("cattest.lua"), COLORS_FIXTURE).unwrap();
    dir
}

/// Poll redraws (bounded) until the painted view satisfies `done`, returning
/// `(redraw params, painted buffer)`. Highlight resolution and the colorscheme
/// load are independent async events, so we wait for the screen to actually
/// reflect both rather than for a single barrier.
async fn paint_until(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    done: impl Fn(&Buffer) -> bool,
) -> (Vec<Value>, Buffer) {
    for _ in 0..100 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            let buf = paint(&View::from_redraw(&params), COLS, ROWS);
            if done(&buf) {
                return (params, buf);
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the painted screen never satisfied the condition within timeout");
}

/// Phase 5, end to end: a loaded colorscheme turns the screen truecolor. Open a
/// Rust source file, source a catppuccin-shaped `colors/` fixture via
/// `:colorscheme`, and assert the real client paints the resolved styles —
/// keyword foreground, string foreground, the editor background, the
/// cursor-line gutter, and the visual selection.
#[tokio::test]
async fn a_loaded_colorscheme_paints_resolved_styles_truecolor() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let rtp = theme_runtimepath("paint");
    // `fn` is a keyword; `"hi"` is a string — both on row 0.
    let file = temp_rs("themed", "fn main() { let s = \"hi\"; }\n");
    let (rpc, mut incoming) = start_with(Some(file), vec![rtp]).await;

    // Wait for treesitter spans (painted in the fallback theme), then load the
    // colorscheme so subsequent redraws resolve those spans to truecolor.
    wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| !row.is_empty())
    })
    .await;
    feed(&rpc, ":colorscheme cattest<CR>");

    const GUTTER: u16 = 4; // hybrid number column, width 4 → text at col 4
    let (params, buf) = paint_until(&rpc, &mut incoming, |buf| {
        buf.cell((GUTTER, 0)).unwrap().style().fg == Some(KEYWORD)
    })
    .await;

    // The `fn` keyword paints mauve, sitting on the Normal background.
    let kw = buf.cell((GUTTER, 0)).unwrap().style();
    assert_eq!(kw.fg, Some(KEYWORD), "the `fn` keyword paints mauve");
    assert_eq!(
        kw.bg,
        Some(NORMAL_BG),
        "themed text sits on the Normal background"
    );

    // The string literal paints green. Locate its span in the resolved spans.
    let hl = highlights_of(&params);
    let str_start = hl[0]
        .iter()
        .find(|(_, _, group)| group.split('.').next() == Some("string"))
        .map(|(start, _, _)| *start as u16)
        .expect("a string span on row 0");
    assert_eq!(
        buf.cell((GUTTER + str_start, 0)).unwrap().style().fg,
        Some(STRING),
        "the string literal paints green"
    );

    // The cursor line's gutter number uses CursorLineNr (the cursor is on row 0).
    assert_eq!(
        buf.cell((0, 0)).unwrap().style().fg,
        Some(CURSOR_LINE_NR),
        "the cursor-line number uses CursorLineNr"
    );

    // A visual selection takes the theme's Visual background — not reverse-video.
    feed(&rpc, "v");
    let (_, sel) = paint_until(&rpc, &mut incoming, |buf| {
        buf.cell((GUTTER, 0)).unwrap().style().bg == Some(VISUAL_BG)
    })
    .await;
    let cell = sel.cell((GUTTER, 0)).unwrap().style();
    assert_eq!(cell.bg, Some(VISUAL_BG), "the selection uses Visual's bg");
    assert_eq!(
        cell.fg,
        Some(KEYWORD),
        "the selected keyword keeps its mauve foreground under Visual"
    );
    assert!(
        !cell.add_modifier.contains(Modifier::REVERSED),
        "Visual replaces reverse-video rather than compositing onto it"
    );
}

#[tokio::test]
async fn the_scroll_animation_band_is_highlighted() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    // A buffer taller than the viewport, so <C-d> produces a scroll gesture.
    let content: String = (0..60)
        .map(|i| format!("fn f{i}() {{ let x = {i}; }}\n"))
        .collect();
    let file = temp_rs("scroll", &content);
    let (rpc, mut incoming) = start(Some(file)).await;
    // Ensure spans are cached (including the one-screen overscan below).
    wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| !row.is_empty())
    })
    .await;

    // Half-page scroll: the redraw must carry a scroll band that is *already*
    // colored (the fix for the white flash — the revealed lines are pre-cached).
    feed(&rpc, "<C-d>");
    let mut band = None;
    for _ in 0..40 {
        barrier(&rpc).await;
        tokio::task::yield_now().await;
        while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
            if method == "redraw" {
                if let Some(hl) = scroll_band_highlights(&params) {
                    band = Some(hl);
                }
            }
        }
        if band.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let band = band.expect("a redraw carrying a scroll gesture");
    assert!(
        band.iter().filter(|row| !row.is_empty()).count() >= 5,
        "the scroll band should be colored, not flashing white (rows colored: {})",
        band.iter().filter(|row| !row.is_empty()).count()
    );
}

#[tokio::test]
async fn an_extmark_wins_over_treesitter_by_priority() {
    // An extmark highlight (default priority 4096) painted over a treesitter
    // span (priority 100) replaces it for the overlapping cells: the merge
    // resolves overlaps server-side into non-overlapping spans, so the `fn`
    // keyword's cells carry the extmark's group, not `keyword`. Proves the
    // priority layering of the decoration layer on top of the syntax engine.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = temp_rs("ext-prio", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    // Wait for the treesitter `keyword` span on `fn` (cols 0..2) first.
    wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| {
            row.iter()
                .any(|(s, e, g)| *s == 0 && *e == 2 && g.split('.').next() == Some("keyword"))
        })
    })
    .await;

    // Cover `fn` with an extmark in a custom group.
    exec_lua(
        &rpc,
        r#"
        local ns = vim.api.nvim_create_namespace('prio')
        vim.api.nvim_buf_set_extmark(0, ns, 0, 0, { end_row = 0, end_col = 2, hl_group = 'ExtMark' })
        "#,
    )
    .await;

    // Cols 0..2 now belong to the extmark group; `keyword` no longer covers them.
    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| {
            row.iter()
                .any(|(s, e, g)| *s == 0 && *e == 2 && g == "ExtMark")
        })
    })
    .await;
    let row0 = &hl[0];
    let col0 = row0
        .iter()
        .find(|(s, _, _)| *s == 0)
        .expect("a span at column 0");
    assert_eq!(
        (col0.1, col0.2.as_str()),
        (2, "ExtMark"),
        "the extmark outranks the treesitter keyword over cols 0..2"
    );
    assert!(
        !row0
            .iter()
            .any(|(s, _, g)| *s == 0 && g.split('.').next() == Some("keyword")),
        "the treesitter keyword span no longer covers column 0"
    );
}

#[tokio::test]
async fn a_plain_text_buffer_has_no_highlights() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    // No path / unknown filetype: no grammar, no highlights, ever.
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // Settle a few frames, then assert nothing was ever highlighted.
    for _ in 0..6 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let params = drain_latest_redraw(&mut incoming).expect("a redraw");
    let hl = highlights_of(&params);
    assert!(
        hl.iter().all(|row| row.is_empty()),
        "a no-name buffer must never be highlighted"
    );
}

#[tokio::test]
async fn switching_buffers_shows_each_buffers_own_highlights() {
    // Per-buffer syntax state: each open buffer keeps its own span cache, so
    // switching back to one shows *its* highlights, never the other buffer's.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let a = temp_rs("switch-a", "fn main() {}\n");
    let b = temp_rs("switch-b", "struct S {}\n");
    let (rpc, mut incoming) = start(Some(a)).await;

    // Buffer A highlighted: capture row 0's spans.
    let a_hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| !row.is_empty())
    })
    .await;
    let a_row0 = a_hl[0].clone();

    // Open B in a second buffer; its row 0 differs from A's.
    rpc.request("nvim_command", vec![Value::from(format!("e {b}"))])
        .await
        .expect("edit b");
    let b_hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first()
            .is_some_and(|row| !row.is_empty() && *row != a_row0)
    })
    .await;
    assert_ne!(b_hl[0], a_row0, "B should highlight differently from A");

    // Switch back to A with <C-^>: A's own spans return (not B's), proving the
    // per-buffer cache is keyed and routed by buffer id.
    feed(&rpc, "<C-^>");
    let back = wait_for_highlights(&rpc, &mut incoming, |hl| hl.first() == Some(&a_row0)).await;
    assert_eq!(back[0], a_row0, "switching back to A shows A's highlights");
}

/// A data dir holding a parser file that is **present but not a valid grammar**
/// (garbage bytes — `dlopen` rejects it), under `parser/python.so`. Stands in for
/// a corrupt / wrong-arch / ABI-mismatched installed grammar.
fn broken_data_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nxvim-ts-broken-{}", std::process::id()));
    let parser_dir = dir.join("parser");
    std::fs::create_dir_all(&parser_dir).unwrap();
    std::fs::write(parser_dir.join("python.so"), b"not a real shared object").unwrap();
    dir
}

#[tokio::test]
async fn a_broken_grammar_echoes_a_load_failure() {
    // A grammar that is *installed but fails to load* (bad ABI / corrupt .so) is
    // worth telling the user about — unlike a missing one. The editor surfaces it
    // synchronously (in-process) the first time the buffer is opened in the engine.
    let _guard = test_lock().lock().await;
    fixture_data_dir(); // establish the baseline data dir so we can restore it
    let saved = std::env::var_os("NXVIM_DATA_DIR");
    let broken = broken_data_dir();
    std::env::set_var("NXVIM_DATA_DIR", &broken);

    let file = temp_file("broken", "py", "x = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    let mut seen = false;
    for _ in 0..100 {
        barrier(&rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(&mut incoming) {
            if message_of(&params).contains("failed to load") {
                seen = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Restore the data dir for the sibling tests (they expect the rust fixture).
    match saved {
        Some(v) => std::env::set_var("NXVIM_DATA_DIR", v),
        None => std::env::remove_var("NXVIM_DATA_DIR"),
    }

    assert!(
        seen,
        "a present-but-broken grammar should echo a load failure"
    );
}

#[tokio::test]
async fn a_missing_grammar_is_silent() {
    // The common case: no parser installed for the language. Highlighting is
    // best-effort, so the buffer is silently un-highlighted — *no* error message.
    // (The rust fixture dir has a rust grammar but no python one.)
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = temp_file("missing", "py", "x = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    // Settle several frames so any (erroneous) message would have appeared.
    for _ in 0..6 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let params = drain_latest_redraw(&mut incoming).expect("a redraw");
    assert!(
        !message_of(&params).contains("failed to load"),
        "a missing grammar must not echo a load failure: {:?}",
        message_of(&params)
    );
    assert!(
        highlights_of(&params).iter().all(|row| row.is_empty()),
        "a buffer with no installed grammar must not be highlighted"
    );
}
