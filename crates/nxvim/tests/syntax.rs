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

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    drain_latest_redraw, exec_lua, feed, message_of, serial_lock as test_lock, start_attached,
    window0, write_temp,
};
use nxvim_tui::paint;
use nxvim_view::View;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const COLS: u16 = 80;
const ROWS: u16 = 24;

// ----- fixture grammar ------------------------------------------------------

/// Build (once) a `NXVIM_DATA_DIR` containing a compiled Rust grammar and its
/// highlights query, point the server's worker env at the real `nxvim` binary,
/// and return the data dir. Mirrors how a user installs a parser, but hermetic.
fn fixture_data_dir() -> &'static Path {
    static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
    DATA_DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join("nxvim-ts-fixture");
        std::fs::create_dir_all(dir.join("parser")).unwrap();

        // `rust`: the workhorse grammar most tests use. Its highlights query is the
        // crate constant; no injection query on disk (injection tests set their own).
        let rust_src = registry_crate_dir("tree-sitter-rust-0.24.2").join("src");
        compile_grammar(&dir, "rust", &rust_src);
        write_query(
            &dir,
            "rust",
            "highlights",
            tree_sitter_rust::HIGHLIGHTS_QUERY,
        );

        // `markdown`: a host grammar that *injects* — its `injections.scm` maps a
        // fenced code block to the fence's language, so the injection tests can
        // drive cross-language (markdown → rust) and nested (markdown → rust → …)
        // injections, not just rust-in-rust. Only the block grammar is installed;
        // its `markdown_inline` / html / yaml injections resolve to uninstalled
        // grammars and are silently skipped (best-effort), which is fine here.
        let md = registry_crate_dir("tree-sitter-md-0.5.3").join("tree-sitter-markdown");
        compile_grammar(&dir, "markdown", &md.join("src"));
        for name in ["highlights", "injections"] {
            let scm = std::fs::read_to_string(md.join("queries").join(format!("{name}.scm")))
                .expect("read markdown query");
            write_query(&dir, "markdown", name, &scm);
        }

        // The engine loads grammars + queries from here, in-process.
        std::env::set_var("NXVIM_DATA_DIR", &dir);
        dir
    })
}

/// Compile a grammar's C sources (`parser.c` + the always-present `scanner.c`) from
/// `src_dir` into `<data>/parser/<lang>.so` (named `.so` on every OS, which our
/// loader tries first), via the system C compiler — mirroring how a user installs a
/// parser, but hermetic.
fn compile_grammar(data_dir: &Path, lang: &str, src_dir: &Path) {
    let out = data_dir.join("parser").join(format!("{lang}.so"));
    let compiler = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let status = std::process::Command::new(compiler)
        .args(["-shared", "-fPIC", "-O1"])
        .arg("-I")
        .arg(src_dir)
        .arg(src_dir.join("parser.c"))
        .arg(src_dir.join("scanner.c"))
        .arg("-o")
        .arg(&out)
        .status()
        .expect("run C compiler");
    assert!(status.success(), "compiling {lang} grammar fixture failed");
}

/// Write `<data>/queries/<lang>/<name>.scm`.
fn write_query(data_dir: &Path, lang: &str, name: &str, scm: &str) {
    let dir = data_dir.join("queries").join(lang);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{name}.scm")), scm).unwrap();
}

