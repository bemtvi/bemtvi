//! The Lua↔Rust value bridges. `mlua::Value` flows out to `rmpv` (the
//! `nvim_exec_lua` return path) and to `serde_json` (`vim.json.encode`, LSP op
//! payloads); JSON flows back in (`vim.json.decode`, LSP replies). The two
//! outbound table walkers share one classifier ([`classify_table`]) so the
//! array-vs-map rule lives in a single place. Also the small opts-table readers
//! the `nvim_set_hl` / `nx.run` bridges lean on.

use crate::ops::OptionValue;
use mlua::{Lua, Table};

/// Maximum table/value nesting the recursive bridges will walk before bailing
/// out with an error instead of recursing further. Guards against unbounded
/// native recursion — a *cyclic* Lua table (`t = {}; t.x = t`) or a pathologically
/// deep one handed to `vim.json.encode` / returned from `nvim_exec_lua` — which
/// would otherwise overflow the (≈2 MB) server-thread stack and **abort the whole
/// process** (a Rust stack overflow is uncatchable). 256 is far above any real
/// config/LSP payload (JSON decoded via serde_json is itself capped at 128 deep)
/// yet a tiny fraction of the available stack, so legitimate data always converts
/// and only a cycle / abuse hits the limit — loudly, as a recoverable Lua error.
const MAX_DEPTH: usize = 256;

/// The loud error raised when [`MAX_DEPTH`] is exceeded — surfaced to Lua (e.g.
/// `vim.json.encode`) or to the RPC caller (`nvim_exec_lua`) rather than crashing.
fn too_deep() -> mlua::Error {
    mlua::Error::external(format!(
        "value nesting too deep (cycle, or more than {MAX_DEPTH} levels)"
    ))
}

/// `mlua::Integer` is the Lua VM's integer width: `i64` on 64-bit native, but `i32`
/// on wasm32 (`lua_Integer` is `ptrdiff_t`, and wasm32 pointers are 32-bit). nxvim's
/// values are `i64`-centric, so these two helpers are the single portable bridge
/// between the widths — each an *identity* on native (which is what the `allow`
/// silences; the lint only fires there) and a real widen/narrow on the wasm edit-host
/// build. Use them instead of a bare `i64::from` / `as mlua::Integer` so a 32-bit-VM
/// build compiles unchanged. (Values crossing here — fds, indices, option numbers —
/// fit `i32`, so the wasm narrow never loses data.)
#[inline]
#[allow(clippy::useless_conversion)] // identity i64→i64 on native; sign-extend on wasm32
pub(crate) fn lua_i64(i: mlua::Integer) -> i64 {
    i64::from(i)
}

#[inline]
#[allow(clippy::unnecessary_cast)] // no-op i64→i64 on native; narrow to i32 on wasm32
pub(crate) fn lua_int(n: i64) -> mlua::Integer {
    n as mlua::Integer
}

/// Read a color field (`fg`/`bg`/`sp`) from an `nvim_set_hl` opts table. A
/// string (`"#rrggbb"` / `"NONE"` / a name) is kept verbatim; an integer color
/// is normalized to `#rrggbb`; anything else (incl. absent) is `None`. The core
/// does the actual parsing.
pub(crate) fn color_field(opts: &Table, key: &str) -> mlua::Result<Option<String>> {
    match opts.get::<mlua::Value>(key)? {
        mlua::Value::String(s) => Ok(Some(s.to_str()?.to_string())),
        mlua::Value::Integer(n) => Ok(Some(format!("#{:06x}", n & 0xff_ffff))),
        mlua::Value::Number(n) => Ok(Some(format!("#{:06x}", (n as i64) & 0xff_ffff))),
        _ => Ok(None),
    }
}

/// Read a boolean attribute (`bold`, `italic`, …) from an opts table; absent or
/// non-boolean reads as `false`.
pub(crate) fn flag_field(opts: &Table, key: &str) -> mlua::Result<bool> {
    Ok(opts.get::<Option<bool>>(key)?.unwrap_or(false))
}

