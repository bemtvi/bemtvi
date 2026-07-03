//! Native buffer text search, exposed to Lua as `nx.buf.search` (the bridge fn
//! `nx._buf_search`). It runs over the buffer line **mirror** the server pushes into
//! the VM (`nx._bufs[buf].lines`) — never live core state — so a plugin can locate a
//! section (a conflict marker, a heading, …) without scanning the buffer line by line
//! in Lua.
//!
//! Three lookup modes, chosen by the opts table:
//!   * `plain`  — a literal substring (optionally ascii-case-insensitive),
//!   * `engine = "pcre"` — the Rust `regex` crate (the default; canonical syntax),
//!   * `engine = "vim"`  — the embedded vim regexp engine (`nxvim_regex`).
//!
//! Matching is line-by-line (each mirror line is its own haystack), exactly like the
//! editor's own search: `^`/`$` anchor to line edges and a multi-line (`\n`-spanning)
//! pattern is not supported. The scan starts at `from = { line, col }` (1-based line,
//! 0-based byte col) and runs forward, or backward with `backward = true`.

use crate::convert::lua_int;
use mlua::{Lua, Table, UserData, UserDataMethods, Value, Variadic};
use regex::RegexBuilder;

/// One match within a single line: byte offsets [start, end) plus the submatch
/// strings (`\1`..) as captured (a non-participating group is "").
struct LineHit {
    start: usize,
    end: usize,
    captures: Vec<String>,
}

/// The compiled needle, ready to scan a line forward or backward.
enum Compiled {
    /// A literal substring; `needle` is pre-lowercased when `ignorecase`.
    Plain {
        needle: String,
        ignorecase: bool,
    },
    Pcre(regex::Regex),
    Vim {
        re: nxvim_regex::VimRegex,
        ignorecase: bool,
    },
}

/// The next scan offset after a match `[start, end)` — past its end, or one char on
/// for an empty (zero-width) match so a backward walk can't spin.
fn advance(line: &str, start: usize, end: usize) -> usize {
    if end > start {
        end
    } else {
        line[end..]
            .chars()
            .next()
            .map_or(end + 1, |c| end + c.len_utf8())
    }
}

impl Compiled {
    fn compile(pattern: &str, plain: bool, engine: &str, ignorecase: bool) -> Result<Self, String> {
        if plain {
            let needle = if ignorecase {
                pattern.to_ascii_lowercase()
            } else {
                pattern.to_string()
            };
            return Ok(Compiled::Plain { needle, ignorecase });
        }
        match engine {
            "pcre" => RegexBuilder::new(pattern)
                .case_insensitive(ignorecase)
                .build()
                .map(Compiled::Pcre)
                .map_err(|e| format!("invalid pcre pattern: {e}")),
            "vim" => nxvim_regex::VimRegex::compile(pattern)
                .map(|re| Compiled::Vim { re, ignorecase })
                .map_err(|e| format!("invalid vim pattern: {e}")),
            other => Err(format!(
                "regex engine must be \"pcre\" or \"vim\", got {other:?}"
            )),
        }
    }

    /// The first match whose start is at byte offset `from` or later.
    fn first_from(&self, line: &str, from: usize) -> Option<LineHit> {
        if from > line.len() {
            return None;
        }
        // A match can only start on a char boundary, but `from` is a caller-chosen
        // byte offset that may point inside a multi-byte char (e.g. a `:find` init).
        // Round it up to the next boundary so the slicing / engines below never
        // panic on an off-boundary start.
        let mut from = from;
        while !line.is_char_boundary(from) {
            from += 1;
        }
        match self {
            Compiled::Plain { needle, ignorecase } => {
                let hay = if *ignorecase {
                    line.to_ascii_lowercase() // ascii-fold preserves byte length → offsets stay valid
                } else {
                    line.to_string()
                };
                hay[from..].find(needle.as_str()).map(|rel| {
                    let start = from + rel;
                    LineHit {
                        start,
                        end: start + needle.len(),
                        captures: vec![],
                    }
                })
            }
            // `captures_at` restarts the scan at `from`, so a match overlapping an
            // earlier (pre-`from`) one is still found — walking the non-overlapping
            // match set from offset 0 would skip it (and rescan the prefix for nothing).
            Compiled::Pcre(re) => re.captures_at(line, from).map(|c| pcre_hit(&c)),
            Compiled::Vim { re, ignorecase } => re
                .exec_line(line, from, *ignorecase)
                .ok()
                .flatten()
                .map(|m| vim_hit(line, &m)),
        }
    }

