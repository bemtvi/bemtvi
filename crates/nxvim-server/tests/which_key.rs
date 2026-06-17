//! End-to-end proof that a native **which-key** renders: the `nx.on_key_pending`
//! oracle (the engine's pending-prefix signal) driving a persistent
//! `nx.ui.float`, exactly as `examples/which-key` wires it. Black-box like the
//! rest — a real server sources a which-key `init.lua`, keys arrive over the same
//! msgpack-RPC a UI uses, and the assertions are on the projected `float` redraw
//! surface (the popup a client paints). Debounce is set to 0ms here so the float
//! opens on the next tick rather than after a wall-clock pause.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{attach, drain_to_latest_redraw, feed, map_get, spawn, temp_dir};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// The which-key glue from `examples/which-key/init.lua`, verbatim in spirit but
/// with a 0ms debounce (`which_key_delay`) so the popup opens on the next event
/// loop tick instead of after the 200ms human pause.
const WHICH_KEY: &str = r##"
vim.g.mapleader = " "
vim.g.which_key_delay = 0

nx.hl.define(0, "WhichKey", { fg = "#7dcfff" })
nx.hl.define(0, "WhichKeyGroup", { fg = "#bb9af7", bold = true })
nx.hl.define(0, "WhichKeyDesc", { fg = "#c0caf5" })
nx.hl.define(0, "WhichKeyDim", { fg = "#565f89", italic = true })

nx.keymap.set("n", "<leader>w", function() end, { desc = "write" })
nx.keymap.set("n", "<leader>q", function() end, { desc = "quit" })
nx.keymap.set("n", "<leader>ff", function() end, { desc = "find file" })
nx.keymap.set("n", "<leader>fg", function() end, { desc = "live grep" })

local DELAY = vim.g.which_key_delay or 200

local function lines_for(ctx)
  if #ctx.continuations == 0 then
    return { { { string.format(" %s ", ctx.label or "…"), "WhichKeyDesc" } } }
  end
  local keyw = 1
  for _, c in ipairs(ctx.continuations) do
    keyw = math.max(keyw, vim.fn.strdisplaywidth(c.key))
  end
  local rows = {}
  for _, c in ipairs(ctx.continuations) do
    local pad = string.rep(" ", keyw - vim.fn.strdisplaywidth(c.key))
    local dim = c.available == false
    local label, label_hl
    if c.kind == "group" then
      label = "+" .. (c.desc ~= "" and c.desc or "more")
      label_hl = "WhichKeyGroup"
    else
      label = c.desc ~= "" and c.desc or ""
      label_hl = "WhichKeyDesc"
    end
    rows[#rows + 1] = {
      { " ", nil },
      { c.key, dim and "WhichKeyDim" or "WhichKey" },
      { pad .. "   ", nil },
      { label, dim and "WhichKeyDim" or label_hl },
      { " ", nil },
    }
  end
  return rows
end

local popup

local open = nx.utils.debounce(function(ctx)
  local lines = lines_for(ctx)
  local title = " " .. ctx.keys
  if ctx.label and ctx.label ~= "" then
    title = title .. " — " .. ctx.label
  end
  title = title .. " "
  if popup and popup:is_open() then
    popup:update(lines, { title = title, relative = "bottom" })
  else
    popup = nx.ui.float(lines, { persist = true, title = title, border = "rounded", relative = "bottom" })
  end
end, DELAY)

nx.on_key_pending(function(ctx)
  if ctx.keys == "" then
    open:cancel()
    if popup then popup:close(); popup = nil end
    return
  end
  open(ctx)
end)
"##;

async fn start(init_lua: &str) -> (Rpc, std::path::PathBuf, UnboundedReceiver<Incoming>) {
    let dir = temp_dir("which_key");
    std::fs::write(dir.join("init.lua"), init_lua).expect("write init.lua");
    let init = ServerInit {
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir.clone()],
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, dir, incoming)
}

