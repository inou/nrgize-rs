//! HTTP builtins (read-class; used by health checks). Uses ureq with a 30s timeout.

use crate::engine::context::SharedCtx;
use crate::engine::types::HttpResponse;
use rhai::Engine;

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
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
        Err(ureq::Error::StatusCode(code)) => HttpResponse {
            status: code as i64,
            body: String::new(),
        },
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
        Err(ureq::Error::StatusCode(code)) => HttpResponse {
            status: code as i64,
            body: String::new(),
        },
        Err(e) => HttpResponse {
            status: 0,
            body: format!("request failed: {e}"),
        },
    }
}

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    use crate::engine::context::EffectMode;
    // http_get — in dry-run, short-circuit to synthetic healthy (200) and record a check, so a
    // wait_healthy loop against a not-yet-started container doesn't fail/hang the plan.
    {
        let ctx = ctx.clone();
        engine.register_fn("http_get", move |url: &str| -> HttpResponse {
            if ctx.lock().unwrap().mode == EffectMode::DryRun {
                ctx.lock()
                    .unwrap()
                    .record("check", None, format!("[assumed healthy] GET {url}"));
                return HttpResponse {
                    status: 200,
                    body: String::new(),
                };
            }
            do_get(url)
        });
    }
    {
        let ctx = ctx.clone();
        engine.register_fn("http_post", move |url: &str, body: &str| -> HttpResponse {
            if ctx.lock().unwrap().mode == EffectMode::DryRun {
                ctx.lock()
                    .unwrap()
                    .record("check", None, format!("[assumed ok] POST {url}"));
                return HttpResponse {
                    status: 200,
                    body: String::new(),
                };
            }
            do_post(url, body)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::shared;
    use crate::engine::runner::FakeRunner;

    #[test]
    fn http_builtins_register() {
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, shared(FakeRunner::shared()));
        // Assert the symbols exist by compiling a script that references them.
        assert!(e
            .compile(r#"fn _f(){ http_get("http://x"); http_post("http://x","{}"); }"#)
            .is_ok());
    }

    #[test]
    fn http_get_short_circuits_in_dry_run() {
        use crate::engine::context::EffectMode;
        let ctx = shared(FakeRunner::shared());
        ctx.lock().unwrap().mode = EffectMode::DryRun;
        let mut e = Engine::new();
        crate::engine::types::register_types(&mut e);
        register(&mut e, ctx);
        // An unreachable URL would error in Live mode; dry-run returns synthetic healthy 200.
        let ok: bool = e.eval(r#"http_get("http://127.0.0.1:1/never").ok"#).unwrap();
        assert!(ok);
    }
}
