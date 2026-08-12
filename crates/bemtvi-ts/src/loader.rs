//! Dynamic grammar loading — the installable-grammar half of the engine.
//!
//! Grammars are **not** linked into bemtvi. They are loaded at runtime from a
//! data directory laid out exactly like neovim's `runtimepath`, so an existing
//! nvim-treesitter `parser/` + `queries/` tree is drop-in usable:
//!
//! ```text
//! <data>/parser/<lang>.{so,dylib,dll}     # exports tree_sitter_<lang>()
//! <data>/queries/<lang>/highlights.scm
//! <data>/queries/<lang>/indents.scm       # optional — treesitter indentation
//! <data>/queries/<lang>/injections.scm    # optional — embedded-language layers
//! <data>/queries/<lang>/folds.scm         # optional — treesitter foldexpr
//! <data>/queries/<lang>/textobjects.scm   # optional — vif/daf/… text objects
//! ```

use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use tree_sitter::{Language, Parser, Query};

/// The query-text overrides the engine holds, keyed by `(lang, query_name)` —
/// the store [`Grammar::load`] consults in place of the on-disk query, and the
/// landing point of the query-resolution bridge (Lua resolves the merged string,
/// the engine compiles + caches + executes it). Only the engine-executed names
/// (`highlights` / `indents` / `injections` / `folds` / `textobjects`) are ever
/// present. See [`Engine::set_query`](crate::engine::Engine::set_query).
pub type QueryOverrides = HashMap<(String, String), String>;

/// Why [`Grammar::load`] didn't return a grammar, so the engine (and the editor
/// above it) can stay silent for an uninstalled language but surface a real load
/// failure once.
pub enum LoadError {
    /// No parser library is installed for this language under the data dir. The
    /// expected, silent case — the buffer is simply not highlighted.
    NotInstalled,
    /// A parser **is** installed but couldn't be loaded or used: an invalid
    /// language name, a dlopen failure, a missing symbol, an ABI mismatch, or a
    /// missing/unparseable highlights query. Worth telling the user about.
    Failed(anyhow::Error),
}

/// A query file whose **compile** waits until something asks for it.
///
/// Compiling a query is the whole cost of loading a grammar: the `dlopen` is
/// ~0.1 ms, while one `Query::new` is ~60 ms for a rust-sized grammar — and it is
/// the grammar, not the query, that sets that price (halving a `highlights.scm`
/// removes ~15% of its compile time, because `ts_query_new` analyzes each pattern
/// against the parse table). The cost is therefore *(query files) x (a per-grammar
/// constant)*, and a language that ships all five costs five times what painting it
/// needs — painting needs `highlights`, plus `injections` to find its child layers.
/// The other three answer a keypress that may never come: `indents` an `=`, `folds`
/// a `foldmethod=expr`, `textobjects` a `vif`.
///
/// So the source is read at load (cheap, and it is what makes "does this language
/// have folds" answerable without a compile) and compiled on the first ask. The
/// result — including a failure, which stays [`Err`] rather than being retried on
/// every later keypress — is remembered.
pub struct LazyQuery {
    /// `"<lang> <name>"`, the label a compile error carries — the grammar knows its
    /// `Language` but not the name it was loaded under.
    label: String,
    /// The `<name>.scm` text, or `None` when the language ships no such file.
    source: Option<String>,
    /// The compile, run at most once. Empty until something asks.
    cell: OnceCell<Result<Query, String>>,
}

impl LazyQuery {
    /// A query source (`None` when the language ships no such file) to compile on
    /// the first ask.
    fn deferred(label: String, source: Option<String>) -> Self {
        LazyQuery {
            label,
            source,
            cell: OnceCell::new(),
        }
    }

    /// An **already-compiled** query, for the one caller that must compile eagerly:
    /// `set_query` installs a resolved override on behalf of a user action, so a
    /// broken one has to fail *that* action rather than a later keypress.
    pub fn ready(label: String, source: String, query: Query) -> Self {
        let cell = OnceCell::new();
        let _ = cell.set(Ok(query));
        LazyQuery {
            label,
            source: Some(source),
            cell,
        }
    }

