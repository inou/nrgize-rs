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
//! Probe FAILURES (ssh down, auth, missing runtime CLI) are handled by MODE (issue #15):
//! - LIVE: they THROW (with stderr), so a real mutating run never takes the wrong branch against
//!   a host whose state we couldn't read. Only a genuine "no such container/image" is absent.
//! - DRY-RUN seeding: they DON'T abort the plan (you often preview a deploy from a machine that
//!   can't reach the hosts). The probe failure is recorded as a visible plan note and the entity
//!   is assumed absent — surfacing the uncertainty instead of silently lying.

use crate::engine::builtins::exec::{effect, to_result};
use crate::engine::context::{EffectMode, RunCtx, SharedCtx};
use crate::engine::runner::{CommandRunner, RawOutput};
use crate::engine::secret::posix_quote;
use crate::engine::types::ExecResult;
use rhai::{Engine, EvalAltResult};
use std::sync::Arc;

/// The configured container runtime command — set by the stdlib's `set_runtime()` into state
/// `nrg.runtime.cmd`, defaulting to `docker`. The Live/seeding inspect probes below use it so
/// they match the runtime the stdlib's mutation commands use (a podman/nerdctl deploy must be
/// probed with `podman`/`nerdctl inspect`, not `docker inspect`).
fn runtime_cmd(ctx: &RunCtx) -> String {
    ctx.state
        .lock()
        .unwrap()
        .get("nrg.runtime.cmd")
        .unwrap_or_else(|| "docker".to_string())
}

/// Classify a non-zero probe exit: a genuine "no such object/container/image" is a legitimate
/// ABSENT answer; anything else (ssh failure exit 255, missing CLI exit 127, auth, timeout) is a
/// probe that FAILED TO RUN and must surface as an error, never be mistaken for "not running".
///
/// Robustness review R4: a missing CLI exits 127 with a shell message like
/// `docker: command not found` (bash) or `sh: docker: not found` (dash/POSIX sh) — both contain
/// the substring "not found", which a naive text match would misclassify as "container absent"
/// instead of "the runtime isn't even installed on this host". exit 127 is checked FIRST and
/// unconditionally errors, regardless of stderr wording (a shell's exact phrasing isn't a stable
/// contract to match text against; even if some shell used a different exit code for
/// "command not found", the fail-safe direction is already preserved — the ONLY remaining path
/// to `Ok(false)` below is a "no such" match, so an unrecognized error still throws instead of
/// silently reporting absent). "no such" reliably covers Docker's and Podman's real
/// container-absent responses (`No such container`, `no such container`). Podman's
/// absent-IMAGE wording (`image not known`, robustness review R31) does NOT contain "no such" —
/// handled separately in `real_image_id` below, scoped to the image probe only, since that
/// phrasing is specific to `image inspect` and shouldn't be folded into this shared classifier
/// that container probes use too. A negative exit code (robustness review R32) — this codebase's
/// own sentinel for "not a real process exit" (local spawn/wait failure, a signal-killed process,
/// an option-injection rejection) — is checked first for the same reason: a local spawn failure's
/// message can ALSO contain "no such" (e.g. "No such file or directory" when ssh itself isn't
/// installed on the machine running nrg), which would otherwise be misclassified the same way.
fn probe_absent_or_err(what: &str, out: &RawOutput) -> Result<bool, Box<EvalAltResult>> {
    if out.exit_code < 0 {
        // Robustness review R32 (found reviewing R4b): -1 is this codebase's own sentinel for
        // "not a real process exit" — a LOCAL spawn/wait failure, an option-injection rejection,
        // or a signal-killed process (see RealRunner::run_ssh/run_local and their *_stdin
        // siblings, all of which map to exit_code -1). A local spawn failure's message (e.g.
        // "ssh spawn failed: No such file or directory" when ssh itself isn't installed on the
        // machine RUNNING nrg) can itself contain "no such" — the stderr-text check below would
        // otherwise misclassify "the probe never even ran" as a legitimate "container absent"
        // answer. Checked first and unconditionally errors, mirroring exit 127's handling below
        // for the analogous remote-side case.
        return Err(format!(
            "container probe failed for {what} (no real exit code — the probe process itself \
             failed to run, was killed, or was rejected before running): {}",
            out.stderr.trim()
        )
        .into());
    }
    if out.exit_code == 127 {
        return Err(format!(
            "container probe failed for {what} (exit 127 — command not found; is the \
             container runtime installed on this host?): {}",
            out.stderr.trim()
        )
        .into());
    }
    let err = out.stderr.to_lowercase();
    if err.contains("no such") {
        return Ok(false); // probe ran and reported the entity absent
    }
    Err(format!(
        "container probe failed for {what} (exit {}): {}",
        out.exit_code,
        out.stderr.trim()
    )
    .into())
}

