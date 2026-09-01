//! Behavior tests for the reStructuredText renderer behind `btv.rst.render` — the
//! pure rst → stripped-lines + `@markup.*` highlights transform in `bemtvi_core::rst`,
//! exposed to Lua. Black-box per the project conventions: a real server over RPC,
//! driven with `nvim_exec_lua`, asserting on the rendered lines, highlight spans and
//! code blocks it returns. The shape of `tests/markdown.rs`, for its sibling format.
//!
//! rst reaches the editor as `plaintext` — LSP's `MarkupKind` has no rst value — so
//! nothing detects it and these tests drive the renderer directly, which is exactly
//! how a caller reaches it too (`docs_format = "rst"` chooses this renderer; it never
//! chooses itself).

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{exec_lua, start_attached};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

async fn lua_string(rpc: &Rpc, code: &str) -> String {
    exec_lua(rpc, code)
        .await
        .as_str()
        .map(str::to_owned)
        .unwrap_or_default()
}

/// The rendered lines joined with `|`, so a test asserts the whole stripped block in
/// one string.
async fn render_lines(rpc: &Rpc, src: &str) -> String {
    let code = format!("local r = btv.rst.render({src:?})\nreturn table.concat(r.lines, '|')");
    lua_string(rpc, &code).await
}

/// The first highlight whose group is `group`, encoded `line,col_start,col_end` (or
/// `""` when none).
async fn first_span(rpc: &Rpc, src: &str, group: &str) -> String {
    let code = format!(
        "local r = btv.rst.render({src:?})\n\
         for _, h in ipairs(r.highlights) do\n\
           if h.group == {group:?} then\n\
             return string.format('%d,%d,%d', h.line, h.col_start, h.col_end)\n\
           end\n\
         end\n\
         return ''"
    );
    lua_string(rpc, &code).await
}

/// Every highlight group painted, in order, joined with `|`.
async fn groups(rpc: &Rpc, src: &str) -> String {
    let code = format!(
        "local r = btv.rst.render({src:?})\n\
         local out = {{}}\n\
         for _, h in ipairs(r.highlights) do out[#out + 1] = h.group end\n\
         return table.concat(out, '|')"
    );
    lua_string(rpc, &code).await
}

/// The code blocks encoded `first_line-last_line:lang` (`-` for a block with no
/// declared language), joined with `|`.
async fn code_blocks(rpc: &Rpc, src: &str) -> String {
    let code = format!(
        "local r = btv.rst.render({src:?})\n\
         local out = {{}}\n\
         for _, c in ipairs(r.code) do\n\
           out[#out + 1] = string.format('%d-%d:%s', c.first_line, c.last_line, c.lang or '-')\n\
         end\n\
         return table.concat(out, '|')"
    );
    lua_string(rpc, &code).await
}

/// The fills encoded `line:char:group`, joined with `|` — one per transition.
async fn fills(rpc: &Rpc, src: &str) -> String {
    let code = format!(
        "local r = btv.rst.render({src:?})\n\
         local out = {{}}\n\
         for _, f in ipairs(r.fills) do\n\
           out[#out + 1] = string.format('%d:%s:%s', f.line, f.char, f.group)\n\
         end\n\
         return table.concat(out, '|')"
    );
    lua_string(rpc, &code).await
}

// ---- inline markup ---------------------------------------------------------

/// The inline markers are consumed and their text kept, the way the markdown
/// renderer treats its own.
#[tokio::test]
async fn inline_markers_are_consumed_and_their_text_kept() {
    let (rpc, _incoming) = start().await;
    assert_eq!(
        render_lines(&rpc, "some **bold** and *italic* and ``literal`` here").await,
        "some bold and italic and literal here"
    );
    assert_eq!(
        first_span(&rpc, "some **bold** and more", "@markup.strong").await,
        "1,6,10",
        "the span covers the word, not the asterisks"
    );
}