    /// No such query — the language ships no file and none was set.
    pub fn absent(label: String) -> Self {
        LazyQuery {
            label,
            source: None,
            cell: OnceCell::new(),
        }
    }

    /// Whether the language *has* this query at all. Answerable without compiling,
    /// which is what the `…_available` probes need.
    pub fn is_present(&self) -> bool {
        self.source.is_some()
    }

    /// The compiled query, compiling it now if this is the first ask: `Ok(None)`
    /// when the language ships no such file, `Err` with the labelled compile error
    /// (the same one every later ask gets — the failure is not retried).
    pub fn get(&self, language: &Language) -> Result<Option<&Query>, &str> {
        if self.source.is_none() {
            return Ok(None);
        }
        let compiled = self.cell.get_or_init(|| {
            let src = self.source.as_deref().unwrap_or_default();
            compile_query(language, src).map_err(|e| format!("compiling {}: {e}", self.label))
        });
        match compiled {
            Ok(query) => Ok(Some(query)),
            Err(msg) => Err(msg.as_str()),
        }
    }
}

/// A loaded grammar: the dynamic library (kept alive because `language` borrows
/// code inside it), the `Language`, the compiled highlights `Query`, the compiled
/// injections query (the injection-query bridge — which patterns mark a node's text
/// as another language), and the three [`LazyQuery`] slots nothing needs to paint:
/// indents (treesitter indentation), folds, and textobjects.
///
/// `highlights` and `injections` are compiled at load because every painted buffer
/// wants both immediately; see [`LazyQuery`] for why the rest wait.
pub struct Grammar {
    // Field order matters: every query field drops before `_lib`, so the loaded
    // code outlives anything pointing into it.
    pub language: Language,
    pub query: Query,
    pub injections: Option<Query>,
    /// `indents.scm` — treesitter indentation, compiled on the first `=` / `o`.
    pub indents: LazyQuery,
    /// `folds.scm` (`@fold` captures → foldable node ranges), compiled the first
    /// time `foldmethod=expr` asks. The core builds per-line levels from the ranges.
    pub folds: LazyQuery,
    /// `textobjects.scm` (`@function.inner/outer`, `@parameter.*`, `@class.*`,
    /// `@comment.*`, … captures → syntactic text-object ranges), compiled on the
    /// first `vif` / `daf` / `dia`.
    pub textobjects: LazyQuery,
    _lib: libloading::Library,
}

/// Just the parser half of a grammar: the `Language` and the dynamic library it
/// lives in, with **no** query files required — a bare parser load for a caller
/// that wants to create a parser or compile its own query without the editor's
/// `highlights.scm`/`indents.scm`, which only the [`Grammar`] highlighter needs.
/// The library is held so the language's code outlives every tree and node
/// derived from it.
pub struct LoadedLanguage {
    // Field order matters: `language` drops before `_lib`.
    pub language: Language,
    _lib: libloading::Library,
}

impl LoadedLanguage {
    /// Load just the `Language` for `lang` from `data_dir` — the parser library,
    /// dlopen'd and ABI-probed, without touching any query file. Same
    /// missing-vs-broken distinction as [`Grammar::load`].
    pub fn load(data_dir: &Path, lang: &str) -> Result<LoadedLanguage, LoadError> {
        let (language, lib) = open_language(data_dir, lang)?;
        Ok(LoadedLanguage {
            language,
            _lib: lib,
        })
    }
}

