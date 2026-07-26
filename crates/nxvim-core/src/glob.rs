//! The canonical glob engine: a shell/gitignore-style pattern compiled into a Rust
//! regex, cached so repeated matching is a regex run and never a re-parse.
//!
//! Pure and synchronous like the rest of the core — string math only, no I/O. This
//! module *matches* paths; it never walks a directory (that is the async fs seam's
//! job). It backs `nx.glob` in Lua and is the one definition of a glob for every
//! in-tree caller.
//!
//! # globset is the translator, not the matcher
//!
//! [`globset`] parses the pattern, and [`globset::Glob::regex`] hands back regex
//! source; we compile and match that ourselves. We deliberately do NOT use
//! globset's own `GlobMatcher`, because its candidate normalization is
//! `#[cfg(windows)]`-gated on the **host** (`globset::pathutil::normalize_path`):
//! separator handling would then be decided at build time. nxvim needs it decided
//! per call — the daemon/remote session hands a Unix-hosted client Windows paths, and
//! the wasm build has no host filesystem at all. Owning the regex makes
//! [`PathStyle`] an explicit option and makes matching byte-identical on every host.
//!
//! Note globset emits a **byte**-oriented regex (`syntax.utf8(false)`), so it must be
//! compiled with [`regex::bytes`] — which is also the right way to match a path,
//! whose bytes need not be UTF-8.
//!
//! # The candidate is bytes; the pattern is UTF-8
//!
//! Every matching entry point takes its candidate as `impl AsRef<[u8]>`, so a path
//! that is *not* valid UTF-8 — a latin-1 filename on disk, a name that arrived over
//! the encoding seam — matches by its actual bytes rather than being rejected or
//! lossily rewritten. That is the whole point of compiling to [`regex::bytes`], and it
//! reaches all the way out to `nx.glob` (Lua strings are byte strings).
//!
//! The **pattern** is `&str`, because globset's parser is: glob syntax is ASCII, and a
//! pattern whose bytes are not UTF-8 is a mistake worth failing loud on rather than
//! silently matching nothing.
//!
//! # Syntax
//!
//! ```text
//! *        any run of characters, NOT crossing a separator (see `literal_separator`)
//! **       any run of characters, crossing separators ( `**/x`, `a/**`, `a/**/b` )
//! ?        exactly one character — also separator-stopped, per `literal_separator`
//! [abc]    one of the listed characters      [a-z] a range
//! [!abc]   none of them ( `[^abc]` is accepted as the same thing )
//! {a,b}    either alternative                {a,b/**} nests
//! \x       in Unix style, a literal `x` — in Windows style `\` is a separator
//! ```

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Which separator convention a pattern and its candidates use. Explicit rather than
/// inherited from `#[cfg(windows)]`: a session can be handed paths from the *other*
/// convention over the wire, so the build target is the wrong thing to key on.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum PathStyle {
    /// `/` separates; `\` is the pattern escape character and an ordinary byte in a
    /// candidate.
    #[default]
    Unix,
    /// `/` and `\` both separate. `\` in a pattern is therefore a separator and NOT
    /// an escape, so `src\*.rs` is a path. Drive letters (`C:/foo`) and UNC prefixes
    /// (`//server/share`) are ordinary components.
    Windows,
}

impl PathStyle {
    /// The style of the machine this build runs on — the default when a caller
    /// expresses no preference. Only ever a *default*: every entry point takes the
    /// style explicitly so a caller can override it per pattern.
    pub const HOST: PathStyle = if cfg!(windows) {
        PathStyle::Windows
    } else {
        PathStyle::Unix
    };

    /// Parse the Lua-facing spelling. `None` for an unrecognized name, so the caller
    /// can fail loud with its own message rather than silently picking a style.
    pub fn from_name(name: &str) -> Option<PathStyle> {
        match name {
            "unix" => Some(PathStyle::Unix),
            "windows" | "win" => Some(PathStyle::Windows),
            _ => None,
        }
    }