/// Coerce a Lua value into the wire [`OptionValue`] the option-write bridges
/// (`nx._buf_set_option` / `_set_global_option` / `_win_set_option` /
/// `dock._set_opt` / `_set_workspace_option`) queue: a boolean → `Bool`, an
/// integer/float → `Number`, a string → `String`. Any other type (or a non-UTF-8
/// Lua string) yields `None`, which every caller treats as "ignore this write".
/// `nil` is *not* special-cased here — the workspace bridge handles `nil` as an
/// explicit clear before consulting this helper.
pub(crate) fn value_to_option(value: &mlua::Value) -> mlua::Result<Option<OptionValue>> {
    Ok(match value {
        mlua::Value::Boolean(b) => Some(OptionValue::Bool(*b)),
        mlua::Value::Integer(n) => Some(OptionValue::Number(lua_i64(*n))),
        mlua::Value::Number(n) => Some(OptionValue::Number(*n as i64)),
        mlua::Value::String(s) => s.to_str().ok().map(|s| OptionValue::String(s.to_string())),
        _ => None,
    })
}

/// Parse a color spec — as already normalized by [`color_field`] to `"#rrggbb"`,
/// a named color, or `"NONE"` — into the `0xRRGGBB` integer `nvim_get_hl` reports
/// colors as. Returns `None` for `NONE` / empty / unrecognized (all "no color").
///
/// This is a deliberate, parity-exact port of `nxvim_core::highlight::parse_color`
/// (+ its `named_color` table): the `nvim_set_hl` write-through uses it to build
/// the *same-turn* `nx._hl_defs` mirror row, which must match byte-for-byte the
/// row the server's between-turn push derives by sending the same string through
/// the core parser. Keep the two in lockstep — if core gains a color form, mirror
/// it here. nxvim-lua intentionally carries no `nxvim-core` dependency (it stays
/// free of color/registry types so the server pattern-matches the wire ops
/// directly), hence the small port rather than a shared call.
pub(crate) fn color_to_u32(spec: &str) -> Option<u32> {
    let spec = spec.trim();
    if spec.eq_ignore_ascii_case("none") || spec.is_empty() {
        return None;
    }
    if let Some(hex) = spec.strip_prefix('#') {
        return (hex.len() == 6)
            .then(|| u32::from_str_radix(hex, 16).ok())
            .flatten();
    }
    let (r, g, b) = match spec.to_ascii_lowercase().as_str() {
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "green" => (0, 128, 0),
        "blue" => (0, 0, 255),
        "yellow" => (255, 255, 0),
        "cyan" => (0, 255, 255),
        "magenta" => (255, 0, 255),
        "gray" | "grey" => (128, 128, 128),
        _ => return None,
    };
    Some(((r as u32) << 16) | ((g as u32) << 8) | b as u32)
}

/// Stringify a Lua value for `print` capture: prefer Lua's own `tostring`
/// (honors `__tostring`), fall back to a debug form.
pub(crate) fn stringify(lua: &Lua, value: &mlua::Value) -> String {
    match lua.coerce_string(value.clone()) {
        Ok(Some(s)) => s.to_str().map(|s| s.to_string()).unwrap_or_default(),
        _ => format!("{value:?}"),
    }
}

/// The metatable field `nx.json.null` / `nx.json.empty_object()` mark themselves with.
/// Lua cannot say either shape on its own — a `nil` value simply isn't a table entry,
/// and an empty table is indistinguishable from an empty array — so the two JSON values
/// a protocol peer *can* distinguish need a carrier. Both are single tables with a
/// marked metatable, checked before the array-vs-object rule ever runs.
const JSON_MARK: &str = "__nxvim_json";

/// Which marked JSON value `value` is, if any.
fn json_mark(value: &mlua::Value) -> Option<String> {
    let t = value.as_table()?;
    let meta = t.metatable()?;
    meta.get::<Option<String>>(JSON_MARK).ok().flatten()
}