impl Grammar {
    /// Load the grammar for `lang` from `data_dir`. Distinguishes a *missing*
    /// parser ([`LoadError::NotInstalled`], silent) from a parser that is present
    /// but broken ([`LoadError::Failed`], worth echoing).
    ///
    /// `overrides` is the query-override store: a `(lang, name)` entry supplies the
    /// query text directly (from `btv.treesitter.set_query` — a replace, no merge) in
    /// place of the on-disk `highlights.scm` / `indents.scm`. With no entry the disk
    /// file is read exactly as before, so the common no-customization case is
    /// byte-identical.
    pub fn load(
        data_dir: &Path,
        lang: &str,
        overrides: &QueryOverrides,
    ) -> Result<Grammar, LoadError> {
        let (language, lib) = open_language(data_dir, lang)?;

        let hl_src = match overrides.get(&(lang.to_string(), "highlights".to_string())) {
            Some(text) => text.clone(),
            None => resolve_query(data_dir, lang, "highlights")
                .with_context(|| {
                    format!(
                        "reading {}",
                        query_path(data_dir, lang, "highlights.scm").display()
                    )
                })
                .map_err(LoadError::Failed)?
                .ok_or_else(|| {
                    LoadError::Failed(anyhow!(
                        "reading {}: no such file",
                        query_path(data_dir, lang, "highlights.scm").display()
                    ))
                })?,
        };
        let query = compile_query(&language, &hl_src)
            .with_context(|| format!("compiling {lang} highlights"))
            .map_err(LoadError::Failed)?;

        // `injections.scm` is optional but wanted immediately — a buffer's child
        // layers are found on its first highlight — so it is compiled here, and a
        // present-but-broken one is a real error, surfaced like a broken highlights
        // query. The other three are only ever wanted by a keypress, so their source
        // is read (that is what `is_present` answers) and the compile waits; a broken
        // one fails the keypress that asks, not the load.
        let injections = load_optional_query(data_dir, lang, &language, "injections", overrides)?;
        let lazy = |name: &str| -> Result<LazyQuery, LoadError> {
            let source = optional_query_source(data_dir, lang, name, overrides)?;
            Ok(LazyQuery::deferred(format!("{lang} {name}"), source))
        };

        Ok(Grammar {
            language,
            query,
            injections,
            indents: lazy("indents")?,
            folds: lazy("folds")?,
            textobjects: lazy("textobjects")?,
            _lib: lib,
        })
    }
}

/// Load and compile an **optional** query (`indents.scm`, `injections.scm`): the
/// `overrides` entry wins, else the on-disk `<name>.scm`, else `None` when no file
/// exists. A present-but-broken source — override *or* disk — is a real
/// [`LoadError::Failed`], surfaced like a broken highlights query (no silent
/// stubs). The required `highlights` query is loaded separately, since its absence
/// is itself a failure.
fn load_optional_query(
    data_dir: &Path,
    lang: &str,
    language: &Language,
    name: &str,
    overrides: &QueryOverrides,
) -> Result<Option<Query>, LoadError> {
    match optional_query_source(data_dir, lang, name, overrides)? {
        Some(s) => Ok(Some(
            compile_query(language, &s)
                .with_context(|| format!("compiling {lang} {name}"))
                .map_err(LoadError::Failed)?,
        )),
        None => Ok(None),
    }
}

/// The source text of an **optional** query: the `overrides` entry wins, else the
/// on-disk `<name>.scm`, else `None`. Reading is cheap — it is the compile that
/// costs (see [`LazyQuery`]) — so both the eager and the deferred slots resolve
/// their source here, at load.
fn optional_query_source(
    data_dir: &Path,
    lang: &str,
    name: &str,
    overrides: &QueryOverrides,
) -> Result<Option<String>, LoadError> {
    match overrides.get(&(lang.to_string(), name.to_string())) {
        Some(text) => Ok(Some(text.clone())),
        None => resolve_query(data_dir, lang, name).map_err(|e| {
            LoadError::Failed(anyhow::Error::new(e).context(format!(
                "reading {}",
                query_path(data_dir, lang, &format!("{name}.scm")).display()
            )))
        }),
    }
}

