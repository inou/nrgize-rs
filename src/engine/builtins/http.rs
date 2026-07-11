//! HTTP builtins (read-class; used by health checks). Uses ureq; timeout defaults to 30s but is
//! configurable per-call (robustness review R12) since `sim_http_healthy`'s caller (`wait_healthy`)
//! runs it in a bounded retry loop — a fixed 30s timeout unrelated to that loop's own `interval`
//! meant a hanging endpoint could make `attempts: 30` (intended as a ~1 minute budget at the
//! default `interval: 2`) actually take up to 30 * 30s = 15 minutes.

use crate::engine::context::SharedCtx;
use crate::engine::types::HttpResponse;
use rhai::Engine;

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

fn do_get(url: &str, timeout_secs: i64) -> HttpResponse {
    match agent(timeout_secs).get(url).call() {
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

fn do_post(url: &str, body: &str) -> HttpResponse {
    match agent(DEFAULT_HTTP_TIMEOUT_SECS)
        .post(url)
        .header("Content-Type", "application/json")
        .send(body.as_bytes())
    {
        Ok(resp) => {
            let status = resp.status().as_u16() as i64;
            let rbody = resp.into_body().read_to_string().unwrap_or_default();
            HttpResponse {
                status,
                body: rbody,
            }
        }
        Err(e) => HttpResponse {
            status: 0,
            body: format!("request failed: {e}"),
        },
    }
}

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    use crate::engine::context::EffectMode;
    // http_get — a READ of EXISTING reality. It executes FOR REAL even in dry-run (a GET has no
    // side effect), so a script that gates the plan on current prod health —
    // `if !http_get(prod_url).ok { throw }` — sees the truth, not a synthetic 200 (issue #16).
    // The plan records the probed status. The NOT-yet-started new container is checked via
    // `sim_http_healthy` (below), which the stdlib's wait_healthy uses instead.
    {
        let ctx = ctx.clone();
        engine.register_fn("http_get", move |url: &str| -> HttpResponse {
            let r = do_get(url, DEFAULT_HTTP_TIMEOUT_SECS);
            if ctx.mode == EffectMode::DryRun {
                ctx.record("check", None, format!("GET {url} -> {} (probed live)", r.status));
            }
            r
        });
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
            do_get(url, timeout_secs)
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
            do_get(url, DEFAULT_HTTP_TIMEOUT_SECS)
        });
    }
    {
        let ctx = ctx.clone();
        engine.register_fn("http_post", move |url: &str, body: &str| -> HttpResponse {
            // POST is a WRITE — never execute it in dry-run; record + synthetic ok.
            if ctx.mode == EffectMode::DryRun {
                ctx.record("check", None, format!("[assumed ok] POST {url}"));
                return HttpResponse { status: 200, body: String::new() };
            }
            do_post(url, body)
        });
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
                r#"fn _f(){ http_get("http://x"); http_post("http://x","{}"); sim_http_healthy("http://x"); }"#
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

    /// Bind an ephemeral localhost listener that accepts exactly one connection, discards
    /// whatever the client sends, writes `response` verbatim, then exits — a minimal real HTTP
    /// server standing in for a health-check endpoint, so these tests exercise the REAL `ureq`
    /// round trip (status parsing, body extraction) instead of only ever hitting unreachable
    /// URLs. Returns the address to connect to.
    fn spawn_http_responder(response: &'static str) -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf); // drain the request so the client's write completes
                let _ = stream.write_all(response.as_bytes());
            }
        });
        addr
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
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut stream, _)) = listener.accept() {
                // Headers and body can arrive as separate TCP segments, so a single fixed-size
                // `read()` may only capture the headers — keep reading (bounded by a short
                // per-read timeout) until the client stops sending.
                stream.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
                let mut received = Vec::new();
                let mut buf = [0u8; 1024];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => received.extend_from_slice(&buf[..n]),
                        Err(_) => break, // timed out waiting for more — client is done sending
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&received).into_owned());
                let _ = stream.write_all(
                    b"HTTP/1.1 201 Created\r\nContent-Length: 8\r\nConnection: close\r\n\r\naccepted",
                );
            }
        });

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
}