    /// Rewrite `\` to `/` in a *pattern* under [`PathStyle::Windows`], so one separator
    /// reaches the glob parser. Borrows when there is nothing to rewrite, which is the
    /// common case (`/`-spelled patterns, and every Unix-style call).
    fn normalize<'a>(&self, s: &'a str) -> std::borrow::Cow<'a, str> {
        if *self == PathStyle::Windows && s.contains('\\') {
            std::borrow::Cow::Owned(s.replace('\\', "/"))
        } else {
            std::borrow::Cow::Borrowed(s)
        }
    }

    /// [`PathStyle::normalize`] for a *candidate*, which is bytes rather than `&str`: a
    /// path need not be valid UTF-8, and matching its real bytes is the reason the
    /// compiled regex is a [`regex::bytes`] one. `\` is a single byte in every encoding
    /// this sees, so the rewrite is byte-for-byte and cannot split a character.
    fn normalize_candidate<'a>(&self, s: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
        if *self == PathStyle::Windows && s.contains(&b'\\') {
            std::borrow::Cow::Owned(
                s.iter()
                    .map(|&b| if b == b'\\' { b'/' } else { b })
                    .collect(),
            )
        } else {
            std::borrow::Cow::Borrowed(s)
        }
    }
}

/// Everything that changes the regex a pattern compiles to — and so, exactly the
/// cache key alongside the pattern itself.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct GlobOpts {
    /// Separator convention for both the pattern and its candidates.
    pub style: PathStyle,
    /// Match without regard to ASCII case. The usual choice for Windows paths, but
    /// never implied by [`PathStyle::Windows`] — a caller matching case-sensitively
    /// against Windows-spelled paths is asking for something legitimate.
    pub ignorecase: bool,
    /// Whether `*` and `?` stop at a separator. **Defaults to `true`** — shell /
    /// gitignore / LSP-spec semantics, where `*.lua` does not match `a/b/c.lua` and
    /// `**/*.lua` is how you say that. (globset's own default is the opposite; vim's
    /// autocmd patterns want the opposite too, and pass `false`.)
    pub literal_separator: bool,
    /// Match a separator-less pattern against the candidate's **last component**
    /// instead of the whole path, so `*.lua` matches `a/b/c.lua` by its tail. vim's
    /// file-pattern rule and gitignore's; off by default.
    pub basename: bool,
    /// Whether `{a,}` may contain an empty alternative. Off by default, matching
    /// globset — an empty alternate is nearly always a typo.
    pub empty_alternates: bool,
}

impl Default for GlobOpts {
    fn default() -> Self {
        GlobOpts {
            style: PathStyle::HOST,
            ignorecase: false,
            literal_separator: true,
            basename: false,
            empty_alternates: false,
        }
    }
}

/// A compiled glob: the pattern as written, the regex it translated to, and that
/// regex compiled. Cheap to clone via the [`Rc`] the cache hands out.
pub struct Glob {
    pattern: String,
    regex_src: String,
    re: regex::bytes::Regex,
    opts: GlobOpts,
    /// Whether this pattern matches the candidate's last component rather than the
    /// whole path — `opts.basename` *and* a separator-less pattern. Decided once at
    /// compile time so matching stays a single regex run.
    match_basename: bool,
}

impl Glob {
    /// Does `path` match? The candidate is normalized per [`PathStyle`] and, for a
    /// basename pattern, reduced to its last component first.
    ///
    /// Takes bytes (`&str`, `&[u8]`, `String`, …) so a path that is not valid UTF-8
    /// matches by its real bytes instead of being rejected — see the module docs.
    pub fn is_match(&self, path: impl AsRef<[u8]>) -> bool {
        let normalized = self.opts.style.normalize_candidate(path.as_ref());
        let candidate = if self.match_basename {
            last_component(&normalized)
        } else {
            normalized.as_ref()
        };
        self.re.is_match(candidate)
    }

