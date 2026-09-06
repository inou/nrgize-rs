//! Command-execution builtins. Effect classification is BY BUILTIN:
//! `ssh_exec`/`local_exec`/`ssh_exec_all` are MUTATING; `ssh_probe` is READ-ONLY.
//! (Phase 3 uses this distinction for dry-run.)
//!
//! Every mutating command-effect funnels through one of the `effect*` helpers below, which own
//! the invariant trio: (1) reject a stringified `Secret` that leaked into the command, (2) trace
//! it (redacted), and (3) in dry-run record a `PlannedAction` (+ apply a sim mutation) and return
//! a synthetic ok INSTEAD of executing. Centralizing this means "dry-run can't execute" and "a
//! Secret can't reach a host as text" are structurally guaranteed, not re-implemented per builtin.

use crate::engine::context::{EffectMode, RunCtx, SharedCtx};
use crate::engine::runner::RawOutput;
use crate::engine::secret::assert_no_secret_leak;
use crate::engine::types::ExecResult;
use rhai::{Array, Dynamic, Engine, EvalAltResult};
use std::thread;

pub fn to_result(host: &str, raw: RawOutput) -> ExecResult {
    ExecResult {
        stdout: raw.stdout,
        stderr: raw.stderr,
        exit_code: raw.exit_code,
        host: host.to_string(),
    }
}

/// A synthetic success result for a dry-run mutation (no command ran).
pub fn synthetic_ok(host: &str) -> ExecResult {
    ExecResult {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        host: host.to_string(),
    }
}

/// Pre-flight a command before it executes or is recorded: reject a leaked `Secret`, then emit a
/// redacted trace line. Shared by every command-effect (mutating and read-only).
fn precheck(ctx: &RunCtx, label: &str, cmd: &str) -> Result<(), Box<EvalAltResult>> {
    assert_no_secret_leak(cmd)?;
    if ctx.trace {
        eprintln!("[nrg] {}", ctx.redacted(&format!("{label} -> {cmd}")));
    }
    Ok(())
}

