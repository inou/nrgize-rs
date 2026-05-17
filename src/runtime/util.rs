//! Starlark built-in utility functions: sleep, secret, env, print helpers.

use starlark::environment::GlobalsBuilder;
use starlark::values::none::NoneType;
use starlark::values::Heap;
use starlark::values::Value;

/// Register utility built-in functions into the Starlark global environment.
#[starlark::starlark_module]
pub fn util_builtins(builder: &mut GlobalsBuilder) {
    /// Sleep for the given number of seconds.
    ///
    /// Accepts fractional seconds. Primarily used in health check polling loops.
    ///
    /// Example:
    ///   for attempt in range(10):
    ///       r = http_get("http://host:3000/up")
    ///       if r.ok:
    ///           break
    ///       sleep(2)
    fn sleep(seconds: i32) -> anyhow::Result<NoneType> {
        if seconds < 0 {
            return Err(anyhow::anyhow!("sleep() seconds must be non-negative, got {}", seconds));
        }
        if seconds > 3600 {
            return Err(anyhow::anyhow!(
                "sleep() seconds must be <= 3600 (1 hour), got {}",
                seconds
            ));
        }
        std::thread::sleep(std::time::Duration::from_secs(seconds as u64));
        Ok(NoneType)
    }

    /// Get an environment variable by name. Fails if the variable is not set.
    ///
    /// Example:
    ///   tag = nrg_env("DEPLOY_TAG")
    #[starlark(speculative_exec_safe)]
    fn nrg_env<'v>(
        name: &str,
        heap: &'v Heap,
    ) -> anyhow::Result<Value<'v>> {
        match std::env::var(name) {
            Ok(val) => Ok(heap.alloc(val.as_str())),
            Err(_) => Err(anyhow::anyhow!(
                "Environment variable '{}' is not set",
                name
            )),
        }
    }

    /// Get an environment variable by name, or return a default if not set.
    ///
    /// Example:
    ///   tag = env_or("DEPLOY_TAG", "latest")
    #[starlark(speculative_exec_safe)]
    fn env_or<'v>(
        name: &str,
        default: &str,
        heap: &'v Heap,
    ) -> anyhow::Result<Value<'v>> {
        let val = std::env::var(name).unwrap_or_else(|_| default.to_string());
        Ok(heap.alloc(val.as_str()))
    }

    /// Access a secret by name from the secrets store.
    ///
    /// Secrets are loaded from:
    ///   1. Environment variables (NRG_SECRET_<NAME>)
    ///   2. .energize/secrets file (KEY=VALUE format)
    ///   3. .env file (KEY=VALUE format)
    ///
    /// Fails if the secret is not found in any source.
    ///
    /// Example:
    ///   ssh_exec(host, "docker login -u user -p " + secret("REGISTRY_PASSWORD"))
    #[starlark(speculative_exec_safe)]
    fn secret<'v>(
        name: &str,
        heap: &'v Heap,
    ) -> anyhow::Result<Value<'v>> {
        // Strategy 1: Check environment variable with NRG_SECRET_ prefix
        let env_key = format!("NRG_SECRET_{}", name.to_uppercase());
        if let Ok(val) = std::env::var(&env_key) {
            return Ok(heap.alloc(val.as_str()));
        }

        // Strategy 2: Check environment variable directly
        if let Ok(val) = std::env::var(name) {
            return Ok(heap.alloc(val.as_str()));
        }

        // Strategy 3: Check .energize/secrets file
        if let Some(val) = load_secret_from_file(".energize/secrets", name) {
            return Ok(heap.alloc(val.as_str()));
        }

        // Strategy 4: Check .env file
        if let Some(val) = load_secret_from_file(".env", name) {
            return Ok(heap.alloc(val.as_str()));
        }

        Err(anyhow::anyhow!(
            "Secret '{}' not found. Checked: ${}, ${}, .energize/secrets, .env",
            name,
            env_key,
            name
        ))
    }
}

/// Load a secret from a KEY=VALUE file. Returns None if file doesn't exist or key not found.
fn load_secret_from_file(path: &str, key: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let line = line.trim();
        // Skip comments and empty lines
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let v = v.trim();
            // Strip surrounding quotes if present
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(v);
            if k == key {
                return Some(v.to_string());
            }
        }
    }
    None
}
