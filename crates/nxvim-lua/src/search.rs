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

use mlua::{Lua, Table, Value};
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
                .map_err(|e| format!("nx.buf.search: invalid pcre pattern: {e}")),
            "vim" => nxvim_regex::VimRegex::compile(pattern)
                .map(|re| Compiled::Vim { re, ignorecase })
                .map_err(|e| format!("nx.buf.search: invalid vim pattern: {e}")),
            other => Err(format!(
                "nx.buf.search: engine must be \"pcre\" or \"vim\", got {other:?}"
            )),
        }
    }

    /// The first match whose start is at byte offset `from` or later.
    fn first_from(&self, line: &str, from: usize) -> Option<LineHit> {
        if from > line.len() {
            return None;
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
            Compiled::Pcre(re) => re
                .captures_iter(line)
                .find(|c| c.get(0).map(|m| m.start() >= from).unwrap_or(false))
                .map(|c| pcre_hit(&c)),
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
                let upper = hay.len().min(limit.saturating_add(needle.len()));
                hay[..upper.min(hay.len())]
                    .rfind(needle.as_str())
                    .and_then(|start| {
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

    let line_at = |i: usize| -> mlua::Result<String> {
        Ok(lines.get::<Option<String>>(i as i64)?.unwrap_or_default())
    };

    if backward {
        for i in (1..=from_line.min(n)).rev() {
            let line = line_at(i)?;
            let before = if i == from_line {
                Some(from_col.min(line.len()))
            } else {
                None
            };
            if let Some(hit) = compiled.last_before(&line, before) {
                return make_match(lua, i, &line, &hit);
            }
        }
    } else {
        for i in from_line..=n {
            let line = line_at(i)?;
            let from = if i == from_line {
                from_col.min(line.len())
            } else {
                0
            };
            if let Some(hit) = compiled.first_from(&line, from) {
                return make_match(lua, i, &line, &hit);
            }
        }
    }
    Ok(Value::Nil)
}

/// Build the result table: `{ line, col, end_line, end_col, text, captures }`
/// (1-based line, 0-based byte cols; `end_col` exclusive).
fn make_match(lua: &Lua, line_no: usize, line: &str, hit: &LineHit) -> mlua::Result<Value> {
    let t = lua.create_table()?;
    t.set("line", line_no)?;
    t.set("col", hit.start)?;
    t.set("end_line", line_no)?;
    t.set("end_col", hit.end)?;
    t.set("text", &line[hit.start..hit.end])?;
    t.set(
        "captures",
        lua.create_sequence_from(hit.captures.iter().cloned())?,
    )?;
    Ok(Value::Table(t))
}
