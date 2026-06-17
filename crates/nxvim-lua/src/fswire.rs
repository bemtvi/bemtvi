//! The `nx.fs` off-tick op wire codec — [`FsJob`] / [`FsValue`] / [`FsError`] ⟷
//! [`rmpv::Value`]. Used **only** by the wasm `luafs_op` leg (the browser edit-host
//! routing `nx.fs` ops to a remote daemon over WebTransport — Phase 2 of
//! `docs/plans/2026-06-16-nx-fs-off-tick-daemon-leg.md`):
//!
//! - the **daemon** ([`nxvim_server`]'s `run_daemon_io`) decodes the request map into an
//!   [`FsJob`] ([`fs_job_from_value`]), runs it through [`run_fs_job`](crate::run_fs_job),
//!   and encodes the typed result back ([`fs_result_to_value`]);
//! - the **wasm edit-host** ([`nxvim_edithost`]) decodes that reply into the
//!   `Result<FsValue, FsError>` it marshals into the resolved / rejected Lua value
//!   ([`fs_result_from_value`]).
//!
//! Native-**bare** never touches this — it runs [`FsJob`]s on the event-loop actor
//! against a local `StdLuaFs` directly. Native-**daemon** and **wasm** both encode the
//! job here ([`fs_job_to_value`]) and send it over the same `luafs_op` leg, and both
//! decode the reply here ([`fs_result_from_value`]) — so all the encode/decode ends live
//! together, in lock-step.
//!
//! The codec works on [`rmpv::Value`] only (no `mlua` / transport types) so it is shared
//! across the native daemon and the wasm edit-host without dragging either's machinery in.
//! Bytes ride as msgpack `bin` (`Value::Binary`) so `nx.fs.read`'s raw content and
//! `nx.fs.write`'s payload cross intact (no UTF-8 mangling).
//!
//! ### Wire shapes
//!
//! **Request** (edit-host → daemon) — a map keyed by string, one `op` field plus the op's
//! arguments: `{ op = "readdir", path = "…" }`, `{ op = "write", path, data = <bin> }`,
//! `{ op = "read_text", path, encoding = "utf-8" }`, `{ op = "copy", src, dst,
//! recursive = true }`, … The map form (not positional) keeps the JS Worker a near-dumb
//! pipe: it forwards the same object it drained from the edit-host, converting only `data`
//! to a byte buffer.
//!
//! **Reply** (daemon → edit-host) — `["ok", <fs-value>]` on success, `["err", code,
//! message]` on a reject. The `<fs-value>` is itself a tagged array (`["nil"]` /
//! `["bool", b]` / `["bytes", <bin>]` / `["text", s]` / `["stat", […]]` /
//! `["dir", [[kind, name], …]]`) — the union of every `nx.fs` op's success payload.

use crate::luafs::{FileKind, LuaDirEntry, LuaStat};
use crate::ops::{FsError, FsJob, FsValue};
use rmpv::Value;