/// One real `<rt> inspect -f '{{.State.Running}}'` probe (read-only). `Ok(true/false)` when the
/// probe ran; `Err` when it failed to run (see `probe_absent_or_err`).
fn real_inspect_running(
    runner: &Arc<dyn CommandRunner>,
    rt: &str,
    host: &str,
    name: &str,
) -> Result<bool, Box<EvalAltResult>> {
    let cmd = format!("{rt} inspect -f '{{{{.State.Running}}}}' {}", posix_quote(name));
    let out = runner.run_ssh(host, &cmd);
    if out.exit_code == 0 {
        Ok(out.stdout.trim() == "true")
    } else {
        probe_absent_or_err(&format!("{name} on {host}"), &out)
    }
}

/// One real `<rt> image inspect -f '{{.Id}}'` probe (read-only). `Ok("")` only when the image is
/// genuinely absent; `Err` on a probe failure.
fn real_image_id(
    runner: &Arc<dyn CommandRunner>,
    rt: &str,
    host: &str,
    tag: &str,
) -> Result<String, Box<EvalAltResult>> {
    let cmd = format!("{rt} image inspect -f '{{{{.Id}}}}' {}", posix_quote(tag));
    let out = runner.run_ssh(host, &cmd);
    if out.exit_code == 0 {
        Ok(out.stdout.trim().to_string())
    } else if out.stderr.to_lowercase().contains("image not known") {
        // Podman's real absent-image wording (confirmed against containers/storage's
        // `ErrImageUnknown = "image not known"`, robustness review R31) doesn't contain "no
        // such", so the shared `probe_absent_or_err` classifier below wouldn't catch it — a
        // first deploy of a new tag under Podman would otherwise throw instead of correctly
        // treating the image as not-yet-pulled. Scoped to the IMAGE probe only (not folded into
        // the shared classifier, which containers use too) since this phrasing is specific to
        // `image inspect`.
        Ok(String::new())
    } else {
        // Absent image -> "". A real failure throws.
        probe_absent_or_err(&format!("image {tag} on {host}"), &out).map(|_| String::new())
    }
}

/// One real `<rt> inspect -f '{{.State.Health.Status}}'` probe (read-only). A container with no
/// HEALTHCHECK returns exit 0 with an empty / `<no value>` status — that is a valid "not healthy
/// yet", NOT a failure. A non-zero exit is classified like the other probes.
fn real_inspect_healthy(
    runner: &Arc<dyn CommandRunner>,
    rt: &str,
    host: &str,
    name: &str,
) -> Result<bool, Box<EvalAltResult>> {
    let cmd = format!("{rt} inspect -f '{{{{.State.Health.Status}}}}' {}", posix_quote(name));
    let out = runner.run_ssh(host, &cmd);
    if out.exit_code == 0 {
        Ok(out.stdout.trim() == "healthy")
    } else {
        probe_absent_or_err(&format!("{name} on {host}"), &out)
    }
}

/// One real `nc -z localhost <port>` probe (read-only). `Ok(true)` iff the port answers (`nc`
/// connected, exit 0); `Ok(false)` iff `nc` ran and reported nothing listening (any other exit —
/// `nc`'s own connection-refused/timeout codes have no single reserved value across BSD/GNU/ncat/
/// busybox variants, so this mirrors `probe_absent_or_err`'s "anything not explicitly recognized as
/// a failure-to-run falls through to the ordinary negative answer" shape). `Err` when the probe
/// itself never validly ran.
///
/// Robustness review R16: `nc -z`'s exit code carries no meaning distinguishing "nothing's
/// listening" from "the shell couldn't even find `nc`" — a missing `nc` binary exits 127
/// (`nc: command not found`), exactly the same failure-to-run signal `probe_absent_or_err` already
/// guards against for container-runtime probes. Without this guard, EVERY candidate port on a host
/// with no `nc` installed looked "free", so `pick_port` (below) would return an already-bound port
/// and the deploy died later with an opaque `docker run -p` bind-conflict error, far from the
/// actual cause. A negative exit code (this codebase's own "not a real process exit" sentinel — a
/// local spawn failure, e.g. `ssh` itself missing on the machine running `nrg`) is checked for the
/// same reason `probe_absent_or_err` checks it. Exit 255 is `ssh`'s OWN reserved code for "ssh
/// itself failed" (couldn't connect, auth failure, connection dropped mid-command) — never a real
/// `nc` exit — so it's a transport-level failure too, checked for the same reason (found reviewing
/// this very fix: `probe_absent_or_err` already throws on it via its stderr-unrecognized fallback,
/// but this function's inverted default — anything not explicitly guarded falls through to a plain
/// negative answer — would otherwise have silently misread a dropped connection mid-scan/mid-wait
/// as "port free"/"never opened" instead of the real transport failure).
fn real_port_open(runner: &Arc<dyn CommandRunner>, host: &str, port: u16) -> Result<bool, Box<EvalAltResult>> {
    let cmd = format!("nc -z localhost {port}");
    let out = runner.run_ssh(host, &cmd);
    if out.exit_code < 0 {
        return Err(format!(
            "port-scan probe failed for {host}:{port} (no real exit code — the probe process \
             itself failed to run, was killed, or was rejected before running): {}",
            out.stderr.trim()
        )
        .into());
    }
    if out.exit_code == 127 {
        return Err(format!(
            "port-scan probe failed for {host}:{port} (exit 127 — `nc` not found on {host}; \
             install netcat, or avoid automatic port selection by setting the port explicitly): {}",
            out.stderr.trim()
        )
        .into());
    }
    if out.exit_code == 255 {
        return Err(format!(
            "port-scan probe failed for {host}:{port} (exit 255 — ssh itself failed to reach \
             {host}, not a port-scan result): {}",
            out.stderr.trim()
        )
        .into());
    }
    Ok(out.exit_code == 0)
}

