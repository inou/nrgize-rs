//! Typed reads of the hosts' real state: container running/healthy, image id, port open and free
//! port selection.
//!
//! The stdlib does ALL container existence/state/health reads through these builtins, never a
//! raw `docker inspect` / `nc -z` over `ssh_exec`, so probe failures are classified in one place.
//! They are reads: they run the same way in live and dry-run mode.
//!
//! Probe FAILURES (ssh down, auth, missing runtime CLI) are handled by MODE:
//! - LIVE: they THROW (with stderr), so a real mutating run never takes the wrong branch against
//!   a host whose state we couldn't read. Only a genuine "no such container/image" is absent.
//! - DRY-RUN: they DON'T abort the plan (you often preview a deploy from a machine that can't
//!   reach the hosts). The probe failure is recorded as a visible plan note and the entity is
//!   assumed absent, surfacing the uncertainty instead of silently lying.

use crate::engine::context::{RunCtx, SharedCtx};
use crate::engine::runner::{CommandRunner, RawOutput};
use crate::engine::secret::posix_quote;
use rhai::{Engine, EvalAltResult};
use std::sync::Arc;

/// The configured container runtime command — set by the stdlib's `set_runtime()` into the
/// EPHEMERAL per-run `session` store (`nrg.runtime.cmd`), defaulting to `docker`. The Live/
/// seeding inspect probes below use it so they match the runtime the stdlib's mutation commands
/// use (a podman/nerdctl deploy must be probed with `podman`/`nerdctl inspect`, not
/// `docker inspect`).
///
/// This deliberately reads `ctx.session`, NOT `ctx.state`: `ctx.state` is loaded from the
/// project's on-disk `state.json` at the start of the run, so a THIS-run `set_runtime()` call
/// aside, it may still carry a runtime choice a PREVIOUS run persisted there. Probing with that
/// stale value regardless of what (if anything) this run's own script configured is exactly
/// the "runtime choice leaks across runs" bug; `session` is fresh (empty)
/// every run, so a script that never calls `set_runtime()` always probes with the true default.
fn runtime_cmd(ctx: &RunCtx) -> String {
    ctx.session
        .lock()
        .unwrap()
        .get("nrg.runtime.cmd")
        .cloned()
        .unwrap_or_else(|| "docker".to_string())
}

