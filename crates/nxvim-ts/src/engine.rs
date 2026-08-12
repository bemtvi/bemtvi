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

/// Wall-clock budget for **all** of a buffer's child (injection) work on one
/// refresh — grammar loads included — the injection analogue of [`PARSE_DEADLINE`].
/// Injected regions reparse per edit, so an adversarial config (many regions, or a
/// pathological child grammar) could otherwise stall the edit path. On expiry the
/// remaining child parses are cancelled and their last-good (edit-shifted) trees are
/// kept, so the cost is one frame of stale injected highlights rather than a hang.
///
/// A region with no last-good tree — a `<script lang="ts">` on the frame the file
/// opened — is not lost by expiry: it is kept pending and resumed a budget at a time
/// on later frames ([`PendingInjection`]), so it colours in shortly *after* the file
/// paints instead of holding the paint up.
const INJECTION_DEADLINE: Duration = Duration::from_millis(50);

/// How deep injected layers may nest (host → injected → injected-within-injected →
/// …). Markdown → rust → regex is two levels; real configs rarely exceed three.
/// The bound caps a pathological or cyclic config (e.g. a self-injection that keeps
/// finding regions) from building unbounded layers each frame; past it, deeper
/// regions are dropped.
const MAX_INJECTION_DEPTH: usize = 4;

/// How many lines a doc block may hold and still be tried as a **list of items**
/// (see [`Engine::highlight_fragment`]). Every line of a list costs its own ladder,
/// so the bound is what keeps a long block — which, being long, is far likelier to
/// be real source that simply failed to parse than a list of overloads — from
/// turning a doc float's repaint into a parse storm. Typeshed's most-overloaded
/// signatures run to a couple of dozen lines.
const MAX_SPLIT_ITEMS: usize = 64;

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
    /// The `(language, ranges)` region-sets [`injections`](Self::injections) was
    /// built from — kept so an edit can update them **incrementally** instead of
    /// re-running the host's injection query over the whole tree. See
    /// [`Engine::update_injection_layers`].
    injection_regions: Vec<InjectionRegion>,
    /// Child parses [`INJECTION_DEADLINE`] cancelled with no previous tree to fall
    /// back on — the injection analogue of [`incomplete`](Self::incomplete). Each
    /// keeps its own `Parser`, which still holds the outstanding parse, so the next
    /// [`Engine::highlights`] *resumes* it a budget further rather than restarting
    /// it; an injected region too big for one frame colours in progressively, the
    /// way the host tree already does, instead of waiting for the next edit.
    pending_injections: Vec<PendingInjection>,
    /// Lines a full-line-background capture ([`LINE_BACKGROUND_GROUPS`]) touched in
    /// the most recent [`Engine::highlights`] call — read back by the server via
    /// [`line_background_lines`](nxvim_core::syntax::SyntaxEngine::line_background_lines)
    /// to paint the `line_bg` layer under a markdown fenced code block.
    line_bg_lines: Vec<usize>,
}

/// A child parse still in flight: cancelled by [`INJECTION_DEADLINE`] before it
/// produced a tree, and kept whole so the next refresh can resume it.
///
/// The `parser` is the point of this type. tree-sitter retains a cancelled parse on
/// the parser that ran it, so calling `parse` again continues from where the budget
/// ran out — dropping the parser instead (what the engine used to do) would restart
/// the region from scratch every frame and, for a region genuinely larger than one
/// budget, never finish. The rest of the fields are what
/// [`Engine::build_injection_layers`] matches it back to its region by.
///
/// A *nested* pending region needs no depth or injector stored alongside it: the
/// pass that resumes it re-derives its parent layer first (from the parent's own
/// tree, so no reparse), and the parent's injection query re-enqueues the child at
/// the right depth and under the right injector, where `(language, ranges)` finds
/// this parser again.
struct PendingInjection {
    /// The injected language (normalized), half of the key matching it to a region.
    language: String,
    /// The buffer byte ranges the parse covers, already merged and ascending — the
    /// other half of the key, so a region the text moved gets a fresh parse rather
    /// than one resumed against bytes it never started on.
    ranges: Vec<Range<usize>>,
    /// Holds the outstanding parse; `parse` on it resumes rather than restarts.
    /// `None` for a region the pass deferred *before* starting it — a first region
    /// of a language whose grammar was still uncached when the frame's budget ran
    /// out (see [`Engine::build_injection_layers`]). It gets a fresh parser when the
    /// next frame reaches it.
    parser: Option<Parser>,
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
    /// Per-language **fragment contexts**: the framings
    /// [`Engine::highlight_fragment`] wraps a snippet in when it doesn't parse on
    /// its own, in order of preference. Set from Lua
    /// (`nx.treesitter.fragment_context`), which also ships the defaults.
    fragment_contexts: HashMap<String, Vec<FragmentContext>>,
    /// The languages the most recent **stateless** highlight
    /// ([`Engine::highlight_text_bg`] / [`Engine::highlight_fragment`]) injected,
    /// read back by
    /// [`text_injected_languages`](Engine::text_injected_languages). Stashed rather
    /// than returned for the same reason [`BufferState::line_bg_lines`] is: the
    /// callers go through the [`SyntaxEngine`] trait, whose return types the wasm
    /// implementor shares.
    last_text_injections: Vec<String>,
}

/// One framing a fragment can be parsed inside — a template split at its `%s`.
/// `"struct __nx {\n%s\n}"` becomes `prefix = "struct __nx {\n"`, `suffix =
/// "\n}"`, and the line/column offsets the fragment's spans must be shifted back
/// by.
#[derive(Clone, Debug)]
struct FragmentContext {
    prefix: String,
    suffix: String,
    /// Newlines in `prefix` — the fragment's first line in wrapped coordinates.
    line_offset: usize,
    /// Bytes of `prefix` after its last newline — the column the fragment starts at.
    /// Zero for the usual newline-terminated prefix.
    col_offset: usize,
    /// Set when what follows the prefix's last newline is **pure indentation**
    /// (`"class __nx:\n    %s"`). Then the opener isn't something the first line
    /// merely continues — it's the block level the *whole* fragment sits at, so it
    /// is repeated on every line and every line's columns shift back by it. Without
    /// it a multi-line fragment in an indentation-sensitive language (python) would
    /// be framed as `class __nx:` + one indented line + a dedent, which is a syntax
    /// error rather than a block. `None` for a same-line opener (`"return %s"`),
    /// which applies to the first line only.
    indent: Option<String>,
}

impl FragmentContext {
    /// Split `template` at its first `%s`. `None` when the template has no `%s` —
    /// it would wrap nothing, so it is not a framing.
    fn parse(template: &str) -> Option<Self> {
        let (prefix, suffix) = template.split_once("%s")?;
        let opener = &prefix[prefix.rfind('\n').map_or(0, |i| i + 1)..];
        Some(FragmentContext {
            line_offset: prefix.matches('\n').count(),
            col_offset: opener.len(),
            indent: (!opener.is_empty() && opener.bytes().all(|b| b == b' ' || b == b'\t'))
                .then(|| opener.to_string()),
            prefix: prefix.to_string(),
            suffix: suffix.to_string(),
        })
    }

