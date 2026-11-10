//! Command-execution builtins. Effect classification is BY BUILTIN:
//! `ssh_exec`/`local_exec`/`ssh_exec_all` are MUTATING; `ssh_probe` is READ-ONLY.
//! (Dry-run uses this distinction.)
//!
//! Every mutating builtin funnels through the helpers below, which own the invariant trio: (1)
//! reject a stringified `Secret` that leaked into the command, (2) trace it (redacted), and (3) in
//! dry-run record a `PlannedAction` (+ apply a sim mutation) and return a synthetic ok INSTEAD of
//! executing. `dispatch` is the one place a single-host effect branches on the effect mode;
//! `effect` (command effects), `effect_fanout` (one command on many hosts) and the `write_remote`
//! / transfer builtins are thin callers of it, and `dry_run_record` / `dry_run_each` are the guard
//! for the record-only mutations (locks, HTTP writes, state, rollback registration, steps).
//! Centralizing this means "dry-run can't execute" and "a Secret can't reach a host as text" are
//! structurally guaranteed, not re-implemented per builtin.

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

/// Reject a leaked `Secret` in `cmd`, then emit a redacted trace line (built lazily, only when
/// tracing). The trace text is separate from `cmd` for effects whose trace must not echo the
/// command-adjacent payload (e.g. `write_remote` traces a byte count).
pub(crate) fn precheck_with(
    ctx: &RunCtx,
    cmd: &str,
    trace_line: impl FnOnce() -> String,
) -> Result<(), Box<EvalAltResult>> {
    assert_no_secret_leak(cmd)?;
    if ctx.trace {
        eprintln!("[nrg] {}", ctx.redacted(&trace_line()));
    }
    Ok(())
}

/// Pre-flight a command before it executes or is recorded: reject a leaked `Secret`, then emit a
/// redacted `label -> cmd` trace line. Shared by every command-effect (mutating and read-only).
fn precheck(ctx: &RunCtx, label: &str, cmd: &str) -> Result<(), Box<EvalAltResult>> {
    precheck_with(ctx, cmd, || format!("{label} -> {cmd}"))
}

/// The dry-run guard for a mutation that only needs RECORDING (no secret precheck, no
/// diagnostics): in dry-run, records `detail` and returns `true` so the caller skips the live
/// effect; in a live run, returns `false` and builds nothing.
pub(crate) fn dry_run_record(
    ctx: &RunCtx,
    kind: &str,
    host: Option<&str>,
    detail: impl FnOnce() -> String,
) -> bool {
    if ctx.mode != EffectMode::DryRun {
        return false;
    }
    ctx.record(kind, host, detail());
    true
}

/// The dry-run guard for a batch of record-only mutations: in dry-run, maps every item through
/// `each` (which records) and returns `Ok(results)`; in a live run, hands the items back as `Err`.
pub(crate) fn dry_run_each<T, R>(
    ctx: &RunCtx,
    items: Vec<T>,
    each: impl FnMut(T) -> R,
) -> Result<Vec<R>, Vec<T>> {
    if ctx.mode != EffectMode::DryRun {
        return Err(items);
    }
    Ok(items.into_iter().map(each).collect())
}

/// How a single-host mutation is recorded (dry-run) and journaled (live).
pub(crate) struct Dispatch<'a> {
    /// `PlannedAction` kind recorded in dry-run.
    pub kind: &'a str,
    pub host: Option<&'a str>,
    /// `PlannedAction` detail recorded in dry-run (never contains a payload body).
    pub detail: &'a str,
    /// Diagnostics name / operation for the live run's journal entry.
    pub diag_name: &'a str,
    pub diag_op: &'a str,
}