/// Decode an [`FsJob`] request map (the daemon side). Returns the op name in the error so a
/// malformed / unknown request fails loud rather than silently mapping to some default op.
pub fn fs_job_from_value(v: &Value) -> Result<FsJob, String> {
    let get = |key: &str| map_get(v, key);
    let op = get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| "luafs_op: request has no 'op' field".to_string())?;
    let str_field = |key: &str| -> Result<String, String> {
        get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("luafs_op: op '{op}' missing string field '{key}'"))
    };
    let bool_field = |key: &str| get(key).and_then(Value::as_bool).unwrap_or(false);
    // Unix permission bits for `mkdir`; default 0o755 when absent (older peers).
    let u32_field = |key: &str, default: u32| {
        get(key)
            .and_then(Value::as_u64)
            .map(|n| n as u32)
            .unwrap_or(default)
    };
    let bytes_field = |key: &str| -> Result<Vec<u8>, String> {
        get(key)
            .map(value_to_bytes)
            .ok_or_else(|| format!("luafs_op: op '{op}' missing bytes field '{key}'"))
    };
    Ok(match op {
        "stat" => FsJob::Stat {
            path: str_field("path")?,
        },
        "lstat" => FsJob::Lstat {
            path: str_field("path")?,
        },
        "exists" => FsJob::Exists {
            path: str_field("path")?,
        },
        "readdir" => FsJob::Readdir {
            path: str_field("path")?,
        },
        "read" => FsJob::Read {
            path: str_field("path")?,
        },
        "read_text" => FsJob::ReadText {
            path: str_field("path")?,
            // The wrapper always sends an explicit encoding; default to UTF-8 if absent.
            encoding: get("encoding")
                .and_then(Value::as_str)
                .unwrap_or("utf-8")
                .to_string(),
        },
        "write" => FsJob::Write {
            path: str_field("path")?,
            data: bytes_field("data")?,
        },
        "append" => FsJob::Append {
            path: str_field("path")?,
            data: bytes_field("data")?,
        },
        "mkdir" => FsJob::Mkdir {
            path: str_field("path")?,
            recursive: bool_field("recursive"),
            mode: u32_field("mode", 0o755),
        },
        "rename" => FsJob::Rename {
            from: str_field("from")?,
            to: str_field("to")?,
        },
        "remove" => FsJob::Remove {
            path: str_field("path")?,
            recursive: bool_field("recursive"),
        },
        "copy" => FsJob::Copy {
            src: str_field("src")?,
            dst: str_field("dst")?,
            recursive: bool_field("recursive"),
        },
        "realpath" => FsJob::Realpath {
            path: str_field("path")?,
        },
        other => return Err(format!("luafs_op: unknown op '{other}'")),
    })
}

/// Encode an [`FsJob`] into its request map — the inverse of [`fs_job_from_value`], the
/// edit-host side. The native-daemon event-loop actor uses this to send a whole job over
/// the `luafs_op` leg in one round-trip (the daemon runs [`run_fs_job`](crate::run_fs_job)
/// and decomposes any compound op there); the wasm Worker builds the identical map shape
/// in JS. Bytes ride as msgpack `bin` so `write`/`append` payloads cross intact.
pub fn fs_job_to_value(job: &FsJob) -> Value {
    fn m(pairs: Vec<(&str, Value)>) -> Value {
        Value::Map(
            pairs
                .into_iter()
                .map(|(k, v)| (Value::from(k), v))
                .collect(),
        )
    }
    match job {
        FsJob::Stat { path } => m(vec![("op", "stat".into()), ("path", path.as_str().into())]),
        FsJob::Lstat { path } => m(vec![("op", "lstat".into()), ("path", path.as_str().into())]),
        FsJob::Exists { path } => m(vec![
            ("op", "exists".into()),
            ("path", path.as_str().into()),
        ]),
        FsJob::Readdir { path } => m(vec![
            ("op", "readdir".into()),
            ("path", path.as_str().into()),
        ]),
        FsJob::Read { path } => m(vec![("op", "read".into()), ("path", path.as_str().into())]),
        FsJob::ReadText { path, encoding } => m(vec![
            ("op", "read_text".into()),
            ("path", path.as_str().into()),
            ("encoding", encoding.as_str().into()),
        ]),
        FsJob::Write { path, data } => m(vec![
            ("op", "write".into()),
            ("path", path.as_str().into()),
            ("data", Value::Binary(data.clone())),
        ]),
        FsJob::Append { path, data } => m(vec![
            ("op", "append".into()),
            ("path", path.as_str().into()),
            ("data", Value::Binary(data.clone())),
        ]),
        FsJob::Mkdir {
            path,
            recursive,
            mode,
        } => m(vec![
            ("op", "mkdir".into()),
            ("path", path.as_str().into()),
            ("recursive", Value::from(*recursive)),
            ("mode", Value::from(*mode)),
        ]),
        FsJob::Rename { from, to } => m(vec![
            ("op", "rename".into()),
            ("from", from.as_str().into()),
            ("to", to.as_str().into()),
        ]),
        FsJob::Remove { path, recursive } => m(vec![
            ("op", "remove".into()),
            ("path", path.as_str().into()),
            ("recursive", Value::from(*recursive)),
        ]),
        FsJob::Copy {
            src,
            dst,
            recursive,
        } => m(vec![
            ("op", "copy".into()),
            ("src", src.as_str().into()),
            ("dst", dst.as_str().into()),
            ("recursive", Value::from(*recursive)),
        ]),
        FsJob::Realpath { path } => m(vec![
            ("op", "realpath".into()),
            ("path", path.as_str().into()),
        ]),
    }
}

