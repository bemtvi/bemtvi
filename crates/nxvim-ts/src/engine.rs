//! The incremental parse + highlight engine.
//!
//! Per buffer the engine keeps a **shadow rope** and a persistent **parse tree**.
//! Edits arrive as deltas: the shadow is patched in place, the old tree is
//! `edit`ed and reparsed **incrementally**, so per-edit cost scales with the edit
//! — not the file. Highlights are extracted by running the grammar's query over
//! just the requested line range.

use std::collections::{HashMap, HashSet};
use std::ops::{ControlFlow, Range};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use nxvim_core::{BufferEdit, BufferId, IndentParams, OpenOutcome, Span, SyntaxEngine};
use ropey::{LineType, Rope};
use streaming_iterator::StreamingIterator;
use tree_sitter::{InputEdit, Node, ParseOptions, Parser, Point, Query, QueryCursor, Tree};

use crate::loader::{query_path, Grammar, LoadError, QueryOverrides};

const LINE_TYPE: LineType = LineType::LF_CR;

/// Wall-clock budget for a single (incremental) parse. In-process, a runaway or
/// pathological grammar would otherwise stall the editor on the frame that
/// triggered the reparse; the worker process used to bound this by being async.
/// On expiry the parse is cancelled and the last good tree is kept (see
/// [`BufferState::reparse`]), so the cost is one frame of stale highlights rather
/// than a hang. Generous enough that a normal incremental reparse never trips it.
const PARSE_DEADLINE: Duration = Duration::from_millis(50);

/// Per-buffer parse state.
struct BufferState {
    shadow: Rope,
    parser: Parser,
    tree: Option<Tree>,
    language: String,
}

