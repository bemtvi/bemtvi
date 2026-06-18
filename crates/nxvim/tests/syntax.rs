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

/// The picker preview pane's `highlights` (Phase 3b) — per windowed line, the
/// `(start_char, end_char, group)` spans — from a redraw map, or `None` when no
/// menu/preview is open. Mirrors [`highlights_of`] but reaches into `menu.preview`.
fn preview_highlights_of(params: &[Value]) -> Option<Vec<Vec<(u64, u64, String)>>> {
    let field = |m: &Value, key: &str| -> Option<Value> {
        match m {
            Value::Map(m) => m
                .iter()
                .find(|(k, _)| k.as_str() == Some(key))
                .map(|(_, v)| v.clone()),
            _ => None,
        }
    };
    let menu = field(params.first()?, "menu")?;
    let rows = field(&field(&menu, "preview")?, "highlights")?;
    let rows = rows.as_array()?;
    Some(
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
            .collect(),
    )
}

#[tokio::test]
async fn the_file_preview_pane_is_syntax_highlighted() {
    // Phase 3b: a `preview = "file"` picker renders the selected file's content with
    // native tree-sitter colours, not plain text. Open a picker over a rust file and
    // assert the preview's `highlights` carry the `fn` keyword span.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = temp_rs("preview_hl", "fn main() {}\n");
    let (rpc, mut incoming) = start(None).await;

    // Register a preview source over the rust file and open it (no `rg` spawn).
    exec_lua(
        &rpc,
        &format!(
            "nx.picker.source{{ name='prev', preview='file', \
               items=function(ctx) ctx.push{{ text='f', path='{file}' }} end }}; \
             nx.picker.open('prev')"
        ),
    )
    .await;

    // Poll until the menu's preview carries highlights for the first (`fn main`) row.
    let hl = {
        let mut found = None;
        for _ in 0..100 {
            barrier(&rpc).await;
            tokio::task::yield_now().await;
            if let Some(params) = drain_latest_redraw(&mut incoming) {
                if let Some(hl) = preview_highlights_of(&params) {
                    if hl.first().is_some_and(|row| !row.is_empty()) {
                        found = Some(hl);
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        found.expect("the preview pane never carried tree-sitter highlights")
    };

    // `fn` (char cols 0..2) is a keyword — the preview is genuinely colourised.
    let fn_span = hl[0]
        .iter()
        .find(|(s, _, _)| *s == 0)
        .expect("a preview span at column 0 (the `fn` keyword)");
    assert_eq!(fn_span.1, 2, "`fn` spans two chars");
    assert_eq!(
        fn_span.2.split('.').next().unwrap(),
        "keyword",
        "`fn` is a keyword, got group {:?}",
        fn_span.2
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

// One Dark hex values the bundled `:colorscheme nxvim` sets, mirrored here as
// the RGB the painted cells must carry once it loads (the same palette the GUI
// and edit-host ship — proving the terminal reaches color parity).
const ONEDARK_BG: Color = Color::Rgb(0x28, 0x2c, 0x34);
const ONEDARK_KEYWORD: Color = Color::Rgb(0xc6, 0x78, 0xdd); // purple
const ONEDARK_STRING: Color = Color::Rgb(0x98, 0xc3, 0x79); // green

/// The bundled scheme, end to end through the real TUI: `:colorscheme nxvim`
/// needs *no* runtimepath fixture (it's embedded in the binary), yet the real
/// client paints the resolved One Dark styles — keyword foreground, string
/// foreground, and the editor background.
#[tokio::test]
async fn the_builtin_nxvim_colorscheme_paints_one_dark_in_the_terminal() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    // `fn` is a keyword; `"hi"` is a string — both on row 0.
    let file = temp_rs("builtin-theme", "fn main() { let s = \"hi\"; }\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    // Wait for treesitter spans (painted in the fallback theme), then load the
    // built-in scheme — note no runtimepath was seeded, unlike the catppuccin
    // fixture test above.
    wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| !row.is_empty())
    })
    .await;
    feed(&rpc, ":colorscheme nxvim<CR>");

    const GUTTER: u16 = 4; // hybrid number column, width 4 → text at col 4
    let (params, buf) = paint_until(&rpc, &mut incoming, |buf| {
        buf.cell((GUTTER, 0)).unwrap().style().fg == Some(ONEDARK_KEYWORD)
    })
    .await;

    // The `fn` keyword paints purple, sitting on the Normal background.
    let kw = buf.cell((GUTTER, 0)).unwrap().style();
    assert_eq!(
        kw.fg,
        Some(ONEDARK_KEYWORD),
        "the `fn` keyword paints purple"
    );
    assert_eq!(
        kw.bg,
        Some(ONEDARK_BG),
        "themed text sits on the One Dark background"
    );

    // The string literal paints green.
    let hl = highlights_of(&params);
    let str_start = hl[0]
        .iter()
        .find(|(_, _, group)| group.split('.').next() == Some("string"))
        .map(|(start, _, _)| *start as u16)
        .expect("a string span on row 0");
    assert_eq!(
        buf.cell((GUTTER + str_start, 0)).unwrap().style().fg,
        Some(ONEDARK_STRING),
        "the string literal paints green"
    );
}

/// The shipped `examples/colorscheme/` config, end to end: its `init.lua` calls
/// `vim.cmd.colorscheme('nxvim')` at startup, so the very first painted frame of
/// its sample buffer is already One Dark — guarding the example against bitrot.
#[tokio::test]
async fn the_colorscheme_example_config_themes_the_buffer() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let example = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/colorscheme"
    ))
    .canonicalize()
    .expect("examples/colorscheme exists");
    let file = example.join("sample.rs").to_string_lossy().into_owned();
    let (rpc, mut incoming) = start_full(Some(file), Vec::new(), Some(example)).await;

    // No `:colorscheme` is fed here — the example's init.lua loaded it. Row 0 of
    // the sample is a `//` comment, so wait for it to paint in One Dark grey on
    // the One Dark background, proving both the chrome and syntax themed at
    // startup straight from the shipped config.
    const ONEDARK_COMMENT: Color = Color::Rgb(0x5c, 0x63, 0x70);
    const GUTTER: u16 = 4;
    let (_, buf) = paint_until(&rpc, &mut incoming, |buf| {
        buf.cell((GUTTER, 0)).unwrap().style().fg == Some(ONEDARK_COMMENT)
    })
    .await;
    assert_eq!(
        buf.cell((GUTTER, 0)).unwrap().style().bg,
        Some(ONEDARK_BG),
        "the example config paints its buffer on the One Dark background"
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
    rpc.request("nx_command", vec![Value::from(format!("e {b}"))])
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

// ----- two-noun model: `filetype` (language) vs `ts_highlight` (whether) -----

#[tokio::test]
async fn nx_bo_filetype_and_ts_highlight_drive_highlighting() {
    // The native front doors: `nx.bo.filetype` (language noun) and
    // `nx.bo.ts_highlight` (whether noun) reach the core and read back through the
    // server-pushed mirror. A `.txt` the extension table misses highlights once
    // `nx.bo.filetype = "rust"`, darkens on `nx.bo.ts_highlight = false`, and the
    // filetype reads back unchanged across the disable.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = write_temp("nx-bo-ts", "txt", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    exec_lua(&rpc, "nx.bo.filetype = 'rust'").await;
    wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| !row.is_empty())
    })
    .await;
    assert_eq!(
        exec_lua(&rpc, "return nx.bo.filetype").await.as_str(),
        Some("rust"),
        "nx.bo.filetype reads back through the mirror"
    );

    // Disable highlighting; the buffer darkens but the filetype is retained.
    exec_lua(&rpc, "nx.bo.ts_highlight = false").await;
    wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.iter().all(|row| row.is_empty())
    })
    .await;
    assert_eq!(
        exec_lua(&rpc, "return nx.bo.filetype").await.as_str(),
        Some("rust"),
        "filetype survives ts_highlight = false (orthogonal nouns)"
    );
    assert_eq!(
        exec_lua(&rpc, "return nx.bo.ts_highlight").await.as_bool(),
        Some(false),
        "nx.bo.ts_highlight reads back false"
    );
}

