//! The `btv.http.fetch` off-tick request wire codec — [`HttpRequest`] / [`HttpResponse`]
//! / [`HttpError`] ⟷ [`rmpv::Value`]. The HTTP sibling of [`fswire`](crate::fswire),
//! used by the `http_op` leg a **native-daemon** or **wasm** session runs `btv.http`
//! requests over (the browser / remote edit-host routing a fetch to the daemon, which
//! owns the network and dodges CORS):
//!
//! - the **daemon** ([`bemtvi_server`]) decodes the request map into an [`HttpRequest`]
//!   ([`http_request_from_value`]), runs the round-trip, and encodes the typed result
//!   back ([`http_result_to_value`]);
//! - the **edit-host** ([`bemtvi_edithost`] / the native-daemon actor) decodes that reply
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
//! blocking HTTP client dragged into `bemtvi-lua`). The request/response bodies ride as
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
/// is a loud error, never a silent empty fetch. ABSENT optional fields keep their
/// defaults; a present-but-wrong-typed field fails loud — a mangled `body` must not
/// silently become an empty-body fetch.
pub fn http_request_from_value(v: &Value) -> Result<HttpRequest, String> {
    let get = |key: &str| map_get(v, key);
    let url = get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| "http_op: request has no string 'url' field".to_string())?
        .to_string();
    let method = match get("method") {
        None => "GET".to_string(),
        Some(v) => v
            .as_str()
            .ok_or_else(|| "http_op: 'method' is not a string".to_string())?
            .to_string(),
    };
    let headers = match get("headers") {
        None => vec![],
        Some(v) => v
            .as_array()
            .ok_or_else(|| "http_op: 'headers' is not an array".to_string())?
            .iter()
            .map(|pair| {
                let pair = pair
                    .as_array()
                    .ok_or_else(|| "http_op: a header row is not an array".to_string())?;
                let name = pair
                    .first()
                    .ok_or_else(|| "http_op: a header row has no name".to_string())?
                    .as_str()
                    .ok_or_else(|| "http_op: a header name is not a string".to_string())?
                    .to_string();
                let value = pair
                    .get(1)
                    .ok_or_else(|| "http_op: a header row has no value".to_string())?
                    .as_str()
                    .ok_or_else(|| "http_op: a header value is not a string".to_string())?
                    .to_string();
                Ok((name, value))
            })
            .collect::<Result<Vec<_>, String>>()?,
    };
    let body = match get("body") {
        None => vec![],
        Some(v) => value_to_bytes(v).map_err(|e| format!("http_op: {e}"))?,
    };
    let timeout_ms = match get("timeout_ms") {
        None => None,
        Some(v) => Some(
            v.as_u64()
                .ok_or_else(|| "http_op: 'timeout_ms' is not an integer".to_string())?,
        ),
    };
    let redirect = match get("redirect") {
        None => "follow".to_string(),
        Some(v) => v
            .as_str()
            .ok_or_else(|| "http_op: 'redirect' is not a string".to_string())?
            .to_string(),
    };
    let max_redirects = match get("max_redirects") {
        None => None,
        Some(v) => Some(
            u32::try_from(
                v.as_u64()
                    .ok_or_else(|| "http_op: 'max_redirects' is not an integer".to_string())?,
            )
            .map_err(|_| "http_op: 'max_redirects' exceeds the u32 range".to_string())?,
        ),
    };
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
            // Every field the daemon's encoder always emits is required; a missing or
            // wrong-typed one is a malformed reply, not a silent 0/""/empty — a
            // mangled body must not resolve as a 0-byte success.
            let status: u16 = map_get(payload, "status")
                .ok_or_else(|| wire_error("ok reply has no status"))?
                .as_u64()
                .ok_or_else(|| wire_error("status is not an integer"))?
                .try_into()
                .map_err(|_| wire_error("status exceeds the u16 range"))?;
            let status_text = map_get(payload, "status_text")
                .ok_or_else(|| wire_error("ok reply has no status_text"))?
                .as_str()
                .ok_or_else(|| wire_error("status_text is not a string"))?
                .to_string();
            let headers = map_get(payload, "headers")
                .ok_or_else(|| wire_error("ok reply has no headers"))?
                .as_array()
                .ok_or_else(|| wire_error("headers is not an array"))?
                .iter()
                .map(|pair| {
                    let pair = pair
                        .as_array()
                        .ok_or_else(|| wire_error("a header row is not an array"))?;
                    let name = pair
                        .first()
                        .ok_or_else(|| wire_error("a header row has no name"))?
                        .as_str()
                        .ok_or_else(|| wire_error("a header name is not a string"))?
                        .to_string();
                    let value = pair
                        .get(1)
                        .ok_or_else(|| wire_error("a header row has no value"))?
                        .as_str()
                        .ok_or_else(|| wire_error("a header value is not a string"))?
                        .to_string();
                    Ok((name, value))
                })
                .collect::<Result<Vec<_>, HttpError>>()?;
            let body = value_to_bytes(
                map_get(payload, "body").ok_or_else(|| wire_error("ok reply has no body"))?,
            )
            .map_err(|e| wire_error(&e))?;
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
                .unwrap_or("btv.http: unknown transport error")
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
/// Any other shape is a malformed wire, not empty bytes (see the call sites).
fn value_to_bytes(v: &Value) -> Result<Vec<u8>, String> {
    match v {
        Value::Binary(b) => Ok(b.clone()),
        Value::String(s) => Ok(s.as_bytes().to_vec()),
        Value::Array(items) => items
            .iter()
            .map(|n| {
                let b = n
                    .as_u64()
                    .ok_or_else(|| "byte-array element is not an integer".to_string())?;
                u8::try_from(b).map_err(|_| "byte-array element exceeds the u8 range".to_string())
            })
            .collect(),
        other => Err(format!("expected a byte string, got {other:?}")),
    }
}

/// A malformed-wire [`HttpError`] (the reply didn't match the protocol).
fn wire_error(detail: &str) -> HttpError {
    HttpError {
        message: format!("http_op: {detail}"),
    }
}
