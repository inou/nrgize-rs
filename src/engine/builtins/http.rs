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
