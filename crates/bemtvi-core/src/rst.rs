//! reStructuredText → the same [`Rendered`](crate::markdown::Rendered) the markdown
//! renderer produces: stripped display lines, `@markup.*` byte spans, whole-line
//! fills, and fenced code blocks. The second markup format bemtvi renders, and the
//! reason it exists is that LSP cannot name it.
//!
//! `MarkupKind` is a **closed two-value set** — `plaintext` or `markdown`. There is
//! no rst kind, so a server whose docstrings are reStructuredText declares
//! `plaintext`, the only honest value available to it, and the text arrives claiming
//! not to be markup while being exactly that. Nothing in the protocol distinguishes
//! it from genuinely-plain text, so rst is never *detected* here: a block reaches
//! this renderer only because the user declared that server's plaintext to be rst
//! (`docs_format = "rst"`). Sniffing the content — "it starts with `:param`" — would
//! silently reinterpret a plain docstring that happens to contain a `*`.
//!
//! **The docstring dialect, not the whole of Docutils.** rst is a large
//! specification with an open directive registry; docstrings use a narrow and
//! well-known part of it. That part is interpreted; everything else degrades to
//! *showing its own text* rather than being eaten — an unknown directive renders its
//! name and its body, a table renders as the ASCII art it already is.
//!
//! The output is built by driving the very same [`Renderer`] the markdown parser
//! drives, so where the two formats agree they render identically: a bullet is the
//! same bullet, a section title takes the same `@markup.heading.N`, a code block is
//! the same [`MdCode`](crate::markdown::MdCode) the doc float syntax-highlights.
//! What rst adds is a code block whose language is *declared* — `.. code-block::
//! python` — which is more than a markdown rendering of the same docstring ever
//! recovers.

use crate::markdown::{Rendered, Renderer, HEADING, ITALIC, LINK_LABEL, RAW, STRONG};

/// Render reStructuredText `src` into stripped display lines + styling. Pure and
/// infallible — text this dialect doesn't interpret still reaches the reader.
pub fn render(src: &str) -> Rendered {
    let mut r = Renderer::new();
    r.feed_rst(src);
    r.finish()
}

/// Fold `src` into an in-progress document (one section of a doc float). The entry
/// point behind [`Renderer::feed_rst`].
pub(crate) fn feed(r: &mut Renderer, src: &str) {
    // Tabs are 8-column tab stops in rst, and every construct here is decided by
    // indentation — so they are expanded once, up front, rather than at each of the
    // dozen places that measures an indent.
    let lines: Vec<String> = src.lines().map(expand_tabs).collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    Rst::default().block(r, &refs);
}

/// The punctuation a section under/overline may be drawn with (Docutils' set).
const ADORNMENT: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~";

/// Directives rendered as an **admonition**: a styled label line, then the body as
/// ordinary rst. The label is the directive's own name, which is what the reader of
/// the source saw; `.. note::` reads as `Note`.
const ADMONITIONS: &[&str] = &[
    "attention",
    "caution",
    "danger",
    "error",
    "hint",
    "important",
    "note",
    "tip",
    "warning",
    "admonition",
    "deprecated",
    "seealso",
    "versionadded",
    "versionchanged",
];

/// Directives whose body is **code**, and whose argument is its language.
const CODE_DIRECTIVES: &[&str] = &["code", "code-block", "sourcecode", "highlight"];

/// Docstring field roles that take an **argument** naming what they describe, so the
/// argument is the display name and the role word is dropped: `:param path:` reads
/// as `path`, `:raises ValueError:` as `ValueError`. A field whose role is not here
/// keeps its whole name (`:returns:` reads as `returns`).
const ARGUMENT_ROLES: &[&str] = &[
    "param",
    "parameter",
    "arg",
    "argument",
    "key",
    "keyword",
    "raises",
    "raise",
    "except",
    "exception",
    "var",
    "ivar",
    "cvar",
];

/// The type-carrying fields, and the field each annotates: `:type x:` belongs to
/// `:param x:`, `:rtype:` to `:returns:`. Sphinx renders the pair as one row
/// (`path (str) -- the file to read`) and so does this.
const TYPE_ROLES: &[&str] = &["type", "vartype"];
const RETURN_ROLES: &[&str] = &["returns", "return"];

