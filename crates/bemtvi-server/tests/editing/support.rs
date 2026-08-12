//! Shared support for the `editing` test submodules.
//!
//! Re-exports the workspace-wide black-box harness ([`bemtvi_test_harness`]) plus
//! the common RPC / server / msgpack types, then layers on the editing-specific
//! helpers that several submodules share (the `start*` fixtures, the redraw/view
//! accessors, the panel and substitute conveniences). Each submodule pulls the
//! whole surface in with a single `use crate::support::*;`.
// These re-exports are a single glob surface for the submodules; not every one is
// used by every submodule, and a few (the lower-level RPC/server types) are only
// here for completeness, so quiet the unused-import lint on the re-export block.
#![allow(unused_imports)]

pub use bemtvi_test_harness::*;

pub use bemtvi_rpc::{connect, Incoming, Rpc};
pub use bemtvi_server::{run as run_server, ServerInit};
pub use rmpv::Value;
pub use tokio::sync::mpsc::UnboundedReceiver;

pub use std::path::PathBuf;

// ===== server fixtures =======================================================

/// Start a server on its own thread and return a connected, UI-attached client.
pub async fn start(file: Option<String>) -> (Rpc, UnboundedReceiver<Incoming>) {
    start_with(ServerInit {
        file,
        ..Default::default()
    })
    .await
}

/// Like [`start`], but with a fully-specified [`ServerInit`] — used by tests
/// that need an explicit config dir / runtimepath (kept off the host's home).
/// Attaches an 80×25 UI (the windows-area height; each window spends its bottom
/// row on a status line, so the text viewport is 24 rows).
pub async fn start_with(init: ServerInit) -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(init, 80, 25).await
}

/// Start a server whose `"+` / `"*` registers are backed by an injected
/// [`FakeClipboard`]; returns the client plus the shared handle to inspect.
pub async fn start_with_clipboard() -> (Rpc, UnboundedReceiver<Incoming>, FakeClipboard) {
    let fake = FakeClipboard::default();
    let (rpc, incoming) = start_with(ServerInit {
        clipboard: bemtvi_server::ClipboardProvider::Custom(Box::new(fake.clone())),
        ..Default::default()
    })
    .await;
    (rpc, incoming, fake)
}

/// Start a server whose `"+` / `"*` registers fall back to the client terminal's
/// **OSC 52** clipboard — the ssh case: no host clipboard tool on this machine, so
/// the copy rides an escape sequence out to the terminal emulator instead. The UI
/// attaches declaring `osc52` support, the way the real TUI does when it has
/// probed the terminal.
pub async fn start_with_osc52() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_osc52_with_caps(vec![(Value::from("osc52"), Value::Boolean(true))]).await
}

/// [`start_with_osc52`] with an explicit capabilities map, so a test can attach a
/// client that does *not* declare `osc52` (a terminal whose probe came back
/// negative) and assert the registers stay loudly unavailable.
pub async fn start_osc52_with_caps(
    caps: Vec<(Value, Value)>,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (rpc, incoming) = spawn(ServerInit {
        clipboard: bemtvi_server::ClipboardProvider::Osc52,
        ..Default::default()
    });
    attach_with_caps(&rpc, 80, 25, caps).await;
    (rpc, incoming)
}

// ===== redraw / view accessors ===============================================

/// Feed `keys` and return the `redraw` carrying the one-shot `scroll` gesture —
/// the input's own frame, not the state-only barrier repaint that trails it.
pub async fn scroll_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> Vec<(Value, Value)> {
    redraw_after_matching(rpc, incoming, keys, |map| scroll(map).is_some()).await
}

/// Number of entries in the redraw's `lines` array.
pub fn lines_len(map: &[(Value, Value)]) -> usize {
    field(map, "lines")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0)
}

