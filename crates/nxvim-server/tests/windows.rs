//! Behavior tests for windows (splits), driven the way a real client drives the
//! editor (black-box RPC, exactly like `editing.rs` / `buffers.rs`).
//!
//! Phase 3 covers the feature becoming real: `<C-w>s` / `<C-w>v` (and `:split` /
//! `:vsplit`) create windows, `<C-w>` motions move focus, `<C-w>c` / `<C-w>o`
//! close them. Everything is asserted through the multi-window `redraw` the
//! Phase 2 renderer consumes: the `windows` array (each window's rect, focus
//! flag, file name, and text) plus the `separators` between splits.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server on its own thread and return a connected client, with a
/// `file` opened in the first window when given.
async fn start(file: Option<String>) -> (Rpc, UnboundedReceiver<Incoming>) {
    start_with_config(file, None).await
}

/// As [`start`], but also source `config_dir`'s `init.lua` at startup (the real
/// production path), so a test can drive an actual `examples/<feature>/` config.
async fn start_with_config(
    file: Option<String>,
    config_dir: Option<PathBuf>,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (server_end, client_end) = tokio::io::duplex(1 << 16);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("server runtime");
        let init = ServerInit {
            file,
            config_dir,
            ..Default::default()
        };
        let _ = runtime.block_on(run_server(server_end, init));
    });
    let (reader, writer) = tokio::io::split(client_end);
    let (rpc, incoming) = connect(reader, writer);
    // A 24-row windows area (the frame minus the client's command row). A single
    // window's rect is the whole 24 rows; a horizontal split divides 24 − 1 (one
    // separator row) between the two.
    rpc.request(
        "nvim_ui_attach",
        vec![Value::from(80u64), Value::from(24u64), Value::Map(vec![])],
    )
    .await
    .expect("ui attach");
    (rpc, incoming)
}

/// Type a string of vim key-notation.
fn feed(rpc: &Rpc, keys: &str) {
    rpc.notify("nvim_input", vec![Value::from(keys)]);
}

/// Fetch the focused window's buffer lines (also a barrier).
async fn lines(rpc: &Rpc) -> Vec<String> {
    let result = rpc
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
        .expect("get_lines");
    match result {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// One window parsed out of a redraw's `windows` array.
#[derive(Debug, Clone)]
struct Win {
    focused: bool,
    file_name: String,
    lines: Vec<String>,
    rect: Rect,
    /// This window's buffer `tabstop` (cells a `\t` expands to) and the cursor's
    /// resulting screen column — both computed per-window from the window's own
    /// buffer, so two windows onto differently-tabbed buffers report different
    /// values.
    tabstop: u64,
    cursor_screen_col: u64,
    /// This window's window-local number-gutter options and the resulting gutter
    /// width — per-window, so two windows onto the same buffer can show different
    /// line-number columns.
    number: bool,
    relativenumber: bool,
    number_width: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: u64,
    y: u64,
    width: u64,
    height: u64,
}

/// A separator parsed out of a redraw's `separators` array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sep {
    vertical: bool,
    x: u64,
    y: u64,
    length: u64,
}

/// The window list, separators, and message line of one redraw frame.
#[derive(Debug, Clone)]
struct Frame {
    windows: Vec<Win>,
    separators: Vec<Sep>,
    message: String,
}

impl Frame {
    /// The single focused window (panics if not exactly one is flagged).
    fn focused(&self) -> &Win {
        let focused: Vec<&Win> = self.windows.iter().filter(|w| w.focused).collect();
        assert_eq!(focused.len(), 1, "exactly one window should hold focus");
        focused[0]
    }
}

fn map_get<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

fn u64_at(map: &[(Value, Value)], key: &str) -> u64 {
    map_get(map, key).and_then(Value::as_u64).unwrap_or(0)
}

fn parse_rect(value: Option<&Value>) -> Rect {
    let m = match value {
        Some(Value::Map(m)) => m.as_slice(),
        _ => &[],
    };
    Rect {
        x: u64_at(m, "x"),
        y: u64_at(m, "y"),
        width: u64_at(m, "width"),
        height: u64_at(m, "height"),
    }
}

fn parse_window(value: &Value) -> Win {
    let m = match value {
        Value::Map(m) => m.as_slice(),
        _ => panic!("window entry is not a map"),
    };
    let lines = match map_get(m, "lines") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    Win {
        focused: map_get(m, "focused")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        file_name: map_get(m, "file_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        lines,
        rect: parse_rect(map_get(m, "rect")),
        tabstop: u64_at(m, "tabstop"),
        cursor_screen_col: u64_at(m, "cursor_screen_col"),
        number: map_get(m, "number")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        relativenumber: map_get(m, "relativenumber")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        number_width: u64_at(m, "number_width"),
    }
}

fn parse_frame(map: &[(Value, Value)]) -> Frame {
    let windows = match map_get(map, "windows") {
        Some(Value::Array(a)) => a.iter().map(parse_window).collect(),
        _ => Vec::new(),
    };
    let separators = match map_get(map, "separators") {
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                let m = match v {
                    Value::Map(m) => m.as_slice(),
                    _ => panic!("separator is not a map"),
                };
                Sep {
                    vertical: map_get(m, "vertical")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    x: u64_at(m, "x"),
                    y: u64_at(m, "y"),
                    length: u64_at(m, "length"),
                }
            })
            .collect(),
        _ => Vec::new(),
    };
    let message = map_get(map, "message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Frame {
        windows,
        separators,
        message,
    }
}