    /// The glob as written (pre-normalization) — what a caller passed in.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// The regex source this glob translated to. Exposed for introspection (and
    /// `nx.glob.to_regex`): a glob that mismatches is far easier to debug against the
    /// regex it actually became. It is a `regex::bytes` pattern.
    ///
    /// It is the regex [`Glob::is_match`] runs, **not** a standalone equivalent of the
    /// glob: under [`GlobOpts::basename`] the candidate is reduced to its last
    /// component *before* this regex sees it, and that reduction is not expressible in
    /// the regex (with `literal_separator = false` a `*` may cross `/`, so no
    /// `(?:.*/)?` prefix is faithful). Handing this to another engine therefore means
    /// reproducing the reduction too — which is why [`to_regex`], whose whole purpose
    /// is to hand the translation elsewhere, refuses a `basename` glob outright.
    pub fn regex(&self) -> &str {
        &self.regex_src
    }
}

/// Several globs sharing one [`regex::bytes::RegexSet`], so testing a path against N
/// patterns is one pass rather than N. The win grows with the pattern count — an
/// ignore list or a set of LSP watcher globs is the shape this is for.
pub struct GlobSet {
    set: regex::bytes::RegexSet,
    /// Per-pattern basename flag, parallel to `set`. A mixed set (some patterns
    /// separator-less, some not) is normal, so the reduction is per pattern — which
    /// means the whole-path and basename patterns cannot share a single pass.
    basename: Vec<bool>,
    /// Whether *any* pattern in the set is a basename one. Precomputed so the common
    /// uniform-whole-path set takes a single [`regex::bytes::RegexSet`] pass with no
    /// per-pattern work at all.
    any_basename: bool,
    opts: GlobOpts,
    patterns: Vec<String>,
}

impl GlobSet {
    /// Does **any** pattern in the set match `path`? Takes bytes, as
    /// [`Glob::is_match`] does.
    pub fn is_match(&self, path: impl AsRef<[u8]>) -> bool {
        let normalized = self.opts.style.normalize_candidate(path.as_ref());
        if !self.any_basename {
            // Uniform whole-path set: one RegexSet pass, no per-pattern work.
            return self.set.is_match(&normalized);
        }
        // The binding is load-bearing, not redundant: `matches_iter` borrows
        // `normalized`, and as a tail expression that iterator would be a temporary
        // dropped only AFTER the `Cow` it borrows (E0597). Binding it drops the
        // iterator at the end of this statement instead.
        let hit = self.matches_iter(&normalized).next().is_some();
        hit
    }

    /// The **0-based** indices of the patterns matching `path`, ascending.
    pub fn matches(&self, path: impl AsRef<[u8]>) -> Vec<usize> {
        let normalized = self.opts.style.normalize_candidate(path.as_ref());
        self.matches_iter(&normalized).collect()
    }

    /// The patterns as written, in set order.
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// Shared walk behind `is_match`/`matches`: run the set against the whole path and
    /// against the last component, and keep each hit that came from the pass its own
    /// pattern actually wants. Two passes at most, regardless of pattern count.
    fn matches_iter<'a>(&'a self, normalized: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
        let whole = self.set.matches(normalized);
        let base = self.any_basename.then(|| {
            let tail = last_component(normalized);
            // `tail == normalized` for a separator-less candidate; skip the second
            // pass rather than run an identical one.
            (tail != normalized).then(|| self.set.matches(tail))
        });
        let base = base.flatten();
        (0..self.basename.len()).filter(move |&i| {
            if self.basename[i] {
                match &base {
                    Some(m) => m.matched(i),
                    // No separator in the candidate, so its tail IS the whole path.
                    None => whole.matched(i),
                }
            } else {
                whole.matched(i)
            }
        })
    }
}