/// The shape of a Lua table once classified: a sequence (every key an integer in
/// `1..=len`) or a map (anything else). `Map` keeps the keys *raw* so each caller
/// coerces them into its own key space ([`lua_to_rmpv`] vs [`json_key`]); the
/// integer keys of a non-sequence table are re-emitted into the map, after the
/// string-keyed entries, exactly as the old per-format walkers did.
enum LuaTable<V> {
    Array(Vec<V>),
    Map(Vec<(mlua::Value, V)>),
}

/// Walk a Lua table once, classifying it and converting every value with `conv`.
/// The single home of the "array iff keys are exactly `1..=len`" rule, shared by
/// the `rmpv` and `json` bridges, which differ only in their value/key types.
fn classify_table<V>(
    t: &Table,
    mut conv: impl FnMut(&mlua::Value) -> mlua::Result<V>,
) -> mlua::Result<LuaTable<V>> {
    let raw_len = t.raw_len();
    let len = raw_len as i64;
    // The sequence part is at most `raw_len` long; reserve it up front (exact for a
    // pure array, the common case). The map part stays unsized — its length is unknown.
    let mut entries: Vec<(i64, V)> = Vec::with_capacity(raw_len);
    let mut map: Vec<(mlua::Value, V)> = Vec::new();
    let mut is_seq = true;
    for pair in t.clone().pairs::<mlua::Value, mlua::Value>() {
        let (k, v) = pair?;
        let cv = conv(&v)?;
        match &k {
            mlua::Value::Integer(i) if lua_i64(*i) >= 1 && lua_i64(*i) <= len => {
                entries.push((lua_i64(*i), cv))
            }
            _ => {
                is_seq = false;
                map.push((k, cv));
            }
        }
    }
    if is_seq {
        entries.sort_by_key(|(i, _)| *i);
        Ok(LuaTable::Array(
            entries.into_iter().map(|(_, v)| v).collect(),
        ))
    } else {
        // Re-emit the integer-keyed entries we provisionally treated as sequence.
        for (i, v) in entries {
            map.push((mlua::Value::Integer(lua_int(i)), v));
        }
        Ok(LuaTable::Map(map))
    }
}

/// Convert an `mlua::Value` to an RPC [`rmpv::Value`] for `nvim_exec_lua`. A
/// table with contiguous `1..=n` integer keys becomes an array (a Lua sequence);
/// any other table becomes a map; an empty table becomes an empty array.
/// Functions / userdata / threads (not representable over msgpack) collapse to
/// nil. Covers the scalar-and-table shapes nxvim's synchronous getters return.
pub(crate) fn lua_to_rmpv(value: &mlua::Value) -> mlua::Result<rmpv::Value> {
    lua_to_rmpv_at(value, 0)
}

fn lua_to_rmpv_at(value: &mlua::Value, depth: usize) -> mlua::Result<rmpv::Value> {
    use mlua::Value as L;
    if depth > MAX_DEPTH {
        return Err(too_deep());
    }
    Ok(match value {
        L::Nil => rmpv::Value::Nil,
        L::Boolean(b) => rmpv::Value::from(*b),
        L::Integer(i) => rmpv::Value::from(*i),
        L::Number(n) => rmpv::Value::from(*n),
        L::String(s) => rmpv::Value::from(s.to_str()?.to_string()),
        L::Table(t) => match classify_table(t, |v| lua_to_rmpv_at(v, depth + 1))? {
            LuaTable::Array(items) => rmpv::Value::Array(items),
            LuaTable::Map(pairs) => {
                let mut map = Vec::with_capacity(pairs.len());
                for (k, v) in pairs {
                    map.push((lua_to_rmpv_at(&k, depth + 1)?, v));
                }
                rmpv::Value::Map(map)
            }
        },
        // Non-serializable Lua values have no msgpack representation.
        _ => rmpv::Value::Nil,
    })
}

