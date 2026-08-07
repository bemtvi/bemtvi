//! A pure CommonMark + GFM → *stripped-lines + styling* renderer for nxvim's
//! read-only documentation popups (LSP hover and the completion-docs sidebar).
//!
//! The problem it solves: an LSP server sends hover/documentation as **markdown**,
//! and nxvim used to drop it into a float verbatim — so you saw the literal
//! `**bold**`, `# heading`, ` ``` ` fences and unaligned `|` tables. This module
//! parses that markdown **once, at ingest**, and produces:
//!
//! - [`Rendered::lines`] — the display text with the markup syntax removed
//!   (`**strong**` → `strong`, `# Title` → `Title`, fences dropped, `- item` →
//!   `• item`);
//! - [`Rendered::spans`] — inline highlight ranges (byte offsets *within a line*)
//!   tagged with a neovim `@markup.*` capture name, so a colorscheme styles them
//!   (bold/italic attributes and all) with no extra config;
//! - [`Rendered::fills`] — row fills that run from a line's text to the right edge
//!   (a thematic break → a full-width `─` rule; a section header → a labelled one);
//! - [`Rendered::code`] — the fenced code blocks, for the caller to syntax-color
//!   in their own language (the float does this via `preview_highlights`).
//!
//! It is **pure** (string in, data out) and never fails: any construct it does not
//! specially style still contributes its text — nothing is silently dropped. Spans
//! and fills are the raw material the doc-float renderer turns into extmarks; the
//! Lua surface (`nx.markdown.render`) exposes the same output to plugins.
//!
//! Author line structure is preserved: a soft break inside a paragraph becomes a
//! real line break (many servers lay out one sentence per line and expect it kept),
//! and the doc-float window wraps any line that overruns its width. Prose is *not*
//! hard-wrapped here.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Highlight capture names emitted for each markdown construct. These are neovim's
/// `@markup.*` treesitter capture names, so any colorscheme that styles markdown /
/// treesitter buffers styles these popups identically — resolved through the same
/// capture→`Style` table as buffer highlights, so `@markup.strong`'s bold attribute
/// actually renders bold. Kept in one place so the styled surface is auditable.
const HEADING: [&str; 6] = [
    "@markup.heading.1",
    "@markup.heading.2",
    "@markup.heading.3",
    "@markup.heading.4",
    "@markup.heading.5",
    "@markup.heading.6",
];
const STRONG: &str = "@markup.strong";
const ITALIC: &str = "@markup.italic";
const STRIKE: &str = "@markup.strikethrough";
const RAW: &str = "@markup.raw";
const LINK_LABEL: &str = "@markup.link.label";
const LINK_URL: &str = "@markup.link.url";
const LIST: &str = "@markup.list";
const QUOTE: &str = "@markup.quote";
const RULE: &str = "@punctuation.special";

/// The glyph a **section header** (and a thematic break) is drawn with, the text that
/// leads one, and the groups its two parts take: `─ pyright ────────`, a label inset in
/// a rule — the shape a float's border title has. Public because the signature-help
/// float, whose content is *code* rather than markdown, heads each contributing
/// server's block with the same thing and must spell it identically.
///
/// The rule takes **`FloatBorder`** — it *is* border drawn inside the float, so it
/// reads as one piece of chrome with the box around it rather than as a third color
/// (a thematic break inside a server's own markdown keeps [`RULE`], which is content).
/// The label then has to be an accent *distinct from the border*, so the server's name
/// stands out of the rule it is inset in: `Special`, the group nxvim already accents
/// widget chrome with (the picker prompt, the completion match).
pub const SECTION_FILL: char = '─';
pub const SECTION_LEAD: &str = "─ ";
pub const SECTION_RULE_GROUP: &str = "FloatBorder";
pub const SECTION_LABEL_GROUP: &str = "Special";

/// The line text of a section header: the label inset after [`SECTION_LEAD`], with a
/// trailing space parting it from the fill that runs on to the right edge.
pub fn section_header_line(label: &str) -> String {
    format!("{SECTION_LEAD}{label} ")
}

/// An inline highlight range on one rendered line. `start`/`end` are **byte**
/// offsets into [`Rendered::lines`]`[line]` (extmarks are byte-anchored); `group`
/// is one of the `@markup.*` names above.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdSpan {
    pub line: usize,
    pub start: usize,
    pub end: usize,
    pub group: &'static str,
}

