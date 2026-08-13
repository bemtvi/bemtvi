//! Every string the server puts on the wire for a client to *paint* is
//! display-scrubbed: an unprintable control char becomes its vim `^X` / `<xx>`
//! token, exactly as buffer text already was.
//!
//! Buffer lines went through `unicode::display_line` from the start; the rest of
//! the frame — file names, statusline segments, tab labels, virtual text,
//! completion rows, float titles — did not. Those strings are not all the user's
//! own typing: a file name comes from the filesystem, virtual text and completion
//! rows come from plugins and language servers.
//!
//! Two things follow from scrubbing, and both are asserted here:
//!
//! 1. **Fidelity.** The bundled TUI renders through ratatui, which silently drops
//!    any grapheme containing a control char — so before this, a control byte in a
//!    file name simply *vanished* from the statusline while the same byte in the
//!    buffer showed as `^A`. One display, two answers. (Note what this is not: the
//!    TUI's filter means an ESC in these payloads was never going to reach the
//!    terminal as an escape sequence *through this client*. Scrubbing at the
//!    source is what makes that a property of the protocol rather than of one
//!    client's rendering library.)
//! 2. **Offsets.** Payloads that pair a string with char offsets into it — the
//!    completion rows and their matched-character spans — must translate those
//!    offsets through the same substitution, or the client bolds the wrong
//!    characters. `^A` is two chars where the original was one.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    attach, exec_lua, feed, map_get, menu_items, menu_of, poll_menu, redraw_after, spawn,
    start_attached, start_with_file, temp_dir, wait_redraw, window0_field,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// A `\x01` — the control char every fixture here smuggles. `display_line`
/// renders it `^A`.
const CTRL_A: char = '\u{01}';

/// A file name is filesystem input, and it lands in the statusline, the tabline
/// and E-messages. A control byte in one used to reach the client verbatim (and
/// then vanish); it now arrives as `^A`.
#[tokio::test]
async fn a_control_char_in_a_file_name_reaches_the_client_as_its_caret_token() {
    let dir = temp_dir("display_scrub_name");
    let file = dir.join(format!("a{CTRL_A}b.txt"));
    std::fs::write(&file, "x\n").expect("write the oddly-named file");

    let init = ServerInit {
        file: Some(file.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, mut incoming) = start_attached(init, 80, 24).await;
    let map = redraw_after(&rpc, &mut incoming, "").await;

    let name = window0_field(&map, "file_name")
        .and_then(Value::as_str)
        .expect("the window carries a file name")
        .to_string();
    assert!(
        name.ends_with("a^Ab.txt"),
        "the control char should arrive as its `^A` token, got {name:?}"
    );
    assert!(
        !name.contains(CTRL_A),
        "no raw control char may reach a client that paints this verbatim: {name:?}"
    );
}

/// Virtual text is *plugin* input — `btv.buf.set_extmark{ virt_text = … }` takes
/// whatever string the plugin hands it, including one built from an LSP
/// diagnostic message.
#[tokio::test]
async fn a_control_char_in_virtual_text_is_scrubbed() {
    let text: String = (0..5).map(|i| format!("line {i}\n")).collect();
    let (rpc, mut incoming) = start_with_file(&text).await;
    exec_lua(
        &rpc,
        &format!(
            "local ns = btv.ns.create('scrub')
             btv.buf.set_extmark(0, ns, 0, 0, {{ virt_text = {{ {{ 'x{CTRL_A}y', 'Comment' }} }} }})"
        ),
    )
    .await;
    let _ = exec_lua(&rpc, "return 1").await;
    let map = wait_redraw(&mut incoming, |m| {
        window0_field(m, "virt_text").is_some_and(|v| {
            v.as_array().is_some_and(|rows| {
                rows.iter()
                    .any(|r| !r.is_nil() && r.as_array().is_some_and(|c| !c.is_empty()))
            })
        })
    })
    .await;

    // The placement's chunks sit a few levels down (row → placements →
    // `[col, …, chunks]`), so collect every string in the subtree rather than
    // hard-coding a shape this test does not otherwise care about.
    let mut texts = Vec::new();
    collect_strings(
        window0_field(&map, "virt_text").expect("virt_text"),
        &mut texts,
    );
    assert!(
        texts.iter().any(|c| c.contains("x^Ay")),
        "the chunk text should be scrubbed, got {texts:?}"
    );
    assert!(
        !texts.iter().any(|c| c.contains(CTRL_A)),
        "no raw control char may reach the client: {texts:?}"
    );
}

/// Every string anywhere in `value`, in order.
fn collect_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.as_str().unwrap_or_default().to_owned()),
        Value::Array(items) => items.iter().for_each(|v| collect_strings(v, out)),
        Value::Map(pairs) => pairs.iter().for_each(|(_, v)| collect_strings(v, out)),
        _ => {}
    }
}

