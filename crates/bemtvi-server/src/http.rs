//! The native `btv.http.fetch` runner — a blocking [`ureq`] round-trip mapped onto the
//! typed [`HttpRequest`] / [`HttpResponse`] the `btv.http` promise resolves with. Runs on
//! the event-loop actor's `spawn_blocking` pool (native-bare) or on the daemon
//! (native-daemon / wasm-with-daemon, over the `http_op` leg) — never on the editor tick.
//!
//! **Fetch semantics.** `ureq` returns a non-2xx/3xx status as an `Err(Status)`, but
//! `fetch` resolves any HTTP status (the caller reads `response.ok`) and rejects only on a
//! network / transport failure. So a `Status` error is folded back into a *resolved*
//! [`HttpResponse`]; only a `Transport` error becomes an [`HttpError`] (a promise reject).
//! `ureq` follows redirects by default, as `fetch` does.

use bemtvi_lua::{HttpError, HttpRequest, HttpResponse};
use std::io::Read;
use std::time::Duration;

/// The default redirect-follow cap when `redirect == "follow"` and the caller gives no
/// `max_redirects` (browsers cap at ~20; ureq's own default is 5 — we split the difference
/// at a generous-but-bounded 10).
const DEFAULT_REDIRECTS: u32 = 10;

/// Build a `ureq::Agent` honoring the request's redirect policy. `"manual"` / `"error"`
/// follow zero redirects (the 3xx response is returned as-is; `"error"` is turned into a
/// reject by the caller); `"follow"` follows up to `max_redirects` (default
/// [`DEFAULT_REDIRECTS`]).
fn agent_for(req: &HttpRequest) -> ureq::Agent {
    let follow = req.redirect != "manual" && req.redirect != "error";
    let redirects = if follow {
        req.max_redirects.unwrap_or(DEFAULT_REDIRECTS)
    } else {
        0
    };
    ureq::builder().redirects(redirects).build()
}

/// Run one HTTP round-trip with `ureq`. Blocking — call only off the editor tick (the
/// actor's blocking pool / the daemon). Returns `Ok` for any HTTP status (including
/// 4xx/5xx, with `HttpResponse::status` carrying it), `Err` only on a transport failure
/// (or a redirect when `redirect == "error"`).
pub fn run_http_request(req: &HttpRequest) -> Result<HttpResponse, HttpError> {
    let mut request = agent_for(req).request(&req.method, &req.url);
    for (name, value) in &req.headers {
        request = request.set(name, value);
    }
    if let Some(ms) = req.timeout_ms {
        request = request.timeout(Duration::from_millis(ms));
    }
    // `call()` sends no body (a bare GET); `send_bytes` sends the raw payload.
    let outcome = if req.body.is_empty() {
        request.call()
    } else {
        request.send_bytes(&req.body)
    };
    match outcome {
        Ok(resp) => {
            // `redirect = "error"`: a 3xx that we didn't follow (redirects=0) rejects.
            if req.redirect == "error" && (300..400).contains(&resp.status()) {
                return Err(HttpError {
                    message: format!(
                        "btv.http: redirect ({}) not followed (redirect = \"error\")",
                        resp.status()
                    ),
                });
            }
            response_from_ureq(resp)
        }
        // A 4xx/5xx is a *resolved* response under fetch semantics — the caller branches
        // on `response.ok` / `response.status`, it is not a promise reject.
        Err(ureq::Error::Status(_code, resp)) => response_from_ureq(resp),
        // A real network / transport fault (DNS, connect, TLS, timeout, bad URL) rejects.
        Err(ureq::Error::Transport(t)) => Err(HttpError {
            message: format!("btv.http: {t}"),
        }),
    }
}

/// Project a `ureq::Response` onto the typed [`HttpResponse`]. Reads status + headers
/// (owned copies) before consuming the response to drain its body; a body-read failure is
/// a transport error (a mid-stream disconnect), so it rejects loud rather than resolving a
/// truncated body.
fn response_from_ureq(resp: ureq::Response) -> Result<HttpResponse, HttpError> {
    let status = resp.status();
    let status_text = resp.status_text().to_string();
    // `headers_names()` lists each header once; pair it with its value (lowercased name,
    // matching the browser `Headers` casing the `fetch`-modeled API mirrors).
    let headers = resp
        .headers_names()
        .into_iter()
        .filter_map(|name| {
            resp.header(&name)
                .map(|value| (name.to_lowercase(), value.to_string()))
        })
        .collect();
    let mut body = Vec::new();
    resp.into_reader()
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