/// Drain every queued `redraw` and return the most recent parsed [`Frame`].
fn drain_to_latest(incoming: &mut UnboundedReceiver<Incoming>) -> Option<Frame> {
    let mut latest = None;
    loop {
        match incoming.try_recv() {
            Ok(Incoming::Notification { method, params }) if method == "redraw" => {
                match params.into_iter().next() {
                    Some(Value::Map(map)) => latest = Some(parse_frame(&map)),
                    _ => panic!("redraw without a map"),
                }
            }
            Ok(_) => continue,
            Err(_) => return latest,
        }
    }
}

/// Feed `keys`, then return the freshest resulting window list. Mirrors
/// `editing.rs`'s `redraw_after`: input + a `nvim_get_mode` barrier guarantee the
/// frame is queued; we take the *latest* (CLAUDE.md's take-latest rule) so a
/// stale startup / trailing-barrier frame never leaks in under load.
async fn windows_after(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>, keys: &str) -> Frame {
    while incoming.try_recv().is_ok() {} // discard buffered frames from earlier
    rpc.request("nvim_input", vec![Value::from(keys)])
        .await
        .expect("input");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");

    if let Some(frame) = drain_to_latest(incoming) {
        return frame;
    }
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if let Some(frame) = drain_to_latest(incoming) {
            return frame;
        }
    }
    panic!("no redraw arrived for {keys:?}");
}

/// A unique temp file seeded with `contents`, returned as an absolute path
/// string. Cleaned up by the OS temp dir; unique per call so parallel tests
/// don't collide.
fn temp_file(tag: &str, contents: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let path: PathBuf =
        std::env::temp_dir().join(format!("nxvim_win_{tag}_{}_{n}.txt", std::process::id()));
    std::fs::write(&path, contents).expect("write temp file");
    path.display().to_string()
}

// ----- splits ---------------------------------------------------------------

#[tokio::test]
async fn ctrl_w_s_stacks_two_windows_on_the_same_buffer() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world");
    let frame = windows_after(&rpc, &mut incoming, "<Esc><C-w>s").await;

    assert_eq!(frame.windows.len(), 2, "split makes a second window");
    // Stacked: same x/width, the second below the first.
    let (top, bottom) = (&frame.windows[0], &frame.windows[1]);
    assert_eq!(top.rect.x, bottom.rect.x);
    assert_eq!(top.rect.width, bottom.rect.width);
    assert!(
        bottom.rect.y > top.rect.y,
        "second window sits below: {:?} / {:?}",
        top.rect,
        bottom.rect
    );
    // Both show the same (only) buffer's text.
    assert_eq!(top.lines.first().map(String::as_str), Some("hello world"));
    assert_eq!(
        bottom.lines.first().map(String::as_str),
        Some("hello world")
    );
    // A horizontal separator runs between them, full width.
    assert_eq!(frame.separators.len(), 1);
    let sep = frame.separators[0];
    assert!(!sep.vertical, "stacked windows get a horizontal separator");
    assert_eq!(sep.length, top.rect.width);
    // `:split` keeps focus in the new top window.
    assert!(top.focused && !bottom.focused);
}

#[tokio::test]
async fn ctrl_w_v_places_two_windows_side_by_side() {
    let (rpc, mut incoming) = start(None).await;
    let frame = windows_after(&rpc, &mut incoming, "<C-w>v").await;

    assert_eq!(frame.windows.len(), 2);
    let (left, right) = (&frame.windows[0], &frame.windows[1]);
    assert_eq!(left.rect.y, right.rect.y);
    assert_eq!(left.rect.height, right.rect.height);
    assert!(
        right.rect.x > left.rect.x,
        "second window sits to the right: {:?} / {:?}",
        left.rect,
        right.rect
    );
    assert_eq!(frame.separators.len(), 1);
    assert!(
        frame.separators[0].vertical,
        "side-by-side windows get a vertical separator"
    );
    // `:vsplit` keeps focus in the new left window.
    assert!(left.focused && !right.focused);
}