impl BufferState {
    /// Reparse from the shadow, reusing the old tree when present (incremental).
    fn reparse(&mut self) {
        let shadow = &self.shadow;
        let mut callback = |byte: usize, _: Point| -> &[u8] { read_chunk(shadow, byte) };
        // Cancel the parse once it has run longer than the deadline — the
        // in-process replacement for the worker's "never stalls the UI" property.
        let started = Instant::now();
        let mut budget = |_: &tree_sitter::ParseState| -> ControlFlow<()> {
            if started.elapsed() >= PARSE_DEADLINE {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let options = ParseOptions::new().progress_callback(&mut budget);
        // Keep the last good tree if the parse yields `None` (the deadline fired):
        // overwriting it with `None` would throw away all incremental reuse and
        // leave the buffer un-highlightable until a full re-open. So a cancelled
        // parse costs one frame of stale highlights, not a permanently dark buffer.
        if let Some(tree) =
            self.parser
                .parse_with_options(&mut callback, self.tree.as_ref(), Some(options))
        {
            self.tree = Some(tree);
        }
    }
}

/// A cached grammar-load result for a language. Remembers *why* a grammar is
/// absent so the editor can stay silent for an uninstalled one but echo a real
/// load failure. Cached on first use, so the dlopen (and its outcome) happen
/// once per language, not once per keystroke.
enum Slot {
    Loaded(Grammar),
    /// No parser installed — silent.
    NotInstalled,
    /// Installed but broken; the reason to echo.
    Failed(String),
}

/// Owns every buffer's parse state and a lazily-populated grammar cache.
pub struct Engine {
    data_dir: PathBuf,
    grammars: HashMap<String, Slot>,
    buffers: HashMap<BufferId, BufferState>,
    /// Query-text overrides from the resolution bridge, consulted by
    /// [`Grammar::load`] and applied in place by [`Engine::set_query`].
    query_overrides: QueryOverrides,
}

impl Engine {
    pub fn new(data_dir: PathBuf) -> Self {
        Engine {
            data_dir,
            grammars: HashMap::new(),
            buffers: HashMap::new(),
            query_overrides: QueryOverrides::new(),
        }
    }

    /// Lazily load (and cache) the grammar for `lang`, returning its cache slot.
    /// The load — and its outcome (loaded / not-installed / failed) — happens once
    /// per language; later calls are a cache hit.
    fn grammar(&mut self, lang: &str) -> &Slot {
        if !self.grammars.contains_key(lang) {
            let slot = match Grammar::load(&self.data_dir, lang, &self.query_overrides) {
                Ok(g) => Slot::Loaded(g),
                Err(LoadError::NotInstalled) => Slot::NotInstalled,
                Err(LoadError::Failed(e)) => Slot::Failed(format!("{e:#}")),
            };
            self.grammars.insert(lang.to_string(), slot);
        }
        &self.grammars[lang]
    }

    /// Install (or, with `text = None`, clear) a resolved query override for
    /// `(lang, name)` — the engine half of the query-resolution bridge. Lua has
    /// already merged `query.set` / `after/queries` / `;extends` into the final
    /// `text`; here the engine compiles + caches it, consulting it in place of the
    /// on-disk query. Only the paint-driving names `highlights` / `indents` reach
    /// the engine; any other name is a no-op (folds/injections stay Lua-side).
    ///
    /// If the grammar is already loaded, the affected query is recompiled **in
    /// place** against the live `Language` — never by evicting the grammar, whose
    /// library must outlive the `Language` every open buffer's parser holds. A
    /// compile failure is returned (the editor echoes it loud) and the previous
    /// compiled query is left untouched, so a bad override degrades to "no change"
    /// rather than a dark buffer.
    pub fn set_query(
        &mut self,
        lang: &str,
        name: &str,
        text: Option<String>,
    ) -> Result<(), String> {
        if name != "highlights" && name != "indents" {
            return Ok(());
        }
        let key = (lang.to_string(), name.to_string());
        match &text {
            Some(t) => {
                self.query_overrides.insert(key, t.clone());
            }
            None => {
                self.query_overrides.remove(&key);
            }
        }
        self.recompile_query(lang, name, text)
    }

    /// Install a resolved on-disk overlay only when it differs from the base file
    /// the engine would read off disk — the buffer-open half of the query bridge
    /// (a pure `after/queries` / `;extends` merge, with no explicit `query.set`).
    /// When `text` matches the disk file (a language with no customization), the
    /// override is *cleared* so the engine stays on the byte-identical disk path.
    /// Delegates to [`Self::set_query`] for the actual compile + in-place recompile.
    pub fn set_query_overlay(
        &mut self,
        lang: &str,
        name: &str,
        text: Option<String>,
    ) -> Result<(), String> {
        if name != "highlights" && name != "indents" {
            return Ok(());
        }
        // The base file content (None when absent). A resolved overlay equal to it
        // carries no customization, so we drop the override and read disk instead.
        let disk = self.read_disk_query(lang, name)?;
        let effective = match text {
            Some(t) if Some(&t) != disk.as_ref() => Some(t),
            // Equal to disk, or nothing resolved: no override → disk path.
            _ => None,
        };
        self.set_query(lang, name, effective)
    }

    /// Read the on-disk `<name>.scm` for `lang`, returning `None` when absent. The
    /// base content [`set_query`](Self::set_query) reverts to on clear and that
    /// [`set_query_overlay`](Self::set_query_overlay) compares a resolved overlay
    /// against.
    fn read_disk_query(&self, lang: &str, name: &str) -> Result<Option<String>, String> {
        let path = query_path(&self.data_dir, lang, &format!("{name}.scm"));
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("reading {}: {e}", path.display())),
        }
    }

    /// Recompile the affected query in place against the already-loaded `Language`,
    /// or do nothing if the grammar isn't loaded yet (the next `Grammar::load`
    /// picks up the override map). `text` is the override just stored (`None` on a
    /// clear, where the source reverts to the on-disk file). Shared by both query
    /// entry points so the in-place recompile lives in one place.
    fn recompile_query(
        &mut self,
        lang: &str,
        name: &str,
        text: Option<String>,
    ) -> Result<(), String> {
        // The source for the affected query: the override text, or — on clear —
        // the on-disk file (absent indents file means "no indent query"). Read
        // before the grammar borrow so `&self.read_disk_query` can't collide.
        let src = match text {
            Some(t) => Some(t),
            None => self.read_disk_query(lang, name)?,
        };
        // Recompile in place only if the grammar is already loaded; otherwise the
        // override is picked up by the next `Grammar::load`.
        let Some(Slot::Loaded(g)) = self.grammars.get_mut(lang) else {
            return Ok(());
        };
        match name {
            "highlights" => {
                let s = src.ok_or_else(|| format!("no highlights query on disk for '{lang}'"))?;
                g.query = Query::new(&g.language, &s)
                    .map_err(|e| format!("compiling {lang} highlights: {e}"))?;
            }
            "indents" => {
                g.indents = match src {
                    Some(s) => Some(
                        Query::new(&g.language, &s)
                            .map_err(|e| format!("compiling {lang} indents: {e}"))?,
                    ),
                    None => None,
                };
            }
            _ => unreachable!("guarded above"),
        }
        Ok(())
    }