/// A row fill: repeat `ch` from the end of [`Rendered::lines`]`[line]`'s own text to
/// the right edge of the text area, in `group`. Used for a thematic break (`---` → a
/// `─` rule, on an otherwise empty line, so it spans the float's actual width without
/// the renderer having to know it) and for a **section header** — a labelled rule
/// (`─ pyright ─────`), where the line carries the label and the fill continues past
/// it to the edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdFill {
    pub line: usize,
    pub ch: char,
    pub group: &'static str,
}

/// A fenced (or indented) code block: `len` lines of [`Rendered::lines`] starting at
/// `first_line`, to be syntax-highlighted in `lang` by the caller. `lang` is `None`
/// for an indented block or a bare ` ``` ` fence with no info string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdCode {
    pub first_line: usize,
    pub len: usize,
    pub lang: Option<String>,
}

/// The rendered markdown: stripped display lines plus the styling to paint over
/// them. See the module docs for how each field is consumed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rendered {
    pub lines: Vec<String>,
    pub spans: Vec<MdSpan>,
    pub fills: Vec<MdFill>,
    pub code: Vec<MdCode>,
}

/// Render CommonMark + GFM `src` into stripped display lines + styling. Pure and
/// infallible — malformed or unsupported markdown still yields its text. Thematic
/// breaks and table separators are emitted as [`MdFill`]s so the caller (which
/// knows the float's final width) expands them; nothing here needs a width.
pub fn render(src: &str) -> Rendered {
    let mut r = Renderer::new();
    r.feed(src);
    r.finish()
}

/// Render several **labelled sections** into one document: each `(label, markdown)`
/// pair is headed by a labelled rule — `─ pyright ────────`, the shape a float's
/// border title has — and its markdown renders directly under it, with no blank row
/// between (the rule is the separation; a gap under it would read as a detached
/// heading). One blank row parts a section from the one above — the first takes none —
/// and a section's own trailing blank rows / dangling rule are trimmed off at the
/// boundary, so that gap is exactly one row however the server's markup ended.
///
/// This is what the LSP hover uses when more than one server answers: a type-checker's
/// signature and a linter's rule explanation are different kinds of claim, so the
/// reader has to see which server made which — and the rule says it without a `#`
/// heading's markup competing with headings inside a server's own markdown. A section
/// with an **empty** label takes no rule at all — the lone contributor renders bare.
pub fn render_sections<'a>(sections: impl IntoIterator<Item = (&'a str, &'a str)>) -> Rendered {
    let mut r = Renderer::new();
    for (label, src) in sections {
        r.section_header(label);
        r.feed(src);
    }
    r.finish()
}

/// One entry of the ordered/unordered list stack: `Some(n)` is an ordered list
/// whose next item number is `n`; `None` is a bullet list.
type ListMarker = Option<u64>;

struct Renderer {
    lines: Vec<String>,
    spans: Vec<MdSpan>,
    fills: Vec<MdFill>,
    code: Vec<MdCode>,