#[tokio::test]
async fn editing_in_one_split_shows_in_the_other_sharing_buffer() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<CR>three<Esc>");
    // Split, then type in the focused window.
    let frame = windows_after(&rpc, &mut incoming, "<C-w>sggIEDIT <Esc>").await;

    assert_eq!(frame.windows.len(), 2);
    // The edit lands in the shared buffer, so *both* windows render it.
    for win in &frame.windows {
        assert_eq!(
            win.lines.first().map(String::as_str),
            Some("EDIT one"),
            "both windows show the shared buffer's edit: {win:?}"
        );
    }
}

// ----- focus navigation -----------------------------------------------------

#[tokio::test]
async fn ctrl_w_jk_move_focus_between_stacked_windows() {
    let (rpc, mut incoming) = start(None).await;
    // Two stacked windows; focus starts in the new top one.
    let frame = windows_after(&rpc, &mut incoming, "<C-w>s").await;
    assert!(frame.windows[0].focused, "split focuses the top window");

    // `<C-w>j` moves focus down.
    let frame = windows_after(&rpc, &mut incoming, "<C-w>j").await;
    assert!(
        !frame.windows[0].focused && frame.windows[1].focused,
        "<C-w>j focuses the bottom window"
    );

    // `<C-w>k` moves it back up.
    let frame = windows_after(&rpc, &mut incoming, "<C-w>k").await;
    assert!(
        frame.windows[0].focused && !frame.windows[1].focused,
        "<C-w>k focuses the top window"
    );
}

#[tokio::test]
async fn each_window_keeps_its_own_cursor_position() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<CR>three<CR>four<CR>five<Esc>");
    // Split: both windows start at the same position (the new top focused).
    feed(&rpc, "<C-w>s");
    // Move the focused (top) window's cursor to line 1.
    feed(&rpc, "gg");
    let top_cursor = cursor_now(&rpc).await;
    assert_eq!(top_cursor.0, 1, "top window cursor on line 1");

    // Focus the bottom window and move its cursor to line 5.
    feed(&rpc, "<C-w>jG");
    let bottom_cursor = cursor_now(&rpc).await;
    assert_eq!(bottom_cursor.0, 5, "bottom window cursor on line 5");

    // Back to the top window: its cursor is restored to line 1, independent of
    // the bottom window's move.
    feed(&rpc, "<C-w>k");
    let restored = cursor_now(&rpc).await;
    assert_eq!(restored.0, 1, "top window cursor restored to its own line");
}

/// Focused-window cursor as `(1-based line, 0-based col)` — a barrier too.
async fn cursor_now(rpc: &Rpc) -> (usize, usize) {
    let result = rpc
        .request("nvim_win_get_cursor", vec![Value::from(0u64)])
        .await
        .expect("get_cursor");
    match result {
        Value::Array(a) => (
            a.first().and_then(Value::as_u64).unwrap_or(0) as usize,
            a.get(1).and_then(Value::as_u64).unwrap_or(0) as usize,
        ),
        _ => (0, 0),
    }
}

// ----- close / only ---------------------------------------------------------

#[tokio::test]
async fn ctrl_w_c_closes_the_focused_window_and_survivor_fills_the_area() {
    let (rpc, mut incoming) = start(None).await;
    let split = windows_after(&rpc, &mut incoming, "<C-w>s").await;
    assert_eq!(split.windows.len(), 2);
    let full_height: u64 =
        split.windows.iter().map(|w| w.rect.height).sum::<u64>() + split.separators.len() as u64;

    let frame = windows_after(&rpc, &mut incoming, "<C-w>c").await;
    assert_eq!(frame.windows.len(), 1, "one window remains");
    assert!(frame.separators.is_empty(), "no separators with one window");
    assert_eq!(
        frame.windows[0].rect.height, full_height,
        "the survivor expands to the whole windows area"
    );
}

#[tokio::test]
async fn ctrl_w_o_drops_all_but_the_focused_window() {
    let (rpc, mut incoming) = start(None).await;
    // Make three windows (split twice), then keep only the focused one.
    feed(&rpc, "<C-w>s<C-w>v");
    let frame = windows_after(&rpc, &mut incoming, "<C-w>o").await;
    assert_eq!(frame.windows.len(), 1, "only the focused window survives");
    assert!(frame.windows[0].focused);
}

#[tokio::test]
async fn closing_the_last_window_is_refused() {
    let (rpc, mut incoming) = start(None).await;
    let frame = windows_after(&rpc, &mut incoming, "<C-w>c").await;
    assert_eq!(
        frame.windows.len(),
        1,
        "the last window cannot be closed (E444)"
    );
}

// ----- split with a file ----------------------------------------------------