    /// (Re)initialize a buffer from full text and do the initial parse. The
    /// [`OpenOutcome`] reports whether an *installed* grammar failed to load
    /// (worth echoing) vs the silent no-grammar / parsed-fine cases.
    pub fn open(&mut self, buffer: BufferId, lang: &str, text: &str) -> OpenOutcome {
        let language = match self.grammar(lang) {
            Slot::Loaded(g) => g.language.clone(),
            Slot::NotInstalled => return OpenOutcome::Ok, // silent: best-effort
            Slot::Failed(reason) => return OpenOutcome::LoadFailed(reason.clone()),
        };
        let mut parser = Parser::new();
        if let Err(e) = parser.set_language(&language) {
            // Unreachable in practice (the ABI is probed at load), but report it
            // honestly rather than silently dropping the buffer.
            return OpenOutcome::LoadFailed(format!("set_language: {e}"));
        }
        let mut state = BufferState {
            shadow: Rope::from_str(text),
            parser,
            tree: None,
            language: lang.to_string(),
        };
        state.reparse();
        self.buffers.insert(buffer, state);
        OpenOutcome::Ok
    }

    /// Apply edit deltas to a buffer's shadow + tree, then reparse incrementally.
    pub fn edit(&mut self, buffer: BufferId, edits: &[BufferEdit]) {
        let Some(state) = self.buffers.get_mut(&buffer) else {
            return; // never opened; the editor opens before editing
        };
        for e in edits {
            // Defend the shadow against a bad delta: an out-of-range, mis-ordered,
            // or mid-codepoint range would panic ropey and leave the shadow and
            // tree half-mutated, poisoning the buffer for every later edit (and,
            // in-process, taking the editor down). Validate against the live shadow
            // and drop a delta that doesn't fit rather than trust it. `try_*` is a
            // second guard so a mutation can still never panic.
            let len = state.shadow.len();
            let valid = e.start_byte <= e.old_end_byte
                && e.old_end_byte <= len
                && state.shadow.is_char_boundary(e.start_byte)
                && state.shadow.is_char_boundary(e.old_end_byte);
            if !valid {
                continue;
            }
            // Patch the shadow: remove the old range, insert the new bytes.
            if e.old_end_byte > e.start_byte
                && state
                    .shadow
                    .try_remove(e.start_byte..e.old_end_byte)
                    .is_err()
            {
                continue;
            }
            if !e.text.is_empty() && state.shadow.try_insert(e.start_byte, &e.text).is_err() {
                continue;
            }
            if let Some(tree) = state.tree.as_mut() {
                tree.edit(&InputEdit {
                    start_byte: e.start_byte,
                    old_end_byte: e.old_end_byte,
                    new_end_byte: e.new_end_byte,
                    start_position: point(e.start_point),
                    old_end_position: point(e.old_end_point),
                    new_end_position: point(e.new_end_point),
                });
            }
        }
        state.reparse();
    }

    /// Forget a buffer's shadow text and parse tree (the editor deleted it).
    pub fn close(&mut self, buffer: BufferId) {
        self.buffers.remove(&buffer);
    }

    /// Whether a buffer is known (opened) and which language it uses.
    pub fn language_of(&self, buffer: BufferId) -> Option<&str> {
        self.buffers.get(&buffer).map(|b| b.language.as_str())
    }

    /// Extract highlight spans for the visible line range `[first_line, last_line)`.
    pub fn highlights(
        &mut self,
        buffer: BufferId,
        first_line: usize,
        last_line: usize,
    ) -> Vec<Span> {
        let Some(state) = self.buffers.get(&buffer) else {
            return Vec::new();
        };
        let Some(tree) = state.tree.as_ref() else {
            return Vec::new();
        };
        let Some(Slot::Loaded(grammar)) = self.grammars.get(&state.language) else {
            return Vec::new();
        };
        extract_spans(grammar, tree, &state.shadow, first_line, last_line)
    }