/// **The rule that makes rst docstrings survive at all.** Docutils only recognizes a
/// start-string preceded by whitespace (or opening punctuation) and followed by
/// non-whitespace, and an end-string preceded by non-whitespace. Without that,
/// `*args, **kwargs` — in the docstring of nearly every python function that takes
/// them — opens spans that swallow the rest of the paragraph and eat the markers a
/// reader needs to see.
#[tokio::test]
async fn a_star_inside_a_word_is_text_not_markup() {
    let (rpc, _incoming) = start().await;
    assert_eq!(
        render_lines(&rpc, "Takes *args* and **kwargs and a*b*c.").await,
        "Takes args and **kwargs and a*b*c.",
        "only the properly-delimited pair is markup"
    );
    assert_eq!(
        groups(&rpc, "Takes *args* and **kwargs and a*b*c.").await,
        "@markup.italic",
        "and nothing else is styled"
    );
    // The two halves of the rule are load-bearing separately. Here the *end* is
    // well-formed (`z* ` closes cleanly), so only the start rule — a marker inside a
    // word opens nothing — keeps `x*y z* w` from emphasising across the words.
    assert_eq!(
        render_lines(&rpc, "x*y z* w").await,
        "x*y z* w",
        "a start-string inside a word opens nothing"
    );
    assert_eq!(groups(&rpc, "x*y z* w").await, "");
}

/// An unmatched start-string is an asterisk, not an error and not a span that runs to
/// the end of the document.
#[tokio::test]
async fn an_unclosed_marker_is_literal_text() {
    let (rpc, _incoming) = start().await;
    assert_eq!(
        render_lines(&rpc, "a *dangling marker and more text").await,
        "a *dangling marker and more text"
    );
    assert_eq!(groups(&rpc, "a *dangling marker").await, "");
}

/// A backslash escape makes the next character literal — `\*` is how a docstring
/// writes an asterisk it means as one.
#[tokio::test]
async fn a_backslash_escapes_the_marker_it_precedes() {
    let (rpc, _incoming) = start().await;
    assert_eq!(
        render_lines(&rpc, r"literal \*stars\* here").await,
        "literal *stars* here"
    );
    assert_eq!(groups(&rpc, r"literal \*stars\* here").await, "");
}

/// An interpreted-text role names how to read its text; the reader wants the text.
/// A reference keeps its label, and the embedded-URI form shows the target after it
/// the way a markdown link does.
#[tokio::test]
async fn roles_and_references_keep_their_text() {
    let (rpc, _incoming) = start().await;
    assert_eq!(
        render_lines(&rpc, "see :class:`Path` and `the docs <https://x.dev>`_").await,
        "see Path and the docs (https://x.dev)"
    );
    assert_eq!(
        groups(&rpc, "see :class:`Path` and `the docs <https://x.dev>`_").await,
        "@markup.raw|@markup.link.label|@markup.link.url"
    );
}

// ---- block constructs ------------------------------------------------------

/// A section title is its own row, its underline consumed, and its level is the
/// **order of first appearance** of the adornment character — rst gives `=` and `-`
/// no fixed meaning, only the order a document introduces them in.
#[tokio::test]
async fn section_titles_take_levels_by_first_appearance() {
    let (rpc, _incoming) = start().await;
    let src = "Title\n=====\n\nbody\n\nSub\n---\n\nmore\n";
    assert_eq!(render_lines(&rpc, src).await, "Title||body||Sub||more");
    assert_eq!(
        groups(&rpc, src).await,
        "@markup.heading.1|@markup.heading.2",
        "`=` appeared first, so it is level 1 and `-` level 2"
    );
}

/// A paragraph's line breaks carry no meaning in rst, so it reflows into one logical
/// line — the float wraps it to its own width. (This is the difference that made the
/// markdown renderer *nearly* right for rst and the plaintext one nearly right too.)
#[tokio::test]
async fn a_paragraph_reflows_its_soft_line_breaks() {
    let (rpc, _incoming) = start().await;
    assert_eq!(
        render_lines(&rpc, "one line\nand its\ncontinuation\n").await,
        "one line and its continuation"
    );
}