/// The last `/`-separated component of an already-normalized candidate, ignoring a
/// trailing separator (`"a/b/"` -> `"b"`). The whole string when there is no
/// separator, so a basename pattern degrades to a whole-path match on a bare name.
/// Byte-wise, like everything on the candidate side.
fn last_component(path: &[u8]) -> &[u8] {
    let trimmed = path.strip_suffix(b"/").unwrap_or(path);
    match trimmed.iter().rposition(|&b| b == b'/') {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    }
}

/// Whether `s` carries a glob metacharacter — i.e. whether it is a pattern at all
/// rather than a plain path. The single definition for every caller that needs to
/// branch on "did the user type a glob here". Every metacharacter is ASCII, so this is
/// a byte scan and answers for a non-UTF-8 name as readily as for a `&str`.
pub fn is_glob(s: impl AsRef<[u8]>) -> bool {
    s.as_ref()
        .iter()
        .any(|b| matches!(b, b'*' | b'?' | b'[' | b'{'))
}

/// Translate a glob to regex source without compiling or caching it. The
/// translation step on its own, for a caller that wants the regex to hand to
/// another engine.
///
/// Fails on [`GlobOpts::basename`] rather than answering with a regex that does not
/// mean what the glob does: `basename` reduces the *candidate* to its last component
/// before the regex runs, and that reduction cannot be folded into the regex (with
/// `literal_separator = false` a `*` crosses `/`, so no `(?:.*/)?` prefix is
/// faithful). Since the entire point of this function is to hand the translation to
/// an engine that will not perform the reduction, returning one anyway would be a
/// silently-wrong answer — see [`Glob::regex`] for the introspection form, which
/// belongs to a glob that *does* apply the reduction.
pub fn to_regex(pattern: &str, opts: &GlobOpts) -> Result<String, String> {
    let (match_basename, src) = build(pattern, opts)?;
    if match_basename {
        return Err(format!(
            "cannot translate '{pattern}' with basename = true to a standalone regex: \
             basename matches the candidate's last component, which is a reduction of \
             the path rather than part of the pattern (drop the option, or reduce the \
             path yourself before running the regex)"
        ));
    }
    Ok(src)
}

/// Parse `pattern` under `opts` and return `(match_basename, regex source)`.
///
/// `backslash_escape` is forced off in Windows style: there, `\` is a separator, so
/// treating it as an escape would make `src\*.rs` mean `src*.rs`. (Under Unix style
/// it stays on, which is how a literal `*` in a filename is spelled: `a\*b`.)
/// `allow_unclosed_class` is on so `foo[bar` is the literal `foo[bar` rather than a
/// hard error — a filename may genuinely contain a bracket, and this is the one glob
/// malformation common enough to be worth tolerating.
fn build(pattern: &str, opts: &GlobOpts) -> Result<(bool, String), String> {
    let normalized = opts.style.normalize(pattern);
    let glob = globset::GlobBuilder::new(&normalized)
        .case_insensitive(opts.ignorecase)
        .literal_separator(opts.literal_separator)
        .backslash_escape(opts.style == PathStyle::Unix)
        .empty_alternates(opts.empty_alternates)
        .allow_unclosed_class(true)
        .build()
        .map_err(|e| format!("invalid glob '{pattern}': {}", glob_error(&e)))?;
    let match_basename = opts.basename && !normalized.contains('/');
    Ok((match_basename, glob.regex().to_string()))
}

/// globset's `Display` prefixes the pattern (`error parsing glob '<pat>': …`), which
/// would read twice in our own message. Keep just the reason.
fn glob_error(e: &globset::Error) -> String {
    e.kind().to_string()
}

/// Compile the regex source globset emitted. `unicode(false)` +
/// `dot_matches_new_line(true)` mirror how globset configures its own engine
/// (`syntax.utf8(false).dot_matches_new_line(true)`); mismatching those would change
/// what the emitted source means.
fn compile_regex(src: &str, pattern: &str) -> Result<regex::bytes::Regex, String> {
    regex::bytes::RegexBuilder::new(src)
        .unicode(false)
        .dot_matches_new_line(true)
        .build()
        .map_err(|e| format!("invalid glob '{pattern}': {e}"))
}