    /// Target indent **width in columns** for the 0-indexed `line`, by running the
    /// grammar's `indents.scm` over the tree — a faithful port of
    /// nvim-treesitter's `indent.lua` `get_indent`. Returns `None` when there is
    /// no grammar, no indent query, or the query is inconclusive (an `@indent.auto`
    /// node or an unselectable node), so the editor falls back. Column 0 is a real
    /// verdict (`@indent.zero` / inside an `@indent.ignore` block), returned as
    /// `Some(0)`.
    pub fn indent(&mut self, buffer: BufferId, line: usize, p: &IndentParams) -> Option<usize> {
        let state = self.buffers.get(&buffer)?;
        let tree = state.tree.as_ref()?;
        let Some(Slot::Loaded(grammar)) = self.grammars.get(&state.language) else {
            return None;
        };
        let query = grammar.indents.as_ref()?;
        let rope = &state.shadow;
        let root = tree.root_node();
        let maps = build_indent_maps(query, &root, rope);
        let indent_size = p.shiftwidth as i64;

        let line_count = rope.len_lines(LINE_TYPE).saturating_sub(1);

        // --- pick the node whose ancestry decides this line's indent ----------
        // For an empty line (the o/O/Enter case) we reason from the *previous*
        // non-blank line's last node; for a non-empty line, from its first node.
        let is_empty = line >= line_count || line_text(rope, line).trim().is_empty();
        let node = if is_empty {
            let prev = prevnonblank(rope, line.min(line_count.saturating_sub(1)))?;
            let indentcols = leading_ws(rope, prev);
            let prevline = line_text(rope, prev);
            let prevline = prevline.trim();
            let col = indentcols + prevline.len().saturating_sub(1);
            let mut n = node_at(&root, prev, col)?;
            // A trailing comment on the previous line must not drive the indent —
            // re-pick the last node of the code that precedes it.
            if n.kind().contains("comment") {
                let first = node_at(&root, prev, indentcols)?;
                if first.id() != n.id() {
                    let scol = n.start_position().column;
                    let cut = scol.saturating_sub(indentcols).min(prevline.len());
                    if prevline.is_char_boundary(cut) {
                        let pre = prevline[..cut].trim_end();
                        let col = indentcols + pre.len().saturating_sub(1);
                        n = node_at(&root, prev, col)?;
                    }
                }
            }
            // If that last node *closes* a block (`@indent.end`), the new line sits
            // outside it, so decide from the new line's own (first) node instead.
            if maps.end.contains(&n.id()) {
                node_at(&root, line, leading_ws(rope, line))?
            } else {
                n
            }
        } else {
            node_at(&root, line, leading_ws(rope, line))?
        };

        if maps.zero.contains(&node.id()) {
            return Some(0);
        }

        // --- accumulate indent by walking ancestors ---------------------------
        // `processed` holds start-rows already credited a level, so a line with
        // several openers nested on it only indents once (nvim-treesitter's
        // `is_processed_by_row`).
        let mut indent: i64 = 0;
        let mut processed: HashSet<usize> = HashSet::new();
        let mut cur = Some(node);
        while let Some(n) = cur {
            let nid = n.id();
            let srow = n.start_position().row;
            let erow = n.end_position().row;

            // `@indent.auto` (e.g. inside a raw string): defer to the editor's
            // fallback rather than guess. Lua returns -1; we return None.
            if !maps.begin.contains_key(&nid)
                && !maps.align.contains(&nid)
                && maps.auto.contains(&nid)
                && srow < line
                && line <= erow
            {
                return None;
            }
            // `@indent.ignore` block (e.g. inside a block comment): force column 0.
            if !maps.begin.contains_key(&nid)
                && maps.ignore.contains(&nid)
                && srow < line
                && line <= erow
            {
                return Some(0);
            }

            let row_done = processed.contains(&srow);
            let mut is_processed = false;

            // Branch (`else`/`}` opening row) and dedent close a level.
            if !row_done
                && ((maps.branch.contains(&nid) && srow == line)
                    || (maps.dedent.contains(&nid) && srow != line))
            {
                indent -= indent_size;
                is_processed = true;
            }

            // A node in an ERROR parent is treated as if it spanned multiple lines,
            // so a half-typed opener still indents (matches nvim-treesitter).
            let is_in_err = !row_done && n.parent().is_some_and(|pr| pr.has_error());

            if !row_done {
                if let Some(meta) = maps.begin.get(&nid) {
                    if (srow != erow || is_in_err || meta.immediate)
                        && (srow != line || meta.start_at_same_line)
                    {
                        indent += indent_size;
                        is_processed = true;
                    }
                }
            }

            // `@indent.align` (delimiter alignment) is a documented v2 follow-up;
            // its nodes are still collected above so the auto/ignore guards stay
            // correct, but the alignment math is not applied. The rust query (and
            // the core captures this v1 targets) does not use it.

            if is_processed {
                processed.insert(srow);
            }
            cur = n.parent();
        }

        Some(indent.max(0) as usize)
    }
}