/// A transition is a rule, emitted as a whole-line fill exactly like a markdown
/// thematic break — same glyph, same group, so both formats draw one boundary.
#[tokio::test]
async fn a_transition_renders_as_a_rule() {
    let (rpc, _incoming) = start().await;
    let src = "before\n\n----------\n\nafter\n";
    assert_eq!(fills(&rpc, src).await, "2:─:@punctuation.special");
}

/// A paragraph ending in `::` introduces a literal block: the marker is reduced to a
/// single colon (Docutils' rule, so `Example::` reads as `Example:`) and the indented
/// block that follows becomes a code block with no declared language.
#[tokio::test]
async fn a_literal_block_keeps_its_text_and_loses_its_marker() {
    let (rpc, _incoming) = start().await;
    let src = "Example::\n\n    x = 1\n    y = 2\n";
    assert_eq!(render_lines(&rpc, src).await, "Example:||x = 1|y = 2");
    assert_eq!(
        code_blocks(&rpc, src).await,
        "3-4:-",
        "the indented block is code, in no particular language"
    );
}

/// A bare `::` paragraph disappears entirely rather than leaving a stray colon.
#[tokio::test]
async fn a_bare_literal_marker_leaves_no_line() {
    let (rpc, _incoming) = start().await;
    assert_eq!(
        render_lines(&rpc, "text\n\n::\n\n    code\n").await,
        "text||code"
    );
}

/// **The prize.** `.. code-block:: python` *declares* its language, which is more
/// than a markdown rendering of the same docstring ever recovers — so the block comes
/// back ready for the doc float to syntax-highlight.
#[tokio::test]
async fn a_code_directive_carries_its_declared_language() {
    let (rpc, _incoming) = start().await;
    let src = "Usage:\n\n.. code-block:: python\n\n   open(path).read()\n";
    assert_eq!(render_lines(&rpc, src).await, "Usage:||open(path).read()");
    assert_eq!(code_blocks(&rpc, src).await, "3-3:python");
}

/// A doctest block is an interactive python session by definition, so unlike a
/// literal block it comes with its language too.
#[tokio::test]
async fn a_doctest_block_is_python() {
    let (rpc, _incoming) = start().await;
    let src = "Example:\n\n>>> f(1)\n2\n";
    assert_eq!(render_lines(&rpc, src).await, "Example:||>>> f(1)|2");
    assert_eq!(code_blocks(&rpc, src).await, "3-4:python");
}

/// An admonition is labelled by its own name and its body renders as ordinary rst.
#[tokio::test]
async fn an_admonition_is_labelled_and_its_body_rendered() {
    let (rpc, _incoming) = start().await;
    let src = ".. note::\n\n   This is **important**.\n";
    assert_eq!(render_lines(&rpc, src).await, "Note|This is important.");
    assert_eq!(
        groups(&rpc, src).await,
        "@markup.strong|@markup.strong",
        "the label, then the body's own emphasis"
    );
}

/// A directive this dialect has never heard of still shows every word it wrote —
/// rendering must degrade to showing text, never to eating it.
#[tokio::test]
async fn an_unknown_directive_shows_its_name_and_body() {
    let (rpc, _incoming) = start().await;
    let src = ".. autoclass:: Widget\n\n   the widget\n";
    assert_eq!(render_lines(&rpc, src).await, "autoclass Widget|the widget");
}

/// A comment renders nothing — that is what a comment is — and takes its indented
/// body with it. Hyperlink targets and substitution definitions go the same way.
#[tokio::test]
async fn comments_and_targets_render_nothing() {
    let (rpc, _incoming) = start().await;
    let src =
        "text\n\n.. this is a comment\n   still the comment\n\n.. _label: https://x.dev\n\nmore\n";
    assert_eq!(render_lines(&rpc, src).await, "text||more");
}