/// The single dry-run dispatch for a MUTATING single-host effect, AFTER the caller's secret guard
/// and trace. Dry-run: record, apply `sim`, return synthetic ok. Live: journal around `real`
/// (which does its own result post-processing), honour an interrupt, and also return the
/// diagnostics message for callers that turn a failure into an error.
pub(crate) fn dispatch(
    ctx: &RunCtx,
    d: &Dispatch,
    sim: impl FnOnce(&RunCtx),
    real: impl FnOnce(&RunCtx) -> ExecResult,
) -> Result<(ExecResult, String), Box<EvalAltResult>> {
    if dry_run_record(ctx, d.kind, d.host, || d.detail.to_string()) {
        sim(ctx);
        return Ok((synthetic_ok(d.host.unwrap_or("")), String::new()));
    }
    let started =
        crate::engine::diagnostics::begin(ctx, d.diag_name, d.host.unwrap_or(""), d.diag_op);
    let result = real(ctx);
    let message =
        crate::engine::diagnostics::record_started(ctx, d.diag_name, d.diag_op, &result, started);
    ctx.check_interrupt()?;
    Ok((result, message))
}

/// The dispatch for a MUTATING command-effect. Prechecks the command, then either records it
/// (dry-run: apply `sim`, return synthetic ok) or runs it (`real`). `host` is `None` for local
/// effects (recorded with no host; synthetic result carries an empty host).
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
    let d = Dispatch {
        kind,
        host,
        detail: cmd,
        diag_name: label,
        diag_op: kind,
    };
    dispatch(ctx, &d, sim, real).map(|(result, _)| result)
}