/// dlopen the parser library for `lang` under `data_dir`, resolve its
/// `tree_sitter_<lang>` export, and ABI-probe it — returning the `Language` and
/// the library that must outlive it. The shared core of [`Grammar::load`] and
/// [`LoadedLanguage::load`]; the only place that executes installed native code.
fn open_language(
    data_dir: &Path,
    lang: &str,
) -> Result<(Language, libloading::Library), LoadError> {
    // Security boundary: `lang` flows into the parser `.so` path and the query
    // directory, and we then `dlopen` that path — i.e. execute native code. A
    // name containing `.`, `/`, `\`, or path components could escape `data_dir`
    // (traversal / absolute path) and load an arbitrary shared object. Reject
    // anything that isn't a plain grammar identifier before touching the
    // filesystem. Callers only ever *should* pass names from the fixed filetype
    // table, but must not assume that.
    if !is_valid_language(lang) {
        return Err(LoadError::Failed(anyhow!("invalid language name '{lang}'")));
    }

    // No parser file at all is the common, expected case — not a failure.
    let Some(lib_path) = parser_path(data_dir, lang) else {
        return Err(LoadError::NotInstalled);
    };

    // SAFETY: loading arbitrary native code is inherently unsafe. A poison
    // grammar can segfault the process (neovim's posture); the ABI probe below
    // is the load-time mitigation.
    let lib = unsafe { libloading::Library::new(&lib_path) }
        .with_context(|| format!("dlopen {}", lib_path.display()))
        .map_err(LoadError::Failed)?;

    let symbol = format!("tree_sitter_{}", lang.replace('-', "_"));
    let language = unsafe {
        let func: libloading::Symbol<unsafe extern "C" fn() -> *const ()> = lib
            .get(symbol.as_bytes())
            .with_context(|| format!("symbol {symbol} in {}", lib_path.display()))
            .map_err(LoadError::Failed)?;
        Language::from_raw(func() as *const _)
    };

    // Validate the grammar ABI against our tree-sitter before trusting it.
    let mut probe = Parser::new();
    probe
        .set_language(&language)
        .with_context(|| format!("grammar '{lang}' ABI incompatible"))
        .map_err(LoadError::Failed)?;

    Ok((language, lib))
}

/// A grammar identifier: non-empty and only ASCII letters, digits, `_` or `-`
/// (e.g. `rust`, `c`, `cpp`, `c_sharp`, `tsx`). Excluding `.`, `/`, `\` and the
/// empty string is what makes path traversal and absolute-path escapes
/// impossible when the name is joined into `data_dir`.
pub(crate) fn is_valid_language(lang: &str) -> bool {
    !lang.is_empty()
        && lang
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Whether a parser for `lang` is installed under `data_dir` (a `parser/<lang>.*`
/// exists). Used by the engine to pick the effective root over the data-dir
/// search path before loading. A bad name (which can't form a valid path) is
/// simply "not here".
pub fn has_parser(data_dir: &Path, lang: &str) -> bool {
    is_valid_language(lang) && parser_path(data_dir, lang).is_some()
}

/// First existing `parser/<lang>.<ext>` over the platform's candidate
/// extensions. `.so` is tried first on every OS because nvim-treesitter names
/// its parsers `<lang>.so` even on macOS.
fn parser_path(data_dir: &Path, lang: &str) -> Option<PathBuf> {
    let dir = data_dir.join("parser");
    ["so", native_lib_ext()]
        .into_iter()
        .map(|ext| dir.join(format!("{lang}.{ext}")))
        .find(|p| p.exists())
}

/// The platform's native shared-library extension for parser objects (`dll` on
/// Windows, `dylib` on macOS, `so` elsewhere). `.so` is also tried first on every
/// OS because nvim-treesitter names its parsers `<lang>.so` even on macOS, so
/// callers pair this with `"so"`.
pub(crate) fn native_lib_ext() -> &'static str {
    if cfg!(target_os = "windows") {
        "dll"
    } else if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    }
}

pub(crate) fn query_path(data_dir: &Path, lang: &str, file: &str) -> PathBuf {
    data_dir.join("queries").join(lang).join(file)
}

