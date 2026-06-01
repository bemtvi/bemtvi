//! Treesitter syntax highlighting, end to end through the real stack: the
//! in-process server spawns the **real** `nxvim` binary as the crash-isolated
//! syntax worker (`NXVIM_TS_WORKER`), which loads a grammar we compile into a
//! temp `NXVIM_DATA_DIR` fixture. Highlighting is asynchronous, so these tests
//! poll redraws until the spans arrive (bounded wait) rather than using a single
//! barrier.
//!
//! These tests spawn subprocesses and share process-global env, so they
//! serialize on a single lock.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
use nxvim_tui::{paint, ScrollHarness, View};
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

        // The worker is the real nxvim binary; point the server at it (the test
        // binary, not nxvim, is this process's current_exe).
        std::env::set_var("NXVIM_TS_WORKER", env!("CARGO_BIN_EXE_nxvim"));
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

/// Start a server (worker spawning enabled) editing `file`, attach a UI.
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

/// The per-row highlight spans `[(start_col, end_col, group)]` from a redraw map.
fn highlights_of(params: &[Value]) -> Vec<Vec<(u64, u64, String)>> {
    let Some(Value::Map(map)) = params.first() else {
        return Vec::new();
    };
    let Some(rows) = map
        .iter()
        .find(|(k, _)| k.as_str() == Some("highlights"))
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
    let Some(Value::Map(map)) = params.first() else {
        return None;
    };
    let scroll = map
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
    let path = std::env::temp_dir().join(format!("nxvim-ts-{}-{}.rs", std::process::id(), name));
    std::fs::write(&path, content).unwrap();
    path.display().to_string()
}

// ----- tests ----------------------------------------------------------------

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
async fn editing_a_huge_file_sends_only_a_tiny_delta() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    // Record what the worker receives, so we can assert delta sizes.
    let record = std::env::temp_dir().join(format!("nxvim-ts-record-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&record);
    std::env::set_var("NXVIM_TS_RECORD", &record);

    // A genuinely large buffer.
    let line = "fn f() { let value = 123; }\n";
    let huge = line.repeat(40_000);
    let file = temp_rs("huge", &huge);

    let (rpc, mut incoming) = start(Some(file)).await;
    // Initial open + highlights.
    wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| !row.is_empty())
    })
    .await;

    // Type one character, then wait for the follow-up highlight pass.
    feed(&rpc, "ix");
    feed(&rpc, "<Esc>");
    for _ in 0..40 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        if record_has_edit(&record) {
            break;
        }
    }
    std::env::remove_var("NXVIM_TS_RECORD");

    let log = std::fs::read_to_string(&record).unwrap_or_default();
    // Exactly one full-text open, sized to the file...
    let open = log
        .lines()
        .find(|l| l.starts_with("ts_open"))
        .expect("an initial ts_open");
    let open_len: usize = open
        .split("text=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap();
    assert!(
        open_len > 1_000_000,
        "ts_open should carry the whole huge file, got text={open_len}"
    );

    // ...and the edit after it carries a *tiny* delta, not the file.
    let edit = log
        .lines()
        .find(|l| l.starts_with("ts_edit"))
        .expect("a ts_edit after typing");
    let delta: usize = edit
        .split("delta=")
        .nth(1)
        .and_then(|s| s.trim().parse().ok())
        .unwrap();
    assert!(
        delta <= 4,
        "typing one char into a {open_len}-byte file must send a tiny delta, got delta={delta}"
    );
}

fn record_has_edit(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|s| s.lines().any(|l| l.starts_with("ts_edit")))
        .unwrap_or(false)
}