/// How many compiled globs (and sets) to keep before dropping the lot. A plugin that
/// builds a fresh pattern per keystroke must not grow the cache without bound; a
/// wholesale clear at the ceiling keeps the hot working set (which recompiles once)
/// without the bookkeeping of a true LRU.
const CACHE_MAX: usize = 512;

/// A single glob's cache entry, keyed by the pattern and the options — every option
/// changes the emitted regex, so keying on the pattern alone would serve one
/// compile's answer to every caller.
type GlobCache = RefCell<HashMap<(String, GlobOpts), Rc<Glob>>>;
/// A glob set's cache entry, keyed the same way over the whole pattern list.
type SetCache = RefCell<HashMap<(Vec<String>, GlobOpts), Rc<GlobSet>>>;

thread_local! {
    /// Compiled globs. Thread-local rather than a global `Mutex`: the core is
    /// synchronous and the Lua VM single-threaded, so there is no contention to
    /// arbitrate — and no lock to hold across a match.
    static GLOB_CACHE: GlobCache = RefCell::new(HashMap::new());
    /// Compiled sets.
    static SET_CACHE: SetCache = RefCell::new(HashMap::new());
}

/// Compile `pattern` under `opts`, reusing the cached regex when this exact
/// (pattern, opts) pair has been compiled before on this thread. The entry point
/// every caller should use: matching in a loop then costs one parse and one regex
/// build total.
///
/// Returns the compile error as a message (never panics on a bad pattern) so callers
/// can fail loud in their own idiom.
pub fn compile(pattern: &str, opts: &GlobOpts) -> Result<Rc<Glob>, String> {
    let key = (pattern.to_string(), *opts);
    if let Some(hit) = GLOB_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return Ok(hit);
    }
    let (match_basename, regex_src) = build(pattern, opts)?;
    let re = compile_regex(&regex_src, pattern)?;
    let glob = Rc::new(Glob {
        pattern: pattern.to_string(),
        regex_src,
        re,
        opts: *opts,
        match_basename,
    });
    GLOB_CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        if cache.len() >= CACHE_MAX {
            cache.clear();
        }
        cache.insert(key, Rc::clone(&glob));
    });
    Ok(glob)
}

/// Compile `patterns` into one cached [`GlobSet`]. An invalid pattern fails the whole
/// set (loud, with which pattern) rather than being dropped — a silently-ignored
/// pattern in an ignore list is a bug that surfaces much later.
pub fn compile_set(patterns: &[String], opts: &GlobOpts) -> Result<Rc<GlobSet>, String> {
    let key = (patterns.to_vec(), *opts);
    if let Some(hit) = SET_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return Ok(hit);
    }
    let mut sources = Vec::with_capacity(patterns.len());
    let mut basename = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        let (is_base, src) = build(pattern, opts)?;
        sources.push(src);
        basename.push(is_base);
    }
    let set = regex::bytes::RegexSetBuilder::new(&sources)
        .unicode(false)
        .dot_matches_new_line(true)
        .build()
        .map_err(|e| format!("invalid glob set: {e}"))?;
    let globset = Rc::new(GlobSet {
        set,
        any_basename: basename.iter().any(|b| *b),
        basename,
        opts: *opts,
        patterns: patterns.to_vec(),
    });
    SET_CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        if cache.len() >= CACHE_MAX {
            cache.clear();
        }
        cache.insert(key, Rc::clone(&globset));
    });
    Ok(globset)
}

/// One-shot `pattern` against `path`, through the cache — the convenience behind
/// `nx.glob.match`. Identical in cost to `compile(..)?.is_match(..)` after the first
/// call. `path` is bytes, as everywhere on the candidate side.
pub fn is_match(pattern: &str, path: impl AsRef<[u8]>, opts: &GlobOpts) -> Result<bool, String> {
    Ok(compile(pattern, opts)?.is_match(path))
}