/// Section-title state, which is the only thing that has to outlive one call:
/// heading level is assigned by the **order of first appearance** of an adornment
/// character, and a nested block (a directive body, a list item) sits inside the same
/// document as the sections around it.
#[derive(Default)]
struct Rst {
    /// Adornment characters in first-appearance order; a title's level is its
    /// character's index here. `(overlined, char)` — Docutils treats an overlined
    /// `=` as a different level from a plain underlined one.
    levels: Vec<(bool, char)>,
}

impl Rst {
    /// Render a block of `lines`, all at the same base indentation, into `r`.
    ///
    /// One construct is recognized per step and consumed whole (its own continuation
    /// and any block it owns), so the loop never has to remember what it is inside
    /// of. Order matters: the tests that can be confused with one another are
    /// resolved by trying the more specific first — a section title before a
    /// definition list (both are "a line followed by something"), a literal block
    /// before a block quote (both are "an indented block").
    fn block(&mut self, r: &mut Renderer, lines: &[&str]) {
        let base = base_indent(lines);
        let mut i = 0;
        while i < lines.len() {
            if lines[i].trim().is_empty() {
                i += 1;
                continue;
            }
            let next = 'dispatch: {
                // Indented past the block: a block quote. (A literal block's indent
                // was consumed by the paragraph that introduced it, and a construct's
                // own continuation by that construct, so nothing else reaches here
                // indented.)
                if indent_of(lines[i]) > base {
                    let end = indented_run(lines, i, base);
                    let inner = dedent(&lines[i..end]);
                    r.quote_push();
                    self.block(r, &borrow(&inner));
                    r.quote_pop();
                    break 'dispatch end;
                }
                if let Some(next) = self.directive_or_comment(r, lines, i) {
                    break 'dispatch next;
                }
                if let Some(next) = self.field_list(r, lines, i) {
                    break 'dispatch next;
                }
                if let Some(next) = self.list(r, lines, i) {
                    break 'dispatch next;
                }
                if let Some(next) = self.line_block(r, lines, i) {
                    break 'dispatch next;
                }
                if let Some(next) = self.doctest(r, lines, i) {
                    break 'dispatch next;
                }
                if let Some(next) = self.table(r, lines, i) {
                    break 'dispatch next;
                }
                if let Some(next) = self.section(r, lines, i) {
                    break 'dispatch next;
                }
                if let Some(next) = transition(r, lines, i) {
                    break 'dispatch next;
                }
                if let Some(next) = self.definition(r, lines, i) {
                    break 'dispatch next;
                }
                self.paragraph(r, lines, i)
            };
            // One blank row parts every block from the next, as it does in markdown —
            // the constructs themselves only ever open with `block_gap`, so the gap is
            // declared here, once, rather than at the tail of each of them.
            r.end_block();
            i = next;
        }
    }

    /// A **section title**: a text line under a run of one adornment character at
    /// least as long as it, optionally over one too. The level is the adornment's
    /// place in [`Rst::levels`] — rst has no fixed meaning for `=` versus `-`, only
    /// the order a document introduces them in.
    fn section(&mut self, r: &mut Renderer, lines: &[&str], i: usize) -> Option<usize> {
        let (title, adorn, overlined, next) = match adornment_char(lines.get(i)?) {
            // Overline: adornment, title, matching underline.
            Some(over) => {
                let title = lines.get(i + 1)?;
                let under = adornment_char(lines.get(i + 2)?)?;
                (title.trim(), (under == over).then_some(over)?, true, i + 3)
            }
            None => {
                let under = adornment_char(lines.get(i + 1)?)?;
                let title = lines[i].trim();
                // The underline must reach the title, or this is something else
                // entirely (a `---` under a short paragraph line is a transition).
                (
                    title,
                    (lines[i + 1].trim().chars().count() >= title.chars().count())
                        .then_some(under)?,
                    false,
                    i + 2,
                )
            }
        };
        if title.is_empty() {
            return None;
        }
        let key = (overlined, adorn);
        let level = match self.levels.iter().position(|k| *k == key) {
            Some(at) => at,
            None => {
                self.levels.push(key);
                self.levels.len() - 1
            }
        };
        r.block_gap();
        let group = HEADING[level.min(HEADING.len() - 1)];
        r.open(group);
        inline(r, title);
        r.close(group);
        r.newline();
        Some(next)
    }

    /// A **directive** (`.. name:: argument`) or a **comment** (`..` followed by
    /// anything that isn't one). A comment renders nothing at all — that is what a
    /// comment is — and takes its indented body with it.
    fn directive_or_comment(
        &mut self,
        r: &mut Renderer,
        lines: &[&str],
        i: usize,
    ) -> Option<usize> {
        let line = lines[i];
        let rest = line.trim_start().strip_prefix("..")?;
        if !rest.is_empty() && !rest.starts_with(' ') {
            return None; // `..word` is not a marker
        }
        let base = indent_of(line);
        let end = indented_run(lines, i + 1, base).max(i + 1);
        let body = dedent(&lines[i + 1..end]);
        let rest = rest.trim_start();
        let Some((name, argument)) = directive_head(rest) else {
            return Some(end); // a comment / hyperlink target / substitution
        };
        let name = name.to_ascii_lowercase();
        if CODE_DIRECTIVES.contains(&name.as_str()) {
            // `:linenos:`-style options belong to the directive, not the code.
            let code: Vec<&str> = body
                .iter()
                .map(String::as_str)
                .skip_while(|l| l.trim_start().starts_with(':') && l.contains(':'))
                .skip_while(|l| l.trim().is_empty())
                .collect();
            let lang = argument.split_whitespace().next().filter(|s| !s.is_empty());
            r.code_block(lang, &code.join("\n"), false);
            return Some(end);
        }
        // An admonition is labelled by its own name; an unrecognized directive is
        // labelled the same way rather than being dropped, so a docstring using a
        // directive this dialect has never heard of still shows every word it wrote.
        let label = if ADMONITIONS.contains(&name.as_str()) {
            title_case(&name)
        } else {
            name.clone()
        };
        r.block_gap();
        r.open(STRONG);
        r.write(&label);
        if !argument.trim().is_empty() {
            r.write(" ");
            r.write(argument.trim());
        }
        r.close(STRONG);
        r.newline();
        self.block(r, &borrow(&body));
        Some(end)
    }

    /// A **field list**: consecutive `:name: body` items, rendered as an aligned
    /// two-column block. This is the shape of nearly every documented python
    /// signature, so it is the construct that most repays being laid out rather than
    /// shown: `:param path:` / `:type path:` / `:returns:` / `:rtype:` become two
    /// rows reading `path (str)` and `returns (bytes)`, the way Sphinx renders them.
    fn field_list(&mut self, r: &mut Renderer, lines: &[&str], i: usize) -> Option<usize> {
        let base = indent_of(lines[i]);
        field_head(lines[i])?;
        let mut fields: Vec<(String, String)> = Vec::new();
        let mut at = i;
        while at < lines.len() {
            let Some((name, first)) = field_head(lines[at]) else {
                break;
            };
            if indent_of(lines[at]) != base {
                break;
            }
            let end = indented_run(lines, at + 1, base).max(at + 1);
            let mut body: Vec<String> = vec![first.trim().to_string()];
            body.extend(dedent(&lines[at + 1..end]));
            fields.push((name.to_string(), join_paragraph(&body)));
            at = end;
            while at < lines.len() && lines[at].trim().is_empty() {
                // A blank row inside a field list is allowed; a second one ends it.
                if lines.get(at + 1).is_some_and(|l| field_head(l).is_some()) {
                    at += 1;
                    continue;
                }
                break;
            }
        }
        if fields.is_empty() {
            return None;
        }
        emit_fields(r, fields);
        Some(at)
    }

    /// A **bullet or enumerated list**. Each item owns its marker line plus every
    /// line indented past the marker; the items' bodies render as rst in their own
    /// right, so a nested list or a literal block inside an item works.
    fn list(&mut self, r: &mut Renderer, lines: &[&str], i: usize) -> Option<usize> {
        let base = indent_of(lines[i]);
        let (_, first_start) = list_marker(lines[i])?;
        let ordered = enumerator(lines[i]).is_some();
        r.list_push(ordered.then_some(enumerator(lines[i]).unwrap_or(1)));
        let mut at = i;
        let mut item_start = first_start;
        while at < lines.len() {
            let Some((_, start)) = list_marker(lines.get(at)?) else {
                break;
            };
            if indent_of(lines[at]) != base || enumerator(lines[at]).is_some() != ordered {
                break;
            }
            item_start = start;
            let end = indented_run(lines, at + 1, base).max(at + 1);
            let mut body: Vec<String> = vec![lines[at][start..].to_string()];
            body.extend(lines[at + 1..end].iter().map(|l| l.to_string()));
            r.list_item();
            self.block(r, &borrow(&dedent_by(&body, item_start)));
            at = end;
            let mut loose = false;
            while at < lines.len() && lines[at].trim().is_empty() {
                loose = true;
                at += 1;
            }
            // A **tight** list (items adjacent in the source) draws its bullets on
            // consecutive rows: the item's own content ended with a block gap pending,
            // and nothing separates it from the next bullet. A list the author spaced
            // out keeps that spacing, as a loose markdown list does.
            if !loose {
                r.no_gap();
            }
            if at < lines.len() && indent_of(lines[at]) != base {
                break;
            }
        }
        let _ = item_start;
        r.list_pop();
        Some(at)
    }

    /// A **line block** (`| `): lines whose breaks are significant — an address, a
    /// verse, a hand-laid-out list. Rendered as its own lines with the markers gone.
    fn line_block(&mut self, r: &mut Renderer, lines: &[&str], i: usize) -> Option<usize> {
        let base = indent_of(lines[i]);
        strip_line_block(lines[i])?;
        r.block_gap();
        let mut at = i;
        while at < lines.len() && indent_of(lines[at]) == base {
            let Some(text) = strip_line_block(lines[at]) else {
                break;
            };
            inline(r, text);
            r.newline();
            at += 1;
        }
        Some(at)
    }

    /// A **doctest block** (`>>> …`): an interactive session, which is python by
    /// definition — so unlike a literal block it comes with its language, and the
    /// doc float highlights it.
    fn doctest(&mut self, r: &mut Renderer, lines: &[&str], i: usize) -> Option<usize> {
        if !lines[i].trim_start().starts_with(">>>") {
            return None;
        }
        let end = lines[i..]
            .iter()
            .position(|l| l.trim().is_empty())
            .map_or(lines.len(), |n| i + n);
        r.code_block(Some("python"), &dedent(&lines[i..end]).join("\n"), false);
        Some(end)
    }

    /// A **grid or simple table**, shown verbatim. Both are ASCII art that already
    /// reads as a table in a fixed-width float; re-laying one out would gain
    /// alignment it already has, and misparsing one would lose the whole table.
    fn table(&mut self, r: &mut Renderer, lines: &[&str], i: usize) -> Option<usize> {
        if !is_table_border(lines[i]) {
            return None;
        }
        let end = lines[i..]
            .iter()
            .position(|l| l.trim().is_empty())
            .map_or(lines.len(), |n| i + n);
        r.verbatim_lines(&lines[i..end]);
        Some(end)
    }

    /// A **definition list**: a one-line term with its definition indented directly
    /// under it, no blank line between. Recognized last of the "line plus something"
    /// constructs, so a section title and a literal block get first refusal.
    fn definition(&mut self, r: &mut Renderer, lines: &[&str], i: usize) -> Option<usize> {
        let base = indent_of(lines[i]);
        if lines[i].trim_end().ends_with("::") {
            return None; // introduces a literal block; the paragraph owns it
        }
        if indent_of(lines.get(i + 1)?) <= base || lines[i + 1].trim().is_empty() {
            return None;
        }
        let end = indented_run(lines, i + 1, base);
        r.block_gap();
        // The term, then its definition indented under it as its own block.
        r.open(STRONG);
        inline(r, lines[i].trim());
        r.close(STRONG);
        r.newline();
        r.quote_push();
        self.block(r, &borrow(&dedent(&lines[i + 1..end])));
        r.quote_pop();
        Some(end)
    }

    /// A **paragraph**: every line to the next blank one or the next construct,
    /// reflowed into one logical line (rst line breaks inside a paragraph are not
    /// significant) with its inline markup applied.
    ///
    /// A paragraph ending in `::` introduces a **literal block** — the indented block
    /// that follows it, verbatim, with no language. The marker itself is removed
    /// (`text ::` and a bare `::`) or reduced to a single colon (`text::`), which is
    /// what Docutils specifies and what makes `Example::` read as `Example:`.
    fn paragraph(&mut self, r: &mut Renderer, lines: &[&str], i: usize) -> usize {
        let base = indent_of(lines[i]);
        let mut end = i + 1;
        while end < lines.len()
            && !lines[end].trim().is_empty()
            && indent_of(lines[end]) >= base
            && !self.starts_construct(lines, end)
        {
            end += 1;
        }
        let text = join_paragraph(
            &lines[i..end]
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>(),
        );
        let (text, literal) = match text.strip_suffix("::") {
            Some(head) if head.trim_end().is_empty() => (String::new(), true),
            Some(head) if head.ends_with(char::is_whitespace) => {
                (head.trim_end().to_string(), true)
            }
            Some(head) => (format!("{head}:"), true),
            None => (text, false),
        };
        if !text.is_empty() {
            r.block_gap();
            inline(r, &text);
            r.newline();
        }
        if !literal {
            return end;
        }
        // The literal block is its own block, so it is parted from the paragraph that
        // introduced it exactly as a markdown fence is from the paragraph above it.
        r.end_block();
        // The literal block: the indented run after the marker, verbatim.
        let mut at = end;
        while at < lines.len() && lines[at].trim().is_empty() {
            at += 1;
        }
        if at >= lines.len() || indent_of(lines[at]) <= base {
            return end;
        }
        let stop = indented_run(lines, at, base);
        r.code_block(None, &dedent(&lines[at..stop]).join("\n"), false);
        stop
    }

    /// Whether line `at` opens a construct, so the paragraph before it must stop.
    /// Only the constructs that can follow a paragraph line *without* a blank row
    /// between are listed — the rest are separated by definition.
    fn starts_construct(&self, lines: &[&str], at: usize) -> bool {
        field_head(lines[at]).is_some()
            || list_marker(lines[at]).is_some()
            || strip_line_block(lines[at]).is_some()
            || lines[at].trim_start().starts_with(">>>")
            || lines[at].trim_start().starts_with(".. ")
            // A section underline ends the line above it, which is its title.
            || adornment_char(lines[at]).is_some()
    }
}