/// Read exactly one `<name>.scm` for `lang` under `root` — no `; inherits:`
/// resolution. `Ok(None)` when the file is absent (an optional query a language
/// simply doesn't ship).
pub(crate) fn read_one_query(
    root: &Path,
    lang: &str,
    name: &str,
) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(query_path(root, lang, &format!("{name}.scm"))) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The languages `(lang, name)` inherits, transitively, in merge order — deepest
/// ancestor first, `lang` itself excluded. Cycle-guarded, so a chain that loops
/// back terminates instead of recursing forever.
///
/// nvim-treesitter shares one query set between related grammars this way:
/// `javascript/folds.scm` is *only* `; inherits: ecma,jsx`, with every pattern in
/// `ecma/folds.scm`. The whole chain resolves within `root`, the same root the
/// parser loaded from, so a grammar borrowed from a read-only fallback root reads
/// *its* ancestors rather than a half-matched pair.
pub(crate) fn query_inherits(root: &Path, lang: &str, name: &str) -> Vec<String> {
    fn walk(
        root: &Path,
        lang: &str,
        name: &str,
        chain: &mut Vec<String>,
        seen: &mut HashMap<String, ()>,
    ) {
        let Ok(Some(text)) = read_one_query(root, lang, name) else {
            return;
        };
        for parent in bemtvi_core::parse_query_inherits(&text) {
            if seen.insert(parent.clone(), ()).is_some() {
                continue; // already pulled in via another edge, or a cycle
            }
            walk(root, &parent, name, chain, seen); // post-order: ancestors first
            chain.push(parent);
        }
    }
    let mut chain = Vec::new();
    let mut seen = HashMap::from([(lang.to_string(), ())]);
    walk(root, lang, name, &mut chain, &mut seen);
    chain
}

/// The query source for `(lang, name)` as the engine compiles it: the file's own
/// text with every `; inherits:` ancestor folded in. `Ok(None)` when the language
/// has no such file.
///
/// Merge order is ancestors first, this language last, so its own patterns are the
/// later write and win a tie against what it inherits. The file's leading comment
/// block is re-emitted *first*, ahead of the ancestors, so the `; inherits:`
/// modeline stays where a reader finds it — the server parses the chain off this
/// very text to pull runtimepath overlays for the inherited languages, which the
/// engine cannot see. Comments are inert in a query, so the duplicate costs nothing.
///
/// This is the **one** query reader: `Grammar::load` compiles what it returns and
/// [`Engine::base_query`](crate::Engine::base_query) reports it, so the compiled
/// query and the text the server diffs against can never drift.
pub(crate) fn resolve_query(
    root: &Path,
    lang: &str,
    name: &str,
) -> std::io::Result<Option<String>> {
    let Some(own) = read_one_query(root, lang, name)? else {
        return Ok(None);
    };
    let inherited = query_inherits(root, lang, name);
    if inherited.is_empty() {
        return Ok(Some(own));
    }
    let (head, body) = split_leading_comments(&own);
    let mut merged = String::with_capacity(own.len() * 2);
    merged.push_str(head);
    for ancestor in &inherited {
        if let Some(text) = read_one_query(root, ancestor, name)? {
            // Drop the ancestor's own modeline block: its chain is already flattened
            // into `inherited`, and a stray modeline mid-file would only mislead a
            // later reader.
            merged.push_str(split_leading_comments(&text).1);
            merged.push('\n');
        }
    }
    merged.push_str(body);
    Ok(Some(merged))
}

/// Split a query file at the end of its **leading comment block**: `(head, body)`,
/// where `head` is the run of `;`-comment and blank lines the file opens with (the
/// `; inherits:` modeline lives there) and `body` is everything from the first
/// pattern on.
fn split_leading_comments(text: &str) -> (&str, &str) {
    let mut cut = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(';') {
            cut += line.len();
        } else {
            break;
        }
    }
    text.split_at(cut)
}

/// Compile a query, first making its source palatable to our tree-sitter binding.
/// The one entry point for turning `.scm` text into a [`Query`] — every loader and
/// the engine's recompile path route through here so the neovim-compatibility
/// rewrite below is applied uniformly.
pub(crate) fn compile_query(
    language: &Language,
    src: &str,
) -> Result<Query, tree_sitter::QueryError> {
    Query::new(language, &sanitize_set_directives(src))
}