/// Locate an unpacked crate source directory in the cargo registry by its
/// `<name>-<version>` folder name (a dev-dependency, so cargo guarantees presence).
fn registry_crate_dir(crate_dir: &str) -> PathBuf {
    let cargo_home = std::env::var("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".cargo"));
    let registry = cargo_home.join("registry").join("src");
    for index in std::fs::read_dir(&registry).expect("read cargo registry src") {
        let candidate = index.unwrap().path().join(crate_dir);
        if candidate.is_dir() {
            return candidate;
        }
    }
    panic!("{crate_dir} source not found under {registry:?}");
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
    start_full(file, runtimepath, None).await
}

/// As [`start_with`], but also sources `config_dir/init.lua` at startup — the
/// real config-load path, used to exercise an `examples/<feature>/init.lua`
/// end-to-end (so the shipped example can't rot).
async fn start_with_config(
    file: Option<String>,
    config_dir: PathBuf,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    start_full(file, Vec::new(), Some(config_dir)).await
}

async fn start_full(
    file: Option<String>,
    runtimepath: Vec<PathBuf>,
    config_dir: Option<PathBuf>,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(
        ServerInit {
            file,
            runtimepath,
            config_dir,
            ..Default::default()
        },
        COLS,
        ROWS - 2,
    )
    .await
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
    write_temp(name, "rs", content)
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

    let file = write_temp("broken", "py", "x = 1\n");
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
    let file = write_temp("missing", "py", "x = 1\n");
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

// ----- vim.treesitter.start / stop bridge (ADR 0001, #1) --------------------

#[tokio::test]
async fn treesitter_start_highlights_a_buffer_the_extension_table_misses() {
    // A `.txt` file: `language_of_path` has no mapping, so nxvim's extension
    // floor never highlights it. `vim.treesitter.start(0, 'rust')` forces the
    // native engine on at `rust` regardless of extension — the common case the
    // bridge unblocks (getting *any* highlighting onto a buffer the table misses).
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = write_temp("ts-start", "txt", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    // Before `start`: a `.txt` buffer is never highlighted.
    for _ in 0..6 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let before = drain_latest_redraw(&mut incoming).expect("a redraw");
    assert!(
        highlights_of(&before).iter().all(|row| row.is_empty()),
        "a .txt buffer must not be highlighted before vim.treesitter.start"
    );

    // Turn the native engine on for this buffer at `rust`.
    exec_lua(&rpc, "vim.treesitter.start(0, 'rust')").await;

    // Now the `fn` keyword on row 0 is highlighted by the rust grammar.
    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| !row.is_empty())
    })
    .await;
    let fn_span = hl[0]
        .iter()
        .find(|(s, _, _)| *s == 0)
        .expect("a span at column 0 (the `fn` keyword) after start");
    assert_eq!(fn_span.1, 2, "the `fn` keyword spans two columns");
    assert_eq!(
        fn_span.2.split('.').next().unwrap(),
        "keyword",
        "`fn` is a keyword, got group {:?}",
        fn_span.2
    );
}

#[tokio::test]
async fn the_treesitter_start_example_config_highlights_on_startup() {
    // The shipped `examples/treesitter-start/` config calls
    // `vim.treesitter.start(0, "rust")` at the top level of init.lua. Sourcing it
    // at startup (against its own `.txt` sample, which the extension table misses)
    // must leave the buffer rust-highlighted on the first frame — verifying the
    // example end-to-end and guarding it against rot.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let example = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/treesitter-start")
        .canonicalize()
        .expect("example dir exists");
    let sample = example.join("sample.txt").display().to_string();
    let (rpc, mut incoming) = start_with_config(Some(sample), example).await;

    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| !row.is_empty())
    })
    .await;
    let fn_span = hl[0]
        .iter()
        .find(|(s, _, _)| *s == 0)
        .expect("a span at column 0 (the `fn` keyword) from the example config");
    assert_eq!(
        fn_span.2.split('.').next().unwrap(),
        "keyword",
        "the example's vim.treesitter.start should highlight `fn` as a keyword, got {:?}",
        fn_span.2
    );
}

#[tokio::test]
async fn treesitter_stop_clears_highlighting_even_for_a_known_extension() {
    // A `.rs` buffer auto-highlights off the extension floor. `vim.treesitter.stop`
    // is an *explicit* off switch: it must darken the buffer even though the
    // extension is recognized (override `Some(None)` beats `language_of_path`).
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = temp_rs("ts-stop", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    // It starts highlighted by the extension floor.
    wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| !row.is_empty())
    })
    .await;

    // Stop, then confirm the buffer goes dark and stays dark.
    exec_lua(&rpc, "vim.treesitter.stop(0)").await;
    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.iter().all(|row| row.is_empty())
    })
    .await;
    assert!(
        hl.iter().all(|row| row.is_empty()),
        "vim.treesitter.stop must clear highlighting for a .rs buffer"
    );
}

// ----- query-resolution bridge (ADR 0001, #4) -------------------------------

/// Does row 0 carry a span of capture group `group` (major part before any `.`)?
fn row0_has_group(hl: &[Vec<(u64, u64, String)>], group: &str) -> bool {
    hl.first().is_some_and(|row| {
        row.iter()
            .any(|(_, _, g)| g.split('.').next() == Some(group))
    })
}

/// A keyword span sitting at column 0 (the `fn` of `fn main`).
fn row0_keyword_at_0(hl: &[Vec<(u64, u64, String)>]) -> bool {
    hl.first().is_some_and(|row| {
        row.iter()
            .any(|(s, _, g)| *s == 0 && g.split('.').next() == Some("keyword"))
    })
}