/// A **transition**: a rule of 4+ adornment characters on its own, separating what
/// comes before from what follows. Distinguished from a section underline by having
/// been offered the line *after* [`Rst::section`] declined it.
fn transition(r: &mut Renderer, lines: &[&str], i: usize) -> Option<usize> {
    let ch = adornment_char(lines[i])?;
    if lines[i].trim().chars().count() < 4 || ch == ':' {
        return None;
    }
    r.rule();
    Some(i + 1)
}

/// Lay a field list out as an aligned two-column block: the display name, padded to
/// the widest, then the body. The type fields are folded into the rows they annotate
/// first (`:type path: str` onto `:param path:`), which is the Sphinx reading and
/// costs a row rather than adding one.
fn emit_fields(r: &mut Renderer, fields: Vec<(String, String)>) {
    /// The name column's cap: past this a long parameter name gets its own width
    /// rather than pushing every row's text off the float.
    const MAX_NAME: usize = 24;

    let mut types: Vec<(String, String)> = Vec::new();
    for (name, body) in &fields {
        let (role, arg) = split_role(name);
        if TYPE_ROLES.contains(&role) {
            types.push((arg.to_string(), body.clone()));
        } else if role == "rtype" {
            types.push((String::from("\0return"), body.clone()));
        }
    }
    let mut rows: Vec<(String, String)> = Vec::new();
    for (name, body) in fields {
        let (role, arg) = split_role(&name);
        if TYPE_ROLES.contains(&role) || role == "rtype" {
            continue; // folded into its own field's row below
        }
        let display = if ARGUMENT_ROLES.contains(&role) && !arg.is_empty() {
            arg.to_string()
        } else {
            name.clone()
        };
        let key = if RETURN_ROLES.contains(&role) {
            "\0return"
        } else {
            arg
        };
        let display = match types.iter().find(|(k, _)| k == key) {
            Some((_, ty)) if !ty.is_empty() => format!("{display} ({ty})"),
            _ => display,
        };
        rows.push((display, body));
    }
    if rows.is_empty() {
        return;
    }
    let width = rows
        .iter()
        .map(|(n, _)| n.chars().count())
        .max()
        .unwrap_or(0)
        .min(MAX_NAME);
    r.block_gap();
    for (name, body) in rows {
        r.write("  ");
        r.open(STRONG);
        r.write(&name);
        r.close(STRONG);
        let pad = width.saturating_sub(name.chars().count()) + 2;
        r.write(&" ".repeat(pad));
        inline(r, &body);
        r.newline();
    }
}