/// Encode an op's outcome into the `["ok", <fs-value>] | ["err", code, message]` reply
/// envelope (the daemon side).
pub fn fs_result_to_value(result: &Result<FsValue, FsError>) -> Value {
    match result {
        Ok(value) => Value::Array(vec![Value::from("ok"), fs_value_to_value(value)]),
        Err(e) => Value::Array(vec![
            Value::from("err"),
            Value::from(e.code.as_str()),
            Value::from(e.message.as_str()),
        ]),
    }
}

/// Decode the `["ok", …] | ["err", …]` reply envelope back into the typed outcome (the
/// wasm edit-host side). A reply that doesn't match either shape is itself a (loud)
/// [`FsError`] — never a silent success.
pub fn fs_result_from_value(v: &Value) -> Result<FsValue, FsError> {
    let arr = v
        .as_array()
        .ok_or_else(|| wire_error("reply is not an array"))?;
    match arr.first().and_then(Value::as_str) {
        Some("ok") => {
            let payload = arr
                .get(1)
                .ok_or_else(|| wire_error("ok reply has no value"))?;
            fs_value_from_value(payload)
        }
        Some("err") => Err(FsError {
            code: arr
                .get(1)
                .and_then(Value::as_str)
                .unwrap_or("EIO")
                .to_string(),
            message: arr
                .get(2)
                .and_then(Value::as_str)
                .unwrap_or("luafs_op error")
                .to_string(),
        }),
        _ => Err(wire_error("reply tag is neither 'ok' nor 'err'")),
    }
}

/// Encode an [`FsValue`] into its tagged-array wire form.
fn fs_value_to_value(value: &FsValue) -> Value {
    match value {
        FsValue::Nil => Value::Array(vec![Value::from("nil")]),
        FsValue::Bool(b) => Value::Array(vec![Value::from("bool"), Value::from(*b)]),
        FsValue::Bytes(bytes) => {
            Value::Array(vec![Value::from("bytes"), Value::Binary(bytes.clone())])
        }
        FsValue::Text(s) => Value::Array(vec![Value::from("text"), Value::from(s.as_str())]),
        FsValue::Stat(st) => Value::Array(vec![Value::from("stat"), encode_stat(st)]),
        FsValue::Dir(entries) => {
            let rows = entries
                .iter()
                .map(|e| {
                    Value::Array(vec![
                        Value::from(e.kind.as_str()),
                        Value::from(e.name.as_str()),
                    ])
                })
                .collect();
            Value::Array(vec![Value::from("dir"), Value::Array(rows)])
        }
    }
}

/// Decode a tagged-array [`FsValue`]. An unknown / malformed tag is a loud [`FsError`].
fn fs_value_from_value(v: &Value) -> Result<FsValue, FsError> {
    let arr = v
        .as_array()
        .ok_or_else(|| wire_error("fs value is not an array"))?;
    let tag = arr
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| wire_error("fs value has no tag"))?;
    let payload = arr.get(1);
    Ok(match tag {
        "nil" => FsValue::Nil,
        "bool" => FsValue::Bool(payload.and_then(Value::as_bool).unwrap_or(false)),
        "bytes" => FsValue::Bytes(payload.map(value_to_bytes).unwrap_or_default()),
        "text" => FsValue::Text(
            payload
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        "stat" => FsValue::Stat(
            payload
                .and_then(decode_stat)
                .ok_or_else(|| wire_error("malformed stat payload"))?,
        ),
        "dir" => FsValue::Dir(decode_dir(payload)),
        other => return Err(wire_error(&format!("unknown fs value tag '{other}'"))),
    })
}