#[tokio::test]
async fn ts_highlight_toggles_independently_of_an_explicit_filetype() {
    // The two nouns are orthogonal. A `.txt` buffer (extension table misses) is
    // given an explicit `filetype=rust`, so it highlights. `:set nots_highlight`
    // darkens it *without* clearing the filetype — proven by re-enabling with a
    // bare `:set ts_highlight` (which sets no filetype): it highlights as rust
    // again, so the explicit `rust` filetype survived the disable. The extension
    // alone could never supply `rust` here, so re-highlighting proves retention.
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = write_temp("ts-two-noun", "txt", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    // Force the language noun; the buffer the extension misses now highlights.
    exec_lua(&rpc, "vim.cmd('set filetype=rust')").await;
    wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| !row.is_empty())
    })
    .await;

    // Disable the *whether* noun: highlighting goes dark, filetype untouched.
    exec_lua(&rpc, "vim.cmd('set nots_highlight')").await;
    wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.iter().all(|row| row.is_empty())
    })
    .await;

    // Re-enable highlighting only — set no filetype. It paints rust again, so the
    // explicit filetype persisted across the disable (orthogonal nouns).
    exec_lua(&rpc, "vim.cmd('set ts_highlight')").await;
    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| !row.is_empty())
    })
    .await;
    let fn_span = hl[0]
        .iter()
        .find(|(s, _, _)| *s == 0)
        .expect("the `fn` keyword repaints after re-enabling ts_highlight");
    assert_eq!(
        fn_span.2.split('.').next().unwrap(),
        "keyword",
        "the retained filetype=rust repaints `fn` as a keyword, got {:?}",
        fn_span.2
    );
}