/// A field's `(role, argument)`: `param path` splits into `("param", "path")`,
/// `returns` into `("returns", "")`. The role is lowercased for comparison against
/// the known sets; the argument keeps its case, being a name from the code.
fn split_role(name: &str) -> (&str, &str) {
    match name.split_once(char::is_whitespace) {
        Some((role, arg)) => (role, arg.trim()),
        None => (name, ""),
    }
}

/// `:name: rest` at the start of a line — the field's name and the remainder of its
/// first line. `None` when the line is not a field: a bare `:` , an unterminated
/// name, or an inline role (`:class:`Foo``, whose colon is not at column zero of the
/// stripped line).
fn field_head(line: &str) -> Option<(&str, &str)> {
    let rest = line.trim_start().strip_prefix(':')?;
    let end = rest.find(':')?;
    let name = &rest[..end];
    if name.is_empty() || name.contains('`') {
        return None;
    }
    let after = &rest[end + 1..];
    // A field marker is followed by whitespace or nothing; `:foo:bar` is not one.
    if !after.is_empty() && !after.starts_with(' ') {
        return None;
    }
    Some((name, after))
}

/// `.. name:: argument` — the directive's name and its argument. `None` for a
/// comment, a hyperlink target (`.. _label:`), or a substitution definition.
fn directive_head(rest: &str) -> Option<(&str, &str)> {
    let end = rest.find("::")?;
    let name = &rest[..end];
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_+:.".contains(c))
    {
        return None;
    }
    Some((name, &rest[end + 2..]))
}