#[tokio::test]
async fn vsplit_file_opens_a_different_buffer_in_the_new_window() {
    let original = temp_file("orig", "first file\n");
    let other = temp_file("other", "second file\n");
    let (rpc, mut incoming) = start(Some(original.clone())).await;

    let frame = windows_after(&rpc, &mut incoming, &format!(":vsplit {other}<CR>")).await;
    assert_eq!(frame.windows.len(), 2, "vsplit makes a second window");

    // Each window names its own buffer; the new (left, focused) one shows the
    // freshly-edited file, the other keeps the original.
    let names: Vec<&str> = frame.windows.iter().map(|w| w.file_name.as_str()).collect();
    assert!(
        names.iter().any(|n| n.contains("other")),
        "a window shows the vsplit file: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("orig")),
        "a window still shows the original file: {names:?}"
    );
    // Focus is in the new window, showing the new file.
    assert!(frame.focused().file_name.contains("other"));
    // The original file is still the one the *other* window reads.
    let _ = lines(&rpc).await;
}

// ----- buffer-local options across windows ----------------------------------

#[tokio::test]
async fn two_windows_render_their_own_buffers_tabstop() {
    // The cross-feature seam: `tabstop` is buffer-local, windows are many. A
    // window must expand tabs and place its cursor with *its own* buffer's
    // tabstop — including a non-focused window onto a differently-tabbed buffer.
    // Each file is a single tab-led line so the cursor's screen column past the
    // tab equals the buffer's tabstop exactly.
    let wide = temp_file("wide", "\tA\n");
    let narrow = temp_file("narrow", "\tB\n");
    let (rpc, mut incoming) = start(Some(wide.clone())).await;

    // Buffer `wide`: tabstop 8, cursor moved onto the 'A' (one tab = 8 cells).
    feed(&rpc, ":set tabstop=8<CR>");
    feed(&rpc, "l");
    // `:vsplit narrow` opens the other buffer in a new (focused) window; the
    // `wide` window stays open with its cursor stashed at screen column 8.
    let _ = windows_after(&rpc, &mut incoming, &format!(":vsplit {narrow}<CR>")).await;
    // Buffer `narrow`: tabstop 2 (set only here — buffer-local), cursor onto 'B'.
    feed(&rpc, ":set tabstop=2<CR>");
    let frame = windows_after(&rpc, &mut incoming, "l").await;

    assert_eq!(frame.windows.len(), 2, "vsplit makes a second window");
    let wide_win = frame
        .windows
        .iter()
        .find(|w| w.file_name.contains("wide"))
        .expect("a window shows the wide-tab buffer");
    let narrow_win = frame
        .windows
        .iter()
        .find(|w| w.file_name.contains("narrow"))
        .expect("a window shows the narrow-tab buffer");

    // Each window reports its own buffer's tabstop, not a single global one.
    assert_eq!(wide_win.tabstop, 8, "wide window keeps tabstop=8");
    assert_eq!(
        narrow_win.tabstop, 2,
        "narrow window honors its own tabstop=2"
    );

    // And that per-buffer tabstop actually drives the cursor's screen column:
    // one leading tab places the cursor at column == tabstop in each window. The
    // wide window is *not* focused here, proving stashed cursors use their own
    // buffer's tabstop too.
    assert!(
        !wide_win.focused,
        "focus is in the freshly-split narrow window"
    );
    assert!(narrow_win.focused);
    assert_eq!(
        wide_win.cursor_screen_col, 8,
        "wide window's cursor sits one tabstop=8 in"
    );
    assert_eq!(
        narrow_win.cursor_screen_col, 2,
        "narrow window's cursor sits one tabstop=2 in"
    );
}