#[tokio::test]
async fn query_set_replaces_the_engine_highlights_query() {
    // `vim.treesitter.query.set(lang, 'highlights', text)` (no modeline) REPLACES
    // the query, exactly as in neovim. The engine must paint with the new query:
    // `(identifier) @variable` lights up `main` but no longer the `fn` keyword.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = temp_rs("q-set", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    // Baseline: the disk query highlights `fn` as a keyword at column 0.
    wait_for_highlights(&rpc, &mut incoming, row0_keyword_at_0).await;

    // Override with an identifier-only query.
    exec_lua(
        &rpc,
        "vim.treesitter.query.set('rust', 'highlights', '(identifier) @variable')",
    )
    .await;

    // The keyword span is gone; `main` is now captured as @variable.
    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        !row0_keyword_at_0(hl) && row0_has_group(hl, "variable")
    })
    .await;
    assert!(
        !row0_keyword_at_0(&hl),
        "the replaced query no longer paints the `fn` keyword: {hl:?}"
    );
    assert!(
        row0_has_group(&hl, "variable"),
        "the replaced query paints `main` as @variable: {hl:?}"
    );
}

#[tokio::test]
async fn query_set_with_extends_merges_onto_the_base_query() {
    // A `;extends` modeline merges the override ON TOP of the base query rather
    // than replacing it. The merge runs in the vendored Lua (`query.get` prepends
    // the base file found on the runtimepath — which now includes the engine's
    // data dir), so the engine paints BOTH the base `fn` keyword and the added
    // @variable on `main`. This is the case the loader could never do alone.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = temp_rs("q-extends", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    wait_for_highlights(&rpc, &mut incoming, row0_keyword_at_0).await;

    exec_lua(
        &rpc,
        "vim.treesitter.query.set('rust', 'highlights', ';extends\\n(identifier) @variable')",
    )
    .await;

    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        row0_keyword_at_0(hl) && row0_has_group(hl, "variable")
    })
    .await;
    assert!(
        row0_keyword_at_0(&hl),
        "`;extends` keeps the base `fn` keyword: {hl:?}"
    );
    assert!(
        row0_has_group(&hl, "variable"),
        "`;extends` adds the @variable capture on top: {hl:?}"
    );
}

#[tokio::test]
async fn a_broken_set_query_echoes_loud_and_keeps_the_old_paint() {
    // No silent stubs: an override that fails to compile must echo loud (like a
    // broken on-disk query), not swallow the error. The previously compiled query
    // is left in place, so the buffer degrades to "unchanged", never to dark.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = temp_rs("q-broken", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    wait_for_highlights(&rpc, &mut incoming, row0_keyword_at_0).await;

    exec_lua(
        &rpc,
        "vim.treesitter.query.set('rust', 'highlights', '((((')",
    )
    .await;

    // The compile failure is surfaced on the message line.
    let mut seen = false;
    for _ in 0..100 {
        barrier(&rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(&mut incoming) {
            if message_of(&params).contains("failed to compile") {
                seen = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(seen, "a broken query.set must echo a compile failure");

    // And the buffer still paints the prior (base) query — not dark.
    let hl = wait_for_highlights(&rpc, &mut incoming, row0_keyword_at_0).await;
    assert!(
        row0_keyword_at_0(&hl),
        "a broken override keeps the previous paint: {hl:?}"
    );
}

#[tokio::test]
async fn clearing_a_set_query_reverts_to_the_disk_query() {
    // `query.set(lang, name, nil)` drops the override; the engine reverts to the
    // on-disk query (resolved back through Lua off the data-dir runtimepath entry),
    // so the `fn` keyword the replace had removed comes back.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = temp_rs("q-clear", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    wait_for_highlights(&rpc, &mut incoming, row0_keyword_at_0).await;

    // Replace (keyword disappears), then clear (keyword returns).
    exec_lua(
        &rpc,
        "vim.treesitter.query.set('rust', 'highlights', '(identifier) @variable')",
    )
    .await;
    wait_for_highlights(&rpc, &mut incoming, |hl| !row0_keyword_at_0(hl)).await;

    exec_lua(&rpc, "vim.treesitter.query.set('rust', 'highlights', nil)").await;
    let hl = wait_for_highlights(&rpc, &mut incoming, row0_keyword_at_0).await;
    assert!(
        row0_keyword_at_0(&hl),
        "clearing the override restores the disk query's `fn` keyword: {hl:?}"
    );
}

#[tokio::test]
async fn the_treesitter_query_example_config_extends_on_startup() {
    // The shipped `examples/treesitter-query/` config calls
    // `vim.treesitter.query.set('rust','highlights', ';extends\n(identifier) @variable')`
    // at the top level of init.lua. Sourced at startup against its own sample, the
    // buffer must paint BOTH the base `fn` keyword and the added @variable — the
    // query bridge resolving + pushing the merge end-to-end through the example.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let example = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/treesitter-query")
        .canonicalize()
        .expect("example dir exists");
    let sample = example.join("sample.rs").display().to_string();
    let (rpc, mut incoming) = start_with_config(Some(sample), example).await;

    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        row0_keyword_at_0(hl) && row0_has_group(hl, "variable")
    })
    .await;
    assert!(
        row0_keyword_at_0(&hl),
        "the example keeps the base `fn` keyword: {hl:?}"
    );
    assert!(
        row0_has_group(&hl, "variable"),
        "the example's ;extends adds @variable on `main`: {hl:?}"
    );
}

/// Create a fresh runtimepath dir holding an on-disk `queries/rust/highlights.scm`
/// overlay with the given text (a `;extends` modeline merges onto the engine's
/// base query, which also sits on the runtimepath via `NXVIM_DATA_DIR`).
fn query_overlay_runtimepath(tag: &str, scm: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nxvim-tsq-{}-{}", std::process::id(), tag));
    let qdir = dir.join("queries").join("rust");
    std::fs::create_dir_all(&qdir).unwrap();
    std::fs::write(qdir.join("highlights.scm"), scm).unwrap();
    dir
}

#[tokio::test]
async fn an_on_disk_query_overlay_merges_with_no_query_set() {
    // The buffer-open half of the bridge: a *pure on-disk* `queries/rust/`
    // overlay with a `;extends` modeline — and NO `vim.treesitter.query.set` call
    // anywhere — must still change what the engine paints. The first time a rust
    // buffer is highlighted, the server resolves the merged query through the Lua
    // runtimepath (base on `NXVIM_DATA_DIR` + this overlay) and pushes it. The
    // buffer paints BOTH the base `fn` keyword and the overlay's @variable on
    // `main`, neither of which the engine could reach from a single base file.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let rtp = query_overlay_runtimepath("extends", ";extends\n(identifier) @variable");
    let file = temp_rs("q-overlay", "fn main() {}\n");
    let (rpc, mut incoming) = start_with(Some(file), vec![rtp]).await;

    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        row0_keyword_at_0(hl) && row0_has_group(hl, "variable")
    })
    .await;
    assert!(
        row0_keyword_at_0(&hl),
        "the on-disk `;extends` overlay keeps the base `fn` keyword: {hl:?}"
    );
    assert!(
        row0_has_group(&hl, "variable"),
        "the on-disk overlay adds @variable on `main` with no query.set: {hl:?}"
    );
}

