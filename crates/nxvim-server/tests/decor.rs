//! Behavior tests for `nx.decor` — viewport-scoped decoration providers
//! (`docs/specs/2026-06-11-native-plugin-api.md` §6;
//! `docs/plans/2026-06-15-nx-decor-viewport-decorations.md`).
//!
//! Phase 2: the provider registry + the off-tick dispatch + the `ctx` snapshot. A
//! provider records the `ctx` it was handed into a Lua global; the test drives
//! scrolling over the same msgpack-RPC a UI uses and reads the global back through
//! `nvim_exec_lua` — proving the provider is dispatched off the viewport signal, that
//! the snapshot tracks the visible range (top advances on scroll), and that the
//! `bufs.filetype` filter skips non-matching buffers.
//!
//! Phase 3 (this file too): the publish path → render. A provider that `publish`es
//! `hl` marks has them lowered into its namespace in the extmark layer and painted —
//! asserted through the redraw highlight map: the rainbow spans land on the right
//! cells, scrolling re-colours the newly-revealed lines, and a publish carrying a
//! generation the window has already scrolled past (a stale gen) paints nothing.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, drain_to_latest_redraw, exec_lua, feed, lua_u64, spawn, temp_dir, wait_redraw,
    window0_field,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start(dir: &std::path::Path, init_lua: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(dir.join("init.lua"), init_lua).expect("write init.lua");
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// An `init.lua` registering a `lua`-scoped probe provider (records its `ctx` into a
/// global) and a `rust`-scoped one (sets a flag if it ever runs — it must not, on a
/// `lua` buffer). Each also `publish`es a mark, exercising the publish path.
const PROBE_INIT: &str = r#"
_G.probe = nil
_G.rust_ran = false
nx.decor.provider {
  name = "probe",
  bufs = { filetype = { "lua" } },
  on_range = function(ctx, publish)
    _G.probe = { top = ctx.top, bot = ctx.bot, n = #ctx.lines, ft = ctx.filetype, gen = ctx.gen }
    publish({ { ctx.top, 0, end_col = 1, hl = "Comment" } })
  end,
}
nx.decor.provider {
  name = "rust_only",
  bufs = { filetype = { "rust" } },
  on_range = function(_ctx, _publish)
    _G.rust_ran = true
  end,
}
"#;

/// A rainbow-delimiters provider scoped to `lua` buffers: colours every bracket by
/// nesting depth across three named groups (defined here so they resolve to real
/// styles), publishing one `hl` mark per bracket. The flagship `nx.decor` shape — the
/// render tests assert these marks land on the right cells.
const RAINBOW_INIT: &str = r##"
local R = { "Rainbow1", "Rainbow2", "Rainbow3" }
local COLORS = { "#ff0000", "#00ff00", "#0000ff" }
for i, g in ipairs(R) do
  nx.hl.define(0, g, { fg = COLORS[i] })
end
nx.decor.provider {
  name = "rainbow",
  bufs = { filetype = { "lua" } },
  on_range = function(ctx, publish)
    local marks, depth = {}, 0
    for i, line in ipairs(ctx.lines) do
      local row = ctx.top + i - 1
      for col = 1, #line do
        local c = line:sub(col, col)
        if c == "(" or c == "[" or c == "{" then
          marks[#marks + 1] = { row, col - 1, end_col = col, hl = R[depth % 3 + 1] }
          depth = depth + 1
        elseif c == ")" or c == "]" or c == "}" then
          depth = math.max(0, depth - 1)
          marks[#marks + 1] = { row, col - 1, end_col = col, hl = R[depth % 3 + 1] }
        end
      end
    end
    publish(marks)
  end,
}
"##;

/// A provider that only *records* its viewport `ctx` (win/buf/gen) and never
/// publishes — the baseline for the stale-drop test, where the marks come from a
/// hand-issued `nx._decor_publish` carrying a chosen generation.
const RECORD_INIT: &str = r#"
_G.vp = nil
nx.decor.provider {
  name = "record",
  bufs = { filetype = { "lua" } },
  on_range = function(ctx, _publish)
    _G.vp = { win = ctx.win, buf = ctx.buf, gen = ctx.gen }
  end,
}
"#;

/// Write a `.lua` file with `n` numbered lines, in `dir`, and return its path. Each
/// line carries a `{ … }` so the rainbow provider has a bracket to colour on it.
fn write_big_lua(dir: &std::path::Path, n: usize) -> std::path::PathBuf {
    let body: String = (0..n)
        .map(|i| format!("local x{i} = {{ {i} }}\n"))
        .collect();
    let path = dir.join("big.lua");
    std::fs::write(&path, body).expect("write big.lua");
    path
}

/// The highlight spans on screen `row` of the focused window, as
/// `(start_col, end_col, group)` — the redraw highlight tuple
/// `[start, end, group, style_id]` with the style-palette id dropped.
fn row_spans(map: &[(Value, Value)], row: usize) -> Vec<(u64, u64, String)> {
    let rows = match window0_field(map, "highlights").and_then(Value::as_array) {
        Some(rows) => rows,
        None => return Vec::new(),
    };
    let Some(spans) = rows.get(row).and_then(Value::as_array) else {
        return Vec::new();
    };
    spans
        .iter()
        .filter_map(|span| {
            let span = span.as_array()?;
            let start = span.first()?.as_u64()?;
            let end = span.get(1)?.as_u64()?;
            let group = span.get(2)?.as_str()?.to_string();
            Some((start, end, group))
        })
        .collect()
}

/// Whether any span on screen `row` carries highlight group `group`.
fn row_has_group(map: &[(Value, Value)], row: usize, group: &str) -> bool {
    row_spans(map, row).iter().any(|(_, _, g)| g == group)
}

/// The highlight group covering cell `col` on screen `row` (the span whose
/// `[start, end)` contains `col`), if any. Adjacent same-group cells are coalesced
/// into one span by the projection, so address a *cell*, not a span boundary.
fn group_at(map: &[(Value, Value)], row: usize, col: u64) -> Option<String> {
    row_spans(map, row)
        .into_iter()
        .find(|(s, e, _)| *s <= col && col < *e)
        .map(|(_, _, g)| g)
}

// ===== Phase 2: dispatch + snapshot ==========================================

#[tokio::test]
async fn provider_is_dispatched_with_the_visible_slice_and_tracks_scroll() {
    let dir = temp_dir("decor_scroll");
    let (rpc, _incoming) = start(&dir, PROBE_INIT).await;
    let path = write_big_lua(&dir, 200);

    // Open the file — switching to the `lua` buffer is a viewport change, so the
    // provider runs with the freshly-visible top-of-file slice.
    feed(&rpc, &format!(":e {}<CR>", path.display()));

    let top0 = lua_u64(&rpc, "return _G.probe and _G.probe.top").await;
    assert_eq!(top0, Some(0), "first dispatch sees the top of the file");
    let ft = exec_lua(&rpc, "return _G.probe.ft").await;
    assert_eq!(
        ft.as_str(),
        Some("lua"),
        "ctx.filetype is the buffer filetype"
    );
    let n = lua_u64(&rpc, "return _G.probe.n").await.unwrap();
    let bot0 = lua_u64(&rpc, "return _G.probe.bot").await.unwrap();
    // `lines` is exactly the [top, bot] slice — a full screen, well short of 200.
    assert_eq!(n, bot0 + 1, "ctx.lines covers exactly top..=bot");
    assert!(
        (10..200).contains(&n),
        "a viewport-sized slice, not the whole buffer: {n}"
    );

    // The `rust`-scoped provider never fires on a `lua` buffer (the bufs filter).
    assert_eq!(
        exec_lua(&rpc, "return _G.rust_ran").await.as_bool(),
        Some(false),
        "a filetype-scoped provider skips non-matching buffers"
    );

    // Jump to the bottom: the viewport scrolls, so the provider re-runs with an
    // advanced top reflecting the new visible range.
    feed(&rpc, "G");
    let top1 = lua_u64(&rpc, "return _G.probe.top").await.unwrap();
    assert!(
        top1 > 0,
        "scrolling re-dispatches with the moved viewport: {top1}"
    );
}

#[tokio::test]
async fn publish_records_normalized_marks() {
    let dir = temp_dir("decor_publish");
    let (rpc, _incoming) = start(&dir, PROBE_INIT).await;
    let path = write_big_lua(&dir, 50);
    feed(&rpc, &format!(":e {}<CR>", path.display()));

    // The probe publishes one mark per dispatch — recorded Lua-side for inspection. It
    // is normalized into the canonical positional→named form the extmark layer takes.
    let row = lua_u64(&rpc, "return nx._decor.last.marks[1].row").await;
    assert_eq!(row, Some(0), "positional row survives normalization");
    let end_col = lua_u64(&rpc, "return nx._decor.last.marks[1].end_col").await;
    assert_eq!(end_col, Some(1), "named end_col is carried through");
    let hl = exec_lua(&rpc, "return nx._decor.last.marks[1].hl").await;
    assert_eq!(
        hl.as_str(),
        Some("Comment"),
        "the hl group is carried through"
    );
}

#[tokio::test]
async fn a_mark_without_an_hl_fails_loud() {
    // v1 renders `hl` only; a mark carrying no `hl` can render nothing, so rather than
    // silently no-op it routes through the provider-error path (Decision 6). The
    // provider is isolated (the dispatch survives), and the error is surfaced.
    let dir = temp_dir("decor_no_hl");
    let init = r#"
_G.err = nil
nx.notify = function(msg, _level) _G.err = msg end
nx.decor.provider {
  name = "no_hl",
  bufs = { filetype = { "lua" } },
  on_range = function(ctx, publish)
    publish({ { ctx.top, 0, end_col = 1 } })   -- no hl
  end,
}
"#;
    let (rpc, _incoming) = start(&dir, init).await;
    let path = write_big_lua(&dir, 20);
    feed(&rpc, &format!(":e {}<CR>", path.display()));

    let err = exec_lua(&rpc, "return _G.err").await;
    assert!(
        err.as_str().is_some_and(|m| m.contains("hl")),
        "a hl-less mark is reported loud, not dropped: {err:?}"
    );
}

// ===== Phase 3: publish → render =============================================

#[tokio::test]
async fn colours_on_startup_without_any_interaction() {
    // The real path: the file is opened at boot (the command-line arg), not via a `:e`
    // keystroke. The marks must be on the FIRST frame the client paints after attach —
    // a fresh session shouldn't need a keypress to colour. (`nvim_ui_attach` assigns the
    // window its first rect, so the provider's viewport is only then known; the attach
    // arm drives `run_pending` to dispatch it before that frame.) Without the fix this
    // test times out: no interaction ever produces a coloured frame.
    let dir = temp_dir("decor_startup");
    std::fs::write(dir.join("init.lua"), RAINBOW_INIT).expect("write init.lua");
    let path = dir.join("nest.lua");
    std::fs::write(&path, "return (())\n").expect("write nest.lua");
    let init = ServerInit {
        file: Some(path.to_string_lossy().into_owned()),
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    };
    let (rpc, mut incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    // No feed — the very first frame must already be coloured.
    let map = wait_redraw(&mut incoming, |m| row_has_group(m, 0, "Rainbow1")).await;
    assert_eq!(
        group_at(&map, 0, 7).as_deref(),
        Some("Rainbow1"),
        "the startup frame colours the brackets with no keypress: {:?}",
        row_spans(&map, 0)
    );
}

#[tokio::test]
async fn rainbow_marks_render_on_the_bracket_cells() {
    let dir = temp_dir("decor_render");
    let (rpc, mut incoming) = start(&dir, RAINBOW_INIT).await;
    // `return (())` — the brackets nest 0,1 then unwind: ( @col7 depth0→R1,
    // ( @col8 depth1→R2, ) @col9 →R2, ) @col10 →R1.
    let path = dir.join("nest.lua");
    std::fs::write(&path, "return (())\n").expect("write nest.lua");

    feed(&rpc, &format!(":e {}<CR>", path.display()));
    // Wait for the frame carrying the published marks (the provider runs off-tick in
    // run_pending; the marks fold into the next redraw).
    let map = wait_redraw(&mut incoming, |m| row_has_group(m, 0, "Rainbow1")).await;

    // Address cells (not span boundaries — adjacent same-group cells coalesce): the
    // `( ( ) )` at cells 7,8,9,10 colour R1, R2, R2, R1 by nesting depth.
    let spans = row_spans(&map, 0);
    assert_eq!(
        group_at(&map, 0, 7).as_deref(),
        Some("Rainbow1"),
        "outer ( at depth 0: {spans:?}"
    );
    assert_eq!(
        group_at(&map, 0, 8).as_deref(),
        Some("Rainbow2"),
        "inner ( at depth 1: {spans:?}"
    );
    assert_eq!(
        group_at(&map, 0, 9).as_deref(),
        Some("Rainbow2"),
        "inner ) closes depth 1: {spans:?}"
    );
    assert_eq!(
        group_at(&map, 0, 10).as_deref(),
        Some("Rainbow1"),
        "outer ) closes depth 0: {spans:?}"
    );
}

#[tokio::test]
async fn an_on_screen_edit_recolors_without_scrolling() {
    // The viewport key carries the buffer's changedtick, so an edit that leaves the
    // visible range unchanged (typing a bracket on screen — no scroll, same top/bot)
    // still re-dispatches the provider. Without it a fresh bracket would stay
    // uncoloured until the next scroll — the central rainbow case.
    let dir = temp_dir("decor_edit");
    let (rpc, mut incoming) = start(&dir, RAINBOW_INIT).await;
    let path = dir.join("e.lua");
    std::fs::write(&path, "return x\n").expect("write e.lua");
    feed(&rpc, &format!(":e {}<CR>", path.display()));
    // No brackets on line 0 yet.
    let opened = wait_redraw(&mut incoming, |m| window0_field(m, "lines").is_some()).await;
    assert!(
        !row_has_group(&opened, 0, "Rainbow1"),
        "no bracket, no rainbow span yet"
    );

    // Append `()` to `return x` → `return x()`: `(` at col 8, `)` at col 9, both depth 0.
    feed(&rpc, "A()<Esc>");
    let map = wait_redraw(&mut incoming, |m| row_has_group(m, 0, "Rainbow1")).await;
    assert_eq!(
        group_at(&map, 0, 8).as_deref(),
        Some("Rainbow1"),
        "the just-typed ( colours without a scroll: {:?}",
        row_spans(&map, 0)
    );
}

#[tokio::test]
async fn scrolling_recolors_the_newly_revealed_lines() {
    let dir = temp_dir("decor_render_scroll");
    let (rpc, mut incoming) = start(&dir, RAINBOW_INIT).await;
    let path = write_big_lua(&dir, 200);

    // Top of file: the first line's `{ … }` colours immediately.
    feed(&rpc, &format!(":e {}<CR>", path.display()));
    let top = wait_redraw(&mut incoming, |m| row_has_group(m, 0, "Rainbow1")).await;
    assert!(
        row_has_group(&top, 0, "Rainbow1"),
        "the top line's bracket is coloured on open"
    );
    // The opening line of a content buffer is buffer line 0 — far below 200 is off
    // screen, so its bracket cannot already be in this frame.
    let last_screen_row = window0_field(&top, "lines")
        .and_then(Value::as_array)
        .map(|l| l.len())
        .unwrap_or(0);
    assert!(
        last_screen_row < 200,
        "the whole file does not fit on screen"
    );

    // Jump to the bottom: the now-visible high-numbered lines must colour as they
    // come into view (the provider re-ran for the moved viewport and republished).
    feed(&rpc, "G");
    let bottom = wait_redraw(&mut incoming, |m| {
        // The last buffer line is `return { … }`-shaped; find any coloured bracket on
        // a row that wasn't visible at the top (use the final visible content row).
        (0..24).any(|r| row_has_group(m, r, "Rainbow1"))
    })
    .await;
    // The final content line (buffer line 199) is visible and coloured. Its `{` sits
    // after `local x199 = ` — assert *some* visible row past the original viewport
    // carries a rainbow span (the file's first screen was rows 0..~22).
    let coloured_rows: Vec<usize> = (0..24)
        .filter(|&r| row_has_group(&bottom, r, "Rainbow1"))
        .collect();
    assert!(
        !coloured_rows.is_empty(),
        "scrolling to the bottom colours the freshly-revealed brackets"
    );
}

#[tokio::test]
async fn the_rainbow_example_colours_its_sample_end_to_end() {
    // The flagship `examples/rainbow/` config, verified end-to-end: load the real
    // init.lua, open its bracket-dense sample, and assert the brackets colour across
    // the depth groups — the example actually works, not merely parses.
    let init = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/rainbow/init.lua"
    ))
    .expect("read example init.lua");
    let sample = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/rainbow/sample.lua"
    );
    let dir = temp_dir("decor_example");
    let (rpc, mut incoming) = start(&dir, &init).await;

    feed(&rpc, &format!(":e {sample}<CR>"));
    // The sample nests several levels, so distinct depth groups must appear on screen.
    let map = wait_redraw(&mut incoming, |m| {
        (0..24).any(|r| row_has_group(m, r, "Rainbow2"))
    })
    .await;
    let groups: std::collections::HashSet<String> = (0..24)
        .flat_map(|r| row_spans(&map, r))
        .map(|(_, _, g)| g)
        .filter(|g| g.starts_with("Rainbow"))
        .collect();
    assert!(
        groups.contains("Rainbow1") && groups.contains("Rainbow2"),
        "the example colours nested brackets across depth groups: {groups:?}"
    );
}

#[tokio::test]
async fn a_stale_publish_paints_nothing() {
    // The gen-gate (Decision 4): a publish stamped with a generation the window has
    // already scrolled past is dropped before any mark is set, so a viewport the user
    // left never paints. Driven deterministically by hand-issuing `nx._decor_publish`
    // with a future (never-stamped) generation, then with the live one.
    let dir = temp_dir("decor_stale");
    let (rpc, mut incoming) = start(&dir, RECORD_INIT).await;
    exec_lua(&rpc, "nx.hl.define(0, 'StaleMark', { fg = '#ff00ff' })").await;
    // A multi-line file (its visible range differs from the empty start buffer) so the
    // record provider dispatches and stamps _G.vp with the live generation.
    let path = write_big_lua(&dir, 40);
    feed(&rpc, &format!(":e {}<CR>", path.display()));

    // The live viewport generation (and the provider's namespace + window/buffer).
    let _ = exec_lua(&rpc, "return _G.vp").await;
    let win = lua_u64(&rpc, "return _G.vp.win").await.unwrap();
    let buf = lua_u64(&rpc, "return _G.vp.buf").await.unwrap();
    let gen = lua_u64(&rpc, "return _G.vp.gen").await.unwrap();
    let ns = lua_u64(&rpc, "return nx._decor.providers[1].ns")
        .await
        .unwrap();

    // A publish carrying a generation that is *ahead* of the live one — as if a newer
    // scroll had already superseded it. The server drops it: nothing paints.
    let stale = format!(
        "nx._decor_publish({ns}, {g}, {win}, {buf}, {{0}}, {{0}}, {{0}}, {{1}}, {{'StaleMark'}}, {{-1}})",
        g = gen + 50
    );
    // `nvim_exec_lua` itself repaints (the RPC handler emits a frame after draining the
    // chunk's effects), so the publish lands in the very next frame — no cursor move.
    exec_lua(&rpc, &stale).await;
    let after_stale = wait_redraw(&mut incoming, |m| window0_field(m, "lines").is_some()).await;
    let after_stale = drain_to_latest_redraw(&mut incoming, |_| true).unwrap_or(after_stale);
    assert!(
        !row_has_group(&after_stale, 0, "StaleMark"),
        "a publish from a superseded generation paints nothing: {:?}",
        row_spans(&after_stale, 0)
    );

    // The same publish with the *live* generation paints — proving the drop above was
    // the gen-gate, not a broken publish path. It shows on the next frame with no input.
    let live = format!(
        "nx._decor_publish({ns}, {gen}, {win}, {buf}, {{0}}, {{0}}, {{0}}, {{1}}, {{'StaleMark'}}, {{-1}})"
    );
    exec_lua(&rpc, &live).await;
    let after_live = wait_redraw(&mut incoming, |m| row_has_group(m, 0, "StaleMark")).await;
    assert!(
        row_has_group(&after_live, 0, "StaleMark"),
        "the live-generation publish paints: {:?}",
        row_spans(&after_live, 0)
    );
}