/// A bullet or enumerator at the start of `line`: the marker and the byte offset its
/// text begins at. The offset is what an item's continuation lines are dedented by,
/// so a wrapped item lines up under its own first word.
fn list_marker(line: &str) -> Option<(&str, usize)> {
    let indent = indent_of(line);
    let rest = &line[indent..];
    let mut chars = rest.chars();
    let first = chars.next()?;
    if "-*+•‣⁃".contains(first) {
        let after = &rest[first.len_utf8()..];
        if after.is_empty() {
            return Some((&rest[..first.len_utf8()], line.len()));
        }
        if !after.starts_with(' ') {
            return None;
        }
        let text = after.len() - after.trim_start().len();
        return Some((&rest[..first.len_utf8()], indent + first.len_utf8() + text));
    }
    enumerator_marker(rest).map(|(m, off)| (m, indent + off))
}

/// The number an enumerated-list line starts at (`1.` → `1`, `#.` → the list simply
/// continues), or `None` when the line is not enumerated.
fn enumerator(line: &str) -> Option<u64> {
    let rest = &line[indent_of(line)..];
    let (marker, _) = enumerator_marker(rest)?;
    marker
        .trim_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()
        .or(Some(1))
}

/// The enumerator prefix of `rest` (`1.`, `2)`, `(3)`, `#.`) and where its text
/// starts. Arabic and the auto-enumerator only: a lone `a.` or `i.` is far more often
/// a sentence than a list, and rst itself requires two such items to disambiguate.
fn enumerator_marker(rest: &str) -> Option<(&str, usize)> {
    let body = rest.strip_prefix('(').unwrap_or(rest);
    let paren = rest.len() - body.len();
    let digits = body
        .find(|c: char| !c.is_ascii_digit())
        .filter(|n| *n > 0 || body.starts_with('#'))?;
    let digits = if body.starts_with('#') { 1 } else { digits };
    let after = &body[digits..];
    let close = if after.starts_with(')') || after.starts_with('.') {
        1
    } else {
        return None;
    };
    let end = paren + digits + close;
    let tail = &rest[end..];
    if tail.is_empty() {
        return Some((&rest[..end], rest.len()));
    }
    if !tail.starts_with(' ') {
        return None;
    }
    let text = tail.len() - tail.trim_start().len();
    Some((&rest[..end], end + text))
}