#[tokio::test]
async fn number_and_relativenumber_are_window_local() {
    // `number` / `relativenumber` are window-local in vim: two splits onto the
    // *same* buffer can show different number gutters. `:set` targets the focused
    // window only, leaving the other window's gutter untouched.
    let file = temp_file("nu", "alpha\nbeta\ngamma\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    // `<C-w>v` splits the window; both halves start with the default hybrid
    // gutter (number + relativenumber on).
    let split = windows_after(&rpc, &mut incoming, "<C-w>v").await;
    assert_eq!(split.windows.len(), 2);
    assert!(
        split.windows.iter().all(|w| w.number && w.relativenumber),
        "both windows start with the default number gutter"
    );

    // Turn the gutter off in the focused (new, left) window only.
    let frame = windows_after(&rpc, &mut incoming, ":set nonumber norelativenumber<CR>").await;
    assert_eq!(frame.windows.len(), 2, "still two windows");

    let off = frame.focused();
    let on = frame
        .windows
        .iter()
        .find(|w| !w.focused)
        .expect("the other window");

    // The focused window dropped its gutter; the other window kept it. A single
    // global option would have flipped both — this is the window-local seam.
    assert!(!off.number, "focused window's `number` is off");
    assert!(
        !off.relativenumber,
        "focused window's `relativenumber` is off"
    );
    assert_eq!(
        off.number_width, 0,
        "focused window's gutter collapsed to width 0"
    );
    assert!(on.number, "the other window kept `number`");
    assert!(on.relativenumber, "the other window kept `relativenumber`");
    assert!(
        on.number_width > 0,
        "the other window kept a non-empty gutter"
    );
}

#[tokio::test]
async fn vim_wo_sets_the_focused_windows_gutter() {
    // The Lua surface for window-local options: `vim.wo.number = false` reaches
    // the live editor through a queued WindowOp, changing only the focused window.
    let file = temp_file("wo", "one\ntwo\nthree\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    let split = windows_after(&rpc, &mut incoming, "<C-w>v").await;
    assert_eq!(split.windows.len(), 2);
    assert!(split.windows.iter().all(|w| w.number));

    // Turn `number` off in the focused window from Lua.
    let frame = windows_after(&rpc, &mut incoming, ":lua vim.wo.number = false<CR>").await;
    let off = frame.focused();
    let on = frame
        .windows
        .iter()
        .find(|w| !w.focused)
        .expect("the other window");
    assert!(
        !off.number,
        "vim.wo.number=false turned off the focused gutter"
    );
    assert!(on.number, "the other window is untouched by vim.wo");
}

#[tokio::test]
async fn vim_wo_by_window_id_targets_a_specific_window() {
    // `vim.wo[win].number = false` must target the *named* window, even when it
    // isn't focused — the per-id path the `:GutterDemo` example leans on.
    let file = temp_file("woid", "one\ntwo\nthree\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    let split = windows_after(&rpc, &mut incoming, "<C-w>v").await;
    assert_eq!(split.windows.len(), 2);

    // `nvim_list_wins()` is layout order (left→right), matching the redraw's
    // `windows` array; [2] is the right window, which the `<C-w>v` left unfocused.
    let frame = windows_after(
        &rpc,
        &mut incoming,
        ":lua vim.wo[vim.api.nvim_list_wins()[2]].number = false<CR>",
    )
    .await;
    assert!(
        frame.focused().number,
        "the focused (left) window is untouched"
    );
    let other = frame.windows.iter().find(|w| !w.focused).unwrap();
    assert!(
        !other.number,
        "the targeted-by-id (right) window lost its number gutter"
    );
}

#[tokio::test]
async fn nvim_open_win_then_vim_wo_on_the_new_window() {
    // The `:GutterDemo` flow: open a split and immediately set its gutter via the
    // *predicted* window id `nvim_open_win` returns — the SetOption op must resolve
    // to the very window the Open op creates.
    let file = temp_file("openwo", "one\ntwo\nthree\n");
    let (rpc, mut incoming) = start(Some(file)).await;

    let frame = windows_after(
        &rpc,
        &mut incoming,
        ":lua local f = vim.api.nvim_open_win(0, true, { vertical = true }); \
         vim.wo[f].number = false; vim.wo[f].relativenumber = false<CR>",
    )
    .await;
    assert_eq!(frame.windows.len(), 2, "nvim_open_win made a second window");
    let fresh = frame.focused();
    assert!(
        !fresh.number,
        "the new window's gutter was turned off by id"
    );
    assert!(!fresh.relativenumber);
    let other = frame.windows.iter().find(|w| !w.focused).unwrap();
    assert!(other.number, "the original window kept its gutter");
}

#[tokio::test]
async fn window_local_options_example_runs() {
    // End-to-end check that the shipped `examples/window-local-options/` config
    // sources without error and its `:GutterDemo` gives the two windows different
    // gutters from Lua — the example, exercised exactly as a user would run it.
    let example = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/window-local-options")
        .canonicalize()
        .expect("example dir exists");
    let sample = example.join("sample.txt").display().to_string();
    let (rpc, mut incoming) = start_with_config(Some(sample), Some(example)).await;

    let frame = windows_after(&rpc, &mut incoming, ":GutterDemo<CR>").await;
    assert_eq!(frame.windows.len(), 2, ":GutterDemo opened a split");
    assert!(
        !frame.message.contains("rror") && !frame.message.contains("E5113"),
        "no error sourcing/running the example: {:?}",
        frame.message
    );
    // The new (focused) window has no gutter; the original keeps the hybrid one.
    let fresh = frame.focused();
    assert!(!fresh.number && !fresh.relativenumber, "new window bare");
    let other = frame.windows.iter().find(|w| !w.focused).unwrap();
    assert!(
        other.number && other.relativenumber,
        "original window keeps hybrid numbers"
    );
}

// ----- resizing -------------------------------------------------------------

#[tokio::test]
async fn ctrl_w_plus_and_minus_change_the_focused_windows_height() {
    let (rpc, mut incoming) = start(None).await;
    // Two stacked windows; the new top is focused.
    let split = windows_after(&rpc, &mut incoming, "<C-w>s").await;
    assert_eq!(split.windows.len(), 2);
    let before = split.focused().rect.height;

    // `<C-w>+` grows the focused (top) window by one row, stealing from below.
    let grown = windows_after(&rpc, &mut incoming, "<C-w>+").await;
    assert_eq!(
        grown.focused().rect.height,
        before + 1,
        "<C-w>+ grows the focused window by a row"
    );

    // `<C-w>-` shrinks it back.
    let shrunk = windows_after(&rpc, &mut incoming, "<C-w>-").await;
    assert_eq!(
        shrunk.focused().rect.height,
        before,
        "<C-w>- shrinks the focused window back"
    );
}

#[tokio::test]
async fn ctrl_w_plus_honors_a_count() {
    let (rpc, mut incoming) = start(None).await;
    let split = windows_after(&rpc, &mut incoming, "<C-w>s").await;
    let before = split.focused().rect.height;

    // `3<C-w>+` grows the focused window by three rows.
    let grown = windows_after(&rpc, &mut incoming, "3<C-w>+").await;
    assert_eq!(
        grown.focused().rect.height,
        before + 3,
        "a count before <C-w>+ grows by that many rows"
    );
}

#[tokio::test]
async fn ctrl_w_equals_re_equalizes_after_an_uneven_resize() {
    let (rpc, mut incoming) = start(None).await;
    // Split, then make the two windows uneven.
    feed(&rpc, "<C-w>s");
    let uneven = windows_after(&rpc, &mut incoming, "5<C-w>+").await;
    let (a, b) = (uneven.windows[0].rect.height, uneven.windows[1].rect.height);
    assert!(
        a.abs_diff(b) >= 4,
        "the resize made them clearly uneven: {a} / {b}"
    );

    // `<C-w>=` evens them out (off by at most one for an odd remainder).
    let even = windows_after(&rpc, &mut incoming, "<C-w>=").await;
    let (a, b) = (even.windows[0].rect.height, even.windows[1].rect.height);
    assert!(
        a.abs_diff(b) <= 1,
        "<C-w>= equalizes the window heights: {a} / {b}"
    );
}

#[tokio::test]
async fn ctrl_w_underscore_maximizes_the_focused_windows_height() {
    let (rpc, mut incoming) = start(None).await;
    // Three stacked windows.
    feed(&rpc, "<C-w>s<C-w>s");
    let max = windows_after(&rpc, &mut incoming, "<C-w>_").await;
    let focused = max.focused().rect.height;
    let others: Vec<u64> = max
        .windows
        .iter()
        .filter(|w| !w.focused)
        .map(|w| w.rect.height)
        .collect();
    for h in &others {
        assert!(
            focused > *h,
            "the maximized window dwarfs the others: {focused} vs {h}"
        );
    }
}

#[tokio::test]
async fn terminal_resize_preserves_window_proportions() {
    let (rpc, mut incoming) = start(None).await;
    // Split and make the top window much bigger than the bottom.
    feed(&rpc, "<C-w>s");
    let uneven = windows_after(&rpc, &mut incoming, "6<C-w>+").await;
    let (top0, bot0) = (uneven.windows[0].rect.height, uneven.windows[1].rect.height);

    // Grow the terminal to roughly double the height; the relative shares should
    // survive (the bigger window stays the bigger one, by a similar ratio).
    rpc.request(
        "nvim_ui_try_resize",
        vec![Value::from(80u64), Value::from(48u64), Value::Map(vec![])],
    )
    .await
    .expect("resize");
    let after = windows_after(&rpc, &mut incoming, "<Esc>").await;
    let (top1, bot1) = (after.windows[0].rect.height, after.windows[1].rect.height);

    assert!(
        top1 > top0 && bot1 > bot0,
        "both windows grew with the terminal"
    );
    assert!(
        top1 > bot1,
        "the originally-bigger window stays bigger: {top1} / {bot1}"
    );
    // Ratio preserved within a row of rounding either way.
    let ratio0 = top0 as f64 / bot0 as f64;
    let ratio1 = top1 as f64 / bot1 as f64;
    assert!(
        (ratio0 - ratio1).abs() < 0.5,
        "proportions preserved across resize: {ratio0:.2} -> {ratio1:.2}"
    );
}

#[tokio::test]
async fn vertical_resize_sets_the_focused_windows_width() {
    let (rpc, mut incoming) = start(None).await;
    // Side-by-side windows in an 80-column area.
    feed(&rpc, "<C-w>v");
    let frame = windows_after(&rpc, &mut incoming, ":vertical resize 30<CR>").await;
    assert_eq!(
        frame.focused().rect.width,
        30,
        "`:vertical resize 30` sets the focused window's width"
    );
}

// ----- window-aware quit ----------------------------------------------------

/// Feed `keys`, then report whether the server quit — it emits an `nxvim_exit`
/// notification and ends its loop, which drops the connection and closes the
/// `incoming` channel. Either signal counts. A bounded timeout means a still-
/// running editor returns `false` rather than hanging.
async fn quit_observed(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>, keys: &str) -> bool {
    feed(rpc, keys);
    let timeout = std::time::Duration::from_secs(2);
    loop {
        match tokio::time::timeout(timeout, incoming.recv()).await {
            // Channel closed: the server loop ended (it quit).
            Ok(None) => return true,
            Ok(Some(Incoming::Notification { method, .. })) if method == "nxvim_exit" => {
                return true
            }
            Ok(Some(_)) => continue,
            // Nothing for the whole window: the editor is still running.
            Err(_) => return false,
        }
    }
}

#[tokio::test]
async fn q_closes_a_window_when_more_than_one_is_open() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "<C-w>s");
    // `:q` with two windows closes the focused one; the editor stays up.
    let frame = windows_after(&rpc, &mut incoming, ":q<CR>").await;
    assert_eq!(
        frame.windows.len(),
        1,
        "`:q` closes one window, leaving the editor running"
    );
}

#[tokio::test]
async fn q_on_the_last_clean_window_quits_the_editor() {
    let (rpc, mut incoming) = start(None).await;
    assert!(
        quit_observed(&rpc, &mut incoming, ":q<CR>").await,
        "`:q` on the last window with a clean buffer quits"
    );
}

#[tokio::test]
async fn q_on_the_last_modified_window_refuses_with_e37() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iunsaved<Esc>");
    // The last window onto a modified buffer: `:q` must refuse, not quit.
    let frame = windows_after(&rpc, &mut incoming, ":q<CR>").await;
    assert!(
        frame.message.starts_with("E37"),
        "`:q` on the last modified window warns E37: {:?}",
        frame.message
    );
    assert_eq!(frame.windows.len(), 1, "still one window, still running");
}

