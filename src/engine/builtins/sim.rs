//! Mode-aware container/port/health builtins backed by the dry-run `SimState` overlay.
//!
//! The ported `.rhai` stdlib does ALL container existence/state/health reads and container/
//! proxy mutations through these typed builtins — NEVER through a raw `docker inspect` /
//! `nc -z` over `ssh_exec` (which would bypass the sim and diverge under dry-run).
//!
//! In **Live** mode each builtin runs the real command via the runner (mutations) or a real
//! probe (reads). In **DryRun** mode a read seeds lazily from ONE real probe per (host, name)
//! and thereafter reflects stubbed mutations; a mutation records a `PlannedAction` and applies
//! the matching sim change, returning a synthetic ok. So a stubbed `sim_docker_run` of the NEW
//! container makes `sim_container_running(new)` and `sim_container_healthy(new)` true — the
//! deploy dry-run takes the same branches a real run would.
//!
//! Seeding is lock-safe: the one real probe runs on a `runner` cloned out of the snapshot,
//! WITHOUT holding the `RunCtx` or `SimState` lock (mirrors `exec.rs`).

use crate::engine::context::{EffectMode, SharedCtx, Snapshot};
use crate::engine::runner::CommandRunner;
use crate::engine::types::ExecResult;
use rhai::Engine;
use std::sync::Arc;

/// A synthetic success result for a dry-run mutation.
fn synthetic_ok(host: &str) -> ExecResult {
    ExecResult {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        host: host.to_string(),
    }
}

fn to_result(host: &str, raw: crate::engine::runner::RawOutput) -> ExecResult {
    ExecResult {
        stdout: raw.stdout,
        stderr: raw.stderr,
        exit_code: raw.exit_code,
        host: host.to_string(),
    }
}

/// The configured container runtime command — set by the stdlib's `set_runtime()` into state
/// `nrg.runtime.cmd`, defaulting to `docker`. The Live/seeding inspect probes below use it so
/// they match the runtime the stdlib's mutation commands use (a podman/nerdctl deploy must be
/// probed with `podman`/`nerdctl inspect`, not `docker inspect`).
fn runtime_cmd(snap: &Snapshot) -> String {
    snap.state
        .lock()
        .unwrap()
        .get("nrg.runtime.cmd")
        .unwrap_or_else(|| "docker".to_string())
}

/// One real `<rt> inspect -f '{{.State.Running}}'` probe (read-only). Used to seed the sim and
/// for the Live path.
fn real_inspect_running(runner: &Arc<dyn CommandRunner>, rt: &str, host: &str, name: &str) -> bool {
    let cmd = format!("{rt} inspect -f '{{{{.State.Running}}}}' {name}");
    let out = runner.run_ssh(host, &cmd);
    out.exit_code == 0 && out.stdout.trim() == "true"
}

/// One real `<rt> image inspect -f '{{.Id}}'` probe (read-only).
fn real_image_id(runner: &Arc<dyn CommandRunner>, rt: &str, host: &str, tag: &str) -> String {
    let cmd = format!("{rt} image inspect -f '{{{{.Id}}}}' {tag}");
    let out = runner.run_ssh(host, &cmd);
    if out.exit_code == 0 {
        out.stdout.trim().to_string()
    } else {
        String::new()
    }
}

/// One real `<rt> inspect -f '{{.State.Health.Status}}'` probe (read-only).
fn real_inspect_healthy(runner: &Arc<dyn CommandRunner>, rt: &str, host: &str, name: &str) -> bool {
    let cmd = format!("{rt} inspect -f '{{{{.State.Health.Status}}}}' {name}");
    let out = runner.run_ssh(host, &cmd);
    out.exit_code == 0 && out.stdout.trim() == "healthy"
}

/// One real `nc -z localhost <port>` probe (read-only); true iff the port answers.
fn real_port_open(runner: &Arc<dyn CommandRunner>, host: &str, port: u16) -> bool {
    let cmd = format!("nc -z localhost {port}");
    runner.run_ssh(host, &cmd).exit_code == 0
}

