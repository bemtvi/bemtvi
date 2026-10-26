//! The native `btv.http.fetch` runner — a blocking [`ureq`] round-trip mapped onto the
//! typed [`HttpRequest`] / [`HttpResponse`] the `btv.http` promise resolves with. Runs on
//! the event-loop actor's `spawn_blocking` pool (native-bare) or on the daemon
//! (native-daemon / wasm-with-daemon, over the `http_op` leg) — never on the editor tick.
//!
//! **Fetch semantics.** `fetch` resolves any HTTP status (the caller reads `response.ok`)
//! and rejects only on a network / transport failure. So the agent is built with
//! `http_status_as_error(false)` — a 4xx/5xx comes back as a *resolved* [`HttpResponse`] —
//! and every `ureq::Error` that remains becomes an [`HttpError`] (a promise reject).
//! `ureq` follows redirects by default, as `fetch` does.

use bemtvi_lua::{HttpError, HttpRequest, HttpResponse};
use std::io::Read;
use std::time::Duration;

/// The default redirect-follow cap when `redirect == "follow"` and the caller gives no
/// `max_redirects` (browsers cap at ~20; ureq's own default is 5 — we split the difference
/// at a generous-but-bounded 10).
const DEFAULT_REDIRECTS: u32 = 10;

/// Build a `ureq::Agent` honoring the request's redirect policy and timeout. `"manual"` /
/// `"error"` follow zero redirects (the 3xx response is returned as-is; `"error"` is turned
/// into a reject by the caller); `"follow"` follows up to `max_redirects` (default
/// [`DEFAULT_REDIRECTS`]), and running past the cap is a transport error.
fn agent_for(req: &HttpRequest) -> ureq::Agent {
    let follow = req.redirect != "manual" && req.redirect != "error";
    let redirects = if follow {
        req.max_redirects.unwrap_or(DEFAULT_REDIRECTS)
    } else {
        0
    };
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .allow_non_standard_methods(true)
        .max_redirects(redirects)
        .timeout_global(req.timeout_ms.map(Duration::from_millis))
        .build();
    ureq::Agent::new_with_config(config)
}

fn transport_error(e: impl std::fmt::Display) -> HttpError {
    HttpError {
        message: format!("btv.http: {e}"),
    }
}

/// Run one HTTP round-trip with `ureq`. Blocking — call only off the editor tick (the
/// actor's blocking pool / the daemon). Returns `Ok` for any HTTP status (including
/// 4xx/5xx, with `HttpResponse::status` carrying it), `Err` only on a transport failure
/// (or a redirect when `redirect == "error"`).
pub fn run_http_request(req: &HttpRequest) -> Result<HttpResponse, HttpError> {
    let mut request = ureq::http::Request::builder()
        .method(req.method.as_str())
        .uri(&req.url);
    for (name, value) in &req.headers {
        request = request.header(name, value);
    }
    let agent = agent_for(req);
    // An empty body sends none (a bare GET); otherwise the raw payload.
    let outcome = if req.body.is_empty() {
        agent.run(request.body(()).map_err(transport_error)?)
    } else {
        agent.run(request.body(&req.body).map_err(transport_error)?)
    };
    // A real network / transport fault (DNS, connect, TLS, timeout, bad URL) rejects; a
    // 4xx/5xx is a *resolved* response (the caller branches on `response.ok`).
    let resp = outcome.map_err(transport_error)?;
    // `redirect = "error"`: a 3xx that we didn't follow (redirects=0) rejects.
    if req.redirect == "error" && resp.status().is_redirection() {
        return Err(HttpError {
            message: format!(
                "btv.http: redirect ({}) not followed (redirect = \"error\")",
                resp.status().as_u16()
            ),
        });
    }
    response_from_ureq(resp)
}

/// Project a `ureq` response onto the typed [`HttpResponse`]. Reads status + headers
/// (owned copies) before consuming the response to drain its body; a body-read failure is
/// a transport error (a mid-stream disconnect), so it rejects loud rather than resolving a
/// truncated body.
fn response_from_ureq(resp: ureq::http::Response<ureq::Body>) -> Result<HttpResponse, HttpError> {
    let status = resp.status().as_u16();
    // HTTP/1.1 reason phrases are not kept by `ureq` (and HTTP/2 has none), so this is the
    // status code's canonical reason — what the browser reports for an h2 response too.
    let status_text = resp.status().canonical_reason().unwrap_or("").to_string();
    // `keys()` lists each header once (already lowercase, matching the browser `Headers`
    // casing the `fetch`-modeled API mirrors); pair it with its first value.
    let headers = resp
        .headers()
        .keys()
        .filter_map(|name| {
            resp.headers().get(name).map(|value| {
                (
                    name.as_str().to_string(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
        })
        .collect();
    let mut body = Vec::new();
    resp.into_body()
        .into_reader()
        .read_to_end(&mut body)
        .map_err(|e| HttpError {
            message: format!("btv.http: reading response body: {e}"),
        })?;
    Ok(HttpResponse {
        status,
        status_text,
        headers,
        body,
    })
}
