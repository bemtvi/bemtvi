//! The `nx.git` off-tick op wire codec — [`GitJob`] / [`GitValue`] / [`GitError`] ⟷
//! [`rmpv::Value`]. The git twin of [`fswire`](crate::fswire), used by the `git_op`
//! leg (a native-daemon or web session routing `nx.git.*` ops to the daemon, where the
//! files — and the gix engine — live):
//!
//! - the **daemon** decodes the request map into a [`GitJob`] ([`git_job_from_value`]),
//!   runs it through `nxvim_git::run_git_job`, and encodes the typed result back
//!   ([`git_result_to_value`]);
//! - the **edit-host** (native-daemon actor / wasm) decodes that reply into the
//!   `Result<GitValue, GitError>` it marshals into the resolved / rejected Lua value
//!   ([`git_result_from_value`]).
//!
//! Native-**bare** never touches this — it runs [`GitJob`]s on the event-loop actor
//! against the local gix engine directly. The codec works on [`rmpv::Value`] only (no
//! `mlua` / transport / gix types) so it is shared across both ends in lock-step, and
//! `show`'s blob rides as msgpack `bin` (`Value::Binary`) so raw content crosses intact.
//!
//! ### Wire shapes
//!
//! **Request (read)** — `{ op = "discover", path }`, `{ op = "head", path }`, `{ op =
//! "show", file, rev }`, `{ op = "diff_file", path, file }`, `{ op = "status", path }`.
//! **Request (mutation)** — `{ op = "clone", url, dir, depth?, branch? }`, `{ op =
//! "checkout", dir, rev, detach }`, `{ op = "pull", dir }`, `{ op = "submodule_update",
//! dir, init, recursive }`.
//!
//! **Reply** — `["ok", <git-value>]` on success, `["err", code, message]` on a reject.
//! The `<git-value>` is a tagged array: `["nil"]` / `["cloned", dir]` / `["pull",
//! updated, sha]` / `["bytes", <bin>]` / `["discover", root, git_dir, prefix]` /
//! `["head", branch|nil, detached, sha]` / `["diff", added, changed, removed,
//! [[o_start,o_count,n_start,n_count], …]]` / `["status", dirty, [[path, index,
//! worktree], …]]`.

use crate::ops::{GitError, GitHunk, GitJob, GitStatusEntry, GitValue};
use rmpv::Value;

/// Decode a [`GitJob`] request map (the daemon side). The op name rides in the error so a
/// malformed / unknown request fails loud rather than silently mapping to a default op.
pub fn git_job_from_value(v: &Value) -> Result<GitJob, String> {
    let get = |key: &str| map_get(v, key);
    let op = get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| "git_op: request has no 'op' field".to_string())?;
    let str_field = |key: &str| -> Result<String, String> {
        get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("git_op: op '{op}' missing string field '{key}'"))
    };
    Ok(match op {
        "discover" => GitJob::Discover {
            path: str_field("path")?,
        },
        "head" => GitJob::Head {
            path: str_field("path")?,
        },
        "show" => GitJob::Show {
            file: str_field("file")?,
            rev: str_field("rev")?,
        },
        "diff_file" => GitJob::DiffFile {
            path: str_field("path")?,
            file: str_field("file")?,
        },
        "status" => GitJob::Status {
            path: str_field("path")?,
        },
        "clone" => GitJob::Clone {
            url: str_field("url")?,
            dir: str_field("dir")?,
            // `depth`/`branch` are optional; a missing key is None (full history /
            // remote-default branch), not an error.
            depth: get("depth").and_then(Value::as_u64).map(|d| d as u32),
            branch: get("branch").and_then(Value::as_str).map(str::to_string),
        },
        "checkout" => GitJob::Checkout {
            dir: str_field("dir")?,
            rev: str_field("rev")?,
            detach: get("detach").and_then(Value::as_bool).unwrap_or(false),
        },
        "pull" => GitJob::Pull {
            dir: str_field("dir")?,
        },
        "submodule_update" => GitJob::SubmoduleUpdate {
            dir: str_field("dir")?,
            init: get("init").and_then(Value::as_bool).unwrap_or(false),
            recursive: get("recursive").and_then(Value::as_bool).unwrap_or(false),
        },
        other => return Err(format!("git_op: unknown op '{other}'")),
    })
}