/// The single dry-run dispatch for a MUTATING command-effect. Prechecks the command, then either
/// records it (dry-run: apply `sim`, return synthetic ok) or runs it (`real`). `host` is `None`
/// for local effects (recorded with no host; synthetic result carries an empty host).
pub(crate) fn effect(
    ctx: &RunCtx,
    kind: &str,
    host: Option<&str>,
    label: &str,
    cmd: &str,
    sim: impl FnOnce(&RunCtx),
    real: impl FnOnce(&RunCtx) -> ExecResult,
) -> Result<ExecResult, Box<EvalAltResult>> {
    precheck(ctx, label, cmd)?;
    if ctx.mode == EffectMode::DryRun {
        ctx.record(kind, host, cmd.to_string());
        sim(ctx);
        return Ok(synthetic_ok(host.unwrap_or("")));
    }
    Ok(real(ctx))
}

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "remote_lock_acquire",
            move |host: &str, directory: &str| -> Result<(), Box<EvalAltResult>> {
                if ctx.is_dry_run() {
                    ctx.record(
                        "lock",
                        Some(host),
                        format!("mkdir {directory} (owned deploy lock)"),
                    );
                    return Ok(());
                }
                let held = crate::engine::remote_lock::RemoteLock::acquire(
                    ctx.runner.clone(),
                    host,
                    directory,
                )?;
                ctx.remote_locks.lock().unwrap().push(held);
                Ok(())
            },
        );
    }
    {
        let ctx = ctx.clone();
        engine.register_fn("remote_lock_release", move |host: &str, directory: &str| {
            if ctx.is_dry_run() {
                ctx.record(
                    "lock",
                    Some(host),
                    format!("rmdir {directory} (owned deploy lock)"),
                );
            }
            ctx.remote_locks
                .lock()
                .unwrap()
                .retain(|l| l.host != host || l.directory != directory);
        });
    }
    // ssh_exec — MUTATING remote command.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "ssh_exec",
            move |host: &str, cmd: &str| -> Result<ExecResult, Box<EvalAltResult>> {
                effect(
                    &ctx,
                    "ssh",
                    Some(host),
                    &format!("ssh_exec {host}"),
                    cmd,
                    |_| {},
                    |c| to_result(host, c.runner.run_ssh(host, cmd)),
                )
            },
        );
    }

    // ssh_probe — READ-ONLY remote command (still executes in dry-run, Phase 3).
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "ssh_probe",
            move |host: &str, cmd: &str| -> Result<ExecResult, Box<EvalAltResult>> {
                precheck(&ctx, &format!("ssh_probe {host}"), cmd)?;
                Ok(to_result(host, ctx.runner.run_ssh(host, cmd)))
            },
        );
    }

    // local_exec — MUTATING local command.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "local_exec",
            move |cmd: &str| -> Result<ExecResult, Box<EvalAltResult>> {
                effect(
                    &ctx,
                    "local",
                    None,
                    "local_exec",
                    cmd,
                    |_| {},
                    |c| to_result("", c.runner.run_local(cmd)),
                )
            },
        );
    }

    // ssh_exec_all — parallel fan-out across hosts. Never aborts on single-host failure.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "ssh_exec_all",
            move |hosts: Array, cmd: &str| -> Result<Array, Box<EvalAltResult>> {
                precheck(&ctx, "ssh_exec_all", cmd)?;
                // Reject non-string host elements loudly rather than silently coercing a
                // typo'd / wrong-typed entry to "" (which would run `ssh ""`).
                let mut host_strs: Vec<String> = Vec::with_capacity(hosts.len());
                for (i, h) in hosts.iter().enumerate() {
                    match h.clone().into_string() {
                        Ok(s) => host_strs.push(s),
                        Err(ty) => {
                            return Err(format!(
                                "ssh_exec_all: host[{i}] must be a string, got {ty}"
                            )
                            .into())
                        }
                    }
                }
                let cmd = cmd.to_string();
                if ctx.mode == EffectMode::DryRun {
                    for h in &host_strs {
                        ctx.record("ssh-all", Some(h), cmd.clone());
                    }
                    return Ok(host_strs
                        .into_iter()
                        .map(|h| Dynamic::from(synthetic_ok(&h)))
                        .collect());
                }
                let runner = ctx.runner.clone();
                let mut results = Vec::new();
                for chunk in host_strs.chunks(16) {
                    let batch: Vec<ExecResult> = thread::scope(|s| {
                        let handles: Vec<_> = chunk
                            .iter()
                            .map(|h| {
                                let runner = runner.clone();
                                let cmd = cmd.clone();
                                let h = h.clone();
                                s.spawn(move || to_result(&h, runner.run_ssh(&h, &cmd)))
                            })
                            .collect();
                        handles
                            .into_iter()
                            .zip(chunk.iter())
                            .map(|(j, h)| {
                                // On a thread panic, attribute it to the right host (don't lose the
                                // host name, which the stdlib reports during an incident).
                                j.join().unwrap_or_else(|_| ExecResult {
                                    stdout: String::new(),
                                    stderr: "thread panicked".into(),
                                    exit_code: -1,
                                    host: h.clone(),
                                })
                            })
                            .collect()
                    });
                    results.extend(batch);
                }
                Ok(results.into_iter().map(Dynamic::from).collect())
            },
        );
    }

    // ssh_exec_stdin(host, cmd, stdin) — MUTATING; delivers `stdin` off-argv (e.g. a password
    // to `docker login --password-stdin`). The payload is NEVER traced or put on argv.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "ssh_exec_stdin",
            move |host: &str, cmd: &str, stdin: &str| -> Result<ExecResult, Box<EvalAltResult>> {
                effect(
                    &ctx,
                    "ssh-stdin",
                    Some(host),
                    &format!("ssh_exec_stdin {host} (stdin {} bytes)", stdin.len()),
                    cmd,
                    |_| {},
                    |c| to_result(host, c.runner.run_ssh_stdin(host, cmd, stdin)),
                )
            },
        );
    }

    // local_exec_stdin(cmd, stdin) — MUTATING local mirror.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "local_exec_stdin",
            move |cmd: &str, stdin: &str| -> Result<ExecResult, Box<EvalAltResult>> {
                effect(
                    &ctx,
                    "local-stdin",
                    None,
                    &format!("local_exec_stdin (stdin {} bytes)", stdin.len()),
                    cmd,
                    |_| {},
                    |c| to_result("", c.runner.run_local_stdin(cmd, stdin)),
                )
            },
        );
    }

    // write_remote(host, content, remote_path) — MUTATING; writes content to a 0600 remote file
    // via the stdin channel (content never on argv). For secret env-files, configs, etc.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "write_remote",
            move |host: &str, content: &str, remote_path: &str| -> Result<ExecResult, Box<EvalAltResult>> {
                let cmd = format!(
                    "umask 077; dest={}; [ ! -L \"$dest\" ] && [ ! -d \"$dest\" ] || exit 1; tmp=$(mktemp \"${{dest}}.XXXXXXXXXX\") || exit 1; trap 'rm -f \"$tmp\"' EXIT HUP INT TERM; cat > \"$tmp\" && mv -f \"$tmp\" \"$dest\"",
                    crate::engine::secret::posix_quote(remote_path)
                );
                // The content body is delivered off-argv; only the destination path is in `cmd`.
                // Guard the path for a leaked Secret, but trace the byte count (never the body).
                assert_no_secret_leak(&cmd)?;
                if ctx.trace {
                    eprintln!("[nrg] {}", ctx.redacted(&format!("write_remote {host} -> {remote_path} ({} bytes)", content.len())));
                }
                if ctx.mode == EffectMode::DryRun {
                    ctx.record(
                        "write",
                        Some(host),
                        format!("write {} bytes -> {remote_path}", content.len()),
                    );
                    return Ok(synthetic_ok(host));
                }
                Ok(to_result(host, ctx.runner.run_ssh_stdin(host, &cmd, content)))
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::{shared, shared_dry};
    use crate::engine::runner::FakeRunner;
    use crate::engine::types::register_types;

    fn engine_with(ctx: SharedCtx) -> Engine {
        let mut e = Engine::new();
        register_types(&mut e);
        register(&mut e, ctx);
        e
    }

    #[test]
    fn ssh_exec_runs_through_runner_and_returns_ok() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        let ok: bool = e.eval(r#"ssh_exec("web1", "uptime").ok"#).unwrap();
        assert!(ok);
        assert_eq!(fake.calls(), vec!["ssh web1: uptime".to_string()]);
    }

    #[test]
    fn ssh_exec_all_fans_out_to_every_host() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        let n: i64 = e
            .eval(r#"ssh_exec_all(["a","b","c"], "docker pull x").len()"#)
            .unwrap();
        assert_eq!(n, 3);
        assert_eq!(fake.calls().len(), 3);
    }

    #[test]
    fn ssh_exec_stdin_keeps_payload_off_argv() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        e.run(r#"ssh_exec_stdin("web1", "docker login -u u --password-stdin", "topsecretpw");"#)
            .unwrap();
        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        let (argv, stdin) = calls[0].split_once("<<<").unwrap();
        assert!(argv.contains("docker login -u u --password-stdin"));
        assert!(!argv.contains("topsecretpw"), "payload must not be on argv");
        assert!(stdin.contains("topsecretpw"), "payload must be on stdin");
    }

    #[test]
    fn write_remote_uses_stdin_not_argv() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        e.run(r#"write_remote("web1", "SECRET=abc123", "/run/app.env");"#)
            .unwrap();
        let calls = fake.calls();
        assert!(calls[0].contains("umask 077; dest='/run/app.env'"));
        let (argv, stdin) = calls[0].split_once("<<<").unwrap();
        assert!(!argv.contains("abc123"), "content must not be on argv");
        assert!(stdin.contains("SECRET=abc123"), "content must be on stdin");
    }

    #[test]
    fn write_remote_records_in_dry_run() {
        let fake = FakeRunner::shared();
        let ctx = shared_dry(fake.clone());
        let e = engine_with(ctx.clone());
        e.run(r#"write_remote("web1", "BIG=body", "/run/app.env");"#)
            .unwrap();
        assert!(fake.calls().is_empty());
        let plan = ctx.plan.lock().unwrap().clone();
        assert_eq!(plan[0].kind, "write");
        assert!(plan[0].detail.contains("/run/app.env"));
    }

    #[test]
    fn trace_redacts_registered_secret() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        ctx.register_secret("supersecretvalue");
        assert_eq!(
            ctx.redacted("docker login -p supersecretvalue"),
            "docker login -p ***"
        );
    }

    #[test]
    fn dry_run_records_instead_of_executing() {
        let fake = FakeRunner::shared();
        let ctx = shared_dry(fake.clone());
        let e = engine_with(ctx.clone());
        let ok: bool = e.eval(r#"ssh_exec("web1", "rm -rf /data").ok"#).unwrap();
        assert!(ok); // synthetic ok
        assert!(fake.calls().is_empty(), "dry-run must not execute");
        let plan = ctx.plan.lock().unwrap().clone();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].kind, "ssh");
        assert_eq!(plan[0].detail, "rm -rf /data");
    }

    #[test]
    fn ssh_exec_all_rejects_non_string_host() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        // A numeric host element must abort, not silently become `ssh ""`.
        let r = e.eval::<rhai::Array>(r#"ssh_exec_all(["a", 42], "uptime")"#);
        assert!(r.is_err());
        assert!(
            fake.calls().is_empty(),
            "must not run any ssh before validation"
        );
    }

    #[test]
    fn ssh_exec_all_reports_per_host_failure_with_correct_attribution() {
        // Partial-fleet failure (issue #27): host "b" fails; the result list must stay in HOST
        // ORDER and the failed entry must carry host "b" (so the stdlib reports the right host).
        let fake = FakeRunner::shared();
        fake.fail_host("b", 1, "boom on b");
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        let r: rhai::Array = e
            .eval(r#"ssh_exec_all(["a","b","c"], "do thing")"#)
            .unwrap();
        assert_eq!(r.len(), 3);
        let got: Vec<(String, bool)> = r
            .into_iter()
            .map(|d| {
                let er = d.cast::<ExecResult>();
                (er.host.clone(), er.exit_code == 0)
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("a".to_string(), true),
                ("b".to_string(), false),
                ("c".to_string(), true)
            ],
            "results must be in host order with b failing and correctly attributed"
        );
    }

    #[test]
    fn ssh_probe_reads_through_runner() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        let out: String = e.eval(r#"ssh_probe("web1", "docker ps").host"#).unwrap();
        assert_eq!(out, "web1");
        assert_eq!(fake.calls(), vec!["ssh web1: docker ps".to_string()]);
    }

    #[test]
    fn interpolated_secret_is_rejected_before_executing() {
        // A `${secret(...)}` in a command stringifies to the sentinel; the exec boundary must
        // throw rather than run a command with a wrong value.
        // Robustness review: "Flaky patterns" — serialize against every other env-mutating test
        // in this binary (parallel test threads + set_var/getenv racing is UB-adjacent on glibc).
        let _env_guard = crate::test_support::lock_env();
        std::env::set_var("NRG_SECRET_LEAK", "leakedvalue");
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        let mut e = engine_with(ctx);
        crate::engine::secret::register(&mut e, crate::engine::context::shared(fake.clone()));
        let r = e.run(r#"ssh_exec("web1", `docker login -p ${secret("LEAK")}`);"#);
        assert!(r.is_err(), "leaked secret must throw");
        assert!(
            fake.calls().is_empty(),
            "must not execute a command with a leaked secret"
        );
        std::env::remove_var("NRG_SECRET_LEAK");
    }
}