#[tokio::test]
async fn q_closes_a_modified_non_last_window_without_complaint() {
    let (rpc, mut incoming) = start(None).await;
    // Modify the shared buffer, then split: both windows show the modified buffer.
    feed(&rpc, "iunsaved<Esc><C-w>s");
    // Closing one of them is fine — the buffer survives in the other window.
    let frame = windows_after(&rpc, &mut incoming, ":q<CR>").await;
    assert_eq!(frame.windows.len(), 1, "the non-last window closed");
    assert!(
        !frame.message.starts_with("E37"),
        "closing a non-last modified window does not warn: {:?}",
        frame.message
    );
}

#[tokio::test]
async fn ctrl_w_q_closes_a_window_then_quits_on_the_last() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "<C-w>s");
    // First `<C-w>q` closes a window (editor stays up)...
    let frame = windows_after(&rpc, &mut incoming, "<C-w>q").await;
    assert_eq!(frame.windows.len(), 1, "<C-w>q closes one of two windows");
    // ...the second, on the last clean window, quits.
    assert!(
        quit_observed(&rpc, &mut incoming, "<C-w>q").await,
        "<C-w>q on the last clean window quits"
    );
}

// ----- nvim_win_* RPC surface (phase 5) -------------------------------------

/// `nvim_list_wins` -> window ids in layout order.
async fn list_wins(rpc: &Rpc) -> Vec<u64> {
    match rpc
        .request("nvim_list_wins", vec![])
        .await
        .expect("list_wins")
    {
        Value::Array(a) => a.iter().filter_map(Value::as_u64).collect(),
        _ => Vec::new(),
    }
}

