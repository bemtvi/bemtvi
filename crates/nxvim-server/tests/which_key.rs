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
const WHICH_KEY: &str = r#"
vim.g.mapleader = " "
vim.g.which_key_delay = 0

nx.keymap.set("n", "<leader>w", function() end, { desc = "write" })
nx.keymap.set("n", "<leader>q", function() end, { desc = "quit" })
nx.keymap.set("n", "<leader>ff", function() end, { desc = "find file" })
nx.keymap.set("n", "<leader>fg", function() end, { desc = "live grep" })

local DELAY = vim.g.which_key_delay or 200

local function lines_for(ctx)
  if #ctx.continuations == 0 then
    return { string.format(" %s ", ctx.label or "…") }
  end
  local keyw = 1
  for _, c in ipairs(ctx.continuations) do
    keyw = math.max(keyw, vim.fn.strdisplaywidth(c.key))
  end
  local rows = {}
  for _, c in ipairs(ctx.continuations) do
    local pad = string.rep(" ", keyw - vim.fn.strdisplaywidth(c.key))
    local label
    if c.kind == "group" then
      label = "+" .. (c.desc ~= "" and c.desc or "more")
    else
      label = c.desc ~= "" and c.desc or ""
    end
    rows[#rows + 1] = string.format(" %s%s   %s ", c.key, pad, label)
  end
  return rows
end

local popup

local open = nx.utils.debounce(function(ctx)
  local lines = lines_for(ctx)
  local title = " " .. ctx.keys .. " "
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
"#;

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

fn float_lines(float: &[(Value, Value)]) -> Vec<String> {
    match map_get(float, "lines") {
        Some(Value::Array(a)) => a
            .iter()
            .map(|l| l.as_str().unwrap_or("").to_string())
            .collect(),
        other => panic!("expected lines array, got {other:?}"),
    }
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
    assert_eq!(
        map_get(&float, "title").and_then(Value::as_str),
        Some(" f ")
    );

    // Typing the target char completes the motion and closes the card.
    feed(&rpc, "w");
    poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Nil))
        .await
        .expect("the hint card closes once the find completes");
}