    /// The last match whose start is strictly before `before` (the whole line when
    /// `before` is `None`).
    fn last_before(&self, line: &str, before: Option<usize>) -> Option<LineHit> {
        let limit = before.unwrap_or(usize::MAX);
        match self {
            Compiled::Plain { needle, ignorecase } => {
                let hay = if *ignorecase {
                    line.to_ascii_lowercase()
                } else {
                    line.to_string()
                };
                // last occurrence whose start is < limit
                let mut upper = hay.len().min(limit.saturating_add(needle.len()));
                // A valid-UTF-8 needle can only match ending on a char boundary, so
                // flooring a mid-char `upper` (limit is a caller-chosen byte offset)
                // excludes no match while keeping the slice panic-free.
                while !hay.is_char_boundary(upper) {
                    upper -= 1;
                }
                hay[..upper].rfind(needle.as_str()).and_then(|start| {
                    if start < limit {
                        Some(LineHit {
                            start,
                            end: start + needle.len(),
                            captures: vec![],
                        })
                    } else {
                        None
                    }
                })
            }
            Compiled::Pcre(re) => re
                .captures_iter(line)
                .filter(|c| c.get(0).map(|m| m.start() < limit).unwrap_or(false))
                .last()
                .map(|c| pcre_hit(&c)),
            Compiled::Vim { re, ignorecase } => {
                let mut from = 0;
                let mut best: Option<LineHit> = None;
                while from <= line.len() {
                    let Some(m) = re.exec_line(line, from, *ignorecase).ok().flatten() else {
                        break;
                    };
                    if m.start >= limit {
                        break;
                    }
                    let next = advance(line, m.start, m.end);
                    best = Some(vim_hit(line, &m));
                    from = next;
                }
                best
            }
        }
    }

    /// Every non-overlapping match in `line`, left to right (a zero-width match
    /// advances one char so the walk can't spin). Used by the `nx.regex` object's
    /// `:gmatch` / `:gsub`, which need the whole match set in one pass.
    fn all(&self, line: &str) -> Vec<LineHit> {
        let mut out = Vec::new();
        match self {
            Compiled::Pcre(re) => {
                for c in re.captures_iter(line) {
                    out.push(pcre_hit(&c));
                }
            }
            Compiled::Plain { needle, ignorecase } => {
                let hay = if *ignorecase {
                    line.to_ascii_lowercase()
                } else {
                    line.to_string()
                };
                let mut from = 0;
                while from <= hay.len() {
                    let Some(rel) = hay[from..].find(needle.as_str()) else {
                        break;
                    };
                    let start = from + rel;
                    let end = start + needle.len();
                    out.push(LineHit {
                        start,
                        end,
                        captures: vec![],
                    });
                    from = advance(line, start, end);
                }
            }
            Compiled::Vim { re, ignorecase } => {
                let mut from = 0;
                while from <= line.len() {
                    let Some(m) = re.exec_line(line, from, *ignorecase).ok().flatten() else {
                        break;
                    };
                    let next = advance(line, m.start, m.end);
                    out.push(vim_hit(line, &m));
                    from = next;
                }
            }
        }
        out
    }

    /// Whether `line` matches anywhere (cheaper than [`Compiled::first_from`] for
    /// pcre, which can answer without building captures).
    fn is_match(&self, line: &str) -> bool {
        match self {
            Compiled::Pcre(re) => re.is_match(line),
            _ => self.first_from(line, 0).is_some(),
        }
    }
}

/// Captures `\1`.. from a pcre match (group 0 is the whole match, skipped). A
/// non-participating group becomes "".
fn pcre_hit(c: &regex::Captures) -> LineHit {
    let whole = c.get(0).expect("group 0 always present");
    let captures = (1..c.len())
        .map(|i| c.get(i).map(|m| m.as_str().to_string()).unwrap_or_default())
        .collect();
    LineHit {
        start: whole.start(),
        end: whole.end(),
        captures,
    }
}

/// Captures `\1`.. from a vim match, trimmed to the highest participating group.
fn vim_hit(line: &str, m: &nxvim_regex::LineMatch) -> LineHit {
    let highest = (1..m.submatches.len())
        .rev()
        .find(|&i| m.submatches[i].is_some())
        .unwrap_or(0);
    let captures = (1..=highest)
        .map(|i| {
            m.submatches[i]
                .map(|(s, e)| line[s..e].to_string())
                .unwrap_or_default()
        })
        .collect();
    LineHit {
        start: m.start,
        end: m.end,
        captures,
    }
}

