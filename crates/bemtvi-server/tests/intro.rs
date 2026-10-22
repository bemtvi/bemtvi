//! The intro screen (`btv.intro`): what an editor started with no file shows — the app
//! icon in half-block characters, the name, the version and the first commands — drawn
//! as virtual lines on the empty startup buffer, and gone for good once the editor is
//! used. See docs/plans/2026-10-21-intro-screen.md.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::*;
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// A session armed for the intro (as the interactive binaries are), with `init` as its
/// config, attached at `cols` × `rows`, with or without truecolor.
async fn start(
    tag: &str,
    init: &str,
    truecolor: bool,
    cols: u16,
    rows: u16,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    let dir = temp_dir(tag);
    std::fs::write(dir.join("init.lua"), init).expect("write init.lua");
    let (rpc, incoming) = spawn(ServerInit {
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir],
        show_intro: true,
        ..Default::default()
    });
    let caps = vec![(Value::from("truecolor"), Value::Boolean(truecolor))];
    attach_with_caps(&rpc, cols, rows, caps).await;
    (rpc, incoming)
}

/// The virtual-line rows the startup window paints, in screen order, each flattened to
/// its text: what the clients draw, read from the latest frame after a no-op key
/// (`<Esc>` in Normal mode). Empty when the intro isn't shown.
async fn intro_rows(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> Vec<String> {
    let map = redraw_after(rpc, incoming, "<Esc>").await;
    window0_field(&map, "virt_lines")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    Some(
                        r.as_array()?
                            .iter()
                            .filter_map(|c| c.as_array()?.first()?.as_str())
                            .collect::<String>(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn has_logo(rows: &[String]) -> bool {
    rows.iter().any(|r| r.contains('▀'))
}

fn row_with<'a>(rows: &'a [String], needle: &str) -> Option<&'a String> {
    rows.iter().find(|r| r.contains(needle))
}

#[tokio::test]
async fn an_editor_started_with_no_file_shows_the_intro() {
    let (rpc, mut incoming) = start("intro_shown", "", true, 100, 30).await;
    let rows = intro_rows(&rpc, &mut incoming).await;
    assert!(
        has_logo(&rows),
        "the logo is drawn in half blocks: {rows:#?}"
    );
    // The logo is the icon's 23x23 pixels: 12 rows of cells.
    let logo_rows = rows
        .iter()
        .filter(|r| r.contains('▀') || r.contains('▄'))
        .count();
    assert_eq!(logo_rows, 12, "{rows:#?}");
    assert!(row_with(&rows, "bemtvi").is_some(), "{rows:#?}");
    assert!(row_with(&rows, "version 0.1.0").is_some(), "{rows:#?}");
    assert!(row_with(&rows, ":q<Enter>").is_some(), "{rows:#?}");
    assert!(row_with(&rows, ":Plugins<Enter>").is_some(), "{rows:#?}");
    // `:help` is only advertised when the help plugin provides it.
    assert!(row_with(&rows, ":help").is_none(), "{rows:#?}");
    // The buffer itself is untouched: one empty, unmodified line.
    assert_eq!(lines(&rpc).await, vec![String::new()]);
    assert_eq!(
        exec_lua(&rpc, "return btv.bo.modified").await,
        Value::Boolean(false)
    );
}

#[tokio::test]
async fn the_intro_is_centred_in_the_text_area() {
    let (rpc, mut incoming) = start("intro_centred", "", true, 100, 30).await;
    let rows = intro_rows(&rpc, &mut incoming).await;
    let textoff = exec_lua(&rpc, "return btv.win.textoff(0)")
        .await
        .as_u64()
        .unwrap() as usize;
    let width = 100 - textoff;
    // The name is centred on its own: as much room on its left as on its right.
    let name = row_with(&rows, "bemtvi").unwrap();
    let left = name.len() - name.trim_start().len();
    let right = width - left - "bemtvi".len();
    assert!(
        left.abs_diff(right) <= 1,
        "left {left}, right {right} in {width}"
    );
    // Vertically, the box sits in the middle of the window, under line 1 (the rows
    // above it are single-space padding; the box's own rows span its width).
    let height = exec_lua(&rpc, "return btv.win.height(0)")
        .await
        .as_u64()
        .unwrap() as usize;
    let blank_above = rows.iter().take_while(|r| r.as_str() == " ").count();
    let block = rows.len() - blank_above;
    let above = 1 + blank_above;
    let below = height - above - block;
    assert!(above.abs_diff(below) <= 1, "above {above}, below {below}");
}

#[tokio::test]
async fn the_intro_redraws_centred_after_a_resize() {
    let (rpc, mut incoming) = start("intro_resize", "", true, 100, 30).await;
    let before = intro_rows(&rpc, &mut incoming).await;
    rpc.request(
        "btv_ui_try_resize",
        vec![Value::from(140u64), Value::from(40u64)],
    )
    .await
    .expect("resize");
    let after = intro_rows(&rpc, &mut incoming).await;
    let indent = |rows: &[String]| {
        let name = row_with(rows, "bemtvi").unwrap();
        name.len() - name.trim_start().len()
    };
    assert!(
        indent(&after) > indent(&before),
        "the name moved right with the wider window"
    );
    let blank = |rows: &[String]| rows.iter().take_while(|r| r.as_str() == " ").count();
    assert!(
        blank(&after) > blank(&before),
        "and down with the taller one"
    );
}

#[tokio::test]
async fn without_truecolor_only_the_text_is_shown() {
    // The logo's colours are 24-bit; a downgraded copy would not read as the icon.
    let (rpc, mut incoming) = start("intro_no_truecolor", "", false, 100, 30).await;
    let rows = intro_rows(&rpc, &mut incoming).await;
    assert!(!has_logo(&rows), "{rows:#?}");
    assert!(row_with(&rows, "bemtvi").is_some(), "{rows:#?}");
}

#[tokio::test]
async fn a_window_too_small_for_the_logo_shows_only_the_text() {
    let (rpc, mut incoming) = start("intro_small", "", true, 60, 14).await;
    let rows = intro_rows(&rpc, &mut incoming).await;
    assert!(!has_logo(&rows), "{rows:#?}");
    assert!(row_with(&rows, "bemtvi").is_some(), "{rows:#?}");
}

#[tokio::test]
async fn a_window_too_small_for_the_text_shows_nothing() {
    let (rpc, mut incoming) = start("intro_tiny", "", true, 30, 8).await;
    assert!(intro_rows(&rpc, &mut incoming).await.is_empty());
}

#[tokio::test]
async fn the_config_can_opt_out() {
    let (rpc, mut incoming) = start("intro_opt_out", "btv.g.intro = false\n", true, 100, 30).await;
    assert!(intro_rows(&rpc, &mut incoming).await.is_empty());
}

#[tokio::test]
async fn a_config_that_opens_a_file_gets_no_intro() {
    let dir = temp_dir("intro_config_file");
    let file = dir.join("notes.txt");
    std::fs::write(&file, "hello\n").unwrap();
    let init = format!("vim.cmd.edit({:?})\n", file.to_str().unwrap());
    let (rpc, mut incoming) = start("intro_config_opens", &init, true, 100, 30).await;
    assert_eq!(lines(&rpc).await, vec!["hello".to_string()]);
    assert!(intro_rows(&rpc, &mut incoming).await.is_empty());
}

#[tokio::test]
async fn sessions_not_armed_for_it_never_show_it() {
    // Headless runs and the test suites leave `show_intro` off.
    let dir = temp_dir("intro_unarmed");
    let (rpc, mut incoming) = spawn(ServerInit {
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir],
        ..Default::default()
    });
    attach_truecolor(&rpc, 100, 30).await;
    assert!(intro_rows(&rpc, &mut incoming).await.is_empty());
}

#[tokio::test]
async fn typing_into_the_buffer_dismisses_it() {
    let (rpc, mut incoming) = start("intro_typing", "", true, 100, 30).await;
    assert!(!intro_rows(&rpc, &mut incoming).await.is_empty());
    feed(&rpc, "ihello<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello".to_string()]);
    assert!(intro_rows(&rpc, &mut incoming).await.is_empty());
    // For good: undoing back to an empty buffer doesn't bring it back.
    feed(&rpc, "u");
    assert_eq!(lines(&rpc).await, vec![String::new()]);
    assert!(intro_rows(&rpc, &mut incoming).await.is_empty());
}

