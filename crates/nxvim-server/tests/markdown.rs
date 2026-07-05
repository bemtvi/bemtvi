//! Behavior tests for the markdown renderer behind `nx.markdown.render` — the pure
//! CommonMark+GFM → stripped-lines + `@markup.*` highlights transform in
//! `nxvim_core::markdown`, exposed to Lua. Black-box per the project conventions: a
//! real server over RPC, driven with `nvim_exec_lua`, asserting on the rendered
//! lines and highlight spans it returns.
//!
//! Each test runs a Lua chunk that renders a markdown string and `return`s a small
//! assertable projection of the result (the lines joined, or one highlight encoded
//! as a string), so the assertion doesn't have to walk a nested msgpack map.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, barrier, drain_to_latest_redraw, exec_lua, feed, map_get, spawn, start_attached,
    temp_dir,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

async fn lua_string(rpc: &Rpc, code: &str) -> Option<String> {
    exec_lua(rpc, code).await.as_str().map(str::to_owned)
}

/// The rendered lines joined with `|`, so a test asserts the whole stripped block
/// in one string.
async fn render_lines(rpc: &Rpc, src: &str) -> String {
    let code = format!("local r = nx.markdown.render({src:?})\nreturn table.concat(r.lines, '|')");
    lua_string(rpc, &code).await.unwrap_or_default()
}

/// The first highlight whose group is `group`, encoded `line,col_start,col_end` (or
/// `""` when none) — over the line-joined text so a test can pin a span's position.
async fn first_span(rpc: &Rpc, src: &str, group: &str) -> String {
    let code = format!(
        "local r = nx.markdown.render({src:?})\n\
         for _, h in ipairs(r.highlights) do\n\
           if h.group == {group:?} then\n\
             return string.format('%d,%d,%d', h.line, h.col_start, h.col_end)\n\
           end\n\
         end\n\
         return ''"
    );
    lua_string(rpc, &code).await.unwrap_or_default()
}

/// The fills encoded as `line:char:group` joined with `|` — one per thematic break /
/// table separator, so a test can assert a rule was emitted at the right line.
async fn fills(rpc: &Rpc, src: &str) -> String {
    let code = format!(
        "local r = nx.markdown.render({src:?})\n\
         local out = {{}}\n\
         for _, f in ipairs(r.fills) do\n\
           out[#out + 1] = string.format('%d:%s:%s', f.line, f.char, f.group)\n\
         end\n\
         return table.concat(out, '|')"
    );
    lua_string(rpc, &code).await.unwrap_or_default()
}

#[tokio::test]
async fn strong_emphasis_and_inline_code_lose_their_markers() {
    let (rpc, _incoming) = start().await;
    // The markup characters are gone; the visible text remains.
    assert_eq!(
        render_lines(&rpc, "some **bold** and *italic* and `code` here").await,
        "some bold and italic and code here"
    );
}

#[tokio::test]
async fn strong_span_covers_the_word_not_the_asterisks() {
    let (rpc, _incoming) = start().await;
    // "some bold and ..." → "bold" is chars 6..9 inclusive, exclusive end 10.
    assert_eq!(
        first_span(&rpc, "some **bold** and more", "@markup.strong").await,
        "1,6,10"
    );
}

#[tokio::test]
async fn heading_drops_the_hashes_and_is_tagged() {
    let (rpc, _incoming) = start().await;
    assert_eq!(render_lines(&rpc, "# Title").await, "Title");
    assert_eq!(
        first_span(&rpc, "# Title", "@markup.heading.1").await,
        "1,1,6"
    );
    // Level tracks the number of hashes.
    assert_eq!(
        first_span(&rpc, "### Deep", "@markup.heading.3").await,
        "1,1,5"
    );
}

#[tokio::test]
async fn heading_and_paragraph_are_separated_by_a_blank_line() {
    let (rpc, _incoming) = start().await;
    assert_eq!(
        render_lines(&rpc, "# Title\n\nbody text").await,
        "Title||body text"
    );
}

#[tokio::test]
async fn soft_line_breaks_within_a_paragraph_collapse_to_a_space() {
    let (rpc, _incoming) = start().await;
    // A single newline inside a paragraph is a *soft* break: markdown reflows it to a
    // space (so a consumer can re-wrap), it does NOT keep the source line break.
    assert_eq!(
        render_lines(&rpc, "one two\nthree four\nfive").await,
        "one two three four five"
    );
    // A blank line still separates paragraphs, and a hard break (two trailing spaces)
    // still forces a line break.
    assert_eq!(
        render_lines(&rpc, "para one\n\npara two").await,
        "para one||para two"
    );
    assert_eq!(render_lines(&rpc, "hard  \nbreak").await, "hard|break");
}

#[tokio::test]
async fn bullet_list_gets_a_bullet_glyph_not_a_dash() {
    let (rpc, _incoming) = start().await;
    assert_eq!(render_lines(&rpc, "- one\n- two").await, "• one|• two");
    // The bullet marker itself is tagged as list punctuation.
    assert_eq!(first_span(&rpc, "- one", "@markup.list").await, "1,1,3");
}

#[tokio::test]
async fn ordered_list_keeps_its_numbers() {
    let (rpc, _incoming) = start().await;
    assert_eq!(
        render_lines(&rpc, "1. first\n2. second").await,
        "1. first|2. second"
    );
}

