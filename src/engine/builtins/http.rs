//! HTTP builtins (read-class; used by health checks). Uses ureq; timeout defaults to 30s but is
//! configurable per-call (robustness review R12) since `sim_http_healthy`'s caller (`wait_healthy`)
//! runs it in a bounded retry loop — a fixed 30s timeout unrelated to that loop's own `interval`
//! meant a hanging endpoint could make `attempts: 30` (intended as a ~1 minute budget at the
//! default `interval: 2`) actually take up to 30 * 30s = 15 minutes.
//!
//! Bunny Magic Containers plan, Phase 1 (D1/D2): every verb now accepts an optional trailing
//! `headers` map argument (`#{"Authorization": "Bearer " + token}`) so a script can drive a real
//! authenticated REST API, and `http_put`/`http_patch`/`http_delete` join the existing
//! `http_get`/`http_post` — enough surface for a PaaS provider module (Bunny et al.) to be written
//! entirely in Rhai stdlib, no new Rust builtin needed. `do_get`/`do_post`'s pre-existing external
//! behavior (including their own dry-run classification) is unchanged; the headers-accepting
//! overloads share the SAME `finish`/`apply_headers`/`do_body_request`/`do_bodyless_request` path
//! as the non-headers ones, not a second near-duplicate implementation.
//!
//! Header-secret redaction: a header value built via `reveal(secret(...))` (string concatenation
//! with a bare `Secret` is refused elsewhere — see `secret.rs`'s `NO_CONCAT`) is a plain `String`
//! by the time it reaches this file, but the underlying plaintext is already registered in
//! `ctx.secrets` (secret() does that at read time) — so routing every recorded dry-run detail
//! through the SAME `ctx.record()` every other builtin already uses (which itself calls
//! `secret::redact()` before storing) is sufficient; no second redaction mechanism needed here.
//! Accordingly, the dry-run "check" detail for a write verb never includes raw header CONTENT
//! (only a header count) — belt-and-suspenders, since `ctx.record` would redact a leaked secret
//! substring anyway, but there is then nothing secret-shaped to redact in the first place.

use crate::engine::context::{EffectMode, SharedCtx};
use crate::engine::types::HttpResponse;
use rhai::{Array, Dynamic, Engine, EvalAltResult};
use std::thread;
use ureq::typestate::{WithBody, WithoutBody};
use ureq::RequestBuilder;

const DEFAULT_HTTP_TIMEOUT_SECS: i64 = 30;

fn agent(timeout_secs: i64) -> ureq::Agent {
    // A non-positive value would mean "no timeout" to ureq (Some(0) panics; None disables it
    // entirely) — neither is a sane meaning for a caller-supplied retry-loop timeout, so clamp to
    // at least 1s rather than letting a `0`/negative cfg value silently hang forever.
    let secs = timeout_secs.max(1) as u64;
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(secs)))
        // Do NOT treat 4xx/5xx as transport errors: ureq 3 defaults to `http_status_as_error`,
        // which folds every non-2xx into `Error::StatusCode` with an EMPTY body — a 503 health
        // endpoint returning JSON diagnostics would lose the body the script wants to inspect.
        // With this off, those responses land in the `Ok` arm with status + body intact, and only
        // a genuine TRANSPORT failure (DNS, connect, TLS, timeout) reaches the `Err` arm (issue
        // #28). That `status:0` is then unambiguous: 0 means "no HTTP response at all".
        .http_status_as_error(false)
        .build()
        .new_agent()
}

/// Apply every `(key, value)` header pair to a request builder, in whatever order `headers`
/// iterates (in practice key-sorted — see `headers_from_dynamic`'s own doc comment — since a Rhai
/// map can't express duplicate header names anyway, order has no HTTP-semantic effect here).
/// Generic over the ureq typestate (`WithBody`/`WithoutBody`) since header-setting is identical
/// either way — this is the "one shared request path" for headers the plan calls for, used by
/// every verb below.
fn apply_headers<B>(mut req: RequestBuilder<B>, headers: &[(String, String)]) -> RequestBuilder<B> {
    for (k, v) in headers {
        req = req.header(k, v);
    }
    req
}