#[tokio::test]
async fn opening_a_file_dismisses_it() {
    // `:e file` on the untouched startup buffer reuses it (as vim does) rather than
    // leaving it, so the load itself must clear the intro.
    let dir = temp_dir("intro_edit_file");
    let file = dir.join("notes.txt");
    std::fs::write(&file, "hello\n").unwrap();
    let (rpc, mut incoming) = start("intro_edit", "", true, 100, 30).await;
    assert!(!intro_rows(&rpc, &mut incoming).await.is_empty());
    feed(&rpc, &format!(":e {}<CR>", file.display()));
    assert_eq!(
        await_lines(&rpc, &["hello"]).await,
        vec!["hello".to_string()]
    );
    assert!(intro_rows(&rpc, &mut incoming).await.is_empty());
}

#[tokio::test]
async fn splitting_the_window_dismisses_it() {
    let (rpc, mut incoming) = start("intro_split", "", true, 100, 30).await;
    assert!(!intro_rows(&rpc, &mut incoming).await.is_empty());
    feed(&rpc, ":split<CR>");
    assert!(intro_rows(&rpc, &mut incoming).await.is_empty());
}

#[tokio::test]
async fn a_float_over_it_leaves_it_alone() {
    // Popups (which-key, completion, pickers, the first-run offer) are floats showing
    // their own buffers: the intro stays behind them.
    let (rpc, mut incoming) = start("intro_float", "", true, 100, 30).await;
    let before = intro_rows(&rpc, &mut incoming).await;
    exec_lua(
        &rpc,
        "btv.ui.float({ 'a popup' }, { border = 'rounded' })\n\
         return 1",
    )
    .await;
    assert_eq!(intro_rows(&rpc, &mut incoming).await, before);
}