    /// Put `text` inside this framing. In [indent](Self::indent) mode every line
    /// after the first is indented to match (the prefix already indents the first);
    /// a blank line is left alone rather than given trailing whitespace.
    ///
    /// The result always ends in a newline. A template's suffix usually closes a
    /// block (`"\n}"`), and some grammars want a terminator after the last
    /// declaration — tree-sitter-go reports a `MISSING` one, which is a defect, so
    /// without this a Go struct-field framing could never win despite producing a
    /// perfect tree.
    fn wrap(&self, text: &str) -> String {
        let mut out = String::with_capacity(self.prefix.len() + text.len() + self.suffix.len() + 1);
        out.push_str(&self.prefix);
        match self.indent.as_deref() {
            None => out.push_str(text),
            Some(indent) => {
                for (i, line) in text.split_inclusive('\n').enumerate() {
                    if i > 0 && !line.trim().is_empty() {
                        out.push_str(indent);
                    }
                    out.push_str(line);
                }
            }
        }
        out.push_str(&self.suffix);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out
    }
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
            fragment_contexts: HashMap::new(),
            last_text_injections: Vec::new(),
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
        crate::loader::resolve_query(self.root_for(lang), lang, name).map_err(|e| {
            format!(
                "reading {}: {e}",
                query_path(self.root_for(lang), lang, &format!("{name}.scm")).display()
            )
        })
    }

    /// The **single-file** base for `(lang, name)`: this language's own query with
    /// no `; inherits:` resolution — the raw link the server needs when a
    /// runtimepath file replaces one language of a chain.
    pub fn base_query_raw(&self, lang: &str, name: &str) -> Result<Option<String>, String> {
        crate::loader::read_one_query(self.root_for(lang), lang, name).map_err(|e| {
            format!(
                "reading {}: {e}",
                query_path(self.root_for(lang), lang, &format!("{name}.scm")).display()
            )
        })
    }

    /// The languages `(lang, name)`'s on-disk query inherits, transitively, in merge
    /// order (deepest ancestor first, `lang` excluded) — the chain
    /// [`read_disk_query`](Self::read_disk_query) already folded into the base.
    ///
    /// The server reads this to pull the **runtimepath** overlays of the same
    /// languages: a config's `queries/ecma/injections.scm` has to reach a javascript
    /// buffer, and only the server can see the runtimepath.
    pub fn query_inherits(&self, lang: &str, name: &str) -> Vec<String> {
        crate::loader::query_inherits(self.root_for(lang), lang, name)
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

    /// Advance the child parses [`INJECTION_DEADLINE`] cut short, one budget further.
    ///
    /// Called from [`highlights`](Self::highlights) — so the server's "repaint while
    /// [`parse_pending`](Self::parse_pending)" loop drives it, exactly as it already
    /// drives the root parse's resumption. The regions come from the cache rather
    /// than a fresh query: this path runs only when the text has not changed, so
    /// re-running the host's injection query would rediscover the same set at the
    /// cost of a whole-tree walk.
    fn resume_pending_injections(&mut self, buffer: BufferId) {
        let Some(state) = self.buffers.get_mut(&buffer) else {
            return;
        };
        let regions = state.injection_regions.clone();
        // The layers that already landed, handed back as their own parse hints so
        // this pass re-derives them from their trees instead of reparsing them.
        let mut old_by_lang: HashMap<String, Vec<Tree>> = HashMap::new();
        for layer in std::mem::take(&mut state.injections) {
            old_by_lang
                .entry(layer.language)
                .or_default()
                .push(layer.tree);
        }
        self.build_injection_layers(buffer, regions, old_by_lang);
    }

    /// Re-derive a buffer's injected child layers **incrementally** after an edit.
    /// Each surviving child tree is `edit`ed with this frame's deltas and reused as
    /// the parse hint for the region of its language, so unchanged subtrees are not
    /// reparsed (and it doubles as the last-good fallback under the parse budget).
    fn update_injection_layers(
        &mut self,
        buffer: BufferId,
        edits: &[InputEdit],
        before: Option<&Tree>,
    ) {
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
        let regions = self
            .incremental_injection_regions(buffer, edits, before)
            .unwrap_or_else(|| self.top_level_injection_regions(buffer));
        self.build_injection_layers(buffer, regions, old_by_lang);
    }

    /// Update the cached top-level injection regions for `buffer` from what the edit
    /// changed, or `None` when that cannot be done soundly and the caller must fall
    /// back to the full walk.
    ///
    /// **Why this exists.** The full walk runs the host grammar's injection query over
    /// the entire tree, and it ran on *every edit*. On a 2000-line rust file, an
    /// injection query matching nothing in it still cost 15x the same edits with no
    /// query at all — and the cost grows with the file
    /// (`docs/plans/2026-08-08-per-keystroke-costs-round-2.md`).
    ///
    /// **Why not clip the query to the viewport**, the way `extract_spans` does? Two
    /// reasons the highlight path does not have. A region's identity would churn on
    /// every scroll, and [`build_injection_layers`] hands a region's previous tree to
    /// tree-sitter as the incremental parse hint — a hint from an unrelated region
    /// yields a *wrong* parse, not a slow one. And a combined region-set spans the
    /// whole document by construction, so a viewport would cut it in half.
    ///
    /// **The shape used instead.** A region outside every dirty range is, by
    /// construction, unaffected by the edit: its nodes did not change and the text its
    /// predicates read did not change. So shift the cached regions through the edits,
    /// drop the ones the dirty set touches, re-run the query restricted to the dirty
    /// ranges, and union the results back. Regions keep their identity, which is what
    /// keeps the parse hints valid.
    ///
    /// Returns `None` — deferring to the full walk — when the query contains any
    /// `injection.combined` pattern, because such a set is accumulated *across*
    /// matches spread over the document and a partial re-derivation would produce a
    /// partial set. Of the languages that ship queries here only markdown has one, so
    /// this costs almost nothing in practice, and it fails to today's behavior rather
    /// than to a wrong one.
    fn incremental_injection_regions(
        &self,
        buffer: BufferId,
        edits: &[InputEdit],
        before: Option<&Tree>,
    ) -> Option<Vec<InjectionRegion>> {
        let state = self.buffers.get(&buffer)?;
        let tree = state.tree.as_ref()?;
        let Some(Slot::Loaded(host)) = self.grammars.get(&state.language) else {
            return None;
        };
        let query = host.injections.as_ref()?;
        if query_has_combined(query) {
            return None;
        }
        // Computed only now, past every early-out: `changed_ranges` walks both trees,
        // so a language that ships no injection query (or a combined one, which takes
        // the full walk anyway) must not pay for it.
        let dirty = Self::dirty_ranges(before, Some(tree), edits);
        let dirty = &dirty[..];
        // Survivors: cached regions shifted onto the new text, minus anything the
        // dirty set reaches.
        let touches_dirty =
            |r: &Range<usize>| dirty.iter().any(|d| r.start < d.end && d.start < r.end);
        let mut regions: Vec<InjectionRegion> = state
            .injection_regions
            .iter()
            .map(|r| InjectionRegion {
                language: r.language.clone(),
                ranges: r.ranges.iter().map(|x| shift_range(x, edits)).collect(),
                extent: shift_range(&r.extent, edits),
            })
            // Dropped on the **extent**, not on the injected ranges: the match may
            // have read text outside what it injects (markdown's fence language is
            // the shipping example), and that text changing changes the match.
            .filter(|r| !touches_dirty(&r.extent))
            .collect();
        // Re-derive within each dirty range. A match is returned when it *intersects*
        // the range, and its captured nodes are reported whole, so a region straddling
        // the edge comes back complete rather than clipped.
        for d in dirty {
            for found in collect_injection_regions_in(
                query,
                tree,
                &state.shadow,
                Some(&state.language),
                None,
                Some(d.clone()),
            ) {
                // A match can be found from more than one dirty range, and one whose
                // content sits outside the dirty set may survive above as well.
                if !regions.contains(&found) {
                    regions.push(found);
                }
            }
        }
        // Document order, so the layer vector (and therefore the painter's precedence
        // between same-depth layers) does not depend on which regions happened to be
        // re-derived this edit.
        regions.sort_by_key(|r| r.ranges.first().map_or(usize::MAX, |x| x.start));
        Some(regions)
    }

    /// Run the host grammar's injection query over the root tree and resolve the
    /// matches to `(language, ranges)` region-sets — the top (depth-1) of the layer
    /// tree. The host is the query's `injection.self`; it has no parent. `&self` (no
    /// grammar load), so it can borrow the buffer + grammar caches together and
    /// return owned data the caller then builds with `&mut self`.
    fn top_level_injection_regions(&self, buffer: BufferId) -> Vec<InjectionRegion> {
        let Some(state) = self.buffers.get(&buffer) else {
            return Vec::new();
        };
        let Some(tree) = state.tree.as_ref() else {
            return Vec::new();
        };
        match self.grammars.get(&state.language) {
            Some(Slot::Loaded(host)) => match host.injections.as_ref() {
                Some(query) => collect_injection_regions_in(
                    query,
                    tree,
                    &state.shadow,
                    Some(&state.language),
                    None,
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
        regions: Vec<InjectionRegion>,
        mut old_by_lang: HashMap<String, Vec<Tree>>,
    ) {
        // The host language injected the top-level regions; it is their parent for
        // the `injection.parent` directive when those layers are queried in turn.
        let Some(host_lang) = self.buffers.get(&buffer).map(|s| s.language.clone()) else {
            return;
        };
        // Whatever the last refresh left mid-parse. A region that still exists takes
        // its parser back below and resumes; one that doesn't drops with this vec.
        let mut resumable: Vec<PendingInjection> = Vec::new();
        if let Some(state) = self.buffers.get_mut(&buffer) {
            state.injection_regions = regions.clone();
            resumable = std::mem::take(&mut state.pending_injections);
        }
        let started = Instant::now();
        // One wall-clock budget for the whole pass, load time included: what this
        // bounds is the *frame*, and a cold `dlopen` plus a compile of every `.scm` a
        // language ships is as real a stall as a pathological parse (hundreds of ms
        // for typescript). A language's first region therefore usually spends the
        // whole budget arriving and parses nothing — that is fine and is not the same
        // as being dropped: it is kept as a [`PendingInjection`], `parse_pending`
        // reports it, and the next frame resumes it with a full budget against a
        // grammar that is now cached. The region colours in a frame or two after the
        // file paints rather than stalling the paint it belongs to.
        let mut layers = Vec::with_capacity(regions.len());
        let mut still_pending: Vec<PendingInjection> = Vec::new();
        // (language, ranges, depth, injector) — `injector` is the language that
        // injected this region, used as `injection.parent` when recursing into it.
        let mut queue: VecDeque<(String, Vec<Range<usize>>, usize, String)> = regions
            .into_iter()
            .map(|r| (r.language, r.ranges, 1, host_lang.clone()))
            .collect();

        while let Some((language, mut ranges, depth, injector)) = queue.pop_front() {
            // `included_ranges` must be ascending and non-overlapping. A combined
            // pattern can match *nested* nodes (a section inside a section), whose
            // ranges overlap — passed through raw, `set_included_ranges` would
            // reject them and the whole layer would silently drop. Merge each
            // overlap into its union (identical coverage for both the child parse
            // and the painter's clipping). Merged before the load below so a region
            // deferred there keys the same way as one that got this far.
            ranges.sort_by_key(|r| r.start);
            ranges.dedup_by(|next, prev| {
                if next.start <= prev.end {
                    prev.end = prev.end.max(next.end);
                    true
                } else {
                    false
                }
            });

            // A grammar not yet cached costs a `dlopen` plus a compile of every
            // `.scm` it ships — hundreds of ms for typescript, none of it
            // interruptible. So no load *starts* on a frame that has already spent
            // its budget: the region it belongs to can't be parsed on that frame
            // anyway, and a document with fenced code in eight languages would
            // otherwise pay all eight loads at once. Deferring costs nothing — the
            // region stays pending, so the next frame comes right back and spends
            // its budget here — and the file keeps painting while its injections
            // arrive over the frames after it.
            //
            // Keyed on *unattempted*, not on "not loaded": a language whose load
            // already failed (or that isn't installed) is a cached lookup below,
            // costs nothing, and drops the region — deferring that one instead would
            // leave it pending forever and the server repainting to resolve it.
            let cold = !self.grammars.contains_key(&language);
            if cold && started.elapsed() >= INJECTION_DEADLINE {
                still_pending.push(PendingInjection {
                    language,
                    ranges,
                    parser: None,
                });
                continue;
            }
            // Lazily load (cache) the child grammar; skip silently if it is missing
            // or broken — the region just keeps the host's flat paint.
            let child_language = match self.grammar(&language) {
                Slot::Loaded(g) => g.language.clone(),
                _ => continue,
            };
            // The parser that already holds this region's cancelled parse, if the
            // last refresh left one — resuming it continues from where the budget
            // ran out, where a fresh parser would restart the region from scratch.
            // Keyed by the merged ranges, so a region the edit moved gets a fresh
            // parse rather than one resumed against the wrong text.
            let resumed = resumable
                .iter()
                .position(|p| p.language == language && p.ranges == ranges)
                .and_then(|i| resumable.remove(i).parser);
            let resuming = resumed.is_some();
            let mut parser = resumed.unwrap_or_else(Parser::new);
            if !resuming && parser.set_language(&child_language).is_err() {
                continue;
            }
            // An edit-shifted tree of this language, reused as the incremental parse
            // hint and the stale fallback if this frame's parse is cancelled. Never
            // handed to a *resumed* parse: that parse already fixed its old tree when
            // it started, and swapping one in mid-flight would parse against a hint
            // it never began from.
            let old = match resuming {
                true => None,
                false => old_by_lang.get_mut(&language).and_then(Vec::pop),
            };

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
                // has nothing to paint, so its parse is kept alive instead — the next
                // refresh resumes it a budget further (see [`PendingInjection`]),
                // which is what makes a region larger than one budget colour in over
                // a few frames rather than wait for an edit that rebuilds it.
                None => {
                    match old {
                        Some(tree) => layers.push(InjectionLayer {
                            language,
                            tree,
                            ranges,
                        }),
                        None => still_pending.push(PendingInjection {
                            language,
                            ranges,
                            parser: Some(parser),
                        }),
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
            state.pending_injections = still_pending;
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
    ///
    /// `lang` is a filetype, which may be an *alias* of the grammar's own noun
    /// (`sh` → `bash`, `jsonc` → `json`); it is resolved here so everything the
    /// engine stores and looks up downstream — `BufferState::language`, the grammar
    /// cache key, [`language_of`](Self::language_of) — is the canonical grammar name.
    pub fn open(&mut self, buffer: BufferId, lang: &str, text: &str) -> OpenOutcome {
        let lang = nxvim_core::resolve_language(lang);
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
            injection_regions: Vec::new(),
            pending_injections: Vec::new(),
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
        // Same hazard one layer down: a child parse still in flight was reading the
        // *pre-edit* shadow, so resuming it now would parse stale bytes. Drop them —
        // the rebuild below re-derives the regions and starts their parses fresh.
        state.pending_injections.clear();
        // The edited-but-not-yet-reparsed tree, kept to ask tree-sitter which byte
        // ranges the reparse actually *changed* — the input to the incremental
        // injection update below. Cloning a `Tree` is a refcount bump, not a copy.
        let before = state.tree.clone();
        state.reparse();
        // The injected regions move with every edit, so re-derive the child layers
        // from the fresh root tree — incrementally, replaying `applied` onto the
        // surviving child trees. `state`'s borrow ends at the line above.
        self.update_injection_layers(buffer, &applied, before.as_ref());
    }

    /// The byte ranges an edit could have changed the *injection structure* of:
    /// tree-sitter's `changed_ranges` between the pre- and post-reparse trees, plus
    /// the ranges the edits themselves wrote.
    ///
    /// The union is not belt-and-braces. `changed_ranges` reports where the **syntax**
    /// differs, so a same-shape token substitution (`"a"` → `"b"` inside a string) can
    /// leave it empty while changing the very text an injection query's `#eq?` /
    /// `#match?` / `#lua-match?` predicate reads. Adding the written ranges covers
    /// that; leaving them out would let an injection silently fail to appear.
    fn dirty_ranges(
        before: Option<&Tree>,
        after: Option<&Tree>,
        edits: &[InputEdit],
    ) -> Vec<Range<usize>> {
        let mut out: Vec<Range<usize>> = edits
            .iter()
            .map(|e| e.start_byte..e.new_end_byte.max(e.start_byte))
            .collect();
        if let (Some(before), Some(after)) = (before, after) {
            out.extend(
                before
                    .changed_ranges(after)
                    .map(|r| r.start_byte..r.end_byte.max(r.start_byte)),
            );
        }
        // Merge into a minimal ascending set, so the query below runs once per
        // genuinely separate region rather than once per overlapping report.
        out.sort_by_key(|r| r.start);
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(out.len());
        for r in out {
            match merged.last_mut() {
                Some(prev) if r.start <= prev.end => prev.end = prev.end.max(r.end),
                _ => merged.push(r),
            }
        }
        merged
    }

    /// Forget a buffer's shadow text and parse tree (the editor deleted it).
    pub fn close(&mut self, buffer: BufferId) {
        self.buffers.remove(&buffer);
    }

    /// Whether `buffer` still has parse work pending — the root parse cancelled by
    /// [`PARSE_DEADLINE`] mid-way, or an injected region's child parse cancelled by
    /// [`INJECTION_DEADLINE`]. The server polls this after each redraw to decide
    /// whether to schedule another frame, which resumes the outstanding parses via
    /// [`Self::highlights`], until they converge. False for an unknown buffer or a
    /// fully-parsed one.
    ///
    /// Injections count because the server's highlight memo hits on every frame that
    /// changed neither the text nor the viewport: without this, a child parse the
    /// budget cut short would never be resumed and the injected language would stay
    /// unpainted until the next edit.
    pub fn parse_pending(&self, buffer: BufferId) -> bool {
        self.buffers
            .get(&buffer)
            .is_some_and(|s| s.incomplete || !s.pending_injections.is_empty())
    }

    /// Whether a buffer is known (opened) and which language it uses.
    pub fn language_of(&self, buffer: BufferId) -> Option<&str> {
        self.buffers.get(&buffer).map(|b| b.language.as_str())
    }

    /// Every language `buffer` currently has an injected layer for, deepest nesting
    /// included and deduplicated — the typescript of a vue file's `<script setup
    /// lang="ts">`, the rust of a markdown fence.
    ///
    /// The set is only knowable *after* a parse: an injected language usually comes
    /// from the document itself (`lang="ts"` is node text, not a constant in the
    /// query), so nothing upstream can predict it. The server reads this right after
    /// [`highlights`](Self::highlights) to resolve those languages' runtimepath
    /// queries, which would otherwise reach only a language that is some buffer's own
    /// filetype. A region whose parse is still pending counts: its layer is coming,
    /// and resolving its queries now saves re-painting it later.
    pub fn injected_languages(&self, buffer: BufferId) -> Vec<String> {
        let Some(state) = self.buffers.get(&buffer) else {
            return Vec::new();
        };
        let mut out: Vec<String> = Vec::new();
        let injected = state.injections.iter().map(|l| &l.language);
        let pending = state.pending_injections.iter().map(|p| &p.language);
        for lang in injected.chain(pending) {
            if !out.iter().any(|l| l == lang) {
                out.push(lang.clone());
            }
        }
        out
    }

    /// [`injected_languages`](Self::injected_languages) for the most recent
    /// **stateless** highlight — [`highlight_text`](Self::highlight_text),
    /// [`highlight_text_bg`](Self::highlight_text_bg) or
    /// [`highlight_fragment`](Self::highlight_fragment). Those surfaces (the picker
    /// preview, an LSP doc float, `nx.treesitter.highlight`) inject exactly as an
    /// open buffer does but own no [`BufferId`] to key off, so the languages are
    /// stashed by the call and read back immediately after it.
    pub fn text_injected_languages(&self) -> Vec<String> {
        self.last_text_injections.clone()
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
        } else if self
            .buffers
            .get(&buffer)
            .is_some_and(|s| !s.pending_injections.is_empty())
        {
            // A child parse the budget cut short. Re-run the build over the *cached*
            // regions (the text has not changed, so they still hold): each pending
            // region takes its parser back and resumes a budget further, while the
            // layers that already landed are re-derived from their own trees, which
            // with no edits to replay is a refcount bump rather than a parse.
            self.resume_pending_injections(buffer);
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
            extract_spans(&layers, &state.shadow, first_line, last_line, None)
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
    /// reuse. For the picker preview pane, which paints a file that is not an open
    /// buffer. Returns the host + injected highlight spans in `text` coordinates,
    /// or empty when no grammar is installed for `lang` or the parse is cancelled.
    pub fn highlight_text(
        &mut self,
        lang: &str,
        text: &str,
        first_line: usize,
        last_line: usize,
    ) -> Vec<Span> {
        self.highlight_text_bg(lang, text, first_line, last_line).0
    }

    /// [`highlight_text`](Self::highlight_text) plus the 0-based lines a
    /// full-line-background capture (`@markup.raw.block` — a fenced code block)
    /// touches. The block background must be painted as a separate under-layer, not
    /// left in the per-cell spans: the winner-takes-cell merge (and, in a `>lua`
    /// block, the injected token spans) would otherwise overwrite it on every
    /// non-blank cell, leaving the background only on the whitespace between tokens.
    /// The caller (the preview projection) tints those lines the way a window's
    /// `line_hl_group` does. Empty background list when no grammar / no block.
    pub fn highlight_text_bg(
        &mut self,
        lang: &str,
        text: &str,
        first_line: usize,
        last_line: usize,
    ) -> (Vec<Span>, Vec<usize>) {
        self.highlight_snippet(lang, text, first_line, last_line, false)
    }

    /// [`highlight_text`](Self::highlight_text) for a snippet that is **not a whole
    /// program** — a fenced code block inside LSP documentation (hover, completion
    /// docs). Those blocks are either a *fragment* of the language (a struct field, a
    /// bare statement, a signature with no body) or an annotation dialect the server
    /// invented for display: `lua_ls` puts `function f(t: table)` in a ` ```lua `
    /// fence, `tsserver` prefixes `(method) `. Neither is source the grammar can
    /// parse.
    ///
    /// Handed to the whole-file path, the second kind does not merely degrade — it
    /// comes out **confidently wrong**, because a structural query matched a
    /// construct that isn't there (`Vec` in `field: Vec<String>` paints as
    /// `constructor`; the `lua_ls` hover loses its `function` keyword and paints the
    /// *types* as parameters). Plausible-but-wrong colour reads worse than none.
    ///
    /// So fragment mode works in steps, each of which either makes the snippet a
    /// **whole parse** or hands on to the next.
    ///
    /// **The framing ladder.** A snippet that doesn't parse on its own is tried
    /// inside each of its language's [fragment contexts](Self::set_fragment_context)
    /// in turn — `field: Vec<String>` inside `struct __nx { … }`, a bare statement
    /// inside `fn __nx() { … }`. The *first framing that parses cleanly* wins, and
    /// its spans are mapped back into the snippet's own coordinates. Only a clean
    /// parse is accepted: the point is to turn a broken parse into a whole one, and a
    /// framing that merely fails *differently* would just relocate the guesswork.
    /// Structure recovered this way is real — the wrapped text genuinely is a
    /// program, so `Vec` comes back as `@type`, not `@constructor`.
    ///
    /// **The annotation peel.** A leading `(kind) ` is the server's own display
    /// label, not code — `pyright` writes `(method) def join(self, x: str) -> str`,
    /// `tsserver` `(property) Foo.bar: number` — and it is what stops an otherwise
    /// framable signature from framing. So when the ladder comes up empty, the label
    /// is [taken off](annotation_prefix) and the ladder runs again on what's left;
    /// its spans shift back by the label's width and the label itself is painted
    /// `comment`, the non-code text it is. All or nothing: if the remainder doesn't
    /// frame either, the snippet goes on to the next step whole, with no label span
    /// invented over text nothing explained.
    ///
    /// **The item split.** A doc block is often a *list* rather than one fragment —
    /// `ty` sends every overload of a function as its own signature line. Together
    /// they are a fragment of nothing and no framing takes them, so each line is
    /// resolved in its own right instead (its own ladder, its own peel, possibly a
    /// different rung). All or nothing again: one line that isn't a whole item means
    /// this isn't a list, and forcing the rest would paint them out of a context the
    /// parse says isn't there.
    ///
    /// **The repaint**, when nothing above made it whole (an annotation dialect is
    /// not a fragment of anything). Then fragment mode trusts structure only where
    /// the parse is sound: every `ERROR` region of the host tree has its structural
    /// captures dropped — the token-classifying ones survive, the lexer having worked
    /// where the parser didn't — and is repainted from the leaves' own token kinds
    /// (see [`fragment_repaint`]).
    ///
    /// A snippet that parses cleanly on its own — most rust-analyzer hovers, every
    /// `:help` example — takes none of the steps and is byte-identical to
    /// [`highlight_text`](Self::highlight_text).
    pub fn highlight_fragment(
        &mut self,
        lang: &str,
        text: &str,
        first_line: usize,
        last_line: usize,
    ) -> Vec<Span> {
        if let Some(spans) = self.resolve_fragment(lang, text, first_line, last_line) {
            return spans;
        }
        if let Some(spans) = self.split_fragment(lang, text, first_line, last_line) {
            return spans;
        }
        self.highlight_snippet(lang, text, first_line, last_line, true)
            .0
    }

    /// The whole snippet resolved as **one** item — a clean parse, a framing, or a
    /// framing of what's left once its display annotation is peeled off. `None` when
    /// none of those makes it whole, which is what sends
    /// [`highlight_fragment`](Self::highlight_fragment) on to the item split and then
    /// the repaint.
    fn resolve_fragment(
        &mut self,
        lang: &str,
        text: &str,
        first_line: usize,
        last_line: usize,
    ) -> Option<Vec<Span>> {
        // A snippet that stands on its own needs no step at all. `parses_cleanly` is a
        // throwaway parse, but a doc block is a handful of lines and this only runs
        // for a surface that just changed (a hover reply, a completion row).
        if self.parses_cleanly(lang, text) {
            return Some(
                self.highlight_snippet(lang, text, first_line, last_line, false)
                    .0,
            );
        }
        for context in self
            .fragment_contexts
            .get(nxvim_core::resolve_language(lang))
            .cloned()
            .unwrap_or_default()
        {
            let wrapped = context.wrap(text);
            if !self.parses_cleanly(lang, &wrapped) {
                continue;
            }
            // The wrapped text is a whole program, so it highlights through the
            // ordinary path; only the coordinates need bringing home.
            let lines = line_lengths(text);
            let spans = self
                .highlight_snippet(
                    lang,
                    &wrapped,
                    first_line + context.line_offset,
                    last_line + context.line_offset,
                    false,
                )
                .0;
            return Some(unwrap_spans(spans, &context, &lines));
        }
        self.peel_annotation(lang, text, first_line, last_line)
    }

    /// Take the server's display annotation (`(method) `, `(type alias) `) off the
    /// snippet's first line and resolve what's left, bringing its first-line columns
    /// back over the label and painting the label `comment`. `None` when there is no
    /// label, or when the remainder doesn't resolve either — a peel that explains
    /// nothing must leave no trace.
    ///
    /// A label the recursion strips is one the shorter text no longer carries, so a
    /// doubly-labelled snippet terminates, and each label lands at its true column.
    fn peel_annotation(
        &mut self,
        lang: &str,
        text: &str,
        first_line: usize,
        last_line: usize,
    ) -> Option<Vec<Span>> {
        let (label, skip) = annotation_prefix(text)?;
        let mut spans = self.resolve_fragment(lang, &text[skip..], first_line, last_line)?;
        for span in &mut spans {
            if span.line == 0 {
                span.start_byte += skip;
                span.end_byte += skip;
            }
        }
        if first_line == 0 {
            spans.push(Span {
                line: 0,
                start_byte: 0,
                end_byte: label,
                group: "comment".to_string(),
            });
        }
        spans.sort_by_key(|s| (s.line, s.start_byte));
        Some(spans)
    }

    /// The snippet resolved as a **list of items**, one line each: every non-blank
    /// line must [resolve](Self::resolve_fragment) on its own, or this isn't a list
    /// and the caller falls back to the whole-block repaint. Spans come back on the
    /// line they were found on; lines outside `first_line..last_line` are still
    /// resolved (they're what makes it a list) but paint nothing.
    fn split_fragment(
        &mut self,
        lang: &str,
        text: &str,
        first_line: usize,
        last_line: usize,
    ) -> Option<Vec<Span>> {
        let lines: Vec<&str> = text.lines().collect();
        // One line is not a list, and a block long enough to be a parse storm is
        // source that should have parsed whole — the repaint is the cheap answer for
        // both. The cap keeps the worst case at `MAX_SPLIT_ITEMS` × the ladder.
        if lines.len() < 2 || lines.len() > MAX_SPLIT_ITEMS {
            return None;
        }
        let mut out = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue; // a blank line separates items; it has nothing to resolve
            }
            let item = format!("{line}\n");
            let spans = self.resolve_fragment(lang, &item, 0, 1)?;
            if index < first_line || index >= last_line {
                continue;
            }
            out.extend(spans.into_iter().map(|span| Span {
                line: index,
                ..span
            }));
        }
        (!out.is_empty()).then_some(out)
    }

    /// Whether `text` parses in `lang` with no `ERROR` and no `MISSING` node — the
    /// bar a [fragment context](Self::set_fragment_context) must clear. A language
    /// with no installed grammar reports `true`: there is nothing to be wrong about,
    /// and the caller paints nothing either way.
    fn parses_cleanly(&mut self, lang: &str, text: &str) -> bool {
        let lang = nxvim_core::resolve_language(lang);
        let language = match self.grammar(lang) {
            Slot::Loaded(g) => g.language.clone(),
            _ => return true,
        };
        let mut parser = Parser::new();
        if parser.set_language(&language).is_err() {
            return true;
        }
        let shadow = Rope::from_str(text);
        let started = Instant::now();
        let mut budget = deadline_budget(started, PARSE_DEADLINE);
        let options = ParseOptions::new().progress_callback(&mut budget);
        let mut callback = |byte: usize, _: Point| -> &[u8] { read_chunk(&shadow, byte) };
        match parser.parse_with_options(&mut callback, None, Some(options)) {
            // A cancelled parse is not evidence of a clean one — don't let a framing
            // win on a timeout.
            None => false,
            Some(tree) => !tree_has_defect(&tree),
        }
    }

    /// Install the **fragment contexts** for `lang` — the framings
    /// [`highlight_fragment`](Self::highlight_fragment) tries, in order, when a
    /// snippet doesn't parse on its own. Each is a template with one `%s` where the
    /// snippet goes (`"struct __nx {\n%s\n}"`); a template without `%s` is dropped
    /// (it would wrap nothing). Replaces any previous list for the language; an empty
    /// list turns the ladder off for it.
    pub fn set_fragment_context(&mut self, lang: &str, templates: Vec<String>) {
        let contexts: Vec<FragmentContext> = templates
            .iter()
            .filter_map(|t| FragmentContext::parse(t))
            .collect();
        self.fragment_contexts
            .insert(nxvim_core::resolve_language(lang).to_string(), contexts);
    }

    /// The shared body of [`highlight_text_bg`](Self::highlight_text_bg) and
    /// [`highlight_fragment`](Self::highlight_fragment): one stateless parse of
    /// `text`, its injected child layers, and the span extraction. `fragment` turns
    /// on the low-confidence repaint described on `highlight_fragment`.
    fn highlight_snippet(
        &mut self,
        lang: &str,
        text: &str,
        first_line: usize,
        last_line: usize,
        fragment: bool,
    ) -> (Vec<Span>, Vec<usize>) {
        // The host language is an alias-resolvable name too: this is fed a fence's
        // info string (a markdown doc float's code block) as often as a filetype.
        let lang = nxvim_core::resolve_language(lang);
        // Cleared up front so an early return below reports *this* call's (empty) set
        // rather than leaving the previous call's languages behind.
        self.last_text_injections.clear();
        let language = match self.grammar(lang) {
            Slot::Loaded(g) => g.language.clone(),
            _ => return (Vec::new(), Vec::new()), // silent: no grammar (or load failed)
        };
        let mut parser = Parser::new();
        if parser.set_language(&language).is_err() {
            return (Vec::new(), Vec::new());
        }
        let shadow = Rope::from_str(text);
        // Parse under the same wall-clock deadline as `reparse`, so a pathologically
        // large preview file can't stall the frame (it just renders plain).
        let started = Instant::now();
        let mut budget = deadline_budget(started, PARSE_DEADLINE);
        let options = ParseOptions::new().progress_callback(&mut budget);
        let mut callback = |byte: usize, _: Point| -> &[u8] { read_chunk(&shadow, byte) };
        let Some(tree) = parser.parse_with_options(&mut callback, None, Some(options)) else {
            return (Vec::new(), Vec::new());
        };
        // Fragment mode: the byte ranges this parse could not make sense of, plus the
        // token-level paint to put there in place of the host layer's guessed
        // structure. Computed here, before the tree moves into the layer list.
        let repaint = fragment.then(|| fragment_repaint(&tree, text));

        // Build the layer trees, host first, then injected children — a **stateless**
        // mirror of the buffer path's `build_injection_layers` (no `BufferId`, no
        // incremental reuse, no `line_bg` stash). Owned here for the duration so the
        // `Layer` borrows below stay valid; each child parses through its
        // `included_ranges`, so all trees share the one `shadow` coordinate space and
        // `extract_spans` paints a deeper (injected) layer over the shallower host.
        // Lets a help `>lua` block, a markdown fenced block, … show real per-language
        // tokens in the preview, exactly as in an open buffer.
        struct OwnedLayer {
            language: String,
            tree: Tree,
            ranges: Vec<Range<usize>>,
            depth: usize,
            /// The language that injected this layer (`injection.parent`); `None` for
            /// the host.
            injector: Option<String>,
        }
        let mut owned = vec![OwnedLayer {
            language: lang.to_string(),
            tree,
            ranges: Vec::new(), // the host covers the whole snippet — no clipping
            depth: 0,
            injector: None,
        }];
        // Breadth-first: index `i` walks the growing list, appending the regions each
        // layer injects. The whole child pass shares one `INJECTION_DEADLINE` budget,
        // so a pathological (or cyclic) config can't stall the preview frame.
        let mut i = 0;
        while i < owned.len() {
            let (this_lang, this_depth, injector) = {
                let l = &owned[i];
                (l.language.clone(), l.depth, l.injector.clone())
            };
            i += 1;
            if this_depth >= MAX_INJECTION_DEPTH {
                continue;
            }
            // Regions this layer injects, resolved through its own grammar's injection
            // query (`injection.self` = this layer's language, `injection.parent` = the
            // language that injected it).
            let regions = match self.grammars.get(&this_lang) {
                Some(Slot::Loaded(g)) => match g.injections.as_ref() {
                    Some(query) => collect_injection_regions(
                        query,
                        &owned[i - 1].tree,
                        &shadow,
                        Some(&this_lang),
                        injector.as_deref(),
                    ),
                    None => Vec::new(),
                },
                _ => Vec::new(),
            };
            for (child_lang, mut ranges) in regions {
                let child_language = match self.grammar(&child_lang) {
                    Slot::Loaded(g) => g.language.clone(),
                    _ => continue, // missing/broken child grammar → region keeps host paint
                };
                let mut child_parser = Parser::new();
                if child_parser.set_language(&child_language).is_err() {
                    continue;
                }
                // `included_ranges` must be ascending and non-overlapping — merge any
                // overlap into its union (same as the buffer path), or the layer drops.
                ranges.sort_by_key(|r| r.start);
                ranges.dedup_by(|next, prev| {
                    if next.start <= prev.end {
                        prev.end = prev.end.max(next.end);
                        true
                    } else {
                        false
                    }
                });
                let included: Vec<tree_sitter::Range> =
                    ranges.iter().map(|r| ts_range(&shadow, r)).collect();
                if included.is_empty() || child_parser.set_included_ranges(&included).is_err() {
                    continue;
                }
                let mut budget = deadline_budget(started, INJECTION_DEADLINE);
                let options = ParseOptions::new().progress_callback(&mut budget);
                let mut callback = |byte: usize, _: Point| -> &[u8] { read_chunk(&shadow, byte) };
                let Some(child_tree) =
                    child_parser.parse_with_options(&mut callback, None, Some(options))
                else {
                    continue; // budget exhausted / cancelled: region keeps host paint
                };
                owned.push(OwnedLayer {
                    language: child_lang,
                    tree: child_tree,
                    ranges,
                    depth: this_depth + 1,
                    injector: Some(this_lang.clone()),
                });
            }
        }

        // Stash the injected languages for
        // [`text_injected_languages`](Self::text_injected_languages), which the server
        // reads right after this call to resolve their runtimepath queries. The host
        // (index 0) is excluded: its caller resolved it before calling.
        self.last_text_injections.clear();
        for l in owned.iter().skip(1) {
            if !self.last_text_injections.contains(&l.language) {
                self.last_text_injections.push(l.language.clone());
            }
        }

        // Re-borrow each layer's compiled query immutably (the `&mut self` grammar
        // loads have ended) and assemble the layer list in owned order (host first, so
        // its rank is shallowest and children win over it).
        let layers: Vec<Layer> = owned
            .iter()
            .filter_map(|l| match self.grammars.get(&l.language) {
                Some(Slot::Loaded(g)) => Some(Layer {
                    query: &g.query,
                    tree: &l.tree,
                    ranges: &l.ranges,
                }),
                _ => None,
            })
            .collect();
        // Return the spans plus the full-line-background lines (`@markup.raw.block`)
        // so the preview can paint them under the text as its own `line_bg` layer —
        // otherwise the token spans (a `>lua` block's injected lua) overwrite the
        // block background on every non-blank cell.
        extract_spans(&layers, &shadow, first_line, last_line, repaint.as_ref())
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

    fn injected_languages(&self, buffer: BufferId) -> Vec<String> {
        Engine::injected_languages(self, buffer)
    }

    fn text_injected_languages(&self) -> Vec<String> {
        Engine::text_injected_languages(self)
    }

    fn highlight_text(&mut self, lang: &str, text: &str, first: usize, last: usize) -> Vec<Span> {
        Engine::highlight_text(self, lang, text, first, last)
    }

    fn highlight_text_bg(
        &mut self,
        lang: &str,
        text: &str,
        first: usize,
        last: usize,
    ) -> (Vec<Span>, Vec<usize>) {
        Engine::highlight_text_bg(self, lang, text, first, last)
    }

    fn highlight_fragment(
        &mut self,
        lang: &str,
        text: &str,
        first: usize,
        last: usize,
    ) -> Vec<Span> {
        Engine::highlight_fragment(self, lang, text, first, last)
    }

    fn set_fragment_context(&mut self, lang: &str, templates: Vec<String>) {
        Engine::set_fragment_context(self, lang, templates)
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
        // The base the engine would compile with no override: the on-disk file with
        // its `; inherits:` chain folded in — the same source `set_query_overlay`
        // compares a resolved overlay against.
        self.read_disk_query(lang, name)
    }

    fn base_query_raw(&self, lang: &str, name: &str) -> Result<Option<String>, String> {
        Engine::base_query_raw(self, lang, name)
    }

    fn query_inherits(&self, lang: &str, name: &str) -> Vec<String> {
        Engine::query_inherits(self, lang, name)
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

/// Whether `tree` holds an `ERROR` or a `MISSING` node — i.e. the parser had to
/// invent its way through the text. Both count: an `ERROR` means it gave up on a
/// region, a `MISSING` means it inserted a token that isn't there, and a framing
/// that leaves either behind hasn't actually made the snippet whole.
fn tree_has_defect(tree: &Tree) -> bool {
    // `has_error` covers both on the root, so the common (clean) case is O(1).
    tree.root_node().has_error()
}

/// The byte length of each line of `text`, excluding its terminator — the geometry
/// [`unwrap_spans`] clips against.
fn line_lengths(text: &str) -> Vec<usize> {
    text.lines().map(str::len).collect()
}

/// The longest `(…)` [annotation](annotation_prefix) label taken as one — long
/// enough for `(type parameter)`, short enough that a parenthesised line of real
/// code cannot pass for a label.
const MAX_ANNOTATION_LABEL: usize = 24;

/// The **display annotation** a language server puts in front of a hover, as
/// `(label width, bytes to skip)` — `Some((8, 9))` for `"(method) join(…)"`, `None`
/// when the snippet doesn't open with one.
///
/// The shape is an LSP display convention rather than any one server's: `pyright`
/// writes `(class) Foo` / `(type alias) Bar`, `tsserver` `(local var) x: number`.
/// So the rule is deliberately narrow — a parenthesised run of words on the first
/// line, followed by a space and then some code — and it is only ever consulted
/// after a snippet has already failed to parse every way it could. Code that
/// genuinely opens with a parenthesised expression (a lisp form, a cast, a tuple)
/// parses, and so never reaches here.
fn annotation_prefix(text: &str) -> Option<(usize, usize)> {
    let line = text.lines().next()?;
    let close = line.strip_prefix('(')?.find(')')? + 1;
    let label = &line[1..close];
    // Words and spaces only: `type alias` yes, `a + b` or `Foo::bar` no.
    if label.is_empty()
        || label.len() > MAX_ANNOTATION_LABEL
        || !label
            .chars()
            .all(|c| c.is_ascii_alphabetic() || c == ' ' || c == '-')
        || !label.starts_with(|c: char| c.is_ascii_alphabetic())
    {
        return None;
    }
    let rest = line[close + 1..].trim_start_matches(' ');
    // Nothing after the label (or nothing *between* it and the rest) means this is
    // the snippet itself, not an annotation on one.
    if rest.is_empty() || rest.len() == line.len() - close - 1 {
        return None;
    }
    Some((close + 1, line.len() - rest.len()))
}

/// Bring `spans` back from a [framed](FragmentContext) parse into the fragment's own
/// coordinates: shift the line index by the prefix's newline count, shift the *first*
/// line's columns by the prefix's trailing column (a same-line template like
/// `"return %s"`), and drop or clip anything that falls outside the fragment — the
/// framing's own tokens, and any suffix that shares the fragment's last line.
fn unwrap_spans(spans: Vec<Span>, context: &FragmentContext, line_lens: &[usize]) -> Vec<Span> {
    spans
        .into_iter()
        .filter_map(|span| {
            let line = span.line.checked_sub(context.line_offset)?;
            let len = *line_lens.get(line)?;
            let (mut start, mut end) = (span.start_byte, span.end_byte);
            // The opener shares the fragment's first line — and in indent mode it is
            // repeated on all of them — so what it covers belongs to the framing and
            // what survives shifts left by its width.
            if line == 0 || context.indent.is_some() {
                start = start.saturating_sub(context.col_offset);
                end = end.checked_sub(context.col_offset)?;
            }
            end = end.min(len); // a suffix sharing the fragment's last line
            (start < end).then_some(Span {
                line,
                start_byte: start,
                end_byte: end,
                group: span.group,
            })
        })
        .collect()
}

/// The regions of a fragment's parse the grammar could not make sense of, plus the
/// token-level paint to put there — see
/// [`Engine::highlight_fragment`](Engine::highlight_fragment).
struct FragmentPaint {
    /// Byte ranges covered by an `ERROR` node. The host layer's captures touching one
    /// are dropped: inside an `ERROR` the tree is a recovery guess, and a structural
    /// query that matches there names a construct that isn't in the text.
    errors: Vec<Range<usize>>,
    /// `(start, end, group)` recovered from the leaves inside those ranges.
    tokens: Vec<(usize, usize, &'static str)>,
}

impl FragmentPaint {
    /// Whether `[s, e)` touches a region the parse failed on.
    fn overlaps_error(&self, s: usize, e: usize) -> bool {
        self.errors.iter().any(|r| s < r.end && r.start < e)
    }
}

/// Whether the capture `name` classifies a **token** rather than naming a
/// construct — the captures that stay trustworthy inside an `ERROR` region, where
/// the lexer still worked but the parser did not.
///
/// A string, a number, a comment, a keyword, an operator, a bracket: the grammar
/// knows these from the text alone, and a query that captured one under an `ERROR`
/// captured a token that really is there. Everything else (`@type`, `@function`,
/// `@variable.parameter`, `@constructor`, `@property`, …) names the *role* a node
/// plays in a construct, which is precisely what error recovery guessed at.
/// Matched on the major segment, so refinements (`constant.builtin`,
/// `punctuation.delimiter`, `keyword.function`) travel with their family.
fn is_lexical_group(name: &str) -> bool {
    matches!(
        name.split('.').next().unwrap_or(name),
        "comment"
            | "string"
            | "character"
            | "number"
            | "float"
            | "boolean"
            | "constant"
            | "escape"
            | "keyword"
            | "operator"
            | "punctuation"
    )
}

/// The highlight group for a **literal** node kind, or `None` when the kind isn't
/// one. Matched on a substring of the kind *name* so it holds for any grammar
/// without a per-language table — `string_literal` / `interpreted_string_literal`
/// / `raw_string`, `line_comment` / `block_comment`, `integer_literal` / `number`
/// / `float`. A literal is painted whole and never descended into, so a string's
/// own quote delimiters (anonymous tokens) can't leak out as operators.
fn literal_group(kind: &str) -> Option<&'static str> {
    if kind.contains("comment") {
        Some("comment")
    } else if kind.contains("string") || kind.contains("char") {
        Some("string")
    } else if kind.contains("number") || kind.contains("integer") || kind.contains("float") {
        Some("number")
    } else {
        None
    }
}

/// The highlight group for a leaf inside an `ERROR` region, or `None` to leave it
/// plain.
///
/// Anonymous tokens *are* the grammar's own keyword / punctuation / operator set —
/// tree-sitter still lexes them under error recovery — so they classify from their
/// spelling alone, in any language. Named leaves that aren't literals are
/// identifiers, and an identifier inside an `ERROR` could be a variable, a type, a
/// parameter, or a display-only annotation: guessing between those is exactly what
/// fragment mode exists to stop, so they stay plain.
fn leaf_group(kind: &str, named: bool, text: &str) -> Option<&'static str> {
    if kind.contains("keyword") {
        return Some("keyword"); // some grammars name their keyword nodes
    }
    if named || text.is_empty() {
        return None;
    }
    if text.chars().all(|c| c.is_alphabetic() || c == '_') {
        Some("keyword")
    } else if text.chars().all(|c| "()[]{}".contains(c)) {
        Some("punctuation.bracket")
    } else if text.chars().all(|c| ",;:.".contains(c)) {
        Some("punctuation.delimiter")
    } else {
        Some("operator")
    }
}

/// Collect the `ERROR` regions of `tree` and repaint them from their own leaves.
/// `text` is the source the tree was parsed from, so node byte ranges index it
/// directly.
fn fragment_repaint(tree: &Tree, text: &str) -> FragmentPaint {
    let mut paint = FragmentPaint {
        errors: Vec::new(),
        tokens: Vec::new(),
    };
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.is_error() {
            // Outermost `ERROR` only — a nested one is already inside this range, and
            // `repaint_tokens` walks the whole subtree.
            paint.errors.push(node.byte_range());
            repaint_tokens(node, text, &mut paint.tokens);
            continue;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    paint.errors.sort_by_key(|r| r.start);
    paint
}

/// Walk `node`'s subtree, emitting a span for every literal (whole, not descended
/// into) and every other leaf [`leaf_group`] can classify.
fn repaint_tokens(node: Node, text: &str, out: &mut Vec<(usize, usize, &'static str)>) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        let range = n.byte_range();
        if range.is_empty() {
            continue; // a MISSING node the recovery inserted: no text to paint
        }
        if let Some(group) = literal_group(n.kind()) {
            out.push((range.start, range.end, group));
            continue;
        }
        let mut cursor = n.walk();
        let mut children = n.children(&mut cursor).peekable();
        if children.peek().is_some() {
            for child in children {
                stack.push(child);
            }
            continue;
        }
        let Some(slice) = text.get(range.clone()) else {
            continue;
        };
        if let Some(group) = leaf_group(n.kind(), n.is_named(), slice) {
            out.push((range.start, range.end, group));
        }
    }
}

/// resolve the captures into per-line byte spans. Within a layer the most-specific
/// (narrowest) capture wins; across layers a deeper (injected) layer overwrites a
/// shallower one inside its region — so injected highlighting paints over the host.
///
/// Returns the per-line spans plus the absolute line numbers a
/// [`LINE_BACKGROUND_GROUPS`] capture touches (for the `line_bg` layer). The line
/// list is recorded when a background capture is bucketed onto a line — *before* the
/// per-cell overwrite — so a line stays listed even where an injected token covers
/// every cell (e.g. a `}` at column 0 with no surrounding space).
///
/// `repaint` is the fragment-mode low-confidence pass (`None` for a buffer or a
/// whole-file snippet): inside its `ERROR` ranges the host layer's captures are
/// dropped and its recovered tokens painted instead.
fn extract_spans(
    layers: &[Layer],
    shadow: &Rope,
    first_line: usize,
    last_line: usize,
    repaint: Option<&FragmentPaint>,
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
            // Fragment mode: inside an `ERROR` region the host layer's *structure* is
            // a recovery guess, so a capture that names a construct there (a type, a
            // function, a parameter) is naming something that isn't in the text — drop
            // it. Its *lexing* is still sound, so a capture that only classifies a
            // token (a keyword, a string, a number) is kept. Child layers keep
            // everything: an injected region is its own parse, with its own tree.
            if rank == 0
                && !is_lexical_group(name)
                && repaint.is_some_and(|f| f.overlaps_error(s, e))
            {
                continue;
            }
            if layer.ranges.is_empty() {
                // `rank + 1`: fill precedence 0 is reserved for the fragment-mode
                // token repaint below, so a real capture always wins over a recovered
                // one. Relative order between layers is unchanged.
                raw.push((s, e, name, rank + 1)); // host: no clipping
            } else {
                // Child: clip to the injected ranges so a node spanning the gap
                // between a combined layer's ranges paints only within them.
                for r in layer.ranges {
                    let (cs, ce) = (s.max(r.start), e.min(r.end));
                    if cs < ce {
                        raw.push((cs, ce, name, rank + 1));
                    }
                }
            }
        }
    }

    // Fragment mode: the token-level paint for the regions cleared above, at fill
    // precedence 0 — below every real capture, so the lexical captures that survived
    // the drop (and any genuinely injected layer) paint over it, and the repaint only
    // shows through where the parse left nothing.
    if let Some(f) = repaint {
        for &(s, e, group) in &f.tokens {
            if s < hi && lo < e {
                raw.push((s, e, group, 0));
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
    collect_injection_regions_in(query, tree, rope, self_lang, parent_lang, None)
        .into_iter()
        .map(|r| (r.language, r.ranges))
        .collect()
}

/// One top-level injection region-set, as derived from the host's injection query.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InjectionRegion {
    /// The injected language.
    language: String,
    /// The buffer byte ranges the child grammar parses.
    ranges: Vec<Range<usize>>,
    /// The byte span of **everything the match matched on** — every captured node,
    /// not only the injected content.
    ///
    /// This is what an edit must be checked against to decide whether the region is
    /// still valid, and it is wider than `ranges` for the query markdown actually
    /// ships: `(info_string (language) @injection.language)` reads the fence's
    /// language from *outside* the content it injects, so rewriting ```` ```rust ````
    /// as ```` ```ruby ```` changes the match while touching none of its content.
    extent: Range<usize>,
}

/// Whether `query` has any pattern carrying `injection.combined` — the property that
/// makes a region-set accumulate across matches from anywhere in the document, and so
/// the one thing that cannot be re-derived from a byte range. Cheap: a handful of
/// patterns, each with a handful of properties, against a walk of the whole tree.
fn query_has_combined(query: &Query) -> bool {
    (0..query.pattern_count()).any(|i| {
        query
            .property_settings(i)
            .iter()
            .any(|p| &*p.key == "injection.combined")
    })
}

/// Map a byte range through a sequence of edits, in the order they were applied. A
/// position at or before an edit's start is unmoved; one after the replaced span
/// slides by the edit's length delta; one *inside* it collapses to the span's new
/// end. That last case only arises for a region the caller is about to discard
/// anyway — a region containing an edit's interior necessarily intersects the dirty
/// set — so it is defined for totality rather than for its result.
fn shift_range(r: &Range<usize>, edits: &[InputEdit]) -> Range<usize> {
    let shift = |mut b: usize| {
        for e in edits {
            b = if b <= e.start_byte {
                b
            } else if b <= e.old_end_byte {
                e.new_end_byte
            } else {
                (b + e.new_end_byte).saturating_sub(e.old_end_byte)
            };
        }
        b
    };
    let start = shift(r.start);
    start..shift(r.end).max(start)
}

/// [`collect_injection_regions`] restricted to a byte range: only matches
/// intersecting `within` are returned, and the cursor prunes subtrees outside it
/// rather than walking them. Captured nodes are still reported in full, so a region
/// straddling the boundary comes back whole.
fn collect_injection_regions_in(
    query: &Query,
    tree: &Tree,
    rope: &Rope,
    self_lang: Option<&str>,
    parent_lang: Option<&str>,
    within: Option<Range<usize>>,
) -> Vec<InjectionRegion> {
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    if let Some(w) = within {
        cursor.set_byte_range(w);
    }
    let mut out: Vec<InjectionRegion> = Vec::new();
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
        // Every captured node, content or not — the text this match's result depends
        // on, and therefore the span an edit invalidates it through.
        let mut extent: Option<Range<usize>> = None;
        for cap in m.captures {
            let (cs, ce) = (cap.node.start_byte(), cap.node.end_byte());
            extent = Some(match extent {
                Some(e) => e.start.min(cs)..e.end.max(ce),
                None => cs..ce,
            });
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
        let extent = extent.unwrap_or_else(|| {
            let lo = ranges.iter().map(|r| r.start).min().unwrap_or(0);
            let hi = ranges.iter().map(|r| r.end).max().unwrap_or(0);
            lo..hi
        });
        if combined {
            match combined_set.get(&(language.clone(), m.pattern_index)) {
                Some(&idx) => {
                    out[idx].ranges.extend(ranges);
                    out[idx].extent.start = out[idx].extent.start.min(extent.start);
                    out[idx].extent.end = out[idx].extent.end.max(extent.end);
                }
                None => {
                    combined_set.insert((language.clone(), m.pattern_index), out.len());
                    out.push(InjectionRegion {
                        language,
                        ranges,
                        extent,
                    });
                }
            }
        } else {
            out.push(InjectionRegion {
                language,
                ranges,
                extent,
            });
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
/// does — strip whitespace, lowercase, `-`→`_`, reject anything that isn't a legal
/// grammar identifier — then resolve the *alias* (`resolve_lang`'s `get_lang` half):
/// a fence info string names a language the way its writers spell it (```` ```sh ````,
/// ```` ```jsonc ````, ```` ```cs ````), which is not always the grammar's own noun.
/// Returns `None` for an empty or invalid name (skipped).
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
    Some(nxvim_core::resolve_language(&norm).to_string())
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
    nxvim_core::ENGINE_QUERY_NAMES.contains(&name)
}