#[tokio::test]
async fn an_on_disk_replacing_query_overlay_replaces_the_base() {
    // A pure on-disk overlay with NO `;extends` modeline REPLACES the base query
    // (it becomes the sole non-extension file resolution picks), just as a plain
    // `query.set` would — again with no `query.set` call. The buffer paints the
    // overlay's @variable on `main` and no longer the base `fn` keyword, proving
    // the buffer-open trigger honors a replacing overlay too, not only `;extends`.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let rtp = query_overlay_runtimepath("replace", "(identifier) @variable");
    let file = temp_rs("q-overlay-replace", "fn main() {}\n");
    let (rpc, mut incoming) = start_with(Some(file), vec![rtp]).await;

    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        !row0_keyword_at_0(hl) && row0_has_group(hl, "variable")
    })
    .await;
    assert!(
        !row0_keyword_at_0(&hl),
        "the replacing on-disk overlay drops the base `fn` keyword: {hl:?}"
    );
    assert!(
        row0_has_group(&hl, "variable"),
        "the replacing on-disk overlay paints `main` as @variable: {hl:?}"
    );
}

// ----- injections bridge, Phase 0 -------------------------------------------
// `injections` joins `highlights` / `indents` as a paint-relevant query name that
// resolves through the vendored Lua and is pushed to the engine, which compiles +
// stores it on the grammar. Phase 0 proves only the *resolution* half: the query
// reaches the engine and compiles (valid → silent, broken → loud). Nothing
// consumes the stored injection query for paint yet — that is Phase 1 — so these
// tests make no paint-of-an-injected-region assertion.