    /// The line currently being built, and the spans closed on it so far (byte
    /// ranges into `line`, resolved to a line index at [`Self::newline`]).
    line: String,
    line_spans: Vec<(usize, usize, &'static str)>,
    /// `false` until the first content of a logical line is written, so the block
    /// prefix (indent + list marker) is emitted lazily exactly once per line.
    started: bool,

    /// Open inline styles: `(group, byte start in `line`)`. A style still open at a
    /// line break is closed at end-of-line and reopened at the next line's prefix.
    styles: Vec<(&'static str, usize)>,

    /// Active list nesting; for the item currently opening, the indent + marker to
    /// emit at its first line (kept apart so a task-list checkbox can replace the
    /// bullet while keeping the indent).
    lists: Vec<ListMarker>,
    pending_indent: String,
    pending_marker: Option<String>,
    /// Block-quote nesting depth (a `▎ ` bar per level, prefixed on each line).
    quote: usize,
    /// A pending GFM task-list checkbox glyph — replaces the bullet for this item.
    pending_task: Option<&'static str>,

    /// Set inside a fenced/indented code block: the accumulated body and its lang.
    in_code: bool,
    code_buf: String,
    code_lang: Option<String>,

    /// The href of the link currently open, appended (dimmed) after its label when
    /// it differs from the visible text.
    link_dest: Option<String>,

    /// A GFM table being accumulated: alignments, and the collected cell strings
    /// row-by-row. `None` when not inside a table.
    table: Option<Table>,

    /// A blank separator is due before the next top-level block.
    want_gap: bool,
}

/// Column alignments (`-1` left / `0` centre / `1` right) and the accumulated rows
/// of a GFM table, formatted into aligned lines when the table ends.
struct Table {
    aligns: Vec<i8>,
    rows: Vec<Vec<String>>,
    /// Cells of the row currently being read.
    row: Vec<String>,
    /// The cell currently being read (table cells capture text plainly — inline
    /// styling inside a cell is not carried through the alignment pass).
    cell: String,
    in_cell: bool,
}

impl Renderer {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            spans: Vec::new(),
            fills: Vec::new(),
            code: Vec::new(),
            line: String::new(),
            line_spans: Vec::new(),
            started: false,
            styles: Vec::new(),
            lists: Vec::new(),
            pending_indent: String::new(),
            pending_marker: None,
            quote: 0,
            pending_task: None,
            in_code: false,
            code_buf: String::new(),
            code_lang: None,
            link_dest: None,
            table: None,
            want_gap: false,
        }
    }

    /// Parse `src` and fold its events into the document built so far. Called once by
    /// [`render`], once per section by [`render_sections`] — each section parses on its
    /// own (a fence left open in one server's markup can't swallow the next).
    fn feed(&mut self, src: &str) {
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        opts.insert(Options::ENABLE_TABLES);
        opts.insert(Options::ENABLE_TASKLISTS);
        for event in Parser::new_ext(src, opts) {
            self.event(event);
        }
    }

    /// Emit a section header: a labelled rule (`─ pyright ─────`) — the label as real
    /// line text, the rest of the row filled by the [`MdFill`].
    ///
    /// Tight **below**: the content starts on the next row, so the section reads as a
    /// titled box's body. Parted **above** by one blank row (never above the first
    /// section) so the previous contributor's last line doesn't run into the next
    /// title — the previous section's own trailing blanks, and any rule its markup
    /// ended on, are trimmed off first so the gap is exactly one row. An empty label
    /// emits nothing at all — the lone contributor renders bare.
    fn section_header(&mut self, label: &str) {
        self.newline_if_dirty();
        self.trim_trailing();
        self.want_gap = false;
        if label.is_empty() {
            return;
        }
        if !self.lines.is_empty() {
            self.lines.push(String::new());
        }
        let line = self.lines.len();
        self.spans.push(MdSpan {
            line,
            start: 0,
            end: SECTION_LEAD.len(),
            group: SECTION_RULE_GROUP,
        });
        self.spans.push(MdSpan {
            line,
            start: SECTION_LEAD.len(),
            end: SECTION_LEAD.len() + label.len(),
            group: SECTION_LABEL_GROUP,
        });
        self.lines.push(section_header_line(label));
        self.fills.push(MdFill {
            line,
            ch: SECTION_FILL,
            group: SECTION_RULE_GROUP,
        });
    }

    /// Drop the trailing blank rows the block-gap logic left — and with them any rule
    /// they leave dangling (a `---` closing the markup separates nothing; see
    /// [`rule`](Self::rule), whose *leading* case this is the counterpart of). A
    /// **labelled** section rule is content — it names its contributor — and its row is
    /// never blank, so it stays. Run at the end of the document and at each section
    /// boundary, where a server's markup ends.
    fn trim_trailing(&mut self) {
        while self.lines.last().is_some_and(|l| l.trim().is_empty()) {
            self.lines.pop();
        }
        self.spans.retain(|s| s.line < self.lines.len());
        self.fills.retain(|f| f.line < self.lines.len());
    }