/// One indent capture's `#set!` directives that the algorithm consults.
#[derive(Default, Clone, Copy)]
struct BeginMeta {
    /// `(#set! indent.immediate)` — indent even for a node that opens and closes
    /// on the same line.
    immediate: bool,
    /// `(#set! indent.start_at_same_line)` — indent even when the node starts on
    /// the target line.
    start_at_same_line: bool,
}

/// Captured node ids by indent role, built once per `indent()` call by running the
/// `indents.scm` query over the whole tree (ancestors anywhere can carry a
/// capture, so the query is not range-limited). Mirrors nvim-treesitter's `q`.
#[derive(Default)]
struct IndentMaps {
    begin: HashMap<usize, BeginMeta>,
    end: HashSet<usize>,
    dedent: HashSet<usize>,
    branch: HashSet<usize>,
    ignore: HashSet<usize>,
    align: HashSet<usize>,
    auto: HashSet<usize>,
    zero: HashSet<usize>,
}

fn build_indent_maps(query: &Query, root: &Node, rope: &Rope) -> IndentMaps {
    let mut maps = IndentMaps::default();
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let provider =
        |node: Node| std::iter::once(node_bytes(rope, node.start_byte()..node.end_byte()));
    let mut caps = cursor.captures(query, *root, provider);
    while let Some((m, idx)) = caps.next() {
        let cap = m.captures[*idx];
        let name = names[cap.index as usize];
        if name.starts_with('_') {
            continue; // internal/predicate capture, not an indent role
        }
        let id = cap.node.id();
        match name {
            "indent.begin" => {
                let mut meta = BeginMeta::default();
                for prop in query.property_settings(m.pattern_index) {
                    match &*prop.key {
                        "indent.immediate" => meta.immediate = true,
                        "indent.start_at_same_line" => meta.start_at_same_line = true,
                        _ => {}
                    }
                }
                maps.begin.insert(id, meta);
            }
            "indent.end" => {
                maps.end.insert(id);
            }
            "indent.dedent" => {
                maps.dedent.insert(id);
            }
            "indent.branch" => {
                maps.branch.insert(id);
            }
            "indent.ignore" => {
                maps.ignore.insert(id);
            }
            "indent.align" => {
                maps.align.insert(id);
            }
            "indent.auto" => {
                maps.auto.insert(id);
            }
            "indent.zero" => {
                maps.zero.insert(id);
            }
            _ => {}
        }
    }
    maps
}

/// The smallest node covering one byte column on a line — nvim-treesitter's
/// `descendant_for_range(row, col, row, col+1)`.
fn node_at<'t>(root: &Node<'t>, row: usize, col: usize) -> Option<Node<'t>> {
    root.descendant_for_point_range(Point::new(row, col), Point::new(row, col + 1))
}

/// The 0-indexed row of the nearest non-blank line at or above `start`.
fn prevnonblank(rope: &Rope, start: usize) -> Option<usize> {
    (0..=start)
        .rev()
        .find(|&r| !line_text(rope, r).trim().is_empty())
}

/// Line `row`'s text (with its trailing newline, which callers `trim`).
fn line_text(rope: &Rope, row: usize) -> String {
    rope.line(row, LINE_TYPE).to_string()
}

/// Leading-whitespace byte count of line `row` (its indent in bytes).
fn leading_ws(rope: &Rope, row: usize) -> usize {
    line_text(rope, row)
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count()
}

/// The synchronous backend the editor owns (`nxvim-core`'s [`SyntaxEngine`]).
/// Every method delegates to the inherent ones; `indent` runs the ported
/// nvim-treesitter algorithm and returns `None` for the honest fallback cases
/// (no grammar / no `indents.scm` / `@indent.auto`), where the editor uses
/// copy-previous-line autoindent, then column 0.
impl SyntaxEngine for Engine {
    fn open(&mut self, buffer: BufferId, language: &str, text: &str) -> OpenOutcome {
        Engine::open(self, buffer, language, text)
    }

    fn edit(&mut self, buffer: BufferId, edits: &[BufferEdit]) {
        Engine::edit(self, buffer, edits);
    }

    fn close(&mut self, buffer: BufferId) {
        Engine::close(self, buffer);
    }

    fn highlights(&mut self, buffer: BufferId, first: usize, last: usize) -> Vec<Span> {
        Engine::highlights(self, buffer, first, last)
    }

    fn indent(&mut self, buffer: BufferId, line: usize, p: &IndentParams) -> Option<usize> {
        Engine::indent(self, buffer, line, p)
    }