/// Convert an RPC [`rmpv::Value`] into the equivalent `mlua::Value` — the inverse
/// of [`lua_to_rmpv`], for handing server-built msgpack data (e.g. the
/// `vim.fn.undotree()` projection) to Lua. Maps become string-keyed tables (a
/// non-string key is stringified); arrays and integer/float/bool/string/nil map
/// directly; binary blobs become Lua strings.
pub(crate) fn rmpv_to_lua(lua: &Lua, value: &rmpv::Value) -> mlua::Result<mlua::Value> {
    rmpv_to_lua_at(lua, value, 0)
}

fn rmpv_to_lua_at(lua: &Lua, value: &rmpv::Value, depth: usize) -> mlua::Result<mlua::Value> {
    use rmpv::Value as R;
    if depth > MAX_DEPTH {
        return Err(too_deep());
    }
    Ok(match value {
        R::Nil => mlua::Value::Nil,
        R::Boolean(b) => mlua::Value::Boolean(*b),
        R::Integer(i) => match i.as_i64() {
            Some(n) => mlua::Value::Integer(lua_int(n)),
            None => mlua::Value::Number(i.as_f64().unwrap_or(0.0)),
        },
        R::F32(n) => mlua::Value::Number(*n as f64),
        R::F64(n) => mlua::Value::Number(*n),
        R::String(s) => mlua::Value::String(lua.create_string(s.as_bytes())?),
        R::Binary(b) => mlua::Value::String(lua.create_string(b)?),
        R::Array(items) => {
            let t = lua.create_table_with_capacity(items.len(), 0)?;
            for (i, item) in items.iter().enumerate() {
                t.raw_set(i + 1, rmpv_to_lua_at(lua, item, depth + 1)?)?;
            }
            mlua::Value::Table(t)
        }
        R::Map(pairs) => {
            let t = lua.create_table_with_capacity(0, pairs.len())?;
            for (k, v) in pairs {
                let key = match k {
                    R::String(s) => s.as_str().unwrap_or_default().to_string(),
                    other => other.to_string(),
                };
                t.raw_set(key, rmpv_to_lua_at(lua, v, depth + 1)?)?;
            }
            mlua::Value::Table(t)
        }
        R::Ext(_, _) => mlua::Value::Nil,
    })
}

/// Convert a parsed [`serde_json::Value`] into the equivalent `mlua::Value` for
/// `vim.json.decode`: objects become string-keyed tables, arrays become Lua
/// sequences, and JSON `null` becomes `nil` (so a null-valued object key reads
/// back absent — fine for the `cargo metadata` shape the `lsp/<server>.lua`
/// configs decode, which only index present string/array fields).
pub(crate) fn json_to_lua(lua: &Lua, value: &serde_json::Value) -> mlua::Result<mlua::Value> {
    json_to_lua_at(lua, value, 0)
}

fn json_to_lua_at(lua: &Lua, value: &serde_json::Value, depth: usize) -> mlua::Result<mlua::Value> {
    use serde_json::Value as J;
    if depth > MAX_DEPTH {
        return Err(too_deep());
    }
    Ok(match value {
        J::Null => mlua::Value::Nil,
        J::Bool(b) => mlua::Value::Boolean(*b),
        J::Number(n) => match n.as_i64() {
            Some(i) => mlua::Value::Integer(lua_int(i)),
            None => mlua::Value::Number(n.as_f64().unwrap_or(0.0)),
        },
        J::String(s) => mlua::Value::String(lua.create_string(s)?),
        J::Array(items) => {
            let t = lua.create_table_with_capacity(items.len(), 0)?;
            for (i, item) in items.iter().enumerate() {
                t.raw_set(i + 1, json_to_lua_at(lua, item, depth + 1)?)?;
            }
            mlua::Value::Table(t)
        }
        J::Object(map) => {
            let t = lua.create_table_with_capacity(0, map.len())?;
            for (k, v) in map {
                t.raw_set(k.as_str(), json_to_lua_at(lua, v, depth + 1)?)?;
            }
            mlua::Value::Table(t)
        }
    })
}