    fn finish(mut self) -> Rendered {
        self.newline_if_dirty();
        self.trim_trailing();
        Rendered {
            lines: self.lines,
            spans: self.spans,
            fills: self.fills,
            code: self.code,
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                if self.in_code {
                    self.code_buf.push_str(&t);
                } else if let Some(table) = self.table.as_mut() {
                    if table.in_cell {
                        table.cell.push_str(&t);
                    }
                } else {
                    self.write(&t);
                }
            }
            // Inline code: pulldown already stripped the backticks; style the text.
            Event::Code(t) => {
                if let Some(table) = self.table.as_mut() {
                    if table.in_cell {
                        table.cell.push_str(&t);
                    }
                } else {
                    self.open(RAW);
                    self.write(&t);
                    self.close(RAW);
                }
            }
            // A **soft** break (a single newline within a paragraph) reflows to a
            // space — markdown joins a paragraph's wrapped source lines into one, so
            // the doc float can re-wrap it to its own width. Only a **hard** break
            // (two trailing spaces / a backslash) or a blank line starts a new visual
            // line. Inside a code block / table this doesn't fire (code text carries
            // its own newlines), but guard anyway.
            Event::SoftBreak => {
                if !self.in_code && self.table.is_none() {
                    self.write(" ");
                }
            }
            Event::HardBreak => {
                if !self.in_code && self.table.is_none() {
                    self.newline();
                }
            }
            Event::Rule => self.rule(),
            Event::TaskListMarker(done) => {
                // Emitted right after the list item opens, before its text. Replace
                // the pending bullet with a checkbox glyph.
                self.pending_task = Some(if done { "☑ " } else { "☐ " });
            }
            // Raw/inline HTML in docs is rare; render its text so nothing is dropped.
            Event::Html(t) | Event::InlineHtml(t) => {
                if !self.in_code && self.table.is_none() {
                    self.write(t.trim_end_matches('\n'));
                }
            }
            // Math is not enabled, but the variants exist — render the source so it
            // is never silently dropped.
            Event::InlineMath(t) | Event::DisplayMath(t) => {
                if !self.in_code && self.table.is_none() {
                    self.write(&t);
                }
            }
            Event::FootnoteReference(_) => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => self.block_gap(),
            Tag::Heading { level, .. } => {
                self.block_gap();
                self.open(HEADING[heading_index(level)]);
            }
            Tag::Strong => self.open(STRONG),
            Tag::Emphasis => self.open(ITALIC),
            Tag::Strikethrough => self.open(STRIKE),
            Tag::CodeBlock(kind) => {
                self.block_gap();
                self.in_code = true;
                self.code_buf.clear();
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    CodeBlockKind::Indented => None,
                };
            }
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.block_gap();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.newline_if_dirty();
                let depth = self.lists.len().saturating_sub(1);
                self.pending_indent = "  ".repeat(depth);
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                self.pending_marker = Some(marker);
            }
            Tag::BlockQuote(_) => {
                self.block_gap();
                self.quote += 1;
            }
            Tag::Link { dest_url, .. } => {
                self.link_dest = Some(dest_url.to_string());
                self.open(LINK_LABEL);
            }
            // Images render as their alt text (styled like a link label).
            Tag::Image { .. } => self.open(LINK_LABEL),
            Tag::Table(aligns) => {
                self.block_gap();
                self.table = Some(Table {
                    aligns: aligns.iter().map(alignment_code).collect(),
                    rows: Vec::new(),
                    row: Vec::new(),
                    cell: String::new(),
                    in_cell: false,
                });
            }
            Tag::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    t.cell.clear();
                    t.in_cell = true;
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.newline();
                self.want_gap = true;
            }
            TagEnd::Heading(level) => {
                self.close(HEADING[heading_index(level)]);
                self.newline();
                self.want_gap = true;
            }
            TagEnd::Strong => self.close(STRONG),
            TagEnd::Emphasis => self.close(ITALIC),
            TagEnd::Strikethrough => self.close(STRIKE),
            TagEnd::CodeBlock => {
                self.flush_code();
                self.want_gap = true;
            }
            TagEnd::List(_) => {
                self.lists.pop();
                if self.lists.is_empty() {
                    self.want_gap = true;
                }
            }
            TagEnd::Item => {
                self.newline_if_dirty();
                // An item that produced no line still consumes its marker.
                self.pending_marker = None;
                self.pending_task = None;
            }
            TagEnd::BlockQuote(_) => {
                self.quote = self.quote.saturating_sub(1);
                self.want_gap = true;
            }
            TagEnd::Link => {
                self.close(LINK_LABEL);
                if let Some(dest) = self.link_dest.take() {
                    self.append_link_url(&dest);
                }
            }
            TagEnd::Image => self.close(LINK_LABEL),
            TagEnd::Table => self.flush_table(),
            TagEnd::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    t.in_cell = false;
                    let cell = std::mem::take(&mut t.cell);
                    t.row.push(cell.trim().to_string());
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    let row = std::mem::take(&mut t.row);
                    t.rows.push(row);
                }
            }
            _ => {}
        }
    }

    // --- line building ------------------------------------------------------

    /// Emit the block prefix (quote bars, list indent + marker) for the current
    /// line the first time content is written to it, and anchor open styles past it.
    fn ensure_prefix(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        // Block-quote bars, styled so the quote reads as an aside.
        for _ in 0..self.quote {
            let start = self.line.len();
            self.line.push_str("▎ ");
            self.line_spans.push((start, self.line.len(), QUOTE));
        }
        // A list item's first line: indent, then either a task-list checkbox (which
        // replaces the bullet) or the bullet / ordinal, styled as list punctuation.
        if let Some(marker) = self.pending_marker.take() {
            self.line
                .push_str(&std::mem::take(&mut self.pending_indent));
            let start = self.line.len();
            match self.pending_task.take() {
                Some(glyph) => self.line.push_str(glyph),
                None => self.line.push_str(&marker),
            }
            self.line_spans.push((start, self.line.len(), LIST));
        }
        // Open styles begin after the prefix (a reopened style from a wrapped
        // line, or a style opened before any text on this line).
        let at = self.line.len();
        for (_, start) in self.styles.iter_mut() {
            *start = at;
        }
    }

    /// Append `text` to the current line, honoring embedded newlines as line breaks.
    fn write(&mut self, text: &str) {
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                self.newline();
            }
            if part.is_empty() {
                continue;
            }
            self.ensure_prefix();
            self.line.push_str(part);
        }
    }

    /// Flush the current line to the output, closing (and remembering to reopen)
    /// any styles that span the break.
    fn newline(&mut self) {
        let end = self.line.len();
        for (group, start) in self.styles.iter() {
            if *start < end {
                self.line_spans.push((*start, end, group));
            }
        }
        let idx = self.lines.len();
        for (s, e, g) in self.line_spans.drain(..) {
            if s < e {
                self.spans.push(MdSpan {
                    line: idx,
                    start: s,
                    end: e,
                    group: g,
                });
            }
        }
        self.lines.push(std::mem::take(&mut self.line));
        self.started = false;
    }

    /// Whether the document holds anything a rule could separate *from* what follows:
    /// some content that is not itself a rule / section-header row. Blank rows and
    /// filled rows don't count, so a leading `---`, and a `---` right under another
    /// rule or a section header, have nothing above them.
    fn has_separable_content(&self) -> bool {
        for (i, line) in self.lines.iter().enumerate().rev() {
            if self.fills.iter().any(|f| f.line == i) {
                return false; // a rule / labelled section rule — not content
            }
            if !line.trim().is_empty() {
                return true;
            }
        }
        // A partial line in progress counts (its block hasn't been flushed yet).
        !self.line.trim().is_empty()
    }

    /// Flush only if a partial line is in progress (no empty line otherwise).
    fn newline_if_dirty(&mut self) {
        if self.started || !self.line.is_empty() {
            self.newline();
        }
    }

    /// Open a blank separator before a top-level block (never between list items or
    /// inside a quote/list, which read tighter).
    fn block_gap(&mut self) {
        self.newline_if_dirty();
        if self.want_gap {
            self.want_gap = false;
            if self.lines.last().is_some_and(|l| !l.trim().is_empty()) {
                self.lines.push(String::new());
            }
        }
    }

    fn open(&mut self, group: &'static str) {
        self.ensure_prefix();
        let at = self.line.len();
        self.styles.push((group, at));
    }

    fn close(&mut self, group: &'static str) {
        if let Some(pos) = self.styles.iter().rposition(|(g, _)| *g == group) {
            let (g, start) = self.styles.remove(pos);
            let end = self.line.len();
            if start < end {
                self.line_spans.push((start, end, g));
            }
        }
    }

    /// Append a link's URL after its label when it adds information (skip when the
    /// label already *is* the URL, as for a bare autolink).
    fn append_link_url(&mut self, dest: &str) {
        if dest.is_empty() || self.line.ends_with(dest) {
            return;
        }
        self.write(" (");
        self.open(LINK_URL);
        self.write(dest);
        self.close(LINK_URL);
        self.write(")");
    }

    fn flush_code(&mut self) {
        self.in_code = false;
        let body = std::mem::take(&mut self.code_buf);
        // Drop the single trailing newline pulldown appends to a fenced block.
        let body = body.strip_suffix('\n').unwrap_or(&body);
        let first_line = self.lines.len();
        let mut len = 0;
        for line in body.split('\n') {
            self.lines.push(line.to_string());
            len += 1;
        }
        self.code.push(MdCode {
            first_line,
            len,
            lang: self.code_lang.take(),
        });
    }

    /// A thematic break (`---`) — a full-width `─` rule on its own row, **tight**: no
    /// blank row above or below it. The rule already separates the blocks it sits
    /// between; padding it with gaps costs three rows for one boundary, and a hover
    /// float is small (a server that heads its docs with `<signature>\n---\n<prose>`
    /// wastes a third of a short popup on whitespace).
    ///
    /// A rule that separates **nothing** is dropped: one opening the document, or one
    /// following another rule / a section header. LSP servers emit their markup by
    /// template — `<signature>\n---\n<docs>` — so an item with no docs arrives as a
    /// bare trailing rule, and one with no signature as a leading one; drawing that
    /// boundary line promises a section that isn't there. (The trailing case falls out
    /// of [`finish`](Self::finish), which pops the rule's own empty row.)
    fn rule(&mut self) {
        if !self.has_separable_content() {
            return;
        }
        self.newline_if_dirty();
        let idx = self.lines.len();
        self.lines.push(String::new());
        self.fills.push(MdFill {
            line: idx,
            ch: '─',
            group: RULE,
        });
        self.want_gap = false;
    }

    /// Format the accumulated GFM table into aligned, padded lines with a header
    /// separator, each cell styled `@markup.raw` so it reads as tabular data.
    fn flush_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        if table.rows.is_empty() {
            return;
        }
        let cols = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0usize; cols];
        for row in &table.rows {
            for (c, cell) in row.iter().enumerate() {
                widths[c] = widths[c].max(cell.chars().count());
            }
        }
        let align = |c: usize| table.aligns.get(c).copied().unwrap_or(-1);
        for (r, row) in table.rows.iter().enumerate() {
            let mut line = String::new();
            for (c, &w) in widths.iter().enumerate() {
                if c > 0 {
                    line.push_str("  ");
                }
                let cell = row.get(c).map(String::as_str).unwrap_or("");
                pad_cell(&mut line, cell, w, align(c));
            }
            let idx = self.lines.len();
            let text = line.trim_end().to_string();
            let end = text.len();
            self.lines.push(text);
            self.spans.push(MdSpan {
                line: idx,
                start: 0,
                end,
                group: RAW,
            });
            // A `─` separator row under the header.
            if r == 0 {
                let sep_idx = self.lines.len();
                self.lines.push(String::new());
                self.fills.push(MdFill {
                    line: sep_idx,
                    ch: '─',
                    group: QUOTE,
                });
            }
        }
        self.want_gap = true;
    }
}

/// Left/centre/right pad `cell` to `width` display columns, appending to `line`.
fn pad_cell(line: &mut String, cell: &str, width: usize, align: i8) {
    let pad = width.saturating_sub(cell.chars().count());
    let (left, right) = match align {
        1 => (pad, 0),                 // right
        0 => (pad / 2, pad - pad / 2), // centre
        _ => (0, pad),                 // left (default)
    };
    for _ in 0..left {
        line.push(' ');
    }
    line.push_str(cell);
    for _ in 0..right {
        line.push(' ');
    }
}

fn heading_index(level: HeadingLevel) -> usize {
    (level as usize).clamp(1, 6) - 1
}

fn alignment_code(a: &pulldown_cmark::Alignment) -> i8 {
    use pulldown_cmark::Alignment::*;
    match a {
        Left => -1,
        Center => 0,
        Right => 1,
        None => -1,
    }
}