/// The first visible buffer line of the focused window (`lines[0]`) — reveals the
/// viewport `top` for a content-numbered buffer (e.g. `write_n_lines`).
pub fn first_visible_line(map: &[(Value, Value)]) -> String {
    field(map, "lines")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// The `scroll` sub-map, or `None` when the redraw carries no scroll gesture.
pub fn scroll(map: &[(Value, Value)]) -> Option<&Vec<(Value, Value)>> {
    match field(map, "scroll") {
        Some(Value::Map(m)) => Some(m),
        _ => None,
    }
}

/// Read a u64 field out of the `scroll` sub-map.
///
/// The band is now **screen-row based** (its endpoints are `from_row`/`to_row`/
/// `from_cursor_row`/`to_cursor_row` offsets into the band). The legacy
/// buffer-line names (`from_top`/`to_top`/`from_cursor`/`to_cursor`) are derived
/// here from those screen-row offsets via the band's `numbers` (each band row's
/// 1-based buffer line), so existing tests keep asserting absolute 0-based buffer
/// lines without caring how the band is laid out. (For a buffer with no
/// `virt_lines` the screen-row offset equals the buffer-line offset, so the
/// derived value matches the old wire exactly.)
pub fn scroll_u64(map: &[(Value, Value)], key: &str) -> u64 {
    let row_key = match key {
        "from_top" => Some("from_row"),
        "to_top" => Some("to_row"),
        "from_cursor" => Some("from_cursor_row"),
        "to_cursor" => Some("to_cursor_row"),
        _ => None,
    };
    let s = scroll(map).expect("scroll present");
    let raw = |k: &str| {
        s.iter()
            .find(|(kk, _)| kk.as_str() == Some(k))
            .and_then(|(_, v)| v.as_u64())
    };
    let Some(rk) = row_key else {
        return raw(key).unwrap_or_else(|| panic!("scroll.{key} missing"));
    };
    let row = raw(rk).unwrap_or_else(|| panic!("scroll.{rk} missing")) as usize;
    let numbers = scroll_numbers(map);
    numbers
        .get(row)
        .copied()
        .flatten()
        .unwrap_or_else(|| panic!("scroll band row {row} (for {key}) has no buffer line"))
        - 1
}

/// The band's per-row 1-based buffer line numbers (`scroll.numbers`), `None` for
/// virtual / `~` filler rows. Lets a test map a band screen-row offset back to a
/// buffer line.
pub fn scroll_numbers(map: &[(Value, Value)]) -> Vec<Option<u64>> {
    let s = scroll(map).expect("scroll present");
    s.iter()
        .find(|(k, _)| k.as_str() == Some("numbers"))
        .and_then(|(_, v)| v.as_array())
        .map(|a| a.iter().map(Value::as_u64).collect())
        .unwrap_or_default()
}

/// Length of a named per-row array on the scroll band (e.g. `diagnostics`,
/// `diagnostics_signs`), or `None` when the band omits the key. Lets a test assert
/// an overlay rides the band and stays aligned with its rows.
pub fn scroll_array_len(map: &[(Value, Value)], key: &str) -> Option<usize> {
    let s = scroll(map).expect("scroll present");
    s.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .and_then(|(_, v)| v.as_array())
        .map(|a| a.len())
}

/// Number of entries in `scroll.lines`.
pub fn scroll_lines_len(map: &[(Value, Value)]) -> usize {
    let s = scroll(map).expect("scroll present");
    s.iter()
        .find(|(k, _)| k.as_str() == Some("lines"))
        .and_then(|(_, v)| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}

/// The band's visual-selection spans (`scroll.selection`): per band row, the
/// highlighted screen-column span `[start, end)`, or `None`.
pub fn scroll_selection(map: &[(Value, Value)]) -> Vec<Option<(u64, u64)>> {
    let s = scroll(map).expect("scroll present");
    s.iter()
        .find(|(k, _)| k.as_str() == Some("selection"))
        .and_then(|(_, v)| v.as_array())
        .map(|a| {
            a.iter()
                .map(|v| match v.as_array() {
                    Some(p) if p.len() == 2 => {
                        Some((p[0].as_u64().unwrap_or(0), p[1].as_u64().unwrap_or(0)))
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The band's search-match spans (`scroll.search`): per band row, the half-open
/// screen-column spans `[start, end)` of every `hlsearch` match (empty inner vec
/// for rows with no match), so the highlight rides the slide.
pub fn scroll_search(map: &[(Value, Value)]) -> Vec<Vec<(u64, u64)>> {
    let s = scroll(map).expect("scroll present");
    s.iter()
        .find(|(k, _)| k.as_str() == Some("search"))
        .and_then(|(_, v)| v.as_array())
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.as_array()
                        .map(|spans| {
                            spans
                                .iter()
                                .filter_map(|v| match v.as_array() {
                                    Some(p) if p.len() == 2 => Some((
                                        p[0].as_u64().unwrap_or(0),
                                        p[1].as_u64().unwrap_or(0),
                                    )),
                                    _ => None,
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The band's selection orientation (`scroll.sel_extends_down`): `Some(true)` the
/// selection extends downward from its anchor, `Some(false)` upward, `None` when
/// no visual selection is sliding.
pub fn scroll_sel_extends_down(map: &[(Value, Value)]) -> Option<bool> {
    let s = scroll(map).expect("scroll present");
    s.iter()
        .find(|(k, _)| k.as_str() == Some("sel_extends_down"))
        .and_then(|(_, v)| v.as_bool())
}

// ===== view helpers ==========================================================

/// The most recent `redraw` view map currently buffered on the connection.
pub fn latest_view(incoming: &mut UnboundedReceiver<Incoming>) -> Option<Vec<(Value, Value)>> {
    let mut latest = None;
    while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
        if method == "redraw" {
            if let Some(Value::Map(map)) = params.into_iter().next() {
                latest = Some(map);
            }
        }
    }
    latest
}

pub fn view_lines(view: &[(Value, Value)]) -> Vec<String> {
    view_get(view, "lines")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The focused window's per-row `numbers` array as `Option<u64>` (None = `~`
/// filler / virtual row). Unlike [`numbers`], this resolves through the window.
pub fn view_numbers(view: &[(Value, Value)]) -> Vec<Option<u64>> {
    view_get(view, "numbers")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(Value::as_u64).collect())
        .unwrap_or_default()
}

/// Per visible row, the soft-wrap `continuation` flag (`true` on a buffer line's
/// 2nd+ display row) the client reads to blank the number gutter.
pub fn view_continuation(view: &[(Value, Value)]) -> Vec<bool> {
    view_get(view, "continuation")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|v| v.as_bool().unwrap_or(false)).collect())
        .unwrap_or_default()
}

/// Per visible row, the highlighted screen-column span `[start, end)`, or
/// `None` for rows with no visual selection.
pub fn view_selection(view: &[(Value, Value)]) -> Vec<Option<(u64, u64)>> {
    view_get(view, "selection")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| match v.as_array() {
                    Some(pair) if pair.len() == 2 => {
                        Some((pair[0].as_u64().unwrap_or(0), pair[1].as_u64().unwrap_or(0)))
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Per visible row, the highlighted screen-column spans `[start, end)` of every
/// *secondary* cursor's visual selection (the primary's lives in `selection`).
/// Empty inner vecs for rows no secondary selection touches.
pub fn view_secondary_selection(view: &[(Value, Value)]) -> Vec<Vec<(u64, u64)>> {
    view_get(view, "secondary_selection")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|row| {
                    row.as_array()
                        .map(|spans| {
                            spans
                                .iter()
                                .filter_map(|v| {
                                    let pair = v.as_array()?;
                                    Some((pair.first()?.as_u64()?, pair.get(1)?.as_u64()?))
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Per visible row, the diagnostic underline spans as `(start_col, end_col,
/// severity)` (the `style_id` is dropped). Empty inner vecs for rows with none.
pub fn view_diag_spans(view: &[(Value, Value)]) -> Vec<Vec<(u64, u64, u64)>> {
    view_get(view, "diagnostics")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|row| {
                    row.as_array()
                        .map(|spans| {
                            spans
                                .iter()
                                .filter_map(|v| {
                                    let s = v.as_array()?;
                                    Some((
                                        s.first()?.as_u64()?,
                                        s.get(1)?.as_u64()?,
                                        s.get(2)?.as_u64()?,
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Per visible row, the gutter sign as `(glyph, severity)`, or `None` when the
/// row carries none. Shared shape for `diagnostics_signs` / `diagnostics_virt`.
fn view_diag_pairs(view: &[(Value, Value)], key: &str) -> Vec<Option<(String, u64)>> {
    view_get(view, key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|row| {
                    let s = row.as_array()?;
                    Some((s.first()?.as_str()?.to_string(), s.get(1)?.as_u64()?))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Per visible row, the diagnostic gutter sign `(glyph, severity)` or `None`.
pub fn view_diag_signs(view: &[(Value, Value)]) -> Vec<Option<(String, u64)>> {
    view_diag_pairs(view, "diagnostics_signs")
}

/// Per visible row, the inline diagnostic virtual text `(text, severity)` or `None`.
pub fn view_diag_virt(view: &[(Value, Value)]) -> Vec<Option<(String, u64)>> {
    view_diag_pairs(view, "diagnostics_virt")
}

pub fn view_str(view: &[(Value, Value)], key: &str) -> String {
    view_get(view, key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

pub fn view_u64(view: &[(Value, Value)], key: &str) -> u64 {
    view_get(view, key).and_then(Value::as_u64).unwrap_or(0)
}

pub fn view_get<'a>(view: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    view.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
        .or_else(|| window0_field(view, key))
}

// ===== numbers / misc redraw =================================================

pub fn field_bool(map: &[(Value, Value)], key: &str) -> bool {
    field(map, key).and_then(Value::as_bool).unwrap_or(false)
}

/// The redraw's per-row `numbers` array as `Option<u64>` (None = `~` filler).
pub fn numbers(map: &[(Value, Value)]) -> Vec<Option<u64>> {
    field(map, "numbers")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(Value::as_u64).collect())
        .unwrap_or_default()
}

// ===== highlight conveniences ================================================

/// `#rrggbb` as the `0xRRGGBB` integer the highlight RPCs report colors as.
pub fn hex(rgb: &str) -> u64 {
    u32::from_str_radix(rgb.trim_start_matches('#'), 16).expect("hex color") as u64
}

/// A color field (`fg`/`bg`/`sp`) from a resolved-style map.
pub fn hl_color(map: &[(Value, Value)], key: &str) -> Option<u64> {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .and_then(|(_, v)| v.as_u64())
}

// ===== config fixture ========================================================

/// The harness [`bemtvi_test_harness::start_with_config`], at this suite's 80×25
/// geometry (see [`start_with`]) — shadowing the glob re-export on purpose.
pub async fn start_with_config(
    dir: &std::path::Path,
    init_lua: &str,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    start_with(config_init(dir, init_lua)).await
}

/// The message line from the redraw produced by a no-op input — i.e. whatever
/// `init.lua` left behind at startup.
pub async fn startup_message(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> String {
    let map = redraw_after(rpc, incoming, "").await;
    field(&map, "message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

// ===== panel =================================================================

/// Drain to the *latest* redraw — the one reflecting the settled state after the
/// preceding action. A barrier (`nvim_get_mode`) ensures that action's redraw is
/// already queued; unlike [`redraw_after`] this tolerates leftover redraws from
/// earlier fire-and-forget `feed`s/requests still in the channel.
pub async fn drain_latest(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Vec<(Value, Value)> {
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    tokio::task::yield_now().await; // let the reader task push buffered frames
    let mut latest = None;
    while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
        if method == "redraw" {
            latest = params.into_iter().next();
        }
    }
    match latest {
        Some(Value::Map(map)) => map,
        _ => panic!("no redraw arrived"),
    }
}

/// Feed `keys`, then drain to the latest redraw (see [`drain_latest`]).
pub async fn latest_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> Vec<(Value, Value)> {
    rpc.notify("btv_input", vec![Value::from(keys)]);
    drain_latest(rpc, incoming).await
}

// ===== substitute / fixtures =================================================

/// `vim.fn.substitute(input, pat, sub, flags)` via the live VM. `pat`/`sub` ride
/// Lua long-bracket literals so vim backslashes pass through unescaped.
pub async fn substitute(rpc: &Rpc, input: &str, pat: &str, sub: &str, flags: &str) -> String {
    let code =
        format!("return vim.fn.substitute({input:?}, [==[{pat}]==], [==[{sub}]==], {flags:?})");
    exec_lua(rpc, &code)
        .await
        .as_str()
        .unwrap_or("<not a string>")
        .to_string()
}

/// Build a small three-line buffer ("foo bar" / "baz foo" / "qux foo") and park
/// the cursor at the top, for the search tests below.
pub async fn search_fixture() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (rpc, incoming) = start(None).await;
    feed(&rpc, "ifoo bar<CR>baz foo<CR>qux foo<Esc>gg");
    assert_eq!(
        lines(&rpc).await,
        vec!["foo bar", "baz foo", "qux foo"],
        "fixture buffer"
    );
    (rpc, incoming)
}

pub async fn range_fixture() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (rpc, incoming) = start(None).await;
    // Five lines; line 4 is indented so we can see the cursor land on the
    // first non-blank rather than column 0.
    feed(&rpc, "ione<CR>two<CR>three<CR>    four<CR>five<Esc>gg");
    assert_eq!(
        lines(&rpc).await,
        vec!["one", "two", "three", "    four", "five"],
        "fixture buffer"
    );
    (rpc, incoming)
}