/// Poll a bounded window of frames for a treesitter compile-failure echo, returning
/// `true` as soon as one is seen. Used by the Phase-0 no-echo assertions (expecting
/// `false`) and mirrors the polling shape of the broken-`query.set` test.
async fn saw_ts_compile_failure(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> bool {
    for _ in 0..30 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            if message_of(&params).contains("failed to compile") {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn query_set_injections_compiles_without_echo() {
    // A *valid* `query.set(lang, 'injections', text)` now reaches the engine and
    // compiles silently (no "failed to compile" echo), and — since nothing reads
    // the stored injection query for paint yet — leaves the base highlights intact.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = temp_rs("inj-set", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    wait_for_highlights(&rpc, &mut incoming, row0_keyword_at_0).await;

    exec_lua(
        &rpc,
        "vim.treesitter.query.set('rust', 'injections', \
         '((line_comment) @injection.content (#set! injection.language \"rust\"))')",
    )
    .await;

    assert!(
        !saw_ts_compile_failure(&rpc, &mut incoming).await,
        "a valid injections query.set must not echo a compile failure"
    );
    let hl = wait_for_highlights(&rpc, &mut incoming, row0_keyword_at_0).await;
    assert!(
        row0_keyword_at_0(&hl),
        "the injections push leaves the base paint intact: {hl:?}"
    );
}

#[tokio::test]
async fn a_broken_injections_query_echoes_loud() {
    // No silent stubs: an injection query that fails to compile must echo loud,
    // exactly like a broken `highlights` query — proving `injections` is wired
    // through the same compile-and-surface path, not silently dropped.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = temp_rs("inj-broken", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    wait_for_highlights(&rpc, &mut incoming, row0_keyword_at_0).await;

    exec_lua(
        &rpc,
        "vim.treesitter.query.set('rust', 'injections', '((((')",
    )
    .await;

    assert!(
        saw_ts_compile_failure(&rpc, &mut incoming).await,
        "a broken injections query.set must echo a compile failure"
    );
    // The base paint survives a broken injection push.
    let hl = wait_for_highlights(&rpc, &mut incoming, row0_keyword_at_0).await;
    assert!(
        row0_keyword_at_0(&hl),
        "a broken injections push keeps the base paint: {hl:?}"
    );
}

/// Create a fresh runtimepath dir holding an on-disk `queries/rust/injections.scm`
/// — the buffer-open resolution path for injections, the analogue of
/// [`query_overlay_runtimepath`] for the highlights overlay.
fn injections_overlay_runtimepath(tag: &str, scm: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nxvim-tsi-{}-{}", std::process::id(), tag));
    let qdir = dir.join("queries").join("rust");
    std::fs::create_dir_all(&qdir).unwrap();
    std::fs::write(qdir.join("injections.scm"), scm).unwrap();
    dir
}

#[tokio::test]
async fn an_on_disk_injections_overlay_compiles_without_echo() {
    // The buffer-open half: a pure on-disk `queries/rust/injections.scm` (no
    // `query.set` call) is resolved through the Lua runtimepath the first time a
    // rust buffer is highlighted and pushed to the engine. A valid one compiles
    // silently and does not disturb the base paint.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let rtp = injections_overlay_runtimepath(
        "valid",
        "((line_comment) @injection.content (#set! injection.language \"rust\"))",
    );
    let file = temp_rs("inj-overlay", "fn main() {}\n");
    let (rpc, mut incoming) = start_with(Some(file), vec![rtp]).await;

    wait_for_highlights(&rpc, &mut incoming, row0_keyword_at_0).await;
    assert!(
        !saw_ts_compile_failure(&rpc, &mut incoming).await,
        "a valid on-disk injections overlay must not echo a compile failure"
    );
    let hl = wait_for_highlights(&rpc, &mut incoming, row0_keyword_at_0).await;
    assert!(
        row0_keyword_at_0(&hl),
        "the on-disk injections overlay leaves the base paint intact: {hl:?}"
    );
}

// ----- injections bridge, Phase 1 -------------------------------------------
// Single-level injection highlighting: the engine runs the injection query over
// the root tree, parses each injected region with its child grammar, and paints
// the child's captures over the host's. Self-injection (host == injected == rust)
// exercises the whole pipeline with only the one fixture grammar.

/// Does `row` carry a span of major capture group `major` starting at screen column
/// `col`? Used to catch an injected capture at a column the host paints differently.
fn row_group_at(hl: &[Vec<(u64, u64, String)>], row: usize, col: u64, major: &str) -> bool {
    hl.get(row).is_some_and(|spans| {
        spans
            .iter()
            .any(|(s, _, g)| *s == col && g.split('.').next() == Some(major))
    })
}

/// `row_group_at` for the `keyword` group — catches an injected `fn`/`let` inside a
/// string, a column the host paints `@string`.
fn row_keyword_at(hl: &[Vec<(u64, u64, String)>], row: usize, col: u64) -> bool {
    row_group_at(hl, row, col, "keyword")
}

/// `row_keyword_at` on row 0 — the common single-line injection case.
fn row0_keyword_at(hl: &[Vec<(u64, u64, String)>], col: u64) -> bool {
    row_keyword_at(hl, 0, col)
}

#[tokio::test]
async fn injected_rust_paints_over_a_host_string_self_injection() {
    // A rust string's content is painted flat as `@string` by the host grammar.
    // Injecting rust INTO that content makes the inner `fn` paint as a keyword — a
    // finer capture inside a node the host paints flat, which the single-tree paint
    // could never produce. Host == injected == rust, so no second grammar is needed.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    // The injected snippet's `fn` sits at column 17 (after `const S: &str = "`).
    let file = temp_rs("inj-self", "const S: &str = \"fn x() {}\";\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    // Baseline: the string interior is flat — its content carries no keyword.
    let base = wait_for_highlights(&rpc, &mut incoming, |hl| row0_has_group(hl, "string")).await;
    assert!(
        !row0_keyword_at(&base, 17),
        "baseline: the host paints the string flat, no keyword inside it: {base:?}"
    );

    // Inject rust into the string's content.
    exec_lua(
        &rpc,
        "vim.treesitter.query.set('rust', 'injections', \
         '((string_content) @injection.content (#set! injection.language \"rust\"))')",
    )
    .await;

    // The injected `fn` now paints as a keyword at column 17, over the host @string.
    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| row0_keyword_at(hl, 17)).await;
    assert!(
        row0_keyword_at(&hl, 17),
        "the injected rust paints `fn` as a keyword inside the string: {hl:?}"
    );
}

#[tokio::test]
async fn an_edit_keeps_the_injection_layers_alive() {
    // The child layers are re-derived from the root tree after every edit (Phase 1
    // rebuilds from scratch). An edit must not lose the injection: after opening a
    // new line below, the string on row 0 is still injected and its `fn` still
    // paints as a keyword — proving the post-edit rebuild re-finds the region.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = temp_rs("inj-edit", "const S: &str = \"fn x() {}\";\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    wait_for_highlights(&rpc, &mut incoming, |hl| row0_has_group(hl, "string")).await;
    exec_lua(
        &rpc,
        "vim.treesitter.query.set('rust', 'injections', \
         '((string_content) @injection.content (#set! injection.language \"rust\"))')",
    )
    .await;
    wait_for_highlights(&rpc, &mut incoming, |hl| row0_keyword_at(hl, 17)).await;

    // Open a new line below (an edit that leaves row 0's string untouched), forcing
    // a reparse + injection-layer rebuild.
    feed(&rpc, "ofn z() {}<Esc>");

    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| row0_keyword_at(hl, 17)).await;
    assert!(
        row0_keyword_at(&hl, 17),
        "the injection survives an edit's layer rebuild: {hl:?}"
    );
}

// ----- injections bridge, Phase 2 -------------------------------------------
// Faithful child parsing: the child grammar parses the host buffer through
// `included_ranges` (buffer-absolute coordinates, no substring copy) and reparses
// incrementally across edits. Plus the dynamic `@injection.language` node-text
// form and sibling regions of the same language.

#[tokio::test]
async fn an_edit_inside_an_injected_region_tracks_incrementally() {
    // The injected child reparses across edits. Start with a string holding a bare
    // identifier (no keyword inside it), inject rust, then type `fn ` INTO the
    // string. The incremental child reparse must pick up the new text and paint the
    // freshly-typed `fn` as a keyword — proving the captures track the edit, not a
    // stale one-shot parse.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = temp_rs("inj-incr", "const S: &str = \"x\";\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    wait_for_highlights(&rpc, &mut incoming, |hl| row0_has_group(hl, "string")).await;
    exec_lua(
        &rpc,
        "vim.treesitter.query.set('rust', 'injections', \
         '((string_content) @injection.content (#set! injection.language \"rust\"))')",
    )
    .await;
    // The lone identifier `x` carries no keyword inside the string.
    let injected =
        wait_for_highlights(&rpc, &mut incoming, |hl| row0_has_group(hl, "string")).await;
    assert!(
        !row0_keyword_at(&injected, 17),
        "baseline: a bare identifier in the string is no keyword: {injected:?}"
    );

    // Jump onto the `x` (one past the opening quote) and insert `fn ` before it, so
    // the string content becomes `fn x`. The opening quote stays at column 16, so
    // the injected `fn` lands at column 17.
    feed(&rpc, "f\"lifn <Esc>");

    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| row0_keyword_at(hl, 17)).await;
    assert!(
        row0_keyword_at(&hl, 17),
        "the incremental child reparse paints the newly-typed `fn`: {hl:?}"
    );
}