// ----- vim.treesitter.start / stop bridge (ADR 0001, #1) --------------------

// ----- `:set filetype` / `:setfiletype` (no-Lua treesitter override) --------

#[tokio::test]
async fn setfiletype_command_highlights_then_reset_darkens() {
    // `:setfiletype rust` is the idiomatic alias for `:set filetype=rust`; it must
    // highlight a `.txt` the same way. Then `:set filetype&` resets to the
    // extension-derived default — which for `.txt` is *no* language — so the buffer
    // goes dark again (distinct from `:set ft=`'s explicit stop).
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let file = write_temp("setf-cmd", "txt", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    feed(&rpc, ":setfiletype rust<CR>");
    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.first().is_some_and(|row| !row.is_empty())
    })
    .await;
    assert!(
        hl[0]
            .iter()
            .any(|(s, _, g)| *s == 0 && g.split('.').next() == Some("keyword")),
        ":setfiletype rust must highlight the `fn` keyword"
    );

    feed(&rpc, ":set filetype&<CR>");
    let hl = wait_for_highlights(&rpc, &mut incoming, |hl| {
        hl.iter().all(|row| row.is_empty())
    })
    .await;
    assert!(
        hl.iter().all(|row| row.is_empty()),
        ":set filetype& must restore the .txt extension default (no highlighting)"
    );
}

// ----- query-resolution bridge (ADR 0001, #4) -------------------------------

// ----- injections bridge, Phase 0 -------------------------------------------
// `injections` joins `highlights` / `indents` as a paint-relevant query name that
// resolves through the vendored Lua and is pushed to the engine, which compiles +
// stores it on the grammar. Phase 0 proves only the *resolution* half: the query
// reaches the engine and compiles (valid → silent, broken → loud). Nothing
// consumes the stored injection query for paint yet — that is Phase 1 — so these
// tests make no paint-of-an-injected-region assertion.

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

// ----- injections bridge, Phase 2 -------------------------------------------
// Faithful child parsing: the child grammar parses the host buffer through
// `included_ranges` (buffer-absolute coordinates, no substring copy) and reparses
// incrementally across edits. Plus the dynamic `@injection.language` node-text
// form and sibling regions of the same language.

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