/// Rewrite neovim `#set!` directives whose *value* is a capture so our tree-sitter
/// binding accepts them.
///
/// neovim's query runtime lets a `#set!` directive's value be a capture — e.g.
/// vimdoc's `(#set! @string.special.url url @string.special.url)`, which tags the
/// matched URL's own text as clickable-link metadata. The tree-sitter Rust crate's
/// stricter `#set!` parser allows at most *one* capture per directive (the target)
/// and rejects any further one with "Unexpected second capture name", failing the
/// whole query. bemtvi consumes the upstream nvim-treesitter queries verbatim, so it
/// must tolerate this form rather than refuse to highlight the language.
///
/// We quote every capture after the first within each `#set!` directive, turning
/// `(#set! @cap key @cap)` into `(#set! @cap key "@cap")`. The result is a valid
/// one-capture directive carrying the same key; the (string) value is harmless,
/// since bemtvi's `#set!` consumers only read the `indent.*` / `injection.*` keys and
/// never the URL-metadata value. Comments and string literals are skipped so a `@`
/// or `#set!` inside them is left untouched. The common no-`#set!` source returns
/// borrowed and unchanged.
fn sanitize_set_directives(src: &str) -> Cow<'_, str> {
    if !src.contains("#set!") {
        return Cow::Borrowed(src);
    }

    let bytes = src.as_bytes();
    // One frame per open `(`: whether it is a `#set!` directive, and how many
    // captures have appeared directly inside it so far.
    struct Frame {
        is_set: bool,
        captures: u32,
    }
    let mut stack: Vec<Frame> = Vec::new();
    // Byte offsets where a `"` must be inserted to quote a surplus capture.
    let mut inserts: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b';' => {
                // Line comment: skip to end of line.
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' => {
                // String literal: skip to the closing quote, honouring `\` escapes.
                i += 1;
                while i < bytes.len() {
                    let c = bytes[i];
                    i += 1;
                    if c == b'\\' {
                        i += 1;
                    } else if c == b'"' {
                        break;
                    }
                }
            }
            b'(' => {
                stack.push(Frame {
                    is_set: false,
                    captures: 0,
                });
                i += 1;
            }
            b')' => {
                stack.pop();
                i += 1;
            }
            b'#' => {
                // A predicate/directive name; tag the enclosing frame if it's `#set!`.
                let start = i;
                i += 1;
                while i < bytes.len() && is_predicate_name_byte(bytes[i]) {
                    i += 1;
                }
                if &bytes[start..i] == b"#set!" {
                    if let Some(top) = stack.last_mut() {
                        top.is_set = true;
                    }
                }
            }
            b'@' => {
                // A capture. Inside a `#set!` directive, the first is the target and
                // is kept; quote any after it so only one capture survives.
                let start = i;
                i += 1;
                while i < bytes.len() && is_capture_name_byte(bytes[i]) {
                    i += 1;
                }
                if let Some(top) = stack.last_mut() {
                    if top.is_set {
                        top.captures += 1;
                        if top.captures > 1 {
                            inserts.push(start);
                            inserts.push(i);
                        }
                    }
                }
            }
            _ => i += 1,
        }
    }

    if inserts.is_empty() {
        return Cow::Borrowed(src);
    }

    // Splice a `"` at each recorded offset (already in ascending order).
    let mut out = String::with_capacity(src.len() + inserts.len());
    let mut prev = 0;
    for at in inserts {
        out.push_str(&src[prev..at]);
        out.push('"');
        prev = at;
    }
    out.push_str(&src[prev..]);
    Cow::Owned(out)
}

/// Bytes that may appear in a predicate/directive name after the leading `#`
/// (`set!`, `any-of?`, `eq?`, …): letters, digits, `-`, `_`, `?`, `!`.
fn is_predicate_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'?' | b'!')
}

/// Bytes that may appear in a capture name after the leading `@`
/// (`string.special.url`, `_variable`, …): letters, digits, `.`, `_`, `-`.
fn is_capture_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
}
