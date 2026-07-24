//! The incremental parse + highlight engine.
//!
//! Per buffer the engine keeps a **shadow rope** and a persistent **parse tree**.
//! Edits arrive as deltas: the shadow is patched in place, the old tree is
//! `edit`ed and reparsed **incrementally**, so per-edit cost scales with the edit
//! — not the file. Highlights are extracted by running the grammar's query over
//! just the requested line range.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::{ControlFlow, Range};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nxvim_core::{BufferEdit, BufferId, FoldRange, IndentParams, OpenOutcome, Span, SyntaxEngine};
use ropey::{LineType, Rope};
use streaming_iterator::StreamingIterator;
use tree_sitter::{
    InputEdit, Node, ParseOptions, Parser, Point, Query, QueryCursor, QueryMatch,
    QueryPredicateArg, Tree,
};

use crate::loader::{compile_query, query_path, Grammar, LoadError, QueryOverrides};

const LINE_TYPE: LineType = LineType::LF_CR;

/// Wall-clock budget for a single (incremental) parse. In-process, a runaway or
/// pathological grammar would otherwise stall the editor on the frame that
/// triggered the reparse; the worker process used to bound this by being async.
/// On expiry the parse is cancelled and the last good tree is kept (see
/// [`BufferState::reparse`]), so the cost is one frame of stale highlights rather
/// than a hang. Generous enough that a normal incremental reparse never trips it.
const PARSE_DEADLINE: Duration = Duration::from_millis(50);

/// Wall-clock budget for **all** of a buffer's child (injection) parses on one
/// refresh, the injection analogue of [`PARSE_DEADLINE`]. Injected regions reparse
/// per edit, so an adversarial config (many regions, or a pathological child
/// grammar) could otherwise stall the edit path. On expiry the remaining child
/// parses are cancelled and their last-good (edit-shifted) trees are kept, so the
/// cost is one frame of stale injected highlights rather than a hang.
const INJECTION_DEADLINE: Duration = Duration::from_millis(50);

/// How deep injected layers may nest (host → injected → injected-within-injected →
/// …). Markdown → rust → regex is two levels; real configs rarely exceed three.
/// The bound caps a pathological or cyclic config (e.g. a self-injection that keeps
/// finding regions) from building unbounded layers each frame; past it, deeper
/// regions are dropped.
const MAX_INJECTION_DEPTH: usize = 4;

/// Per-buffer parse state.
struct BufferState {
    shadow: Rope,
    parser: Parser,
    tree: Option<Tree>,
    language: String,
    /// The last reparse hit [`PARSE_DEADLINE`] and was cancelled, so this buffer's
    /// parse is **unfinished** — tree-sitter retains the outstanding parse on the
    /// `parser`, and re-invoking parse *resumes* it. While set, the engine keeps
    /// making progress (each [`Engine::highlights`] resumes a frame's worth) and the
    /// server keeps redrawing (see [`Engine::parse_pending`]) until it converges, so
    /// a large file highlights progressively instead of staying dark forever.
    incomplete: bool,
    /// Injected sub-language layers, re-derived from the root tree after each
    /// reparse (and when the injection query changes). Empty when the grammar
    /// ships no injection query or no region matches. See
    /// [`Engine::rebuild_injection_layers`].
    injections: Vec<InjectionLayer>,
    /// Lines a full-line-background capture ([`LINE_BACKGROUND_GROUPS`]) touched in
    /// the most recent [`Engine::highlights`] call — read back by the server via
    /// [`line_background_lines`](nxvim_core::syntax::SyntaxEngine::line_background_lines)
    /// to paint the `line_bg` layer under a markdown fenced code block.
    line_bg_lines: Vec<usize>,
}

/// One injected sub-language layer: a child grammar's parse of the host buffer
/// restricted (via `Parser::set_included_ranges`) to the injected region(s) — the
/// rust inside a `vim.cmd[[…]]`, or rust inside a string.
///
/// The child parses *through* `included_ranges` over the buffer shadow, so `tree`'s
/// byte/point coordinates are **buffer-absolute** — no per-layer offset and no
/// substring copy. This is faithful for position-sensitive grammars and lets one
/// combined layer own several ranges. The layer is keyed by `language` for
/// incremental reparse: across an edit the old tree is `edit`ed and reused as the
/// parse hint rather than rebuilt from scratch.
struct InjectionLayer {
    /// The injected language (already normalized, e.g. `rust`); looked up in the
    /// grammar cache to find the child highlights query.
    language: String,
    /// The child parse tree, in buffer coordinates.
    tree: Tree,
    /// The buffer byte ranges this layer covers. A combined layer owns several; a
    /// node spanning the gap between two ranges must paint only *within* them, so
    /// the painter clips this layer's captures to these ranges.
    ranges: Vec<Range<usize>>,
}

impl BufferState {
    /// Reparse from the shadow, reusing the old tree when present (incremental).
    fn reparse(&mut self) {
        let shadow = &self.shadow;
        let mut callback = |byte: usize, _: Point| -> &[u8] { read_chunk(shadow, byte) };
        // Cancel the parse once it has run longer than the deadline — the
        // in-process replacement for the worker's "never stalls the UI" property.
        let mut budget = deadline_budget(Instant::now(), PARSE_DEADLINE);
        let options = ParseOptions::new().progress_callback(&mut budget);
        // A `None` result means the deadline fired mid-parse. tree-sitter keeps the
        // outstanding parse on the parser, so the *next* `parse` call resumes where
        // this one stopped (the budget only ever costs one frame of work, never a
        // restart). Keep the last good tree for this frame's highlights and flag the
        // buffer `incomplete` so the engine resumes on the next `highlights` and the
        // server keeps redrawing until the parse converges — a large file colours in
        // progressively instead of staying permanently dark.
        match self
            .parser
            .parse_with_options(&mut callback, self.tree.as_ref(), Some(options))
        {
            Some(tree) => {
                self.tree = Some(tree);
                self.incomplete = false;
            }
            None => self.incomplete = true,
        }
    }
}

/// A cached grammar-load result for a language. Remembers *why* a grammar is
/// absent so the editor can stay silent for an uninstalled one but echo a real
/// load failure. Cached on first use, so the dlopen (and its outcome) happen
/// once per language, not once per keystroke.
enum Slot {
    // Boxed: a loaded `Grammar` (a `Language` + several compiled `Query`s) dwarfs
    // the other two variants, so inline storage would bloat every absent slot.
    Loaded(Box<Grammar>),
    /// No parser installed — silent.
    NotInstalled,
    /// Installed but broken; the reason to echo.
    Failed(String),
}

