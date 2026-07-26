//! The `nx.glob` bridge: Lua-facing wrappers over the canonical glob engine in
//! [`nxvim_core::glob`]. The parsing, the glob→regex translation and the compiled-regex
//! cache all live in the core (pure, synchronous, wasm-safe); this module only
//! converts the opts table, hands back userdata, and turns a bad pattern into a
//! loud Lua error.
//!
//! The documented surface (`nx.glob.match`, `nx.glob.compile`, …) is the thin Lua
//! wrapper in `prelude/glob.lua`; the `nx._glob*` bridges here are what it calls.

use mlua::{Table, UserData, UserDataMethods, Value};
use nxvim_core::glob::{GlobOpts, PathStyle};

/// A candidate-path argument, as the **bytes** Lua handed over. Paths need not be
/// valid UTF-8 (a latin-1 name on disk, a name off the encoding seam), and the engine
/// matches bytes, so nothing here decodes: rejecting or lossily rewriting the name is
/// exactly what the byte-oriented regex exists to avoid.
///
/// Checked by hand rather than through mlua's automatic coercion so a wrong type names
/// the *public* function and its argument. mlua's own message would read
/// `bad argument #1 … in field '_glob_is_glob'`, pointing at a private bridge the
/// caller never wrote.
pub(crate) fn candidate<'a>(
    func: &str,
    arg: &str,
    v: &'a Value,
) -> mlua::Result<&'a mlua::LuaString> {
    v.as_string().ok_or_else(|| {
        mlua::Error::runtime(format!(
            "{func}: {arg} must be a string, got {}",
            v.type_name()
        ))
    })
}

/// A pattern argument. Unlike a candidate this must be valid UTF-8, because globset's
/// parser takes `&str` — glob syntax is ASCII, so a pattern that is not UTF-8 is a
/// mistake to fail loud on rather than quietly match nothing.
pub(crate) fn pattern(func: &str, v: &Value) -> mlua::Result<String> {
    let s = candidate(func, "pattern", v)?;
    s.to_str()
        .map(|s| s.to_string())
        .map_err(|_| mlua::Error::runtime(format!("{func}: pattern must be valid UTF-8")))
}

/// Read the shared opts table. Every field is optional and defaults to
/// [`GlobOpts::default`] — except `style`, whose *unknown* spellings fail loud rather
/// than silently falling back to the host convention (a typo'd `style = "win32"`
/// that quietly matched Unix-style would be near-impossible to spot).
pub(crate) fn opts_from_table(opts: Option<&Table>) -> mlua::Result<GlobOpts> {
    let mut out = GlobOpts::default();
    let Some(t) = opts else { return Ok(out) };
    if let Some(name) = t.get::<Option<String>>("style")? {
        out.style = PathStyle::from_name(&name).ok_or_else(|| {
            mlua::Error::runtime(format!(
                "nx.glob: unknown path style '{name}' (expected \"unix\" or \"windows\")"
            ))
        })?;
    }
    if let Some(v) = t.get::<Option<bool>>("ignorecase")? {
        out.ignorecase = v;
    }
    if let Some(v) = t.get::<Option<bool>>("literal_separator")? {
        out.literal_separator = v;
    }
    if let Some(v) = t.get::<Option<bool>>("basename")? {
        out.basename = v;
    }
    if let Some(v) = t.get::<Option<bool>>("empty_alternates")? {
        out.empty_alternates = v;
    }
    Ok(out)
}

/// A compiled single glob, handed to Lua as `nx.glob.compile(...)`. Holds the core's
/// cached [`nxvim_core::glob::Glob`], so several Lua objects over the same
/// (pattern, opts) share one compiled regex.
pub struct LuaGlob {
    inner: std::rc::Rc<nxvim_core::glob::Glob>,
}

impl LuaGlob {
    pub fn compile(pattern: &str, opts: Option<&Table>) -> mlua::Result<Self> {
        let opts = opts_from_table(opts)?;
        let inner = nxvim_core::glob::compile(pattern, &opts)
            .map_err(|e| mlua::Error::runtime(format!("nx.glob: {e}")))?;
        Ok(LuaGlob { inner })
    }
}

impl UserData for LuaGlob {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // `g:test(path)` -> boolean: does the glob match `path`? Matched as bytes, so
        // a non-UTF-8 path answers by its real bytes rather than raising.
        methods.add_method("test", |_, this, path: Value| {
            let path = candidate("nx.glob glob:test", "path", &path)?;
            Ok(this.inner.is_match(&*path.as_bytes()))
        });
        // `g:pattern()` -> the glob as written.
        methods.add_method("pattern", |_, this, ()| {
            Ok(this.inner.pattern().to_string())
        });
        // `g:regex()` -> the regex source the glob translated to (introspection: a
        // glob that mismatches is easiest to debug against its actual regex).
        methods.add_method("regex", |_, this, ()| Ok(this.inner.regex().to_string()));
    }
}

/// A compiled glob **set**, handed to Lua as `nx.glob.set(...)`. One `RegexSet` pass
/// tests a path against every pattern, so an ignore list stays one match regardless
/// of how many globs it holds.
pub struct LuaGlobSet {
    inner: std::rc::Rc<nxvim_core::glob::GlobSet>,
}

impl LuaGlobSet {
    pub fn compile(patterns: Vec<String>, opts: Option<&Table>) -> mlua::Result<Self> {
        let opts = opts_from_table(opts)?;
        let inner = nxvim_core::glob::compile_set(&patterns, &opts)
            .map_err(|e| mlua::Error::runtime(format!("nx.glob: {e}")))?;
        Ok(LuaGlobSet { inner })
    }
}

impl UserData for LuaGlobSet {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // `s:test(path)` -> boolean: does ANY pattern match? Bytes, as `glob:test`.
        methods.add_method("test", |_, this, path: Value| {
            let path = candidate("nx.glob globset:test", "path", &path)?;
            Ok(this.inner.is_match(&*path.as_bytes()))
        });
        // `s:matches(path)` -> list of the 1-based indices of the matching patterns
        // (Lua-indexed, so `patterns[i]` on the caller's own list lines up).
        methods.add_method("matches", |lua, this, path: Value| {
            let path = candidate("nx.glob globset:matches", "path", &path)?;
            let hits = this.inner.matches(&*path.as_bytes());
            lua.create_sequence_from(
                hits.into_iter()
                    .map(|i| crate::convert::lua_int(i as i64 + 1)),
            )
        });
        // `s:patterns()` -> the pattern list as written, in set order.
        methods.add_method("patterns", |lua, this, ()| {
            lua.create_sequence_from(this.inner.patterns().iter().map(String::as_str))
        });
    }
}
