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
                ctx.lock().unwrap().record("ssh", Some(host), cmd.to_string());
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
                ctx.lock().unwrap().record("local", None, cmd.to_string());
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
                for h in &host_strs {
                    ctx.lock().unwrap().record("ssh-all", Some(h), cmd.clone());
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

    // ssh_exec_stdin(host, cmd, stdin) — MUTATING; delivers `stdin` off-argv (e.g. a password
    // to `docker login --password-stdin`). The payload is NEVER traced or put on argv.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "ssh_exec_stdin",
            move |host: &str, cmd: &str, stdin: &str| -> ExecResult {
                let snap = ctx.lock().unwrap().snapshot();
                if snap.trace {
                    eprintln!(
                        "[nrg] ssh_exec_stdin {host} -> {} (stdin {} bytes)",
                        traced(cmd, &snap),
                        stdin.len()
                    );
                }
                if snap.mode == EffectMode::DryRun {
                    ctx.lock().unwrap().record("ssh-stdin", Some(host), cmd.to_string());
                    return ExecResult {
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: 0,
                        host: host.into(),
                    };
                }
                to_result(host, snap.runner.run_ssh_stdin(host, cmd, stdin))
            },
        );
    }

    // local_exec_stdin(cmd, stdin) — MUTATING local mirror.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "local_exec_stdin",
            move |cmd: &str, stdin: &str| -> ExecResult {
                let snap = ctx.lock().unwrap().snapshot();
                if snap.trace {
                    eprintln!(
                        "[nrg] local_exec_stdin -> {} (stdin {} bytes)",
                        traced(cmd, &snap),
                        stdin.len()
                    );
                }
                if snap.mode == EffectMode::DryRun {
                    ctx.lock().unwrap().record("local-stdin", None, cmd.to_string());
                    return ExecResult {
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: 0,
                        host: String::new(),
                    };
                }
                to_result("", snap.runner.run_local_stdin(cmd, stdin))
            },
        );
    }

    // write_remote(host, content, remote_path) — MUTATING; writes content to a 0600 remote file
    // via the stdin channel (content never on argv). For secret env-files, configs, etc.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "write_remote",
            move |host: &str, content: &str, remote_path: &str| -> ExecResult {
                let snap = ctx.lock().unwrap().snapshot();
                let cmd = format!(
                    "umask 077; cat > {}",
                    crate::engine::secret::posix_quote(remote_path)
                );
                if snap.trace {
                    eprintln!(
                        "[nrg] write_remote {host} -> {remote_path} ({} bytes)",
                        content.len()
                    );
                }
                if snap.mode == EffectMode::DryRun {
                    ctx.lock().unwrap().record(
                        "write",
                        Some(host),
                        format!("write {} bytes -> {remote_path}", content.len()),
                    );
                    return ExecResult {
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: 0,
                        host: host.into(),
                    };
                }
                to_result(host, snap.runner.run_ssh_stdin(host, &cmd, content))
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
        assert!(calls[0].contains("umask 077; cat > '/run/app.env'"));
        let (argv, stdin) = calls[0].split_once("<<<").unwrap();
        assert!(!argv.contains("abc123"), "content must not be on argv");
        assert!(stdin.contains("SECRET=abc123"), "content must be on stdin");
    }

    #[test]
    fn write_remote_records_in_dry_run() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        ctx.lock().unwrap().mode = EffectMode::DryRun;
        let e = engine_with(ctx.clone());
        e.run(r#"write_remote("web1", "BIG=body", "/run/app.env");"#)
            .unwrap();
        assert!(fake.calls().is_empty());
        let plan = ctx.lock().unwrap().plan.lock().unwrap().clone();
        assert_eq!(plan[0].kind, "write");
        assert!(plan[0].detail.contains("/run/app.env"));
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