/// Encode a [`LuaStat`] as a positional array — kind string then the numeric fields, with
/// the optional `(secs, nsecs)` times flattened as `secs` (or nil) + `nsecs`.
fn encode_stat(st: &LuaStat) -> Value {
    let (mtime_s, mtime_ns) = st
        .mtime
        .map_or((Value::Nil, 0), |(s, n)| (Value::from(s), n));
    let (atime_s, atime_ns) = st
        .atime
        .map_or((Value::Nil, 0), |(s, n)| (Value::from(s), n));
    Value::Array(vec![
        Value::from(st.kind.as_str()),
        Value::from(st.size),
        Value::from(st.mode),
        mtime_s,
        Value::from(mtime_ns),
        atime_s,
        Value::from(atime_ns),
        Value::from(st.ino),
        Value::from(st.uid),
        Value::from(st.gid),
        Value::from(st.nlink),
        Value::from(st.dev),
    ])
}

/// Decode a positional [`LuaStat`] array (the inverse of [`encode_stat`]). `None` on a
/// shape too short to read the kind.
fn decode_stat(v: &Value) -> Option<LuaStat> {
    let a = v.as_array()?;
    let kind = FileKind::from_wire(a.first()?.as_str()?);
    let u64_at = |i: usize| a.get(i).and_then(Value::as_u64).unwrap_or(0);
    let u32_at = |i: usize| a.get(i).and_then(Value::as_u64).unwrap_or(0) as u32;
    let opt_time = |si: usize, ni: usize| -> Option<(i64, u32)> {
        a.get(si)
            .and_then(Value::as_i64)
            .map(|s| (s, a.get(ni).and_then(Value::as_u64).unwrap_or(0) as u32))
    };
    Some(LuaStat {
        kind,
        size: u64_at(1),
        mode: u32_at(2),
        mtime: opt_time(3, 4),
        atime: opt_time(5, 6),
        ino: u64_at(7),
        uid: u32_at(8),
        gid: u32_at(9),
        nlink: u64_at(10),
        dev: u64_at(11),
    })
}

/// Decode the `[[kind, name], …]` directory listing (skipping any malformed row rather
/// than failing the whole listing).
fn decode_dir(v: Option<&Value>) -> Vec<LuaDirEntry> {
    v.and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let e = row.as_array()?;
                    Some(LuaDirEntry {
                        kind: FileKind::from_wire(e.first()?.as_str().unwrap_or("file")),
                        name: e.get(1)?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Look up `key` in a `Value::Map` keyed by string (the request map). `None` if `v` isn't
/// a map or the key is absent.
fn map_get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, val)| val)
}

/// Bytes out of a wire value: a msgpack `bin` (the faithful form), or — defensively — an
/// array of integers (the JSON-byte-array form a transport might send) or a string.
fn value_to_bytes(v: &Value) -> Vec<u8> {
    match v {
        Value::Binary(b) => b.clone(),
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Array(items) => items
            .iter()
            .map(|n| n.as_u64().unwrap_or(0) as u8)
            .collect(),
        _ => Vec::new(),
    }
}

/// A malformed-wire [`FsError`] (the reply didn't match the protocol). `EWIRE` isn't a
/// libuv errno — it marks a transport/codec fault distinct from a real fs error code.
fn wire_error(detail: &str) -> FsError {
    FsError {
        code: "EWIRE".to_string(),
        message: format!("luafs_op: {detail}"),
    }
}