#[tokio::test]
async fn fenced_code_block_drops_the_fences_and_keeps_the_body() {
    let (rpc, _incoming) = start().await;
    assert_eq!(
        render_lines(&rpc, "```rust\nlet x = 1;\nlet y = 2;\n```").await,
        "let x = 1;|let y = 2;"
    );
    // (The block's language reaches the caller through the renderer's `code` list;
    // that the hover float actually syntax-colors the fence is covered in the float
    // tests — the Lua surface intentionally exposes only lines + highlights.)
}

#[tokio::test]
async fn link_shows_its_label_then_the_url() {
    let (rpc, _incoming) = start().await;
    // Label kept; the differing URL is appended in parentheses.
    assert_eq!(
        render_lines(&rpc, "see [the docs](https://example.com)").await,
        "see the docs (https://example.com)"
    );
    assert_eq!(
        first_span(
            &rpc,
            "see [the docs](https://example.com)",
            "@markup.link.label"
        )
        .await,
        "1,5,13"
    );
}

#[tokio::test]
async fn unicode_columns_are_char_not_byte() {
    let (rpc, _incoming) = start().await;
    // "café **bold**" — the é is one char but two bytes; a byte-based span would
    // report the wrong column. "café " is 5 chars, so bold is 6..10.
    assert_eq!(
        first_span(&rpc, "café **bold**", "@markup.strong").await,
        "1,6,10"
    );
}

#[tokio::test]
async fn plain_text_is_returned_unchanged() {
    let (rpc, _incoming) = start().await;
    assert_eq!(
        render_lines(&rpc, "just a sentence.").await,
        "just a sentence."
    );
}

// ----- phase 4: block quotes, task lists, tables, rules ----------------------

#[tokio::test]
async fn block_quote_gets_a_styled_bar() {
    let (rpc, _incoming) = start().await;
    assert_eq!(render_lines(&rpc, "> quoted text").await, "▎ quoted text");
    // The bar is tagged @markup.quote (chars 1..3 — "▎ ").
    assert_eq!(
        first_span(&rpc, "> quoted text", "@markup.quote").await,
        "1,1,3"
    );
}

#[tokio::test]
async fn task_list_renders_checkboxes_not_bullets() {
    let (rpc, _incoming) = start().await;
    // A checkbox replaces the bullet — no "•" and no literal "[ ]".
    assert_eq!(
        render_lines(&rpc, "- [ ] todo\n- [x] done").await,
        "☐ todo|☑ done"
    );
}

#[tokio::test]
async fn gfm_table_is_column_aligned() {
    let (rpc, _incoming) = start().await;
    // Column B is 2 wide ("22"), so the header "B" pads to keep the column aligned
    // between the header and body rows; the header/body separator is a fill line.
    let lines = render_lines(&rpc, "| A | B |\n|---|---|\n| 1 | 22 |").await;
    let rows: Vec<&str> = lines.split('|').collect();
    assert_eq!(rows[0], "A  B", "header aligned, got {lines:?}");
    assert_eq!(rows[2], "1  22", "body aligned, got {lines:?}");
    // The middle row is the header separator, emitted as a fill.
    assert_eq!(
        fills(&rpc, "| A | B |\n|---|---|\n| 1 | 22 |").await,
        "2:─:@markup.quote"
    );
}

#[tokio::test]
async fn thematic_break_emits_a_rule_fill() {
    let (rpc, _incoming) = start().await;
    // "above", a block-gap blank (line 2), then the rule fill (line 3), then "below".
    assert_eq!(
        fills(&rpc, "above\n\n---\n\nbelow").await,
        "3:─:@punctuation.special"
    );
}

// ----- shipped example -------------------------------------------------------

/// A content-float's rendered text rows (each row is a run of `[text, style]` chunks).
fn content_float_lines(map: &[(Value, Value)]) -> Vec<String> {
    let Some(Value::Map(float)) = map_get(map, "float") else {
        return Vec::new();
    };
    let Some(Value::Array(rows)) = map_get(float, "lines") else {
        return Vec::new();
    };
    rows.iter()
        .map(|row| match row {
            Value::Array(chunks) => chunks
                .iter()
                .filter_map(|c| c.as_array()?.first()?.as_str())
                .collect::<String>(),
            _ => String::new(),
        })
        .collect()
}

/// The shipped `examples/markdown/` config must load and render its sample buffer:
/// pressing `K` runs the config's `nx.markdown.render` → `nx.ui.float` glue and opens
/// a popup whose content is the *stripped* markdown — the "verified end-to-end"
/// example convention.
#[tokio::test]
async fn shipped_example_renders_the_buffer_into_a_float() {
    let example_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/markdown");
    let sample = include_str!("../../../examples/markdown/sample.md");
    let dir = temp_dir("md_example");
    let path = dir.join("sample.md");
    std::fs::write(&path, sample).expect("write sample");

    let init = ServerInit {
        config_dir: Some(example_dir),
        file: Some(path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, mut incoming) = spawn(init);
    attach(&rpc, 100, 40).await;

    // The config maps `K` to render the current buffer into a popup float.
    feed(&rpc, "K");
    barrier(&rpc).await;
    let map = drain_to_latest_redraw(&mut incoming, |m| !content_float_lines(m).is_empty())
        .expect("the example's markdown popup opens");
    let lines = content_float_lines(&map);

    assert!(
        lines.iter().any(|l| l.contains("Markdown rendering")),
        "the heading renders (stripped of '#'): {lines:?}"
    );
    assert!(
        lines
            .iter()
            .all(|l| !l.contains("**") && !l.contains("```")),
        "no raw markdown markers in the popup: {lines:?}"
    );
}