#[tokio::test]
async fn sibling_injected_regions_of_the_same_language_both_paint() {
    // Two separate injected regions of the same language each get their own child
    // layer; both must paint. Here two rust strings on two rows both highlight their
    // inner `fn` — at column 17 (after `const A: &str = "`) on each row.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = temp_rs(
        "inj-sib",
        "const A: &str = \"fn a() {}\";\nconst B: &str = \"fn b() {}\";\n",
    );
    let (rpc, mut incoming) = start(Some(file)).await;

    wait_for_highlights(&rpc, &mut incoming, |hl| row0_has_group(hl, "string")).await;
    exec_lua(
        &rpc,
        "vim.treesitter.query.set('rust', 'injections', \
         '((string_content) @injection.content (#set! injection.language \"rust\"))')",
    )
    .await;

    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        row_keyword_at(hl, 0, 17) && row_keyword_at(hl, 1, 17)
    })
    .await;
    assert!(
        row_keyword_at(&hl, 0, 17),
        "the first injected region paints its `fn`: {hl:?}"
    );
    assert!(
        row_keyword_at(&hl, 1, 17),
        "the second sibling injected region paints its `fn` too: {hl:?}"
    );
}

#[tokio::test]
async fn the_injected_language_can_come_from_a_capture_node_text() {
    // The injected language can be resolved dynamically from an `@injection.language`
    // capture's node TEXT (e.g. a markdown fence's info string), not only a static
    // `#set!`. Here the const's name identifier `rust` drives the language, and the
    // string body is injected with it — `fn` inside paints as a keyword.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    // `fn` of the injected body sits at column 20 (after `const rust: &str = "`).
    let file = temp_rs("inj-dyn", "const rust: &str = \"fn x() {}\";\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    wait_for_highlights(&rpc, &mut incoming, |hl| row0_has_group(hl, "string")).await;
    exec_lua(
        &rpc,
        "vim.treesitter.query.set('rust', 'injections', \
         '(const_item name: (identifier) @injection.language \
          value: (string_literal (string_content) @injection.content))')",
    )
    .await;

    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| row0_keyword_at(hl, 20)).await;
    assert!(
        row0_keyword_at(&hl, 20),
        "the language from the `@injection.language` node text injects rust: {hl:?}"
    );
}

