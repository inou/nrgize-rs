//! Command-execution builtins. Effect classification is BY BUILTIN:
//! `ssh_exec`/`local_exec`/`ssh_exec_all` are MUTATING; `ssh_probe` is READ-ONLY.
//! (Phase 3 uses this distinction for dry-run.)

use crate::engine::context::{EffectMode, SharedCtx, Snapshot};
use crate::engine::runner::RawOutput;
use crate::engine::types::ExecResult;
use rhai::{Array, Dynamic, Engine, EvalAltResult};
use std::thread;

fn to_result(host: &str, raw: RawOutput) -> ExecResult {
    ExecResult {
        stdout: raw.stdout,
        stderr: raw.stderr,
        exit_code: raw.exit_code,
        host: host.to_string(),
    }
}

/// Redact a command for display against the snapshot's registered secret values.
fn traced(cmd: &str, snap: &Snapshot) -> String {
    crate::engine::secret::redact(cmd, &snap.secrets.lock().unwrap())
}

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    // ssh_exec — MUTATING remote command.
    {
        let ctx = ctx.clone();
        engine.register_fn("ssh_exec", move |host: &str, cmd: &str| -> ExecResult {
            let snap = ctx.lock().unwrap().snapshot();
            if snap.trace {
                eprintln!("[nrg] ssh_exec {host} -> {}", traced(cmd, &snap));
            }
            if snap.mode == EffectMode::DryRun {
                ctx.lock().unwrap().record("ssh", Some(host), traced(cmd, &snap));
                return ExecResult {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                    host: host.into(),
                };
            }
            to_result(host, snap.runner.run_ssh(host, cmd))
        });
    }

    // ssh_probe — READ-ONLY remote command (still executes in dry-run, Phase 3).
    {
        let ctx = ctx.clone();
        engine.register_fn("ssh_probe", move |host: &str, cmd: &str| -> ExecResult {
            let snap = ctx.lock().unwrap().snapshot();
            if snap.trace {
                eprintln!("[nrg] ssh_probe {host} -> {}", traced(cmd, &snap));
            }
            to_result(host, snap.runner.run_ssh(host, cmd))
        });
    }

    // local_exec — MUTATING local command.
    {
        let ctx = ctx.clone();
        engine.register_fn("local_exec", move |cmd: &str| -> ExecResult {
            let snap = ctx.lock().unwrap().snapshot();
            if snap.trace {
                eprintln!("[nrg] local_exec -> {}", traced(cmd, &snap));
            }
            if snap.mode == EffectMode::DryRun {
                ctx.lock().unwrap().record("local", None, traced(cmd, &snap));
                return ExecResult {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                    host: String::new(),
                };
            }
            to_result("", snap.runner.run_local(cmd))
        });
    }

    // ssh_exec_all — parallel fan-out across hosts. Never aborts on single-host failure.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "ssh_exec_all",
            move |hosts: Array, cmd: &str| -> Result<Array, Box<EvalAltResult>> {
            let snap = ctx.lock().unwrap().snapshot();
            if snap.trace {
                eprintln!("[nrg] ssh_exec_all -> {}", traced(cmd, &snap));
            }
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
            if snap.mode == EffectMode::DryRun {
                let detail = traced(&cmd, &snap);
                for h in &host_strs {
                    ctx.lock().unwrap().record("ssh-all", Some(h), detail.clone());
                }
                return Ok(host_strs
                    .into_iter()
                    .map(|h| {
                        Dynamic::from(ExecResult {
                            stdout: String::new(),
                            stderr: String::new(),
                            exit_code: 0,
                            host: h,
                        })
                    })
                    .collect());
            }
            let runner = snap.runner;
            let results: Vec<ExecResult> = thread::scope(|s| {
                let handles: Vec<_> = host_strs
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
                    .map(|j| {
                        j.join().unwrap_or_else(|_| ExecResult {
                            stdout: String::new(),
                            stderr: "thread panicked".into(),
                            exit_code: -1,
                            host: String::new(),
                        })
                    })
                    .collect()
            });
            Ok(results.into_iter().map(Dynamic::from).collect())
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::shared;
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
    fn trace_redacts_registered_secret() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        ctx.lock().unwrap().register_secret("supersecretvalue");
        let snap = ctx.lock().unwrap().snapshot();
        assert_eq!(
            super::traced("docker login -p supersecretvalue", &snap),
            "docker login -p ***"
        );
    }

    #[test]
    fn dry_run_records_instead_of_executing() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        ctx.lock().unwrap().mode = EffectMode::DryRun;
        let e = engine_with(ctx.clone());
        let ok: bool = e.eval(r#"ssh_exec("web1", "rm -rf /data").ok"#).unwrap();
        assert!(ok); // synthetic ok
        assert!(fake.calls().is_empty(), "dry-run must not execute");
        let plan = ctx.lock().unwrap().plan.lock().unwrap().clone();
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
        assert!(fake.calls().is_empty(), "must not run any ssh before validation");
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
}
