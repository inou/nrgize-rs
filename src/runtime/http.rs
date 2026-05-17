//! Starlark built-in functions for HTTP requests.
//!
//! These are primarily used for health checks during deployment, but are
//! general-purpose enough for any HTTP interaction.

use crate::runtime::types::HttpResponse;
use starlark::environment::GlobalsBuilder;
use starlark::values::Heap;
use starlark::values::Value;

/// Build a ureq Agent with a 30-second global timeout.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .new_agent()
}

/// Register HTTP built-in functions into the Starlark global environment.
#[starlark::starlark_module]
pub fn http_builtins(builder: &mut GlobalsBuilder) {
    /// Perform an HTTP GET request and return the response.
    ///
    /// Returns an HttpResponse with status, body, and ok attributes.
    /// Timeouts after 30 seconds by default. Connection errors result in status=-1.
    ///
    /// Example:
    ///   r = http_get("http://10.0.0.1:3000/up")
    ///   if r.status == 200:
    ///       print("healthy")
    fn http_get<'v>(
        url: &str,
        heap: &'v Heap,
    ) -> anyhow::Result<Value<'v>> {
        let trace = std::env::var("NRG_TRACE").is_ok();
        if trace {
            eprintln!("[nrg] http_get -> {}", url);
        }

        let a = agent();
        let resp = match a.get(url).call() {
            Ok(resp) => {
                let status = resp.status().as_u16() as i32;
                let body = resp.into_body().read_to_string().unwrap_or_default();
                HttpResponse { status, body }
            }
            Err(ureq::Error::StatusCode(code)) => {
                // Server responded with an error status — still a valid response
                HttpResponse {
                    status: code as i32,
                    body: String::new(),
                }
            }
            Err(e) => {
                // Connection error, timeout, DNS failure, etc.
                HttpResponse {
                    status: -1,
                    body: format!("Request failed: {}", e),
                }
            }
        };

        if trace {
            eprintln!("[nrg]   status={} body_len={}", resp.status, resp.body.len());
        }

        Ok(heap.alloc(resp))
    }

    /// Perform an HTTP POST request with a string body and return the response.
    ///
    /// The content type defaults to "application/json".
    ///
    /// Example:
    ///   r = http_post("http://deploy-webhook.example.com/notify", '{"status": "deployed"}')
    fn http_post<'v>(
        url: &str,
        body: &str,
        heap: &'v Heap,
    ) -> anyhow::Result<Value<'v>> {
        let trace = std::env::var("NRG_TRACE").is_ok();
        if trace {
            eprintln!("[nrg] http_post -> {} (body_len={})", url, body.len());
        }

        let a = agent();
        let resp = match a.post(url)
            .header("Content-Type", "application/json")
            .send(body.as_bytes())
        {
            Ok(resp) => {
                let status = resp.status().as_u16() as i32;
                let resp_body = resp.into_body().read_to_string().unwrap_or_default();
                HttpResponse {
                    status,
                    body: resp_body,
                }
            }
            Err(ureq::Error::StatusCode(code)) => HttpResponse {
                status: code as i32,
                body: String::new(),
            },
            Err(e) => HttpResponse {
                status: -1,
                body: format!("Request failed: {}", e),
            },
        };

        if trace {
            eprintln!("[nrg]   status={} body_len={}", resp.status, resp.body.len());
        }

        Ok(heap.alloc(resp))
    }
}