/// A field list is laid out as an aligned two-column block, and `:type x:` folds into
/// its `:param x:` row — the Sphinx reading (`path (str)`), which costs a row rather
/// than adding one. This is the shape of nearly every documented python signature.
#[tokio::test]
async fn a_field_list_aligns_and_folds_its_types() {
    let (rpc, _incoming) = start().await;
    let src =
        ":param path: the file to read\n:type path: str\n:returns: its contents\n:rtype: bytes\n";
    assert_eq!(
        render_lines(&rpc, src).await,
        "  path (str)       the file to read|  returns (bytes)  its contents"
    );
    assert_eq!(
        first_span(&rpc, src, "@markup.strong").await,
        "1,3,13",
        "the name column is styled, the body is not"
    );
}

/// A field whose role takes no argument keeps its own name, and one this dialect
/// doesn't know keeps the whole field name — nothing is invented or dropped.
#[tokio::test]
async fn unknown_fields_keep_their_names() {
    let (rpc, _incoming) = start().await;
    assert_eq!(
        render_lines(
            &rpc,
            ":raises ValueError: on a bad path\n:custom thing: kept\n"
        )
        .await,
        "  ValueError    on a bad path|  custom thing  kept"
    );
}

/// Bullet and enumerated lists draw with the markdown renderer's own prefixes, so one
/// list looks the same whichever format described it.
#[tokio::test]
async fn lists_render_with_the_shared_markers() {
    let (rpc, _incoming) = start().await;
    assert_eq!(render_lines(&rpc, "- one\n- two\n").await, "• one|• two");
    assert_eq!(
        render_lines(&rpc, "1. first\n2. second\n").await,
        "1. first|2. second"
    );
}

/// A definition list is its term, styled, with the definition indented under it.
#[tokio::test]
async fn a_definition_list_styles_its_term() {
    let (rpc, _incoming) = start().await;
    let src = "path\n    the file to read\n";
    assert_eq!(render_lines(&rpc, src).await, "path|▎ the file to read");
    assert_eq!(first_span(&rpc, src, "@markup.strong").await, "1,1,5");
}

/// A line block's breaks are significant — an address, a verse, a hand-laid-out
/// list — so they survive, with the markers gone.
#[tokio::test]
async fn a_line_block_keeps_its_breaks() {
    let (rpc, _incoming) = start().await;
    assert_eq!(
        render_lines(&rpc, "| first line\n| second line\n").await,
        "first line|second line"
    );
}

/// A table is shown as the ASCII art it already is: in a fixed-width float it reads
/// as a table, and misparsing one would lose it entirely. Crucially, a simple table's
/// `====  ====` border must not be mistaken for a section underline.
#[tokio::test]
async fn a_table_renders_verbatim() {
    let (rpc, _incoming) = start().await;
    let src = "====  =====\nname  value\n====  =====\nx     1\n====  =====\n";
    assert_eq!(
        render_lines(&rpc, src).await,
        "====  =====|name  value|====  =====|x     1|====  ====="
    );
    assert_eq!(
        groups(&rpc, src).await,
        "",
        "and nothing in it is read as markup"
    );
}

/// A whole docstring, end to end: the shape the feature exists for.
#[tokio::test]
async fn a_python_docstring_renders_whole() {
    let (rpc, _incoming) = start().await;
    let src = "Read a file.\n\nReads the whole of *path* and returns it.\n\n\
               :param path: the file to read\n:type path: str\n:returns: its contents\n\n\
               .. code-block:: python\n\n   open(path).read()\n";
    assert_eq!(
        render_lines(&rpc, src).await,
        "Read a file.||Reads the whole of path and returns it.||  \
         path (str)  the file to read|  returns     its contents||open(path).read()"
    );
    assert_eq!(code_blocks(&rpc, src).await, "8-8:python");
}