/// Coerce a Rhai i64 port to a u16, clamping out-of-range values.
fn as_port(port: i64) -> u16 {
    port.clamp(0, u16::MAX as i64) as u16
}

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    // is_dry_run() — cheap mode check for cosmetic stdlib branches (e.g. printing <auto>).
    {
        let ctx = ctx.clone();
        engine.register_fn("is_dry_run", move || -> bool { ctx.lock().unwrap().is_dry_run() });
    }

    // sim_container_running(host, name) -> bool
    // DryRun: lazily seed from ONE real inspect, then reflect stubbed mutations. Live: real probe.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "sim_container_running",
            move |host: &str, name: &str| -> bool {
                let snap = snapshot(&ctx);
                if snap.mode == EffectMode::Live {
                    return real_inspect_running(&snap.runner, &runtime_cmd(&snap), host, name);
                }
                // DryRun: probe for real ONLY on first access, with NO lock held; thereafter
                // just read the (possibly mutated) sim value.
                if snap.sim.lock().unwrap().is_seeded(host, name) {
                    return snap.sim.lock().unwrap().is_running(host, name);
                }
                let real = real_inspect_running(&snap.runner, &runtime_cmd(&snap), host, name);
                let mut sim = snap.sim.lock().unwrap();
                sim.seed_running(host, name, real)
            },
        );
    }

    // sim_image_id(host, tag) -> String
    // DryRun: sim image (synthetic stable token "<tag>" if unknown), lazily seeded. Live: real.
    {
        let ctx = ctx.clone();
        engine.register_fn("sim_image_id", move |host: &str, tag: &str| -> String {
            let snap = snapshot(&ctx);
            if snap.mode == EffectMode::Live {
                return real_image_id(&snap.runner, &runtime_cmd(&snap), host, tag);
            }
            // DryRun: a tag here names the IMAGE (not a container). Seed once from a real read,
            // caching under the (host, tag) entity; fall back to a branch-stable synthetic token.
            let already = snap.sim.lock().unwrap().is_seeded(host, tag);
            if !already {
                let real = real_image_id(&snap.runner, &runtime_cmd(&snap), host, tag);
                let mut sim = snap.sim.lock().unwrap();
                // seed_running records the entity as seeded; store the real id as the image.
                sim.seed_running(host, tag, false);
                if !real.is_empty() {
                    sim.set_image(host, tag, &real);
                }
            }
            let id = snap.sim.lock().unwrap().image_id(host, tag);
            if id.is_empty() {
                format!("<{tag}>")
            } else {
                id
            }
        });
    }

    // sim_pick_port(host, base) -> i64
    // DryRun: deterministic symbolic port (base+10000, +1 per pick), record a 'check', NO probe.
    // Live: real `nc -z` scan from base+10000 upward for the first free slot.
    {
        let ctx = ctx.clone();
        engine.register_fn("sim_pick_port", move |host: &str, base: i64| -> i64 {
            let snap = snapshot(&ctx);
            if snap.mode == EffectMode::DryRun {
                let port = snap.sim.lock().unwrap().pick_port(host, as_port(base));
                ctx.lock().unwrap().record(
                    "check",
                    Some(host),
                    format!("pick free port from {}", base + 10000),
                );
                return port as i64;
            }
            // Live: scan upward from base+10000 for the first port that is NOT answering.
            let start = as_port(base).saturating_add(10000);
            for offset in 0..100u16 {
                let candidate = start.saturating_add(offset);
                if !real_port_open(&snap.runner, host, candidate) {
                    return candidate as i64;
                }
            }
            start as i64
        });
    }

    // sim_docker_run(host, tag, name, cmd) -> ExecResult
    // DryRun: record + sim.set_running(host,name,tag) (running+healthy), synthetic ok.
    // Live: run `cmd` via runner (== ssh_exec).
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "sim_docker_run",
            move |host: &str, tag: &str, name: &str, cmd: &str| -> ExecResult {
                let snap = snapshot(&ctx);
                if snap.mode == EffectMode::DryRun {
                    ctx.lock().unwrap().record("ssh", Some(host), cmd.to_string());
                    snap.sim.lock().unwrap().set_running(host, name, tag);
                    return synthetic_ok(host);
                }
                to_result(host, snap.runner.run_ssh(host, cmd))
            },
        );
    }

    // sim_docker_stop(host, name, cmd) -> ExecResult
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "sim_docker_stop",
            move |host: &str, name: &str, cmd: &str| -> ExecResult {
                let snap = snapshot(&ctx);
                if snap.mode == EffectMode::DryRun {
                    ctx.lock().unwrap().record("ssh", Some(host), cmd.to_string());
                    snap.sim.lock().unwrap().set_stopped(host, name);
                    return synthetic_ok(host);
                }
                to_result(host, snap.runner.run_ssh(host, cmd))
            },
        );
    }

    // sim_docker_rename(host, old, new, cmd) -> ExecResult
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "sim_docker_rename",
            move |host: &str, old: &str, new: &str, cmd: &str| -> ExecResult {
                let snap = snapshot(&ctx);
                if snap.mode == EffectMode::DryRun {
                    ctx.lock().unwrap().record("ssh", Some(host), cmd.to_string());
                    snap.sim.lock().unwrap().rename(host, old, new);
                    return synthetic_ok(host);
                }
                to_result(host, snap.runner.run_ssh(host, cmd))
            },
        );
    }

    // sim_docker_remove(host, name, cmd) -> ExecResult
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "sim_docker_remove",
            move |host: &str, name: &str, cmd: &str| -> ExecResult {
                let snap = snapshot(&ctx);
                if snap.mode == EffectMode::DryRun {
                    ctx.lock().unwrap().record("ssh", Some(host), cmd.to_string());
                    snap.sim.lock().unwrap().remove(host, name);
                    return synthetic_ok(host);
                }
                to_result(host, snap.runner.run_ssh(host, cmd))
            },
        );
    }

    // sim_proxy_switch(host, service, target, cmd) -> ExecResult
    // DryRun: record + sim.proxy_switch (stores current target for read-back / rollback snapshot).
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "sim_proxy_switch",
            move |host: &str, service: &str, target: &str, cmd: &str| -> ExecResult {
                let snap = snapshot(&ctx);
                if snap.mode == EffectMode::DryRun {
                    ctx.lock().unwrap().record("ssh", Some(host), cmd.to_string());
                    snap.sim.lock().unwrap().proxy_switch(host, service, target);
                    return synthetic_ok(host);
                }
                to_result(host, snap.runner.run_ssh(host, cmd))
            },
        );
    }

    // sim_wait_port(host, port) -> bool
    // DryRun: true iff the sim marks that port occupied (agrees with the just-stubbed container),
    // no probe / no sleep. Live: a real `nc -z` retry loop.
    {
        let ctx = ctx.clone();
        engine.register_fn("sim_wait_port", move |host: &str, port: i64| -> bool {
            let snap = snapshot(&ctx);
            if snap.mode == EffectMode::DryRun {
                ctx.lock().unwrap().record(
                    "check",
                    Some(host),
                    format!("wait for port {port}"),
                );
                return snap.sim.lock().unwrap().port_open(host, as_port(port));
            }
            for _ in 0..30 {
                if real_port_open(&snap.runner, host, as_port(port)) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            false
        });
    }

    // sim_container_healthy(host, name) -> bool
    // DryRun: true iff the sim has (host,name) running AND healthy (set by sim_docker_run), no
    // probe. Live: a real `inspect -f {{.State.Health.Status}}` retry loop.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "sim_container_healthy",
            move |host: &str, name: &str| -> bool {
                let snap = snapshot(&ctx);
                if snap.mode == EffectMode::DryRun {
                    ctx.lock().unwrap().record(
                        "check",
                        Some(host),
                        format!("wait for {name} healthy"),
                    );
                    return snap.sim.lock().unwrap().is_healthy(host, name);
                }
                let rt = runtime_cmd(&snap);
                for _ in 0..30 {
                    if real_inspect_healthy(&snap.runner, &rt, host, name) {
                        return true;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
                false
            },
        );
    }
}