/// `nx._buf_search(lines, pattern, opts)` — see the module header. `lines` is the
/// mirror's 1-based line array; returns a match table or `nil`.
pub fn buf_search(lua: &Lua, lines: Table, pattern: String, opts: Table) -> mlua::Result<Value> {
    let plain = opts.get::<Option<bool>>("plain")?.unwrap_or(false);
    let engine = opts
        .get::<Option<String>>("engine")?
        .unwrap_or_else(|| "pcre".into());
    let ignorecase = opts.get::<Option<bool>>("ignorecase")?.unwrap_or(false);
    let backward = opts.get::<Option<bool>>("backward")?.unwrap_or(false);
    let (from_line, from_col) = match opts.get::<Option<Table>>("from")? {
        Some(f) => (
            f.get::<Option<usize>>("line")?.unwrap_or(1).max(1),
            f.get::<Option<usize>>("col")?.unwrap_or(0),
        ),
        None => (1, 0),
    };

    let n = lines.raw_len();
    if n == 0 {
        return Ok(Value::Nil);
    }
    let compiled =
        Compiled::compile(&pattern, plain, &engine, ignorecase).map_err(mlua::Error::runtime)?;

    // Fetch line `i` from the mirror as a borrowed Lua string (zero-copy): the
    // `Compiled` scanners take `&str`, so reading the bytes in place avoids a fresh
    // Rust `String` allocation per scanned line (a no-match scan would otherwise
    // allocate one owned line for the whole buffer).
    let line_str = |i: usize| -> mlua::Result<Option<mlua::String>> {
        lines.get::<Option<mlua::String>>(i as i64)
    };

    if backward {
        for i in (1..=from_line.min(n)).rev() {
            let raw = line_str(i)?;
            let borrowed = raw.as_ref().map(|s| s.to_str()).transpose()?;
            let line = borrowed.as_deref().unwrap_or("");
            let before = if i == from_line {
                Some(from_col.min(line.len()))
            } else {
                None
            };
            if let Some(hit) = compiled.last_before(line, before) {
                return make_match(lua, i, line, &hit);
            }
        }
    } else {
        for i in from_line..=n {
            let raw = line_str(i)?;
            let borrowed = raw.as_ref().map(|s| s.to_str()).transpose()?;
            let line = borrowed.as_deref().unwrap_or("");
            let from = if i == from_line {
                from_col.min(line.len())
            } else {
                0
            };
            if let Some(hit) = compiled.first_from(line, from) {
                return make_match(lua, i, line, &hit);
            }
        }
    }
    Ok(Value::Nil)
}

/// Build the result table: `{ line, col, end_line, end_col, text, captures }`
/// (1-based line, 0-based byte cols; `end_col` exclusive).
fn make_match(lua: &Lua, line_no: usize, line: &str, hit: &LineHit) -> mlua::Result<Value> {
    let t = lua.create_table_with_capacity(0, 6)?;
    t.set("line", line_no)?;
    t.set("col", hit.start)?;
    t.set("end_line", line_no)?;
    t.set("end_col", hit.end)?;
    t.set("text", &line[hit.start..hit.end])?;
    t.set(
        "captures",
        // Borrow each capture as `&str` — mlua copies the bytes into the Lua string
        // either way, so cloning into an intermediate `String` first is wasted work.
        lua.create_sequence_from(hit.captures.iter().map(String::as_str))?,
    )?;
    Ok(Value::Table(t))
}

/// Slice `s[start..end]`, failing loud rather than panicking if the engine handed
/// back a reversed or off-char-boundary range (possible with the vim engine's
/// `\zs`/`\ze`/look-around). Mirrors `vimregex::safe_slice`.
fn slice(s: &str, start: usize, end: usize) -> mlua::Result<&str> {
    s.get(start..end).ok_or_else(|| {
        mlua::Error::runtime(format!(
            "nx.regex: match byte range {start}..{end} is not a valid slice (off a char boundary or reversed)"
        ))
    })
}

/// Translate a `string.find`-style `init` (1-based, may be negative to count from
/// the end) into a 0-based byte offset to start scanning from.
fn norm_init(init: Option<i64>, len: usize) -> usize {
    match init {
        None => 0,
        Some(i) if i > 0 => ((i - 1) as usize).min(len),
        Some(0) => 0,
        // Negative: from the end, where -1 is the last byte.
        Some(i) => len.saturating_sub(i.unsigned_abs() as usize),
    }
}