/// Coerce a Rhai i64 port to a u16, clamping out-of-range values.
fn as_port(port: i64) -> u16 {
    port.clamp(0, u16::MAX as i64) as u16
}

/// DRY-RUN seeding only: run `probe`; if it FAILS TO RUN (the host is unreachable from the
/// planning machine — the normal "preview the deploy plan from my laptop / CI" case), don't abort
/// the whole plan. Record a VISIBLE note and fall back to `absent`. This addresses issue #15's
/// real complaint — a dry-run silently seeding "nothing running" on an unreachable host "with no
/// warning" — by surfacing the uncertainty in the plan, while keeping the plan usable. The LIVE
/// path never calls this: there a probe failure must throw (taking the wrong branch on a real
/// mutating run is the dangerous case).
fn seed_or_note<T>(
    ctx: &RunCtx,
    host: &str,
    what: &str,
    absent: T,
    probe: impl FnOnce() -> Result<T, Box<EvalAltResult>>,
) -> T {
    match probe() {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("{e}");
            let first = msg.lines().next().unwrap_or(&msg);
            ctx.record(
                "check",
                Some(host),
                format!("[probe unreachable — assuming {what} absent] {first}"),
            );
            absent
        }
    }
}

/// The sim_docker_* mutating builtins all share one shape: record+apply-sim in dry-run, run the
/// command in live. `sim` is the dry-run overlay mutation.
fn docker_mutation(
    ctx: &RunCtx,
    host: &str,
    cmd: &str,
    label: &str,
    sim: impl FnOnce(&RunCtx),
) -> Result<ExecResult, Box<EvalAltResult>> {
    effect(
        ctx,
        "ssh",
        Some(host),
        label,
        cmd,
        sim,
        |c| to_result(host, c.runner.run_ssh(host, cmd)),
    )
}

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    // is_dry_run() — cheap mode check for cosmetic stdlib branches (e.g. printing <auto>).
    {
        let ctx = ctx.clone();
        engine.register_fn("is_dry_run", move || -> bool { ctx.is_dry_run() });
    }

    // sim_container_running(host, name) -> bool
    // DryRun: lazily seed from ONE real inspect, then reflect stubbed mutations. Live: real probe.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "sim_container_running",
            move |host: &str, name: &str| -> Result<bool, Box<EvalAltResult>> {
                if ctx.mode == EffectMode::Live {
                    // LIVE: a probe that failed to RUN must throw (don't take the wrong branch).
                    return real_inspect_running(&ctx.runner, &runtime_cmd(&ctx), host, name);
                }
                if ctx.sim.lock().unwrap().is_seeded(host, name) {
                    return Ok(ctx.sim.lock().unwrap().is_running(host, name));
                }
                // DRY-RUN seeding: tolerate an unreachable host (note it, assume absent).
                let real = seed_or_note(&ctx, host, name, false, || {
                    real_inspect_running(&ctx.runner, &runtime_cmd(&ctx), host, name)
                });
                Ok(ctx.sim.lock().unwrap().seed_running(host, name, real))
            },
        );
    }

    // sim_image_id(host, tag) -> String
    // DryRun: sim image (synthetic stable token "<tag>" if unknown), lazily seeded. Live: real.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "sim_image_id",
            move |host: &str, tag: &str| -> Result<String, Box<EvalAltResult>> {
                if ctx.mode == EffectMode::Live {
                    return real_image_id(&ctx.runner, &runtime_cmd(&ctx), host, tag);
                }
                let already = ctx.sim.lock().unwrap().is_seeded(host, tag);
                if !already {
                    // DRY-RUN seeding: tolerate an unreachable host (note it, assume image absent).
                    let real = seed_or_note(&ctx, host, &format!("image {tag}"), String::new(), || {
                        real_image_id(&ctx.runner, &runtime_cmd(&ctx), host, tag)
                    });
                    let mut sim = ctx.sim.lock().unwrap();
                    sim.seed_running(host, tag, false);
                    if !real.is_empty() {
                        sim.set_image(host, tag, &real);
                    }
                }
                let id = ctx.sim.lock().unwrap().image_id(host, tag);
                Ok(if id.is_empty() { format!("<{tag}>") } else { id })
            },
        );
    }

    // sim_pick_port(host, base) -> i64
    // DryRun: deterministic symbolic port (base+10000, +1 per pick), record a 'check', NO probe.
    // Live: real `nc -z` scan from base+10000 upward for the first free slot.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "sim_pick_port",
            move |host: &str, base: i64| -> Result<i64, Box<EvalAltResult>> {
                if ctx.mode == EffectMode::DryRun {
                    let port = ctx.sim.lock().unwrap().pick_port(host, as_port(base));
                    ctx.record(
                        "check",
                        Some(host),
                        format!("pick free port from {}", base + 10000),
                    );
                    return Ok(port as i64);
                }
                // Live: scan upward from base+10000 for the first port that is NOT answering.
                let start = as_port(base).saturating_add(10000);
                for offset in 0..100u16 {
                    let candidate = start.saturating_add(offset);
                    if !real_port_open(&ctx.runner, host, candidate)? {
                        return Ok(candidate as i64);
                    }
                }
                // Exhausted: every candidate answered. Returning `start` (a known-BUSY port) would
                // make `docker run -p` fail later with a confusing bind error far from the cause.
                Err(format!(
                    "no free host port found on {host} in {}..{} (all 100 candidates are in use)",
                    start,
                    start.saturating_add(100)
                )
                .into())
            },
        );
    }

    // sim_docker_run(host, tag, name, cmd) -> ExecResult
    // DryRun: record + sim.set_running(host,name,tag) (running+healthy), synthetic ok.
    // Live: run `cmd` via runner (== ssh_exec).
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "sim_docker_run",
            move |host: &str, tag: &str, name: &str, cmd: &str| -> Result<ExecResult, Box<EvalAltResult>> {
                let tag = tag.to_string();
                let name = name.to_string();
                docker_mutation(&ctx, host, cmd, &format!("sim_docker_run {host}"), move |c| {
                    c.sim.lock().unwrap().set_running(host, &name, &tag);
                })
            },
        );
    }

    // sim_docker_stop(host, name, cmd) -> ExecResult
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "sim_docker_stop",
            move |host: &str, name: &str, cmd: &str| -> Result<ExecResult, Box<EvalAltResult>> {
                let name = name.to_string();
                docker_mutation(&ctx, host, cmd, &format!("sim_docker_stop {host}"), move |c| {
                    c.sim.lock().unwrap().set_stopped(host, &name);
                })
            },
        );
    }

    // sim_docker_rename(host, old, new, cmd) -> ExecResult
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "sim_docker_rename",
            move |host: &str, old: &str, new: &str, cmd: &str| -> Result<ExecResult, Box<EvalAltResult>> {
                let old = old.to_string();
                let new = new.to_string();
                docker_mutation(&ctx, host, cmd, &format!("sim_docker_rename {host}"), move |c| {
                    c.sim.lock().unwrap().rename(host, &old, &new);
                })
            },
        );
    }

    // sim_docker_remove(host, name, cmd) -> ExecResult
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "sim_docker_remove",
            move |host: &str, name: &str, cmd: &str| -> Result<ExecResult, Box<EvalAltResult>> {
                let name = name.to_string();
                docker_mutation(&ctx, host, cmd, &format!("sim_docker_remove {host}"), move |c| {
                    c.sim.lock().unwrap().remove(host, &name);
                })
            },
        );
    }

    // sim_proxy_switch(host, service, target, cmd) -> ExecResult
    // DryRun: record + sim.proxy_switch (stores current target for read-back / rollback snapshot).
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "sim_proxy_switch",
            move |host: &str, service: &str, target: &str, cmd: &str| -> Result<ExecResult, Box<EvalAltResult>> {
                let service = service.to_string();
                let target = target.to_string();
                docker_mutation(&ctx, host, cmd, &format!("sim_proxy_switch {host}"), move |c| {
                    c.sim.lock().unwrap().proxy_switch(host, &service, &target);
                })
            },
        );
    }

    // sim_wait_port(host, port) -> bool
    // DryRun: true iff the sim marks that port occupied (agrees with the just-stubbed container),
    // no probe / no sleep. Live: ONE real `nc -z` probe (robustness review R11 — see below).
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "sim_wait_port",
            move |host: &str, port: i64| -> Result<bool, Box<EvalAltResult>> {
                if ctx.mode == EffectMode::DryRun {
                    ctx.record("check", Some(host), format!("wait for port {port}"));
                    return Ok(ctx.sim.lock().unwrap().port_open(host, as_port(port)));
                }
                // Robustness review R11: this used to retry internally (30 x 2s = up to 60s) on
                // top of `lib/healthcheck.rhai`'s `wait_port` wrapping it in its OWN
                // `cfg.attempts`/`cfg.interval` retry loop (`sim_wait_port`'s only caller), so
                // `wait_port(host, port, #{attempts: 5, interval: 1})` — which an operator reads
                // as a ~5s bound — actually blocked up to 5 x 60s = 5 minutes, holding the fleet
                // transaction open the whole time. Retrying here too was pure duplication, not
                // extra safety margin: one real probe per call, with the caller's `attempts`/
                // `interval` as the sole, correct source of truth for total wait time.
                real_port_open(&ctx.runner, host, as_port(port))
            },
        );
    }

    // sim_container_healthy(host, name) -> bool
    // DryRun: true iff the sim has (host,name) running AND healthy (set by sim_docker_run), no
    // probe. Live: ONE real `inspect -f {{.State.Health.Status}}` probe (robustness review R11,
    // same reasoning as sim_wait_port above — `wait_container_healthy` is its only caller and
    // already retries with the operator's own `attempts`/`interval`).
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "sim_container_healthy",
            move |host: &str, name: &str| -> Result<bool, Box<EvalAltResult>> {
                if ctx.mode == EffectMode::DryRun {
                    ctx.record("check", Some(host), format!("wait for {name} healthy"));
                    return Ok(ctx.sim.lock().unwrap().is_healthy(host, name));
                }
                let rt = runtime_cmd(&ctx);
                real_inspect_healthy(&ctx.runner, &rt, host, name)
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::{shared, shared_dry};
    use crate::engine::runner::{FakeRunner, RawOutput};
    use crate::engine::types::register_types;
    use std::sync::Mutex;

    fn engine_with(ctx: SharedCtx) -> Engine {
        let mut e = Engine::new();
        register_types(&mut e);
        register(&mut e, ctx);
        e
    }

    #[test]
    fn is_dry_run_builtin_reflects_mode() {
        let ctx = shared(FakeRunner::shared());
        let e = engine_with(ctx.clone());
        assert!(!e.eval::<bool>("is_dry_run()").unwrap());
        let ctx = shared_dry(FakeRunner::shared());
        let e = engine_with(ctx);
        assert!(e.eval::<bool>("is_dry_run()").unwrap());
    }

    #[test]
    fn stubbed_run_makes_container_running_and_healthy_in_dry_run() {
        let fake = FakeRunner::shared();
        let ctx = shared_dry(fake.clone());
        let e = engine_with(ctx.clone());
        let script = r#"
            sim_docker_run("web1", "img:v2", "app-new", "docker run -d --name app-new img:v2");
            [sim_container_running("web1", "app-new"), sim_container_healthy("web1", "app-new")]
        "#;
        let r: rhai::Array = e.eval(script).unwrap();
        assert!(r[0].clone().as_bool().unwrap(), "new container must be running");
        assert!(r[1].clone().as_bool().unwrap(), "new container must be healthy");
        assert!(fake.calls().is_empty(), "dry-run must not execute");
        let plan = ctx.plan.lock().unwrap().clone();
        assert!(plan.iter().any(|a| a.kind == "ssh" && a.detail.contains("docker run -d")));
    }

    #[test]
    fn pick_port_is_deterministic_in_dry_run() {
        let ctx = shared_dry(FakeRunner::shared());
        let e = engine_with(ctx);
        let a: i64 = e.eval(r#"sim_pick_port("web1", 3000)"#).unwrap();
        let b: i64 = e.eval(r#"sim_pick_port("web1", 3000)"#).unwrap();
        assert_eq!(a, 13000);
        assert_eq!(b, 13001);
    }

    #[test]
    fn live_pick_port_throws_when_all_busy() {
        // A runner whose `nc -z` always succeeds (every port "busy") must make sim_pick_port
        // throw, NOT return a known-busy port.
        let fake = Arc::new(BusyPortRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<i64>(r#"sim_pick_port("web1", 3000)"#);
        assert!(r.is_err(), "exhausted scan must throw");
        assert!(format!("{}", r.unwrap_err()).contains("no free host port"));
    }

    #[test]
    fn live_pick_port_throws_when_nc_is_missing_instead_of_treating_every_port_as_free() {
        // Robustness review R16: `nc`'s own exit code carries no meaning distinguishing "nothing's
        // listening" from "the shell couldn't even find `nc`" (a missing binary exits 127, same as
        // a missing container-runtime CLI). Without the fix, EVERY candidate looked free on a host
        // with no `nc` installed, so pick_port would silently hand back an already-bound port.
        let fake = Arc::new(CommandNotFoundRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<i64>(r#"sim_pick_port("web1", 3000)"#);
        assert!(r.is_err(), "a missing `nc` must throw, not silently report every port as free");
        let msg = format!("{}", r.unwrap_err());
        assert!(msg.contains("127") && msg.contains("nc"), "must name the real cause: {msg}");
    }

    #[test]
    fn live_pick_port_throws_on_a_local_spawn_failure_instead_of_treating_every_port_as_free() {
        // A local spawn failure (e.g. `ssh` itself missing) must not be misread as "port free"
        // either — same -1 sentinel guard as probe_absent_or_err.
        let fake = Arc::new(LocalSpawnFailureRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<i64>(r#"sim_pick_port("web1", 3000)"#);
        assert!(r.is_err(), "a local spawn failure must throw, not silently report every port as free");
    }

    #[test]
    fn live_wait_port_throws_when_nc_is_missing() {
        let fake = Arc::new(CommandNotFoundRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<bool>(r#"sim_wait_port("web1", 3000)"#);
        assert!(r.is_err(), "a missing `nc` must throw, not silently report the port as never open");
    }

    #[test]
    fn live_pick_port_throws_on_an_ssh_transport_failure_instead_of_treating_every_port_as_free() {
        // Follow-up from Fable's review of the R16 fix above: exit 255 is ssh's OWN reserved code
        // for "ssh itself failed" (couldn't connect, auth failure, dropped mid-command) — never a
        // real `nc` exit — so a dropped connection mid-scan must not be misread as "port free".
        let fake = Arc::new(SshDownRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<i64>(r#"sim_pick_port("web1", 3000)"#);
        assert!(r.is_err(), "an ssh transport failure must throw, not silently report every port as free");
    }

    #[test]
    fn live_wait_port_throws_on_an_ssh_transport_failure() {
        let fake = Arc::new(SshDownRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<bool>(r#"sim_wait_port("web1", 3000)"#);
        assert!(r.is_err(), "an ssh transport failure must throw, not silently report the port as never open");
    }

    #[test]
    fn live_wait_port_probes_exactly_once_no_internal_retry() {
        // Robustness review R11: sim_wait_port used to retry internally (30 x 2s = up to 60s) on
        // TOP of lib/healthcheck.rhai's own cfg.attempts/cfg.interval retry loop (its only caller)
        // — so a caller-configured "5 attempts, 1s apart" bound actually took up to 5 minutes.
        // A single ordinary "closed" probe (exit 1, nc's plain connection-refused code — not one
        // of the failure-to-run guards) must now return false IMMEDIATELY, not after retrying.
        let fake = FakeRunner::shared();
        fake.fail_host("web1", 1, "");
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        let start = std::time::Instant::now();
        let r: bool = e.eval(r#"sim_wait_port("web1", 3000)"#).unwrap();
        assert!(!r, "an ordinary closed port must report false");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "must return immediately (no internal retry loop): took {:?}",
            start.elapsed()
        );
        assert_eq!(fake.calls().len(), 1, "must probe exactly once: {:?}", fake.calls());
    }

    #[test]
    fn live_container_healthy_probes_exactly_once_no_internal_retry() {
        // Same reasoning as live_wait_port_probes_exactly_once_no_internal_retry, for
        // sim_container_healthy / wait_container_healthy.
        let fake = FakeRunner::shared();
        fake.fail_host("web1", 0, ""); // exit 0, stdout empty (not "healthy") -> ordinary false
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        let start = std::time::Instant::now();
        let r: bool = e.eval(r#"sim_container_healthy("web1", "app")"#).unwrap();
        assert!(!r, "an ordinary not-yet-healthy container must report false");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "must return immediately (no internal retry loop): took {:?}",
            start.elapsed()
        );
        assert_eq!(fake.calls().len(), 1, "must probe exactly once: {:?}", fake.calls());
    }

    #[test]
    fn proxy_switch_stores_target_in_dry_run() {
        let ctx = shared_dry(FakeRunner::shared());
        let e = engine_with(ctx.clone());
        e.run(r#"sim_proxy_switch("web1", "app", "localhost:13000", "kamal-proxy deploy app --target localhost:13000");"#)
            .unwrap();
        assert_eq!(
            ctx.sim.lock().unwrap().proxy_target("web1", "app"),
            Some("localhost:13000".to_string())
        );
    }

    #[test]
    fn promote_rename_makes_canonical_running_and_old_gone() {
        let ctx = shared_dry(FakeRunner::shared());
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
        let ctx = shared_dry(FakeRunner::shared());
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
        let ctx = shared_dry(FakeRunner::shared());
        let e = engine_with(ctx);
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
        let fake = Arc::new(TrueRunner::default());
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        let running: bool = e.eval(r#"sim_container_running("web1", "app")"#).unwrap();
        assert!(running);
        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains("docker inspect -f '{{.State.Running}}' 'app'"));
    }

    #[test]
    fn live_probe_failure_throws_instead_of_reporting_absent() {
        // A probe that fails to RUN (ssh down: exit 255, stderr not a "no such object") must
        // throw, not be folded into "not running" (issue #15).
        let fake = Arc::new(SshDownRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<bool>(r#"sim_container_running("web1", "app")"#);
        assert!(r.is_err(), "probe failure must throw");
        assert!(format!("{}", r.unwrap_err()).contains("probe failed"));
    }

    #[test]
    fn dry_run_tolerates_an_unreachable_host_and_notes_it() {
        // Regression: a `--dry-run` deploy previewed from a machine that can't reach the hosts
        // must NOT abort — the seeding probe failure is recorded as a note and the container is
        // assumed absent, so the plan proceeds (issue #15 dry-run case; the CI failure on PR #29).
        let fake = Arc::new(SshDownRunner);
        let ctx = shared_dry(fake);
        let e = engine_with(ctx.clone());
        let running: bool = e.eval(r#"sim_container_running("web1", "kamal-proxy")"#).unwrap();
        assert!(!running, "unreachable host -> assume absent, don't throw");
        let plan = ctx.plan.lock().unwrap().clone();
        assert!(
            plan.iter().any(|a| a.detail.contains("probe unreachable")),
            "the plan must surface the unreachable-probe note: {plan:?}"
        );
    }

    #[test]
    fn live_probe_absent_container_reports_not_running() {
        // A genuine "no such object" (exit 1) is a legitimate absent answer, not a failure.
        let fake = Arc::new(NoSuchRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let running: bool = e.eval(r#"sim_container_running("web1", "gone")"#).unwrap();
        assert!(!running);
    }

    #[test]
    fn live_probe_missing_cli_throws_instead_of_reporting_absent() {
        // Robustness review R4: a missing container runtime (exit 127, "docker: command not
        // found") must throw — NOT be folded into "container absent" the way a naive substring
        // match on "not found" used to. A deploy against a host with no runtime installed should
        // fail with a clear "is the runtime installed" error, not silently take the
        // fresh-install branch and fail confusingly mid-deploy instead.
        let fake = Arc::new(CommandNotFoundRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<bool>(r#"sim_container_running("web1", "app")"#);
        assert!(r.is_err(), "a missing CLI must throw, not report absent: {r:?}");
        let msg = format!("{}", r.unwrap_err());
        assert!(msg.contains("exit 127"), "got: {msg}");
        assert!(msg.contains("is the container runtime installed"), "got: {msg}");
    }

    #[test]
    fn live_probe_local_spawn_failure_throws_instead_of_reporting_absent() {
        // Robustness review R32 (found reviewing R4b): a LOCAL spawn failure (e.g. ssh itself
        // isn't installed on the machine running nrg) maps to exit_code -1 with a message like
        // "ssh spawn failed: No such file or directory" (RealRunner::run_ssh) — which itself
        // contains "no such" and would otherwise be misclassified as "container absent" instead
        // of "the probe never even ran".
        let fake = Arc::new(LocalSpawnFailureRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<bool>(r#"sim_container_running("web1", "app")"#);
        assert!(r.is_err(), "a local spawn failure must throw, not report absent: {r:?}");
        let msg = format!("{}", r.unwrap_err());
        assert!(msg.contains("no real exit code"), "got: {msg}");
        assert!(msg.contains("No such file or directory"), "got: {msg}");
    }

    #[test]
    fn live_image_id_recognizes_podmans_absent_image_wording() {
        // Robustness review R31 (confirmed against containers/storage's `ErrImageUnknown =
        // "image not known"`): Podman's `image inspect` on a missing image doesn't say "no
        // such", so the shared classifier alone wouldn't catch it — a first deploy of a new tag
        // under Podman would otherwise throw instead of correctly reporting the image absent.
        let fake = Arc::new(PodmanImageNotKnownRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let id: String = e.eval(r#"sim_image_id("web1", "myapp:v1")"#).unwrap();
        assert_eq!(id, "", "a genuinely-absent Podman image must report absent, not throw");
    }

    #[test]
    fn live_probe_honors_configured_runtime() {
        let fake = Arc::new(TrueRunner::default());
        let ctx = shared(fake.clone());
        ctx.state.lock().unwrap().set("nrg.runtime.cmd", "podman").unwrap();
        let e = engine_with(ctx);
        let _running: bool = e.eval(r#"sim_container_running("web1", "app")"#).unwrap();
        let calls = fake.calls.lock().unwrap();
        assert!(
            calls[0].contains("podman inspect -f '{{.State.Running}}' 'app'"),
            "got: {}",
            calls[0]
        );
        assert!(!calls[0].contains("docker"));
    }

    #[test]
    fn dry_run_container_running_seeds_from_one_real_probe() {
        let fake = Arc::new(TrueRunner::default());
        let ctx = shared_dry(fake.clone());
        let e = engine_with(ctx);
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
            RawOutput { stdout: stdout.to_string(), stderr: String::new(), exit_code: 0 }
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

    /// ssh always fails to connect (exit 255), stderr is a transport error, not "no such object".
    struct SshDownRunner;
    impl CommandRunner for SshDownRunner {
        fn run_ssh(&self, _host: &str, _cmd: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: "ssh: connect to host web1 port 22: Connection refused".into(),
                exit_code: 255,
            }
        }
        fn run_local(&self, _cmd: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_ssh_stdin(&self, _h: &str, _c: &str, _s: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_local_stdin(&self, _c: &str, _s: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
    }

    /// inspect of a missing container: exit 1 with a "No such object" stderr.
    struct NoSuchRunner;
    impl CommandRunner for NoSuchRunner {
        fn run_ssh(&self, _host: &str, _cmd: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: "Error: No such object: gone".into(),
                exit_code: 1,
            }
        }
        fn run_local(&self, _cmd: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_ssh_stdin(&self, _h: &str, _c: &str, _s: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_local_stdin(&self, _c: &str, _s: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
    }

    /// Podman's real `image inspect` failure on a missing image (robustness review R31,
    /// confirmed against containers/storage's `ErrImageUnknown = "image not known"`) — does NOT
    /// contain "no such", unlike Docker's/Podman's container-absent wording.
    struct PodmanImageNotKnownRunner;
    impl CommandRunner for PodmanImageNotKnownRunner {
        fn run_ssh(&self, _host: &str, _cmd: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: "Error: myapp:v1: image not known".into(),
                exit_code: 125,
            }
        }
        fn run_local(&self, _cmd: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_ssh_stdin(&self, _h: &str, _c: &str, _s: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_local_stdin(&self, _c: &str, _s: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
    }

    /// A LOCAL spawn failure (robustness review R32) — e.g. ssh itself isn't installed on the
    /// machine running nrg. `RealRunner::run_ssh`'s own error path formats this as
    /// "ssh spawn failed: <io::Error>", and `io::ErrorKind::NotFound`'s Display text is literally
    /// "No such file or directory" — which itself contains "no such", the exact text a naive
    /// classifier used to misread as "container absent" instead of "the probe never even ran".
    struct LocalSpawnFailureRunner;
    impl CommandRunner for LocalSpawnFailureRunner {
        fn run_ssh(&self, _host: &str, _cmd: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: "ssh spawn failed: No such file or directory (os error 2)".into(),
                exit_code: -1,
            }
        }
        fn run_local(&self, _cmd: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_ssh_stdin(&self, _h: &str, _c: &str, _s: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_local_stdin(&self, _c: &str, _s: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
    }

    /// The container runtime CLI isn't installed on this host at all: exit 127, and the shell's
    /// own "command not found" message — which (robustness review R4) contains the substring
    /// "not found", the exact text a naive classifier used to misread as "container absent".
    struct CommandNotFoundRunner;
    impl CommandRunner for CommandNotFoundRunner {
        fn run_ssh(&self, _host: &str, _cmd: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: "bash: docker: command not found".into(),
                exit_code: 127,
            }
        }
        fn run_local(&self, _cmd: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_ssh_stdin(&self, _h: &str, _c: &str, _s: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_local_stdin(&self, _c: &str, _s: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
    }

    /// Every `nc -z` succeeds, so every candidate port looks busy.
    struct BusyPortRunner;
    impl CommandRunner for BusyPortRunner {
        fn run_ssh(&self, _host: &str, _cmd: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_local(&self, _cmd: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_ssh_stdin(&self, _h: &str, _c: &str, _s: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_local_stdin(&self, _c: &str, _s: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
    }
}