/// Owns every buffer's parse state and a lazily-populated grammar cache.
pub struct Engine {
    /// nxvim's own data dir — the writable root (where `:TSInstall` lands) and the
    /// path query overrides resolve against. Always `roots[0]`.
    data_dir: PathBuf,
    /// Grammar resolution search path: `data_dir` first, then read-only fallbacks
    /// (an existing neovim `site/`). A grammar's parser *and* its queries are
    /// loaded from the first root that has the parser, so the pair stays matched.
    roots: Vec<PathBuf>,
    // Field order matters for drop. `buffers` (each `BufferState`'s `Parser`, and
    // the `Tree`s under it) must drop **before** `grammars`, which owns the dlopen'd
    // grammar libraries (`Slot::Loaded(Grammar)._lib`) that every `Language`, tree,
    // and external scanner lives inside. A parser left mid-parse — e.g. a reparse
    // cancelled by `PARSE_DEADLINE` on a large file — keeps a non-null external
    // scanner payload, so `Parser::drop` → `ts_parser_delete` calls the grammar's
    // `external_scanner.destroy` through the `TSLanguage`. If the library had already
    // been unloaded, that call dereferences unmapped memory and segfaults at exit.
    // Rust drops fields in declaration order, so `buffers` is declared first.
    buffers: HashMap<BufferId, BufferState>,
    grammars: HashMap<String, Slot>,
    /// Grammars evicted from `grammars` by [`Engine::reload_grammar`] whose dlopen'd
    /// library must stay mapped for the rest of the session. A *loaded* grammar's
    /// library is referenced by every open buffer's `Parser` and `Tree` (built from
    /// its `Language`) — including a parser left mid-parse, whose external-scanner
    /// payload `ts_parser_delete` frees *through* the library when the buffer is
    /// re-opened or dropped. Dropping the library at reload time would unmap that
    /// code out from under those live buffers (the same destroy-after-unload SIGSEGV
    /// the field order guards against at teardown — see `tests/drop_order.rs`), so a
    /// reload retires the old grammar here instead of dropping it. Declared **after**
    /// `buffers` for the same reason `grammars` is: it must outlive every parser/tree
    /// that points into it.
    retired_grammars: Vec<Slot>,
    /// Query-text overrides from the resolution bridge, consulted by
    /// [`Grammar::load`] and applied in place by [`Engine::set_query`].
    query_overrides: QueryOverrides,
}

impl Engine {
    pub fn new(data_dir: PathBuf) -> Self {
        let mut roots = vec![data_dir.clone()];
        roots.extend(crate::extra_roots());
        Engine {
            data_dir,
            roots,
            buffers: HashMap::new(),
            grammars: HashMap::new(),
            retired_grammars: Vec::new(),
            query_overrides: QueryOverrides::new(),
        }
    }

    /// The search root `lang`'s grammar resolves from: the first root with an
    /// installed parser, else the writable data dir (so a genuinely-missing
    /// grammar still reports NotInstalled from there). Both the parser *and* its
    /// disk queries load from this root, so the pair stays matched — the query
    /// reads in [`Self::read_disk_query`] must use this, not `data_dir`, or a
    /// grammar borrowed from a read-only fallback root (an existing neovim
    /// `site/`) would report "no base query" to the resolution bridge.
    fn root_for(&self, lang: &str) -> &Path {
        self.roots
            .iter()
            .find(|r| crate::loader::has_parser(r, lang))
            .map(PathBuf::as_path)
            .unwrap_or(&self.data_dir)
    }