/// Classify a non-zero probe exit: a genuine "no such object/container/image" is a legitimate
/// ABSENT answer; anything else (ssh failure exit 255, missing CLI exit 127, auth, timeout) is a
/// probe that FAILED TO RUN and must surface as an error, never be mistaken for "not running".
///
/// A missing CLI exits 127 with a shell message like
/// `docker: command not found` (bash) or `sh: docker: not found` (dash/POSIX sh) — both contain
/// the substring "not found", which a naive text match would misclassify as "container absent"
/// instead of "the runtime isn't even installed on this host". exit 127 is checked FIRST and
/// unconditionally errors, regardless of stderr wording (a shell's exact phrasing isn't a stable
/// contract to match text against; even if some shell used a different exit code for
/// "command not found", the fail-safe direction is already preserved — the ONLY remaining path
/// to `Ok(false)` below is a "no such" match, so an unrecognized error still throws instead of
/// silently reporting absent). "no such" reliably covers Docker's and Podman's real
/// container-absent responses (`No such container`, `no such container`). Podman's
/// absent-IMAGE wording (`image not known`) does NOT contain "no such" —
/// handled separately in `real_image_id` below, scoped to the image probe only, since that
/// phrasing is specific to `image inspect` and shouldn't be folded into this shared classifier
/// that container probes use too. A negative exit code — this codebase's
/// own sentinel for "not a real process exit" (a LOCAL spawn/wait failure or an option-injection
/// rejection; a signal-killed process is now reported via a POSITIVE `128 + signal` code instead
/// — see `RealRunner`'s `exit_code_of` — so it falls through to the generic non-zero-exit error below, not here)
/// — is checked first for the same reason: a local spawn failure's message can ALSO contain
/// "no such" (e.g. "No such file or directory" when ssh itself isn't installed on the machine
/// running nrg), which would otherwise be misclassified the same way.
fn probe_absent_or_err(what: &str, out: &RawOutput) -> Result<bool, Box<EvalAltResult>> {
    if out.exit_code < 0 {
        // -1 is this codebase's own sentinel for
        // "not a real process exit" — a LOCAL spawn/wait failure or an option-injection
        // rejection (see RealRunner::run_ssh/run_local and their *_stdin siblings, all of which
        // map to exit_code -1 in those cases). A signal-killed process is EXCLUDED from this
        // sentinel — it now reports the POSITIVE `128 + signal` convention instead (see
        // `exit_code_of`), so it never reaches this branch; it's handled by the generic
        // non-zero-exit `Err` at the end of this function, with the real numeric code in the
        // message. A local spawn failure's message (e.g. "ssh spawn failed: No such file or
        // directory" when ssh itself isn't installed on the machine RUNNING nrg) can itself
        // contain "no such" — the stderr-text check below would otherwise misclassify "the probe
        // never even ran" as a legitimate "container absent" answer. Checked first and
        // unconditionally errors, mirroring exit 127's handling below for the analogous
        // remote-side case.
        return Err(format!(
            "container probe failed for {what} (no real exit code — the probe process itself \
             failed to run or was rejected before running): {}",
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
    let cmd = format!(
        "{rt} inspect -f '{{{{.State.Running}}}}' {}",
        posix_quote(name)
    );
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
        // `ErrImageUnknown = "image not known"`) doesn't contain "no
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
    let cmd = format!(
        "{rt} inspect -f '{{{{.State.Health.Status}}}}' {}",
        posix_quote(name)
    );
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
/// `nc -z`'s exit code carries no meaning distinguishing "nothing's
/// listening" from "the shell couldn't even find `nc`" — a missing `nc` binary exits 127
/// (`nc: command not found`), exactly the same failure-to-run signal `probe_absent_or_err` already
/// guards against for container-runtime probes. Without this guard, EVERY candidate port on a host
/// with no `nc` installed looked "free", so `pick_port` (below) would return an already-bound port
/// and the deploy died later with an opaque `docker run -p` bind-conflict error, far from the
/// actual cause. A negative exit code (this codebase's own "not a real process exit" sentinel — a
/// local spawn failure, e.g. `ssh` itself missing on the machine running `nrg`) is checked for the
/// same reason `probe_absent_or_err` checks it. Exit 255 is `ssh`'s OWN reserved code for "ssh
/// itself failed" (couldn't connect, auth failure, connection dropped mid-command) — never a real
/// `nc` exit — so it's a transport-level failure too, checked for the same reason (`probe_absent_or_err` already throws on it via its stderr-unrecognized fallback,
/// but this function's inverted default — anything not explicitly guarded falls through to a plain
/// negative answer — would otherwise have silently misread a dropped connection mid-scan/mid-wait
/// as "port free"/"never opened" instead of the real transport failure). An exit code `>= 129`
/// (`RealRunner::exit_code_of` encodes a
/// signal-terminated process as `128 + signal`, and the lowest real signal number is 1, so 129 is
/// the lowest value this encoding can ever produce) is checked for the SAME reason: a signal-killed
/// `nc` (e.g. OOM-killed mid-scan) no longer trips the `< 0` guard the way it used to before that
/// fix — without this check it would silently fall through to the inverted default below and
/// report the port "free" instead of surfacing that the probe never validly completed, exactly
/// the fail-unsafe regression that fix would otherwise have introduced here.
fn real_port_open(
    runner: &Arc<dyn CommandRunner>,
    host: &str,
    port: u16,
) -> Result<bool, Box<EvalAltResult>> {
    let cmd = format!("nc -z localhost {port}");
    let out = runner.run_ssh(host, &cmd);
    if out.exit_code < 0 {
        return Err(format!(
            "port-scan probe failed for {host}:{port} (no real exit code — the probe process \
             itself failed to run or was rejected before running): {}",
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
    if out.exit_code >= 129 {
        return Err(format!(
            "port-scan probe failed for {host}:{port} (exit {} — the probe process was killed by \
             a signal before it could report a real result, not a port-scan answer): {}",
            out.exit_code,
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

/// A typed read of the host's real state. In a live run the probe's result is returned untouched
/// (a probe that failed to run throws: a failed probe is never "absent"). In a dry run a probe
/// that fails to run (the host is unreachable from the planning machine) does not abort the
/// plan: the failure is recorded as a plan note and `absent` is used.
fn read_or_assume<T>(
    ctx: &RunCtx,
    host: &str,
    what: &str,
    absent: T,
    probe: impl FnOnce() -> Result<T, Box<EvalAltResult>>,
) -> Result<T, Box<EvalAltResult>> {
    match probe() {
        Err(e) if ctx.is_dry_run() => {
            let msg = format!("{e}");
            let first = msg.lines().next().unwrap_or(&msg);
            ctx.record(
                "check",
                Some(host),
                format!("[probe failed — assuming {what} absent] {first}"),
            );
            Ok(absent)
        }
        r => r,
    }
}

/// The free-port scan behind `pick_port`.
fn scan_free_port(ctx: &RunCtx, host: &str, base: i64) -> Result<i64, Box<EvalAltResult>> {
    // Live: scan upward from base+10000 for the first port that is NOT answering.
    //
    // Computed in u32 first, not u16: `as_port(base).saturating_add(10000)` (the
    // former direct-in-u16 arithmetic) SILENTLY saturates to `u16::MAX` for any
    // `base >= 55536`, and every subsequent `.saturating_add(offset)` in the loop
    // below then ALSO saturates to that same `u16::MAX` — collapsing all 100
    // distinct candidate ports into repeatedly probing the exact same one port
    // (a base port >= 55536 saturates the u16
    // scan-start arithmetic). Detecting the overflow here and throwing a clear error
    // instead is the same fail-safe direction as the rest of this port scan's fixes:
    // an operator misconfiguring `base` this high needs a loud, specific error, not a
    // scan that silently degrades into checking one port 100 times.
    // `as_port(base)` FIRST, then widen to u32 — using the clamped port number
    // consistently in both the arithmetic and the error message below (rather than
    // the raw, possibly-far-out-of-range `base` argument itself) so the message's
    // own "(base_port + 10000 + 99)" formula always actually matches the displayed
    // numbers, even for a script-supplied `base` far outside `0..=65535` (e.g. a
    // negative value, or `i64::MAX`).
    let base_port = as_port(base);
    let start_u32 = base_port as u32 + 10000;
    let last_candidate_u32 = start_u32 + 99;
    if last_candidate_u32 > u16::MAX as u32 {
        return Err(format!(
            "cannot scan for a free port on {host}: base port {base_port} needs \
             candidate ports up to {last_candidate_u32} (base_port + 10000 + 99) to \
             scan 100 candidates, which exceeds the maximum port number {}; choose a \
             lower base port",
            u16::MAX
        )
        .into());
    }
    let start = start_u32 as u16;
    for offset in 0..100u16 {
        // Safe: `start + offset` (offset <= 99) can't exceed `last_candidate_u32`,
        // already verified above to fit in u16 — no saturation needed here.
        let candidate = start + offset;
        if !real_port_open(&ctx.runner, host, candidate)? {
            return Ok(candidate as i64);
        }
    }
    // Exhausted: every candidate answered. Returning `start` (a known-BUSY port) would
    // make `docker run -p` fail later with a confusing bind error far from the cause.
    //
    // The exclusive upper bound is computed in u32 (`start_u32 + 100`), not
    // `start.saturating_add(100)` in u16: at the highest base that still passes the
    // overflow guard above (`last_candidate_u32 == 65535`), the u16 version displayed
    // a saturated, off-by-one-low `65535` instead of the true `65536` — cosmetic
    // (display-only; this codepath doesn't affect which ports get scanned), but wrong.
    Err(format!(
        "no free host port found on {host} in {start}..{} (all 100 candidates are in use)",
        start_u32 + 100
    )
    .into())
}

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    // container_running(host, name) -> bool
    // A read of the host's real state in both modes. A probe that fails to run throws in a live
    // run; in a dry run it is noted in the plan and answered as absent.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "container_running",
            move |host: &str, name: &str| -> Result<bool, Box<EvalAltResult>> {
                read_or_assume(&ctx, host, name, false, || {
                    real_inspect_running(&ctx.runner, &runtime_cmd(&ctx), host, name)
                })
            },
        );
    }

    // container_image_id(host, tag) -> String ("" when the image is absent)
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "container_image_id",
            move |host: &str, tag: &str| -> Result<String, Box<EvalAltResult>> {
                read_or_assume(&ctx, host, &format!("image {tag}"), String::new(), || {
                    real_image_id(&ctx.runner, &runtime_cmd(&ctx), host, tag)
                })
            },
        );
    }

    // container_healthy(host, name) -> bool — ONE health-status probe.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "container_healthy",
            move |host: &str, name: &str| -> Result<bool, Box<EvalAltResult>> {
                read_or_assume(&ctx, host, name, false, || {
                    real_inspect_healthy(&ctx.runner, &runtime_cmd(&ctx), host, name)
                })
            },
        );
    }

    // port_open(host, port) -> bool — ONE `nc -z localhost <port>` probe.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "port_open",
            move |host: &str, port: i64| -> Result<bool, Box<EvalAltResult>> {
                read_or_assume(&ctx, host, &format!("port {port}"), false, || {
                    real_port_open(&ctx.runner, host, as_port(port))
                })
            },
        );
    }

    // pick_port(host, base) -> i64 — scan upward from base+10000 for the first free port.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "pick_port",
            move |host: &str, base: i64| -> Result<i64, Box<EvalAltResult>> {
                scan_free_port(&ctx, host, base)
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
    fn live_pick_port_throws_when_all_busy() {
        // A runner whose `nc -z` always succeeds (every port "busy") must make pick_port
        // throw, NOT return a known-busy port.
        let fake = Arc::new(BusyPortRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<i64>(r#"pick_port("web1", 3000)"#);
        assert!(r.is_err(), "exhausted scan must throw");
        assert!(format!("{}", r.unwrap_err()).contains("no free host port"));
    }

    #[test]
    fn live_pick_port_exhausted_scan_message_reports_the_correct_upper_bound_at_the_highest_base() {
        // The exhausted-scan message's upper bound used to be computed as
        // `start.saturating_add(100)` in u16 — at the highest base that still passes the
        // overflow guard (`base = 55436`, `start = 65436`), `65436 + 100 = 65536` overflows u16
        // and `.saturating_add` clamped it down to 65535, displaying an off-by-one-low range
        // ("65436..65535") purely in the error text (no effect on which ports were actually
        // scanned). Pin the correct, un-clamped value here.
        let fake = Arc::new(BusyPortRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<i64>(r#"pick_port("web1", 55436)"#);
        let msg = format!("{}", r.unwrap_err());
        assert!(
            msg.contains("65436..65536"),
            "exhausted-scan range must show the true, un-clamped upper bound: {msg}"
        );
    }

    #[test]
    fn live_pick_port_throws_when_nc_is_missing_instead_of_treating_every_port_as_free() {
        // `nc`'s own exit code carries no meaning distinguishing "nothing's
        // listening" from "the shell couldn't even find `nc`" (a missing binary exits 127, same as
        // a missing container-runtime CLI). Without the fix, EVERY candidate looked free on a host
        // with no `nc` installed, so pick_port would silently hand back an already-bound port.
        let fake = Arc::new(CommandNotFoundRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<i64>(r#"pick_port("web1", 3000)"#);
        assert!(
            r.is_err(),
            "a missing `nc` must throw, not silently report every port as free"
        );
        let msg = format!("{}", r.unwrap_err());
        assert!(
            msg.contains("127") && msg.contains("nc"),
            "must name the real cause: {msg}"
        );
    }

    #[test]
    fn live_pick_port_throws_on_a_local_spawn_failure_instead_of_treating_every_port_as_free() {
        // A local spawn failure (e.g. `ssh` itself missing) must not be misread as "port free"
        // either — same -1 sentinel guard as probe_absent_or_err.
        let fake = Arc::new(LocalSpawnFailureRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<i64>(r#"pick_port("web1", 3000)"#);
        assert!(
            r.is_err(),
            "a local spawn failure must throw, not silently report every port as free"
        );
    }

    #[test]
    fn live_wait_port_throws_when_nc_is_missing() {
        let fake = Arc::new(CommandNotFoundRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<bool>(r#"port_open("web1", 3000)"#);
        assert!(
            r.is_err(),
            "a missing `nc` must throw, not silently report the port as never open"
        );
    }

    #[test]
    fn live_pick_port_throws_on_an_ssh_transport_failure_instead_of_treating_every_port_as_free() {
        // Exit 255 is ssh's OWN reserved code
        // for "ssh itself failed" (couldn't connect, auth failure, dropped mid-command) — never a
        // real `nc` exit — so a dropped connection mid-scan must not be misread as "port free".
        let fake = Arc::new(SshDownRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<i64>(r#"pick_port("web1", 3000)"#);
        assert!(
            r.is_err(),
            "an ssh transport failure must throw, not silently report every port as free"
        );
    }

    #[test]
    fn live_wait_port_throws_on_an_ssh_transport_failure() {
        let fake = Arc::new(SshDownRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<bool>(r#"port_open("web1", 3000)"#);
        assert!(
            r.is_err(),
            "an ssh transport failure must throw, not silently report the port as never open"
        );
    }

    #[test]
    fn live_pick_port_throws_on_a_signal_killed_nc_instead_of_treating_every_port_as_free() {
        // After the "signal-killed process" exit-code fix (RealRunner::exit_code_of),
        // a signal-killed `nc` no longer trips the `exit_code < 0` sentinel the way it used to,
        // so without a dedicated `>= 129` guard it would silently fall through to `real_port_open`'s
        // inverted default (`exit_code == 0` -> false -> "port free") — reporting a port whose
        // probe was killed mid-scan as free, the exact fail-unsafe regression that fix would
        // otherwise have introduced here.
        let fake = Arc::new(SignalKilledRunner(137));
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<i64>(r#"pick_port("web1", 3000)"#);
        assert!(
            r.is_err(),
            "a signal-killed nc probe must throw, not silently report every port as free"
        );
        let msg = format!("{}", r.unwrap_err());
        assert!(msg.contains("137"), "must name the real exit code: {msg}");
    }

    #[test]
    fn live_wait_port_throws_on_a_signal_killed_nc() {
        let fake = Arc::new(SignalKilledRunner(137));
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<bool>(r#"port_open("web1", 3000)"#);
        assert!(
            r.is_err(),
            "a signal-killed nc probe must throw, not silently report the port as never open"
        );
    }

    #[test]
    fn live_pick_port_throws_at_exactly_the_lowest_possible_signal_kill_code_129() {
        // Pin the EXACT boundary, not just
        // an arbitrary example (137, SIGKILL) — `129` (`128 + 1`, SIGHUP, the lowest real signal
        // number) is the floor of `RealRunner::exit_code_of`'s signal encoding, so it's the
        // tightest case an off-by-one (e.g. `> 129` instead of `>= 129`) could silently miss.
        let fake = Arc::new(SignalKilledRunner(129));
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<i64>(r#"pick_port("web1", 3000)"#);
        assert!(
            r.is_err(),
            "exit code 129 (the lowest signal-kill encoding) must throw"
        );
    }

    #[test]
    fn live_pick_port_does_not_throw_on_a_plain_exit_128() {
        // The other side of the same boundary: `128` is NEVER produced by this codebase's own
        // signal encoding (whose floor is 129) — it's ambiguous plain `nc` output territory, so
        // the `>= 129` guard must NOT treat it as a signal-kill. A `> 129`-vs-`>= 129` off-by-one
        // wouldn't be caught by this test alone, but it locks in that 128 stays a legitimate,
        // non-throwing "port not open" answer rather than becoming an over-broad false positive.
        let fake = Arc::new(SignalKilledRunner(128));
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<i64>(r#"pick_port("web1", 3000)"#);
        assert!(
            r.is_ok(),
            "exit code 128 must NOT be misread as a signal-kill: {r:?}"
        );
    }

    #[test]
    fn live_pick_port_throws_a_clear_error_instead_of_silently_scanning_one_port_when_base_would_overflow_u16(
    ) {
        // A `base` high enough that `base + 10000 + 99`
        // exceeds `u16::MAX` used to silently saturate EVERY one of the 100 scan candidates
        // down to the exact same port (65535), via `.saturating_add` on already-u16 arithmetic
        // — collapsing "scan 100 distinct ports" into "probe port 65535 a hundred times". The
        // fix must reject this loudly instead. 55437 is the exact boundary: `55437 + 10000 + 99
        // == 65536`, one past `u16::MAX`.
        let fake = Arc::new(SignalKilledRunner(1)); // exit 1 == "not open" == port free, on ANY probe
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<i64>(r#"pick_port("web1", 55437)"#);
        assert!(
            r.is_err(),
            "a base port whose 100-candidate scan window would overflow u16 must throw, not \
             silently return a port"
        );
        let msg = format!("{}", r.unwrap_err());
        assert!(
            msg.contains("exceeds the maximum port number") && msg.contains("65535"),
            "must name the real cause (an impossible scan window), not fall through to the \
             generic \"no free host port\" exhausted-scan message: {msg}"
        );
    }

    #[test]
    fn live_pick_port_succeeds_at_the_highest_base_that_still_fits_entirely_in_u16() {
        // One below the boundary above: `55436 + 10000 + 99 == 65535`, exactly `u16::MAX` — the
        // whole 100-candidate window still fits, so this must succeed normally (not throw),
        // pinning the exact edge of the new guard from the other side.
        let fake = Arc::new(SignalKilledRunner(1)); // every candidate reports "free"
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<i64>(r#"pick_port("web1", 55436)"#);
        assert_eq!(
            r.unwrap(),
            65436,
            "must return the first (base + 10000) candidate, not throw, when the full scan \
             window still fits in u16"
        );
    }

    #[test]
    fn live_wait_port_probes_exactly_once_no_internal_retry() {
        // port_open used to retry internally (30 x 2s = up to 60s) on
        // TOP of lib/healthcheck.rhai's own cfg.attempts/cfg.interval retry loop (its only caller)
        // — so a caller-configured "5 attempts, 1s apart" bound actually took up to 5 minutes.
        // A single ordinary "closed" probe (exit 1, nc's plain connection-refused code — not one
        // of the failure-to-run guards) must now return false IMMEDIATELY, not after retrying.
        let fake = FakeRunner::shared();
        fake.fail_host("web1", 1, "");
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        let start = std::time::Instant::now();
        let r: bool = e.eval(r#"port_open("web1", 3000)"#).unwrap();
        assert!(!r, "an ordinary closed port must report false");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "must return immediately (no internal retry loop): took {:?}",
            start.elapsed()
        );
        assert_eq!(
            fake.calls().len(),
            1,
            "must probe exactly once: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn live_container_healthy_probes_exactly_once_no_internal_retry() {
        // Same reasoning as live_wait_port_probes_exactly_once_no_internal_retry, for
        // container_healthy / wait_container_healthy.
        let fake = FakeRunner::shared();
        fake.fail_host("web1", 0, ""); // exit 0, stdout empty (not "healthy") -> ordinary false
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        let start = std::time::Instant::now();
        let r: bool = e.eval(r#"container_healthy("web1", "app")"#).unwrap();
        assert!(
            !r,
            "an ordinary not-yet-healthy container must report false"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "must return immediately (no internal retry loop): took {:?}",
            start.elapsed()
        );
        assert_eq!(
            fake.calls().len(),
            1,
            "must probe exactly once: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn live_container_running_probes_via_runner() {
        let fake = Arc::new(TrueRunner::default());
        let ctx = shared(fake.clone());
        let e = engine_with(ctx);
        let running: bool = e.eval(r#"container_running("web1", "app")"#).unwrap();
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
        let r = e.eval::<bool>(r#"container_running("web1", "app")"#);
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
        let running: bool = e.eval(r#"container_running("web1", "caddy")"#).unwrap();
        assert!(!running, "unreachable host -> assume absent, don't throw");
        let plan = ctx.plan.lock().unwrap().clone();
        assert!(
            plan.iter().any(|a| a.detail.contains("probe failed")),
            "the plan must surface the unreachable-probe note: {plan:?}"
        );
    }

    #[test]
    fn live_probe_absent_container_reports_not_running() {
        // A genuine "no such object" (exit 1) is a legitimate absent answer, not a failure.
        let fake = Arc::new(NoSuchRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let running: bool = e.eval(r#"container_running("web1", "gone")"#).unwrap();
        assert!(!running);
    }

    #[test]
    fn live_probe_missing_cli_throws_instead_of_reporting_absent() {
        // A missing container runtime (exit 127, "docker: command not
        // found") must throw — NOT be folded into "container absent" the way a naive substring
        // match on "not found" used to. A deploy against a host with no runtime installed should
        // fail with a clear "is the runtime installed" error, not silently take the
        // fresh-install branch and fail confusingly mid-deploy instead.
        let fake = Arc::new(CommandNotFoundRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<bool>(r#"container_running("web1", "app")"#);
        assert!(
            r.is_err(),
            "a missing CLI must throw, not report absent: {r:?}"
        );
        let msg = format!("{}", r.unwrap_err());
        assert!(msg.contains("exit 127"), "got: {msg}");
        assert!(
            msg.contains("is the container runtime installed"),
            "got: {msg}"
        );
    }

    #[test]
    fn live_probe_local_spawn_failure_throws_instead_of_reporting_absent() {
        // A LOCAL spawn failure (e.g. ssh itself
        // isn't installed on the machine running nrg) maps to exit_code -1 with a message like
        // "ssh spawn failed: No such file or directory" (RealRunner::run_ssh) — which itself
        // contains "no such" and would otherwise be misclassified as "container absent" instead
        // of "the probe never even ran".
        let fake = Arc::new(LocalSpawnFailureRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let r = e.eval::<bool>(r#"container_running("web1", "app")"#);
        assert!(
            r.is_err(),
            "a local spawn failure must throw, not report absent: {r:?}"
        );
        let msg = format!("{}", r.unwrap_err());
        assert!(msg.contains("no real exit code"), "got: {msg}");
        assert!(msg.contains("No such file or directory"), "got: {msg}");
    }

    #[test]
    fn live_image_id_recognizes_podmans_absent_image_wording() {
        // (confirmed against containers/storage's `ErrImageUnknown =
        // "image not known"`): Podman's `image inspect` on a missing image doesn't say "no
        // such", so the shared classifier alone wouldn't catch it — a first deploy of a new tag
        // under Podman would otherwise throw instead of correctly reporting the image absent.
        let fake = Arc::new(PodmanImageNotKnownRunner);
        let ctx = shared(fake);
        let e = engine_with(ctx);
        let id: String = e.eval(r#"container_image_id("web1", "myapp:v1")"#).unwrap();
        assert_eq!(
            id, "",
            "a genuinely-absent Podman image must report absent, not throw"
        );
    }

    #[test]
    fn live_probe_honors_configured_runtime() {
        let fake = Arc::new(TrueRunner::default());
        let ctx = shared(fake.clone());
        ctx.session
            .lock()
            .unwrap()
            .insert("nrg.runtime.cmd".to_string(), "podman".to_string());
        let e = engine_with(ctx);
        let _running: bool = e.eval(r#"container_running("web1", "app")"#).unwrap();
        let calls = fake.calls.lock().unwrap();
        assert!(
            calls[0].contains("podman inspect -f '{{.State.Running}}' 'app'"),
            "got: {}",
            calls[0]
        );
        assert!(!calls[0].contains("docker"));
    }

    #[test]
    fn live_probe_ignores_a_stale_persisted_runtime_from_a_previous_run() {
        // `ctx.state` is loaded from the project's on-disk state.json,
        // which may carry a runtime choice a PREVIOUS invocation's `set_runtime()` persisted
        // there. This run's own probes must NOT pick that up unless THIS run also calls
        // `set_runtime()` (which populates the ephemeral `session` store, not `state`) —
        // otherwise a stale `podman` value would silently make every future run probe with
        // `podman inspect` even after the script stopped calling `set_runtime("podman")`.
        let fake = Arc::new(TrueRunner::default());
        let ctx = shared(fake.clone());
        // Simulate a stale value left in persisted state by a prior run, WITHOUT this run
        // populating `session` (i.e. this run never calls `set_runtime()`).
        ctx.state
            .lock()
            .unwrap()
            .set("nrg.runtime.cmd", "podman")
            .unwrap();
        let e = engine_with(ctx);
        let _running: bool = e.eval(r#"container_running("web1", "app")"#).unwrap();
        let calls = fake.calls.lock().unwrap();
        assert!(
            calls[0].contains("docker inspect"),
            "must default to docker, ignoring the stale persisted state: {}",
            calls[0]
        );
    }

    /// A runner whose ssh replies make inspect/health/port probes succeed ("true"/"healthy").
    #[derive(Default)]
    struct TrueRunner {
        calls: Mutex<Vec<String>>,
    }
    impl CommandRunner for TrueRunner {
        fn run_ssh(&self, host: &str, cmd: &str) -> RawOutput {
            self.calls
                .lock()
                .unwrap()
                .push(format!("ssh {host}: {cmd}"));
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
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
        fn run_ssh_stdin(&self, _host: &str, _cmd: &str, _stdin: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
        fn run_local_stdin(&self, _cmd: &str, _stdin: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
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
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
        fn run_ssh_stdin(&self, _h: &str, _c: &str, _s: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
        fn run_local_stdin(&self, _c: &str, _s: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
    }

    /// Reports a caller-chosen `exit_code` on every `run_ssh` call, regardless of what it means.
    /// Despite the name (its original purpose: exercising `real_port_open`'s `>= 129`
    /// signal-kill guard — `128 + signal` per `RealRunner::exit_code_of`, e.g. `128 + 9 = 137`
    /// for SIGKILL — and pinning its exact boundary, `129` must throw, `128` must NOT), it's also
    /// reused elsewhere for perfectly ordinary, non-signal exit codes (`0` = busy/open, `1` =
    /// free/not-open) wherever a test just needs a probe that answers a fixed, arbitrary code on
    /// every call — see its call sites for what each specific code means in context.
    struct SignalKilledRunner(i64);
    impl CommandRunner for SignalKilledRunner {
        fn run_ssh(&self, _host: &str, _cmd: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: self.0,
            }
        }
        fn run_local(&self, _cmd: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
        fn run_ssh_stdin(&self, _h: &str, _c: &str, _s: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
        fn run_local_stdin(&self, _c: &str, _s: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
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
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
        fn run_ssh_stdin(&self, _h: &str, _c: &str, _s: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
        fn run_local_stdin(&self, _c: &str, _s: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
    }

    /// Podman's real `image inspect` failure on a missing image (confirmed against
    /// containers/storage's `ErrImageUnknown = "image not known"`) — does NOT
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
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
        fn run_ssh_stdin(&self, _h: &str, _c: &str, _s: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
        fn run_local_stdin(&self, _c: &str, _s: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
    }

    /// A LOCAL spawn failure — e.g. ssh itself isn't installed on the
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
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
        fn run_ssh_stdin(&self, _h: &str, _c: &str, _s: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
        fn run_local_stdin(&self, _c: &str, _s: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
    }

    /// The container runtime CLI isn't installed on this host at all: exit 127, and the shell's
    /// own "command not found" message — which contains the substring
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
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
        fn run_ssh_stdin(&self, _h: &str, _c: &str, _s: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
        fn run_local_stdin(&self, _c: &str, _s: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
    }

    /// Every `nc -z` succeeds, so every candidate port looks busy.
    struct BusyPortRunner;
    impl CommandRunner for BusyPortRunner {
        fn run_ssh(&self, _host: &str, _cmd: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
        fn run_local(&self, _cmd: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
        fn run_ssh_stdin(&self, _h: &str, _c: &str, _s: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
        fn run_local_stdin(&self, _c: &str, _s: &str) -> RawOutput {
            RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        }
    }
}
