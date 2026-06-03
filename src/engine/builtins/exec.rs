//! Command-execution builtins. Effect classification is BY BUILTIN:
//! `ssh_exec`/`local_exec`/`ssh_exec_all` are MUTATING; `ssh_probe` is READ-ONLY.
//! (Phase 3 uses this distinction for dry-run.)

use crate::engine::context::{EffectMode, SharedCtx};
use crate::engine::runner::{CommandRunner, RawOutput};
use crate::engine::types::ExecResult;
use rhai::{Array, Dynamic, Engine};
use std::sync::Arc;
use std::thread;

fn to_result(host: &str, raw: RawOutput) -> ExecResult {
    ExecResult {
        stdout: raw.stdout,
        stderr: raw.stderr,
        exit_code: raw.exit_code,
        host: host.to_string(),
    }
}

/// Snapshot (mode, runner, trace) under a short lock, then release before blocking.
fn snapshot(ctx: &SharedCtx) -> (EffectMode, Arc<dyn CommandRunner>, bool) {
    let g = ctx.lock().unwrap();
    (g.mode, g.runner.clone(), g.trace)
}

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    // ssh_exec — MUTATING remote command.
    {
        let ctx = ctx.clone();
        engine.register_fn("ssh_exec", move |host: &str, cmd: &str| -> ExecResult {
            let (mode, runner, trace) = snapshot(&ctx);
            if trace {
                eprintln!("[nrg] ssh_exec {host} -> {cmd}");
            }
            if mode == EffectMode::DryRun {
                // Phase 3 records to a plan log; for now the Live path is what's exercised.
                return ExecResult {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                    host: host.into(),
                };
            }
            to_result(host, runner.run_ssh(host, cmd))
        });
    }

    // ssh_probe — READ-ONLY remote command (still executes in dry-run, Phase 3).
    {
        let ctx = ctx.clone();
        engine.register_fn("ssh_probe", move |host: &str, cmd: &str| -> ExecResult {
            let (_mode, runner, trace) = snapshot(&ctx);
            if trace {
                eprintln!("[nrg] ssh_probe {host} -> {cmd}");
            }
            to_result(host, runner.run_ssh(host, cmd))
        });
    }

    // local_exec — MUTATING local command.
    {
        let ctx = ctx.clone();
        engine.register_fn("local_exec", move |cmd: &str| -> ExecResult {
            let (mode, runner, trace) = snapshot(&ctx);
            if trace {
                eprintln!("[nrg] local_exec -> {cmd}");
            }
            if mode == EffectMode::DryRun {
                return ExecResult {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                    host: String::new(),
                };
            }
            to_result("", runner.run_local(cmd))
        });
    }

    // ssh_exec_all — parallel fan-out across hosts. Never aborts on single-host failure.
    {
        let ctx = ctx.clone();
        engine.register_fn("ssh_exec_all", move |hosts: Array, cmd: &str| -> Array {
            let (mode, runner, _trace) = snapshot(&ctx);
            let host_strs: Vec<String> = hosts
                .iter()
                .map(|h| h.clone().into_string().unwrap_or_default())
                .collect();
            let cmd = cmd.to_string();
            if mode == EffectMode::DryRun {
                return host_strs
                    .into_iter()
                    .map(|h| {
                        Dynamic::from(ExecResult {
                            stdout: String::new(),
                            stderr: String::new(),
                            exit_code: 0,
                            host: h,
                        })
                    })
                    .collect();
            }
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
            results.into_iter().map(Dynamic::from).collect()
        });
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
    fn ssh_probe_reads_through_runner() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        let out: String = e.eval(r#"ssh_probe("web1", "docker ps").host"#).unwrap();
        assert_eq!(out, "web1");
        assert_eq!(fake.calls(), vec!["ssh web1: docker ps".to_string()]);
    }
}