async fn start_with_source(
    dir: &std::path::Path,
    init_lua: &str,
) -> (Rpc, UnboundedReceiver<Incoming>) {
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

/// A completion row is server- or plugin-supplied text (an LSP label, a snippet
/// name, a path), and it is the one payload that ships *offsets into itself*: the
/// matched-character spans the client bolds. So it is where scrubbing and offset
/// translation have to agree.
///
/// The source below pushes `<0x01>hello` for the typed prefix `he`, putting the
/// control char **before** the matched characters — which is what makes this a
/// test of the translation rather than of the scrub alone. In the raw label the
/// match `he` is chars 1..3; in the scrubbed `^Ahello` it is 2..4, because `^A` is
/// two chars where the original was one. Ship the raw offsets alongside the
/// scrubbed string and the client bolds `Ah` instead of `he` — one character off,
/// silently, for every control char earlier in the row.
#[tokio::test]
async fn a_completion_rows_match_spans_are_translated_through_the_scrub() {
    let dir = temp_dir("display_scrub_complete");
    let init = "btv.complete.source {\n\
           name = 'ctrl', debounce = 0,\n\
           complete = function(ctx)\n\
             if ctx.prefix ~= '' then ctx.push('\\1' .. ctx.prefix .. 'llo') end\n\
           end,\n\
         }\n\
         btv.complete.setup { sources = { { 'ctrl' } } }";
    let (rpc, mut incoming) = start_with_source(&dir, init).await;

    feed(&rpc, "ihe");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    let items = menu_items(&menu);
    let row = items
        .iter()
        .find(|i| i.contains("hello"))
        .unwrap_or_else(|| panic!("the pushed row should be there, got {items:?}"));
    assert_eq!(
        row, "^Ahello",
        "the row should carry the scrubbed label, got {items:?}"
    );
    assert!(
        !items.iter().any(|i| i.contains(CTRL_A)),
        "no raw control char may reach the client: {items:?}"
    );

    let spans = map_get(&menu, "match_spans")
        .and_then(Value::as_array)
        .expect("match_spans")
        .iter()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(Value::as_array)
        .map(|r| {
            (
                r.first().and_then(Value::as_u64).unwrap_or(0) as usize,
                r.get(1).and_then(Value::as_u64).unwrap_or(0) as usize,
            )
        })
        .collect::<Vec<_>>();

    // Whatever the matcher chose to highlight, every span must index the string
    // the client was actually given — and the characters under it must be the
    // ones the user typed.
    let chars: Vec<char> = row.chars().collect();
    assert!(!spans.is_empty(), "the typed prefix should be highlighted");
    let highlighted: String = spans
        .iter()
        .flat_map(|(start, end)| {
            assert!(
                start <= end && *end <= chars.len(),
                "span {start}..{end} is outside the {}-char row it indexes",
                chars.len()
            );
            chars[*start..*end].iter().copied()
        })
        .collect();
    assert_eq!(
        highlighted,
        "he",
        "the spans must cover the typed characters in the SCRUBBED row (raw \
         offsets would land on {:?})",
        chars[1..3].iter().collect::<String>()
    );
}

/// The scrub must not touch what it isn't for: tabs, and every printable char,
/// pass through unchanged. (`display_line` deliberately leaves `\t` alone —
/// expansion is the renderer's job — so a scrub that "cleaned" it would silently
/// change every indented payload.)
#[tokio::test]
async fn printables_and_tabs_pass_through_untouched() {
    let dir = temp_dir("display_scrub_passthrough");
    let file = dir.join("plain\tname é.txt");
    std::fs::write(&file, "x\n").expect("write");

    let init = ServerInit {
        file: Some(file.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, mut incoming) = start_attached(init, 80, 24).await;
    let map = redraw_after(&rpc, &mut incoming, "").await;

    let name = window0_field(&map, "file_name")
        .and_then(Value::as_str)
        .expect("file name")
        .to_string();
    assert!(
        name.ends_with("plain\tname é.txt"),
        "a tab and a non-ASCII printable must survive verbatim, got {name:?}"
    );
}