/// The text of a line-block line (`| the text`), or `None` for any other line.
fn strip_line_block(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix('|')?;
    if rest.is_empty() {
        return Some("");
    }
    rest.starts_with(' ').then(|| rest.trim_start())
}

/// The single adornment character `line` is a run of, or `None` when it is anything
/// else. A run only — `====  ====` is a simple-table border, not an underline.
fn adornment_char(line: &str) -> Option<char> {
    let t = line.trim();
    let first = t.chars().next()?;
    if !ADORNMENT.contains(first) || t.chars().count() < 2 {
        return None;
    }
    t.chars().all(|c| c == first).then_some(first)
}

/// Whether `line` is a table border: a run of `=` or `-` (simple) or a `+---+` row
/// (grid), with the internal spacing that distinguishes it from a section underline.
fn is_table_border(line: &str) -> bool {
    let t = line.trim();
    if t.len() < 3 {
        return false;
    }
    let grid = t.starts_with('+')
        && t.ends_with('+')
        && t.chars().all(|c| c == '+' || c == '-' || c == '=');
    let simple = t.contains(' ')
        && t.chars().all(|c| c == '=' || c == ' ')
        && t.starts_with('=')
        && t.ends_with('=');
    grid || simple
}

/// The block's own indentation: the smallest indent of its non-blank lines.
fn base_indent(lines: &[&str]) -> usize {
    lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| indent_of(l))
        .min()
        .unwrap_or(0)
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The end of the run of lines from `from` that are indented past `base` (blank lines
/// inside it belong to the run; trailing blanks do not).
fn indented_run(lines: &[&str], from: usize, base: usize) -> usize {
    let mut end = from;
    let mut at = from;
    while at < lines.len() {
        if lines[at].trim().is_empty() {
            at += 1;
            continue;
        }
        if indent_of(lines[at]) <= base {
            break;
        }
        at += 1;
        end = at;
    }
    end
}

/// Remove the block's common indentation, so a nested block renders as a document in
/// its own right rather than as one that happens to be indented.
fn dedent(lines: &[&str]) -> Vec<String> {
    dedent_by(
        &lines.iter().map(|l| (*l).to_string()).collect::<Vec<_>>(),
        base_indent(lines),
    )
}

fn dedent_by(lines: &[String], by: usize) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            let cut = by.min(l.len() - l.trim_start().len());
            l[cut..].to_string()
        })
        .collect()
}

