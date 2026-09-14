//! Checked, named execution with explicit options. No command semantics are inferred.
use super::exec::{synthetic_ok, to_result};
use crate::engine::{
    context::SharedCtx, diagnostics, runner::RunOptions, secret::assert_no_secret_leak,
    types::ExecResult,
};
use rhai::{Engine, EvalAltResult, Map};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Options {
    cwd: Option<String>,
    env: BTreeMap<String, String>,
    stdin: String,
    timeout_secs: Option<u64>,
    stream: bool,
    retries: u32,
    retry_delay_ms: u64,
    idempotent: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            cwd: None,
            env: BTreeMap::new(),
            stdin: String::new(),
            timeout_secs: None,
            stream: true,
            retries: 0,
            retry_delay_ms: 1000,
            idempotent: false,
        }
    }
}

fn run(
    ctx: &SharedCtx,
    host: Option<&str>,
    name: &str,
    cmd: &str,
    map: Map,
) -> Result<ExecResult, Box<EvalAltResult>> {
    let options: Options = rhai::serde::from_dynamic(&map.into()).map_err(|e| e.to_string())?;
    if options.timeout_secs == Some(0) || options.retries > 10 || options.retry_delay_ms > 60_000 {
        return Err("timeout_secs must be positive; retries <= 10; retry_delay_ms <= 60000".into());
    }
    if options.retries > 0 && !options.idempotent {
        return Err("retries require idempotent: true; nrg cannot determine whether a command is safe to repeat".into());
    }
    for key in options.env.keys() {
        let mut chars = key.chars();
        if !chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err("invalid environment variable name".into());
        }
    }
    for text in [name, cmd, host.unwrap_or("")]
        .into_iter()
        .chain(options.cwd.as_deref())
        .chain(options.env.values().map(String::as_str))
    {
        assert_no_secret_leak(text)?;
        if text.contains('\0') {
            return Err("NUL is not supported in command, path or environment values".into());
        }
    }
    // Environment and stdin are explicitly off-argv channels; register their values before
    // streaming in case a command echoes them. They never appear in plans or journal events.
    for value in options.env.values().chain(std::iter::once(&options.stdin)) {
        if !value.is_empty() {
            ctx.register_secret(value);
        }
    }
    let operation = if host.is_some() { "ssh" } else { "local" };
    if ctx.is_dry_run() {
        ctx.record(
            operation,
            host,
            format!(
                "{name}: {cmd} (cwd={}, timeout={:?}, retries={}; env/stdin omitted)",
                options.cwd.as_deref().unwrap_or("inherited"),
                options.timeout_secs,
                options.retries
            ),
        );
        return Ok(synthetic_ok(host.unwrap_or("")));
    }
    let spec = RunOptions {
        cwd: options.cwd,
        env: options.env,
        stdin: options.stdin,
        timeout_secs: options.timeout_secs,
        stream: options.stream,
    };
    let secrets: Vec<_> = ctx.secrets.lock().unwrap().iter().cloned().collect();
    for attempt in 0..=options.retries {
        ctx.check_interrupt()?;
        eprintln!(
            "[nrg] {}",
            ctx.redacted(&format!(
                "step {name} on {} ({operation}, attempt {})",
                host.unwrap_or("local"),
                attempt + 1
            ))
        );
        let started = diagnostics::begin(ctx, name, host.unwrap_or(""), operation);
        let result = to_result(
            host.unwrap_or(""),
            ctx.runner.run_configured(host, cmd, &spec, &secrets),
        );
        let message = diagnostics::record_started(ctx, name, operation, &result, started);
        ctx.check_interrupt()?;
        if result.exit_code == 0 {
            return Ok(result);
        }
        if attempt == options.retries {
            return Err(message.into());
        }
        let start = std::time::Instant::now();
        while start.elapsed().as_millis() < u128::from(options.retry_delay_ms) {
            ctx.check_interrupt()?;
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    unreachable!()
}

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    let c = ctx.clone();
    engine.register_fn("local_step", move |name: &str, cmd: &str| {
        run(&c, None, name, cmd, Map::new())
    });
    let c = ctx.clone();
    engine.register_fn("local_step", move |name: &str, cmd: &str, options: Map| {
        run(&c, None, name, cmd, options)
    });
    let c = ctx.clone();
    engine.register_fn("ssh_step", move |host: &str, name: &str, cmd: &str| {
        run(&c, Some(host), name, cmd, Map::new())
    });
    engine.register_fn(
        "ssh_step",
        move |host: &str, name: &str, cmd: &str, options: Map| {
            run(&ctx, Some(host), name, cmd, options)
        },
    );
}