/// `nx.regex(pat, opts?)` — a compiled pattern object for matching **Lua strings**
/// (a more capable `string.find`/`match`/`gmatch`/`gsub` with a real regex
/// dialect). Defaults to the Rust `regex` crate (`engine = "pcre"`, canonical
/// syntax); `engine = "vim"` selects the vim regexp engine and `plain = true` a
/// literal substring. Offsets follow the `string` library: **1-based**, byte-based,
/// with `:find`'s `end` inclusive so `s:sub(re:find(s))` is the match.
pub struct NxRegex {
    re: Compiled,
}

impl NxRegex {
    pub fn compile(pattern: &str, opts: Option<&Table>) -> mlua::Result<Self> {
        let (plain, engine, ignorecase) = match opts {
            Some(o) => (
                o.get::<Option<bool>>("plain")?.unwrap_or(false),
                o.get::<Option<String>>("engine")?
                    .unwrap_or_else(|| "pcre".into()),
                o.get::<Option<bool>>("ignorecase")?.unwrap_or(false),
            ),
            None => (false, "pcre".to_string(), false),
        };
        let re =
            Compiled::compile(pattern, plain, &engine, ignorecase).map_err(mlua::Error::runtime)?;
        Ok(NxRegex { re })
    }
}

impl UserData for NxRegex {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // `re:find(s, init?)` -> start, end, cap1, cap2, … (1-based, `end`
        // inclusive) or nil — like `string.find` in `plain=false` mode.
        methods.add_method(
            "find",
            |lua, this, (text, init): (mlua::String, Option<i64>)| {
                let s = text.to_str()?;
                let from = norm_init(init, s.len());
                let Some(hit) = this.re.first_from(&s, from) else {
                    // No match -> a single nil, like string.find.
                    return Ok(Variadic::from_iter([Value::Nil]));
                };
                let mut out = vec![
                    Value::Integer(lua_int((hit.start + 1) as i64)),
                    Value::Integer(lua_int(hit.end as i64)),
                ];
                for c in &hit.captures {
                    out.push(Value::String(lua.create_string(c)?));
                }
                Ok(Variadic::from_iter(out))
            },
        );

        // `re:match(s, init?)` -> the capture string(s), or the whole match when
        // the pattern has no captures, or nil — like `string.match`.
        methods.add_method(
            "match",
            |lua, this, (text, init): (mlua::String, Option<i64>)| {
                let s = text.to_str()?;
                let from = norm_init(init, s.len());
                let Some(hit) = this.re.first_from(&s, from) else {
                    // No match -> a single nil, like string.match.
                    return Ok(Variadic::from_iter([Value::Nil]));
                };
                let out = if hit.captures.is_empty() {
                    vec![Value::String(
                        lua.create_string(slice(&s, hit.start, hit.end)?)?,
                    )]
                } else {
                    hit.captures
                        .iter()
                        .map(|c| lua.create_string(c).map(Value::String))
                        .collect::<mlua::Result<Vec<_>>>()?
                };
                Ok(Variadic::from_iter(out))
            },
        );

        // `re:gmatch(s)` -> an iterator yielding each match's captures (or the
        // whole match when the pattern has no captures) — like `string.gmatch`.
        methods.add_method("gmatch", |lua, this, text: mlua::String| {
            let s = text.to_str()?;
            let mut items: Vec<Vec<String>> = Vec::new();
            for hit in this.re.all(&s) {
                if hit.captures.is_empty() {
                    items.push(vec![slice(&s, hit.start, hit.end)?.to_string()]);
                } else {
                    items.push(hit.captures);
                }
            }
            let idx = std::cell::Cell::new(0usize);
            lua.create_function(move |lua, ()| {
                let i = idx.get();
                if i >= items.len() {
                    return Ok(Variadic::new());
                }
                idx.set(i + 1);
                let vals = items[i]
                    .iter()
                    .map(|c| lua.create_string(c).map(Value::String))
                    .collect::<mlua::Result<Vec<_>>>()?;
                Ok(Variadic::from_iter(vals))
            })
        });