/// Encode a [`GitJob`] into its request map — the inverse of [`git_job_from_value`], the
/// edit-host side. The actor sends a whole job over the `git_op` leg in one round-trip;
/// the daemon runs `nxvim_git::run_git_job` there.
pub fn git_job_to_value(job: &GitJob) -> Value {
    fn m(pairs: Vec<(&str, Value)>) -> Value {
        Value::Map(
            pairs
                .into_iter()
                .map(|(k, v)| (Value::from(k), v))
                .collect(),
        )
    }
    match job {
        GitJob::Discover { path } => m(vec![
            ("op", "discover".into()),
            ("path", path.as_str().into()),
        ]),
        GitJob::Head { path } => m(vec![("op", "head".into()), ("path", path.as_str().into())]),
        GitJob::Show { file, rev } => m(vec![
            ("op", "show".into()),
            ("file", file.as_str().into()),
            ("rev", rev.as_str().into()),
        ]),
        GitJob::DiffFile { path, file } => m(vec![
            ("op", "diff_file".into()),
            ("path", path.as_str().into()),
            ("file", file.as_str().into()),
        ]),
        GitJob::Status { path } => m(vec![
            ("op", "status".into()),
            ("path", path.as_str().into()),
        ]),
        GitJob::Clone {
            url,
            dir,
            depth,
            branch,
        } => {
            let mut pairs = vec![
                ("op", "clone".into()),
                ("url", url.as_str().into()),
                ("dir", dir.as_str().into()),
            ];
            // Only send the optional keys when set, so the daemon side decodes them
            // back to None rather than a spurious 0 / empty string.
            if let Some(d) = depth {
                pairs.push(("depth", Value::from(*d)));
            }
            if let Some(b) = branch {
                pairs.push(("branch", b.as_str().into()));
            }
            m(pairs)
        }
        GitJob::Checkout { dir, rev, detach } => m(vec![
            ("op", "checkout".into()),
            ("dir", dir.as_str().into()),
            ("rev", rev.as_str().into()),
            ("detach", Value::from(*detach)),
        ]),
        GitJob::Pull { dir } => m(vec![("op", "pull".into()), ("dir", dir.as_str().into())]),
        GitJob::SubmoduleUpdate {
            dir,
            init,
            recursive,
        } => m(vec![
            ("op", "submodule_update".into()),
            ("dir", dir.as_str().into()),
            ("init", Value::from(*init)),
            ("recursive", Value::from(*recursive)),
        ]),
    }
}

/// Encode an op's outcome into the `["ok", <git-value>] | ["err", code, message]` reply
/// envelope (the daemon side).
pub fn git_result_to_value(result: &Result<GitValue, GitError>) -> Value {
    match result {
        Ok(value) => Value::Array(vec![Value::from("ok"), git_value_to_value(value)]),
        Err(e) => Value::Array(vec![
            Value::from("err"),
            Value::from(e.code.as_str()),
            Value::from(e.message.as_str()),
        ]),
    }
}

/// Decode the `["ok", …] | ["err", …]` reply envelope back into the typed outcome (the
/// edit-host side). A reply that doesn't match either shape is itself a loud [`GitError`].
pub fn git_result_from_value(v: &Value) -> Result<GitValue, GitError> {
    let arr = v
        .as_array()
        .ok_or_else(|| wire_error("reply is not an array"))?;
    match arr.first().and_then(Value::as_str) {
        Some("ok") => {
            let payload = arr
                .get(1)
                .ok_or_else(|| wire_error("ok reply has no value"))?;
            git_value_from_value(payload)
        }
        Some("err") => Err(GitError {
            code: arr
                .get(1)
                .and_then(Value::as_str)
                .unwrap_or("EGIT")
                .to_string(),
            message: arr
                .get(2)
                .and_then(Value::as_str)
                .unwrap_or("git_op error")
                .to_string(),
        }),
        _ => Err(wire_error("reply tag is neither 'ok' nor 'err'")),
    }
}