/// Take a consistent snapshot of the shared handles under a short lock.
fn snapshot(ctx: &SharedCtx) -> Snapshot {
    ctx.lock().unwrap().snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::shared;
    use crate::engine::runner::{FakeRunner, RawOutput};
    use crate::engine::types::register_types;
    use std::sync::Mutex;

    fn engine_with(ctx: SharedCtx) -> Engine {
        let mut e = Engine::new();
        register_types(&mut e);
        register(&mut e, ctx);
        e
    }

    fn dry(ctx: &SharedCtx) {
        ctx.lock().unwrap().mode = EffectMode::DryRun;
    }

    #[test]
    fn is_dry_run_builtin_reflects_mode() {
        let ctx = shared(FakeRunner::shared());
        let e = engine_with(ctx.clone());
        assert!(!e.eval::<bool>("is_dry_run()").unwrap());
        dry(&ctx);
        let e = engine_with(ctx);
        assert!(e.eval::<bool>("is_dry_run()").unwrap());
    }

    #[test]
    fn stubbed_run_makes_container_running_and_healthy_in_dry_run() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        dry(&ctx);
        let e = engine_with(ctx.clone());
        let script = r#"
            sim_docker_run("web1", "img:v2", "app-new", "docker run -d --name app-new img:v2");
            [sim_container_running("web1", "app-new"), sim_container_healthy("web1", "app-new")]
        "#;
        let r: rhai::Array = e.eval(script).unwrap();
        assert!(r[0].clone().as_bool().unwrap(), "new container must be running");
        assert!(r[1].clone().as_bool().unwrap(), "new container must be healthy");
        // No real ssh ran for the run mutation or the reads in dry-run.
        assert!(fake.calls().is_empty(), "dry-run must not execute");
        // The run cmd was recorded.
        let plan = ctx.lock().unwrap().plan.lock().unwrap().clone();
        assert!(plan.iter().any(|a| a.kind == "ssh" && a.detail.contains("docker run -d")));
    }

    #[test]
    fn pick_port_is_deterministic_in_dry_run() {
        let ctx = shared(FakeRunner::shared());
        dry(&ctx);
        let e = engine_with(ctx);
        let a: i64 = e.eval(r#"sim_pick_port("web1", 3000)"#).unwrap();
        let b: i64 = e.eval(r#"sim_pick_port("web1", 3000)"#).unwrap();
        assert_eq!(a, 13000);
        assert_eq!(b, 13001);
    }

    #[test]
    fn proxy_switch_stores_target_in_dry_run() {
        let ctx = shared(FakeRunner::shared());
        dry(&ctx);
        let e = engine_with(ctx.clone());
        e.run(r#"sim_proxy_switch("web1", "app", "localhost:13000", "kamal-proxy deploy app --target localhost:13000");"#)
            .unwrap();
        assert_eq!(
            ctx.lock().unwrap().sim.lock().unwrap().proxy_target("web1", "app"),
            Some("localhost:13000".to_string())
        );
    }

    #[test]
    fn promote_rename_makes_canonical_running_and_old_gone() {
        let ctx = shared(FakeRunner::shared());
        dry(&ctx);
        let e = engine_with(ctx.clone());
        let script = r#"
            sim_docker_run("web1", "img", "app-new", "run app-new");
            sim_docker_rename("web1", "app-new", "app", "rename app-new app");
            [sim_container_running("web1", "app"), sim_container_running("web1", "app-new")]
        "#;
        let r: rhai::Array = e.eval(script).unwrap();
        assert!(r[0].clone().as_bool().unwrap(), "canonical must be running");
        assert!(!r[1].clone().as_bool().unwrap(), "new name must be gone");
    }

    #[test]
    fn remove_clears_container_in_dry_run() {
        let ctx = shared(FakeRunner::shared());
        dry(&ctx);
        let e = engine_with(ctx);
        let still: bool = e
            .eval(
                r#"
                sim_docker_run("web1", "img", "old", "run old");
                sim_docker_remove("web1", "old", "rm -f old");
                sim_container_running("web1", "old")
            "#,
            )
            .unwrap();
        assert!(!still);
    }

    #[test]
    fn wait_port_agrees_with_stubbed_run_in_dry_run() {
        let ctx = shared(FakeRunner::shared());
        dry(&ctx);
        let e = engine_with(ctx);
        // pick a port (marks it occupied) then wait on it -> true; an unpicked port -> false.
        let r: rhai::Array = e
            .eval(
                r#"
                let p = sim_pick_port("web1", 3000);
                [sim_wait_port("web1", p), sim_wait_port("web1", 9999)]
            "#,
            )
            .unwrap();
        assert!(r[0].clone().as_bool().unwrap(), "picked port must be open");
        assert!(!r[1].clone().as_bool().unwrap(), "unpicked port must be closed");
    }

    #[test]
    fn live_docker_run_calls_the_runner() {
        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        // Live mode (default).
        let e = engine_with(ctx);
        let ok: bool = e
            .eval(r#"sim_docker_run("web1", "img", "app", "docker run -d --name app img").ok"#)
            .unwrap();
        assert!(ok);
        assert_eq!(
            fake.calls(),
            vec!["ssh web1: docker run -d --name app img".to_string()]
        );
    }

    #[test]
    fn live_container_running_probes_via_runner() {
        // A FakeRunner that replies "true" to the inspect probe.
        let fake = Arc::new(TrueRunner::default());
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        let running: bool = e.eval(r#"sim_container_running("web1", "app")"#).unwrap();
        assert!(running);
        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains("docker inspect -f '{{.State.Running}}' app"));
    }

    #[test]
    fn live_probe_honors_configured_runtime() {
        // `set_runtime("podman")` persists nrg.runtime.cmd="podman"; the Live inspect probe must
        // use it (a podman/nerdctl host must be probed with the right CLI, not `docker`).
        let fake = Arc::new(TrueRunner::default());
        let ctx = shared(fake.clone());
        ctx.lock()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .set("nrg.runtime.cmd", "podman")
            .unwrap();
        let e = engine_with(ctx);
        let _running: bool = e.eval(r#"sim_container_running("web1", "app")"#).unwrap();
        let calls = fake.calls.lock().unwrap();
        assert!(
            calls[0].contains("podman inspect -f '{{.State.Running}}' app"),
            "got: {}",
            calls[0]
        );
        assert!(!calls[0].contains("docker"));
    }

    #[test]
    fn dry_run_container_running_seeds_from_one_real_probe() {
        // Pre-deploy reality: the OLD container is running (probe says "true").
        let fake = Arc::new(TrueRunner::default());
        let ctx = shared(fake.clone());
        dry(&ctx);
        let e = engine_with(ctx);
        // Two reads: only ONE real probe should fire (lazy seeding).
        let r: rhai::Array = e
            .eval(
                r#"[sim_container_running("web1", "app"), sim_container_running("web1", "app")]"#,
            )
            .unwrap();
        assert!(r[0].clone().as_bool().unwrap());
        assert!(r[1].clone().as_bool().unwrap());
        assert_eq!(
            fake.calls.lock().unwrap().len(),
            1,
            "must seed from exactly one real probe"
        );
    }

    /// A runner whose ssh replies make inspect/health/port probes succeed ("true"/"healthy").
    #[derive(Default)]
    struct TrueRunner {
        calls: Mutex<Vec<String>>,
    }
    impl CommandRunner for TrueRunner {
        fn run_ssh(&self, host: &str, cmd: &str) -> RawOutput {
            self.calls.lock().unwrap().push(format!("ssh {host}: {cmd}"));
            let stdout = if cmd.contains("State.Running") {
                "true\n"
            } else if cmd.contains("Health.Status") {
                "healthy\n"
            } else {
                ""
            };
            RawOutput {
                stdout: stdout.to_string(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
        fn run_local(&self, _cmd: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_ssh_stdin(&self, _host: &str, _cmd: &str, _stdin: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_local_stdin(&self, _cmd: &str, _stdin: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
    }
}