#[tokio::test]
async fn a_colorscheme_that_clears_highlights_keeps_the_logo_coloured() {
    // A scheme opening with `hi clear` drops every group, the logo's included; the
    // intro draws them again on `ColorScheme`.
    let dir = temp_dir("intro_colorscheme");
    std::fs::create_dir_all(dir.join("colors")).unwrap();
    std::fs::write(
        dir.join("colors").join("plain.lua"),
        "vim.cmd('hi clear')\nvim.api.nvim_set_hl(0, 'Normal', { fg = '#ffffff', bg = '#000000' })\n",
    )
    .unwrap();
    let (rpc, mut incoming) = spawn(ServerInit {
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir],
        show_intro: true,
        ..Default::default()
    });
    attach_truecolor(&rpc, 100, 30).await;
    let before = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    let (n, coloured) = logo_chunks_coloured(&before);
    assert!(
        n > 0 && coloured == n,
        "{coloured} of {n} logo chunks painted in colour"
    );
    let after = redraw_after(&rpc, &mut incoming, ":colorscheme plain<CR>").await;
    let (n, coloured) = logo_chunks_coloured(&after);
    assert!(
        n > 0 && coloured == n,
        "after hi clear: {coloured} of {n} logo chunks painted in colour"
    );
}

/// How many of the frame's logo chunks (the half-block runs) there are, and how many
/// are painted with a foreground colour, read through the frame's style palette.
fn logo_chunks_coloured(map: &[(Value, Value)]) -> (usize, usize) {
    let styles = map_get(map, "styles")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut n = 0;
    let mut coloured = 0;
    let rows = window0_field(map, "virt_lines")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for row in rows {
        for chunk in row.as_array().cloned().unwrap_or_default() {
            let Some(parts) = chunk.as_array() else {
                continue;
            };
            let text = parts.first().and_then(Value::as_str).unwrap_or_default();
            if !text.contains('▀') && !text.contains('▄') {
                continue;
            }
            n += 1;
            let style = parts
                .get(1)
                .and_then(Value::as_u64)
                .and_then(|id| styles.get(id as usize));
            let has_fg = matches!(style, Some(Value::Map(m)) if m.iter().any(|(k, v)| k.as_str() == Some("fg") && v.as_u64().is_some()));
            if has_fg {
                coloured += 1;
            }
        }
    }
    (n, coloured)
}