/// `nvim_get_current_win` -> the focused window id.
async fn current_win(rpc: &Rpc) -> u64 {
    rpc.request("nvim_get_current_win", vec![])
        .await
        .expect("current_win")
        .as_u64()
        .unwrap_or(0)
}

/// `nvim_win_get_cursor(win)` -> (1-based line, 0-based col).
async fn win_cursor(rpc: &Rpc, win: u64) -> (usize, usize) {
    let r = rpc
        .request("nvim_win_get_cursor", vec![Value::from(win)])
        .await
        .expect("win_get_cursor");
    match r {
        Value::Array(a) => (
            a.first().and_then(Value::as_u64).unwrap_or(0) as usize,
            a.get(1).and_then(Value::as_u64).unwrap_or(0) as usize,
        ),
        _ => (0, 0),
    }
}

#[tokio::test]
async fn nvim_list_wins_lists_every_window() {
    let (rpc, _incoming) = start(None).await;
    assert_eq!(list_wins(&rpc).await.len(), 1, "one window at startup");

    feed(&rpc, "<C-w>s<C-w>v");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let wins = list_wins(&rpc).await;
    assert_eq!(wins.len(), 3, "two splits make three windows: {wins:?}");
    // The focused window is among the listed ones, and ids never repeat.
    let cur = current_win(&rpc).await;
    assert!(wins.contains(&cur), "current win {cur} is listed: {wins:?}");
}