/// Poll for the latest redraw whose `float` key satisfies `want` (a map when the
/// popup is open, `Nil` when it is gone), retrying so the reader task settles and
/// the 0ms debounce timer fires (take-latest pattern, like `ui_float.rs`).
async fn poll_float(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    want: impl Fn(&Value) -> bool,
) -> Option<Vec<(Value, Value)>> {
    for _ in 0..60 {
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) =
            drain_to_latest_redraw(incoming, |m| map_get(m, "float").is_some_and(&want))
        {
            if let Some(Value::Map(f)) = map_get(&map, "float") {
                return Some(f.clone());
            }
            return Some(vec![]); // float == Nil (closed)
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    None
}

/// Poll for the latest redraw whose `float` is an open map, returning the WHOLE
/// redraw map (so a test can read the `styles` palette the float's style ids index,
/// not just the float sub-map `poll_float` returns).
async fn drain_to_latest_redraw_open(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Vec<(Value, Value)> {
    for _ in 0..60 {
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| {
            matches!(map_get(m, "float"), Some(Value::Map(_)))
        }) {
            return map;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("no redraw with an open float arrived");
}

fn float_lines(float: &[(Value, Value)]) -> Vec<String> {
    float_rows(float)
        .iter()
        .map(|row| {
            row.iter()
                .filter_map(|c| c.as_array()?.first()?.as_str().map(str::to_string))
                .collect()
        })
        .collect()
}

/// The float's raw wire rows — each a chunk run `[[text, style_id], …]` (the
/// `virt_lines` form a styled which-key ships). Lets a test read per-segment
/// highlight, not just the flattened text [`float_lines`] returns.
fn float_rows(float: &[(Value, Value)]) -> Vec<Vec<Value>> {
    match map_get(float, "lines") {
        Some(Value::Array(rows)) => rows
            .iter()
            .map(|row| row.as_array().cloned().unwrap_or_default())
            .collect(),
        other => panic!("expected lines array, got {other:?}"),
    }
}

/// The `(text, style_id)` pairs of one wire row.
fn row_chunks(row: &[Value]) -> Vec<(String, Option<u64>)> {
    row.iter()
        .filter_map(|c| {
            let c = c.as_array()?;
            Some((c.first()?.as_str()?.to_string(), c.get(1)?.as_u64()))
        })
        .collect()
}

/// The shipped `examples/which-key` config sources cleanly and renders for real:
/// its full leader menu (with the `g` git group) and the *default* 200ms debounce.
/// `poll_float` waits well past 200ms, so this proves the example a user runs —
/// not just the test's inlined copy — opens the popup with all its continuations.
#[tokio::test]
async fn the_shipped_example_renders() {
    let example =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/which-key/init.lua");
    let init = std::fs::read_to_string(&example).expect("read examples/which-key/init.lua");
    let (rpc, _dir, mut incoming) = start(&init).await;

    feed(&rpc, "<Space>");
    let float = poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("the example's popup opens on <leader> after the 200ms debounce");

    // f (find/grep group), g (git group), q, w — sorted by key notation.
    assert_eq!(
        float_lines(&float),
        vec![
            " f   +more ".to_string(),
            " g   +more ".to_string(),
            " q   quit ".to_string(),
            " w   write ".to_string(),
        ]
    );
}

/// Pressing `<leader>` and pausing opens the popup, listing every continuation:
/// the single-key maps with their `desc`, and `f` as a `+`-prefixed GROUP (it only
/// leads to the deeper `ff`/`fg`). Sorted by key notation: f, q, w.
#[tokio::test]
async fn leader_opens_the_popup_with_continuations() {
    let (rpc, _dir, mut incoming) = start(WHICH_KEY).await;

    feed(&rpc, "<Space>");
    let float = poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("the which-key popup opens on <leader>");

    // The float pads every row to the box width, so each line carries a trailing
    // space out to the widest row (" f   +more " == 11 cells).
    let lines = float_lines(&float);
    assert_eq!(
        lines,
        vec![
            " f   +more ".to_string(),
            " q   quit ".to_string(),
            " w   write ".to_string(),
        ]
    );
    // The withheld prefix is drawn on the border as the title.
    assert_eq!(
        map_get(&float, "title").and_then(Value::as_str),
        Some(" <Space> ")
    );
    assert_eq!(
        map_get(&float, "border").and_then(Value::as_str),
        Some("rounded")
    );

    // Bottom-RIGHT corner: the box (content + border chrome) sits flush against both
    // the last text row and the text area's right edge. `col`/`row` are relative to
    // the focused window's text area, which on the 80×24 attach is 23 rows (one
    // statusline) × 76 cols (80 minus the 4-cell number gutter), height 3, chrome 2.
    // So the box hugs both far corners: row 18, col 63. Clearly bottom (not centered
    // ~9) and right (not centered ~32).
    let row = map_get(&float, "row").and_then(Value::as_u64).unwrap();
    let col = map_get(&float, "col").and_then(Value::as_u64).unwrap();
    let width = map_get(&float, "width").and_then(Value::as_u64).unwrap();
    let height = map_get(&float, "height").and_then(Value::as_u64).unwrap();
    assert_eq!(height, 3);
    assert_eq!(row + height + 2, 23, "box flush to the bottom text row");
    assert_eq!(
        col + width + 2,
        76,
        "box flush to the text area's right edge"
    );
    assert!(row >= 14, "bottom-anchored, not centered (row was {row})");
    assert!(col >= 40, "right-anchored, not centered (col was {col})");
}

/// Phase 4 — the "pretty" popup: each row is a styled chunk run, so the key, the
/// `+`group label, and a plain description carry DISTINCT highlight groups (which
/// resolve to the colours `lines_for` defines). Proof per-segment highlighting
/// threads all the way from the example's chunk lines to the redraw wire.
#[tokio::test]
async fn rows_colour_keys_groups_and_descriptions() {
    let (rpc, _dir, mut incoming) = start(WHICH_KEY).await;

    feed(&rpc, "<Space>");
    let map = drain_to_latest_redraw_open(&rpc, &mut incoming).await;
    let float = match map_get(&map, "float") {
        Some(Value::Map(f)) => f.clone(),
        other => panic!("expected an open float, got {other:?}"),
    };
    let styles = match map_get(&map, "styles") {
        Some(Value::Array(a)) => a.clone(),
        other => panic!("expected styles palette, got {other:?}"),
    };
    let fg = |id: u64| {
        styles[id as usize]
            .as_map()
            .and_then(|m| map_get(m, "fg"))
            .and_then(Value::as_u64)
    };

    let rows = float_rows(&float);
    // Rows are sorted f, q, w. The `f` row is a GROUP (+more); `w` is a plain map.
    let f_row = row_chunks(&rows[0]);
    let w_row = row_chunks(&rows[2]);

    // The key chunk and the description chunk are separate, distinctly-styled spans.
    let f_key = f_row.iter().find(|(t, _)| t == "f").expect("f key chunk");
    let f_label = f_row
        .iter()
        .find(|(t, _)| t == "+more")
        .expect("+more group chunk");
    let w_key = w_row.iter().find(|(t, _)| t == "w").expect("w key chunk");
    let w_label = w_row
        .iter()
        .find(|(t, _)| t == "write")
        .expect("write desc chunk");

    // Keys are WhichKey (cyan #7dcfff); a group label is WhichKeyGroup (#bb9af7);
    // a plain description is WhichKeyDesc (#c0caf5) — three distinct colours.
    assert_eq!(fg(f_key.1.expect("f key styled")), Some(0x7dcfff));
    assert_eq!(fg(w_key.1.expect("w key styled")), Some(0x7dcfff));
    assert_eq!(fg(f_label.1.expect("group styled")), Some(0xbb9af7));
    assert_eq!(fg(w_label.1.expect("desc styled")), Some(0xc0caf5));
    assert_ne!(
        f_label.1, w_label.1,
        "a +group label and a plain description are coloured differently"
    );
}

/// Phase 4 — a continuation kept visible but no longer firable (a `g`-map after the
/// leader timeout commits `g` to the built-in grammar) is DIMMED with the
/// `WhichKeyDim` group, rather than cued with a trailing `(×)`. The example no
/// longer appends any marker text — the colour carries the meaning.
#[tokio::test]
async fn timed_out_g_map_row_is_dimmed_not_text_cued() {
    let (rpc, _dir, mut incoming) = start(WHICH_KEY).await;

    feed(&rpc, "g"); // withheld `g`: LSP gd/gD/gr maps + built-in motions
    drain_to_latest_redraw_open(&rpc, &mut incoming).await;
    // The idle flush commits `g` to the built-in grammar; the maps stay listed but
    // unavailable (the which-key keeps them visible, now dimmed). After `g`, the
    // continuation key is `d` (relative to the prefix), so locate the map by its
    // description. Poll until the dimmed frame lands — the flush → debounced repaint
    // settles a tick after the available frame.
    rpc.request("nxvim_input_flush", vec![])
        .await
        .expect("input flush");

    // The fg of the "Go to definition" label chunk, when the float is open — or None.
    let go_to_def_fg = |map: &[(Value, Value)]| -> Option<u64> {
        let Some(Value::Map(float)) = map_get(map, "float") else {
            return None;
        };
        let Some(Value::Array(styles)) = map_get(float, "styles").or(map_get(map, "styles")) else {
            return None;
        };
        let id = float_rows(float)
            .iter()
            .map(|r| row_chunks(r))
            .find(|cs| cs.iter().any(|(t, _)| t == "Go to definition"))?
            .iter()
            .find(|(t, _)| t == "Go to definition")
            .and_then(|(_, id)| *id)?;
        styles
            .get(id as usize)?
            .as_map()
            .and_then(|m| map_get(m, "fg"))
            .and_then(Value::as_u64)
    };

    let mut dimmed = None;
    for _ in 0..60 {
        nxvim_test_harness::barrier(&rpc).await;
        if let Some(map) =
            drain_to_latest_redraw(&mut incoming, |m| go_to_def_fg(m) == Some(0x565f89))
        {
            dimmed = Some(map);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let map = dimmed.expect("the gd map row dims to WhichKeyDim after the timeout");
    let float = match map_get(&map, "float") {
        Some(Value::Map(f)) => f.clone(),
        other => panic!("expected an open float, got {other:?}"),
    };
    let styles = match map_get(&map, "styles") {
        Some(Value::Array(a)) => a.clone(),
        other => panic!("expected styles palette, got {other:?}"),
    };
    let dim_id = float_rows(&float)
        .iter()
        .map(|r| row_chunks(r))
        .find(|cs| cs.iter().any(|(t, _)| t == "Go to definition"))
        .and_then(|cs| {
            cs.iter()
                .find(|(t, _)| t == "Go to definition")
                .and_then(|(_, id)| *id)
        })
        .expect("the dimmed label is styled");
    let dim = styles[dim_id as usize].as_map().expect("dim style map");
    assert_eq!(
        map_get(dim, "fg").and_then(Value::as_u64),
        Some(0x565f89),
        "the unavailable map is dimmed"
    );
    assert_eq!(
        map_get(dim, "italic").and_then(Value::as_bool),
        Some(true),
        "WhichKeyDim is italic"
    );
    let joined: String = float_lines(&float).join("\n");
    assert!(
        !joined.contains("(×)"),
        "no text cue — the dim colour carries it: {joined}"
    );
}

/// Descending into a group repaints the SAME popup to that group's keys — the
/// persistent float updates in place rather than stacking a second window.
#[tokio::test]
async fn descending_into_a_group_refreshes_in_place() {
    let (rpc, _dir, mut incoming) = start(WHICH_KEY).await;

    feed(&rpc, "<Space>");
    poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("popup opens at the leader");

    feed(&rpc, "f");
    let float = poll_float(&rpc, &mut incoming, |f| {
        matches!(f, Value::Map(m) if map_get(m, "title").and_then(Value::as_str) == Some(" <Space>f "))
    })
    .await
    .expect("popup refreshes to the f-group");

    assert_eq!(
        float_lines(&float),
        vec![" f   find file ".to_string(), " g   live grep ".to_string()]
    );
}

/// Completing a mapping clears the pending context, which closes the popup — the
/// cleared event cancels any pending render and dismisses the float at once.
#[tokio::test]
async fn completing_a_mapping_closes_the_popup() {
    let (rpc, _dir, mut incoming) = start(WHICH_KEY).await;

    feed(&rpc, "<Space>");
    poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("popup opens");

    feed(&rpc, "w"); // completes <leader>w
    poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Nil))
        .await
        .expect("popup closes once the sequence completes");
}

/// Breaking the prefix with a non-continuation key also clears the context and
/// closes the popup (the withheld keys replay normally; the float just goes away).
#[tokio::test]
async fn breaking_the_prefix_closes_the_popup() {
    let (rpc, _dir, mut incoming) = start(WHICH_KEY).await;

    feed(&rpc, "<Space>");
    poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("popup opens");

    feed(&rpc, "x"); // extends no mapping
    poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Nil))
        .await
        .expect("popup closes when the prefix breaks");
}

/// Source B: pressing `f` (find-char) — an open built-in state with no key list —
/// renders the `ctx.label` as a single hint card, titled with the pending key. This
/// is the find-char swallow made visible: the popup now *tells* you it's waiting for
/// a character instead of silently eating your next key.
#[tokio::test]
async fn find_char_renders_a_label_hint_card() {
    let (rpc, _dir, mut incoming) = start(WHICH_KEY).await;

    feed(&rpc, "ihello world<Esc>0"); // a line so the find-char has somewhere to go
    feed(&rpc, "f");
    let float = poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("the find-char hint card opens on f");

    assert_eq!(float_lines(&float), vec![" Find character ".to_string()]);
    // The label now also titles the card, so the prefix isn't cryptic.
    assert_eq!(
        map_get(&float, "title").and_then(Value::as_str),
        Some(" f — Find character ")
    );

    // Typing the target char completes the motion and closes the card.
    feed(&rpc, "w");
    poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Nil))
        .await
        .expect("the hint card closes once the find completes");
}