#[tokio::test]
async fn the_intro_is_a_solid_box_in_the_editor_background() {
    // Nothing under the intro shows through it ('colorcolumn' rulers, say): every row
    // of the box spans the same width, and every cell of it is painted with the
    // editor's background.
    let (rpc, mut incoming) = start(
        "intro_box",
        "vim.opt.colorcolumn = '50,60'\n",
        true,
        100,
        30,
    )
    .await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    let normal_bg = exec_lua(&rpc, "return btv.hl.get(0, { name = 'Normal' }).bg")
        .await
        .as_u64()
        .expect("the default scheme sets a Normal background");
    let styles = map_get(&map, "styles")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let rows = window0_field(&map, "virt_lines")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut widths = std::collections::BTreeSet::new();
    let mut box_rows = 0;
    for row in rows.iter().filter_map(Value::as_array) {
        let chunks: Vec<&Vec<Value>> = row.iter().filter_map(Value::as_array).collect();
        if chunks.len() < 2 {
            continue; // a padding row above the box
        }
        box_rows += 1;
        // The first chunk is the box's indent; every chunk after it is in the box.
        let mut width = 0;
        for chunk in &chunks[1..] {
            let text = chunk[0].as_str().unwrap_or_default();
            width += text.chars().count();
            let bg = chunk
                .get(1)
                .and_then(Value::as_u64)
                .and_then(|id| styles.get(id as usize))
                .and_then(|s| match s {
                    Value::Map(m) => m
                        .iter()
                        .find(|(k, _)| k.as_str() == Some("bg"))
                        .and_then(|(_, v)| v.as_u64()),
                    _ => None,
                });
            assert!(bg.is_some(), "an unpainted cell in the box: {text:?}");
            if !text.contains('▀') && !text.contains('▄') {
                assert_eq!(
                    bg,
                    Some(normal_bg),
                    "box cells take the editor background: {text:?}"
                );
            }
        }
        widths.insert(width);
    }
    assert!(box_rows > 20, "{box_rows} box rows");
    assert_eq!(
        widths.len(),
        1,
        "every row of the box is as wide: {widths:?}"
    );
}

/// A session not armed for the startup intro, as `:intro` finds it in a test or after
/// the startup one was dismissed.
async fn start_unarmed(tag: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let dir = temp_dir(tag);
    let (rpc, incoming) = spawn(ServerInit {
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir],
        ..Default::default()
    });
    attach_truecolor(&rpc, 100, 30).await;
    (rpc, incoming)
}

#[tokio::test]
async fn intro_shows_it_on_an_empty_buffer() {
    // vim's `:intro`: the screen on demand, whether or not the session started with it.
    let (rpc, mut incoming) = start_unarmed("intro_cmd").await;
    assert!(intro_rows(&rpc, &mut incoming).await.is_empty());
    feed(&rpc, ":intro<CR>");
    let rows = intro_rows(&rpc, &mut incoming).await;
    assert!(has_logo(&rows), "{rows:#?}");
    assert!(row_with(&rows, ":q<Enter>").is_some(), "{rows:#?}");
    // It goes the way the startup one does.
    feed(&rpc, "ihello<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello".to_string()]);
    assert!(intro_rows(&rpc, &mut incoming).await.is_empty());
}

#[tokio::test]
async fn intro_shows_it_even_when_the_config_opted_out_of_the_startup_one() {
    let (rpc, mut incoming) = start("intro_cmd_optout", "btv.g.intro = false", true, 100, 30).await;
    assert!(intro_rows(&rpc, &mut incoming).await.is_empty());
    feed(&rpc, ":intro<CR>");
    assert!(has_logo(&intro_rows(&rpc, &mut incoming).await));
}

#[tokio::test]
async fn intro_on_a_buffer_with_text_fails_loud() {
    let (rpc, mut incoming) = start_unarmed("intro_cmd_text").await;
    feed(&rpc, "ihello<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello".to_string()]);
    let map = redraw_after(&rpc, &mut incoming, ":intro<CR>").await;
    assert!(
        message(&map).contains(":enew"),
        "says why and what to do: {:?}",
        message(&map)
    );
    assert!(intro_rows(&rpc, &mut incoming).await.is_empty());
}

#[tokio::test]
async fn intro_twice_draws_it_once() {
    let (rpc, mut incoming) = start("intro_cmd_twice", "", true, 100, 30).await;
    let once = intro_rows(&rpc, &mut incoming).await;
    feed(&rpc, ":intro<CR>");
    assert_eq!(intro_rows(&rpc, &mut incoming).await, once);
    // And one dismissal still clears it: no second set of subscriptions left behind.
    feed(&rpc, "ix<Esc>");
    assert!(intro_rows(&rpc, &mut incoming).await.is_empty());
}