#[tokio::test]
async fn the_editor_survives_a_crashing_worker() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    // A `.crash` file selects the reserved `__crash` language, whose worker
    // aborts on every open — a stand-in for a segfaulting C grammar. The server
    // respawns it (and the circuit breaker eventually throttles the loop), but
    // crucially the editor must stay fully responsive throughout.
    let path = std::env::temp_dir().join(format!("nxvim-ts-{}-crash.crash", std::process::id()));
    std::fs::write(&path, "hello\n").unwrap();
    let (rpc, _incoming) = start(Some(path.display().to_string())).await;

    // Hammer the editor with edits while the worker is busy crash-looping.
    feed(&rpc, "ggdGiline one<CR>line two<CR>line three<Esc>");
    barrier(&rpc).await;
    // ...and it applied every keystroke, unaffected by the dying worker.
    let lines = rpc
        .request(
            "nvim_buf_get_lines",
            vec![
                Value::from(0u64),
                Value::from(0i64),
                Value::from(-1i64),
                Value::Boolean(false),
            ],
        )
        .await
        .expect("editor still responds while the worker crashes");
    let lines: Vec<String> = match lines {
        Value::Array(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => vec![],
    };
    assert_eq!(
        lines,
        vec![
            "line one".to_string(),
            "line two".to_string(),
            "line three".to_string()
        ]
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
async fn a_plain_text_buffer_has_no_highlights() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    // No path / unknown filetype: no worker, no highlights, ever.
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // Give any (erroneous) worker time to respond, then assert nothing appeared.
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

/// True if this redraw carries a (non-nil) scroll gesture.
fn redraw_has_scroll(params: &[Value]) -> bool {
    let Some(Value::Map(map)) = params.first() else {
        return false;
    };
    map.iter()
        .find(|(k, _)| k.as_str() == Some("scroll"))
        .map(|(_, v)| !matches!(v, Value::Nil))
        .unwrap_or(false)
}

/// The scroll gesture's destination top line (`to_top`), if this redraw carries
/// one.
fn scroll_to_top(params: &[Value]) -> Option<usize> {
    let Some(Value::Map(map)) = params.first() else {
        return None;
    };
    let Value::Map(s) = map
        .iter()
        .find(|(k, _)| k.as_str() == Some("scroll"))
        .map(|(_, v)| v)?
    else {
        return None;
    };
    s.iter()
        .find(|(k, _)| k.as_str() == Some("to_top"))
        .and_then(|(_, v)| v.as_u64())
        .map(|n| n as usize)
}

/// Wait (passively — *no* `barrier`, which would itself make the server redraw)
/// for the next `redraw` notification whose params satisfy `pred`.
async fn next_redraw(
    incoming: &mut UnboundedReceiver<Incoming>,
    pred: impl Fn(&[Value]) -> bool,
) -> Option<Vec<Value>> {
    for _ in 0..150 {
        while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
            if method == "redraw" && pred(&params) {
                return Some(params);
            }
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    None
}

/// The index `N` of the `fN(...)` function on the top text row of a painted
/// frame — i.e. the buffer line the viewport currently starts at — or `None` if
/// the row carries no such token. The fixture names line `i` `fn fi()`, so this
/// reads back the scroll position straight off the screen.
fn top_fn_index(buf: &Buffer) -> Option<usize> {
    let row: String = (0..buf.area.width)
        .map(|x| buf.cell((x, 0)).map(|c| c.symbol()).unwrap_or(""))
        .collect();
    let at = row.find("fn f")? + "fn f".len();
    row[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}

/// Smooth scrolling must survive an incidental syntax-highlight repaint.
///
/// On `<C-d>` the server sends one redraw carrying the `scroll` gesture (the
/// client arms a local slide), then — because the scrolled viewport drives the
/// syntax worker — a second redraw with the worker's highlights for the *same*
/// destination viewport and **no** scroll gesture. The client must treat that
/// second redraw as a repaint, not as an interruption: the slide should keep
/// playing. Today it clears the animation, snapping straight to the destination.
///
/// This drives the client's real render-state machine ([`ScrollHarness`], which
/// shares the event loop's `arm_animation`) with the actual redraws the server
/// emits, so it pins client behavior regardless of how the server schedules its
/// repaints.
#[tokio::test]
async fn a_highlight_repaint_does_not_snap_an_in_flight_scroll() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    // A buffer much taller than the viewport, so <C-d> is a real scroll.
    let content: String = (0..200)
        .map(|i| format!("fn f{i}() {{ let x = {i}; }}\n"))
        .collect();
    let file = temp_rs("scroll-anim", &content);
    let (rpc, mut incoming) = start(Some(file)).await;

    // Warm the worker so spans are cached and no request is in flight — the
    // state in which a scroll fires an *immediate* highlight repaint.
    wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| !row.is_empty())
    })
    .await;
    while incoming.try_recv().is_ok() {}

    // Capture the two real redraws the client would receive, in order.
    feed(&rpc, "<C-d>");
    let scroll = next_redraw(&mut incoming, redraw_has_scroll)
        .await
        .expect("a redraw carrying the scroll gesture");
    let repaint = next_redraw(&mut incoming, |p| !redraw_has_scroll(p))
        .await
        .expect("a follow-up highlight repaint with no scroll gesture");

    // `<C-d>` half-pages down: the viewport slides from line 0 toward `dest`.
    let dest = scroll_to_top(&scroll).expect("scroll redraw carries a destination top");
    assert!(dest > 0, "the scroll should move the viewport down");

    // Replay the two redraws through the client's render-state machine, back to
    // back: the slide is still near its start when the repaint lands.
    let mut client = ScrollHarness::new();
    client.on_redraw(&scroll);
    assert!(client.animating(), "the scroll redraw should arm the slide");
    let started = top_fn_index(&client.paint(COLS, ROWS)).expect("a top line");
    assert!(
        started < dest,
        "the freshly-armed slide should start near the origin, not at the destination",
    );

    client.on_redraw(&repaint);
    let top = top_fn_index(&client.paint(COLS, ROWS));
    assert!(
        client.animating(),
        "a highlight repaint cleared the in-flight scroll slide (it should keep playing)",
    );
    assert!(
        top.is_some_and(|n| n < dest),
        "after the highlight repaint the slide snapped to its destination line {dest} \
         (top line is now {top:?}) instead of continuing the slide",
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

#[tokio::test]
async fn bdelete_sends_ts_close_to_the_worker() {
    // Deleting a buffer frees its parse state in the worker via `ts_close`.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let record = std::env::temp_dir().join(format!("nxvim-ts-close-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&record);
    std::env::set_var("NXVIM_TS_RECORD", &record);

    let a = temp_rs("close-a", "fn a() {}\n");
    let b = temp_rs("close-b", "fn b() {}\n");
    let (rpc, mut incoming) = start(Some(a)).await;
    wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| !row.is_empty())
    })
    .await;
    rpc.request("nvim_command", vec![Value::from(format!("e {b}"))])
        .await
        .expect("edit b");
    wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| !row.is_empty())
    })
    .await;

    // Delete the current buffer (b); the server reaps it and sends `ts_close`.
    rpc.request("nvim_command", vec![Value::from("bd")])
        .await
        .expect("bdelete");

    let mut closed = false;
    for _ in 0..100 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        if std::fs::read_to_string(&record)
            .unwrap_or_default()
            .lines()
            .any(|l| l.starts_with("ts_close"))
        {
            closed = true;
            break;
        }
    }
    std::env::remove_var("NXVIM_TS_RECORD");
    let _ = std::fs::remove_file(&record);
    assert!(closed, "expected a ts_close for the deleted buffer");
}