    /// Lazily load (and cache) the grammar for `lang`, returning its cache slot.
    /// The load — and its outcome (loaded / not-installed / failed) — happens once
    /// per language; later calls are a cache hit.
    fn grammar(&mut self, lang: &str) -> &Slot {
        if !self.grammars.contains_key(lang) {
            // Pick the first search root that actually has this parser (its queries
            // load from the same root); fall back to the writable data dir so a
            // genuinely-missing grammar still reports NotInstalled from there.
            let root = self.root_for(lang).to_path_buf();
            let slot = match Grammar::load(&root, lang, &self.query_overrides) {
                Ok(g) => Slot::Loaded(Box::new(g)),
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
    /// on-disk query. Only the engine-executed names ([`is_engine_query`] —
    /// `highlights` / `indents` / `injections` / `folds` / `textobjects`) reach the
    /// engine; any other name is a no-op here.
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
        if !is_engine_query(name) {
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
        self.recompile_query(lang, name, text)?;
        // A changed injection query re-derives every affected buffer's child layers
        // (they are a function of this query over each buffer's tree). `highlights`
        // / `indents` need no rebuild — the painter reads them straight off the
        // grammar each frame. Skipped on a compile failure (handled above), so a
        // bad query leaves the prior layers in place rather than dropping them.
        if name == "injections" {
            self.rebuild_all_injection_layers();
        }
        Ok(())
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
        if !is_engine_query(name) {
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
    /// against. Read from [`Self::root_for`]'s root — the one the parser (and its
    /// queries) actually load from — so a grammar resolved from a read-only
    /// fallback root reports *its* base, not a missing `data_dir` file.
    fn read_disk_query(&self, lang: &str, name: &str) -> Result<Option<String>, String> {
        let path = query_path(self.root_for(lang), lang, &format!("{name}.scm"));
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
        // Compile an *optional* query (`None` source → no query) against the live
        // language, labelling a compile error with the query name. Shared by the
        // optional arms below; `highlights` stays separate since its absence is an
        // error, not "no query". `language` is a cheap (ref-counted) clone so the
        // closure can borrow it while each arm assigns back into `g`.
        let language = g.language.clone();
        let compile_opt = |src: Option<String>, what: &str| -> Result<Option<Query>, String> {
            match src {
                Some(s) => Ok(Some(
                    compile_query(&language, &s)
                        .map_err(|e| format!("compiling {lang} {what}: {e}"))?,
                )),
                None => Ok(None),
            }
        };
        match name {
            "highlights" => {
                let s = src.ok_or_else(|| format!("no highlights query on disk for '{lang}'"))?;
                g.query = compile_query(&language, &s)
                    .map_err(|e| format!("compiling {lang} highlights: {e}"))?;
            }
            "indents" => g.indents = compile_opt(src, "indents")?,
            "injections" => g.injections = compile_opt(src, "injections")?,
            "folds" => g.folds = compile_opt(src, "folds")?,
            "textobjects" => g.textobjects = compile_opt(src, "textobjects")?,
            _ => unreachable!("guarded above"),
        }
        Ok(())
    }

    /// Re-derive every open buffer's injection layers — the rebuild a changed
    /// injection query triggers. *All* buffers, not just those whose top-level
    /// language matches: with nesting, a buffer of language A can carry a layer of
    /// language B (e.g. a markdown buffer's injected rust), so a change to B's
    /// injection query must refresh A too. A query change is config-time and rare,
    /// so rebuilding every buffer is cheap enough. Buffer ids are collected first so
    /// the per-buffer rebuild can take `&mut self`.
    fn rebuild_all_injection_layers(&mut self) {
        let ids: Vec<BufferId> = self.buffers.keys().copied().collect();
        for id in ids {
            self.rebuild_injection_layers(id);
        }
    }

    /// Re-derive a buffer's injected child layers from scratch (drops the old child
    /// trees) — the rebuild for buffer-open and an injection-query change, where the
    /// query (or buffer) is new so there is nothing to reparse incrementally from.
    fn rebuild_injection_layers(&mut self, buffer: BufferId) {
        let regions = self.top_level_injection_regions(buffer);
        self.build_injection_layers(buffer, regions, HashMap::new());
    }

    /// Re-derive a buffer's injected child layers **incrementally** after an edit.
    /// Each surviving child tree is `edit`ed with this frame's deltas and reused as
    /// the parse hint for the region of its language, so unchanged subtrees are not
    /// reparsed (and it doubles as the last-good fallback under the parse budget).
    fn update_injection_layers(&mut self, buffer: BufferId, edits: &[InputEdit]) {
        // Lift the old layers out, shifting each tree by this frame's deltas so it
        // can serve as an incremental reparse hint for its language's new region(s).
        let old_by_lang = {
            let Some(state) = self.buffers.get_mut(&buffer) else {
                return;
            };
            let mut map: HashMap<String, Vec<Tree>> = HashMap::new();
            for mut layer in std::mem::take(&mut state.injections) {
                for e in edits {
                    layer.tree.edit(e);
                }
                map.entry(layer.language).or_default().push(layer.tree);
            }
            map
        };
        let regions = self.top_level_injection_regions(buffer);
        self.build_injection_layers(buffer, regions, old_by_lang);
    }

    /// Run the host grammar's injection query over the root tree and resolve the
    /// matches to `(language, ranges)` region-sets — the top (depth-1) of the layer
    /// tree. The host is the query's `injection.self`; it has no parent. `&self` (no
    /// grammar load), so it can borrow the buffer + grammar caches together and
    /// return owned data the caller then builds with `&mut self`.
    fn top_level_injection_regions(&self, buffer: BufferId) -> Vec<(String, Vec<Range<usize>>)> {
        let Some(state) = self.buffers.get(&buffer) else {
            return Vec::new();
        };
        let Some(tree) = state.tree.as_ref() else {
            return Vec::new();
        };
        match self.grammars.get(&state.language) {
            Some(Slot::Loaded(host)) => match host.injections.as_ref() {
                Some(query) => collect_injection_regions(
                    query,
                    tree,
                    &state.shadow,
                    Some(&state.language),
                    None,
                ),
                None => Vec::new(), // no injection query → no layers
            },
            _ => Vec::new(),
        }
    }

    /// Run `query_lang`'s injection query over a child `tree` to find the regions it
    /// injects — one level of nesting. `injection.self` resolves to `query_lang`,
    /// `injection.parent` to `parent_lang` (the language that injected this layer).
    fn nested_injection_regions(
        &self,
        query_lang: &str,
        tree: &Tree,
        buffer: BufferId,
        parent_lang: &str,
    ) -> Vec<(String, Vec<Range<usize>>)> {
        let Some(state) = self.buffers.get(&buffer) else {
            return Vec::new();
        };
        let Some(Slot::Loaded(g)) = self.grammars.get(query_lang) else {
            return Vec::new();
        };
        let Some(query) = g.injections.as_ref() else {
            return Vec::new();
        };
        collect_injection_regions(
            query,
            tree,
            &state.shadow,
            Some(query_lang),
            Some(parent_lang),
        )
    }

    /// Build the child layers for `regions` (each a `(language, ranges)` region-set),
    /// reusing the edit-shifted trees in `old_by_lang` (FIFO per language) as
    /// incremental parse hints — empty for a full rebuild. Each set is parsed by its
    /// child grammar restricted to its ranges via `included_ranges` (so one combined
    /// injection's many ranges form one tree, and the tree is in buffer coordinates).
    ///
    /// Nesting is a breadth-first walk: after a layer parses, its own grammar's
    /// injection query runs over it to enqueue the regions *it* injects, down to
    /// [`MAX_INJECTION_DEPTH`]. Shallower layers are pushed first, so the painter's
    /// layer rank (vector order) already makes a deeper layer win over a shallower.
    ///
    /// A missing or broken child grammar is silently skipped (best-effort) and the
    /// region keeps the host's flat paint. The whole pass is bounded by
    /// [`INJECTION_DEADLINE`]: once over budget a cancelled parse falls back to its
    /// (edit-shifted) old tree — one frame stale, never a hang. Runs entirely in Rust
    /// (no Lua), so it is safe off the synchronous redraw path.
    fn build_injection_layers(
        &mut self,
        buffer: BufferId,
        regions: Vec<(String, Vec<Range<usize>>)>,
        mut old_by_lang: HashMap<String, Vec<Tree>>,
    ) {
        // The host language injected the top-level regions; it is their parent for
        // the `injection.parent` directive when those layers are queried in turn.
        let Some(host_lang) = self.buffers.get(&buffer).map(|s| s.language.clone()) else {
            return;
        };
        let started = Instant::now();
        let mut layers = Vec::with_capacity(regions.len());
        // (language, ranges, depth, injector) — `injector` is the language that
        // injected this region, used as `injection.parent` when recursing into it.
        let mut queue: VecDeque<(String, Vec<Range<usize>>, usize, String)> = regions
            .into_iter()
            .map(|(lang, ranges)| (lang, ranges, 1, host_lang.clone()))
            .collect();

        while let Some((language, mut ranges, depth, injector)) = queue.pop_front() {
            // Lazily load (cache) the child grammar; skip silently if it is missing
            // or broken — the region just keeps the host's flat paint.
            let child_language = match self.grammar(&language) {
                Slot::Loaded(g) => g.language.clone(),
                _ => continue,
            };
            let mut parser = Parser::new();
            if parser.set_language(&child_language).is_err() {
                continue;
            }
            // An edit-shifted tree of this language, reused as the incremental parse
            // hint and the stale fallback if this frame's parse is cancelled.
            let old = old_by_lang.get_mut(&language).and_then(Vec::pop);

            // `included_ranges` must be ascending and non-overlapping. A combined
            // pattern can match *nested* nodes (a section inside a section), whose
            // ranges overlap — passed through raw, `set_included_ranges` would
            // reject them and the whole layer would silently drop. Merge each
            // overlap into its union (identical coverage for both the child parse
            // and the painter's clipping).
            ranges.sort_by_key(|r| r.start);
            ranges.dedup_by(|next, prev| {
                if next.start <= prev.end {
                    prev.end = prev.end.max(next.end);
                    true
                } else {
                    false
                }
            });
            let Some(state) = self.buffers.get(&buffer) else {
                return;
            };
            let shadow = &state.shadow;
            let included: Vec<tree_sitter::Range> =
                ranges.iter().map(|r| ts_range(shadow, r)).collect();
            if included.is_empty() || parser.set_included_ranges(&included).is_err() {
                continue;
            }
            let tree = {
                let mut budget = deadline_budget(started, INJECTION_DEADLINE);
                let options = ParseOptions::new().progress_callback(&mut budget);
                let mut callback = |byte: usize, _: Point| -> &[u8] { read_chunk(shadow, byte) };
                parser.parse_with_options(&mut callback, old.as_ref(), Some(options))
            };
            let tree = match tree {
                Some(tree) => tree,
                // Budget exhausted (or parse cancelled): keep the last-good child
                // tree if there is one, painting one frame stale. A brand-new region
                // with no prior tree is dropped this frame and re-attempted next.
                None => {
                    if let Some(tree) = old {
                        layers.push(InjectionLayer {
                            language,
                            tree,
                            ranges,
                        });
                    }
                    continue;
                }
            };

            // Nesting: enqueue the regions this layer itself injects, one level down.
            if depth < MAX_INJECTION_DEPTH {
                for (lang, sub) in
                    self.nested_injection_regions(&language, &tree, buffer, &injector)
                {
                    queue.push_back((lang, sub, depth + 1, language.clone()));
                }
            }
            layers.push(InjectionLayer {
                language,
                tree,
                ranges,
            });
        }

        if let Some(state) = self.buffers.get_mut(&buffer) {
            state.injections = layers;
        }
    }

    /// (Re)initialize a buffer from full text and do the initial parse. The
    /// [`OpenOutcome`] reports whether an *installed* grammar failed to load
    /// (worth echoing) vs the silent no-grammar / parsed-fine cases.
    ///
    /// `open` is also the language-*switch* path (`:set filetype=`, `:w other.ext`):
    /// the editor re-opens an already-known buffer under its new language. Every
    /// early return below therefore drops any previous language's `BufferState` —
    /// leaving it in place would keep painting (and incrementally updating) the
    /// *old* language's highlights on a buffer the editor believes has none.
    pub fn open(&mut self, buffer: BufferId, lang: &str, text: &str) -> OpenOutcome {
        let language = match self.grammar(lang) {
            Slot::Loaded(g) => g.language.clone(),
            Slot::NotInstalled => {
                // Silent: best-effort. But a switch away from a highlighted
                // language must still forget the stale parse state.
                self.buffers.remove(&buffer);
                return OpenOutcome::Ok;
            }
            Slot::Failed(reason) => {
                let reason = reason.clone();
                self.buffers.remove(&buffer);
                return OpenOutcome::LoadFailed(reason);
            }
        };
        let mut parser = Parser::new();
        if let Err(e) = parser.set_language(&language) {
            // Unreachable in practice (the ABI is probed at load), but report it
            // honestly rather than silently dropping the buffer.
            self.buffers.remove(&buffer);
            return OpenOutcome::LoadFailed(format!("set_language: {e}"));
        }
        let mut state = BufferState {
            shadow: Rope::from_str(text),
            parser,
            tree: None,
            language: lang.to_string(),
            incomplete: false,
            injections: Vec::new(),
            line_bg_lines: Vec::new(),
        };
        state.reparse();
        self.buffers.insert(buffer, state);
        self.rebuild_injection_layers(buffer);
        OpenOutcome::Ok
    }

    /// Apply edit deltas to a buffer's shadow + tree, then reparse incrementally.
    pub fn edit(&mut self, buffer: BufferId, edits: &[BufferEdit]) {
        let Some(state) = self.buffers.get_mut(&buffer) else {
            return; // never opened; the editor opens before editing
        };
        // The deltas that actually applied, kept so the same edits can be replayed
        // onto each injected child tree for its incremental reparse.
        let mut applied: Vec<InputEdit> = Vec::new();
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
            let input_edit = InputEdit {
                start_byte: e.start_byte,
                old_end_byte: e.old_end_byte,
                new_end_byte: e.new_end_byte,
                start_position: point(e.start_point),
                old_end_position: point(e.old_end_point),
                new_end_position: point(e.new_end_point),
            };
            if let Some(tree) = state.tree.as_mut() {
                tree.edit(&input_edit);
            }
            applied.push(input_edit);
        }
        // A still-`incomplete` parse left an outstanding parse on the `parser` that
        // was reading the *pre-edit* shadow; resuming it now would parse stale bytes.
        // Reset so the next reparse starts fresh from the just-patched shadow (still
        // reusing `tree` incrementally if a prior parse ever completed).
        if state.incomplete {
            state.parser.reset();
        }
        state.reparse();
        // The injected regions move with every edit, so re-derive the child layers
        // from the fresh root tree — incrementally, replaying `applied` onto the
        // surviving child trees. `state`'s borrow ends at the line above.
        self.update_injection_layers(buffer, &applied);
    }

    /// Forget a buffer's shadow text and parse tree (the editor deleted it).
    pub fn close(&mut self, buffer: BufferId) {
        self.buffers.remove(&buffer);
    }

    /// Whether `buffer`'s parse was cancelled by [`PARSE_DEADLINE`] and still has
    /// work pending — a large file mid-parse. The server polls this after each
    /// redraw to decide whether to schedule another frame, which resumes the parse
    /// via [`Self::highlights`], until it converges. False for an unknown buffer or
    /// a fully-parsed one.
    pub fn parse_pending(&self, buffer: BufferId) -> bool {
        self.buffers.get(&buffer).is_some_and(|s| s.incomplete)
    }

    /// Whether a buffer is known (opened) and which language it uses.
    pub fn language_of(&self, buffer: BufferId) -> Option<&str> {
        self.buffers.get(&buffer).map(|b| b.language.as_str())
    }

    /// Extract highlight spans for the visible line range `[first_line, last_line)`,
    /// merging the host tree with any injected child layers (each painting over the
    /// host within its region — neovim's "injected language wins" rule).
    pub fn highlights(
        &mut self,
        buffer: BufferId,
        first_line: usize,
        last_line: usize,
    ) -> Vec<Span> {
        // Resume a deadline-cancelled parse one budget at a time: each redraw's
        // highlight pull advances the outstanding parse, so a large file converges
        // over a few frames (the server keeps redrawing while `parse_pending`). Until
        // the root parse first completes there is no tree, so the spans below are
        // empty for those first frames; once it lands they become real highlights.
        let resumed_to_completion = match self.buffers.get_mut(&buffer) {
            Some(state) if state.incomplete => {
                state.reparse();
                !state.incomplete
            }
            _ => false,
        };
        if resumed_to_completion {
            // The freshly-completed root tree needs its injection layers built, just
            // as the open/edit paths do after their reparse.
            self.rebuild_injection_layers(buffer);
        }

        // Reset the line-background memo up front, so a buffer with no tree / grammar
        // (the early returns below) reports no backgrounds rather than a stale set.
        if let Some(state) = self.buffers.get_mut(&buffer) {
            state.line_bg_lines.clear();
        }
        let (spans, bg_lines) = {
            let Some(state) = self.buffers.get(&buffer) else {
                return Vec::new();
            };
            let Some(tree) = state.tree.as_ref() else {
                return Vec::new();
            };
            let Some(Slot::Loaded(host)) = self.grammars.get(&state.language) else {
                return Vec::new();
            };

            // Layer 0 is the host tree. Each loaded injected child contributes a deeper
            // layer that paints over it; all layers' nodes are in buffer coordinates
            // (the children parse through `included_ranges`), so the painter reads every
            // layer's predicate text from the one shadow. A child whose grammar isn't
            // loaded contributes nothing (the host paint stands).
            let mut layers = Vec::with_capacity(1 + state.injections.len());
            layers.push(Layer {
                query: &host.query,
                tree,
                ranges: &[], // the host covers the whole buffer — no clipping
            });
            for inj in &state.injections {
                if let Some(Slot::Loaded(child)) = self.grammars.get(&inj.language) {
                    layers.push(Layer {
                        query: &child.query,
                        tree: &inj.tree,
                        ranges: &inj.ranges,
                    });
                }
            }
            extract_spans(&layers, &state.shadow, first_line, last_line)
        };
        // Stash the line-background lines for the server to read (via
        // `line_background_lines`) immediately after this call — the source of the
        // `line_bg` layer under a markdown fenced code block.
        if let Some(state) = self.buffers.get_mut(&buffer) {
            state.line_bg_lines = bg_lines;
        }
        spans
    }

    /// Highlight an off-buffer snippet (`text` in `lang`) over `[first_line,
    /// last_line)` — a **stateless** full parse with no `BufferId`, no incremental
    /// reuse, and no injection layers (the host grammar only). For the picker
    /// preview pane, which paints a file that is not an open buffer. Returns the
    /// host highlight spans in `text` coordinates, or empty when no grammar is
    /// installed for `lang` or the parse is cancelled / fails.
    pub fn highlight_text(
        &mut self,
        lang: &str,
        text: &str,
        first_line: usize,
        last_line: usize,
    ) -> Vec<Span> {
        let language = match self.grammar(lang) {
            Slot::Loaded(g) => g.language.clone(),
            _ => return Vec::new(), // silent: no grammar (or load failed)
        };
        let mut parser = Parser::new();
        if parser.set_language(&language).is_err() {
            return Vec::new();
        }
        let shadow = Rope::from_str(text);
        // Parse under the same wall-clock deadline as `reparse`, so a pathologically
        // large preview file can't stall the frame (it just renders plain).
        let mut budget = deadline_budget(Instant::now(), PARSE_DEADLINE);
        let options = ParseOptions::new().progress_callback(&mut budget);
        let mut callback = |byte: usize, _: Point| -> &[u8] { read_chunk(&shadow, byte) };
        let Some(tree) = parser.parse_with_options(&mut callback, None, Some(options)) else {
            return Vec::new();
        };
        // Re-borrow the grammar's compiled query immutably (the `&mut self` from
        // `grammar` has ended); the host covers the whole snippet — no injections.
        let Some(Slot::Loaded(host)) = self.grammars.get(lang) else {
            return Vec::new();
        };
        let layers = vec![Layer {
            query: &host.query,
            tree: &tree,
            ranges: &[],
        }];
        // The off-buffer preview needs only the spans; its surface has no `line_bg`
        // layer, so the block-background lines are discarded.
        extract_spans(&layers, &shadow, first_line, last_line).0
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

    /// Foldable node ranges for `buffer`, by running the grammar's `folds.scm`
    /// (`@fold` captures) over the current tree. Each captured node contributes its
    /// `[start_row, end_row]` inclusive line span; the core turns the set into
    /// per-line fold levels by containment. Empty when there is no grammar, no fold
    /// query, or no tree yet — the honest "no tree-sitter folds" cases.
    pub fn folds(&self, buffer: BufferId) -> Vec<FoldRange> {
        let Some(state) = self.buffers.get(&buffer) else {
            return Vec::new();
        };
        let Some(tree) = state.tree.as_ref() else {
            return Vec::new();
        };
        let Some(Slot::Loaded(grammar)) = self.grammars.get(&state.language) else {
            return Vec::new();
        };
        let Some(query) = grammar.folds.as_ref() else {
            return Vec::new();
        };
        let rope = &state.shadow;
        let root = tree.root_node();
        let names = query.capture_names();
        let mut out = Vec::new();
        let mut cursor = QueryCursor::new();
        let mut caps = cursor.captures(query, root, node_text_provider(rope));
        while let Some((m, idx)) = caps.next() {
            let cap = m.captures[*idx];
            // `@fold` defines the foldable nodes; any other capture (a predicate
            // helper) is ignored.
            if names[cap.index as usize] != "fold" {
                continue;
            }
            let node = cap.node;
            let start = node.start_position().row;
            let mut end = node.end_position().row;
            // A node whose range ends at column 0 of its last line doesn't really
            // occupy that line (its closer sits on the line above), so trim it — the
            // fold would otherwise swallow a trailing line (neovim's foldexpr does
            // the same).
            if end > start && node.end_position().column == 0 {
                end -= 1;
            }
            out.push(FoldRange { start, end });
        }
        out
    }

    /// Byte ranges of `buffer`'s `textobjects.scm` nodes captured as exactly
    /// `capture` (e.g. `"function.inner"`) that **contain** `byte`
    /// (`start <= byte < end`), innermost (smallest span) first — so a `count`
    /// selects successively larger enclosing scopes. Empty when there is no
    /// grammar, no textobjects query, or nothing matches. This is the tree-sitter
    /// source behind the `vif` / `daf` / `dia` text objects; the core picks the
    /// `count`-th range and hands it to the shared text-object applier. Host-tree
    /// only — injected sub-language layers are not consulted.
    pub fn text_objects_at(
        &self,
        buffer: BufferId,
        capture: &str,
        byte: usize,
    ) -> Vec<(usize, usize)> {
        let Some(state) = self.buffers.get(&buffer) else {
            return Vec::new();
        };
        let Some(tree) = state.tree.as_ref() else {
            return Vec::new();
        };
        let Some(Slot::Loaded(grammar)) = self.grammars.get(&state.language) else {
            return Vec::new();
        };
        let Some(query) = grammar.textobjects.as_ref() else {
            return Vec::new();
        };
        let rope = &state.shadow;
        let root = tree.root_node();
        let names = query.capture_names();
        let mut out: Vec<(usize, usize)> = Vec::new();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, root, node_text_provider(rope));
        while let Some(m) = matches.next() {
            // An nvim-treesitter textobject region can span *several* nodes captured
            // under one name within a single match — `_+ @function.inner` tags every
            // statement between the braces, and the inner object is their combined
            // extent (min start … max end), not any one statement. So union this
            // match's `capture` nodes rather than treating each as its own range.
            let mut lo = usize::MAX;
            let mut hi = 0usize;
            for cap in m.captures {
                if names[cap.index as usize] != capture {
                    continue;
                }
                let r = cap.node.byte_range();
                lo = lo.min(r.start);
                hi = hi.max(r.end);
            }
            // `lo < hi` skips a match that captured nothing under this name; then keep
            // only regions that actually surround the cursor.
            if lo < hi && lo <= byte && byte < hi {
                out.push((lo, hi));
            }
        }
        // Innermost (smallest span) first; ties broken by start. Dedup collapses a
        // region two patterns produce identically.
        out.sort_by_key(|(s, e)| (e - s, *s));
        out.dedup();
        out
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
    let mut caps = cursor.captures(query, *root, node_text_provider(rope));
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

    fn reload_grammar(&mut self, lang: &str) {
        // Evict the cached slot so the next `grammar()` re-resolves it from the
        // search path — picking up a just-installed parser. Buffers already parsed
        // under the old slot keep their state until the editor re-opens them (it
        // drops `syntax_opened` markers in step).
        //
        // A *loaded* grammar must not be dropped here: every open buffer's parser
        // and tree (and any parser left mid-parse, whose external-scanner payload is
        // freed *through* the library on drop/re-open) still points into its dlopen'd
        // `.so`. Dropping it would unmap that code out from under those live buffers —
        // the destroy-after-unload SIGSEGV `tests/drop_order.rs` exercises, here at
        // reload time rather than teardown. Retire it so the library stays mapped for
        // the rest of the session; a not-installed / failed slot owns nothing and is
        // simply dropped.
        if let Some(slot) = self.grammars.remove(lang) {
            if matches!(slot, Slot::Loaded(_)) {
                self.retired_grammars.push(slot);
            }
        }
    }

    fn highlights(&mut self, buffer: BufferId, first: usize, last: usize) -> Vec<Span> {
        Engine::highlights(self, buffer, first, last)
    }

    fn line_background_lines(&self, buffer: BufferId) -> Vec<usize> {
        self.buffers
            .get(&buffer)
            .map(|s| s.line_bg_lines.clone())
            .unwrap_or_default()
    }

    fn parse_pending(&self, buffer: BufferId) -> bool {
        Engine::parse_pending(self, buffer)
    }

    fn highlight_text(&mut self, lang: &str, text: &str, first: usize, last: usize) -> Vec<Span> {
        Engine::highlight_text(self, lang, text, first, last)
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

    fn folds(&mut self, buffer: BufferId) -> Vec<FoldRange> {
        Engine::folds(self, buffer)
    }

    fn folds_available(&self, buffer: BufferId) -> bool {
        let Some(state) = self.buffers.get(&buffer) else {
            return false;
        };
        matches!(
            self.grammars.get(&state.language),
            Some(Slot::Loaded(g)) if g.folds.is_some()
        )
    }

    fn text_objects_at(
        &mut self,
        buffer: BufferId,
        capture: &str,
        byte: usize,
    ) -> Vec<(usize, usize)> {
        Engine::text_objects_at(self, buffer, capture, byte)
    }

    fn text_objects_available(&self, buffer: BufferId) -> bool {
        let Some(state) = self.buffers.get(&buffer) else {
            return false;
        };
        matches!(
            self.grammars.get(&state.language),
            Some(Slot::Loaded(g)) if g.textobjects.is_some()
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

    fn base_query(&self, lang: &str, name: &str) -> Result<Option<String>, String> {
        // The base the engine would compile with no override: the on-disk file,
        // the same source `set_query_overlay` compares a resolved overlay against.
        self.read_disk_query(lang, name)
    }
}

/// One capture source the painter merges: a compiled `query` run over `tree`. Both
/// the host tree and every injected child tree have buffer-absolute coordinates
/// (the children parse through `included_ranges`), so all layers share the one
/// buffer shadow as their predicate text source and need no per-layer offset.
///
/// `ranges` clips this layer's captures: empty for the host (it covers everything),
/// the injected ranges for a child — a combined child's node can span the gap
/// between its ranges, and only the parts inside the ranges may paint.
struct Layer<'a> {
    query: &'a Query,
    tree: &'a Tree,
    ranges: &'a [Range<usize>],
}

/// Whether `name` is a tree-sitter metadata/control capture rather than a visual
/// highlight group. Grammars tag nodes with these alongside a real highlight (e.g.
/// `(comment) @comment @spell`) to mark spell-check regions (`@spell` / `@nospell`)
/// or conceal candidates (`@conceal`); they carry no colour, so the painter must
/// not let them overwrite the highlight capture they sit beside. Matches the major
/// segment so `@spell.foo`-style refinements are caught too.
fn is_metadata_capture(name: &str) -> bool {
    matches!(
        name.split('.').next().unwrap_or(name),
        "spell" | "nospell" | "conceal"
    )
}

/// Whether match `m` satisfies its `#lua-match?` / `#not-lua-match?` predicates.
///
/// These are neovim-specific predicates the tree-sitter binding does not evaluate
/// (it only enforces the standard `#match?` / `#eq?` / `#any-of?` family while
/// iterating), so they surface here as *general* predicates. Each compares a
/// capture's node text against a Lua pattern; a positive `lua-match?` keeps the
/// match only when the text matches, `not-lua-match?` only when it does not. When a
/// match carries no such predicate (the overwhelmingly common case) the loop is
/// empty and this is free. Other general predicates (`#vim-match?`, …) are not
/// understood and left unenforced — best-effort, not a silent narrowing of a
/// predicate we *do* support.
fn match_satisfies_lua_predicates(query: &Query, m: &QueryMatch, shadow: &Rope) -> bool {
    for pred in query.general_predicates(m.pattern_index) {
        let negate = match &*pred.operator {
            "lua-match?" => false,
            "not-lua-match?" => true,
            _ => continue,
        };
        let Some(QueryPredicateArg::Capture(cap_id)) = pred.args.first() else {
            continue; // malformed predicate — don't let it filter anything
        };
        let Some(QueryPredicateArg::String(pat)) = pred.args.get(1) else {
            continue;
        };
        // Apply to every node this capture matched in the match; like the standard
        // `#match?`, all must satisfy. A capture absent from the match is vacuously
        // satisfied (nothing to test).
        for c in m.captures.iter().filter(|c| c.index == *cap_id) {
            let text = node_bytes(shadow, c.node.byte_range());
            if crate::lua_pattern::lua_match(&text, pat.as_bytes()) == negate {
                return false;
            }
        }
    }
    true
}

/// Run each layer's query over the byte range covering the visible lines and
/// Capture groups whose highlight is a **full-line background** — markdown's fenced
/// and indented code blocks (`(fenced_code_block) @markup.raw.block`). The per-cell
/// paint below is winner-takes-cell, so a narrower injected token (the code's own
/// syntax) overwrites the block's background on the cells it covers, leaving the
/// background only on un-tokenized cells (spaces). [`extract_spans`] therefore also
/// reports which lines such a group *touches*, so the server can paint them as the
/// separate `line_bg` layer *under* the text — the same background the doc-float
/// renderer sets as a `line_hl_group` extmark. Keep this to genuine block
/// backgrounds: inline code (`markup.raw`) must not tile a whole line. The names
/// are tree-sitter **capture** names, so no leading `@` (the server re-adds it when
/// resolving the `line_bg` group against the colorscheme).
const LINE_BACKGROUND_GROUPS: &[&str] = &["markup.raw.block"];

/// resolve the captures into per-line byte spans. Within a layer the most-specific
/// (narrowest) capture wins; across layers a deeper (injected) layer overwrites a
/// shallower one inside its region — so injected highlighting paints over the host.
///
/// Returns the per-line spans plus the absolute line numbers a
/// [`LINE_BACKGROUND_GROUPS`] capture touches (for the `line_bg` layer). The line
/// list is recorded when a background capture is bucketed onto a line — *before* the
/// per-cell overwrite — so a line stays listed even where an injected token covers
/// every cell (e.g. a `}` at column 0 with no surrounding space).
fn extract_spans(
    layers: &[Layer],
    shadow: &Rope,
    first_line: usize,
    last_line: usize,
) -> (Vec<Span>, Vec<usize>) {
    let line_count = shadow.len_lines(LINE_TYPE).saturating_sub(1);
    let last_line = last_line.min(line_count);
    if first_line >= last_line {
        return (Vec::new(), Vec::new());
    }
    let lo = shadow.line_to_byte_idx(first_line, LINE_TYPE);
    let hi = shadow.line_to_byte_idx(last_line, LINE_TYPE);

    // Collect captures intersecting the viewport as (start, end, group, layer), all
    // in buffer coordinates. A child tree only holds nodes inside its included
    // ranges, so restricting its cursor to the viewport already bounds it to the
    // visible part of the injected region.
    let mut raw: Vec<(usize, usize, &str, usize)> = Vec::new();
    for (rank, layer) in layers.iter().enumerate() {
        let names = layer.query.capture_names();
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(lo..hi);
        let mut caps = cursor.captures(
            layer.query,
            layer.tree.root_node(),
            node_text_provider(shadow),
        );
        while let Some((m, idx)) = caps.next() {
            let cap = m.captures[*idx];
            let name = names[cap.index as usize];
            if name.starts_with('_') || is_metadata_capture(name) {
                // `_`-prefixed captures are internal/predicate; `spell` / `nospell`
                // / `conceal` are tree-sitter metadata captures (spell-check regions,
                // conceal marks), not visual highlight groups. neovim never paints
                // them. Skipping both means a node tagged `(comment) @comment @spell`
                // keeps `@comment`'s colour instead of being overwritten by the
                // colour-less metadata capture (which would render as plain text).
                continue;
            }
            // The tree-sitter binding enforces the standard text predicates
            // (`#match?` / `#eq?` / `#any-of?`) while iterating, but `#lua-match?`
            // is a neovim-specific predicate it leaves as a general predicate — so
            // we must apply it ourselves, or e.g. bash's `(#lua-match? … "^#!")`
            // shebang rule paints every comment as `@keyword.directive`.
            if !match_satisfies_lua_predicates(layer.query, m, shadow) {
                continue;
            }
            let (s, e) = (cap.node.start_byte(), cap.node.end_byte());
            if e <= s {
                continue;
            }
            if layer.ranges.is_empty() {
                raw.push((s, e, name, rank)); // host: no clipping
            } else {
                // Child: clip to the injected ranges so a node spanning the gap
                // between a combined layer's ranges paints only within them.
                for r in layer.ranges {
                    let (cs, ce) = (s.max(r.start), e.min(r.end));
                    if cs < ce {
                        raw.push((cs, ce, name, rank));
                    }
                }
            }
        }
    }

    // Fill order: shallower layers first, then broadest-first within a layer, so a
    // later write always wins — a deeper layer over a shallower one, and a narrower
    // capture over a broader one within the same layer.
    raw.sort_by_key(|(s, e, _, rank)| (*rank, std::cmp::Reverse(e - s), *s));

    // Per-visible-line geometry, computed once: `(line_start, content_len)` indexed
    // by `line - first_line`. The old code recomputed this inside the capture scan.
    let n_lines = last_line - first_line;
    let mut line_geom: Vec<(usize, usize)> = Vec::with_capacity(n_lines);
    for line in first_line..last_line {
        let line_start = shadow.line_to_byte_idx(line, LINE_TYPE);
        let line_end = shadow.line_to_byte_idx(line + 1, LINE_TYPE);
        // Trim the trailing `\n` / `\r` terminator bytes off the content length
        // by inspecting the last byte(s) in place — this used to materialize each
        // visible line into a fresh `String` every frame just to `trim_end` it.
        let mut content_len = line_end - line_start;
        while content_len > 0 {
            match read_chunk(shadow, line_start + content_len - 1).first() {
                Some(&(b'\n' | b'\r')) => content_len -= 1,
                _ => break,
            }
        }
        line_geom.push((line_start, content_len));
    }

    // Bucket each capture into the visible lines it paints, in one forward pass over
    // the already-sorted `raw`. The old code's per-line inner loop walked all of
    // `raw` and tested `e <= line_start || s >= line_start + content_len` per line;
    // here each capture instead visits only the line window it can possibly touch
    // (the line of `s` through the line of `e-1`) and applies the *identical* guard,
    // so a line is recorded in a bucket iff the old loop would have painted it. Lines
    // outside the window are exactly those the old guard rejected: below it via
    // `s >= line_start + content_len` (content ends before `s`), above it via
    // `e <= line_start`. Because the pass is forward over sorted `raw`, every
    // bucket's entries stay in the same relative order the old loop applied them, so
    // the last-write-wins paint per cell is byte-for-byte unchanged.
    let mut buckets: Vec<Vec<(usize, usize, &str)>> = vec![Vec::new(); n_lines];
    // Lines a full-line-background group touches, recorded here (pre-overwrite) so a
    // line stays flagged even when injected tokens later cover every one of its cells.
    let mut bg_line = vec![false; n_lines];
    for &(s, e, name, _) in &raw {
        if e == 0 {
            continue;
        }
        let lo_line = shadow.byte_to_line_idx(s, LINE_TYPE).max(first_line);
        let hi_line = shadow.byte_to_line_idx(e - 1, LINE_TYPE).min(last_line - 1);
        if lo_line > hi_line {
            continue;
        }
        let line_bg = LINE_BACKGROUND_GROUPS.contains(&name);
        for line in lo_line..=hi_line {
            let (line_start, content_len) = line_geom[line - first_line];
            if e <= line_start || s >= line_start + content_len {
                continue;
            }
            if line_bg {
                bg_line[line - first_line] = true;
            }
            buckets[line - first_line].push((s, e, name));
        }
    }

    let mut out = Vec::new();
    // The per-cell paint buffer, reused across visible lines — cleared + resized
    // per line rather than freshly allocated each one (it was the hottest per-line
    // allocation in this pass).
    let mut groups: Vec<Option<&str>> = Vec::new();
    for line in first_line..last_line {
        let (line_start, content_len) = line_geom[line - first_line];
        if content_len == 0 {
            continue;
        }
        groups.clear();
        groups.resize(content_len, None);
        for &(s, e, name) in &buckets[line - first_line] {
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
    let block_bg_lines = bg_line
        .iter()
        .enumerate()
        .filter_map(|(i, &on)| on.then_some(first_line + i))
        .collect();
    (out, block_bg_lines)
}

/// Run the injection `query` over `tree` and resolve each match to its
/// `(language, ranges)` region-sets — the directive interpreter, a faithful port of
/// `languagetree.lua::_get_injection` + `add_injection`.
///
/// `self_lang` is the language whose tree this is (the query's `injection.self`);
/// `parent_lang` is the language that injected it (`injection.parent`), absent at
/// the host. The language is resolved in upstream's order: `injection.self` >
/// `injection.parent` > a static `(#set! injection.language "<lang>")`, then a
/// dynamic `@injection.language` capture's **node text** overrides all (e.g. a
/// markdown fence's `info_string`). Content ranges come from each
/// `@injection.content` node, with non-`include-children` masking out named
/// children. A `(#set! injection.combined)` pattern accumulates all its matches'
/// ranges into one region-set (one child tree); otherwise each match is its own set.
/// A match with no resolvable language, or no ranges, is skipped.
fn collect_injection_regions(
    query: &Query,
    tree: &Tree,
    rope: &Rope,
    self_lang: Option<&str>,
    parent_lang: Option<&str>,
) -> Vec<(String, Vec<Range<usize>>)> {
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut out: Vec<(String, Vec<Range<usize>>)> = Vec::new();
    // Combined region-sets are keyed by (language, pattern) to an index into `out`,
    // so every match of a combined pattern appends to the same set.
    let mut combined_set: HashMap<(String, usize), usize> = HashMap::new();
    let mut matches = cursor.matches(query, tree.root_node(), node_text_provider(rope));
    while let Some(m) = matches.next() {
        let props = query.property_settings(m.pattern_index);
        let has = |key: &str| props.iter().any(|p| &*p.key == key);
        let combined = has("injection.combined");
        let include_children = has("injection.include-children");
        // Base language: the self / parent directive, else the static `#set!` tag.
        let base = if has("injection.self") {
            self_lang
        } else if has("injection.parent") {
            parent_lang
        } else {
            props.iter().find_map(|p| {
                (&*p.key == "injection.language")
                    .then_some(p.value.as_deref())
                    .flatten()
            })
        };
        // A dynamic `@injection.language` node text overrides the base; gather it and
        // the content ranges in one pass over the captures.
        let mut dynamic_lang: Option<String> = None;
        let mut ranges: Vec<Range<usize>> = Vec::new();
        for cap in m.captures {
            match names[cap.index as usize] {
                "injection.language" => {
                    let (s, e) = (cap.node.start_byte(), cap.node.end_byte());
                    if let Ok(text) = String::from_utf8(node_bytes(rope, s..e)) {
                        dynamic_lang = Some(text);
                    }
                }
                "injection.content" => {
                    for r in content_ranges(cap.node, include_children) {
                        if r.end > r.start {
                            ranges.push(r);
                        }
                    }
                }
                _ => {}
            }
        }
        let Some(language) = dynamic_lang.as_deref().or(base).and_then(normalize_lang) else {
            continue; // no resolvable language for this match
        };
        if ranges.is_empty() {
            continue; // an empty set would be read as "the whole buffer"
        }
        if combined {
            match combined_set.get(&(language.clone(), m.pattern_index)) {
                Some(&idx) => out[idx].1.extend(ranges),
                None => {
                    combined_set.insert((language.clone(), m.pattern_index), out.len());
                    out.push((language, ranges));
                }
            }
        } else {
            out.push((language, ranges));
        }
    }
    out
}

/// The injected byte ranges of one `@injection.content` node. With
/// `include_children` (or a leaf node) it is the node's whole range; otherwise the
/// named children are masked out, leaving the gaps around them — a faithful port of
/// `languagetree.lua::get_node_ranges`.
fn content_ranges(node: Node, include_children: bool) -> Vec<Range<usize>> {
    let (start, end) = (node.start_byte(), node.end_byte());
    if include_children || node.named_child_count() == 0 {
        // One range element — `once` (not a `vec!`/array literal) so it can't be
        // misread as expanding the range into a Vec of indices.
        return std::iter::once(start..end).collect();
    }
    let mut ranges = Vec::new();
    let mut cur = start;
    for i in 0..node.named_child_count() {
        let Some(child) = node.named_child(i as u32) else {
            continue;
        };
        let (cs, ce) = (child.start_byte(), child.end_byte());
        if cs > cur {
            ranges.push(cur..cs);
        }
        cur = ce;
    }
    if end > cur {
        ranges.push(cur..end);
    }
    ranges
}

/// A tree-sitter `Range` for the buffer byte range `r`, with its `Point`s computed
/// from the shadow — what [`Parser::set_included_ranges`] needs to restrict a child
/// parse to an injected region.
fn ts_range(shadow: &Rope, r: &Range<usize>) -> tree_sitter::Range {
    tree_sitter::Range {
        start_byte: r.start,
        end_byte: r.end,
        start_point: point_at(shadow, r.start),
        end_point: point_at(shadow, r.end),
    }
}

/// The `(row, col)` point of buffer byte `byte` in the shadow.
fn point_at(shadow: &Rope, byte: usize) -> Point {
    let line = shadow.byte_to_line_idx(byte, LINE_TYPE);
    let line_start = shadow.line_to_byte_idx(line, LINE_TYPE);
    Point::new(line, byte - line_start)
}

/// Normalize an injection language name the way `languagetree.lua`'s `resolve_lang`
/// does — strip whitespace, lowercase, `-`→`_` — and reject anything that isn't a
/// legal grammar identifier. Returns `None` for an empty or invalid name (skipped).
fn normalize_lang(raw: &str) -> Option<String> {
    let norm: String = raw
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_lowercase()
        .replace('-', "_");
    if norm.is_empty() || !norm.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return None;
    }
    Some(norm)
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

/// A borrowed, zero-copy view of `rope[range]` as a sequence of `&[u8]` rope
/// chunks — the tree-sitter [`tree_sitter::TextProvider`] for `#match?` / `#eq?`
/// predicate text. Yields each underlying rope chunk clipped to `range`, instead
/// of materializing the node's bytes into a fresh `Vec` per predicate per node.
///
/// tree-sitter concatenates a multi-chunk node into its own reused internal buffer
/// (`QueryCursor`'s `buffer1`/`buffer2`) and compares a single contiguous `&[u8]`,
/// so the bytes the predicate sees are identical regardless of how the node is
/// split across chunks; a single-chunk node is compared in place with no copy at
/// all. The lifetime ties the chunks to the `rope` borrow, which outlives the
/// cursor iteration.
struct NodeChunks<'a> {
    rope: &'a Rope,
    pos: usize,
    end: usize,
}

impl<'a> Iterator for NodeChunks<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.end {
            return None;
        }
        let (chunk, start) = self.rope.chunk(self.pos);
        if chunk.is_empty() {
            return None;
        }
        let from = self.pos - start;
        let to = (self.end - start).min(chunk.len());
        // `chunk(pos)` returns the chunk containing `pos`, so `from < to` here
        // whenever `pos < end` (the slice is non-empty); advance past this chunk.
        self.pos = start + chunk.len();
        Some(&chunk.as_bytes()[from..to])
    }
}

/// The borrowed-chunk [`tree_sitter::TextProvider`] over `rope`: maps each node a
/// predicate consults to its bytes as a chunk iterator, allocating nothing.
fn node_text_provider(rope: &Rope) -> impl tree_sitter::TextProvider<&[u8]> + '_ {
    |node: Node| NodeChunks {
        rope,
        pos: node.start_byte(),
        end: node.end_byte(),
    }
}

/// A tree-sitter parse progress callback that cancels the parse once it has run
/// past `deadline` (measured from `started`) — the in-process replacement for the
/// worker's "never stalls the UI" property. Shared by the root reparse, the
/// injection-layer parses, and the off-buffer preview parse so the budget logic
/// lives in one place. Bind the result to a `let mut` and pass it to
/// `ParseOptions::progress_callback` (it must outlive the `ParseOptions`).
fn deadline_budget(
    started: Instant,
    deadline: Duration,
) -> impl FnMut(&tree_sitter::ParseState) -> ControlFlow<()> {
    move |_: &tree_sitter::ParseState| {
        if started.elapsed() >= deadline {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }
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

/// Whether a query name is one the engine itself compiles + executes (the
/// paint-relevant names), so a resolution-bridge push for it lands on the grammar.
/// `highlights` and `indents` drive the paint directly; `injections` drives the
/// sub-language layers built on top of the root tree; `folds` drives the
/// `foldmethod=expr` tree-sitter fold source; `textobjects` drives the tree-sitter
/// text objects (`vif`, `daf`, …). Every other resolved name stays Lua-side and is
/// a no-op here.
fn is_engine_query(name: &str) -> bool {
    matches!(
        name,
        "highlights" | "indents" | "injections" | "folds" | "textobjects"
    )
}