// ----- injections bridge, Phase 3 -------------------------------------------
// Cross-language, combined, and nested injections, driven by the markdown host
// grammar (its on-disk `injections.scm` maps a fenced block to the fence's
// language) plus rust.

#[tokio::test]
async fn markdown_injects_rust_into_a_fenced_code_block() {
    // The canonical cross-language case: a markdown doc with a ```rust fence. The
    // markdown grammar's injection query resolves the fence's language from its info
    // string and injects rust into the fenced content, so `fn` inside the block
    // paints as a keyword — a second grammar painting over the markdown host.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = write_temp("inj-md", "md", "```rust\nfn z() {}\n```\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    // Row 1 is `fn z() {}` inside the fence; injected as rust → `fn` at column 0.
    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| row_keyword_at(hl, 1, 0)).await;
    assert!(
        row_keyword_at(&hl, 1, 0),
        "markdown injects rust into the fenced code block: {hl:?}"
    );
}

#[tokio::test]
async fn a_nested_injection_paints_the_innermost_grammar() {
    // Two levels deep: markdown injects rust into the fenced block (level 1), and
    // rust then injects rust into the string literal inside it (level 2). The
    // innermost `fn` — inside the string, inside the fence — must paint as a keyword,
    // which only the recursive layer build can reach.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = write_temp(
        "inj-nest",
        "md",
        "```rust\nconst S: &str = \"fn z() {}\";\n```\n",
    );
    let (rpc, mut incoming) = start(Some(file)).await;

    // Level 1 is live at open (markdown → rust): row 1's `const` is a keyword, but
    // the string body is still flat (no level-2 injection query for rust yet).
    let base = wait_for_highlights(&rpc, &mut incoming, |hl| row_keyword_at(hl, 1, 0)).await;
    assert!(
        !row_keyword_at(&base, 1, 17),
        "baseline: without rust's injection query the string body is flat: {base:?}"
    );

    // Give rust an injection query → level 2 (rust-in-rust) builds under the fence.
    exec_lua(
        &rpc,
        "vim.treesitter.query.set('rust', 'injections', \
         '((string_content) @injection.content (#set! injection.language \"rust\"))')",
    )
    .await;

    // The innermost `fn` (string body, column 17 on row 1) now paints as a keyword.
    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| row_keyword_at(hl, 1, 17)).await;
    assert!(
        row_keyword_at(&hl, 1, 17),
        "the two-level nested injection paints the innermost grammar: {hl:?}"
    );
}