/// Turn a raw ureq result into an `HttpResponse`, the ONE place every verb below extracts
/// status/body or classifies a transport failure — shared so GET/POST/PUT/PATCH/DELETE can't
/// silently diverge in how they report a transport-vs-real-response outcome.
fn finish(result: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> HttpResponse {
    match result {
        Ok(resp) => {
            let status = resp.status().as_u16() as i64;
            let body = resp.into_body().read_to_string().unwrap_or_default();
            HttpResponse { status, body }
        }
        // Transport failure only (no HTTP response). status:0 distinguishes it from any real code.
        Err(e) => HttpResponse {
            status: 0,
            body: format!("request failed: {e}"),
        },
    }
}

/// GET/DELETE (ureq's `WithoutBody` typestate): apply headers, then `.call()`.
fn do_bodyless_request(req: RequestBuilder<WithoutBody>, headers: &[(String, String)]) -> HttpResponse {
    finish(apply_headers(req, headers).call())
}

/// POST/PUT/PATCH (ureq's `WithBody` typestate): apply headers, default to the same
/// `Content-Type` the original `do_post` always sent, then `.send(body)`.
///
/// Opus review: `.header()` APPENDS rather than replaces (both ureq's `RequestBuilder` and the
/// underlying `http::request::Builder` it wraps) — unconditionally appending the default
/// `Content-Type` AFTER a caller-supplied one produced a request with TWO `Content-Type` headers
/// on the wire, not an override. A real caller hits this immediately: RFC 7396 JSON Merge Patch
/// is the conventional `Content-Type` for a `PATCH`, and a strict server can reject a request with
/// duplicate `Content-Type` headers with a 400, or simply use the wrong one. Only append the
/// default when the caller didn't already supply their own (case-insensitively — HTTP header
/// names are case-insensitive).
fn do_body_request(req: RequestBuilder<WithBody>, body: &str, headers: &[(String, String)]) -> HttpResponse {
    let has_content_type = headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type"));
    let mut req = apply_headers(req, headers);
    if !has_content_type {
        req = req.header("Content-Type", "application/json");
    }
    finish(req.send(body.as_bytes()))
}

fn do_get(url: &str, timeout_secs: i64, headers: &[(String, String)]) -> HttpResponse {
    do_bodyless_request(agent(timeout_secs).get(url), headers)
}

/// The shared write-verb path: POST/PUT/PATCH/DELETE all short-circuit identically under
/// `--dry-run` (a synthetic 200 + a recorded `check`, never a real request), and otherwise route
/// to the one live request helper appropriate for that verb's ureq typestate.
fn write_verb_response(
    ctx: &SharedCtx,
    verb_label: &str,
    url: &str,
    body: Option<&str>,
    headers: &[(String, String)],
) -> HttpResponse {
    if ctx.mode == EffectMode::DryRun {
        // Deliberately no header CONTENT here — see this file's top-level doc comment. A count is
        // still useful for a human reading the plan without risking anything secret-shaped.
        let hdr_note = if headers.is_empty() {
            String::new()
        } else {
            format!(" ({} header(s))", headers.len())
        };
        ctx.record("check", None, format!("[assumed ok] {verb_label} {url}{hdr_note}"));
        return HttpResponse {
            status: 200,
            body: String::new(),
        };
    }
    match verb_label {
        "POST" => do_body_request(agent(DEFAULT_HTTP_TIMEOUT_SECS).post(url), body.unwrap_or(""), headers),
        "PUT" => do_body_request(agent(DEFAULT_HTTP_TIMEOUT_SECS).put(url), body.unwrap_or(""), headers),
        "PATCH" => do_body_request(agent(DEFAULT_HTTP_TIMEOUT_SECS).patch(url), body.unwrap_or(""), headers),
        "DELETE" => do_bodyless_request(agent(DEFAULT_HTTP_TIMEOUT_SECS).delete(url), headers),
        _ => unreachable!("write_verb_response: unsupported verb {verb_label}"),
    }
}

/// Convert a Rhai `headers` argument (expected to be a `#{"Key": "Value", ...}` map) into a
/// `Vec<(String, String)>` — key-sorted, since `rhai::Map` is a `BTreeMap` (irrelevant for HTTP
/// semantics: a map can't express duplicate header names, so iteration order has no observable
/// effect on the request). Rejects — via a Rhai-catchable `EvalAltResult`, never a panic — a
/// non-map argument, or a map whose value isn't itself a string, naming the offending key so a
/// script author isn't left guessing. A bare `Secret` value is rejected here too (it isn't a
/// `String` at the Rhai type level) — the message steers the caller at `reveal(secret(...))`
/// or string concatenation, which is what actually registers the plaintext for redaction.
fn headers_from_dynamic(headers: Dynamic) -> Result<Vec<(String, String)>, Box<EvalAltResult>> {
    let map = headers.try_cast::<rhai::Map>().ok_or_else(|| -> Box<EvalAltResult> {
        "http headers argument must be a map, e.g. #{\"Authorization\": \"Bearer \" + token}".into()
    })?;
    let mut out = Vec::with_capacity(map.len());
    for (k, v) in map {
        let value = v.into_string().map_err(|type_name| -> Box<EvalAltResult> {
            // Fable review: a custom registered type's name comes back as its full Rust path
            // (e.g. "nrg::engine::secret::Secret") rather than the friendly name Rhai scripts
            // know it by ("Secret") — strip any module-path prefix for a cleaner message.
            let friendly = type_name.rsplit("::").next().unwrap_or(type_name);
            format!(
                "http header '{k}' must be a string value, got {friendly} — build it with \
                 string concatenation or reveal(secret(...)) first"
            )
            .into()
        })?;
        out.push((k.to_string(), value));
    }
    Ok(out)
}

/// One element of `http_patch_all`'s `requests` array, after validation.
struct PatchRequest {
    url: String,
    body: String,
    headers: Vec<(String, String)>,
}

/// Parse one `requests[i]` element (expected `#{url: String, body: String, headers?: Map}`) into
/// a `PatchRequest`, or a Rhai-catchable error naming the index — never a panic on a malformed
/// element (a missing/wrong-typed `url`/`body`, or an unparseable `headers`).
fn parse_patch_request(i: usize, item: Dynamic) -> Result<PatchRequest, Box<EvalAltResult>> {
    let map = item.try_cast::<rhai::Map>().ok_or_else(|| -> Box<EvalAltResult> {
        format!(
            "http_patch_all: requests[{i}] must be a map, e.g. #{{url: \"...\", body: \"...\"}}"
        )
        .into()
    })?;
    let url = map
        .get("url")
        .cloned()
        .ok_or_else(|| -> Box<EvalAltResult> {
            format!("http_patch_all: requests[{i}] is missing required key \"url\"").into()
        })?
        .into_string()
        .map_err(|ty| -> Box<EvalAltResult> {
            format!("http_patch_all: requests[{i}].url must be a string, got {ty}").into()
        })?;
    let body = map
        .get("body")
        .cloned()
        .ok_or_else(|| -> Box<EvalAltResult> {
            format!("http_patch_all: requests[{i}] is missing required key \"body\"").into()
        })?
        .into_string()
        .map_err(|ty| -> Box<EvalAltResult> {
            format!("http_patch_all: requests[{i}].body must be a string, got {ty}").into()
        })?;
    let headers = match map.get("headers") {
        Some(h) => headers_from_dynamic(h.clone())
            .map_err(|e| -> Box<EvalAltResult> { format!("requests[{i}]: {e}").into() })?,
        None => Vec::new(),
    };
    Ok(PatchRequest { url, body, headers })
}

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    // http_get(url) — a READ of EXISTING reality. It executes FOR REAL even in dry-run (a GET has
    // no side effect), so a script that gates the plan on current prod health —
    // `if !http_get(prod_url).ok { throw }` — sees the truth, not a synthetic 200 (issue #16).
    // The plan records the probed status. The NOT-yet-started new container is checked via
    // `sim_http_healthy` (below), which the stdlib's wait_healthy uses instead.
    {
        let ctx = ctx.clone();
        engine.register_fn("http_get", move |url: &str| -> HttpResponse {
            let r = do_get(url, DEFAULT_HTTP_TIMEOUT_SECS, &[]);
            if ctx.mode == EffectMode::DryRun {
                ctx.record("check", None, format!("GET {url} -> {} (probed live)", r.status));
            }
            r
        });
    }
    // http_get(url, headers) — headers overload. A header must never change whether GET is
    // treated as a live read or a simulated write — same dry-run semantics as the 1-arg form.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "http_get",
            move |url: &str, headers: Dynamic| -> Result<HttpResponse, Box<EvalAltResult>> {
                let headers = headers_from_dynamic(headers)?;
                let r = do_get(url, DEFAULT_HTTP_TIMEOUT_SECS, &headers);
                if ctx.mode == EffectMode::DryRun {
                    ctx.record("check", None, format!("GET {url} -> {} (probed live)", r.status));
                }
                Ok(r)
            },
        );
    }
    // sim_http_healthy(url, timeout_secs) — the NEW-container health probe used by wait_healthy.
    // In dry-run the new container isn't running yet, so a real probe of its (symbolic) port would
    // always fail; we short-circuit to a synthetic healthy 200 and record a 'check'. Live: a real
    // GET with the CALLER's timeout (robustness review R12) — wait_healthy's own retry loop is the
    // thing that should bound total wait time, not a hardcoded 30s per request unrelated to it.
    {
        let ctx = ctx.clone();
        engine.register_fn("sim_http_healthy", move |url: &str, timeout_secs: i64| -> HttpResponse {
            if ctx.mode == EffectMode::DryRun {
                ctx.record("check", None, format!("[assumed healthy] GET {url}"));
                return HttpResponse { status: 200, body: String::new() };
            }
            do_get(url, timeout_secs, &[])
        });
    }
    // sim_http_healthy(url) — 1-arg overload (the historical fixed 30s default).
    {
        let ctx = ctx.clone();
        engine.register_fn("sim_http_healthy", move |url: &str| -> HttpResponse {
            if ctx.mode == EffectMode::DryRun {
                ctx.record("check", None, format!("[assumed healthy] GET {url}"));
                return HttpResponse { status: 200, body: String::new() };
            }
            do_get(url, DEFAULT_HTTP_TIMEOUT_SECS, &[])
        });
    }
    // http_post(url, body) — POST is a WRITE — never execute it in dry-run; record + synthetic ok.
    {
        let ctx = ctx.clone();
        engine.register_fn("http_post", move |url: &str, body: &str| -> HttpResponse {
            write_verb_response(&ctx, "POST", url, Some(body), &[])
        });
    }
    // http_post(url, body, headers) — headers overload.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "http_post",
            move |url: &str, body: &str, headers: Dynamic| -> Result<HttpResponse, Box<EvalAltResult>> {
                let headers = headers_from_dynamic(headers)?;
                Ok(write_verb_response(&ctx, "POST", url, Some(body), &headers))
            },
        );
    }
    // http_put(url, body) / http_put(url, body, headers) — new verb (Bunny Magic Containers
    // Phase 1): same WRITE semantics as http_post.
    {
        let ctx = ctx.clone();
        engine.register_fn("http_put", move |url: &str, body: &str| -> HttpResponse {
            write_verb_response(&ctx, "PUT", url, Some(body), &[])
        });
    }
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "http_put",
            move |url: &str, body: &str, headers: Dynamic| -> Result<HttpResponse, Box<EvalAltResult>> {
                let headers = headers_from_dynamic(headers)?;
                Ok(write_verb_response(&ctx, "PUT", url, Some(body), &headers))
            },
        );
    }
    // http_patch(url, body) / http_patch(url, body, headers) — new verb.
    {
        let ctx = ctx.clone();
        engine.register_fn("http_patch", move |url: &str, body: &str| -> HttpResponse {
            write_verb_response(&ctx, "PATCH", url, Some(body), &[])
        });
    }
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "http_patch",
            move |url: &str, body: &str, headers: Dynamic| -> Result<HttpResponse, Box<EvalAltResult>> {
                let headers = headers_from_dynamic(headers)?;
                Ok(write_verb_response(&ctx, "PATCH", url, Some(body), &headers))
            },
        );
    }
    // http_delete(url) / http_delete(url, headers) — new verb. No body.
    {
        let ctx = ctx.clone();
        engine.register_fn("http_delete", move |url: &str| -> HttpResponse {
            write_verb_response(&ctx, "DELETE", url, None, &[])
        });
    }
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "http_delete",
            move |url: &str, headers: Dynamic| -> Result<HttpResponse, Box<EvalAltResult>> {
                let headers = headers_from_dynamic(headers)?;
                Ok(write_verb_response(&ctx, "DELETE", url, None, &headers))
            },
        );
    }
    // http_patch_all(requests) — Bunny Magic Containers Phase 3: a generic PARALLEL PATCH
    // fan-out, mirroring ssh_exec_all's exact contract (src/engine/builtins/exec.rs) — never
    // aborts the batch on one request's failure, one response per input in the SAME order.
    // `requests`: Array of #{url: String, body: String, headers?: Map}.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "http_patch_all",
            move |requests: Array| -> Result<Array, Box<EvalAltResult>> {
                let parsed: Vec<PatchRequest> = requests
                    .into_iter()
                    .enumerate()
                    .map(|(i, item)| parse_patch_request(i, item))
                    .collect::<Result<_, _>>()?;

                if ctx.mode == EffectMode::DryRun {
                    // Sequential: nothing real happens either way, so no threads are needed —
                    // each element short-circuits exactly like a single http_patch call would.
                    return Ok(parsed
                        .into_iter()
                        .map(|r| {
                            Dynamic::from(write_verb_response(
                                &ctx, "PATCH", &r.url, Some(&r.body), &r.headers,
                            ))
                        })
                        .collect());
                }

                let results: Vec<HttpResponse> = thread::scope(|s| {
                    let handles: Vec<_> = parsed
                        .iter()
                        .map(|r| {
                            let url = r.url.clone();
                            let body = r.body.clone();
                            let headers = r.headers.clone();
                            s.spawn(move || {
                                do_body_request(
                                    agent(DEFAULT_HTTP_TIMEOUT_SECS).patch(&url),
                                    &body,
                                    &headers,
                                )
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .zip(parsed.iter())
                        .map(|(j, r)| {
                            // On a thread panic, attribute it to the right request (mirrors
                            // ssh_exec_all's own panic-attribution convention) rather than
                            // losing which URL failed.
                            j.join().unwrap_or_else(|_| HttpResponse {
                                status: 0,
                                body: format!("request failed: thread panicked ({})", r.url),
                            })
                        })
                        .collect()
                });
                Ok(results.into_iter().map(Dynamic::from).collect())
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::{shared, shared_dry};
    use crate::engine::runner::FakeRunner;

    #[test]
    fn http_builtins_register() {
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, shared(FakeRunner::shared()));
        // Assert the symbols exist by compiling a script that references them.
        assert!(e
            .compile(
                r#"fn _f(){
                    http_get("http://x"); http_get("http://x", #{});
                    http_post("http://x","{}"); http_post("http://x","{}", #{});
                    http_put("http://x","{}"); http_put("http://x","{}", #{});
                    http_patch("http://x","{}"); http_patch("http://x","{}", #{});
                    http_delete("http://x"); http_delete("http://x", #{});
                    sim_http_healthy("http://x");
                }"#
            )
            .is_ok());
    }

    #[test]
    fn sim_http_healthy_short_circuits_in_dry_run() {
        let ctx = shared_dry(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);
        // The new-container probe returns synthetic healthy 200 even for an unreachable URL.
        let ok: bool = e.eval(r#"sim_http_healthy("http://127.0.0.1:1/never").ok"#).unwrap();
        assert!(ok);
    }

    #[test]
    fn sim_http_healthy_honors_a_caller_supplied_timeout_instead_of_the_fixed_30s() {
        // Robustness review R12: sim_http_healthy's HTTP timeout used to be hardcoded to 30s
        // regardless of the caller's own retry budget, so a hanging (connects, never responds)
        // endpoint made a health-check retry loop take far longer than its `attempts * interval`
        // implied. A listener that accepts the connection and then never writes a response
        // simulates exactly that: with timeout_secs=1 the call must give up in ~1s, not 30s.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                std::thread::sleep(std::time::Duration::from_secs(30));
                drop(stream);
            }
        });

        let ctx = shared(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);
        let url = format!("http://{addr}/");
        let start = std::time::Instant::now();
        let status: i64 = e
            .eval(&format!(r#"sim_http_healthy("{url}", 1).status"#))
            .unwrap();
        let elapsed = start.elapsed();
        assert_eq!(status, 0, "a hung endpoint must surface as a transport failure (status 0)");
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "must honor the 1s timeout, not the old fixed 30s: took {elapsed:?}"
        );
    }

    /// Bind an ephemeral localhost listener that accepts exactly one connection, reads whatever
    /// the client sends into a buffer the test can inspect, writes `response` verbatim, then
    /// exits — a minimal real HTTP server standing in for a REST endpoint, so these tests exercise
    /// the REAL `ureq` round trip (status parsing, body extraction, raw request bytes) instead of
    /// only ever hitting unreachable URLs. Returns the address to connect to and a receiver for
    /// the raw bytes the server actually read off the socket.
    fn spawn_http_responder_capturing_request(
        response: &'static str,
    ) -> (std::net::SocketAddr, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut stream, _)) = listener.accept() {
                stream.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
                let mut received = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => received.extend_from_slice(&buf[..n]),
                        Err(_) => break, // timed out waiting for more — client is done sending
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&received).into_owned());
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (addr, rx)
    }

    /// Bind an ephemeral localhost listener that accepts exactly one connection, discards
    /// whatever the client sends, writes `response` verbatim, then exits — a minimal real HTTP
    /// server standing in for a health-check endpoint, so these tests exercise the REAL `ureq`
    /// round trip (status parsing, body extraction) instead of only ever hitting unreachable
    /// URLs. Returns the address to connect to.
    fn spawn_http_responder(response: &'static str) -> std::net::SocketAddr {
        spawn_http_responder_capturing_request(response).0
    }

    #[test]
    fn http_get_extracts_status_and_body_on_a_real_successful_response() {
        // Robustness review: no test ever performed a SUCCESSFUL HTTP request — only
        // unreachable-URL failures and dry-run short-circuits, leaving the actual `ureq`
        // status/body-extraction wiring unverified against a real 2xx response.
        let addr = spawn_http_responder(
            "HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\nhello, world!",
        );
        let ctx = shared(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);
        let r: HttpResponse = e.eval(&format!(r#"http_get("http://{addr}/")"#)).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "hello, world!");
    }

    #[test]
    fn http_get_extracts_status_and_body_on_a_real_5xx_response_instead_of_a_transport_error() {
        // The whole point of `http_status_as_error(false)` (see `agent()` above): a 503 health
        // endpoint returning JSON diagnostics must land in the `Ok` arm with the REAL status and
        // body intact, not get folded into a transport-style failure with an empty body. This
        // proves that against a real non-2xx response, not just by reading the ureq docs.
        let addr = spawn_http_responder(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 20\r\nConnection: close\r\n\r\n{\"error\":\"overload\"}",
        );
        let ctx = shared(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);
        let r: HttpResponse = e.eval(&format!(r#"http_get("http://{addr}/")"#)).unwrap();
        assert_eq!(r.status, 503, "the real 5xx status must be preserved, not folded to 0");
        assert_eq!(r.body, "{\"error\":\"overload\"}", "the 5xx body must still be extracted");
    }

    #[test]
    fn http_post_sends_its_body_and_extracts_a_real_response() {
        // No test ever exercised a successful http_post round trip either. This also confirms
        // the POST body actually reaches the wire (not just that a request-shaped connection
        // happens), by asserting the server saw it in what it read off the socket.
        let (addr, rx) = spawn_http_responder_capturing_request(
            "HTTP/1.1 201 Created\r\nContent-Length: 8\r\nConnection: close\r\n\r\naccepted",
        );

        let ctx = shared(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);
        let r: HttpResponse =
            e.eval(&format!(r#"http_post("http://{addr}/", "{{\"deploy\":true}}")"#)).unwrap();
        assert_eq!(r.status, 201);
        assert_eq!(r.body, "accepted");

        let received = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(
            received.contains(r#"{"deploy":true}"#),
            "the POST body must actually reach the server, not just a bare request line: {received:?}"
        );
    }

    #[test]
    fn http_get_probes_for_real_in_dry_run() {
        // http_get is an honest READ even in dry-run: an unreachable URL returns a transport
        // failure (status 0), NOT a synthetic 200, so precondition checks can't lie (issue #16).
        let ctx = shared_dry(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx.clone());
        let status: i64 = e.eval(r#"http_get("http://127.0.0.1:1/never").status"#).unwrap();
        assert_eq!(status, 0, "an unreachable host must surface as status 0, not 200");
        let plan = ctx.plan.lock().unwrap().clone();
        assert!(plan.iter().any(|a| a.detail.contains("probed live")));
    }

    // -----------------------------------------------------------------------
    // Bunny Magic Containers Phase 1: headers + PUT/PATCH/DELETE
    // -----------------------------------------------------------------------

    #[test]
    fn http_get_with_a_custom_header_actually_sends_it() {
        let (addr, rx) = spawn_http_responder_capturing_request(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        );
        let ctx = shared(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);
        let r: HttpResponse = e
            .eval(&format!(
                r#"http_get("http://{addr}/", #{{"Authorization": "Bearer test-token"}})"#
            ))
            .unwrap();
        assert_eq!(r.status, 200);
        let received = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        // Header NAMES are normalized to lowercase on the wire by the underlying `http` crate —
        // check case-insensitively, same as any real server would.
        assert!(
            received.to_lowercase().contains("authorization: bearer test-token"),
            "the custom header must actually reach the wire: {received:?}"
        );
    }

    #[test]
    fn http_put_sends_its_body_and_a_custom_header_and_extracts_a_real_response() {
        let (addr, rx) = spawn_http_responder_capturing_request(
            "HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nupdated",
        );
        let ctx = shared(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);
        let r: HttpResponse = e
            .eval(&format!(
                r#"http_put("http://{addr}/", "{{\"name\":\"x\"}}", #{{"Authorization": "Bearer test-token"}})"#
            ))
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "updated");
        let received = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(received.contains(r#"{"name":"x"}"#), "got: {received:?}");
        assert!(
            received.to_lowercase().contains("authorization: bearer test-token"),
            "got: {received:?}"
        );
        assert!(received.starts_with("PUT "), "got: {received:?}");
    }

    #[test]
    fn http_patch_sends_its_body_and_a_custom_header_and_extracts_a_real_response() {
        let (addr, rx) = spawn_http_responder_capturing_request(
            "HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\npatched",
        );
        let ctx = shared(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);
        let r: HttpResponse = e
            .eval(&format!(
                r#"http_patch("http://{addr}/", "{{\"name\":\"y\"}}", #{{"X-Custom": "abc"}})"#
            ))
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "patched");
        let received = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(received.contains(r#"{"name":"y"}"#), "got: {received:?}");
        assert!(received.to_lowercase().contains("x-custom: abc"), "got: {received:?}");
        assert!(received.starts_with("PATCH "), "got: {received:?}");
    }

    #[test]
    fn a_caller_supplied_content_type_replaces_the_default_instead_of_duplicating_it() {
        // Opus review: `.header()` APPENDS rather than replaces (both ureq's own RequestBuilder
        // and the underlying http::request::Builder it wraps) — do_body_request used to
        // unconditionally append the default "Content-Type: application/json" AFTER the caller's
        // headers, so a caller overriding Content-Type (e.g. "application/merge-patch+json" for a
        // real RFC 7396 PATCH — exactly the kind of REST API this feature exists to drive) got
        // TWO Content-Type headers on the wire instead of their own. A strict server can reject a
        // request with a duplicate header outright, or simply honor the wrong one.
        let (addr, rx) = spawn_http_responder_capturing_request(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        );
        let ctx = shared(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);
        let r: HttpResponse = e
            .eval(&format!(
                r#"http_patch("http://{addr}/", "{{}}", #{{"Content-Type": "application/merge-patch+json"}})"#
            ))
            .unwrap();
        assert_eq!(r.status, 200);
        let received = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap().to_lowercase();
        let content_type_lines = received
            .lines()
            .filter(|l| l.starts_with("content-type:"))
            .count();
        assert_eq!(content_type_lines, 1, "expected exactly one Content-Type header, got: {received:?}");
        assert!(
            received.contains("content-type: application/merge-patch+json"),
            "the caller's own Content-Type must win, not the default: {received:?}"
        );
    }

    #[test]
    fn http_delete_sends_its_header_with_no_body_and_reads_the_response() {
        let (addr, rx) = spawn_http_responder_capturing_request(
            "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n",
        );
        let ctx = shared(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);
        let r: HttpResponse = e
            .eval(&format!(
                r#"http_delete("http://{addr}/", #{{"Authorization": "Bearer test-token"}})"#
            ))
            .unwrap();
        assert_eq!(r.status, 204);
        let received = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(received.starts_with("DELETE "), "got: {received:?}");
        assert!(
            received.to_lowercase().contains("authorization: bearer test-token"),
            "got: {received:?}"
        );
    }

    #[test]
    fn every_new_verb_surfaces_a_real_non_2xx_status_and_body_instead_of_a_transport_error() {
        // Mirrors http_get_extracts_status_and_body_on_a_real_5xx_response_instead_of_a_transport_error
        // for the new verbs — a Bunny API returning a 404/409 with a JSON error body is exactly
        // the case a provider script needs to inspect, not have collapsed to status 0.
        for (verb, script_fn) in [
            ("http_put", "http_put"),
            ("http_patch", "http_patch"),
            ("http_delete", "http_delete"),
        ] {
            // The looped-read responder, not the single-`read()` one: PUT/PATCH send a body as a
            // separate write after the headers, and a single non-looping read can race the
            // server's canned reply against the client still writing that body on loopback.
            let (addr, _rx) = spawn_http_responder_capturing_request(
                "HTTP/1.1 409 Conflict\r\nContent-Length: 16\r\nConnection: close\r\n\r\n{\"error\":\"busy\"}",
            );
            let ctx = shared(FakeRunner::shared());
            let mut e = Engine::new();
            crate::engine::types::register_types(&mut e);
            register(&mut e, ctx);
            let script = if verb == "http_delete" {
                format!(r#"{script_fn}("http://{addr}/")"#)
            } else {
                format!(r#"{script_fn}("http://{addr}/", "{{}}")"#)
            };
            let r: HttpResponse = e.eval(&script).unwrap();
            assert_eq!(r.status, 409, "{verb}: real non-2xx status must be preserved, not folded to 0");
            assert_eq!(r.body, "{\"error\":\"busy\"}", "{verb}: non-2xx body must still be extracted");
        }
    }

    #[test]
    fn http_get_with_headers_still_probes_for_real_in_dry_run() {
        // Headers must not change GET's live-vs-simulated classification, mirroring
        // http_get_probes_for_real_in_dry_run for the headers overload.
        let ctx = shared_dry(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx.clone());
        let status: i64 = e
            .eval(r#"http_get("http://127.0.0.1:1/never", #{"Authorization": "Bearer x"}).status"#)
            .unwrap();
        assert_eq!(status, 0, "an unreachable host must surface as status 0, not 200, even with headers");
        let plan = ctx.plan.lock().unwrap().clone();
        assert!(plan.iter().any(|a| a.detail.contains("probed live")));
    }

    #[test]
    fn every_write_verb_short_circuits_under_dry_run() {
        for script in [
            r#"http_post("http://127.0.0.1:1/never", "{}")"#,
            r#"http_post("http://127.0.0.1:1/never", "{}", #{"Authorization": "Bearer x"})"#,
            r#"http_put("http://127.0.0.1:1/never", "{}")"#,
            r#"http_put("http://127.0.0.1:1/never", "{}", #{"Authorization": "Bearer x"})"#,
            r#"http_patch("http://127.0.0.1:1/never", "{}")"#,
            r#"http_patch("http://127.0.0.1:1/never", "{}", #{"Authorization": "Bearer x"})"#,
            r#"http_delete("http://127.0.0.1:1/never")"#,
            r#"http_delete("http://127.0.0.1:1/never", #{"Authorization": "Bearer x"})"#,
        ] {
            let ctx = shared_dry(FakeRunner::shared());
            let mut e = Engine::new();
            crate::engine::types::register_types(&mut e);
            register(&mut e, ctx.clone());
            let r: HttpResponse = e.eval(script).unwrap();
            assert_eq!(r.status, 200, "must short-circuit to a synthetic 200 under dry-run: {script}");
            let plan = ctx.plan.lock().unwrap().clone();
            assert!(
                plan.iter().any(|a| a.kind == "check"),
                "must record a 'check' action under dry-run: {script}"
            );
        }
    }

    #[test]
    fn a_secret_valued_header_is_never_written_to_the_plan_log_in_plaintext() {
        // Definition of done: a secret-valued header must never appear in plaintext in a dry-run
        // plan render. Registers a secret, builds a header value from its revealed plaintext (the
        // only way to get a String out of a Secret — bare concatenation is refused), and confirms
        // the plaintext never appears anywhere in the recorded plan, even though the write verb
        // recorded a 'check' action for this exact call.
        let _env_guard = crate::test_support::lock_env();
        std::env::set_var("NRG_SECRET_BUNNY_TOKEN", "supersecretbunnytoken");
        let ctx = shared_dry(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::secret::register(&mut e, ctx.clone());
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx.clone());
        let r: HttpResponse = e
            .eval(
                r#"http_post("http://127.0.0.1:1/never", "{}",
                    #{"Authorization": "Bearer " + reveal(secret("BUNNY_TOKEN"))})"#,
            )
            .unwrap();
        assert_eq!(r.status, 200);
        let plan = ctx.plan.lock().unwrap().clone();
        assert!(plan.iter().any(|a| a.kind == "check"), "must have recorded a check action");
        for action in plan.iter() {
            assert!(
                !action.detail.contains("supersecretbunnytoken"),
                "the secret plaintext leaked into the plan log: {:?}",
                action.detail
            );
        }
        std::env::remove_var("NRG_SECRET_BUNNY_TOKEN");
    }

    #[test]
    fn headers_argument_rejects_a_non_map() {
        let ctx = shared(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);
        let err = e
            .eval::<HttpResponse>(r#"http_get("http://127.0.0.1:1/never", "not-a-map")"#)
            .unwrap_err();
        assert!(err.to_string().contains("must be a map"), "got: {err}");
    }

    #[test]
    fn headers_argument_rejects_a_non_string_value_and_names_the_offending_key() {
        let ctx = shared(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);
        let err = e
            .eval::<HttpResponse>(r#"http_get("http://127.0.0.1:1/never", #{"X-Count": 5})"#)
            .unwrap_err();
        assert!(err.to_string().contains("X-Count"), "must name the offending key: {err}");
        assert!(err.to_string().contains("must be a string"), "got: {err}");
    }

    #[test]
    fn headers_argument_rejects_a_bare_secret_with_a_friendly_type_name() {
        // Fable review: `Dynamic::into_string`'s error carries the full Rust path of a custom
        // registered type (e.g. "nrg::engine::secret::Secret"), not the friendly name Rhai
        // scripts actually know it by ("Secret") — the message should read cleanly either way,
        // and a bare Secret (no reveal()) must be rejected here rather than silently producing
        // the SECRET_SENTINEL text as a "header value".
        let _env_guard = crate::test_support::lock_env();
        std::env::set_var("NRG_SECRET_BARE_TOKEN", "supersecretbaretoken");
        let ctx = shared(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::secret::register(&mut e, ctx.clone());
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);
        let err = e
            .eval::<HttpResponse>(
                r#"http_get("http://127.0.0.1:1/never", #{"Authorization": secret("BARE_TOKEN")})"#,
            )
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Authorization"), "must name the offending key: {msg}");
        assert!(msg.contains("Secret"), "must name the friendly type, not a Rust path: {msg}");
        assert!(!msg.contains("::"), "must not leak the full Rust module path: {msg}");
        assert!(!msg.contains("supersecretbaretoken"), "must never leak the plaintext: {msg}");
        std::env::remove_var("NRG_SECRET_BARE_TOKEN");
    }

    // -----------------------------------------------------------------------------------------
    // http_patch_all — Bunny Magic Containers Phase 3
    // -----------------------------------------------------------------------------------------

    /// Bind an ephemeral localhost listener that accepts one connection, sleeps `delay` before
    /// responding (simulating a slow endpoint), then writes `response`. Used to prove
    /// `http_patch_all` genuinely runs requests concurrently: N listeners each sleeping `delay`
    /// finish in ~`delay` total only if dispatched in parallel, not `N * delay`.
    fn spawn_slow_http_responder(
        delay: std::time::Duration,
        response: &'static str,
    ) -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut stream, _)) = listener.accept() {
                // A single read is enough — the whole small PATCH request arrives in one packet
                // on loopback. Do NOT loop until EOF/timeout: ureq keeps its write half open
                // while awaiting the response, so a read-to-EOF loop would just block for the
                // full read-timeout on every call, adding constant overhead to every request and
                // defeating the very concurrency proof this helper exists for.
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                std::thread::sleep(delay);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        addr
    }

    #[test]
    fn http_patch_all_registers_and_dry_runs_cleanly() {
        let ctx = shared_dry(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);
        let n: i64 = e
            .eval(
                r#"http_patch_all([
                    #{url: "http://x/1", body: "{}"},
                    #{url: "http://x/2", body: "{}", headers: #{"AccessKey": "k"}},
                ]).len()"#,
            )
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn http_patch_all_runs_requests_concurrently_not_sequentially() {
        const DELAY: std::time::Duration = std::time::Duration::from_millis(400);
        let ok_resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
        let addrs: Vec<_> = (0..4).map(|_| spawn_slow_http_responder(DELAY, ok_resp)).collect();

        let ctx = shared(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);

        let requests = addrs
            .iter()
            .map(|a| format!(r#"#{{url: "http://{a}/", body: "{{}}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let script = format!("http_patch_all([{requests}]).len()");

        let start = std::time::Instant::now();
        let n: i64 = e.eval(&script).unwrap();
        let elapsed = start.elapsed();

        assert_eq!(n, 4);
        assert!(
            elapsed < DELAY * 2,
            "4 requests each taking {DELAY:?} must run concurrently (total ~{DELAY:?}), \
             not sequentially (~{:?}): took {elapsed:?}",
            DELAY * 4
        );
    }

    #[test]
    fn http_patch_all_isolates_a_single_request_failure_from_the_rest() {
        let ok_resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
        let addr_a = spawn_http_responder(ok_resp);
        let addr_b = spawn_http_responder(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let addr_c = spawn_http_responder(ok_resp);

        let ctx = shared(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);

        let script = format!(
            r#"let r = http_patch_all([
                #{{url: "http://{addr_a}/", body: "{{}}"}},
                #{{url: "http://{addr_b}/", body: "{{}}"}},
                #{{url: "http://{addr_c}/", body: "{{}}"}},
            ]);
            r[0].status + "," + r[1].status + "," + r[2].status"#
        );
        let statuses: String = e.eval(&script).unwrap();
        assert_eq!(
            statuses, "200,500,200",
            "one failing request must not lose, corrupt, or reorder the others"
        );
    }

    #[test]
    fn http_patch_all_short_circuits_every_element_under_dry_run_with_no_listener_contacted() {
        let (addr, rx) = spawn_bunny_probe_listener();

        let ctx = shared_dry(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);

        let script = format!(
            r#"let r = http_patch_all([
                #{{url: "http://{addr}/1", body: "{{}}"}},
                #{{url: "http://{addr}/2", body: "{{}}"}},
            ]);
            r[0].status + "," + r[1].status"#
        );
        let statuses: String = e.eval(&script).unwrap();
        assert_eq!(statuses, "200,200", "dry-run must synthesize a 200 per element");
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(300)).is_err(),
            "no element should ever have made a real connection under --dry-run"
        );
    }

    #[test]
    fn http_patch_all_rejects_a_malformed_element_naming_the_index() {
        let ctx = shared(FakeRunner::shared());
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);
        let err = e
            .eval::<Array>(
                r#"http_patch_all([#{url: "http://x/", body: "{}"}, #{body: "{}"}])"#,
            )
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("requests[1]"), "must name the offending index: {msg}");
        assert!(msg.contains("url"), "must name the missing key: {msg}");
    }

    /// A listener that never answers (accepts and blocks) — used to prove a dry-run
    /// `http_patch_all` call never actually connects. `recv_timeout` on the returned channel
    /// times out cleanly if (and only if) no connection was ever made.
    fn spawn_bunny_probe_listener() -> (std::net::SocketAddr, std::sync::mpsc::Receiver<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if listener.accept().is_ok() {
                let _ = tx.send(());
            }
        });
        (addr, rx)
    }
}