/// Convert an optional Lua config table (`init_options` / `settings` /
/// `capabilities` from `nx._lsp_start`) to JSON for `LspOp::Start`. `None`
/// passes through; a present table goes through [`lua_to_json`] (the same bridge
/// `vim.json.encode` uses), so what the config wrote reaches the server verbatim.
pub(crate) fn opt_table_to_json(t: Option<Table>) -> mlua::Result<Option<serde_json::Value>> {
    match t {
        Some(t) => Ok(Some(lua_to_json(&mlua::Value::Table(t))?)),
        None => Ok(None),
    }
}

/// Flatten an `nx.run`-family `spec.env` table (`{ VAR = value }`) into the
/// `(key, value)` pairs the event-loop actor layers onto the child's inherited
/// environment — shared by the `nx._system_async` bridge and its stream /
/// process / local siblings. An absent table yields no pairs.
pub(crate) fn env_pairs(env: Option<Table>) -> mlua::Result<Vec<(String, String)>> {
    let Some(env) = env else {
        return Ok(Vec::new());
    };
    let mut pairs = Vec::new();
    for kv in env.pairs::<String, String>() {
        pairs.push(kv?);
    }
    Ok(pairs)
}

/// Convert an `mlua::Value` to a [`serde_json::Value`] for `vim.json.encode`,
/// using the same array-vs-object rule as [`lua_to_rmpv`]: a table whose keys are
/// exactly `1..=len` is an array, anything else an object (keys coerced to
/// strings); non-serializable values (functions / userdata) collapse to `null`.
pub(crate) fn lua_to_json(value: &mlua::Value) -> mlua::Result<serde_json::Value> {
    lua_to_json_at(value, 0)
}

fn lua_to_json_at(value: &mlua::Value, depth: usize) -> mlua::Result<serde_json::Value> {
    use mlua::Value as L;
    if depth > MAX_DEPTH {
        return Err(too_deep());
    }
    Ok(match value {
        L::Nil => serde_json::Value::Null,
        L::Boolean(b) => serde_json::Value::Bool(*b),
        L::Integer(i) => serde_json::Value::from(*i),
        L::Number(n) => serde_json::Value::from(*n),
        L::String(s) => serde_json::Value::from(s.to_str()?.to_string()),
        L::Table(t) => match json_mark(value).as_deref() {
            Some("null") => serde_json::Value::Null,
            // Marked an object, so it stays one however it is filled in afterwards —
            // the mark is the answer to "array or object?", not a stand-in for the
            // table's contents. Dropping the entries here would silently lose whatever
            // a caller added to it.
            Some("object") => {
                let mut map = serde_json::Map::new();
                for pair in t.clone().pairs::<mlua::Value, mlua::Value>() {
                    let (k, v) = pair?;
                    map.insert(json_key(&k)?, lua_to_json_at(&v, depth + 1)?);
                }
                serde_json::Value::Object(map)
            }
            _ => match classify_table(t, |v| lua_to_json_at(v, depth + 1))? {
                LuaTable::Array(items) => serde_json::Value::Array(items),
                LuaTable::Map(pairs) => {
                    let mut map = serde_json::Map::new();
                    for (k, v) in pairs {
                        map.insert(json_key(&k)?, v);
                    }
                    serde_json::Value::Object(map)
                }
            },
        },
        _ => serde_json::Value::Null,
    })
}

/// Coerce a Lua table key to the JSON object key string `vim.json.encode` uses.
fn json_key(k: &mlua::Value) -> mlua::Result<String> {
    Ok(match k {
        mlua::Value::String(s) => s.to_str()?.to_string(),
        mlua::Value::Integer(i) => i.to_string(),
        mlua::Value::Number(n) => n.to_string(),
        _ => {
            return Err(mlua::Error::external(
                "vim.json.encode: table key is not a string or number",
            ))
        }
    })
}