#[tokio::test]
async fn injection_self_injects_the_host_language() {
    // `(#set! injection.self)` injects the *host's own* language — here rust into a
    // rust string, with no language named. The inner `fn` paints as a keyword.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = temp_rs("inj-self-dir", "const S: &str = \"fn x() {}\";\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    wait_for_highlights(&rpc, &mut incoming, |hl| row0_has_group(hl, "string")).await;
    exec_lua(
        &rpc,
        "vim.treesitter.query.set('rust', 'injections', \
         '((string_content) @injection.content (#set! injection.self))')",
    )
    .await;

    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| row0_keyword_at(hl, 17)).await;
    assert!(
        row0_keyword_at(&hl, 17),
        "`injection.self` injects the host (rust) into its own string: {hl:?}"
    );
}

#[tokio::test]
async fn a_combined_injection_parses_split_regions_as_one_tree() {
    // `(#set! injection.combined)` parses every match of the pattern as ONE child
    // tree spanning all their ranges. Two rust strings hold the two halves of a
    // block comment — `/* a` and `b */`. Combined, they parse as the single comment
    // `/* ab */`, so the SECOND region's `b` paints as a comment. Non-combined, `b`
    // would just be a stray identifier — so this is a true combined-only signal.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = temp_rs(
        "inj-comb",
        "const A: &str = \"/* a\";\nconst B: &str = \"b */\";\n",
    );
    let (rpc, mut incoming) = start(Some(file)).await;

    wait_for_highlights(&rpc, &mut incoming, |hl| row0_has_group(hl, "string")).await;
    exec_lua(
        &rpc,
        "vim.treesitter.query.set('rust', 'injections', \
         '((string_content) @injection.content (#set! injection.language \"rust\") \
           (#set! injection.combined))')",
    )
    .await;

    // The `b` opening the second region (row 1, column 17) is inside the combined
    // comment → painted @comment, which a per-region parse could never produce.
    let hl =
        wait_for_highlights(&rpc, &mut incoming, |hl| row_group_at(hl, 1, 17, "comment")).await;
    assert!(
        row_group_at(&hl, 1, 17, "comment"),
        "the combined injection parses the split regions as one comment: {hl:?}"
    );
}

// ----- injections bridge, Phase 4 (the platform half / drift oracle) --------

#[tokio::test]
async fn the_engine_paint_agrees_with_the_platform_injection_resolution() {
    // Drift oracle: the engine's painted injection (the directive logic ported to
    // Rust) and the vendored `LanguageTree`'s `_get_injections` (pure Lua, over
    // nxvim's snapshot primitives) must agree on what is injected where. For a
    // markdown ```rust fence the engine paints rust into the fence AND the platform
    // resolves the fenced region's language to rust — a divergence between the two
    // independent ports of the injection-directive vocabulary would surface here.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = write_temp("inj-oracle", "md", "```rust\nfn z() {}\n```\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    // Engine side: rust is painted inside the fence — row 1's `fn` is a keyword.
    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| row_keyword_at(hl, 1, 0)).await;
    assert!(
        row_keyword_at(&hl, 1, 0),
        "engine paints rust into the fenced block: {hl:?}"
    );

    // Platform side: the vendored LanguageTree resolves the same region to rust.
    let lang = exec_lua(
        &rpc,
        r#"
        local p = vim.treesitter.get_parser(0, 'markdown')
        p:parse(true)
        local child = p:language_for_range({ 1, 0, 1, 0 })
        return child and child:lang() or 'nil'
        "#,
    )
    .await;
    assert_eq!(
        lang.as_str(),
        Some("rust"),
        "the platform resolves the fenced region to rust, agreeing with the engine paint"
    );
}

#[tokio::test]
async fn the_treesitter_injections_example_config_injects_on_startup() {
    // The shipped `examples/treesitter-injections/` config calls
    // `vim.treesitter.query.set('rust','injections', <string_content → rust>)` at
    // the top of init.lua. Sourced at startup against its own sample, the Rust
    // inside the `const SNIPPET` string literal must paint — its `fn` (row 4, column
    // 23) is a keyword — proving the injection bridge resolves + pushes + paints
    // end-to-end through the example, so the shipped config can't rot.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let example = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/treesitter-injections")
        .canonicalize()
        .expect("example dir exists");
    let sample = example.join("sample.rs").display().to_string();
    let (rpc, mut incoming) = start_with_config(Some(sample), example).await;

    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| row_keyword_at(hl, 4, 23)).await;
    assert!(
        row_keyword_at(&hl, 4, 23),
        "the example injects rust into the string, painting its `fn` as a keyword: {hl:?}"
    );
}
