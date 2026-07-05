//! The `nx.http.fetch` off-tick request wire codec — [`HttpRequest`] / [`HttpResponse`]
//! / [`HttpError`] ⟷ [`rmpv::Value`]. The HTTP sibling of [`fswire`](crate::fswire),
//! used by the `http_op` leg a **native-daemon** or **wasm** session runs `nx.http`
//! requests over (the browser / remote edit-host routing a fetch to the daemon, which
//! owns the network and dodges CORS):
//!
//! - the **daemon** ([`nxvim_server`]) decodes the request map into an [`HttpRequest`]
//!   ([`http_request_from_value`]), runs the round-trip, and encodes the typed result
//!   back ([`http_result_to_value`]);
//! - the **edit-host** ([`nxvim_edithost`] / the native-daemon actor) decodes that reply
//!   into the `Result<HttpResponse, HttpError>` it marshals into the resolved / rejected
//!   Lua value ([`http_result_from_value`]).
//!
//! Native-**bare** never touches this — it runs the round-trip on the event-loop actor
//! with a local `ureq` directly. A **serverless** wasm session doesn't either — it has no
//! daemon, so it uses the browser's own `fetch()` (its natural HTTP client). Only the
//! native-daemon and wasm-with-daemon paths encode the request here and decode the reply
//! here, in lock-step.
//!
//! The codec works on [`rmpv::Value`] only (no `mlua` / transport / `ureq` types) so it is
//! shared across the native daemon and the wasm edit-host, and stays wasm-safe (no
//! blocking HTTP client dragged into `nxvim-lua`). The request/response bodies ride as
//! msgpack `bin` (`Value::Binary`) so raw bytes cross intact.
//!
//! ### Wire shapes
//!
//! **Request** (edit-host → daemon) — a map: `{ method = "GET", url = "…", headers =
//! [[name, value], …], body = <bin>, timeout_ms = <int|nil> }`.
//!
//! **Reply** (daemon → edit-host) — `["ok", { status, status_text, headers = [[name,
//! value], …], body = <bin> }]` on success, `["err", message]` on a transport failure.

use crate::ops::{HttpError, HttpRequest, HttpResponse};
use rmpv::Value;

/// Encode an [`HttpRequest`] into its request map (the edit-host side).
pub fn http_request_to_value(req: &HttpRequest) -> Value {
    let headers = Value::Array(
        req.headers
            .iter()
            .map(|(k, v)| Value::Array(vec![Value::from(k.as_str()), Value::from(v.as_str())]))
            .collect(),
    );
    let mut map = vec![
        (Value::from("method"), Value::from(req.method.as_str())),
        (Value::from("url"), Value::from(req.url.as_str())),
        (Value::from("headers"), headers),
        (Value::from("body"), Value::Binary(req.body.clone())),
        (Value::from("redirect"), Value::from(req.redirect.as_str())),
    ];
    if let Some(ms) = req.timeout_ms {
        map.push((Value::from("timeout_ms"), Value::from(ms)));
    }
    if let Some(n) = req.max_redirects {
        map.push((Value::from("max_redirects"), Value::from(n)));
    }
    Value::Map(map)
}

/// Decode an [`HttpRequest`] request map (the daemon side). A missing `url` fails loud
/// (`method` defaults to `GET`; `headers`/`body` default empty) so a malformed request
/// is a loud error, never a silent empty fetch.
pub fn http_request_from_value(v: &Value) -> Result<HttpRequest, String> {
    let get = |key: &str| map_get(v, key);
    let url = get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| "http_op: request has no string 'url' field".to_string())?
        .to_string();
    let method = get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_string();
    let headers = get("headers")
        .and_then(Value::as_array)
        .map(|pairs| {
            pairs
                .iter()
                .filter_map(|pair| {
                    let pair = pair.as_array()?;
                    let name = pair.first()?.as_str()?.to_string();
                    let value = pair.get(1)?.as_str()?.to_string();
                    Some((name, value))
                })
                .collect()
        })
        .unwrap_or_default();
    let body = get("body").map(value_to_bytes).unwrap_or_default();
    let timeout_ms = get("timeout_ms").and_then(Value::as_u64);
    let redirect = get("redirect")
        .and_then(Value::as_str)
        .unwrap_or("follow")
        .to_string();
    let max_redirects = get("max_redirects")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    Ok(HttpRequest {
        method,
        url,
        headers,
        body,
        timeout_ms,
        redirect,
        max_redirects,
    })
}

/// Encode the typed outcome into the `["ok", …] | ["err", …]` reply envelope (the daemon
/// side).
pub fn http_result_to_value(result: &Result<HttpResponse, HttpError>) -> Value {
    match result {
        Ok(resp) => {
            let headers = Value::Array(
                resp.headers
                    .iter()
                    .map(|(k, v)| {
                        Value::Array(vec![Value::from(k.as_str()), Value::from(v.as_str())])
                    })
                    .collect(),
            );
            let map = Value::Map(vec![
                (Value::from("status"), Value::from(resp.status)),
                (
                    Value::from("status_text"),
                    Value::from(resp.status_text.as_str()),
                ),
                (Value::from("headers"), headers),
                (Value::from("body"), Value::Binary(resp.body.clone())),
            ]);
            Value::Array(vec![Value::from("ok"), map])
        }
        Err(e) => Value::Array(vec![Value::from("err"), Value::from(e.message.as_str())]),
    }
}

/// Decode the `["ok", …] | ["err", …]` reply envelope back into the typed outcome (the
/// edit-host side). A reply that doesn't match either shape is itself a (loud)
/// [`HttpError`] — never a silent success.
pub fn http_result_from_value(v: &Value) -> Result<HttpResponse, HttpError> {
    let arr = v
        .as_array()
        .ok_or_else(|| wire_error("reply is not an array"))?;
    match arr.first().and_then(Value::as_str) {
        Some("ok") => {
            let payload = arr
                .get(1)
                .ok_or_else(|| wire_error("ok reply has no response"))?;
            let status = map_get(payload, "status")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u16;
            let status_text = map_get(payload, "status_text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let headers = map_get(payload, "headers")
                .and_then(Value::as_array)
                .map(|pairs| {
                    pairs
                        .iter()
                        .filter_map(|pair| {
                            let pair = pair.as_array()?;
                            let name = pair.first()?.as_str()?.to_string();
                            let value = pair.get(1)?.as_str()?.to_string();
                            Some((name, value))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let body = map_get(payload, "body")
                .map(value_to_bytes)
                .unwrap_or_default();
            Ok(HttpResponse {
                status,
                status_text,
                headers,
                body,
            })
        }
        Some("err") => Err(HttpError {
            message: arr
                .get(1)
                .and_then(Value::as_str)
                .unwrap_or("nx.http: unknown transport error")
                .to_string(),
        }),
        _ => Err(wire_error("reply is neither ok nor err")),
    }
}

/// Look up `key` in a `Value::Map` keyed by string. `None` if `v` isn't a map or the key
/// is absent.
fn map_get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, val)| val)
}

/// Bytes out of a wire value: a msgpack `bin` (the faithful form), or — defensively — a
/// string or an array of integers (the JSON-byte-array form a transport might send).
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

/// A malformed-wire [`HttpError`] (the reply didn't match the protocol).
fn wire_error(detail: &str) -> HttpError {
    HttpError {
        message: format!("http_op: {detail}"),
    }
}