        // `re:gsub(s, repl, n?)` -> newstring, count — like `string.gsub`. `repl`
        // is a string (`%0` whole match, `%1`-`%9` captures, `%%` literal), a
        // function called with the captures, or a table keyed by the first capture.
        methods.add_method(
            "gsub",
            |lua, this, (text, repl, n): (mlua::String, Value, Option<i64>)| {
                let s = text.to_str()?;
                let max = n.filter(|&n| n >= 0).map(|n| n as usize);
                let mut out = String::new();
                let mut last = 0usize;
                let mut count = 0usize;
                for hit in this.re.all(&s) {
                    if max.is_some_and(|m| count >= m) {
                        break;
                    }
                    out.push_str(slice(&s, last, hit.start)?);
                    let whole = slice(&s, hit.start, hit.end)?;
                    match compute_repl(lua, &repl, whole, &hit.captures)? {
                        Some(r) => out.push_str(&r),
                        None => out.push_str(whole),
                    }
                    last = hit.end;
                    count += 1;
                }
                out.push_str(slice(&s, last, s.len())?);
                Ok((lua.create_string(&out)?, count as i64))
            },
        );

        // `re:test(s)` -> bool: does the pattern match anywhere.
        methods.add_method("test", |_, this, text: mlua::String| {
            Ok(this.re.is_match(&text.to_str()?))
        });
    }
}

/// Expand a `string.gsub`-style replacement template against one match: `%0` is the
/// whole match, `%1`-`%9` the captures (or the whole match for `%1` when the pattern
/// has no captures, as in Lua), `%%` a literal `%`. Fails loud on a bad item.
fn expand_str_repl(template: &str, whole: &str, caps: &[String]) -> mlua::Result<String> {
    let mut out = String::new();
    let mut chars = template.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some(d @ '0'..='9') => {
                let nth = d as usize - '0' as usize;
                // `%0` is the whole match; so is `%1` when the pattern has no
                // captures (Lua's rule).
                if nth == 0 || (caps.is_empty() && nth == 1) {
                    out.push_str(whole);
                } else if let Some(cap) = caps.get(nth - 1) {
                    out.push_str(cap);
                } else {
                    return Err(mlua::Error::runtime(format!(
                        "nx.regex gsub: replacement refers to %{nth} but the pattern has {} capture(s)",
                        caps.len()
                    )));
                }
            }
            Some(other) => {
                return Err(mlua::Error::runtime(format!(
                    "nx.regex gsub: invalid replacement item '%{other}'"
                )))
            }
            None => {
                return Err(mlua::Error::runtime(
                    "nx.regex gsub: replacement string ends with a lone '%'",
                ))
            }
        }
    }
    Ok(out)
}

/// Turn a `gsub` function/table replacement *result* into the text to splice in:
/// `nil`/`false` keeps the original match (Lua semantics), a string/number is used,
/// anything else is a loud error.
fn repl_value_to_string(v: Value) -> mlua::Result<Option<String>> {
    match v {
        Value::Nil | Value::Boolean(false) => Ok(None),
        Value::String(s) => Ok(Some(s.to_str()?.to_owned())),
        Value::Integer(i) => Ok(Some(i.to_string())),
        Value::Number(n) => Ok(Some(n.to_string())),
        other => Err(mlua::Error::runtime(format!(
            "nx.regex gsub: replacement value must be a string, number, or nil/false, got {}",
            other.type_name()
        ))),
    }
}

/// Compute the replacement text for one match from a `gsub` `repl` (string / table /
/// function), or `None` to keep the original match. A function is called with the
/// captures (or the whole match when there are none); a table is keyed by the first
/// capture (or the whole match).
fn compute_repl(
    lua: &Lua,
    repl: &Value,
    whole: &str,
    caps: &[String],
) -> mlua::Result<Option<String>> {
    match repl {
        Value::String(s) => Ok(Some(expand_str_repl(&s.to_str()?, whole, caps)?)),
        Value::Function(f) => {
            let args: Vec<Value> = if caps.is_empty() {
                vec![Value::String(lua.create_string(whole)?)]
            } else {
                caps.iter()
                    .map(|c| lua.create_string(c).map(Value::String))
                    .collect::<mlua::Result<_>>()?
            };
            repl_value_to_string(f.call(Variadic::from_iter(args))?)
        }
        Value::Table(t) => {
            let key = if caps.is_empty() {
                whole
            } else {
                caps[0].as_str()
            };
            repl_value_to_string(t.get(key)?)
        }
        other => Err(mlua::Error::runtime(format!(
            "nx.regex gsub: replacement must be a string, table, or function, got {}",
            other.type_name()
        ))),
    }
}