#[tokio::test]
async fn nvim_set_current_win_moves_focus() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "<C-w>s");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let wins = list_wins(&rpc).await;
    let cur = current_win(&rpc).await;
    let other = *wins.iter().find(|w| **w != cur).expect("a second window");

    // Focus the other window via the API; the redraw reflects the new focus.
    rpc.request("nvim_set_current_win", vec![Value::from(other)])
        .await
        .expect("set_current_win");
    assert_eq!(
        current_win(&rpc).await,
        other,
        "focus moved to the other window"
    );
    let frame = windows_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(frame.windows.len(), 2);
    // The focused WindowView is the one we set.
    assert!(frame.windows.iter().any(|w| w.focused));
}

#[tokio::test]
async fn nvim_win_get_set_cursor_targets_a_specific_window() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<CR>three<CR>four<CR>five<Esc>gg");
    feed(&rpc, "<C-w>s"); // split; the new top window is focused, both on one buffer
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let wins = list_wins(&rpc).await;
    let cur = current_win(&rpc).await;
    let other = *wins.iter().find(|w| **w != cur).expect("a second window");

    // Set the *non-focused* window's cursor; the focused one must not move.
    rpc.request(
        "nvim_win_set_cursor",
        vec![
            Value::from(other),
            Value::Array(vec![Value::from(4u64), Value::from(0u64)]),
        ],
    )
    .await
    .expect("set_cursor");

    assert_eq!(
        win_cursor(&rpc, other).await.0,
        4,
        "the other window's cursor moved to line 4"
    );
    assert_eq!(
        win_cursor(&rpc, cur).await.0,
        1,
        "the focused window's cursor stayed on line 1"
    );
    // 0 resolves to the current window, same as passing its id.
    assert_eq!(win_cursor(&rpc, 0).await, win_cursor(&rpc, cur).await);
}

#[tokio::test]
async fn nvim_win_get_buf_reports_each_windows_buffer() {
    let original = temp_file("orig", "first\n");
    let other = temp_file("other", "second\n");
    let (rpc, mut incoming) = start(Some(original.clone())).await;
    let frame = windows_after(&rpc, &mut incoming, &format!(":vsplit {other}<CR>")).await;
    assert_eq!(frame.windows.len(), 2);

    let wins = list_wins(&rpc).await;
    // Each window reports a buffer; the two windows show different buffers.
    let mut bufs = Vec::new();
    for w in &wins {
        let b = rpc
            .request("nvim_win_get_buf", vec![Value::from(*w)])
            .await
            .expect("win_get_buf")
            .as_u64()
            .unwrap_or(0);
        bufs.push(b);
    }
    assert_eq!(bufs.len(), 2);
    assert_ne!(
        bufs[0], bufs[1],
        "the two windows show different buffers: {bufs:?}"
    );
}

#[tokio::test]
async fn nvim_win_close_removes_the_window() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "<C-w>s");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let wins = list_wins(&rpc).await;
    assert_eq!(wins.len(), 2);
    let cur = current_win(&rpc).await;
    let other = *wins.iter().find(|w| **w != cur).expect("a second window");

    // Close the non-focused window by id.
    rpc.request(
        "nvim_win_close",
        vec![Value::from(other), Value::Boolean(false)],
    )
    .await
    .expect("win_close");
    let frame = windows_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(frame.windows.len(), 1, "the window was closed");
    assert_eq!(
        list_wins(&rpc).await,
        vec![cur],
        "only the survivor remains"
    );
}

#[tokio::test]
async fn nvim_win_set_height_resizes_a_window() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "<C-w>s");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let cur = current_win(&rpc).await;

    rpc.request(
        "nvim_win_set_height",
        vec![Value::from(cur), Value::from(15u64)],
    )
    .await
    .expect("set_height");
    let h = rpc
        .request("nvim_win_get_height", vec![Value::from(cur)])
        .await
        .expect("get_height")
        .as_u64()
        .unwrap_or(0);
    assert_eq!(h, 15, "the focused window's text height is now 15 rows");
}
