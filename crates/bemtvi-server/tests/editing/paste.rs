//! Bracketed paste — the client's "the user hit Cmd/Ctrl+V" input batch.
//!
//! A paste is delivered as one key-notation feed wrapped in `<PasteStart>` …
//! `<PasteEnd>` (what `bemtvi_view::encode_paste` emits), which puts the editor
//! in paste mode for the span: the payload is inserted **literally**, so none of
//! the reactive insert-mode machinery — auto-indent on `<CR>` (treesitter,
//! `smartindent`, `autoindent`), soft-tab expansion, auto-pairs — fires and
//! mangles text that already carries its own indentation.
//!
//! These run on a plain (no-grammar) buffer with `smartindent` / `autoindent`,
//! which are the *same* `indent_for` chain treesitter's verdict fronts — so the
//! grammar case is covered by the same suppression.

use crate::support::*;

/// The exact notation a client sends for a paste of `text` — the Rust encoder the
/// TUI/GUI use, so these tests exercise the real wire form rather than a guess.
fn paste(text: &str) -> String {
    bemtvi_view::encode_paste(text)
}

// ===== the reported bug ======================================================

#[tokio::test]
async fn paste_of_indented_text_keeps_its_own_indentation() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab smartindent<CR>");
    // Already-indented text: every `<CR>` in the payload would otherwise pick up
    // an auto-indent that stacks *on top of* the pasted line's own leading
    // whitespace, so each successive line drifts further right.
    feed(&rpc, "i");
    feed(
        &rpc,
        &paste("def f():\n    if x:\n        return 1\n    return 0\n"),
    );
    feed(&rpc, "<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec![
            "def f():",
            "    if x:",
            "        return 1",
            "    return 0",
            ""
        ],
    );
}

#[tokio::test]
async fn paste_of_indented_text_under_autoindent_keeps_its_own_indentation() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab autoindent<CR>");
    feed(&rpc, "i");
    feed(&rpc, &paste("    alpha\n        beta\n    gamma"));
    feed(&rpc, "<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["    alpha", "        beta", "    gamma"],
    );
}

#[tokio::test]
async fn paste_into_an_indented_line_does_not_re_indent_the_payload() {
    // Pasting at a cursor that already sits inside an indented block: the first
    // line joins where the cursor is, and the rest keep exactly the columns they
    // carried — the indent of the surrounding code is not added to them.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab smartindent<CR>");
    feed(&rpc, "iif x {<CR>");
    feed(&rpc, &paste("a();\n  b();\nc();"));
    feed(&rpc, "<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["if x {", "    a();", "  b();", "c();"],
    );
}

// ===== tabs stay tabs ========================================================

#[tokio::test]
async fn pasted_tabs_stay_literal_tabs_under_expandtab() {
    // A `\t` in the payload is a literal character of the pasted text, not a
    // `<Tab>` keypress: `expandtab` / `softtabstop` must not rewrite it into
    // spaces (vim's `'paste'` resets `expandtab` for exactly this reason).
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab<CR>");
    feed(&rpc, "i");
    feed(&rpc, &paste("\tone\n\t\ttwo"));
    feed(&rpc, "<Esc>");
    assert_eq!(lines(&rpc).await, vec!["\tone", "\t\ttwo"]);
}

// ===== auto-pairs stay out of it ============================================

#[tokio::test]
async fn paste_does_not_fire_autopairs() {
    // Auto-pairs would insert a closer for every opener in the payload — which
    // already has its own closers — doubling them.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab autopairs<CR>");
    feed(&rpc, "i");
    feed(&rpc, &paste("f(a, [b], \"c\")"));
    feed(&rpc, "<Esc>");
    assert_eq!(lines(&rpc).await, vec!["f(a, [b], \"c\")"]);
}

#[tokio::test]
async fn paste_does_not_fire_the_smartindent_electric_dedent() {
    // A `}` at the head of a pasted line is text, not a typed closing bracket, so
    // the electric re-indent must not snap the line to its opener's column.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab smartindent<CR>");
    feed(&rpc, "i");
    feed(&rpc, &paste("if x {\n  body\n  }\n"));
    feed(&rpc, "<Esc>");
    assert_eq!(lines(&rpc).await, vec!["if x {", "  body", "  }", ""]);
}

// ===== the flag doesn't leak ================================================

#[tokio::test]
async fn typing_after_a_paste_auto_indents_again() {
    // Paste mode lasts exactly as long as the payload: the `<CR>` the user types
    // *after* the pasted `{` still opens an indented block, and the `}` they type
    // still snaps back — the payload's own line break did neither.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab smartindent<CR>");
    feed(&rpc, "i");
    feed(&rpc, &paste("if x {\n  pasted"));
    feed(&rpc, "<CR>typed<CR>}<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["if x {", "  pasted", "  typed", "}"],
    );
}

#[tokio::test]
async fn an_unterminated_paste_does_not_wedge_the_editor() {
    // A paste is always one batch, so a `<PasteStart>` with no `<PasteEnd>` can only
    // come from a truncated or malformed feed. It must not leave paste mode latched
    // on — auto-indent (and the popup, and auto-pairs) would silently stay dead for
    // the rest of the session with nothing to point at.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab smartindent<CR>");
    feed(&rpc, "i<PasteStart>if x {");
    // A separate batch: the span is already closed, so this `<CR>` indents normally.
    feed(&rpc, "<CR>body<Esc>");
    assert_eq!(lines(&rpc).await, vec!["if x {", "    body"]);
}
