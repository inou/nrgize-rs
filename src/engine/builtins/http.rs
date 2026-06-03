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

pub fn register(engine: &mut Engine, _ctx: SharedCtx) {
    engine.register_fn("http_get", |url: &str| -> HttpResponse { do_get(url) });
    engine.register_fn("http_post", |url: &str, body: &str| -> HttpResponse {
        do_post(url, body)
    });
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
}
