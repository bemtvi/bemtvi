//! The Lua↔Rust value bridges. `mlua::Value` flows out to `rmpv` (the
//! `nvim_exec_lua` return path) and to `serde_json` (`vim.json.encode`, LSP op
//! payloads); JSON flows back in (`vim.json.decode`, LSP replies). The two
//! outbound table walkers share one classifier ([`classify_table`]) so the
//! array-vs-map rule lives in a single place. Also the small opts-table readers
//! the `nvim_set_hl` / `vim.system` bridges lean on.

use mlua::{Lua, Table};

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

/// Stringify a Lua value for `print` capture: prefer Lua's own `tostring`
/// (honors `__tostring`), fall back to a debug form.
pub(crate) fn stringify(lua: &Lua, value: &mlua::Value) -> String {
    match lua.coerce_string(value.clone()) {
        Ok(Some(s)) => s.to_str().map(|s| s.to_string()).unwrap_or_default(),
        _ => format!("{value:?}"),
    }
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
    let len = t.raw_len() as i64;
    let mut entries: Vec<(i64, V)> = Vec::new();
    let mut map: Vec<(mlua::Value, V)> = Vec::new();
    let mut is_seq = true;
    for pair in t.clone().pairs::<mlua::Value, mlua::Value>() {
        let (k, v) = pair?;
        let cv = conv(&v)?;
        match &k {
            mlua::Value::Integer(i) if *i >= 1 && *i <= len => entries.push((*i, cv)),
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
            map.push((mlua::Value::Integer(i), v));
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
    use mlua::Value as L;
    Ok(match value {
        L::Nil => rmpv::Value::Nil,
        L::Boolean(b) => rmpv::Value::from(*b),
        L::Integer(i) => rmpv::Value::from(*i),
        L::Number(n) => rmpv::Value::from(*n),
        L::String(s) => rmpv::Value::from(s.to_str()?.to_string()),
        L::Table(t) => match classify_table(t, lua_to_rmpv)? {
            LuaTable::Array(items) => rmpv::Value::Array(items),
            LuaTable::Map(pairs) => {
                let mut map = Vec::with_capacity(pairs.len());
                for (k, v) in pairs {
                    map.push((lua_to_rmpv(&k)?, v));
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
    use rmpv::Value as R;
    Ok(match value {
        R::Nil => mlua::Value::Nil,
        R::Boolean(b) => mlua::Value::Boolean(*b),
        R::Integer(i) => match i.as_i64() {
            Some(n) => mlua::Value::Integer(n),
            None => mlua::Value::Number(i.as_f64().unwrap_or(0.0)),
        },
        R::F32(n) => mlua::Value::Number(*n as f64),
        R::F64(n) => mlua::Value::Number(*n),
        R::String(s) => mlua::Value::String(lua.create_string(s.as_bytes())?),
        R::Binary(b) => mlua::Value::String(lua.create_string(b)?),
        R::Array(items) => {
            let t = lua.create_table()?;
            for (i, item) in items.iter().enumerate() {
                t.raw_set(i + 1, rmpv_to_lua(lua, item)?)?;
            }
            mlua::Value::Table(t)
        }
        R::Map(pairs) => {
            let t = lua.create_table()?;
            for (k, v) in pairs {
                let key = match k {
                    R::String(s) => s.as_str().unwrap_or_default().to_string(),
                    other => other.to_string(),
                };
                t.raw_set(key, rmpv_to_lua(lua, v)?)?;
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
    use serde_json::Value as J;
    Ok(match value {
        J::Null => mlua::Value::Nil,
        J::Bool(b) => mlua::Value::Boolean(*b),
        J::Number(n) => match n.as_i64() {
            Some(i) => mlua::Value::Integer(i),
            None => mlua::Value::Number(n.as_f64().unwrap_or(0.0)),
        },
        J::String(s) => mlua::Value::String(lua.create_string(s)?),
        J::Array(items) => {
            let t = lua.create_table()?;
            for (i, item) in items.iter().enumerate() {
                t.raw_set(i + 1, json_to_lua(lua, item)?)?;
            }
            mlua::Value::Table(t)
        }
        J::Object(map) => {
            let t = lua.create_table()?;
            for (k, v) in map {
                t.raw_set(k.as_str(), json_to_lua(lua, v)?)?;
            }
            mlua::Value::Table(t)
        }
    })
}

/// Convert an optional Lua config table (`init_options` / `settings` /
/// `capabilities` from `vim._lsp_start`) to JSON for `LspOp::Start`. `None`
/// passes through; a present table goes through [`lua_to_json`] (the same bridge
/// `vim.json.encode` uses), so what the config wrote reaches the server verbatim.
pub(crate) fn opt_table_to_json(t: Option<Table>) -> mlua::Result<Option<serde_json::Value>> {
    match t {
        Some(t) => Ok(Some(lua_to_json(&mlua::Value::Table(t))?)),
        None => Ok(None),
    }
}

/// Flatten a `vim.system` `opts.env` table (`{ VAR = value }`) into the
/// `(key, value)` pairs the event-loop actor layers onto the child's inherited
/// environment — the async `vim._system_async` analogue of the inline loop in the
/// blocking `vim._system`. An absent table yields no pairs.
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
    use mlua::Value as L;
    Ok(match value {
        L::Nil => serde_json::Value::Null,
        L::Boolean(b) => serde_json::Value::Bool(*b),
        L::Integer(i) => serde_json::Value::from(*i),
        L::Number(n) => serde_json::Value::from(*n),
        L::String(s) => serde_json::Value::from(s.to_str()?.to_string()),
        L::Table(t) => match classify_table(t, lua_to_json)? {
            LuaTable::Array(items) => serde_json::Value::Array(items),
            LuaTable::Map(pairs) => {
                let mut map = serde_json::Map::new();
                for (k, v) in pairs {
                    map.insert(json_key(&k)?, v);
                }
                serde_json::Value::Object(map)
            }
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
