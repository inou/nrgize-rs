//! HTTP builtins (read-class; used by health checks). Uses ureq with a 30s timeout.

use crate::engine::context::SharedCtx;
use crate::engine::types::HttpResponse;
use rhai::Engine;

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
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

fn do_get(url: &str) -> HttpResponse {
    match agent().get(url).call() {
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
    match agent()
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
            let r = do_get(url);
            if ctx.mode == EffectMode::DryRun {
                ctx.record("check", None, format!("GET {url} -> {} (probed live)", r.status));
            }
            r
        });
    }
    // sim_http_healthy(url) — the NEW-container health probe used by wait_healthy. In dry-run the
    // new container isn't running yet, so a real probe of its (symbolic) port would always fail;
    // we short-circuit to a synthetic healthy 200 and record a 'check'. Live: a real GET.
    {
        let ctx = ctx.clone();
        engine.register_fn("sim_http_healthy", move |url: &str| -> HttpResponse {
            if ctx.mode == EffectMode::DryRun {
                ctx.record("check", None, format!("[assumed healthy] GET {url}"));
                return HttpResponse { status: 200, body: String::new() };
            }
            do_get(url)
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