    fn indents_available(&self, buffer: BufferId) -> bool {
        let Some(state) = self.buffers.get(&buffer) else {
            return false;
        };
        matches!(
            self.grammars.get(&state.language),
            Some(Slot::Loaded(g)) if g.indents.is_some()
        )
    }

    fn set_query(&mut self, lang: &str, name: &str, text: Option<String>) -> Result<(), String> {
        Engine::set_query(self, lang, name, text)
    }

    fn set_query_overlay(
        &mut self,
        lang: &str,
        name: &str,
        text: Option<String>,
    ) -> Result<(), String> {
        Engine::set_query_overlay(self, lang, name, text)
    }
}

/// Run the highlights query over the byte range covering the visible lines and
/// resolve the captures into per-line byte spans (most-specific capture wins).
fn extract_spans(
    grammar: &Grammar,
    tree: &Tree,
    rope: &Rope,
    first_line: usize,
    last_line: usize,
) -> Vec<Span> {
    let line_count = rope.len_lines(LINE_TYPE).saturating_sub(1);
    let last_line = last_line.min(line_count);
    if first_line >= last_line {
        return Vec::new();
    }
    let lo = rope.line_to_byte_idx(first_line, LINE_TYPE);
    let hi = rope.line_to_byte_idx(last_line, LINE_TYPE);

    let query = &grammar.query;
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(lo..hi);

    // Collect captures intersecting the viewport as (start, end, group).
    let mut raw: Vec<(usize, usize, &str)> = Vec::new();
    let provider =
        |node: Node| std::iter::once(node_bytes(rope, node.start_byte()..node.end_byte()));
    let mut caps = cursor.captures(query, tree.root_node(), provider);
    while let Some((m, idx)) = caps.next() {
        let cap = m.captures[*idx];
        let name = names[cap.index as usize];
        if name.starts_with('_') {
            continue; // internal/predicate capture, not a highlight group
        }
        let (s, e) = (cap.node.start_byte(), cap.node.end_byte());
        if e > s {
            raw.push((s, e, name));
        }
    }
    drop(caps);

    // Broadest spans first so narrower (more specific) captures overwrite them.
    raw.sort_by_key(|(s, e, _)| (std::cmp::Reverse(e - s), *s));

    let mut out = Vec::new();
    for line in first_line..last_line {
        let line_start = rope.line_to_byte_idx(line, LINE_TYPE);
        let text = rope.line(line, LINE_TYPE).to_string();
        let content_len = text.trim_end_matches(['\n', '\r']).len();
        if content_len == 0 {
            continue;
        }
        let mut groups: Vec<Option<&str>> = vec![None; content_len];
        for &(s, e, name) in &raw {
            if e <= line_start || s >= line_start + content_len {
                continue;
            }
            let cs = s.saturating_sub(line_start).min(content_len);
            let ce = (e - line_start).min(content_len);
            if cs < ce {
                for slot in &mut groups[cs..ce] {
                    *slot = Some(name);
                }
            }
        }
        // Coalesce runs of the same group into spans.
        let mut i = 0;
        while i < content_len {
            match groups[i] {
                Some(g) => {
                    let start = i;
                    while i < content_len && groups[i] == Some(g) {
                        i += 1;
                    }
                    out.push(Span {
                        line,
                        start_byte: start,
                        end_byte: i,
                        group: g.to_string(),
                    });
                }
                None => i += 1,
            }
        }
    }
    out
}

/// Bytes of `rope[range]`, walking chunks (no whole-buffer materialization).
fn node_bytes(rope: &Rope, range: Range<usize>) -> Vec<u8> {
    let mut out = Vec::with_capacity(range.len());
    let mut b = range.start;
    while b < range.end {
        let (chunk, start) = rope.chunk(b);
        if chunk.is_empty() {
            break;
        }
        let from = b - start;
        let to = (range.end - start).min(chunk.len());
        out.extend_from_slice(&chunk.as_bytes()[from..to]);
        b = start + chunk.len();
    }
    out
}

/// The chunk of `rope` starting at byte `byte` (for tree-sitter's read callback).
fn read_chunk(rope: &Rope, byte: usize) -> &[u8] {
    if byte >= rope.len() {
        return &[];
    }
    let (chunk, start) = rope.chunk(byte);
    &chunk.as_bytes()[byte - start..]
}

fn point((row, col): (usize, usize)) -> Point {
    Point::new(row, col)
}
