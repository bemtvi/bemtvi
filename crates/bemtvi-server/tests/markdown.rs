//! Behavior tests for the markdown renderer behind `btv.markdown.render` — the pure
//! CommonMark+GFM → stripped-lines + `@markup.*` highlights transform in
//! `bemtvi_core::markdown`, exposed to Lua. Black-box per the project conventions: a
//! real server over RPC, driven with `nvim_exec_lua`, asserting on the rendered
//! lines and highlight spans it returns.
//!
//! Each test runs a Lua chunk that renders a markdown string and `return`s a small
//! assertable projection of the result (the lines joined, or one highlight encoded
//! as a string), so the assertion doesn't have to walk a nested msgpack map.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
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
    let code = format!("local r = btv.markdown.render({src:?})\nreturn table.concat(r.lines, '|')");
    lua_string(rpc, &code).await.unwrap_or_default()
}

/// The first highlight whose group is `group`, encoded `line,col_start,col_end` (or
/// `""` when none) — over the line-joined text so a test can pin a span's position.
async fn first_span(rpc: &Rpc, src: &str, group: &str) -> String {
    let code = format!(
        "local r = btv.markdown.render({src:?})\n\
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
        "local r = btv.markdown.render({src:?})\n\
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

/// The first fenced code block as `first_line,last_line,lang` (1-based, inclusive; empty
/// `lang` for a bare fence), or `""` when the source has none.
async fn first_code(rpc: &Rpc, src: &str) -> String {
    let code = format!(
        "local r = btv.markdown.render({src:?})\n\
         local c = r.code[1]\n\
         if not c then return '' end\n\
         return string.format('%d,%d,%s', c.first_line, c.last_line, c.lang or '')"
    );
    lua_string(rpc, &code).await.unwrap_or_default()
}

#[tokio::test]
async fn render_reports_fenced_code_blocks_with_bounds_and_language() {
    let (rpc, _incoming) = start().await;
    // A `lua` fence: two body lines (the fences are stripped), so the block spans the
    // 1-based inclusive rows 1..2, tagged `lua`.
    assert_eq!(
        first_code(&rpc, "```lua\nlocal x = 1\nprint(x)\n```").await,
        "1,2,lua"
    );
    // A bare fence keeps its bounds but reports no language (empty).
    assert_eq!(first_code(&rpc, "```\nmake build\n```").await, "1,1,");
    // Blocks are line-tracked against the stripped output: a leading paragraph pushes the
    // block down (a blank line separates them), so a one-line fence lands on line 3.
    assert_eq!(first_code(&rpc, "intro\n\n```\ncode\n```").await, "3,3,");
}

/// A compact projection of `btv.markdown.to_view(src)`: `first=<lines[1]>|fence=<bool>|
/// linehl=<bool>|overlay=<bool>|heading=<bool>` — enough to assert the view-ready assembly
/// (prose stripped/styled, code fence kept + backed + hidden).
async fn to_view_summary(rpc: &Rpc, src: &str) -> String {
    let code = format!(
        "local v = btv.markdown.to_view({src:?})\n\
         local has = function(p)\n\
           for _, m in ipairs(v.decor) do if p(m) then return true end end\n\
           return false\n\
         end\n\
         local fence = false\n\
         for _, l in ipairs(v.lines) do if l:match('^```rust') then fence = true end end\n\
         return table.concat({{\n\
           'first=' .. (v.lines[1] or ''),\n\
           'fence=' .. tostring(fence),\n\
           'linehl=' .. tostring(has(function(m) return m.line_hl_group == '@markup.raw.block' end)),\n\
           'overlay=' .. tostring(has(function(m) return m.virt_text_pos == 'overlay' end)),\n\
           'heading=' .. tostring(has(function(m) return m.hl_group == '@markup.heading.1' end)),\n\
         }}, '|')"
    );
    lua_string(rpc, &code).await.unwrap_or_default()
}

#[tokio::test]
async fn to_view_assembles_stripped_prose_and_kept_hidden_code_fences() {
    let (rpc, _incoming) = start().await;
    let s = to_view_summary(&rpc, "# Title\n\nsome **bold**\n\n```rust\nfn z() {}\n```").await;
    // Prose is rendered: the heading line is stripped to "Title" and carries a styled
    // `@markup.heading.1` span.
    assert!(s.contains("first=Title"), "heading stripped: {s}");
    assert!(s.contains("heading=true"), "heading styled: {s}");
    // The code block is kept as a raw ```rust fence (so tree-sitter injection can fire),
    // backed with an `@markup.raw.block` line background, and its fence lines are hidden
    // behind a blanking overlay.
    assert!(s.contains("fence=true"), "code fence kept: {s}");
    assert!(s.contains("linehl=true"), "code block backed: {s}");
    assert!(s.contains("overlay=true"), "fence hidden by overlay: {s}");
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
    // "above", then the rule's own (empty, fill-covered) line 2, then "below".
    assert_eq!(
        fills(&rpc, "above\n\n---\n\nbelow").await,
        "2:─:@punctuation.special"
    );
}

/// A rule is **tight**: no blank row above or below it. The rule already separates the
/// blocks it sits between, and a hover float is small — a server that heads its docs
/// with `<signature>\n---\n<prose>` would otherwise spend three rows on one boundary.
/// (The rule's own row renders empty in `lines`; the `─` run is the fill above.)
#[tokio::test]
async fn a_rule_does_not_pad_itself_with_blank_lines() {
    let (rpc, _incoming) = start().await;
    assert_eq!(
        render_lines(&rpc, "above\n\n---\n\nbelow").await,
        "above||below"
    );
    // Same when the block above is a fenced code block — the LSP hover shape
    // (signature fence, rule, prose).
    assert_eq!(
        render_lines(&rpc, "```rust\nfn f()\n```\n\n---\n\ndocs").await,
        "fn f()||docs"
    );
}

/// A rule that separates **nothing** is dropped. LSP servers emit their docs by
/// template — `<signature>\n---\n<docs>` — so an item with no docs arrives as a bare
/// trailing rule and one with no signature as a leading one; drawing that boundary
/// promises a section that isn't there.
#[tokio::test]
async fn a_rule_with_nothing_on_one_side_is_dropped() {
    let (rpc, _incoming) = start().await;
    // Trailing: the docs-less completion / hover payload.
    let payload = "```python\ndef foo()\n```\n---\n";
    assert_eq!(render_lines(&rpc, payload).await, "def foo()");
    assert_eq!(
        fills(&rpc, payload).await,
        "",
        "no dangling rule: {payload:?}"
    );
    // Leading, and two rules in a row — same argument from the other side.
    assert_eq!(fills(&rpc, "---\n\ndocs").await, "");
    assert_eq!(render_lines(&rpc, "---\n\ndocs").await, "docs");
    assert_eq!(
        fills(&rpc, "sig\n\n---\n\n---\n\ndocs").await,
        "2:─:@punctuation.special",
        "the second rule adds nothing"
    );
}

// ----- shipped example -------------------------------------------------------

/// The rendered-markdown **float window** map — the example now mounts an
/// `btv.view.component` as a real floating window (`floating == true`).
fn float_window(map: &[(Value, Value)]) -> Option<Vec<(Value, Value)>> {
    let Some(Value::Array(wins)) = map_get(map, "windows") else {
        return None;
    };
    wins.iter()
        .filter_map(Value::as_map)
        .find(|w| map_get(w, "floating").and_then(Value::as_bool) == Some(true))
        .map(|w| w.to_vec())
}

/// A window map's text rows (an ordinary window `lines` string array, not a content-float
/// chunk run).
fn win_lines(win: &[(Value, Value)]) -> Vec<String> {
    match map_get(win, "lines") {
        Some(Value::Array(rows)) => rows
            .iter()
            .map(|r| r.as_str().unwrap_or_default().to_string())
            .collect(),
        _ => Vec::new(),
    }
}

/// A window map's string field (e.g. `filetype`).
fn win_str(win: &[(Value, Value)], key: &str) -> String {
    map_get(win, key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Whether visible row `row` carries an **overlay** virt_text placement — the wire shape
/// is a per-row array of `[pos, col, hl_mode, chunks]`, `pos == 2` being overlay. The
/// example uses one to blank a hidden code-fence delimiter line.
fn win_row_has_overlay(win: &[(Value, Value)], row: usize) -> bool {
    let Some(Value::Array(rows)) = map_get(win, "virt_text") else {
        return false;
    };
    rows.get(row)
        .and_then(Value::as_array)
        .is_some_and(|placements| {
            placements
                .iter()
                .any(|p| p.as_array().and_then(|f| f.first()).and_then(Value::as_u64) == Some(2))
        })
}

/// The window's `line_bg` layer as `(row, style_id)` pairs — the full-width line
/// backgrounds (neovim's `line_hl_group`) the example paints on fenced code-block lines.
fn win_line_bg(win: &[(Value, Value)]) -> Vec<(u64, Option<u64>)> {
    match map_get(win, "line_bg") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|e| {
                let e = e.as_array()?;
                Some((e.first()?.as_u64()?, e.get(1).and_then(Value::as_u64)))
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Every highlight group name painted on the float window's `highlights` rows (each span
/// is `[start, end, group, style_id]`) — proves the component's `set_decor` styling
/// reached the window.
fn float_window_groups(win: &[(Value, Value)]) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(Value::Array(rows)) = map_get(win, "highlights") {
        for row in rows {
            if let Value::Array(spans) = row {
                for span in spans {
                    if let Some(g) = span
                        .as_array()
                        .and_then(|s| s.get(2))
                        .and_then(Value::as_str)
                    {
                        out.push(g.to_string());
                    }
                }
            }
        }
    }
    out
}

/// The `btv.markdown.to_view` → `btv.view.component` glue renders a buffer's markdown
/// into a **real floating window** whose content is the *stripped* markdown — being a
/// real window (not an overlay) is what makes it scroll. This is the composition the
/// `examples/markdown` config wires to `K`, covered here with an inline config.
#[tokio::test]
async fn a_view_component_renders_markdown_into_a_float() {
    // A heading + bold prose up top, a rust fence past the first screenful (so the
    // `G` scroll below genuinely reveals it in the 80%-tall float).
    let mut sample = String::from("# Markdown rendering\n\nProse with **bold** words.\n\n");
    for n in 0..40 {
        sample.push_str(&format!("Filler paragraph {n}.\n\n"));
    }
    sample.push_str("```rust\nfn zzz() {}\n```\n");
    let dir = temp_dir("md_example");
    let path = dir.join("sample.md");
    std::fs::write(&path, &sample).expect("write sample");

    let init = ServerInit {
        file: Some(path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, mut incoming) = spawn(init);
    attach(&rpc, 100, 40).await;
    // A theme so the rendered `@markup.*` groups (and the code-block `@markup.raw.block`
    // line background) resolve to styles.
    exec_lua(&rpc, "vim.cmd('colorscheme bemtvi')").await;

    // The example config's glue, inline: a component whose `render` maps the source
    // markdown to `btv.markdown.to_view`, mounted as a float typed `markdown` (so the
    // grammar's code-fence injections fire in the view buffer).
    exec_lua(
        &rpc,
        r#"
        local MarkdownFloat = btv.view.component({
          setup = function(ctx)
            ctx.wo.wrap = true
            return { src = ctx.props.src }
          end,
          render = function(state)
            return btv.markdown.to_view(state.src)
          end,
        })
        local src = table.concat(btv.buf.lines(btv.buf.current(), 0, -1), "\n")
        MarkdownFloat.mount({
          name = "[Rendered Markdown]",
          filetype = "markdown",
          props = { src = src },
          float = {
            relative = "editor",
            width = "80%",
            height = "80%",
            align = "center",
            border = "rounded",
          },
        })
        "#,
    )
    .await;

    // The view buffer/window and the component's first render arrive over the next few
    // ticks (the `btv._view_buf`/`_view_win` mirror + the reactive lifecycle), so poll.
    let mut win = None;
    for _ in 0..60 {
        barrier(&rpc).await;
        // Wait for the frame carrying the styled heading — the view buffer, its window,
        // the lines, and the `set_decor` extmarks all settle over the next few ticks.
        if let Some(map) = drain_to_latest_redraw(&mut incoming, |m| {
            float_window(m).is_some_and(|w| {
                float_window_groups(&w)
                    .iter()
                    .any(|g| g == "@markup.heading.1")
            })
        }) {
            win = float_window(&map);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let win = win.expect("the markdown float window opens with styling");
    let lines = win_lines(&win);

    assert!(
        lines.iter().any(|l| l.contains("Markdown rendering")),
        "the heading renders (stripped of '#') in the float window: {lines:?}"
    );
    // Prose markup is stripped (`**bold**` → `bold`); code fences are intentionally NOT
    // stripped (kept for the tree-sitter injection below) so they aren't checked here.
    assert!(
        lines.iter().all(|l| !l.contains("**")),
        "no raw inline markers in the rendered prose: {lines:?}"
    );
    // The component's `set_decor` styling reaches the window: the H1 heading carries an
    // `@markup.heading.1` span (byte-column-converted from the renderer's char columns).
    let groups = float_window_groups(&win);
    assert!(
        groups.iter().any(|g| g == "@markup.heading.1"),
        "the rendered heading is styled @markup.heading.1: {groups:?}"
    );
    // The view is typed `markdown` so the grammar's fenced-code injections highlight each
    // code block in its own language (rust here) — the "native tree-sitter" path.
    assert_eq!(
        win_str(&win, "filetype"),
        "markdown",
        "the rendered-markdown view is filetype=markdown so injections fire"
    );
    // The fenced code block lives lower in the document; scroll the focused float
    // to it (`G`) and confirm: it reads as a code region (a resolved `line_bg` background),
    // its ```rust fence is KEPT in the buffer (so injection can fire) but HIDDEN by an
    // overlay so it reads as rendered.
    feed(&rpc, "G");
    let mut scrolled = None;
    for _ in 0..60 {
        barrier(&rpc).await;
        if let Some(map) = drain_to_latest_redraw(&mut incoming, |m| {
            float_window(m).is_some_and(|w| win_line_bg(&w).iter().any(|(_, s)| s.is_some()))
        }) {
            scrolled = float_window(&map);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let scrolled = scrolled.expect("scrolling reveals the backed code block");
    assert!(
        win_line_bg(&scrolled).iter().any(|(_, s)| s.is_some()),
        "a code-block line is backed by a resolved line_bg after scrolling"
    );
    let scrolled_lines = win_lines(&scrolled);
    let fence_row = scrolled_lines
        .iter()
        .position(|l| l.starts_with("```rust"))
        .expect("the ```rust fence is kept in the buffer for injection");
    assert!(
        win_row_has_overlay(&scrolled, fence_row),
        "the kept fence line is hidden by a blanking overlay: {scrolled_lines:?}"
    );
}