fn borrow(lines: &[String]) -> Vec<&str> {
    lines.iter().map(String::as_str).collect()
}

/// Reflow a paragraph's lines into one logical line. rst line breaks inside a
/// paragraph carry no meaning, and the float wraps to its own width.
fn join_paragraph(lines: &[String]) -> String {
    lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Expand tabs to 8-column tab stops — rst's own rule, applied once so every indent
/// measurement downstream is in columns.
fn expand_tabs(line: &str) -> String {
    if !line.contains('\t') {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len() + 8);
    for c in line.chars() {
        if c == '\t' {
            let pad = 8 - (out.chars().count() % 8);
            out.push_str(&" ".repeat(pad));
        } else {
            out.push(c);
        }
    }
    out
}

fn title_case(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

// ---- inline markup ---------------------------------------------------------

/// Docutils' inline-markup recognition rules, which are the whole reason this is a
/// scanner rather than a set of `replace` calls.
///
/// A start-string must be preceded by whitespace or one of ``-:/'"<([{`` and must not
/// be followed by whitespace; an end-string must not be preceded by whitespace and
/// must be followed by whitespace, the end of the text, or one of ``-.,:;!?\/'")]}>``.
///
/// Without them every python docstring that documents `*args, **kwargs` renders
/// wrong: `**kwargs` would open a strong span that swallows the rest of the
/// paragraph, and `a*b*c` would emphasise its middle. With them, `*` inside a word is
/// literal text, which is what a reader of the source sees.
fn can_start(before: Option<char>, after: Option<char>) -> bool {
    let ok_before = before.is_none_or(|c| c.is_whitespace() || "-:/'\"<([{".contains(c));
    let ok_after = after.is_some_and(|c| !c.is_whitespace());
    ok_before && ok_after
}

fn can_end(before: Option<char>, after: Option<char>) -> bool {
    let ok_before = before.is_some_and(|c| !c.is_whitespace());
    let ok_after = after.is_none_or(|c| c.is_whitespace() || "-.,:;!?\\/'\")]}>".contains(c));
    ok_before && ok_after
}

/// The byte offset just past the closing `marker` for a span opened at `from`, or
/// `None` when the text never closes it (in which case the start-string is literal
/// text — an unmatched `**` is not an error, it is an asterisk).
fn find_end(text: &str, from: usize, marker: &str) -> Option<usize> {
    let mut at = from;
    while let Some(rel) = text[at..].find(marker) {
        let start = at + rel;
        if start == from {
            at = start + marker.len();
            continue; // empty span: `**` immediately closing
        }
        let before = text[..start].chars().next_back();
        // A reference's trailing `_` / `__` belongs to the **end-string**, not to
        // what follows it: in `` `the docs <url>`_ `` the closer is `` `_ ``, and it
        // is the character after *that* the rule is about. Without this the backtick
        // never closes and the whole reference renders as its own source.
        let tail = &text[start + marker.len()..];
        let tail = if marker == "`" {
            tail.trim_start_matches('_')
        } else {
            tail
        };
        // An escaped marker closes nothing.
        if before != Some('\\') && can_end(before, tail.chars().next()) {
            return Some(start);
        }
        at = start + marker.len();
    }
    None
}

/// Render `text`'s inline markup into `r`: literals, strong, emphasis, interpreted
/// text and roles, and references (including the embedded-URI form). Anything that
/// doesn't parse is written as the text it is.
fn inline(r: &mut Renderer, text: &str) {
    let mut plain = String::new();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let before = text[..i].chars().next_back();
        // A backslash escape makes the next character literal — the reader wrote
        // `\*` to mean an asterisk.
        if let Some(escaped) = rest.strip_prefix('\\') {
            if let Some(c) = escaped.chars().next() {
                plain.push(c);
                i += 1 + c.len_utf8();
                continue;
            }
        }
        let matched = ["``", "**", "*", "`"].into_iter().find_map(|marker| {
            if !rest.starts_with(marker) {
                return None;
            }
            // `**` must win over `*`, and `` `` `` over `` ` ``: the shorter marker
            // would open a span the longer one's closing pair ends in the wrong place.
            let after = rest[marker.len()..].chars().next();
            if !can_start(before, after) {
                return None;
            }
            let from = i + marker.len();
            let end = find_end(text, from, marker)?;
            Some((marker, from, end))
        });
        let Some((marker, from, end)) = matched else {
            let c = rest.chars().next().expect("non-empty rest");
            plain.push(c);
            i += c.len_utf8();
            continue;
        };
        flush(r, &mut plain);
        let body = &text[from..end];
        let after = &text[end + marker.len()..];
        match marker {
            "``" => styled(r, RAW, body),
            "**" => styled(r, STRONG, body),
            "*" => styled(r, ITALIC, body),
            _ => interpreted(r, body, after.starts_with('_')),
        }
        i = end + marker.len();
        // A reference's trailing `_` (or `__`) is markup, not text.
        if marker == "`" {
            i += after.len() - after.trim_start_matches('_').len();
        }
    }
    flush(r, &mut plain);
}

/// Single-backtick text: an interpreted-text role (`:class:`Foo``, whose role prefix
/// was written before the backtick and is stripped by the caller's plain run), a
/// title reference, or — with a trailing `_` — a hyperlink reference, whose embedded
/// URI is shown after the label the way a markdown link's is.
fn interpreted(r: &mut Renderer, body: &str, reference: bool) {
    if reference {
        // `` `label <url>`_ `` — the embedded-URI form.
        if let Some(open) = body.rfind(" <") {
            if let Some(url) = body[open + 2..].strip_suffix('>') {
                styled(r, LINK_LABEL, body[..open].trim());
                r.append_link_url(url);
                return;
            }
        }
        styled(r, LINK_LABEL, body);
        return;
    }
    styled(r, RAW, body);
}

fn styled(r: &mut Renderer, group: &'static str, text: &str) {
    r.open(group);
    r.write(text);
    r.close(group);
}

fn flush(r: &mut Renderer, plain: &mut String) {
    if plain.is_empty() {
        return;
    }
    // An interpreted-text role written before its backticks (`:class:`Foo``) is
    // markup: the role names how to read the text, and the reader wants the text.
    let text = strip_trailing_role(plain);
    r.write(text);
    plain.clear();
}

/// Drop a trailing `:role:` from a plain run — the prefix of an interpreted-text
/// role whose backticked body follows it. Only a well-formed role is dropped, so a
/// sentence ending in a colon keeps it.
fn strip_trailing_role(plain: &str) -> &str {
    let Some(head) = plain.strip_suffix(':') else {
        return plain;
    };
    let Some(open) = head.rfind(':') else {
        return plain;
    };
    let name = &head[open + 1..];
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '+' || c == ':')
    {
        return plain;
    }
    &plain[..open]
}