/// Encode a [`GitValue`] into its tagged-array wire form.
fn git_value_to_value(value: &GitValue) -> Value {
    match value {
        GitValue::Nil => Value::Array(vec![Value::from("nil")]),
        GitValue::Cloned(dir) => {
            Value::Array(vec![Value::from("cloned"), Value::from(dir.as_str())])
        }
        GitValue::Pull { updated, sha } => Value::Array(vec![
            Value::from("pull"),
            Value::from(*updated),
            Value::from(sha.as_str()),
        ]),
        GitValue::Bytes(bytes) => {
            Value::Array(vec![Value::from("bytes"), Value::Binary(bytes.clone())])
        }
        GitValue::Discover {
            root,
            git_dir,
            prefix,
        } => Value::Array(vec![
            Value::from("discover"),
            Value::from(root.as_str()),
            Value::from(git_dir.as_str()),
            Value::from(prefix.as_str()),
        ]),
        GitValue::Head {
            branch,
            detached,
            sha,
        } => Value::Array(vec![
            Value::from("head"),
            branch.as_deref().map_or(Value::Nil, Value::from),
            Value::from(*detached),
            Value::from(sha.as_str()),
        ]),
        GitValue::Diff {
            added,
            changed,
            removed,
            hunks,
        } => {
            let rows = hunks
                .iter()
                .map(|h| {
                    Value::Array(vec![
                        Value::from(h.old_start),
                        Value::from(h.old_count),
                        Value::from(h.new_start),
                        Value::from(h.new_count),
                    ])
                })
                .collect();
            Value::Array(vec![
                Value::from("diff"),
                Value::from(*added),
                Value::from(*changed),
                Value::from(*removed),
                Value::Array(rows),
            ])
        }
        GitValue::Status { dirty, entries } => {
            let rows = entries
                .iter()
                .map(|e| {
                    Value::Array(vec![
                        Value::from(e.path.as_str()),
                        Value::from(e.index.as_str()),
                        Value::from(e.worktree.as_str()),
                    ])
                })
                .collect();
            Value::Array(vec![
                Value::from("status"),
                Value::from(*dirty),
                Value::Array(rows),
            ])
        }
    }
}

/// Decode a tagged-array [`GitValue`]. An unknown / malformed tag is a loud [`GitError`].
fn git_value_from_value(v: &Value) -> Result<GitValue, GitError> {
    let arr = v
        .as_array()
        .ok_or_else(|| wire_error("git value is not an array"))?;
    let tag = arr
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| wire_error("git value has no tag"))?;
    let str_at = |i: usize| arr.get(i).and_then(Value::as_str).unwrap_or("").to_string();
    let u32_at = |i: usize| arr.get(i).and_then(Value::as_u64).unwrap_or(0) as u32;
    Ok(match tag {
        "nil" => GitValue::Nil,
        "cloned" => GitValue::Cloned(str_at(1)),
        "pull" => GitValue::Pull {
            updated: arr.get(1).and_then(Value::as_bool).unwrap_or(false),
            sha: str_at(2),
        },
        "bytes" => GitValue::Bytes(arr.get(1).map(value_to_bytes).unwrap_or_default()),
        "discover" => GitValue::Discover {
            root: str_at(1),
            git_dir: str_at(2),
            prefix: str_at(3),
        },
        "head" => GitValue::Head {
            // A msgpack nil branch decodes back to `None` (a detached HEAD).
            branch: arr.get(1).and_then(Value::as_str).map(str::to_string),
            detached: arr.get(2).and_then(Value::as_bool).unwrap_or(false),
            sha: str_at(3),
        },
        "diff" => GitValue::Diff {
            added: u32_at(1),
            changed: u32_at(2),
            removed: u32_at(3),
            hunks: decode_hunks(arr.get(4)),
        },
        "status" => GitValue::Status {
            dirty: arr.get(1).and_then(Value::as_bool).unwrap_or(false),
            entries: decode_status_entries(arr.get(2)),
        },
        other => return Err(wire_error(&format!("unknown git value tag '{other}'"))),
    })
}

/// Decode the `[[o_start, o_count, n_start, n_count], …]` hunk list (skipping a malformed
/// row rather than failing the whole diff).
fn decode_hunks(v: Option<&Value>) -> Vec<GitHunk> {
    v.and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let a = row.as_array()?;
                    let u32_at = |i: usize| a.get(i).and_then(Value::as_u64).unwrap_or(0) as u32;
                    Some(GitHunk {
                        old_start: u32_at(0),
                        old_count: u32_at(1),
                        new_start: u32_at(2),
                        new_count: u32_at(3),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Decode the `[[path, index, worktree], …]` status list (skipping a malformed row).
fn decode_status_entries(v: Option<&Value>) -> Vec<GitStatusEntry> {
    v.and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let a = row.as_array()?;
                    Some(GitStatusEntry {
                        path: a.first()?.as_str()?.to_string(),
                        index: a.get(1).and_then(Value::as_str).unwrap_or(" ").to_string(),
                        worktree: a.get(2).and_then(Value::as_str).unwrap_or(" ").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Look up `key` in a `Value::Map` keyed by string (the request map).
fn map_get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, val)| val)
}

/// Bytes out of a wire value: a msgpack `bin` (the faithful form), or — defensively — a
/// string / integer array.
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

/// A malformed-wire [`GitError`] (the reply didn't match the protocol). `EWIRE` marks a
/// transport/codec fault, distinct from a real git error code.
fn wire_error(detail: &str) -> GitError {
    GitError {
        code: "EWIRE".to_string(),
        message: format!("git_op: {detail}"),
    }
}