/// Source B Phase 2: pressing `z` (a *finite* built-in prefix, no map withholds it)
/// renders the enumerated viewport commands as a real key GRID — not a single label
/// card — exactly like a mapped-prefix menu. Proof the example's `lines_for` grid
/// path drives built-in continuations too, so `z`/`g`/`<C-w>` get a key list.
#[tokio::test]
async fn z_prefix_renders_the_builtin_command_grid() {
    let (rpc, _dir, mut incoming) = start(WHICH_KEY).await;

    feed(&rpc, "z");
    let float = poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("the z-prefix grid opens");

    assert_eq!(
        map_get(&float, "title").and_then(Value::as_str),
        Some(" z — Scroll / fold ")
    );
    let lines = float_lines(&float);
    // Six view commands, one row each (zt/zz/zb + z<CR>/z./z-), rendered as a grid.
    assert_eq!(lines.len(), 6, "one row per view command: {lines:?}");
    let joined = lines.join("\n");
    assert!(joined.contains("Scroll line to center"), "zz row: {joined}");
    assert!(joined.contains("Scroll line to top"), "zt row: {joined}");
    assert!(
        joined.contains("<CR>") && joined.contains("Top, first non-blank"),
        "z<CR> row: {joined}"
    );
}

/// Source B Phase 3: pressing an operator (`d`) titles the popup `d — Delete` (no
/// cryptic bare key) and renders the operator-range motions as a grid — so the popup
/// *tells* you `d` is delete and lists what it can act on.
#[tokio::test]
async fn operator_titles_with_its_name_and_lists_motions() {
    let (rpc, _dir, mut incoming) = start(WHICH_KEY).await;

    feed(&rpc, "d");
    let float = poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("the operator-pending popup opens on d");

    assert_eq!(
        map_get(&float, "title").and_then(Value::as_str),
        Some(" d — Delete ")
    );
    // The motion list is long (it overflows the popup height on 80×24 — the example
    // is single-column), so assert on rows in the visible window (sorted by key).
    let joined = float_lines(&float).join("\n");
    assert!(
        joined.contains("to end of line"),
        "$ motion listed: {joined}"
    );
    assert!(joined.contains("current line(s)"), "dd listed: {joined}");
    assert!(
        joined.contains("to first non-blank"),
        "^ motion listed: {joined}"
    );
}

/// Source B Phase 3: selecting a register (`"a`) keeps the popup open, titled
/// `"a — Use register` so you know a register is armed, and lists the actions that
/// consume it (paste / the operators) — instead of silently closing.
#[tokio::test]
async fn selected_register_shows_armed_actions() {
    let (rpc, _dir, mut incoming) = start(WHICH_KEY).await;

    feed(&rpc, "\"a");
    let float = poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("the register-armed popup opens on \"a");

    assert_eq!(
        map_get(&float, "title").and_then(Value::as_str),
        Some(" \"a — Use register ")
    );
    let joined = float_lines(&float).join("\n");
    assert!(
        joined.contains("paste after"),
        "paste action listed: {joined}"
    );
    assert!(
        joined.contains("+delete"),
        "delete operator group: {joined}"
    );
}