/// The dispatch for ONE command on MANY hosts. Dry-run records `cmd` once per host (kind `kind`)
/// and returns a synthetic ok per host, in host order; live runs `real`, which owns fan-out,
/// per-host diagnostics and interrupts. The caller has already prechecked `cmd` and validated
/// `hosts`.
pub(crate) fn effect_fanout(
    ctx: &RunCtx,
    kind: &str,
    hosts: Vec<String>,
    cmd: &str,
    real: impl FnOnce(&RunCtx, &[String]) -> Result<Vec<ExecResult>, Box<EvalAltResult>>,
) -> Result<Vec<ExecResult>, Box<EvalAltResult>> {
    match dry_run_each(ctx, hosts, |h| {
        ctx.record(kind, Some(&h), cmd.to_string());
        synthetic_ok(&h)
    }) {
        Ok(synthetic) => Ok(synthetic),
        Err(hosts) => real(ctx, &hosts),
    }
}

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "remote_lock_acquire",
            move |host: &str, directory: &str| -> Result<(), Box<EvalAltResult>> {
                if dry_run_record(&ctx, "lock", Some(host), || {
                    format!("mkdir {directory} (owned deploy lock)")
                }) {
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
            dry_run_record(&ctx, "lock", Some(host), || {
                format!("rmdir {directory} (owned deploy lock)")
            });
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

    // ssh_probe — READ-ONLY remote command (still executes in dry-run).
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "ssh_probe",
            move |host: &str, cmd: &str| -> Result<ExecResult, Box<EvalAltResult>> {
                precheck(&ctx, &format!("ssh_probe {host}"), cmd)?;
                let result = to_result(host, ctx.runner.run_ssh(host, cmd));
                ctx.check_interrupt()?;
                Ok(result)
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
                let results = effect_fanout(&ctx, "ssh-all", host_strs, &cmd, |ctx, hosts| {
                    let runner = ctx.runner.clone();
                    let mut results = Vec::new();
                    for chunk in hosts.chunks(16) {
                        ctx.check_interrupt()?;
                        let started: Vec<_> = chunk
                            .iter()
                            .map(|host| {
                                crate::engine::diagnostics::begin(
                                    ctx,
                                    "ssh_exec_all",
                                    host,
                                    "ssh-all",
                                )
                            })
                            .collect();
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
                                    // On a thread panic, attribute it to the right host (don't
                                    // lose the host name, which the stdlib reports during an
                                    // incident).
                                    j.join().unwrap_or_else(|_| ExecResult {
                                        stdout: String::new(),
                                        stderr: "thread panicked".into(),
                                        exit_code: -1,
                                        host: h.clone(),
                                    })
                                })
                                .collect()
                        });
                        for (result, id) in batch.iter().zip(started) {
                            crate::engine::diagnostics::record_started(
                                ctx,
                                "ssh_exec_all",
                                "ssh-all",
                                result,
                                id,
                            );
                        }
                        results.extend(batch);
                        ctx.check_interrupt()?;
                    }
                    ctx.check_interrupt()?;
                    Ok(results)
                })?;
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

    // write_remote(host, content, remote_path[, permissions]) — MUTATING; atomically replaces a
    // remote file with `content` via the stdin channel (content never on argv). For secret
    // env-files, configs, etc. Mode 0600 unless an octal `permissions` string is given.
    for with_mode in [false, true] {
        let ctx = ctx.clone();
        let write = move |host: &str,
                          content: &str,
                          remote_path: &str,
                          permissions: Option<&str>|
              -> Result<ExecResult, Box<EvalAltResult>> {
            let mode = match permissions {
                Some(p) => crate::engine::builtins::transfer::parse_mode(p)?,
                None => 0o600,
            };
            let cmd = crate::engine::builtins::transfer::atomic_write_command(
                remote_path,
                mode,
                content.len() as u64,
            );
            // The content body is delivered off-argv; only the destination path is in `cmd`.
            // Guard the path for a leaked Secret, but trace the byte count (never the body).
            precheck_with(&ctx, &cmd, || {
                format!(
                    "write_remote {host} -> {remote_path} ({} bytes)",
                    content.len()
                )
            })?;
            let note = permissions
                .map(|_| format!(" (mode {mode:04o})"))
                .unwrap_or_default();
            let detail = format!("write {} bytes -> {remote_path}{note}", content.len());
            let d = Dispatch {
                kind: "write",
                host: Some(host),
                detail: &detail,
                diag_name: remote_path,
                diag_op: "write_remote",
            };
            dispatch(
                &ctx,
                &d,
                |_| {},
                |c| {
                    let mut result = to_result(host, c.runner.run_ssh_stdin(host, &cmd, content));
                    result.stdout.clear();
                    result.stderr = if result.exit_code == 0 {
                        String::new()
                    } else {
                        "write_remote failed (payload output suppressed)".into()
                    };
                    result
                },
            )
            .map(|(result, _)| result)
        };
        if with_mode {
            engine.register_fn(
                "write_remote",
                move |host: &str, content: &str, remote_path: &str, permissions: &str| {
                    write(host, content, remote_path, Some(permissions))
                },
            );
        } else {
            engine.register_fn(
                "write_remote",
                move |host: &str, content: &str, remote_path: &str| {
                    write(host, content, remote_path, None)
                },
            );
        }
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
    fn write_remote_with_permissions_sets_that_mode_before_the_rename() {
        let fake = FakeRunner::shared();
        let e = engine_with(shared(fake.clone()));
        e.run(r#"write_remote("web1", "site {}", "/etc/caddy/Caddyfile", "0644");"#)
            .unwrap();
        let calls = fake.calls();
        assert!(
            calls[0].contains(r#"chmod 644 "$tmp" && mv -f "$tmp" "$dest""#),
            "{calls:?}"
        );
        e.run(r#"write_remote("web1", "x", "/etc/x");"#).unwrap();
        assert!(fake.calls()[1].contains(r#"chmod 600 "$tmp""#));
    }

    #[test]
    fn write_remote_rejects_invalid_permissions_before_connecting() {
        let fake = FakeRunner::shared();
        let e = engine_with(shared(fake.clone()));
        for bad in ["4755", "rw-r--r--", "644; rm -rf /", "8"] {
            let err = e
                .run(&format!(r#"write_remote("web1", "x", "/etc/x", "{bad}");"#))
                .unwrap_err();
            assert!(err.to_string().contains("octal"), "{bad}: {err}");
        }
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn write_remote_records_permissions_in_dry_run() {
        let fake = FakeRunner::shared();
        let ctx = shared_dry(fake.clone());
        let e = engine_with(ctx.clone());
        e.run(r#"write_remote("web1", "abc", "/etc/app.conf", "644");"#)
            .unwrap();
        assert!(fake.calls().is_empty());
        let plan = ctx.plan.lock().unwrap().clone();
        assert_eq!(plan[0].detail, "write 3 bytes -> /etc/app.conf (mode 0644)");
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
    fn ssh_exec_all_dry_run_records_one_action_per_host_and_never_executes() {
        let fake = FakeRunner::shared();
        let ctx = shared_dry(fake.clone());
        let e = engine_with(ctx.clone());
        let hosts: Vec<String> = e
            .eval::<rhai::Array>(r#"ssh_exec_all(["a","b"], "docker pull x")"#)
            .unwrap()
            .into_iter()
            .map(|r| {
                let r = r.cast::<ExecResult>();
                assert!(r.exit_code == 0 && r.stdout.is_empty() && r.stderr.is_empty());
                r.host
            })
            .collect();
        assert_eq!(hosts, ["a", "b"]);
        assert!(fake.calls().is_empty(), "dry-run must not execute");
        let plan: Vec<_> = ctx
            .plan
            .lock()
            .unwrap()
            .iter()
            .map(|a| (a.kind.clone(), a.host.clone(), a.detail.clone()))
            .collect();
        assert_eq!(
            plan,
            [
                ("ssh-all".into(), Some("a".into()), "docker pull x".into()),
                ("ssh-all".into(), Some("b".into()), "docker pull x".into()),
            ]
        );
    }

    #[test]
    fn every_command_effect_rejects_a_stringified_secret_without_recording_or_running() {
        for script in [
            r#"ssh_exec("h", "echo \x01NRG_SECRET_NEEDS_REVEAL_OR_SH_QUOTE\x01")"#,
            r#"local_exec("echo \x01NRG_SECRET_NEEDS_REVEAL_OR_SH_QUOTE\x01")"#,
            r#"ssh_exec_stdin("h", "echo \x01NRG_SECRET_NEEDS_REVEAL_OR_SH_QUOTE\x01", "")"#,
            r#"local_exec_stdin("echo \x01NRG_SECRET_NEEDS_REVEAL_OR_SH_QUOTE\x01", "")"#,
            r#"ssh_exec_all(["h"], "echo \x01NRG_SECRET_NEEDS_REVEAL_OR_SH_QUOTE\x01")"#,
            r#"write_remote("h", "body", "/etc/\x01NRG_SECRET_NEEDS_REVEAL_OR_SH_QUOTE\x01")"#,
            r#"upload_file("h", "/a", "/b\x01NRG_SECRET_NEEDS_REVEAL_OR_SH_QUOTE\x01", "0644")"#,
        ] {
            for dry in [true, false] {
                let fake = FakeRunner::shared();
                let ctx = if dry {
                    shared_dry(fake.clone())
                } else {
                    shared(fake.clone())
                };
                let mut e = engine_with(ctx.clone());
                crate::engine::builtins::transfer::register(&mut e, ctx.clone());
                let err = e.eval::<Dynamic>(script).unwrap_err().to_string();
                assert!(err.contains("Secret"), "{script} dry={dry}: {err}");
                assert!(fake.calls().is_empty(), "{script} dry={dry}");
                assert!(ctx.plan.lock().unwrap().is_empty(), "{script} dry={dry}");
            }
        }
    }

    #[test]
    fn dry_run_plan_text_of_every_command_effect_is_pinned() {
        let fake = FakeRunner::shared();
        let ctx = shared_dry(fake.clone());
        let mut e = engine_with(ctx.clone());
        crate::engine::builtins::transfer::register(&mut e, ctx.clone());
        e.run(
            r#"
            ssh_exec("h1", "c1");
            local_exec("c2");
            ssh_exec_stdin("h2", "c3", "pw");
            local_exec_stdin("c4", "pw");
            write_remote("h3", "body", "/etc/a");
            upload_file("h4", "/src", "/dst", "0640");
            download_file("h5", "/rsrc", "/ldst", "600");
            remote_lock_acquire("h6", "/var/lock/x");
            remote_lock_release("h6", "/var/lock/x");
            "#,
        )
        .unwrap();
        assert!(fake.calls().is_empty());
        let plan: Vec<_> = ctx
            .plan
            .lock()
            .unwrap()
            .iter()
            .map(|a| {
                format!(
                    "{}|{}|{}",
                    a.kind,
                    a.host.as_deref().unwrap_or("-"),
                    a.detail
                )
            })
            .collect();
        assert_eq!(
            plan,
            [
                "ssh|h1|c1",
                "local|-|c2",
                "ssh-stdin|h2|c3",
                "local-stdin|-|c4",
                "write|h3|write 4 bytes -> /etc/a",
                "upload_file|h4|/src -> /dst mode 640",
                "download_file|h5|/rsrc -> /ldst mode 600",
                "lock|h6|mkdir /var/lock/x (owned deploy lock)",
                "lock|h6|rmdir /var/lock/x (owned deploy lock)",
            ]
        );
    }

    #[test]
    fn remote_lock_acquire_and_release_do_not_touch_the_host_in_dry_run() {
        let fake = FakeRunner::shared();
        let ctx = shared_dry(fake.clone());
        let e = engine_with(ctx.clone());
        e.run(r#"remote_lock_acquire("h", "/l"); remote_lock_release("h", "/l");"#)
            .unwrap();
        assert!(fake.calls().is_empty());
        assert!(ctx.remote_locks.lock().unwrap().is_empty());
    }

    #[test]
    fn live_write_remote_suppresses_payload_output() {
        let fake = FakeRunner::shared();
        fake.fail_host("h", 3, "echoed the SECRET body");
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        let r: ExecResult = e
            .eval(r#"write_remote("h", "SECRET body", "/etc/a")"#)
            .unwrap();
        assert_eq!(r.exit_code, 3);
        assert!(r.stdout.is_empty());
        assert_eq!(r.stderr, "write_remote failed (payload output suppressed)");
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
        // Serialize against every other env-mutating test
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

#[cfg(test)]
mod mise_tests {
    use crate::engine::{
        build_engine,
        context::{shared, shared_dry},
        runner::FakeRunner,
        stdlib,
    };
    fn recipe(ctx: crate::engine::context::SharedCtx) -> rhai::Engine {
        let mut engine = build_engine(ctx);
        stdlib::install(&mut engine, std::path::PathBuf::from("."));
        engine
    }
    const SCRIPT: &str = r#"import "std/mise" as mise; mise::provision("web", "/opt/app", #{erlang:"28.2", elixir:"1.19.4-otp-28", node:"22.14.0"});"#;

    #[test]
    fn mise_order_repeat_and_failure_propagation() {
        let fake = FakeRunner::shared();
        let engine = recipe(shared(fake.clone()));
        engine.run(SCRIPT).unwrap();
        let first = fake.calls();
        assert!(first[1].contains("use --pin --path '/opt/app/mise.toml' 'erlang@28.2'"));
        assert!(first[2].contains("-- erl -noshell"));
        assert!(first[3].contains("use --pin --path '/opt/app/mise.toml' 'elixir@1.19.4-otp-28'"));
        assert!(first
            .last()
            .unwrap()
            .contains("elixir --version && mix --version && node --version"));
        engine.run(SCRIPT).unwrap();
        assert_eq!(&fake.calls()[first.len()..], first);
        for needle in ["erlang@28.2'", "elixir@1.19.4-otp-28'"] {
            let fake = FakeRunner::shared();
            fake.fail_cmd("web", needle, 17, "installer failed");
            let ctx = shared(fake.clone());
            let engine = recipe(ctx.clone());
            let err = engine.run(SCRIPT).unwrap_err().to_string();
            assert!(err.contains("installer failed"));
            assert!(err.contains("17"));
            assert!(!fake.calls().iter().any(|c| c.contains("node@")));
            assert_eq!(ctx.steps.lock().unwrap().last().unwrap().exit_code, 17);
        }
    }
    #[test]
    fn mise_dry_run_plans_but_never_executes() {
        let fake = FakeRunner::shared();
        let ctx = shared_dry(fake.clone());
        recipe(ctx.clone()).run(SCRIPT).unwrap();
        assert!(fake.calls().is_empty());
        assert_eq!(ctx.plan.lock().unwrap().len(), 6);
    }
    #[test]
    fn mise_rejects_floating_versions_before_any_effect() {
        let fake = FakeRunner::shared();
        let engine = recipe(shared(fake.clone()));
        assert!(engine.run(&SCRIPT.replace("28.2", "latest")).is_err());
        assert!(fake.calls().is_empty());
    }
}
